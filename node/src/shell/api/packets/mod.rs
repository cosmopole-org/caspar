//! Shell-API packet schemas — one module per action namespace, each
//! containing both the request (`*Input`) and the response (`*Output`)
//! payload structs.

pub mod auth;
pub mod command;
pub mod creatures;
pub mod gateway;
pub mod invites;
pub mod plugin;
pub mod program;
pub mod simple;
pub mod stores;
