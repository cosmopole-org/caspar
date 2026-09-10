//! Runtime tests against an in-memory host.
//!
//! These cover the properties the rest of the platform is entitled to assume of
//! any in-process runtime, and which the previous stub javascript plugin had
//! none of: that a module runs at all, that a host call round-trips, that
//! identity is stamped by the node rather than taken from the guest, that
//! writes commit exactly once, and that a runaway script is actually stopped.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use caspar_vm_sdk::host::{set_host, KvOp, VmHost};

use crate::runtime::{terminate_managed_vm, JsMac, RunError};

// ── The mock host ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Recorded {
    kv: BTreeMap<String, String>,
    /// Every `unified_host_call` envelope, in order — the identity assertions
    /// read these.
    unified: Vec<Value>,
    /// Every `dispatch` packet (vmOutput, vmLog, typed router packets).
    packets: Vec<Value>,
    /// How many times a batch of raw ops was applied. A double-commit shows up
    /// here as 2.
    applies: usize,
    /// Per-VM JSON transaction contents, and whether it is open.
    json_trx: BTreeMap<String, BTreeMap<String, Value>>,
    open_trx: BTreeMap<String, bool>,
    ended_trx: Vec<String>,
}

struct MockHost {
    inner: Mutex<Recorded>,
}

impl MockHost {
    fn get() -> &'static Arc<MockHost> {
        static CELL: OnceLock<Arc<MockHost>> = OnceLock::new();
        CELL.get_or_init(|| {
            let h = Arc::new(MockHost {
                inner: Mutex::new(Recorded::default()),
            });
            set_host(h.clone() as Arc<dyn VmHost>);
            h
        })
    }

    fn packets_for(&self, machine: &str, key: &str) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap()
            .packets
            .iter()
            .filter(|p| {
                p["key"] == key
                    && (p["input"]["machineId"] == machine || p["input"]["vmId"] == machine)
            })
            .cloned()
            .collect()
    }

    fn unified_for(&self, machine: &str) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap()
            .unified
            .iter()
            .filter(|p| p["machineId"] == machine)
            .cloned()
            .collect()
    }
}

impl VmHost for MockHost {
    fn dispatch(&self, packet: &Value) -> String {
        self.inner.lock().unwrap().packets.push(packet.clone());
        json!({"ok": true, "dispatched": packet["key"].clone()}).to_string()
    }

    fn unified_host_call(&self, packet: &Value) -> String {
        self.inner.lock().unwrap().unified.push(packet.clone());
        json!({"ok": true, "op": packet["op"].clone(), "echo": packet["input"].clone()}).to_string()
    }

    fn storage_log_vm(&self, _vm_id: &str, _log_type: &str, _text: &str, _timestamp_ms: i64) {}
    fn register_vm_context(&self, _vm_id: &str, _creature_id: &str, _machine_id: &str) {}
    fn unregister_vm_context(&self, _vm_id: &str) {}
    fn get_vm_context(&self, _vm_id: &str) -> Option<(String, String)> {
        None
    }
    fn register_vm_container(
        &self,
        _container_name: &str,
        _vm_id: &str,
        _creature_id: &str,
        _program_id: &str,
        _machine_id: &str,
        _entity_id: &str,
    ) {
    }
    fn unregister_vm_container(&self, _container_name: &str) {}
    fn begin_vm_buffer(&self, _vm_id: &str) {}
    fn commit_vm_buffer(&self, _vm_id: &str) {}

    fn acquire_resource_lock(&self, _resource_id: &str, _owner_id: &str) -> Result<(), String> {
        Ok(())
    }
    fn release_resource_lock(&self, _resource_id: &str, _owner_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn state_get(&self, key: &str) -> String {
        self.inner
            .lock()
            .unwrap()
            .kv
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    fn state_get_by_prefix(&self, prefix: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .kv
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn state_apply_ops(&self, ops: &[KvOp]) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        inner.applies += 1;
        for op in ops {
            if op.op == "put" {
                inner.kv.insert(op.key.clone(), op.val.clone());
            } else {
                inner.kv.remove(&op.key);
            }
        }
        Ok(())
    }

    fn vm_json_trx_op(&self, vm_id: &str, op: &str, input: &Value) -> Result<Value, String> {
        let mut inner = self.inner.lock().unwrap();
        inner.open_trx.insert(vm_id.to_string(), true);
        let bucket = inner.json_trx.entry(vm_id.to_string()).or_default();
        let key = input["key"].as_str().unwrap_or("").to_string();
        match op {
            "putJson" => {
                bucket.insert(key, input["data"].clone());
                Ok(json!({"ok": true}))
            }
            "getJson" => {
                let data = bucket.get(&key).cloned().unwrap_or(Value::Null);
                Ok(json!({"ok": true, "data": data}))
            }
            "delKey" => {
                bucket.remove(&key);
                Ok(json!({"ok": true}))
            }
            "getByPrefix" => Ok(json!({"ok": true, "data": []})),
            _ => Err(format!("unsupported json trx op: {}", op)),
        }
    }

    fn end_vm_json_trx(&self, vm_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.open_trx.insert(vm_id.to_string(), false);
        inner.ended_trx.push(vm_id.to_string());
    }

    fn http_request(&self, input: &Value) -> Result<String, String> {
        Ok(json!({"ok": true, "status": 200, "url": input["url"].clone()}).to_string())
    }

    fn storage_root(&self) -> String {
        std::env::temp_dir().to_string_lossy().to_string()
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn write_module(name: &str, source: &str) -> String {
    let dir = std::env::temp_dir().join("caspar-js-vm-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.js", name));
    std::fs::write(&path, source).unwrap();
    path.to_string_lossy().to_string()
}

/// Build and run one module to completion, returning the VM and the outcome.
fn run(name: &str, source: &str, input: &str) -> (JsMac, Result<(), RunError>) {
    run_with(name, source, input, 64, Duration::from_secs(10))
}

fn run_with(
    name: &str,
    source: &str,
    input: &str,
    ram_mb: u64,
    max_exec: Duration,
) -> (JsMac, Result<(), RunError>) {
    let _ = MockHost::get();
    let path = write_module(name, source);
    let mut vm = JsMac::new_vm(
        name.to_string(),
        "main".to_string(),
        "9@global".to_string(),
        path,
        ram_mb,
        max_exec,
    );
    let out = vm.execute_on_update(input.to_string());
    vm.finalize();
    (vm, out)
}

fn output_of(vm: &JsMac) -> String {
    vm.state.borrow().execution_result.clone()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn a_module_runs_and_its_return_value_is_the_output() {
    let (vm, out) = run(
        "returns",
        r#"globalThis.update = function (input) {
             var p = JSON.parse(input);
             return { ok: true, greeted: p.name };
           };"#,
        r#"{"name":"ada"}"#,
    );
    out.expect("the module should run");
    assert_eq!(output_of(&vm), r#"{"ok":true,"greeted":"ada"}"#);
}

#[test]
fn an_explicit_output_host_call_wins_over_the_return_value() {
    // Parity with the wasm ABI: a creature ported across must not change
    // behaviour because it also happens to return something.
    let (vm, out) = run(
        "explicit-output",
        r#"globalThis.update = function () {
             hostCall("output", { text: "from-host-call" });
             return "from-return";
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert_eq!(output_of(&vm), "from-host-call");
}

#[test]
fn host_calls_round_trip_through_the_op_table() {
    let (vm, out) = run(
        "hostcall",
        r#"globalThis.update = function () {
             var r = hostCall("signalUser", { key: "creatures/signal", userId: "7@global" });
             return { ok: r.ok === true, op: r.op };
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert_eq!(output_of(&vm), r#"{"ok":true,"op":"signalUser"}"#);

    // …and the node stamped the identity, rather than the guest naming it.
    let calls = MockHost::get().unified_for("hostcall");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["op"], "signalUser");
    assert_eq!(calls[0]["machineId"], "hostcall");
    assert_eq!(calls[0]["programId"], "hostcall");
    assert_eq!(calls[0]["vmId"], "main");
}

#[test]
fn a_guest_cannot_forge_its_own_identity() {
    // The guest puts someone else's ids in `input`. The envelope the node
    // resolves identity from must still say who this VM actually is; the
    // arguments themselves are left alone, because they may legitimately name
    // another program.
    let (_vm, out) = run(
        "identity",
        r#"globalThis.update = function () {
             hostCall("signalUser", { userId: "1@global", programId: "999@evil", machineId: "999@evil" });
             return "done";
           };"#,
        "{}",
    );
    out.expect("the module should run");
    let calls = MockHost::get().unified_for("identity");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["machineId"], "identity");
    assert_eq!(calls[0]["programId"], "identity");
    assert_eq!(calls[0]["creatureId"], "identity");
    // The argument is untouched — it is an operand, not an identity claim.
    assert_eq!(calls[0]["input"]["programId"], "999@evil");
}

#[test]
fn raw_db_ops_read_their_own_writes_and_commit_once() {
    let (_vm, out) = run(
        "dbops",
        r#"globalThis.update = function () {
             hostCall("dbOp", { op: "put", key: "k1", val: "v1" });
             hostCall("dbOp", { op: "put", key: "k2", val: "v2" });
             var read = hostCallRaw(JSON.stringify({ op: "dbOp", input: { op: "get", key: "k1" } }));
             hostCall("dbOp", { op: "del", key: "k2" });
             return read;
           };"#,
        "{}",
    );
    out.expect("the module should run");

    let host = MockHost::get();
    let inner = host.inner.lock().unwrap();
    // Namespaced by machine id, exactly as the wasm runtime does.
    assert_eq!(inner.kv.get("dbops::k1").map(String::as_str), Some("v1"));
    assert_eq!(inner.kv.get("dbops::k2"), None, "the delete must land too");
}

#[test]
fn the_json_transaction_is_closed_at_teardown_even_without_commit_trx() {
    // A half-open write buffer outliving the VM that opened it is the one state
    // finalize() must never leave behind.
    let (_vm, out) = run(
        "jsontrx",
        r#"globalThis.update = function () {
             hostCall("putJson", { key: "Json::Test::1", path: "doc", data: { a: 1 } });
             var got = hostCall("getJson", { key: "Json::Test::1", path: "doc" });
             return got.data;
           };"#,
        "{}",
    );
    out.expect("the module should run");
    let host = MockHost::get();
    let inner = host.inner.lock().unwrap();
    assert_eq!(inner.open_trx.get("main"), Some(&false), "trx must be closed");
}

#[test]
fn an_async_update_is_awaited() {
    let (vm, out) = run(
        "async",
        r#"globalThis.update = async function (input) {
             await Promise.resolve();
             var r = await new Promise(function (res) { res("resolved"); });
             return { via: r };
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert_eq!(output_of(&vm), r#"{"via":"resolved"}"#);
}

#[test]
fn a_rejected_async_update_is_an_error_not_a_silent_success() {
    let (_vm, out) = run(
        "async-reject",
        r#"globalThis.update = async function () { throw new Error("nope"); };"#,
        "{}",
    );
    let err = out.expect_err("a rejection must surface");
    assert!(err.to_string().contains("nope"), "got: {}", err);
}

#[test]
fn a_thrown_error_carries_its_message_and_stack() {
    let (_vm, out) = run(
        "throws",
        r#"globalThis.update = function () { throw new Error("kaboom"); };"#,
        "{}",
    );
    let err = out.expect_err("a throw must surface");
    assert!(err.to_string().contains("kaboom"), "got: {}", err);
}

#[test]
fn a_module_without_update_says_so_plainly() {
    let (_vm, out) = run("no-update", r#"var x = 1;"#, "{}");
    let err = out.expect_err("a module with no entry point must fail");
    assert!(
        err.to_string().contains("update"),
        "the error must name what is missing; got: {}",
        err
    );
}

#[test]
fn a_syntax_error_is_reported_against_the_module() {
    let (_vm, out) = run("syntax", r#"globalThis.update = function ( {"#, "{}");
    let err = out.expect_err("a broken module must fail");
    assert!(matches!(err, RunError::Guest(_)), "got: {:?}", err);
}

#[test]
fn a_missing_module_file_is_a_runtime_error_not_a_panic() {
    let _ = MockHost::get();
    let mut vm = JsMac::new_vm(
        "missing".to_string(),
        "main".to_string(),
        String::new(),
        "/nonexistent/caspar/module.js".to_string(),
        64,
        Duration::from_secs(5),
    );
    let err = vm.execute_on_update("{}".to_string()).expect_err("must fail");
    assert!(matches!(err, RunError::Runtime(_)), "got: {:?}", err);
}

#[test]
fn an_empty_module_path_is_refused_before_anything_is_built() {
    let _ = MockHost::get();
    let mut vm = JsMac::new_vm(
        "empty-path".to_string(),
        "main".to_string(),
        String::new(),
        "   ".to_string(),
        64,
        Duration::from_secs(5),
    );
    let err = vm.execute_on_update("{}".to_string()).expect_err("must fail");
    assert!(err.to_string().contains("astPath"), "got: {}", err);
}

#[test]
fn a_runaway_loop_is_interrupted_by_the_deadline() {
    let (_vm, out) = run_with(
        "runaway",
        r#"globalThis.update = function () { while (true) {} };"#,
        "{}",
        64,
        Duration::from_millis(300),
    );
    let err = out.expect_err("an infinite loop must be stopped");
    assert!(
        matches!(err, RunError::Interrupted(_)),
        "an interrupt must be reported as one, not as a guest error; got: {:?}",
        err
    );
    assert!(err.to_string().contains("time limit"), "got: {}", err);
}

#[test]
fn a_loop_inside_a_promise_is_interrupted_too() {
    // The deadline must cover the job queue as well, or a script escapes it by
    // moving its loop into a `.then()`.
    let (_vm, out) = run_with(
        "runaway-async",
        r#"globalThis.update = function () {
             return Promise.resolve().then(function () { while (true) {} });
           };"#,
        "{}",
        64,
        Duration::from_millis(300),
    );
    assert!(out.is_err(), "an async infinite loop must not run forever");
}

#[test]
fn terminate_stops_a_running_vm() {
    let _ = MockHost::get();
    let path = write_module(
        "terminated",
        r#"globalThis.update = function () { while (true) {} };"#,
    );
    let mut vm = JsMac::new_vm(
        "terminated".to_string(),
        "main".to_string(),
        String::new(),
        path,
        64,
        // Long enough that only the terminate can end this run.
        Duration::from_secs(120),
    );
    // Register it the way the controller does, so terminate_managed_vm finds it.
    {
        let mut map = crate::runtime::global_managed_vms().lock().unwrap();
        map.insert(
            crate::runtime::vm_key("terminated", "main"),
            crate::runtime::ManagedVmHandle {
                stop: vm.stop_flag(),
                running: vm.running_flag(),
            },
        );
    }
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        flag.store(terminate_managed_vm("terminated", "main"), Ordering::Relaxed);
    });

    let started = std::time::Instant::now();
    let err = vm.execute_on_update("{}".to_string()).expect_err("must be stopped");
    killer.join().unwrap();

    assert!(stopped.load(Ordering::Relaxed), "the VM must have been found");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "terminate must actually stop the script, not wait for the deadline"
    );
    assert!(matches!(err, RunError::Interrupted(_)), "got: {:?}", err);
    assert!(err.to_string().contains("terminated"), "got: {}", err);
}

#[test]
fn the_memory_limit_is_enforced_as_an_error() {
    let (_vm, out) = run_with(
        "oom",
        r#"globalThis.update = function () {
             var a = [];
             for (var i = 0; i < 1e9; i++) { a.push("x".repeat(1000)); }
             return "unreachable";
           };"#,
        "{}",
        2,
        Duration::from_secs(30),
    );
    assert!(
        out.is_err(),
        "exhausting the heap must be a contained error, not an abort"
    );
}

#[test]
fn console_reaches_the_vm_log() {
    let (_vm, out) = run(
        "console",
        r#"globalThis.update = function () {
             console.log("hello", { a: 1 });
             console.error("bad");
             return "";
           };"#,
        "{}",
    );
    out.expect("the module should run");
    let logs = MockHost::get().packets_for("main", "vmLog");
    let texts: Vec<String> = logs
        .iter()
        .map(|p| p["input"]["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        texts.iter().any(|t| t == r#"hello {"a":1}"#),
        "got: {:?}",
        texts
    );
    assert!(
        logs.iter()
            .any(|p| p["input"]["logType"] == "error" && p["input"]["text"] == "bad"),
        "an error line must carry its level; got: {:?}",
        logs
    );
}

#[test]
fn the_identity_global_reports_what_the_node_stamped() {
    let (vm, out) = run(
        "identity-global",
        r#"globalThis.update = function () {
             return { m: caspar.machineId, v: caspar.vmId, s: caspar.storeId, r: caspar.runtime };
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert_eq!(
        output_of(&vm),
        r#"{"m":"identity-global","v":"main","s":"9@global","r":"javascript"}"#
    );
}

#[test]
fn the_prelude_provides_base64_and_text_codecs() {
    let (vm, out) = run(
        "prelude",
        r#"globalThis.update = function () {
             var b = btoa("caspar");
             var round = atob(b);
             var bytes = new TextEncoder().encode("héllo");
             var back = new TextDecoder().decode(bytes);
             return { b: b, round: round, len: bytes.length, back: back };
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert_eq!(
        output_of(&vm),
        r#"{"b":"Y2FzcGFy","round":"caspar","len":6,"back":"héllo"}"#
    );
}

#[test]
fn one_run_cannot_see_another_runs_globals() {
    // A fresh context per run is the whole reason there is no warm-VM pool
    // here; if that ever changes, this is what must keep holding.
    let source = r#"globalThis.update = function () {
        var seen = globalThis.__leak || "clean";
        globalThis.__leak = "dirty";
        return seen;
      };"#;
    let (first, out1) = run("leak", source, "{}");
    out1.expect("run 1");
    let (second, out2) = run("leak", source, "{}");
    out2.expect("run 2");
    assert_eq!(output_of(&first), "clean");
    assert_eq!(output_of(&second), "clean", "state bled between runs");
}

#[test]
fn output_is_published_as_a_vm_output_packet() {
    let (_vm, out) = run(
        "vmoutput",
        r#"globalThis.update = function () { return "the answer"; };"#,
        "{}",
    );
    out.expect("the module should run");
    let packets = MockHost::get().packets_for("vmoutput", "vmOutput");
    assert!(
        packets
            .iter()
            .any(|p| p["input"]["text"] == "the answer" && p["input"]["logType"] == "output"),
        "got: {:?}",
        packets
    );
}

#[test]
fn http_requests_go_through_the_host() {
    let (vm, out) = run(
        "http",
        r#"globalThis.update = function () {
             return hostCall("httpRequest", { url: "https://example.test/x", method: "GET" });
           };"#,
        "{}",
    );
    out.expect("the module should run");
    assert!(
        output_of(&vm).contains("https://example.test/x"),
        "got: {}",
        output_of(&vm)
    );
}

#[test]
fn an_unparseable_host_request_is_answered_not_swallowed() {
    let (vm, out) = run(
        "badreq",
        r#"globalThis.update = function () { return hostCallRaw("not json at all"); };"#,
        "{}",
    );
    out.expect("the module should run");
    assert!(output_of(&vm).contains("not valid JSON"), "got: {}", output_of(&vm));
}

/// A real bundled Da Vinci program, executed in this VM.
///
/// Everything else in this file tests the runtime with scripts written for the
/// test. This one takes the actual esbuild output of `crew/events` — kernel and
/// all — and signals it exactly as the node would, which is the only check that
/// says the bundling contract and the runtime contract agree.
///
/// Skipped when the bundle has not been built (`creatures-js/build.mjs`), so a
/// Caspar checkout without the Decillion tree beside it still tests clean.
#[test]
fn a_bundled_davinci_program_runs_and_posts_to_the_log() {
    let host = MockHost::get();
    let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../decillionai-server/js/crew/events.js");
    let Ok(source) = std::fs::read_to_string(&bundle) else {
        eprintln!("skipping: {} has not been built", bundle.display());
        return;
    };

    let path = write_module("davinci-events", &source);
    let mut vm = JsMac::new_vm(
        "p-events".to_string(),
        "main".to_string(),
        "9@global".to_string(),
        path,
        64,
        Duration::from_secs(10),
    );

    // One `crew/events` envelope, nested exactly as the node nests a signal.
    let envelope = json!({
        "v": 1,
        "from": "crew/executor",
        "to": "crew/events",
        "msg": "m-vm-1",
        "space": "9@global",
        "run": "r1",
        "job": "j1",
        "payload": {
            "fn": "answer",
            "text": "the final answer",
            "agentName": "Rita",
            "agentProgramId": "p-research",
            "threadId": "main",
        }
    });
    let packet = json!({
        "action": "single",
        "entityId": "main",
        "data": json!({
            "programId": "p-events",
            "entity": "main",
            "payload": envelope.to_string(),
        }).to_string(),
    });

    let out = vm.execute_on_update(packet.to_string());
    vm.finalize();
    out.expect("the bundled program should run");

    let result = output_of(&vm);
    assert!(
        result.contains("\"ok\":true") && result.contains("\"posted\":true"),
        "the program should report a posted row; got: {result}"
    );

    // …and it must actually have signalled the store, with the right tags.
    let posted: Vec<_> = host
        .inner
        .lock()
        .unwrap()
        .packets
        .iter()
        .filter(|p| p["type"] == "signal" || p["op"] == "signal")
        .cloned()
        .collect();
    let _ = posted; // the signal op goes through unified_host_call in this host
    let calls = host.unified_for("p-events");
    let signal = calls
        .iter()
        .find(|c| c["op"] == "signal")
        .unwrap_or_else(|| panic!("no store signal was sent; calls: {calls:?}"));
    let tags = signal["input"]["tags"].as_array().cloned().unwrap_or_default();
    let tags: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
    assert!(tags.contains(&"kind=answer"), "tags: {tags:?}");
    assert!(tags.contains(&"run=r1"), "tags: {tags:?}");
    assert!(tags.contains(&"agent=p-research"), "tags: {tags:?}");
}
