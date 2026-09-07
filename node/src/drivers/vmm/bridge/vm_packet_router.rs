//! The unified VM packet router.
//!
//! Every VM lifecycle packet is dispatched dynamically to a registered VM
//! plugin through the `caspar_vm_sdk` registry — the node never names a VM
//! type. Runtime resolution uses, in order: the packet's `runtime` field,
//! its `vmType` hint, plugin artifact-extension detection on the resolved
//! module path, and finally the registered default runtime.

use crate::drivers::vmm::prelude::*;
use crate::drivers::vmm::bridge::vm_packet_schema::{VmPacketKind, VmPacketContext};
use crate::drivers::vmm::bridge::gateway_control_api::handle_gateway_control_api;
use crate::drivers::vmm::bridge::runtime_io::log;
use crate::drivers::vmm::host::vm_host_functions::handle_unified_host_call;

use caspar_vm_sdk::registry as vm_registry;

pub(crate) use caspar_vm_sdk::util::panic_message;

pub fn route_vm_packet(packet: &JsonValue) -> String {
    let owned = packet.clone();
    let dispatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let env = VmPacketContext::from_packet(&owned);
        match env.packet_type {
            VmPacketKind::RunVm => dispatch_run_vm_packet(&owned, &env),
            VmPacketKind::TerminateVm => dispatch_terminate_vm_packet(&owned, &env),
            VmPacketKind::DeleteVm => dispatch_delete_vm_packet(&owned, &env),
            VmPacketKind::ExecVm => dispatch_exec_packet(&owned, |p, pkt| p.exec_vm(pkt)),
            VmPacketKind::CopyToVm => dispatch_exec_packet(&owned, |p, pkt| p.copy_to_vm(pkt)),
            VmPacketKind::BuildVmImage => {
                dispatch_exec_packet(&owned, |p, pkt| p.build_image(pkt))
            }
            VmPacketKind::HostCall => handle_unified_host_call(&owned),
            VmPacketKind::VerifyProgramExecution => dispatch_verify_program_packet(&owned),
            VmPacketKind::ForwardHttp => dispatch_forward_http_packet(&owned),
            VmPacketKind::ApiResponse => {
                json!({"ok": true, "skipped": "in-process mode"}).to_string()
            }
            VmPacketKind::BridgeApi => match handle_gateway_control_api(&owned) {
                Ok(payload) => payload.to_string(),
                Err(err) => json!({"ok": false, "error": err}).to_string(),
            },
            VmPacketKind::Unknown => {
                // Host-API envelopes carry the op under "key" instead of
                // "type" (the appengine callback protocol): hand them to the
                // VMM's key-based dispatcher so signal / createProgram /
                // deployEntity / vmLog / plantTrigger… actually execute.
                let key = owned["key"].as_str().unwrap_or("");
                if !key.is_empty() {
                    match crate::drivers::vmm::globals::with_global_app(|app| {
                        app.tools().vmm().vm_callback(&owned.to_string()).0
                    }) {
                        Some(res) => res,
                        None => json!({"ok": false, "error": "vmm not initialised"})
                            .to_string(),
                    }
                } else {
                    json!({
                        "ok": false,
                        "error": format!(
                            "unsupported packet type: {}",
                            owned["type"].as_str().unwrap_or("")
                        )
                    })
                    .to_string()
                }
            }
        }
    }));
    match dispatch {
        Ok(s) => s,
        Err(payload) => {
            let msg = panic_message(&payload);
            log(format!("vmm dispatcher panic contained: {}", msg));
            json!({"ok": false, "error": format!("vmm panic: {}", msg)}).to_string()
        }
    }
}

fn dispatch_run_vm_packet(packet: &JsonValue, env: &VmPacketContext) -> String {
    let ast_path = packet["astPath"].as_str().unwrap_or("");
    let plugin = match vm_registry::resolve_for_packet(packet, ast_path) {
        Some(p) => p,
        None => {
            return json!({"ok": false, "error": "no VM runtime plugins are registered"})
                .to_string()
        }
    };
    match plugin.run_vm(packet) {
        Ok(res) => res.to_string(),
        // runEntity launches VMs fire-and-forget, so this Result is often
        // discarded by the caller; without logging here a failed launch is
        // completely silent and the creature merely "never serves".
        Err(err) => {
            log(format!(
                "run_vm({}) failed: machine={} vm={} err={}",
                plugin.meta().key,
                env.machine_id,
                env.vm_id,
                err
            ));
            json!({"ok": false, "error": err}).to_string()
        }
    }
}

fn dispatch_terminate_vm_packet(packet: &JsonValue, env: &VmPacketContext) -> String {
    if let Some(plugin) = vm_registry::get(&env.runtime) {
        return match plugin.terminate_vm(packet) {
            Ok(res) => res.to_string(),
            Err(err) => json!({"ok": false, "error": err}).to_string(),
        };
    }
    // No (resolvable) runtime on the packet: ask every in-process runtime to
    // stop its instances of this machine — each plugin no-ops when it has
    // none. This preserves the legacy "terminate managed VM" semantics.
    let mut results: Vec<JsonValue> = Vec::new();
    for plugin in vm_registry::plugins() {
        if plugin.meta().in_process {
            match plugin.terminate_vm(packet) {
                Ok(res) => results.push(res),
                Err(err) => results.push(json!({
                    "ok": false,
                    "runtime": plugin.meta().key,
                    "error": err,
                })),
            }
        }
    }
    json!({"ok": true, "machineId": env.machine_id, "results": results}).to_string()
}

/// Permanently destroy one VM instance.
///
/// Unlike terminate, a delete never fans out across runtimes when the packet
/// names none: destroying every in-process instance of a machine because the
/// caller forgot a `runtime` field would delete VMs nobody asked about. An
/// unresolvable runtime is an error here, not a broadcast.
fn dispatch_delete_vm_packet(packet: &JsonValue, env: &VmPacketContext) -> String {
    let plugin = match vm_registry::get(&env.runtime) {
        Some(p) => p,
        None => {
            let ast_path = packet["astPath"].as_str().unwrap_or("");
            match vm_registry::resolve_for_packet(packet, ast_path) {
                Some(p) => p,
                None => {
                    return json!({
                        "ok": false,
                        "error": "deleteVm requires a resolvable runtime",
                    })
                    .to_string()
                }
            }
        }
    };
    match plugin.delete_vm(packet) {
        Ok(res) => res.to_string(),
        Err(err) => {
            log(format!(
                "delete_vm({}) failed: machine={} vm={} err={}",
                plugin.meta().key,
                env.machine_id,
                env.vm_id,
                err
            ));
            json!({"ok": false, "error": err}).to_string()
        }
    }
}

fn dispatch_exec_packet<F>(packet: &JsonValue, op: F) -> String
where
    F: FnOnce(
        &std::sync::Arc<dyn caspar_vm_sdk::VmPlugin>,
        &JsonValue,
    ) -> Result<JsonValue, String>,
{
    let plugin = match vm_registry::resolve_for_exec(packet) {
        Some(p) => p,
        None => {
            return json!({"ok": false, "error": "no VM runtime plugins are registered"})
                .to_string()
        }
    };
    match op(&plugin, packet) {
        Ok(res) => res.to_string(),
        Err(err) => json!({"ok": false, "error": err}).to_string(),
    }
}

fn dispatch_forward_http_packet(packet: &JsonValue) -> String {
    // Resolve the plugin exactly as `runVm` does: the ingress fills the packet's
    // `vmType`/`astPath` from the entity's recorded runtime, so `resolve_for_packet`
    // (vmType hint → artifact detection → default) picks the same plugin `runVm`
    // would for this entity.
    let ast_path = packet["astPath"].as_str().unwrap_or("");
    let plugin = match vm_registry::resolve_for_packet(packet, ast_path) {
        Some(p) => p,
        None => {
            return json!({"ok": false, "error": "no VM runtime plugins are registered"})
                .to_string()
        }
    };
    match plugin.forward_http(packet) {
        Ok(res) => res.to_string(),
        Err(err) => json!({"ok": false, "error": err}).to_string(),
    }
}

fn dispatch_verify_program_packet(packet: &JsonValue) -> String {
    let plugin = match vm_registry::verifier_plugin() {
        Some(p) => p,
        None => {
            return json!({
                "ok": false,
                "error": "no registered VM runtime provides program execution verification"
            })
            .to_string()
        }
    };
    match plugin.verify_program_execution(packet) {
        Ok(res) => res.to_string(),
        Err(err) => json!({"ok": false, "error": err}).to_string(),
    }
}
