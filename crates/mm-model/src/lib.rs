//! Wire types ported from `server/public/model/`.
//!
//! This crate has **no internal dependencies**. It holds the serde types whose JSON
//! representation must match the Go server byte-for-byte.

pub mod analytics_row;
pub mod audit;
pub mod channel;
pub mod channel_data;
pub mod channel_list;
pub mod channel_member;
pub mod channel_member_history;
pub mod channel_mentions;
pub mod channel_search;
pub mod channel_view;
pub mod custom_status;
pub mod draft;
pub mod emoji;
/// The system-emoji table, emitted from Go by `reference/dump`. Private: `emoji` wraps it.
mod emoji_generated;
pub mod file;
pub mod file_info;
pub mod file_info_list;
pub mod file_info_search_results;
pub mod go_url;
pub mod integration_action;
pub mod limits;
pub mod mention_map;
pub mod message_attachment;
pub mod mm_blocks_actions;
pub mod post;
pub mod post_acknowledgement;
pub mod post_attributes;
pub mod post_embed;
pub mod post_info;
/// The mm_blocks / Block Kit / Adaptive Card tree walkers behind two `Post` methods. Private:
/// every function in the Go file is unexported, and `post` is the surface.
mod post_interactive_blocks;
pub mod post_list;
pub mod post_metadata;
pub mod post_search_results;
pub mod preference;
pub mod reaction;
pub mod scheduled_post;
pub mod scheduled_post_recurrence;
pub mod search_params;
pub mod search_requests;
pub mod session;
pub mod slack_compatibility;
pub mod stats;
pub mod status;
pub mod team;
pub mod team_member;
pub mod unicode;
/// The four CJK script range tables, emitted from Go by `reference/dump`. Private: `unicode`
/// wraps them, and they are Go's `unicode` package rather than a Mattermost source file.
mod unicode_generated;
pub mod user;
pub mod user_autocomplete;
pub mod utils;
pub mod version;
pub mod wrangler;
