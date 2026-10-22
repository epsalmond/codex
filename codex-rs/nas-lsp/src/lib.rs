//! Persistent local navigation broker.
//!
//! See the [design contract](../../../.codex/design/nas-lsp-broker.md).

#![cfg(target_os = "linux")]

pub mod broker;
pub mod config;
pub mod freshness;
pub mod lsp;
pub mod pool;
pub mod resource;
pub mod worker;
