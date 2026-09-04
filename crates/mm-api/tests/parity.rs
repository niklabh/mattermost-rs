//! Consolidated integration test suite for mm-api cross-server parity.
//!
//! This binary consolidates all the parity tests that were previously separate
//! integration test binaries. This dramatically improves build times by compiling
//! the common test infrastructure once instead of 35 times.
//!
//! # Running the suite
//!
//! Needs the development stack up and both servers running:
//!
//! ```sh
//! docker compose up -d
//! cargo run -p mm-api                       # :8066, forwards to :8065
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity
//! ```
//!
//! Filter to specific tests:
//!
//! ```sh
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity users_me
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity channel_get
//! ```
//!
//! Without `MM_PARITY_STACK=1` every test returns early. That is deliberate: `cargo test`
//! on a laptop with no Docker must stay green.

mod common;

mod parity {
    pub mod channel_by_name;
    pub mod channel_get;
    pub mod channel_member;
    pub mod channel_members_for_team_for_user;
    pub mod channel_members_list;
    pub mod channel_posts;
    pub mod channel_stats;
    pub mod channel_unread;
    pub mod channels_for_team_for_user;
    pub mod channels_for_user;
    pub mod emoji_get;
    pub mod post_get;
    pub mod post_reactions;
    pub mod post_thread;
    pub mod preference_reads;
    pub mod preferences;
    pub mod roles;
    pub mod session_team_members;
    pub mod sessions_for_user;
    pub mod sidebar_categories;
    pub mod sidebar_router;
    pub mod status;
    pub mod team_channel_lists;
    pub mod team_get;
    pub mod team_members_route;
    pub mod team_name_members;
    pub mod team_stats;
    pub mod team_unread;
    pub mod teams_all;
    pub mod teams_for_user;
    pub mod teams_unread;
    pub mod user_by_username;
    pub mod user_get;
    pub mod users_autocomplete;
    pub mod users_by_ids;
    pub mod users_list;
    pub mod users_me;
    pub mod users_me_sessions;
}
