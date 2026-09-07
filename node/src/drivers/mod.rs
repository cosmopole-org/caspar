//! Driver adapters — concrete implementations of the port traits defined
//! in [`crate::models::ports`].
//!
//! - `file` — filesystem-backed [`crate::models::ports::file::IFile`]
//! - `ratelimit` — cross-protocol token-bucket request admission control
//! - `signaler` — in-process pub/sub event bus
//! - `storage` — RocksDB + PostgreSQL persistence
//! - `security` — RSA/ECDSA signing and verification
//! - `vmm` — in-process virtual-machine driver (wasm / docker /
//!   javascript / elpify / elpian / firecracker)
//! - `network` — chain consensus + TCP/WS client + federation transports
//! - `cluster` — OpenRaft-replicated geo-distributed instance mesh

pub mod cluster;
pub mod file;
pub mod gateway_subs;
pub mod network;
pub mod ratelimit;
pub mod rocks_tuning;
pub mod security;
pub mod signaler;
pub mod storage;
pub mod vmm;
