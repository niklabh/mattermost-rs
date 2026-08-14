//! Wire types ported from `server/public/model/`.
//!
//! This crate has **no internal dependencies**. It holds the serde types whose JSON
//! representation must match the Go server byte-for-byte.

pub mod channel;
pub mod channel_list;
pub mod channel_member;
pub mod custom_status;
pub mod emoji;
/// The system-emoji table, emitted from Go by `reference/dump`. Private: `emoji` wraps it.
mod emoji_generated;
pub mod file_info;
pub mod post_acknowledgement;
pub mod post_embed;
pub mod preference;
pub mod reaction;
pub mod session;
pub mod status;
pub mod team;
pub mod team_member;
pub mod user;
pub mod utils;
pub mod version;
