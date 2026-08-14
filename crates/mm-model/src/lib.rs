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
pub mod go_url;
pub mod integration_action;
pub mod message_attachment;
pub mod mm_blocks_actions;
pub mod post;
pub mod post_acknowledgement;
pub mod post_embed;
/// The mm_blocks / Block Kit / Adaptive Card tree walkers behind two `Post` methods. Private:
/// every function in the Go file is unexported, and `post` is the surface.
mod post_interactive_blocks;
pub mod post_list;
pub mod post_metadata;
pub mod preference;
pub mod reaction;
pub mod session;
pub mod slack_compatibility;
pub mod status;
pub mod team;
pub mod team_member;
pub mod user;
pub mod utils;
pub mod version;
pub mod wrangler;
