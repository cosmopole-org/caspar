//! Identity and configuration types for the modal runtime.

use serde_json::Value as JsonValue;

/// The identity fields every modal packet addresses a sandbox with.
///
/// A modal VM has no node-local name (no container, no socket), so the vm id
/// *is* the identity: the sandbox id it maps to lives in node state under
/// `ModalSandbox::<vmId>`, written when the sandbox is created and read back
/// by every later operation.
pub(crate) struct ModalIdentity {
    pub(crate) machine_id: String,
    pub(crate) entity_id: String,
    pub(crate) vm_id: String,
    pub(crate) creature_id: String,
}

impl ModalIdentity {
    pub(crate) fn from_packet(packet: &JsonValue) -> Self {
        let vm_id = packet["vmId"].as_str().unwrap_or("").trim();
        Self {
            machine_id: packet["machineId"].as_str().unwrap_or("").trim().to_string(),
            entity_id: packet["entityId"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("main")
                .trim()
                .to_string(),
            vm_id: if vm_id.is_empty() {
                "main".to_string()
            } else {
                vm_id.to_string()
            },
            creature_id: packet["creatureId"]
                .as_str()
                .or_else(|| packet["userId"].as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
        }
    }
}

// ── State link keys ───────────────────────────────────────────────────────
//
// The modal runtime keeps no in-process registry of its VMs: a sandbox
// outlives the node process that started it, so the mapping has to survive a
// restart. These are the only keys it owns.

pub(crate) fn sandbox_link_key(vm_id: &str) -> String {
    format!("ModalSandbox::{}", vm_id)
}

pub(crate) fn volume_link_key(vm_id: &str) -> String {
    format!("ModalVolume::{}", vm_id)
}

pub(crate) fn app_link_key(machine_id: &str) -> String {
    format!("ModalApp::{}", machine_id)
}

pub(crate) fn image_link_key(machine_id: &str, entity_id: &str) -> String {
    format!("ModalImage::{}::{}", machine_id, entity_id)
}

/// Deterministic Modal app name for a Caspar machine. Modal app names allow a
/// limited character set, so the machine id (which carries `@` and `:` for
/// federated creatures) is sanitized rather than passed through.
pub(crate) fn modal_app_name(machine_id: &str) -> String {
    let prefix =
        std::env::var("MODAL_APP_PREFIX").unwrap_or_else(|_| "caspar".to_string());
    format!("{}-{}", prefix, sanitize_component(machine_id))
}

/// Deterministic Modal volume name for one VM instance.
pub(crate) fn modal_volume_name(vm_id: &str) -> String {
    let prefix =
        std::env::var("MODAL_APP_PREFIX").unwrap_or_else(|_| "caspar".to_string());
    format!("{}-vol-{}", prefix, sanitize_component(vm_id))
}

/// Reduce an arbitrary Caspar id to the `[a-z0-9-]` alphabet Modal accepts for
/// resource names, keeping it stable (the same id always maps to the same
/// name, which is what makes get-or-create idempotent).
pub(crate) fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        // Modal caps resource names; keep the tail, which is the part that
        // actually distinguishes two ids sharing a prefix.
        if trimmed.len() > 48 {
            trimmed[trimmed.len() - 48..].trim_matches('-').to_string()
        } else {
            trimmed
        }
    }
}
