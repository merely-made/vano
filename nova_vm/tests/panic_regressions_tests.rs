// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    fmt,
};

use nova_vm::{
    ecmascript::{
        AbstractModule, Agent, AgentOptions, DefaultHostHooks, GcAgent, GraphLoadingStateRecord,
        HostDefined, HostHooks, Job, ModuleRequest, Referrer, String as JsString, Value,
        finish_loading_imported_module, parse_module,
    },
    engine::{Bindable, Global, NoGcScope},
};

fn run_script<'gc>(
    agent: &mut Agent,
    source: &str,
    gc: nova_vm::engine::GcScope<'gc, '_>,
) -> Value<'gc> {
    let source = JsString::from_string(agent, source.to_owned(), gc.nogc()).unbind();
    agent
        .run_script(source, gc)
        .expect("script should complete")
}

#[test]
fn tagged_template_installs_raw_on_its_existing_array_object() {
    let mut agent = GcAgent::new(AgentOptions::default(), &DefaultHostHooks);
    let realm = agent.create_default_realm();
    agent.run_in_realm(&realm, |agent, gc| {
        let result = run_script(agent, "String.raw`site` === 'site'", gc);
        assert_eq!(result, Value::Boolean(true));
    });
    agent.remove_realm(realm);
}

/// `unescape` located each `%` after the first at the wrong index: it panicked
/// slicing backwards on some strings and silently skipped escapes on others.
#[test]
fn unescape_finds_every_escape_after_the_first() {
    let mut agent = GcAgent::new(AgentOptions::default(), &DefaultHostHooks);
    let realm = agent.create_default_realm();
    agent.run_in_realm(&realm, |agent, gc| {
        let result = run_script(
            agent,
            r#"[
              ["yee%F0%90%90%B7eey", "yee\u00F0\u0090\u0090\u00B7eey"],
              ["%F0%90%90%B7", "\u00F0\u0090\u0090\u00B7"],
              ["a%u0041b%41c%u00e9", "aAbAc\u00E9"],
              ["%%41", "%A"],
              ["%zz%41%4", "%zzA%4"],
              ["plain", "plain"],
            ].every(([input, expected]) => unescape(input) === expected)"#,
            gc,
        );
        assert_eq!(result, Value::Boolean(true));
    });
    agent.remove_realm(realm);
}

/// A function that runs `declaration` and reports whether it threw a
/// ReferenceError, as source evaluating to a boolean.
fn tdz(declaration: &str) -> &'static str {
    let source = format!(
        "(() => {{ try {{ {declaration} return false; }} catch (e) {{ return e instanceof ReferenceError; }} }})()"
    );
    Box::leak(source.into_boxed_str())
}

/// A destructuring default can read a binding the same declaration made
/// earlier: `b = a` runs after `a` is initialized. Reads before a binding is
/// made still throw.
#[test]
fn destructuring_defaults_read_earlier_bindings() {
    let cases = [
        ("top-level const array", "const [a, b = a] = [1]; b === 1"),
        (
            "top-level let object",
            "let { a, b = a } = { a: 2 }; b === 2",
        ),
        (
            "function const array",
            "(() => { const [a, b = a] = [3]; return b; })() === 3",
        ),
        (
            "function let object",
            "(() => { let { a, b = a + 1 } = { a: 3 }; return b; })() === 4",
        ),
        (
            "function var array",
            "(() => { var [a, b = a] = [5]; return b; })() === 5",
        ),
        (
            "for-of const",
            "(() => { for (const [a, b = a] of [[6]]) return b; })() === 6",
        ),
        (
            "block const",
            "(() => { { const [a, b = a] = [7]; return b; } })() === 7",
        ),
        ("parameters", "((a, b = a) => b)(8) === 8"),
        (
            "var reads itself",
            "(() => { var x = x; return x; })() === undefined",
        ),
        // These read a binding before it is made, and must still throw.
        ("own default", tdz("const [a = a] = [];")),
        ("later element", tdz("const [b = a, a] = [];")),
        ("initializer", tdz("const [a] = [a];")),
        ("object own default", tdz("let { a = a } = {};")),
        ("computed key", tdz("const { [a]: a } = {};")),
        ("plain initializer", tdz("const a = a;")),
        (
            "parameter own default",
            "(() => { try { ((a = a) => a)(); } catch (e) { return e instanceof ReferenceError; } })()",
        ),
    ];
    let mut failures = Vec::new();
    for (name, source) in cases {
        let mut agent = GcAgent::new(AgentOptions::default(), &DefaultHostHooks);
        let realm = agent.create_default_realm();
        agent.run_in_realm(&realm, |agent, mut gc| {
            let source_text = JsString::from_string(agent, source.to_owned(), gc.nogc()).unbind();
            let thrown = match agent.run_script(source_text, gc.reborrow()) {
                Ok(Value::Boolean(true)) => None,
                Ok(_) => Some(None),
                Err(error) => Some(Some(error.unbind())),
            };
            match thrown {
                None => {}
                Some(None) => failures.push(format!("{name}: false")),
                Some(Some(error)) => {
                    let message = error.to_string(agent, gc.reborrow());
                    failures.push(format!("{name}: {}", message.to_string_lossy(agent)));
                }
            }
        });
        agent.remove_realm(realm);
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[derive(Default)]
struct TestHostHooks {
    promise_jobs: RefCell<VecDeque<Job>>,
    modules: RefCell<HashMap<String, Global<AbstractModule<'static>>>>,
}

impl fmt::Debug for TestHostHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestHostHooks").finish_non_exhaustive()
    }
}

impl HostHooks for TestHostHooks {
    fn enqueue_generic_job(&self, job: Job) {
        self.promise_jobs.borrow_mut().push_back(job);
    }

    fn enqueue_promise_job(&self, job: Job) {
        self.promise_jobs.borrow_mut().push_back(job);
    }

    fn enqueue_timeout_job(&self, job: Job, _milliseconds: u64) {
        self.promise_jobs.borrow_mut().push_back(job);
    }

    fn load_imported_module<'gc>(
        &self,
        agent: &mut Agent,
        referrer: Referrer<'gc>,
        module_request: ModuleRequest<'gc>,
        _host_defined: Option<HostDefined>,
        payload: &mut GraphLoadingStateRecord<'gc>,
        gc: NoGcScope<'gc, '_>,
    ) {
        let specifier = module_request
            .specifier(agent)
            .to_string_lossy(agent)
            .into_owned();
        let loaded = if let Some(module) = self.modules.borrow().get(&specifier) {
            module.get(agent, gc)
        } else {
            let source_text = match specifier.as_str() {
                "forwarder" => {
                    "import { value, setValue } from 'source'; export { value as alias, setValue };"
                }
                "namespace-forwarder" => {
                    "import * as ns from 'source'; export { ns as namespace };"
                }
                "source" => {
                    "export var value = 'through-re-export'; export function setValue(next) { value = next; }"
                }
                _ => panic!("unexpected module request: {specifier}"),
            };
            let source = JsString::from_string(agent, source_text.to_owned(), gc);
            let realm = referrer.realm(agent, gc);
            let module =
                parse_module(agent, source, realm, None, gc).expect("test module should parse");
            let module: AbstractModule<'gc> = module.into();
            let rooted = Global::new(agent, module.unbind());
            self.modules.borrow_mut().insert(specifier.clone(), rooted);
            self.modules
                .borrow()
                .get(&specifier)
                .expect("loaded module was cached")
                .get(agent, gc)
        };
        finish_loading_imported_module(agent, referrer, module_request, payload, Ok(loaded), gc);
    }
}

#[test]
fn import_of_a_re_exported_import_uses_the_indirect_binding() {
    let hooks: &'static TestHostHooks = Box::leak(Box::new(TestHostHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();
    agent.run_in_realm(&realm, |agent, gc| {
        let source = JsString::from_string(
            agent,
            "import { value, setValue } from 'source'; export { value as alias, setValue };"
                .to_owned(),
            gc.nogc(),
        );
        let forwarder = parse_module(
            agent,
            source,
            agent.current_realm(gc.nogc()),
            None,
            gc.nogc(),
        )
        .expect("forwarder module should parse");
        let abstract_module: AbstractModule<'_> = forwarder.into();
        hooks.modules.borrow_mut().insert(
            "forwarder".to_owned(),
            Global::new(agent, abstract_module.unbind()),
        );
        agent
            .run_module(forwarder.unbind(), None, gc)
            .expect("forwarder module should link and evaluate");
    });
    agent.run_in_realm(&realm, |agent, gc| {
        let source = JsString::from_string(
            agent,
            "import { alias, setValue } from 'forwarder'; import { namespace } from 'namespace-forwarder'; globalThis.reexportValue = alias; globalThis.namespaceValue = namespace.value; setValue('updated'); globalThis.reexportValueAfterUpdate = alias; globalThis.namespaceValueAfterUpdate = namespace.value;".to_owned(),
            gc.nogc(),
        );
        let current_realm = agent.current_realm(gc.nogc());
        let root = parse_module(agent, source, current_realm, None, gc.nogc())
            .expect("root module should parse")
            .unbind();
        agent
            .run_module(root, None, gc)
            .expect("module re-export graph should link and evaluate");
    });
    agent.run_in_realm(&realm, |agent, gc| {
        let result = run_script(
            agent,
            "globalThis.reexportValue === 'through-re-export' && globalThis.namespaceValue === 'through-re-export' && globalThis.reexportValueAfterUpdate === 'updated' && globalThis.namespaceValueAfterUpdate === 'updated'",
            gc,
        );
        assert_eq!(result, Value::Boolean(true));
    });
    agent.remove_realm(realm);
}

#[test]
fn eval_in_a_promise_job_can_create_a_function_without_an_active_module() {
    let hooks: &'static TestHostHooks = Box::leak(Box::new(TestHostHooks::default()));
    let mut agent = GcAgent::new(AgentOptions::default(), hooks);
    let realm = agent.create_default_realm();
    agent.run_in_realm(&realm, |agent, mut gc| {
        let _ = run_script(
            agent,
            "Promise.resolve('() => 1').then(eval).then(f => { globalThis.promiseEvalResult = f(); });",
            gc.reborrow(),
        );
        loop {
            let job = hooks.promise_jobs.borrow_mut().pop_front();
            let Some(job) = job else {
                break;
            };
            job.run(agent, gc.reborrow())
                .expect("Promise job should complete");
        }
    });
    agent.run_in_realm(&realm, |agent, gc| {
        let result = run_script(agent, "globalThis.promiseEvalResult === 1", gc);
        assert_eq!(result, Value::Boolean(true));
    });
    agent.remove_realm(realm);
}
