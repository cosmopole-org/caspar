//! Caspar VM plugin: Modal cloud sandbox runtime (`modal`).
//!
//! Where the docker runtime supervises containers on the node's own machine
//! and fire boots microVMs on it, this runtime has no local process at all:
//! a VM is a **Modal sandbox** running in Modal's cloud, addressed over
//! Modal's gRPC control plane. Everything the platform expects of a runtime —
//! run, terminate, delete, status, exec, file transfer, inbound HTTP — is
//! implemented against that API, so a modal VM is an ordinary Caspar VM
//! everywhere else in the node.

mod client;
mod controller;
mod models;

use std::sync::Arc;

use caspar_vm_sdk::{registry, VmPluginMeta};

pub use controller::ModalVmPlugin;

/// The generated Modal gRPC client, from the vendored proto slice.
pub mod proto {
    #![allow(clippy::all)]
    tonic::include_proto!("modal.client");
}

/// Register this VM type with the Caspar VMM plugin registry.
/// Invoked by the build-time-generated plugin aggregation crate.
pub fn register() {
    let meta = VmPluginMeta::from_config_str(include_str!("../vm.config.json"))
        .expect("caspar-vm-modal: invalid vm.config.json");
    registry::register_plugin(Arc::new(ModalVmPlugin::new(meta)));
}
