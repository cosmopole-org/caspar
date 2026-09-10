//! The QuickJS-backed managed VM runtime (`JsMac`).
//!
//! One VM is one QuickJS runtime and one context, built for a single run and
//! torn down at the end of it. There is no warm-VM pool here (unlike the wasm
//! runtime): building a QuickJS context is microseconds rather than the tens of
//! milliseconds WasmEdge's Config/Store/Executor construction costs, so the
//! pooling machinery would buy nothing and would reintroduce the one problem a
//! fresh context makes impossible — state bleeding from one signal into the
//! next.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Runtime};
use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::host::{host, log};
use caspar_vm_sdk::trx::Trx;

use crate::host_calls;

/// The guest prelude, evaluated before every program module.
const PRELUDE: &str = include_str!("prelude.js");

/// QuickJS's own default stack limit is generous for a browser and far too
/// generous for a node thread; a runaway recursion should be a guest error,
/// not a segfault in the host.
const MAX_STACK_BYTES: usize = 1024 * 1024;

/// How often the interrupt handler is allowed to consult the clock. QuickJS
/// calls it very frequently (roughly per backward jump / call), and asking the
/// OS for the time on every one of those is a measurable tax on a tight loop.
const INTERRUPT_CLOCK_EVERY: u32 = 4096;

// ── The live-VM registry ─────────────────────────────────────────────────────
//
// `terminateVm` has to reach a run that is already executing. For wasm that can
// only ever be a cooperative request, because its sync executor exposes no
// preemption. QuickJS does expose one — the interrupt handler — so a terminate
// here genuinely stops the script, at the next interrupt check, wherever it is.

pub struct ManagedVmHandle {
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) running: Arc<AtomicBool>,
}

impl ManagedVmHandle {
    pub fn terminate_vm_instance(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn global_managed_vms() -> &'static Arc<Mutex<HashMap<String, ManagedVmHandle>>> {
    static CELL: OnceLock<Arc<Mutex<HashMap<String, ManagedVmHandle>>>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Key a running VM by machine AND vm id.
///
/// The wasm registry is keyed by machine alone, which is safe there only
/// because a wasm signal always runs as `main`. A javascript program may have
/// several named instances of one entity alive at once, and a terminate that
/// named one of them would otherwise stop whichever happened to be registered.
pub(crate) fn vm_key(machine_id: &str, vm_id: &str) -> String {
    format!("{}::{}", machine_id, vm_id)
}

/// Signal a stop to the managed javascript VM of `machine_id`/`vm_id`.
/// Returns whether a live VM was actually found.
pub fn terminate_managed_vm(machine_id: &str, vm_id: &str) -> bool {
    let mut map = global_managed_vms().lock().unwrap();
    if let Some(handle) = map.remove(&vm_key(machine_id, vm_id)) {
        handle.terminate_vm_instance();
        if handle.running.load(Ordering::Relaxed) {
            log(format!(
                "terminate requested for running javascript vm: {} vm={} (interrupt armed)",
                machine_id, vm_id
            ));
        }
        return true;
    }
    false
}

/// Stop every live VM of one machine, whatever their vm ids.
/// Used by a terminate that names no instance.
pub fn terminate_managed_machine(machine_id: &str) -> usize {
    let prefix = format!("{}::", machine_id);
    let mut map = global_managed_vms().lock().unwrap();
    let keys: Vec<String> = map
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    let mut stopped = 0;
    for k in keys {
        if let Some(handle) = map.remove(&k) {
            handle.terminate_vm_instance();
            stopped += 1;
        }
    }
    stopped
}

// ── VM state ─────────────────────────────────────────────────────────────────

/// The mutable half of a running VM — everything a host call may read or write.
///
/// Held behind `Rc<RefCell<_>>` because QuickJS native functions are `Fn`
/// closures that may be called re-entrantly from the guest, and the whole run
/// happens on one thread. Nothing here crosses a thread boundary, so a mutex
/// would only buy the chance to deadlock against ourselves.
pub struct JsState {
    pub machine_id: String,
    pub vm_id: String,
    pub store_id: String,
    /// Write-ahead buffer for raw `dbOp` calls (shared with the wasm runtime).
    pub trx: Trx,
    /// Whether this VM opened a per-lifecycle JSON transaction on the host.
    /// Lazily set on the first putJson/getJson/getByPrefix/delKey host call and
    /// closed (committing) either on an explicit `commitTrx` host call or at VM
    /// teardown inside [`JsMac::finalize`].
    pub vm_trx_open: bool,
    /// Set by the `output` host op. Takes precedence over update()'s return
    /// value, so a creature ported from wasm behaves identically.
    pub execution_result: String,
    pub has_output: bool,
}

/// Why a run ended. Anything but `Ok` is surfaced to the host as a `vmOutput`
/// error packet, never as a silent drop.
#[derive(Debug)]
pub enum RunError {
    /// The script threw, or failed to compile.
    Guest(String),
    /// The deadline elapsed or `terminateVm` tripped the stop flag.
    Interrupted(String),
    /// The runtime itself could not be built or the module could not be read.
    Runtime(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Guest(m) | RunError::Interrupted(m) | RunError::Runtime(m) => {
                write!(f, "{}", m)
            }
        }
    }
}

pub struct JsMac {
    pub state: Rc<RefCell<JsState>>,
    pub mod_path: String,
    pub ram_limit_mb: u64,
    pub max_exec: Duration,
    stop_: Arc<AtomicBool>,
    running_: Arc<AtomicBool>,
}

impl JsMac {
    pub fn new_vm(
        machine_id: String,
        vm_id: String,
        store_id: String,
        mod_path: String,
        ram_limit_mb: u64,
        max_exec: Duration,
    ) -> Self {
        JsMac {
            state: Rc::new(RefCell::new(JsState {
                machine_id,
                vm_id,
                store_id,
                trx: Trx::new(),
                vm_trx_open: false,
                execution_result: String::new(),
                has_output: false,
            })),
            mod_path,
            ram_limit_mb: ram_limit_mb.max(1),
            max_exec,
            stop_: Arc::new(AtomicBool::new(false)),
            running_: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_)
    }

    pub(crate) fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running_)
    }

    /// Close the VM out: commit its transactions and publish its output.
    ///
    /// Runs on every exit path, success or failure, exactly as the wasm runtime
    /// does — a half-open write buffer that outlives the VM it belonged to is
    /// the one state this must never leave behind.
    pub fn finalize(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.vm_trx_open {
            if let Some(h) = host() {
                h.end_vm_json_trx(&state.vm_id);
            }
            state.vm_trx_open = false;
        }
        state.trx.commit_as_offchain();
        if state.has_output {
            let payload = json!({
                "key": "vmOutput",
                "input": {
                    "text": state.execution_result.clone(),
                    "data": state.execution_result.clone(),
                    "vmId": state.vm_id.clone(),
                    "machineId": state.machine_id.clone(),
                    "logType": "output",
                }
            });
            if let Some(h) = host() {
                let _ = h.dispatch(&payload);
            }
        }
    }

    /// Execute the deployed module against `input`.
    ///
    /// The whole run — prelude, module evaluation, `update()`, and draining the
    /// promise job queue — happens under one interrupt handler, so a script
    /// cannot escape the deadline by moving its loop into a `.then()`.
    pub fn execute_on_update(&mut self, input: String) -> Result<(), RunError> {
        let mod_path = self.mod_path.trim().to_string();
        if mod_path.is_empty() {
            return Err(RunError::Runtime(
                "javascript module path is empty (no astPath resolved for this entity); \
                 deploy the entity before signalling it"
                    .to_string(),
            ));
        }
        let source = std::fs::read_to_string(&mod_path).map_err(|e| {
            RunError::Runtime(format!(
                "could not read javascript module {}: {}",
                mod_path, e
            ))
        })?;

        let rt = Runtime::new()
            .map_err(|e| RunError::Runtime(format!("could not create a javascript runtime: {}", e)))?;
        rt.set_memory_limit((self.ram_limit_mb as usize).saturating_mul(1024 * 1024));
        rt.set_max_stack_size(MAX_STACK_BYTES);

        // The one preemption point. It is consulted by QuickJS on backward
        // jumps and calls, which is what makes `terminateVm` and the exec
        // deadline real here rather than advisory: `while(true){}` stops.
        let stop = self.stop_flag();
        let deadline = Instant::now() + self.max_exec;
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_cb = Arc::clone(&timed_out);
        let mut ticks: u32 = 0;
        rt.set_interrupt_handler(Some(Box::new(move || {
            if stop.load(Ordering::Relaxed) {
                return true;
            }
            ticks = ticks.wrapping_add(1);
            if ticks % INTERRUPT_CLOCK_EVERY != 0 {
                return false;
            }
            if Instant::now() >= deadline {
                timed_out_cb.store(true, Ordering::Relaxed);
                return true;
            }
            false
        })));

        let ctx = Context::full(&rt)
            .map_err(|e| RunError::Runtime(format!("could not create a javascript context: {}", e)))?;

        self.running_.store(true, Ordering::Relaxed);
        let outcome = self.run_in_context(&ctx, &rt, &source, &mod_path, &input);
        self.running_.store(false, Ordering::Relaxed);

        // Drop the interrupt handler before the runtime goes: it borrows the
        // stop flag, and leaving it armed on a runtime we are tearing down is
        // an easy way to interrupt somebody else's future run.
        rt.set_interrupt_handler(None);

        match outcome {
            Ok(()) => Ok(()),
            Err(RunError::Guest(msg)) if self.stopped_or_timed_out(&timed_out) => {
                Err(RunError::Interrupted(self.interrupt_reason(&timed_out, &msg)))
            }
            other => other,
        }
    }

    fn stopped_or_timed_out(&self, timed_out: &Arc<AtomicBool>) -> bool {
        self.stop_.load(Ordering::Relaxed) || timed_out.load(Ordering::Relaxed)
    }

    fn interrupt_reason(&self, timed_out: &Arc<AtomicBool>, detail: &str) -> String {
        if timed_out.load(Ordering::Relaxed) {
            format!(
                "javascript execution exceeded its time limit of {}s and was interrupted ({})",
                self.max_exec.as_secs(),
                detail
            )
        } else {
            format!("javascript execution was terminated ({})", detail)
        }
    }

    fn run_in_context(
        &self,
        ctx: &Context,
        rt: &Runtime,
        source: &str,
        mod_path: &str,
        input: &str,
    ) -> Result<(), RunError> {
        let state = Rc::clone(&self.state);
        let identity = {
            let s = state.borrow();
            json!({
                "machineId": s.machine_id,
                "programId": s.machine_id,
                "vmId": s.vm_id,
                "storeId": s.store_id,
            })
            .to_string()
        };

        // Install the natives, the prelude and the module. Everything the guest
        // can reach is set up here and nowhere else.
        ctx.with(|ctx| -> Result<(), RunError> {
            let globals = ctx.globals();

            let host_state = Rc::clone(&state);
            let host_call = Function::new(ctx.clone(), move |request: String| -> String {
                host_calls::dispatch(&host_state, &request)
            })
            .map_err(|e| RunError::Runtime(format!("could not install hostCall: {}", e)))?;
            globals
                .set("__caspar_hostCall", host_call)
                .map_err(|e| RunError::Runtime(format!("could not install hostCall: {}", e)))?;

            let log_state = Rc::clone(&state);
            let log_fn = Function::new(ctx.clone(), move |text: String, level: String| {
                let vm_id = log_state.borrow().vm_id.clone();
                caspar_vm_sdk::host::log_vm(text, vm_id, &level);
            })
            .map_err(|e| RunError::Runtime(format!("could not install console: {}", e)))?;
            globals
                .set("__caspar_log", log_fn)
                .map_err(|e| RunError::Runtime(format!("could not install console: {}", e)))?;

            globals
                .set("__caspar_identity", identity)
                .map_err(|e| RunError::Runtime(format!("could not install identity: {}", e)))?;

            eval_script(&ctx, PRELUDE, "caspar:prelude.js")
                .map_err(|e| RunError::Runtime(format!("the guest prelude failed: {}", e)))?;

            // The program's own bundle. A creature entity is one self-contained
            // script (esbuild's `iife` format) that assigns `globalThis.update`.
            // It is evaluated as a SCRIPT, not a module: an ES module's exports
            // would not be reachable from the host, and there is no loader to
            // resolve an import against anyway.
            eval_script(&ctx, source, mod_path).map_err(RunError::Guest)?;

            Ok(())
        })?;

        // Invoke, then drain. `__caspar_invoke` parks the outcome rather than
        // returning it, so a promise and a plain value finish the same way.
        ctx.with(|ctx| -> Result<(), RunError> {
            let globals = ctx.globals();
            let invoke: Function = globals
                .get("__caspar_invoke")
                .map_err(|e| RunError::Runtime(format!("the guest prelude is missing: {}", e)))?;
            invoke
                .call::<_, ()>((input.to_string(),))
                .map_err(|e| RunError::Guest(exception_message(&ctx, e)))?;
            Ok(())
        })?;

        // Promise jobs, under the same interrupt handler as everything else.
        loop {
            match rt.execute_pending_job() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(_) => {
                    // A job threw. QuickJS reports the failure without the
                    // context, and an unhandled rejection has already been
                    // recorded by the shim's rejection handler, so keep
                    // draining: the parked result is the authority.
                    continue;
                }
            }
        }

        // Read what the run settled on.
        let settled = ctx.with(|ctx| -> Result<String, RunError> {
            let globals = ctx.globals();
            let result: Function = globals
                .get("__caspar_result")
                .map_err(|e| RunError::Runtime(format!("the guest prelude is missing: {}", e)))?;
            result
                .call::<_, String>(())
                .map_err(|e| RunError::Guest(exception_message(&ctx, e)))
        })?;

        let parsed: JsonValue = serde_json::from_str(&settled).unwrap_or_else(|_| json!({}));
        match parsed["state"].as_str().unwrap_or("") {
            "ok" => {
                // An explicit `output` host call wins, so a creature ported
                // from wasm keeps its exact semantics; the return value is the
                // ergonomic path for one written for this runtime.
                let mut s = self.state.borrow_mut();
                if !s.has_output {
                    s.execution_result = parsed["text"].as_str().unwrap_or("").to_string();
                    s.has_output = true;
                }
                Ok(())
            }
            "pending" => Err(RunError::Guest(
                parsed["error"]
                    .as_str()
                    .unwrap_or("the entity's update() never settled")
                    .to_string(),
            )),
            _ => Err(RunError::Guest(
                parsed["error"]
                    .as_str()
                    .unwrap_or("the entity's update() failed without a message")
                    .to_string(),
            )),
        }
    }
}

/// Evaluate one script in `ctx`, returning the guest's own error text.
fn eval_script(ctx: &Ctx<'_>, source: &str, name: &str) -> Result<(), String> {
    let mut options = rquickjs::context::EvalOptions::default();
    options.strict = false;
    options.global = true;
    options.promise = false;
    match ctx.eval_with_options::<(), _>(source, options) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("{}: {}", name, exception_message(ctx, e))),
    }
}

/// Turn a QuickJS error into something a person can act on.
///
/// `rquickjs::Error::Exception` carries no message of its own — the thrown
/// value is left on the context and has to be taken from it, which is where the
/// stack lives. Reporting the bare error instead yields the string "exception",
/// which says nothing at all about what a creature did wrong.
fn exception_message(ctx: &Ctx<'_>, err: rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(exception) = caught.as_exception() {
            let message = exception.message().unwrap_or_default();
            let stack = exception.stack().unwrap_or_default();
            let mut out = if message.is_empty() {
                exception.to_string()
            } else {
                message
            };
            if !stack.is_empty() {
                out.push('\n');
                out.push_str(&stack);
            }
            return out;
        }
        return caught
            .as_string()
            .and_then(|s| s.to_string().ok())
            .unwrap_or_else(|| "uncaught exception".to_string());
    }
    err.to_string()
}
