//! Which creature owns a VM instance, and therefore who may delete it.
//!
//! `runVm` is deliberately un-ACL'd — a creature launches VMs freely — but a
//! *delete* is destructive and final, so it needs an owner to check against.
//! The owner is recorded at launch, by the same host op that created the VM,
//! and read back here. The link is written from the node (never from a
//! packet field a guest supplies), so a creature cannot claim a VM it did not
//! start.
//!
//! Two shapes of ownership exist, and a delete accepts either:
//!
//!   * `VmOwnerProgram::<vmId>` — written by the `runVm` host op with the
//!     node-resolved calling program. This is the creature-owns-its-VM case
//!     the delete host op enforces.
//!   * `VmInstance::<programId>::<entityId>::<vmId>` — written by
//!     `/programs/runEntity` for a deployed program entity. A VM launched
//!     that way is owned by its program, so that program (and the shell
//!     action's own owner check) may delete it.

use crate::drivers::vmm::globals::with_global_app;
use crate::models::transaction::ITrx;

fn owner_link_key(vm_id: &str) -> String {
    format!("VmOwnerProgram::{}", vm_id)
}

/// Record `program_id` as the owner of `vm_id`. No-op for an empty id, so a
/// runtime that never returned a vm id cannot write a wildcard entry.
pub(crate) fn record_vm_owner(vm_id: &str, program_id: &str) {
    let vm_id = vm_id.trim();
    let program_id = program_id.trim();
    if vm_id.is_empty() || program_id.is_empty() {
        return;
    }
    let key = owner_link_key(vm_id);
    let value = program_id.to_string();
    with_global_app(|app| {
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                trx.put_link(&key, &value);
                Ok(())
            }),
        );
    });
}

/// The program that launched `vm_id`, or an empty string when none was
/// recorded (a VM launched before this registry existed, or by the program
/// API rather than the host op).
pub(crate) fn vm_owner_program(vm_id: &str) -> String {
    let vm_id = vm_id.trim();
    if vm_id.is_empty() {
        return String::new();
    }
    let key = owner_link_key(vm_id);
    let slot = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let slot_c = slot.clone();
    with_global_app(|app| {
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                *slot_c.lock().unwrap() = trx.get_link(&key);
                Ok(())
            }),
        );
    });
    let owner = slot.lock().unwrap().clone();
    owner.trim().to_string()
}

/// Whether `program_id` launched `vm_id` as a program *entity*
/// (`/programs/runEntity`), which records `VmInstance::<program>::<entity>::<vm>`
/// rather than an owner link.
pub(crate) fn owns_vm_instance(program_id: &str, vm_id: &str) -> bool {
    let program_id = program_id.trim();
    let vm_id = vm_id.trim();
    if program_id.is_empty() || vm_id.is_empty() {
        return false;
    }
    let prefix = format!("VmInstance::{}::", program_id);
    let suffix = format!("::{}", vm_id);
    let found = std::sync::Arc::new(std::sync::Mutex::new(false));
    let found_c = found.clone();
    with_global_app(|app| {
        app.modify_state(
            true,
            Box::new(move |trx: &dyn ITrx| {
                if let Ok(links) = trx.get_links_list(&prefix, -1, -1, &[]) {
                    *found_c.lock().unwrap() = links.iter().any(|l| l.ends_with(&suffix));
                }
                Ok(())
            }),
        );
    });
    let hit = *found.lock().unwrap();
    hit
}

/// Drop every state link a deleted VM leaves behind. Called after the runtime
/// has actually destroyed the instance, so a failed delete does not orphan a
/// still-running VM by forgetting who owns it.
pub(crate) fn clear_vm_records(vm_id: &str, program_id: &str) {
    let vm_id = vm_id.trim().to_string();
    if vm_id.is_empty() {
        return;
    }
    let owner_key = owner_link_key(&vm_id);
    let instance_prefix = if program_id.trim().is_empty() {
        String::new()
    } else {
        format!("VmInstance::{}::", program_id.trim())
    };
    let vm_id_for_trx = vm_id.clone();
    with_global_app(|app| {
        app.modify_state(
            false,
            Box::new(move |trx: &dyn ITrx| {
                trx.del_key(&format!("link::{}", owner_key));
                trx.del_key(&format!("link::VmStatus::{}", vm_id_for_trx));
                trx.del_key(&format!("link::VmStartedAt::{}", vm_id_for_trx));
                trx.del_key(&format!("link::VmBilling::{}", vm_id_for_trx));
                trx.del_key(&format!("link::vmDistributed::{}", vm_id_for_trx));
                trx.del_json(&format!("Json::VmBilling::{}", vm_id_for_trx), "payment");
                if !instance_prefix.is_empty() {
                    let suffix = format!("::{}", vm_id_for_trx);
                    if let Ok(links) = trx.get_links_list(&instance_prefix, -1, -1, &[]) {
                        for link in links {
                            if link.ends_with(&suffix) {
                                trx.del_key(&format!("link::{}", link));
                            }
                        }
                    }
                }
                Ok(())
            }),
        );
    });
}
