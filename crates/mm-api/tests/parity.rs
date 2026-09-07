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
    pub mod authorized_oauth_apps;
    pub mod by_ids_lists;
    pub mod channel_autocomplete;
    pub mod channel_by_name;
    pub mod channel_by_name_for_team_name;
    pub mod channel_get;
    pub mod channel_member;
    pub mod channel_members_for_team_for_user;
    pub mod channel_members_for_user;
    pub mod channel_members_list;
    pub mod channel_pinned;
    pub mod channel_posts;
    pub mod channel_posts_unread;
    pub mod channel_search;
    pub mod channel_search_autocomplete;
    pub mod channel_stats;
    pub mod channel_timezones;
    pub mod channel_unread;
    pub mod channels_for_team_for_user;
    pub mod channels_for_user;
    pub mod channels_member_count;
    pub mod common_teams;
    pub mod config_source;
    pub mod data_retention;
    pub mod drafts;
    pub mod emoji_autocomplete;
    pub mod emoji_by_names;
    pub mod emoji_get;
    pub mod emoji_list;
    pub mod emoji_search;
    pub mod file_info;
    pub mod flagged_posts;
    pub mod groups;
    pub mod incoming_hooks;
    pub mod licence_gated_channels;
    pub mod license_client;
    pub mod licensed_features;
    pub mod me_alias;
    pub mod oauth_apps;
    pub mod outgoing_hooks;
    pub mod post_bulk_reactions;
    pub mod post_edit_history;
    pub mod post_get;
    pub mod post_reactions;
    pub mod post_thread;
    pub mod posts_by_ids;
    pub mod preference_reads;
    pub mod preferences;
    pub mod recommended_channels;
    pub mod roles;
    pub mod schemes;
    pub mod server_limits;
    pub mod session_activity;
    pub mod session_team_members;
    pub mod sessions_for_user;
    pub mod sidebar_categories;
    pub mod sidebar_router;
    pub mod single_hooks;
    pub mod status;
    pub mod system_usage;
    pub mod team_channel_lists;
    pub mod team_exists;
    pub mod team_get;
    pub mod team_members_route;
    pub mod team_name_members;
    pub mod team_stats;
    pub mod team_unread;
    pub mod teams_all;
    pub mod teams_for_user;
    pub mod teams_unread;
    pub mod terms_of_service;
    pub mod thread_for_user;
    pub mod threads_for_user;
    pub mod user_audits;
    pub mod user_by_email;
    pub mod user_by_username;
    pub mod user_get;
    pub mod user_terms_of_service;
    pub mod users_autocomplete;
    pub mod users_by_ids;
    pub mod users_by_names;
    pub mod users_group_channels;
    pub mod users_known;
    pub mod users_list;
    pub mod users_me;
    pub mod users_me_sessions;
    pub mod users_search;
    pub mod users_stats;
    pub mod users_stats_filtered;
}
