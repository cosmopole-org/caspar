use crate::drivers::vmm::host::functions::vm_ownership::{
    owns_vm_instance, program_owner_user, vm_owner_program,
};
use crate::drivers::vmm::prelude::*;

/// `vmEndpoints` host op — the public URLs a running VM is reachable on.
///
/// A creature that launched a VM often needs to hand a person the address of
/// something running inside it: a desktop, a preview server, a notebook.
/// Whether such an address exists, and what it looks like, is the runtime's
/// business — a cloud sandbox publishes tunnels, a local container does not —
/// so this asks the plugin rather than encoding any of it here.
///
/// Access control matches `deleteVm`: the launching program or a sibling
/// creature of the same owner. A VM's public address is not secret in the way
/// a token is, but it is the address of somebody's machine, and a creature
/// that did not start it has no business enumerating it.
pub(crate) fn host_fn_vm_endpoints(caller_program_id: &str, input: &JsonValue) -> String {
    let vm_id = input["vmId"].as_str().unwrap_or("").trim().to_string();
    if vm_id.is_empty() {
        return json!({"ok": false, "error": "vmEndpoints requires a vmId"}).to_string();
    }
    let caller = caller_program_id.trim().to_string();
    if caller.is_empty() {
        return json!({"ok": false, "error": "vmEndpoints requires an identified caller"})
            .to_string();
    }

    let owner = vm_owner_program(&vm_id);
    let authorized = if !owner.is_empty() {
        owner == caller || {
            let owner_user = program_owner_user(&owner);
            !owner_user.is_empty() && owner_user == program_owner_user(&caller)
        }
    } else {
        owns_vm_instance(&caller, &vm_id)
    };
    if !authorized {
        return json!({"ok": false, "error": "you are not the owner of this vm"}).to_string();
    }

    let runtime = caspar_vm_sdk::util::normalize_runtime(input["runtime"].as_str().unwrap_or(""));
    let Some(plugin) = caspar_vm_sdk::registry::get(&runtime) else {
        return json!({
            "ok": false,
            "error": "vmEndpoints requires a resolvable runtime",
        })
        .to_string();
    };
    match plugin.vm_endpoints(input) {
        Ok(res) => res.to_string(),
        Err(err) => json!({"ok": false, "error": err}).to_string(),
    }
}
