//! Persistent local navigation broker.
//!
//! See the [design contract](../../../.codex/design/nas-lsp-broker.md).

#![cfg(target_os = "linux")]

pub mod broker;
pub mod config;
pub mod freshness;
pub mod index;
mod language;
mod linked_checkout;
pub mod lsp;
mod metrics;
pub mod pool;
mod prepared;
pub mod resource;
pub mod worker;
