//! Wire types ported from `server/public/model/`.
//!
//! This crate has **no internal dependencies**. It holds the serde types whose JSON
//! representation must match the Go server byte-for-byte.

pub mod analytics_row;
pub mod audit;
pub mod audit_record;
pub mod authorize;
pub mod bot;
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
/// Go's `unicode.IsPrint`, `IsLetter` and `IsNumber` as range tables, emitted from the Go
/// toolchain. Private: `utils` wraps them, and they are Go's `unicode` package rather than a
/// Mattermost source file. Not to be confused with `unicode_generated`, which is the four CJK
/// script tables `model.ContainsCJK` needs.
mod go_unicode_generated;
pub mod go_url;
pub mod integration_action;
pub mod job;
pub mod limits;
pub mod link_metadata;
pub mod locale_generated;
pub mod mention_map;
pub mod message_attachment;
pub mod mm_blocks_actions;
pub mod oauth;
pub mod oauth_dcr;
pub mod permission;
/// The 311 permissions and the seven tables grouping them, emitted from Go by `reference/dump`.
/// Private: `permission` re-exports it, so a caller never has to know which half a name came from.
mod permission_generated;
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
pub mod product_notices;
pub mod reaction;
pub mod role;
/// The 24 default roles and the seven permission/id lists role.go builds in `init()`, emitted
/// from Go by `reference/dump`. Private: `role` re-exports it.
mod role_generated;
pub mod scheduled_post;
pub mod scheduled_post_recurrence;
pub mod scheme;
pub mod search_params;
pub mod search_requests;
pub mod session;
pub mod slack_compatibility;
pub mod stats;
pub mod status;
pub mod team;
pub mod team_member;
pub mod timeutils;
pub mod timezones;
/// The supported-zone table, emitted from Go by `reference/dump`. Private: `timezones` re-exports it.
mod timezones_generated;
pub mod unicode;
/// The four CJK script range tables, emitted from Go by `reference/dump`. Private: `unicode`
/// wraps them, and they are Go's `unicode` package rather than a Mattermost source file.
mod unicode_generated;
pub mod user;
pub mod user_autocomplete;
pub mod utils;
pub mod version;
pub mod view;
pub mod wrangler;
