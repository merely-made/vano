// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "date")]

use std::sync::atomic::{AtomicI64, Ordering};

use nova_vm::{
    ecmascript::{
        AgentOptions, DefaultHostHooks, GcAgent, HostHooks, Job, String, parse_script,
        script_evaluation,
    },
    engine::Bindable,
};

#[derive(Debug)]
struct Clock(AtomicI64);
impl HostHooks for Clock {
    fn enqueue_generic_job(&self, job: Job) {
        DefaultHostHooks.enqueue_generic_job(job);
    }
    fn enqueue_promise_job(&self, job: Job) {
        DefaultHostHooks.enqueue_promise_job(job);
    }
    fn enqueue_timeout_job(&self, job: Job, milliseconds: u64) {
        DefaultHostHooks.enqueue_timeout_job(job, milliseconds);
    }

    fn system_time_millis(&self) -> Option<i64> {
        Some(self.0.load(Ordering::SeqCst))
    }
}

fn evaluate(agent: &mut GcAgent, realm: &nova_vm::ecmascript::RealmRoot, source: &str) {
    agent.run_in_realm(realm, |agent, mut gc| {
        let realm = agent.current_realm(gc.nogc());
        let source = String::from_string(agent, source.to_owned(), gc.nogc());
        let script = parse_script(agent, source, realm, false, None, gc.nogc()).unwrap();
        if let Err(err) = script_evaluation(agent, script.unbind(), gc.reborrow()) {
            panic!(
                "{}",
                err.unbind().to_string(agent, gc).to_string_lossy(agent)
            );
        }
    });
}

#[test]
fn host_clock_controls_all_current_date_entry_points_and_tracks_updates() {
    let clock = Box::leak(Box::new(Clock(AtomicI64::new(1_700_000_000_123))));
    let mut agent = GcAgent::new(AgentOptions::default(), clock);
    let realm = agent.create_default_realm();
    for millis in [1_700_000_000_123, 0, -1, 8_640_000_000_000_000] {
        clock.0.store(millis, Ordering::SeqCst);
        evaluate(
            &mut agent,
            &realm,
            &format!(
                r#"
            if (Date.now() !== {millis}) throw new Error('Date.now ignored host');
            if (new Date().valueOf() !== {millis}) throw new Error('constructor ignored host');
            if (Date() !== new Date({millis}).toString()) throw new Error('Date function ignored host');
            if (new Date(123).valueOf() !== 123) throw new Error('explicit date used host');
        "#
            ),
        );
    }
}

#[test]
fn default_host_uses_system_time() {
    let mut agent = GcAgent::new(AgentOptions::default(), &DefaultHostHooks);
    let realm = agent.create_default_realm();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    evaluate(&mut agent, &realm, "globalThis.clockSample = Date.now();");
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    evaluate(
        &mut agent,
        &realm,
        &format!(
            "if (clockSample < {before} || clockSample > {after}) throw new Error('default clock out of bounds');"
        ),
    );
}

#[test]
fn out_of_range_host_clock_is_an_invalid_date_without_panicking() {
    let clock = Box::leak(Box::new(Clock(AtomicI64::new(i64::MAX))));
    let mut agent = GcAgent::new(AgentOptions::default(), clock);
    let realm = agent.create_default_realm();
    for millis in [
        i64::MAX,
        i64::MIN,
        8_640_000_000_000_001,
        -8_640_000_000_000_001,
    ] {
        clock.0.store(millis, Ordering::SeqCst);
        evaluate(
            &mut agent,
            &realm,
            r#"
            if (!Number.isNaN(Date.now())) throw new Error('invalid Date.now was not NaN');
            if (!Number.isNaN(new Date().valueOf())) throw new Error('invalid date value was not NaN');
            if (Date() !== 'Invalid Date') throw new Error('invalid Date string');
        "#,
        );
    }
}
