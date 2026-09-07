use crate::drivers::vmm::host::functions::vm_ownership::{
    clear_vm_records, owns_vm_instance, vm_owner_program,
};
use crate::drivers::vmm::prelude::*;

/// Unified `deleteVm` host op — the destructive counterpart of `runVm`.
///
/// `terminateVm` suspends: the instance can be brought back by a later run,
/// and its persistent volume survives. `deleteVm` is final — the runtime
/// destroys the instance and everything it owns, and the node drops the state
/// links that described it.
///
/// **Access control.** A VM may only be deleted by the creature that created
/// it. The owner was recorded by `runVm` at launch (`VmOwnerProgram::<vmId>`),
/// or by `/programs/runEntity` as a program instance link; `caller_program_id`
/// is the node-resolved calling program, not a packet field, so a creature
/// cannot present someone else's id. A VM with no recorded owner at all is
/// refused rather than allowed: an unknown owner is not the same as no owner,
/// and a delete is not recoverable.
pub(crate) fn host_fn_delete_vm(caller_program_id: &str, input: &JsonValue) -> String {
    let vm_id = input["vmId"].as_str().unwrap_or("").trim().to_string();
    if vm_id.is_empty() {
        return json!({"ok": false, "error": "deleteVm requires a vmId"}).to_string();
    }
    let caller = caller_program_id.trim().to_string();
    if caller.is_empty() {
        return json!({"ok": false, "error": "deleteVm requires an identified caller"})
            .to_string();
    }

    let owner = vm_owner_program(&vm_id);
    let authorized = if !owner.is_empty() {
        owner == caller
    } else {
        owns_vm_instance(&caller, &vm_id)
    };
    if !authorized {
        return json!({
            "ok": false,
            "error": "you are not the owner of this vm",
        })
        .to_string();
    }

    // Ask the runtime's own plugin to shape the delete packet: the container
    // name, sandbox id or entity fields a given runtime needs to address one
    // concrete instance are the plugin's business, not this dispatcher's.
    let runtime = caspar_vm_sdk::util::normalize_runtime(input["runtime"].as_str().unwrap_or(""));
    let packet = match caspar_vm_sdk::registry::get(&runtime) {
        Some(plugin) => match plugin.build_delete_request(input) {
            Ok(p) => p,
            Err(err) => return json!({"ok": false, "error": err}).to_string(),
        },
        None => {
            let mut p = input.clone();
            if let Some(obj) = p.as_object_mut() {
                obj.insert("type".to_string(), JsonValue::String("deleteVm".to_string()));
                obj.insert("purge".to_string(), JsonValue::Bool(true));
                obj.insert("delete".to_string(), JsonValue::Bool(true));
            }
            p
        }
    };

    let raw = crate::drivers::vmm::dispatch_packet(&packet);
    // Only forget the VM once the runtime actually destroyed it — clearing the
    // owner link after a failed delete would strand a live VM nobody may
    // delete any more.
    let destroyed = serde_json::from_str::<JsonValue>(&raw)
        .ok()
        .map(|v| v["ok"].as_bool().unwrap_or(false))
        .unwrap_or(false);
    if destroyed {
        clear_vm_records(&vm_id, &caller);
    }
    raw
}
