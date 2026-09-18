//! Mattermost's plugin RPC surface, for hosting Go plugins from Rust and writing plugins in Rust.
//!
//! [`wire`] is generated from the pinned Go tree by `reference/dump/plugingen` (the IDL) and
//! `scripts/plugingen.py` (the Rust): see docs/PLUGIN_PLAN.md, D5.

#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod wire;

pub mod http;
pub mod io_rpc;
pub mod rpc;
