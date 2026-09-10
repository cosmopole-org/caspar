//! Runtime models for the wasm VM plugin.
//!
//! The write-ahead transaction behind the raw `dbOp` host calls is shared with
//! every other in-process runtime and lives in the SDK
//! ([`caspar_vm_sdk::trx`]), so the wasm and javascript guests cannot end up
//! reading a differently-behaved database. This module re-exports it under the
//! names the wasm runtime has always used.

pub use caspar_vm_sdk::trx::{DbOp as WasmDbOp, Trx};
