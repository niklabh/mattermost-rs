//! Wire types ported from `server/public/model/`.
//!
//! This crate has **no internal dependencies**. It holds the serde types whose JSON
//! representation must match the Go server byte-for-byte.

pub mod session;
pub mod team;
pub mod team_member;
pub mod user;
pub mod utils;
