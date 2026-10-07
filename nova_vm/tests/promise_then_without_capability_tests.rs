// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{cell::RefCell, collections::VecDeque, fmt};

use nova_vm::{
    ecmascript::{
        Agent, AgentOptions, GcAgent, HostHooks, Job, Promise, String as JsString, Value,
        perform_promise_then_without_capability,
    },
    engine::{Bindable, GcScope, Global},
};

#[derive(Default)]
struct PromiseJobHooks {
    jobs: RefCell<VecDeque<Job>>,
}

impl fmt::Debug for PromiseJobHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromiseJobHooks").finish_non_exhaustive()
    }
}

impl HostHooks for PromiseJobHooks {
    fn enqueue_generic_job(&self, job: Job) {
        self.jobs.borrow_mut().push_back(job);
    }

    fn enqueue_promise_job(&self, job: Job) {
        self.jobs.borrow_mut().push_back(job);
    }

    fn enqueue_timeout_job(&self, job: Job, _milliseconds: u64) {
        self.jobs.borrow_mut().push_back(job);
    }
}

fn eval<'gc>(agent: &mut Agent, source: &str, gc: GcScope<'gc, '_>) -> Value<'gc> {
    let source = JsString::from_string(agent, source.to_owned(), gc.nogc()).unbind();
    agent
        .run_script(source, gc)
        .expect("script should complete")
}

fn drain_promise_jobs(agent: &mut Agent, hooks: &PromiseJobHooks, mut gc: GcScope<'_, '_>) {
    loop {
        let job = hooks.jobs.borrow_mut().pop_front();
        let Some(job) = job else { break };
        job.run(agent, gc.reborrow())
            .expect("Promise reaction job should complete");
    }
}

fn next_promise_job(hooks: &PromiseJobHooks) -> Job {
    hooks
        .jobs
        .borrow_mut()
        .pop_front()
        .expect("one Promise reaction job should be queued")
}

fn promise_value<'a>(value: Value<'a>) -> Promise<'a> {
    let Value::Promise(promise) = value else {
        panic!("script did not produce a Promise")
    };
    promise
}

#[test]
fn fulfilled_handler_uses_standard_job_and_does_not_observe_species() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.sentinel = {}; globalThis.p = Promise.resolve(sentinel); \
             Object.defineProperty(p, 'constructor', { get() { throw new Error('constructor read'); } }); \
             Object.defineProperty(Promise, Symbol.species, { get() { throw new Error('species read'); } });",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let fulfilled_value = eval(
            agent,
            "value => { globalThis.fulfilledValue = value; }",
            gc.reborrow(),
        );
        let fulfilled = Global::new(agent, fulfilled_value.unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        let fulfilled_handle = fulfilled.get(agent, gc.nogc()).bind(gc.nogc());

        perform_promise_then_without_capability(
            agent,
            promise_handle,
            fulfilled_handle,
            Value::Undefined,
            gc.nogc(),
        );
        assert_eq!(
            eval(agent, "typeof fulfilledValue === 'undefined'", gc.reborrow()),
            Value::Boolean(true),
            "fulfilled reactions must be queued, not called inline",
        );
        drain_promise_jobs(agent, hooks, gc.reborrow());
        assert_eq!(
            eval(agent, "fulfilledValue === sentinel", gc.reborrow()),
            Value::Boolean(true),
            "reaction should receive the exact fulfillment value",
        );
    });

    agent.remove_realm(realm);
}

#[test]
fn pending_handler_runs_after_settlement_with_exact_value() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.sentinel = {}; globalThis.resolvePending = undefined; \
             globalThis.p = new Promise(resolve => { resolvePending = resolve; }); \
             globalThis.called = false;",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let fulfilled_value = eval(
            agent,
            "value => { globalThis.called = true; globalThis.pendingValue = value; }",
            gc.reborrow(),
        );
        let fulfilled = Global::new(agent, fulfilled_value.unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        let fulfilled_handle = fulfilled.get(agent, gc.nogc()).bind(gc.nogc());

        perform_promise_then_without_capability(
            agent,
            promise_handle,
            fulfilled_handle,
            Value::Undefined,
            gc.nogc(),
        );
        assert_eq!(
            eval(agent, "called", gc.reborrow()),
            Value::Boolean(false),
            "a pending Promise must not run its reaction during registration",
        );
        eval(agent, "resolvePending(sentinel)", gc.reborrow());
        drain_promise_jobs(agent, hooks, gc.reborrow());
        assert_eq!(
            eval(agent, "called && pendingValue === sentinel", gc.reborrow()),
            Value::Boolean(true),
        );
    });

    agent.remove_realm(realm);
}

#[test]
fn rejected_handler_receives_the_exact_reason() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.reason = {}; globalThis.p = Promise.reject(reason); \
             globalThis.rejectedValue = undefined;",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let rejected_value = eval(
            agent,
            "error => { globalThis.rejectedValue = error; }",
            gc.reborrow(),
        );
        let rejected = Global::new(agent, rejected_value.unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        let rejected_handle = rejected.get(agent, gc.nogc()).bind(gc.nogc());

        perform_promise_then_without_capability(
            agent,
            promise_handle,
            Value::Undefined,
            rejected_handle,
            gc.nogc(),
        );
        drain_promise_jobs(agent, hooks, gc.reborrow());
        assert_eq!(
            eval(agent, "rejectedValue === reason", gc.reborrow()),
            Value::Boolean(true),
            "rejection reaction should receive the exact rejection reason",
        );
    });

    agent.remove_realm(realm);
}

#[test]
fn omitted_fulfilled_handler_is_a_noop_without_a_capability() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.p = Promise.resolve('done');",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        perform_promise_then_without_capability(
            agent,
            promise_handle,
            Value::Undefined,
            Value::Undefined,
            gc.nogc(),
        );

        next_promise_job(hooks)
            .run(agent, gc.reborrow())
            .expect("empty fulfillment handler without a capability is a no-op");
    });

    agent.remove_realm(realm);
}

#[test]
fn omitted_rejection_handler_without_a_capability_returns_the_original_reason() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.reason = {}; globalThis.p = Promise.reject(reason);",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let reason_value = eval(agent, "reason", gc.reborrow());
        let reason = Global::new(agent, reason_value.unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        perform_promise_then_without_capability(
            agent,
            promise_handle,
            Value::Undefined,
            Value::Undefined,
            gc.nogc(),
        );

        let error = next_promise_job(hooks)
            .run(agent, gc.reborrow())
            .expect_err("empty rejection handler should follow the job error path");
        assert_eq!(
            error.value(),
            reason.get(agent, gc.nogc()).bind(gc.nogc()),
            "the job error should preserve the exact rejection reason",
        );
    });

    agent.remove_realm(realm);
}

#[test]
fn thrown_callback_without_a_capability_returns_the_original_reason() {
    let hooks: &'static PromiseJobHooks = Box::leak(Box::new(PromiseJobHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();

    agent.run_in_realm(&realm, |agent, mut gc| {
        eval(
            agent,
            "globalThis.reason = {}; globalThis.p = Promise.resolve('value');",
            gc.reborrow(),
        );
        let promise_result = eval(agent, "p", gc.reborrow());
        let promise = Global::new(agent, promise_value(promise_result).unbind());
        let reason_value = eval(agent, "reason", gc.reborrow());
        let reason = Global::new(agent, reason_value.unbind());
        let throwing_handler_value = eval(agent, "() => { throw reason; }", gc.reborrow());
        let throwing_handler = Global::new(agent, throwing_handler_value.unbind());
        let promise_handle = promise.get(agent, gc.nogc()).bind(gc.nogc());
        let throwing_handler_handle = throwing_handler.get(agent, gc.nogc()).bind(gc.nogc());
        perform_promise_then_without_capability(
            agent,
            promise_handle,
            throwing_handler_handle,
            Value::Undefined,
            gc.nogc(),
        );

        let error = next_promise_job(hooks)
            .run(agent, gc.reborrow())
            .expect_err("a thrown callback should follow the job error path");
        assert_eq!(
            error.value(),
            reason.get(agent, gc.nogc()).bind(gc.nogc()),
            "the job error should preserve the exact thrown value",
        );
    });

    agent.remove_realm(realm);
}
