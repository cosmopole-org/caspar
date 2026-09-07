//! The wasm VM controller — VmPlugin implementation over the WasmEdge
//! managed runtime.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::host::{host, log, set_log_vm_context};
use caspar_vm_sdk::util::{emit_vm_error, panic_message, parse_vm_resource_limits};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

use crate::runtime::{global_managed_vms, terminate_managed_vm, ManagedVmHandle, WasmMac};

/// Spawn a **cancellable** exec-timeout watchdog for one wasm run.
///
/// Returns a sender and the watchdog's join handle. Dropping (or sending on)
/// the sender wakes the watchdog immediately, so a run that finishes in
/// milliseconds reaps its watchdog at once instead of leaving a thread asleep
/// for the full `timeout`. Only if the sender is still alive when `timeout`
/// elapses — a genuinely runaway run — does the watchdog trip `stop_flag`
/// (once) and invoke `on_timeout`.
///
/// This is the fix for the per-signal thread pile-up: a wasm signal spawns a
/// fresh VM every time (no keep-alive), and the previous naked
/// `thread::sleep(timeout)` watchdog stayed alive for the whole `timeout` after
/// every run, so under sustained load ~`rate × timeout` watchdog threads were
/// live at once — hundreds of MB of stacks, ending in thread/memory exhaustion.
fn spawn_exec_watchdog(
    stop_flag: Arc<AtomicBool>,
    timeout: Duration,
    on_timeout: impl FnOnce() + Send + 'static,
) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        // `Timeout` ⇒ the run is still going at the deadline (runaway); any
        // other outcome (`Ok`/`Disconnected`) means the run completed and
        // signalled/dropped the sender, so exit quietly without tripping stop.
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
            // `swap` trips the flag exactly once; only the winner logs.
            if !stop_flag.swap(true, Ordering::Relaxed) {
                on_timeout();
            }
        }
    });
    (tx, handle)
}

pub struct WasmVmController {
    meta: VmPluginMeta,
}

impl WasmVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }
}

impl VmPlugin for WasmVmController {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    /// Spawn a managed wasm VM on its own thread. Every panic and every
    /// wasmedge failure is contained inside that thread, surfaced as a
    /// vmOutput error packet, and the VM slot is always released at the end.
    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let ast_path = packet["astPath"].as_str().unwrap_or("").to_string();
        // Reject an unresolved module path up front, before spawning the VM
        // thread. Loading a wasm module from an empty path aborts the whole
        // node inside WasmEdge's C++ loader (see runtime::execute_on_update),
        // so a missing astPath must fail fast as an ordinary error here.
        if ast_path.trim().is_empty() {
            return Err(format!(
                "wasm run requires a module path (astPath); none resolved for machine={} vm={}",
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                set_log_vm_context(&vm_id);
                let inp1 = input.clone();
                let input_json: JsonValue =
                    serde_json::from_str(&inp1).unwrap_or_else(|_| json!({}));
                let store_id = input_json
                    .get("store")
                    .and_then(|x| x.get("id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();

                let mut rt = WasmMac::new_vm(
                    machine_id.clone(),
                    vm_id.clone(),
                    store_id,
                    ast_path.clone(),
                    limits.ram_mb,
                    Box::new(|packet: JsonValue| match host() {
                        Some(h) => h.dispatch(&packet),
                        None => json!({"ok": false, "error": "caspar vm host is not initialised"})
                            .to_string(),
                    }),
                );
                {
                    let mut map = global_managed_vms().lock().unwrap();
                    map.insert(
                        machine_id.clone(),
                        ManagedVmHandle {
                            stop: rt.stop_flag(),
                            running: rt.running_flag(),
                        },
                    );
                }
                // Cancellable exec-timeout watchdog. A wasm signal spawns a
                // fresh VM every time (no keep-alive), so the watchdog must be
                // woken the instant the run finishes — otherwise it lingers for
                // the full `max_exec_time_secs` (default 60s) after a run that
                // actually took milliseconds. Under sustained signalling that
                // piled up ~one sleeping thread per invocation (rate × 60s live
                // at once), leaking hundreds of MB of thread stacks until the
                // node exhausted threads/memory and crashed.
                //
                // `recv_timeout` returns `Timeout` only if the run is still
                // going at the deadline (a genuine runaway → trip the stop
                // flag); when the run completes we drop `done_tx`, so the
                // watchdog wakes immediately (`Disconnected`) and exits, and we
                // join it so no thread is left detached.
                let timeout_machine = machine_id.clone();
                let timeout_vm = vm_id.clone();
                let timeout_secs = limits.max_exec_time_secs;
                let (done_tx, watchdog) = spawn_exec_watchdog(
                    rt.stop_flag(),
                    Duration::from_secs(timeout_secs),
                    move || {
                        log(format!(
                            "wasm vm timeout reached: machine={} vm={} limit={}s",
                            timeout_machine, timeout_vm, timeout_secs
                        ));
                    },
                );
                let exec_res = rt.execute_on_update(inp1);
                rt.finalize();
                // Wake the watchdog now that the run is done, then reap it. On a
                // panic in the run above, `catch_unwind` unwinds through here:
                // `done_tx` still drops (waking the watchdog) and the join
                // handle detaches, so the watchdog exits promptly either way.
                drop(done_tx);
                let _ = watchdog.join();
                exec_res
            }));

            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    log(format!(
                        "wasm vm error: machine={} vm={} err={}",
                        spawn_machine, spawn_vm, err
                    ));
                    emit_vm_error(&spawn_machine, &spawn_vm, "wasm", &err);
                }
                Err(panic_payload) => {
                    let msg = panic_message(&panic_payload);
                    log(format!(
                        "wasm vm panicked: machine={} vm={} panic={}",
                        spawn_machine, spawn_vm, msg
                    ));
                    emit_vm_error(
                        &spawn_machine,
                        &spawn_vm,
                        "wasm",
                        &format!("panic: {}", msg),
                    );
                }
            }

            let mut map = global_managed_vms().lock().unwrap();
            map.remove(&spawn_machine);
        });

        Ok(json!({
            "ok": true,
            "runtime": "wasm",
            "machineId": packet["machineId"].as_str().unwrap_or(""),
            "vmId": packet["vmId"].as_str().unwrap_or("main"),
        }))
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        terminate_managed_vm(machine_id);
        Ok(json!({"ok": true, "runtime": "wasm", "machineId": machine_id}))
    }

    /// Permanently destroy a wasm VM.
    ///
    /// A wasm "VM" is a module instance the node keeps warm, so deleting it
    /// means terminating the instance, closing its lifecycle transaction (a
    /// half-open write buffer would otherwise outlive the VM it belonged to),
    /// and dropping the execution-context entry. With `removeArtifact` the
    /// module's cached AOT shared object goes too, so a later deploy of the
    /// same path recompiles rather than loading the deleted VM's code.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        terminate_managed_vm(machine_id);
        if let Some(h) = host() {
            h.end_vm_json_trx(vm_id);
            h.commit_vm_buffer(vm_id);
            h.unregister_vm_context(vm_id);
        }
        let mut artifact_removed = false;
        if packet["removeArtifact"].as_bool().unwrap_or(false) {
            let ast_path = packet["astPath"].as_str().unwrap_or("").trim();
            if !ast_path.is_empty() {
                artifact_removed = std::fs::remove_file(format!("{}.so", ast_path)).is_ok();
            }
        }
        Ok(json!({
            "ok": true,
            "runtime": "wasm",
            "machineId": machine_id,
            "vmId": vm_id,
            "deleted": true,
            "artifactRemoved": artifact_removed,
        }))
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let ast_path = packet["astPath"].as_str().unwrap_or("").to_string();
        if ast_path.is_empty() {
            return Err("astPath is required".to_string());
        }
        Ok(json!({"ok": true, "runtime": "wasm", "astPath": ast_path}))
    }

    /// Plan a standalone `runVm` for a deployed wasm entity.
    ///
    /// The generic SDK default omits `astPath`, which would launch the VM with
    /// an empty module path (and abort the node in WasmEdge's loader). The wasm
    /// runtime records the module path on deploy (`setEntityLinksOnDeploy`) at
    /// the link `vmEntityPath::{machineId}::{entityId}`, so resolve it here and
    /// include it in the launch input.
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

    /// Wasm "image build": run the entity's `build.sh` script (which compiles
    /// the creature sources into `module.wasm`).
    fn build_image(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let build_path = packet["imageBuildPath"]
            .as_str()
            .or_else(|| packet["dockerfilePath"].as_str())
            .or_else(|| packet["path"].as_str())
            .unwrap_or("");
        if build_path.is_empty() {
            return Err("build path is required".to_string());
        }
        let script_path = resolve_script_path(build_path, "build.sh")?;
        run_local_build_script(&script_path)?;
        Ok(json!({
            "ok": true,
            "machineId": machine_id,
            "scriptPath": script_path,
            "runtime": "wasm"
        }))
    }
}

fn resolve_script_path(path: &str, default_script_name: &str) -> Result<String, String> {
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(format!("build path does not exist: {}", path));
    }
    if path_ref.is_file() {
        return Ok(path.to_string());
    }
    let script_path = path_ref.join(default_script_name);
    if !script_path.exists() {
        return Err(format!(
            "required build script not found at {}",
            script_path.to_string_lossy()
        ));
    }
    Ok(script_path.to_string_lossy().to_string())
}

fn run_local_build_script(script_path: &str) -> Result<(), String> {
    let script_ref = Path::new(script_path);
    let script_name = script_ref
        .file_name()
        .ok_or_else(|| format!("invalid build script path: {}", script_path))?
        .to_string_lossy()
        .to_string();
    let cwd = script_ref
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let output = Command::new("bash")
        .arg(script_name)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to execute wasm build script {}: {}", script_path, e))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "wasm build script failed (status={}): {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    // The core of the leak fix: a completed run must reap its watchdog at once
    // (no lingering sleeping thread), and must not trip the stop flag.
    #[test]
    fn watchdog_wakes_immediately_when_the_run_completes() {
        let stop = Arc::new(AtomicBool::new(false));
        // A one-hour timeout — before the fix this thread would sleep the whole
        // hour after the run finished.
        let (done_tx, handle) =
            spawn_exec_watchdog(stop.clone(), Duration::from_secs(3600), || {
                panic!("timeout callback must not fire on normal completion");
            });
        drop(done_tx); // the run finished
        let t = Instant::now();
        handle.join().unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "watchdog lingered after completion instead of waking immediately"
        );
        assert!(
            !stop.load(Ordering::Relaxed),
            "stop flag must not trip when the run completed in time"
        );
    }

    // The watchdog must still kill a genuinely runaway run.
    #[test]
    fn watchdog_trips_stop_on_a_real_timeout() {
        let stop = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let fired_cb = fired.clone();
        let (done_tx, handle) =
            spawn_exec_watchdog(stop.clone(), Duration::from_millis(50), move || {
                fired_cb.store(true, Ordering::Relaxed);
            });
        // Keep the sender alive past the deadline (simulating a hung run).
        handle.join().unwrap();
        assert!(stop.load(Ordering::Relaxed), "stop flag must trip on timeout");
        assert!(fired.load(Ordering::Relaxed), "on_timeout must fire on timeout");
        drop(done_tx);
    }
}
