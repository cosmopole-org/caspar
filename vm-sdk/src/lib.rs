//! # Caspar VM SDK
//!
//! The interface contract between the Caspar node's VMM (virtual machine
//! manager) and pluggable VM runtime implementations.
//!
//! A VM type is a standalone Rust project (living under the `vms/` folder next
//! to the Caspar node source tree) that:
//!
//! 1. depends on this crate,
//! 2. implements the [`VmPlugin`] trait for its controller,
//! 3. describes itself with a `vm.config.json` file (deserialized into
//!    [`VmPluginMeta`]), and
//! 4. exposes a `pub fn register()` entry point that calls
//!    [`registry::register_plugin`].
//!
//! At node build time the Caspar CLI (`casparctl vms sync`) scans the `vms/`
//! folder and generates a temporary aggregation crate that imports every
//! enabled VM project and calls its `register()` function, so the plugins are
//! statically compiled into the single Caspar node binary while the node
//! itself never names a VM type.
//!
//! At runtime the node publishes a [`host::VmHost`] implementation through
//! [`host::set_host`]; plugins reach every host capability (packet dispatch,
//! VM context registry, state DB, resource locks, logging, …) exclusively
//! through that interface.

pub mod host;
pub mod meta;
pub mod plugin;
pub mod registry;
pub mod trx;
pub mod util;

pub use host::{host, host_or_err, set_host, KvOp, VmHost};
pub use meta::VmPluginMeta;
pub use plugin::{forward_http_via_signal, VmPlugin};
pub use trx::{DbOp, Trx};
pub use util::{
    normalize_runtime, panic_message, parse_u64_array_field, parse_u8_array_field,
    parse_vm_resource_limits, VmResourceLimits,
};
