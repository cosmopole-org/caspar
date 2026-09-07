//! The javascript VM controller.
//!
//! JavaScript program entities currently execute in the managed wasm VM flow,
//! so run/terminate delegate to whichever plugin provides the platform's
//! default (wasm) runtime — resolved dynamically through the SDK registry,
//! never by a key hardcoded in the Caspar node.

use elpify_lang::transpile_js_to_masm;
use serde_json::{json, Value as JsonValue};

use caspar_vm_sdk::{registry, VmPlugin, VmPluginMeta};

pub struct JavascriptVmController {
    meta: VmPluginMeta,
}

impl JavascriptVmController {
    pub fn new(meta: VmPluginMeta) -> Self {
        Self { meta }
    }

    /// The runtime javascript execution is layered on: the platform default.
    fn backing_runtime(&self) -> Result<String, String> {
        registry::default_key()
            .ok_or_else(|| "no default runtime registered to back the javascript vm".to_string())
    }
}

impl VmPlugin for JavascriptVmController {
    fn meta(&self) -> &VmPluginMeta {
        &self.meta
    }

    fn run_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        registry::run_on(&self.backing_runtime()?, packet)
    }

    fn terminate_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        registry::terminate_on(&self.backing_runtime()?, packet)
    }

    /// The javascript runtime executes on a backing runtime (its transpiled
    /// MASM program), so a delete is delegated there exactly as run and
    /// terminate are — there is no javascript-side instance of its own to
    /// destroy.
    fn delete_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        registry::delete_on(&self.backing_runtime()?, packet)
    }

    fn exec_vm(&self, packet: &JsonValue) -> Result<JsonValue, String> {
        let script_path = packet["astPath"].as_str().unwrap_or("");
        if script_path.is_empty() {
            return Err("astPath is required for javascript runtime".to_string());
        }
        let _ = transpile_js_to_masm(script_path)
            .map_err(|e| format!("javascript transpile failed: {}", e))?;
        Ok(json!({"ok": true, "runtime": "javascript", "astPath": script_path}))
    }
}
