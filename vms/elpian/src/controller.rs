//! The elpian VM controller — VmPlugin implementation over the elpian
//! task runtime.

use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::util::{emit_vm_error, panic_message, parse_vm_resource_limits};
use caspar_vm_sdk::{VmPlugin, VmPluginMeta};

use crate::runtime::execute_elpian_task;

pub struct ElpianVmController {
    meta: VmPluginMeta,
}

impl ElpianVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }

    fn start(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main").to_string();
        let ast_path = packet["astPath"].as_str().unwrap_or("").to_string();
        let input = packet["input"].as_str().unwrap_or("{}").to_string();
        let limits = parse_vm_resource_limits(packet);

        // Contain panics so a malformed AST can never take down the node; the
        // failure is surfaced as a vmOutput error packet instead.
        let machine_for_task = machine_id.to_string();
        let vm_for_task = vm_id.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_elpian_task(&machine_for_task, vm_for_task, ast_path, input, limits)
        }));
        match res {
            Ok(Ok(())) => Ok(json!({"ok": true, "runtime": "elpian", "machineId": machine_id})),
            Ok(Err(e)) => {
                emit_vm_error(machine_id, &vm_id, "elpian", &e);
                Err(format!(
                    "elpian task failed for machine {}: {}",
                    machine_id, e
                ))
            }
            Err(payload) => {
                let msg = panic_message(&payload);
                emit_vm_error(machine_id, &vm_id, "elpian", &format!("panic: {}", msg));
                Err(format!("elpian panic: {}", msg))
            }
        }
    }
}

impl VmPlugin for ElpianVmController {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.start(packet)
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        // Elpian tasks run to completion synchronously; ensure any live VM
        // instance for this machine is destroyed.
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let _ = elpian_vm::api::destroy_vm(machine_id.to_string());
        Ok(json!({"ok": true, "runtime": "elpian", "machineId": machine_id}))
    }

    /// Elpian programs run to completion synchronously, so a delete is a
    /// destroy of any live instance plus the disposal of the lifecycle
    /// transaction and execution context the run left behind.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let machine_id = packet["machineId"].as_str().unwrap_or("");
        if machine_id.is_empty() {
            return Err("machineId is required".to_string());
        }
        let vm_id = packet["vmId"].as_str().unwrap_or("main");
        let _ = elpian_vm::api::destroy_vm(machine_id.to_string());
        if let Some(h) = caspar_vm_sdk::host::host() {
            h.end_vm_json_trx(vm_id);
            h.commit_vm_buffer(vm_id);
            h.unregister_vm_context(vm_id);
        }
        Ok(json!({
            "ok": true,
            "runtime": "elpian",
            "machineId": machine_id,
            "vmId": vm_id,
            "deleted": true,
        }))
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        self.start(packet)
    }
}
