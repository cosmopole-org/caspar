//! The javascript VM controller — `VmPlugin` over the QuickJS managed runtime.
//!
//! Structurally this is the wasm controller: a run happens on its own thread,
//! every panic is contained inside it and surfaced as a `vmOutput` error
//! packet, and the VM slot is always released. The difference is that QuickJS
//! exposes an interrupt hook, so this runtime's exec deadline and its
//! `terminateVm` actually stop a running script instead of asking it to stop.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use elpify_lang::transpile_js_to_masm;
use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::host::{host, log, set_log_vm_context};
use caspar_vm_sdk::util::{emit_vm_error, panic_message, parse_vm_resource_limits};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

use crate::runtime::{
    global_managed_vms, terminate_managed_machine, terminate_managed_vm, vm_key, JsMac,
    ManagedVmHandle, RunError,
};

const RUNTIME_KEY: &str = "javascript";

/// Spawn a **cancellable** exec-timeout watchdog for one run.
///
/// The runtime's own interrupt handler is what enforces the deadline inside the
/// script; this watchdog exists for the case the handler cannot reach — a run
/// blocked in a host call (an outbound HTTP request, a resource lock) rather
/// than in JavaScript. Tripping the stop flag means the interrupt fires the
/// instant control returns to the guest.
///
/// Cancellable for the same reason the wasm one is: a VM is built per signal,
/// so a naked `thread::sleep(timeout)` would leave roughly `rate × timeout`
/// sleeping threads alive under sustained load.
fn spawn_exec_watchdog(
    stop_flag: Arc<AtomicBool>,
    timeout: Duration,
    on_timeout: impl FnOnce() + Send + 'static,
) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
            if !stop_flag.swap(true, Ordering::Relaxed) {
                on_timeout();
            }
        }
    });
    (tx, handle)
}

pub struct JavascriptVmController {
    meta: VmPluginMeta,
}

impl JavascriptVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }
}

impl VmPlugin for JavascriptVmController {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let ast_path = packet["astPath"].as_str().unwrap_or("").trim().to_string();
        // Fail fast, before a thread is spawned: a run with no module is an
        // ordinary error, and reporting it from inside the VM thread would turn
        // a deploy mistake into an asynchronous mystery.
        if ast_path.is_empty() {
            return Err(format!(
                "javascript run requires a module path (astPath); none resolved for machine={} vm={}",
                packet["machineId"].as_str().unwrap_or(""),
                packet["vmId"].as_str().unwrap_or("main")
            ));
        }
        let input = packet["input"].as_str().unwrap_or("{}").to_string();
        let machine_id = packet["machineId"].as_str().unwrap_or("").to_string();
        let vm_id = packet["vmId"].as_str().unwrap_or("main").to_string();
        let limits = parse_vm_resource_limits(packet);

        let spawn_machine = machine_id.clone();
        let spawn_vm = vm_id.clone();
        thread::spawn(move || {
            let key = vm_key(&machine_id, &vm_id);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                set_log_vm_context(&vm_id);
                let input_json: JsonValue =
                    serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
                let store_id = input_json
                    .get("store")
                    .and_then(|x| x.get("id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();

                let mut rt = JsMac::new_vm(
                    machine_id.clone(),
                    vm_id.clone(),
                    store_id,
                    ast_path.clone(),
                    limits.ram_mb,
                    Duration::from_secs(limits.max_exec_time_secs),
                );
                {
                    let mut map = global_managed_vms().lock().unwrap();
                    map.insert(
                        key.clone(),
                        ManagedVmHandle {
                            stop: rt.stop_flag(),
                            running: rt.running_flag(),
                        },
                    );
                }
                let timeout_machine = machine_id.clone();
                let timeout_vm = vm_id.clone();
                let timeout_secs = limits.max_exec_time_secs;
                let (done_tx, watchdog) = spawn_exec_watchdog(
                    rt.stop_flag(),
                    // A grace beat past the in-guest deadline: the interrupt
                    // handler is the primary enforcement and reports a far
                    // better error, so the watchdog should only ever win when
                    // the guest is blocked somewhere the interrupt cannot reach.
                    Duration::from_secs(timeout_secs) + Duration::from_secs(1),
                    move || {
                        log(format!(
                            "javascript vm timeout reached: machine={} vm={} limit={}s",
                            timeout_machine, timeout_vm, timeout_secs
                        ));
                    },
                );
                let exec_res = rt.execute_on_update(input);
                rt.finalize();
                drop(done_tx);
                let _ = watchdog.join();
                exec_res
            }));

            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    let kind = match err {
                        RunError::Interrupted(_) => "interrupted",
                        RunError::Runtime(_) => "runtime",
                        RunError::Guest(_) => "error",
                    };
                    let msg = err.to_string();
                    log(format!(
                        "javascript vm {}: machine={} vm={} err={}",
                        kind, spawn_machine, spawn_vm, msg
                    ));
                    emit_vm_error(&spawn_machine, &spawn_vm, RUNTIME_KEY, &msg);
                }
                Err(panic_payload) => {
                    let msg = panic_message(&panic_payload);
                    log(format!(
                        "javascript vm panicked: machine={} vm={} panic={}",
                        spawn_machine, spawn_vm, msg
                    ));
                    emit_vm_error(
                        &spawn_machine,
                        &spawn_vm,
                        RUNTIME_KEY,
                        &format!("panic: {}", msg),
                    );
                }
            }

            let mut map = global_managed_vms().lock().unwrap();
            map.remove(&vm_key(&spawn_machine, &spawn_vm));
        });

        Ok(json!({
            "ok": true,
            "runtime": RUNTIME_KEY,
            "machineId": packet["machineId"].as_str().unwrap_or(""),
            "vmId": packet["vmId"].as_str().unwrap_or("main"),
        }))
    }

    /// Stop a running VM.
    ///
    /// A terminate naming a `vmId` stops that instance; one naming only a
    /// machine stops every instance of it, because that is what the caller can
    /// have meant — a javascript program may have several named instances of an
    /// entity alive at once, and silently stopping an arbitrary one would be
    /// worse than stopping none.
    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("").trim();
        let stopped = if vm_id.is_empty() {
            terminate_managed_machine(machine_id)
        } else {
            usize::from(terminate_managed_vm(machine_id, vm_id))
        };
        Ok(json!({
            "ok": true,
            "runtime": RUNTIME_KEY,
            "machineId": machine_id,
            "vmId": if vm_id.is_empty() { JsonValue::Null } else { json!(vm_id) },
            "stopped": stopped,
        }))
    }

    /// Permanently destroy a javascript VM.
    ///
    /// A javascript "VM" is a QuickJS context that lives for one run, so a
    /// delete is a terminate plus closing what outlives the context: the
    /// lifecycle JSON transaction (a half-open write buffer must never outlive
    /// the VM that opened it), the buffered writes, and the execution-context
    /// entry.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        terminate_managed_vm(machine_id, vm_id);
        if let Some(h) = host() {
            h.end_vm_json_trx(vm_id);
            h.commit_vm_buffer(vm_id);
            h.unregister_vm_context(vm_id);
        }
        let mut artifact_removed = false;
        if packet["removeArtifact"].as_bool().unwrap_or(false) {
            let ast_path = packet["astPath"].as_str().unwrap_or("").trim();
            if !ast_path.is_empty() {
                artifact_removed = std::fs::remove_file(ast_path).is_ok();
            }
        }
        Ok(json!({
            "ok": true,
            "runtime": RUNTIME_KEY,
            "machineId": machine_id,
            "vmId": vm_id,
            "deleted": true,
            "artifactRemoved": artifact_removed,
        }))
    }

    fn status_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        let map = global_managed_vms().lock().unwrap();
        let running = map
            .get(&vm_key(machine_id, vm_id))
            .map(|h| h.running.load(Ordering::Relaxed))
            .unwrap_or(false);
        Ok(json!({
            "runtime": RUNTIME_KEY,
            "status": if running { "running" } else { "stopped" },
            "running": running,
        }))
    }

    /// Validate a deployed script by transpiling it to MASM.
    ///
    /// This is the provable-execution path (`elpify`), not the execution path:
    /// a script that fails here still runs perfectly well on QuickJS. It is
    /// kept because it is the one thing this plugin already did, it costs
    /// nothing, and it is the only pre-flight check the runtime can offer.
    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let script_path = packet["astPath"].as_str().unwrap_or("");
        if script_path.is_empty() {
            return Err("astPath is required for javascript runtime".to_string());
        }
        let _ = transpile_js_to_masm(script_path)
            .map_err(|e| format!("javascript transpile failed: {}", e))?;
        Ok(json!({"ok": true, "runtime": RUNTIME_KEY, "astPath": script_path}))
    }

    /// Plan a standalone `runVm` for a deployed javascript entity.
    ///
    /// The SDK default omits `astPath`, which would launch a VM with no module
    /// at all. The runtime records the module path on deploy
    /// (`setEntityLinksOnDeploy`) at `vmEntityPath::{machineId}::{entityId}`,
    /// so resolve it here and put it in the launch input.
    fn plan_run_entity(&self, ctx: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = ctx["machineId"].as_str().unwrap_or("");
        let entity_id = ctx["entityId"].as_str().unwrap_or("");
        let params = ctx["params"].clone();
        let data = serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string());

        let mut ast_path = String::new();
        if !machine_id.is_empty() && !entity_id.is_empty() {
            if let Some(h) = host() {
                ast_path = h
                    .state_get(&format!("vmEntityPath::{}::{}", machine_id, entity_id))
                    .trim()
                    .to_string();
            }
        }

        Ok(json!({
            "input": {
                "runtime": self.meta().key,
                "machineId": ctx["machineId"],
                "entityId": ctx["entityId"],
                "standalone": true,
                "vmId": ctx["vmId"],
                "resources": ctx["resources"],
                "astPath": ast_path,
                "data": data,
            },
            "links": [],
        }))
    }

    /// A javascript entity is a single self-contained bundle, so there is
    /// nothing to build — but a deployed file that is not readable is a broken
    /// deploy, and saying so at build time beats discovering it on the first
    /// signal.
    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let path = packet["astPath"]
            .as_str()
            .or_else(|| packet["imageBuildPath"].as_str())
            .or_else(|| packet["path"].as_str())
            .unwrap_or("")
            .trim();
        if path.is_empty() {
            return Ok(json!({"ok": true, "runtime": RUNTIME_KEY, "build": "noop"}));
        }
        if !Path::new(path).is_file() {
            return Err(format!("javascript entity is not a readable file: {}", path));
        }
        Ok(json!({"ok": true, "runtime": RUNTIME_KEY, "build": "noop", "astPath": path}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn watchdog_wakes_immediately_when_the_run_completes() {
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, handle) = spawn_exec_watchdog(stop.clone(), Duration::from_secs(3600), || {
            panic!("timeout callback must not fire on normal completion");
        });
        drop(done_tx);
        let t = Instant::now();
        handle.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(!stop.load(Ordering::Relaxed));
    }

    #[test]
    fn watchdog_trips_stop_on_a_real_timeout() {
        let stop = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let fired_cb = fired.clone();
        let (done_tx, handle) =
            spawn_exec_watchdog(stop.clone(), Duration::from_millis(50), move || {
                fired_cb.store(true, Ordering::Relaxed);
            });
        handle.join().unwrap();
        assert!(stop.load(Ordering::Relaxed));
        assert!(fired.load(Ordering::Relaxed));
        drop(done_tx);
    }
}
