//! Caspar VM plugin: JavaScript runtime (`javascript`).
//!
//! A creature written in JavaScript is one self-contained bundle deployed as
//! the entity `module.js`. It runs in-process on QuickJS and reaches the
//! platform through exactly one import — `hostCall` — which speaks the same
//! `{op, input}` protocol, with the same op table, as the wasm runtime's.

mod controller;
mod host_calls;
mod runtime;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use caspar_vm_sdk::{registry, VmPluginMeta};

pub use controller::JavascriptVmController;

/// Register this VM type with the Caspar VMM plugin registry.
/// Invoked by the build-time-generated plugin aggregation crate.
pub fn register() {
    let meta = VmPluginMeta::from_config_str(include_str!("../vm.config.json"))
        .expect("caspar-vm-javascript: invalid vm.config.json");
    registry::register_plugin(Arc::new(JavascriptVmController::new(meta)));
}
