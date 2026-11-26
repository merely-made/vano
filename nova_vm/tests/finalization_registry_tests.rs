// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{cell::RefCell, collections::VecDeque, fmt};

use nova_vm::{
    ecmascript::{AgentOptions, GcAgent, HostHooks, Job, String as JsString, Value},
    engine::Bindable,
};

#[derive(Default)]
struct CleanupHooks {
    jobs: RefCell<VecDeque<Job>>,
}

impl fmt::Debug for CleanupHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CleanupHooks").finish_non_exhaustive()
    }
}

impl HostHooks for CleanupHooks {
    fn enqueue_generic_job(&self, job: Job) {
        self.jobs.borrow_mut().push_back(job);
    }

    fn enqueue_promise_job(&self, job: Job) {
        self.jobs.borrow_mut().push_back(job);
    }

    fn enqueue_timeout_job(&self, job: Job, _milliseconds: u64) {
        self.jobs.borrow_mut().push_back(job);
    }

    fn enqueue_finalization_registry_cleanup_job(&self, job: Job) {
        self.jobs.borrow_mut().push_back(job);
    }
}

/// Registers `count` targets that die together, collects, runs the cleanup
/// jobs, and returns how many held values the callback saw.
fn cleaned_up(count: usize) -> f64 {
    let hooks: &'static CleanupHooks = Box::leak(Box::new(CleanupHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();
    let setup = format!(
        "globalThis.held = []; \
         globalThis.registry = new FinalizationRegistry(v => held.push(v)); \
         (function () {{ for (let i = 0; i < {count}; i++) registry.register({{}}, i); }})();"
    );
    agent.run_in_realm(&realm, |agent, gc| {
        let source = JsString::from_string(agent, setup, gc.nogc()).unbind();
        agent.run_script(source, gc).expect("setup should complete");
    });
    agent.gc();
    agent.run_in_realm(&realm, |agent, mut gc| {
        loop {
            let job = hooks.jobs.borrow_mut().pop_front();
            let Some(job) = job else { break };
            job.run(agent, gc.reborrow()).expect("cleanup job should complete");
        }
        let source = JsString::from_static_str(agent, "held.length", gc.nogc()).unbind();
        match agent.run_script(source, gc).expect("count should evaluate") {
            Value::Integer(n) => n.into_i64() as f64,
            other => panic!("held.length was {other:?}"),
        }
    })
}

#[test]
fn every_dead_cell_reaches_the_cleanup_callback() {
    for count in 1..=4 {
        assert_eq!(cleaned_up(count), count as f64, "{count} cells died together");
    }
}
