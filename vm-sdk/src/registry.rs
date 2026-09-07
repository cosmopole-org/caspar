//! The global VM plugin registry.
//!
//! Plugins are registered at node start-up by the build-time-generated
//! aggregation crate; afterwards the Caspar node resolves every runtime
//! operation dynamically through this registry — no VM key is ever named in
//! the node's own code.

use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::plugin::VmPlugin;
use crate::util::normalize_runtime;

static REGISTRY: RwLock<Vec<Arc<dyn VmPlugin>>> = RwLock::new(Vec::new());

/// Register (or replace) a plugin. Called by each VM project's `register()`
/// entry point via the generated aggregation crate.
pub fn register_plugin(plugin: Arc<dyn VmPlugin>) {
    {
        let mut reg = REGISTRY.write().unwrap();
        let key = plugin.meta().key.clone();
        reg.retain(|p| p.meta().key != key);
        reg.push(plugin.clone());
    }
    plugin.init();
}

/// Every registered plugin, in registration order.
pub fn plugins() -> Vec<Arc<dyn VmPlugin>> {
    REGISTRY.read().unwrap().clone()
}

/// Canonical keys of every registered runtime.
pub fn keys() -> Vec<String> {
    REGISTRY
        .read()
        .unwrap()
        .iter()
        .map(|p| p.meta().key.clone())
        .collect()
}

/// Look up a plugin by canonical key or alias.
pub fn get(runtime: &str) -> Option<Arc<dyn VmPlugin>> {
    let hint = normalize_runtime(runtime);
    if hint.is_empty() {
        return None;
    }
    let reg = REGISTRY.read().unwrap();
    reg.iter()
        .find(|p| p.meta().key == hint)
        .or_else(|| reg.iter().find(|p| p.meta().matches_key(&hint)))
        .cloned()
}

/// Resolve a runtime hint (key or alias) to its canonical key.
pub fn resolve_key(runtime: &str) -> Option<String> {
    get(runtime).map(|p| p.meta().key.clone())
}

/// The fallback runtime plugin (`defaultRuntime: true` in its config).
pub fn default_plugin() -> Option<Arc<dyn VmPlugin>> {
    REGISTRY
        .read()
        .unwrap()
        .iter()
        .find(|p| p.meta().default_runtime)
        .cloned()
}

/// Canonical key of the fallback runtime, when one is registered.
pub fn default_key() -> Option<String> {
    default_plugin().map(|p| p.meta().key.clone())
}

/// Resolve the plugin responsible for a VM packet: explicit `runtime` field,
/// then `vmType` hint, then artifact-extension detection, then the default
/// runtime.
pub fn resolve_for_packet(packet: &Value, artifact_path: &str) -> Option<Arc<dyn VmPlugin>> {
    for field in ["runtime", "vmType"] {
        if let Some(hint) = packet[field].as_str() {
            if let Some(p) = get(hint) {
                return Some(p);
            }
        }
    }
    let hint_runtime = packet["runtime"].as_str().unwrap_or("");
    let hint_vm_type = packet["vmType"].as_str().unwrap_or("");
    {
        let reg = REGISTRY.read().unwrap();
        for p in reg.iter() {
            if p.detect(hint_runtime, artifact_path) || p.detect(hint_vm_type, artifact_path) {
                return Some(p.clone());
            }
        }
    }
    default_plugin()
}

/// The plugin handling exec/copy/build packets that carry no resolvable
/// runtime hint (legacy container ABI), falling back to the default runtime.
pub fn exec_fallback_plugin() -> Option<Arc<dyn VmPlugin>> {
    let reg = REGISTRY.read().unwrap();
    reg.iter()
        .find(|p| p.meta().exec_fallback)
        .cloned()
        .or_else(|| reg.iter().find(|p| p.meta().default_runtime).cloned())
}

/// The plugin providing program-execution proof verification, if any.
pub fn verifier_plugin() -> Option<Arc<dyn VmPlugin>> {
    REGISTRY
        .read()
        .unwrap()
        .iter()
        .find(|p| p.meta().provides_program_verification)
        .cloned()
}

/// Resolve the plugin for an exec/copy/build packet: explicit runtime hint
/// first, then a runtime derived from a legacy packet-type alias suffix
/// (`execDocker` → `docker`), then the exec-fallback plugin.
pub fn resolve_for_exec(packet: &Value) -> Option<Arc<dyn VmPlugin>> {
    for field in ["runtime", "vmType"] {
        if let Some(hint) = packet[field].as_str() {
            if let Some(p) = get(hint) {
                return Some(p);
            }
        }
    }
    // Legacy alias suffix: "execDocker" / "copyToDocker" / "buildDockerImage".
    if let Some(t) = packet["type"].as_str() {
        for prefix in ["exec", "copyTo", "build"] {
            if let Some(rest) = t.strip_prefix(prefix) {
                let hint = rest.trim_end_matches("Image");
                if let Some(p) = get(hint) {
                    return Some(p);
                }
            }
        }
    }
    exec_fallback_plugin()
}

/// Whether `runtime` names a registered VM type (by key or alias).
pub fn is_supported(runtime: &str) -> bool {
    get(runtime).is_some()
}

/// Whether `runtime` executes inside the node process (managed runtime).
pub fn is_managed(runtime: &str) -> bool {
    get(runtime).map(|p| p.meta().in_process).unwrap_or(false)
}

/// Whether `runtime` executes grouped chain transactions.
pub fn supports_chain_trxs(runtime: &str) -> bool {
    get(runtime)
        .map(|p| p.meta().supports_chain_trxs)
        .unwrap_or(false)
}

/// Cross-plugin delegation: run a packet on another registered runtime.
/// Used by plugins whose execution model is layered on a sibling runtime
/// (e.g. a language VM compiled down to a wasm module).
pub fn run_on(runtime: &str, packet: &Value) -> Result<Value, String> {
    get(runtime)
        .ok_or_else(|| format!("runtime '{}' is not registered", runtime))?
        .run_vm(packet)
}

/// Cross-plugin delegation for terminate.
pub fn terminate_on(runtime: &str, packet: &Value) -> Result<Value, String> {
    get(runtime)
        .ok_or_else(|| format!("runtime '{}' is not registered", runtime))?
        .terminate_vm(packet)
}

/// Cross-plugin delegation for delete.
pub fn delete_on(runtime: &str, packet: &Value) -> Result<Value, String> {
    get(runtime)
        .ok_or_else(|| format!("runtime '{}' is not registered", runtime))?
        .delete_vm(packet)
}
