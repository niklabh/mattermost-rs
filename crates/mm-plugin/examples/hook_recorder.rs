//! A Mattermost plugin that writes down every hook it is handed, for
//! `mm-api`'s `parity::plugin_hooks`.
//!
//! One binary runs under **both** hosts — Go's `plugin.Environment` and `mm_app`'s — because a
//! plugin built on this SDK speaks exactly what a Go plugin speaks (docs/PLUGIN_PLAN.md, D1). So
//! the same client action can be sent to each server and the two transcripts diffed, and a
//! difference is the host's, never the plugin's.
//!
//! Each observation is one JSON line appended to `$HOOK_RECORDER_TRANSCRIPT`:
//!
//! ```text
//! {"hook": "MessageWillBePosted", "args": <render>}
//! ```
//!
//! `args` is `render_typed`, the canonical rendering of `reference/dump/plugingen`: the value is
//! gob-encoded and then rendered from the stream, so what is written down is exactly what gob
//! carried, omissions and all — not what a struct happens to hold.
//!
//! # The rejecting hooks are driven by the post's own message
//!
//! Both hosts must be able to provoke the same branch with the same request, so the behaviour is
//! a function of the input and nothing else:
//!
//! | message starts with | `MessageWillBePosted` answers |
//! |---|---|
//! | `!reject ` | no post, the rest of the message as the rejection reason |
//! | `!dismiss` | no post, `plugin.message_will_be_posted.dismiss_post` |
//! | `!rewrite ` | a post carrying **only** `Message`, which the merge fills back in |
//! | anything else | no post, no reason — "no opinion" |
//!
//! `MessageWillBeUpdated` reads the same prefixes on the *new* post: `!reject-edit <reason>` and
//! `!dismiss-edit` answer no post (which is what rejects an edit — the reason alone does not),
//! `!rewrite-edit <text>` answers the whole new post with its message replaced, and anything else
//! echoes the new post back unchanged, because answering nothing would reject it.
//!
//! # The membership hooks are driven by ids in the environment
//!
//! A `ChannelMember` carries nothing a client chooses, so the two rejecting membership hooks key
//! off the channel or team the member is for, named in the environment the host passes down to
//! this process:
//!
//! | variable | hook | answer |
//! |---|---|---|
//! | `HOOK_RECORDER_REJECT_CHANNEL` | `ChannelMemberWillBeAdded` | no member, [`MEMBER_REJECTION`] |
//! | `HOOK_RECORDER_ADMIN_CHANNEL` | `ChannelMemberWillBeAdded` | a member carrying **only** `SchemeAdmin`, which the merge fills back in |
//! | `HOOK_RECORDER_REJECT_TEAM` | `TeamMemberWillBeAdded` | no member, [`MEMBER_REJECTION`] |
//! | `HOOK_RECORDER_ADMIN_TEAM` | `TeamMemberWillBeAdded` | a member carrying **only** `SchemeAdmin` |
//!
//! Both hosts read the same ids, because the two servers share one database and therefore one
//! channel and one team.
//!
//! # `UserWillLogIn` is driven the same way
//!
//! | variable | hook | answer |
//! |---|---|---|
//! | `HOOK_RECORDER_REJECT_USER` | `UserWillLogIn` | [`LOGIN_REJECTION`], which refuses the login |
//!
//! Anyone else is let in with the empty string.
//!
//! # `FileWillBeDownloaded` is driven by the file's name
//!
//! A download whose `FileInfo.Name` starts with `hookreject` is refused with
//! [`DOWNLOAD_REJECTION`]; any other is allowed.
//!
//! # `FileWillBeUploaded` is driven by the file's name too
//!
//! The entry records the context, the file info and **what the plugin could read**, since the
//! reader is the part a host most easily gets wrong.
//!
//! | name starts with | answer |
//! |---|---|
//! | `hookrefuse` | [`UPLOAD_REJECTION`], with a file info carrying only `Name: renamed-on-reject.txt` — Go merges it before it looks at the reason |
//! | `hookreplace` | writes [`REPLACEMENT`], answers no file info |
//! | `hookrename` | a file info carrying only `Name: renamed.txt`, writes nothing |
//! | `hookunimage` | writes [`REPLACEMENT`] over an image, so its thumbnails cannot be made from it |
//! | anything else | nothing written, no file info |
//!
//! # The channel hooks are driven by the channel's header and name
//!
//! | the channel | hook | answer |
//! |---|---|---|
//! | new header starts with `!reject-update ` | `ChannelWillBeUpdated` | the rest, as the reason |
//! | new header is `!rewrite-header` | `ChannelWillBeUpdated` | the whole new channel, header replaced by [`REWRITTEN_HEADER`] |
//! | new header is `!partial-header` | `ChannelWillBeUpdated` | a channel carrying **only** the header — Go takes it whole |
//! | name starts with `hookkeepalive` | `ChannelWillBeArchived` | [`CHANNEL_REJECTION`] |
//! | name starts with `hookkeeparchived` | `ChannelWillBeRestored` | [`CHANNEL_REJECTION`] |
//!
//! `DraftWillBeUpserted` reads the draft's message: `!reject-draft <reason>` refuses,
//! `!rewrite-draft` answers the whole draft with the message replaced by [`REWRITTEN_HEADER`],
//! and `!partial-draft` answers a draft carrying **only** a message, which Go takes whole.
//!
//! `ScheduledPostWillBeCreated` does the same off the scheduled post's message, on create and on
//! update alike: `!reject-scheduled <reason>` refuses, `!rewrite-scheduled` answers the whole
//! post with the message replaced by [`REWRITTEN_HEADER`], and `!partial-scheduled` answers a
//! post carrying **only** a message — no id, no user, no channel — which Go takes whole.
//!
//! # `GenerateSupportData` is driven by the request's `User-Agent`
//!
//! The hook is handed nothing but the `plugin.Context`, so the one input a client controls is a
//! header the host copies into it. A `User-Agent` containing `hookfail` is answered with
//! [`SUPPORT_REJECTION`] **and** a file, which the host must drop along with the error's warning;
//! any other is answered with [`SUPPORT_FILES`], so the packet's merge of plugin files is what a
//! parity run sees.
//!
//! # The two consumed hooks are opt-in, and driven by each post's message
//!
//! `MessagesWillBeConsumed` and `MessagesWillBeConsumedWithContext` are implemented only when
//! `HOOK_RECORDER_CONSUME` is set, because Go fires them from inside **every** post read —
//! `GetSinglePost` included — and the older tours assert their transcripts entry for entry.
//! Each is handed a slice and answers one replacement per post that asks for one:
//!
//! | message starts with | hook | answer |
//! |---|---|---|
//! | `!consume ` | `MessagesWillBeConsumed` | a post carrying **only** `Id` and `Message` (the rest, with [`CONSUMED_PREFIX`] in front) — Go takes it whole |
//! | `!consume-ctx ` | `MessagesWillBeConsumedWithContext` | the same, with [`CONSUMED_CTX_PREFIX`] |
//! | `!consume-stranger` | `MessagesWillBeConsumed` | a post under an id the host never asked about, which it must ignore |
//! | anything else | both | nothing for that post |
//!
//! The context-aware hook is asked **after** the plain one, over the map as the plain one's
//! answers left it, so a `!consume ` post reaches it already rewritten.

use std::io::Write;
use std::sync::Mutex;

use mm_plugin::rpc::{Hooks, NotImplemented, Plugin, client_main};
use mm_plugin::wire::model::{Channel, Draft, ScheduledPost};
use mm_plugin::wire::model::{ChannelMember, Post, TeamMember};
use mm_plugin::wire::plugin::{
    Z_ChannelHasBeenCreatedArgs, Z_ChannelHasBeenCreatedReturns, Z_ChannelMemberWillBeAddedArgs,
    Z_ChannelMemberWillBeAddedReturns, Z_ChannelWillBeArchivedArgs, Z_ChannelWillBeArchivedReturns,
    Z_ChannelWillBeRestoredArgs, Z_ChannelWillBeRestoredReturns, Z_ChannelWillBeUpdatedArgs,
    Z_ChannelWillBeUpdatedReturns, Z_FileWillBeDownloadedArgs, Z_FileWillBeDownloadedReturns,
    Z_MessageHasBeenDeletedArgs, Z_MessageHasBeenDeletedReturns, Z_MessageHasBeenPostedArgs,
    Z_MessageHasBeenPostedReturns, Z_MessageHasBeenUpdatedArgs, Z_MessageHasBeenUpdatedReturns,
    Z_MessageWillBePostedArgs, Z_MessageWillBePostedReturns, Z_MessageWillBeUpdatedArgs,
    Z_MessageWillBeUpdatedReturns, Z_PreferencesHaveChangedArgs, Z_PreferencesHaveChangedReturns,
    Z_ReactionHasBeenAddedArgs, Z_ReactionHasBeenAddedReturns, Z_ReactionHasBeenRemovedArgs,
    Z_ReactionHasBeenRemovedReturns, Z_TeamMemberWillBeAddedArgs, Z_TeamMemberWillBeAddedReturns,
    Z_UserHasBeenCreatedArgs, Z_UserHasBeenCreatedReturns, Z_UserHasBeenDeactivatedArgs,
    Z_UserHasBeenDeactivatedReturns, Z_UserHasJoinedChannelArgs, Z_UserHasJoinedChannelReturns,
    Z_UserHasJoinedTeamArgs, Z_UserHasJoinedTeamReturns, Z_UserHasLeftChannelArgs,
    Z_UserHasLeftChannelReturns, Z_UserHasLeftTeamArgs, Z_UserHasLeftTeamReturns,
    Z_UserHasLoggedInArgs, Z_UserHasLoggedInReturns, Z_UserWillLogInArgs, Z_UserWillLogInReturns,
};
use mm_plugin::wire::plugin::{Z_DraftWillBeUpsertedArgs, Z_DraftWillBeUpsertedReturns};
use mm_plugin::wire::plugin::{Z_GenerateSupportDataArgs, Z_GenerateSupportDataReturns};
use mm_plugin::wire::plugin::{
    Z_MessagesWillBeConsumedArgs, Z_MessagesWillBeConsumedReturns,
    Z_MessagesWillBeConsumedWithContextArgs, Z_MessagesWillBeConsumedWithContextReturns,
};
use mm_plugin::wire::plugin::{Z_OnInstallArgs, Z_OnInstallReturns};
use mm_plugin::wire::plugin::{
    Z_ScheduledPostWillBeCreatedArgs, Z_ScheduledPostWillBeCreatedReturns,
};
use serde_json::{Value as Json, json};

/// `render.rs` reads a gob oracle for its fixture helpers; this plugin loads no fixture, so the
/// directory is never asked for. Panicking says so rather than reading somewhere arbitrary.
fn oracle_dir() -> std::path::PathBuf {
    panic!("hook_recorder loads no gob fixtures")
}

#[path = "../tests/common/render.rs"]
mod render;

use render::render_typed;

/// `plugin.DismissPostError` (public/plugin/hooks.go:82).
const DISMISS: &str = "plugin.message_will_be_posted.dismiss_post";

/// The reason the two membership hooks refuse with. Unlike the post hooks' reason it is a
/// **parameter** of a real translation key on both hosts, so it never reaches a client verbatim.
const MEMBER_REJECTION: &str = "the hook recorder says no";

/// The reason `UserWillLogIn` refuses with. Go concatenates it into the error **id**, as the post
/// hooks do, and not into a parameter as the membership hooks do.
const LOGIN_REJECTION: &str = "the hook recorder keeps this one out";

/// The reason `FileWillBeDownloaded` refuses with.
const DOWNLOAD_REJECTION: &str = "the hook recorder withholds this file";

/// The reason `FileWillBeUploaded` refuses with.
const UPLOAD_REJECTION: &str = "the hook recorder turns this upload away";

/// What `FileWillBeUploaded` writes over a file it replaces.
const REPLACEMENT: &[u8] = b"replaced by the hook recorder";

/// What `!rewrite-header` becomes.
const REWRITTEN_HEADER: &str = "rewritten by the hook recorder";

/// The reason the archive and restore hooks refuse with.
const CHANNEL_REJECTION: &str = "the hook recorder keeps this channel as it is";

/// The error `GenerateSupportData` answers a `hookfail` request with.
const SUPPORT_REJECTION: &str = "the hook recorder has nothing to report";

/// The files `GenerateSupportData` answers every other request with: a text file in the plugin's
/// own directory, and one whose bytes are not text.
const SUPPORT_FILES: [(&str, &[u8]); 2] = [
    (
        "mmrs.hookrecorder/recorded.txt",
        b"recorded by the hook recorder\n",
    ),
    ("mmrs.hookrecorder/raw.bin", &[0, 1, 2, 0xfe, 0xff]),
];

/// What `MessagesWillBeConsumed` puts in front of a `!consume ` message.
const CONSUMED_PREFIX: &str = "consumed: ";

/// What `MessagesWillBeConsumedWithContext` puts in front of a `!consume-ctx ` message.
const CONSUMED_CTX_PREFIX: &str = "consumed with context: ";

/// The id the stranger replacement is answered under: well-formed, and nobody's.
const STRANGER_ID: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// The two consumed hooks, added to [`IMPLEMENTED`] when `HOOK_RECORDER_CONSUME` is set.
const CONSUMED: [&str; 2] = [
    "MessagesWillBeConsumed",
    "MessagesWillBeConsumedWithContext",
];

/// The hooks this plugin implements, which is what `Plugin.Implemented` answers and therefore
/// what each host's `Implements` gate lets through.
const IMPLEMENTED: [&str; 28] = [
    "MessageWillBePosted",
    "MessageHasBeenPosted",
    "MessageWillBeUpdated",
    "MessageHasBeenUpdated",
    "MessageHasBeenDeleted",
    "ReactionHasBeenAdded",
    "ReactionHasBeenRemoved",
    "ChannelMemberWillBeAdded",
    "UserHasJoinedChannel",
    "UserHasLeftChannel",
    "TeamMemberWillBeAdded",
    "UserHasJoinedTeam",
    "UserHasLeftTeam",
    "UserWillLogIn",
    "UserHasLoggedIn",
    "UserHasBeenCreated",
    "UserHasBeenDeactivated",
    "FileWillBeDownloaded",
    "FileWillBeUploaded",
    "PreferencesHaveChanged",
    "ChannelHasBeenCreated",
    "ChannelWillBeUpdated",
    "ChannelWillBeArchived",
    "ChannelWillBeRestored",
    "DraftWillBeUpserted",
    "OnInstall",
    "ScheduledPostWillBeCreated",
    "GenerateSupportData",
];

/// The id in `name`, or the empty string when the host set no such variable. An unset variable
/// must never match a real id, which is why the empty string is compared against an id that is
/// always 26 characters.
fn configured_id(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

struct Recorder {
    transcript: Mutex<std::fs::File>,
}

impl Recorder {
    fn record(&self, entry: &Json) {
        let mut f = self
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = writeln!(f, "{entry}");
        let _ = f.flush();
    }

    fn saw<A: gobwire::Encode>(&self, name: &str, args: &A) {
        self.record(&json!({ "hook": name, "args": render_typed(args) }));
    }
}

/// The message with `prefix` taken off, when it starts with it.
fn after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message.strip_prefix(prefix)
}

/// The replacements a slice of posts asks for: `!<prefix> <rest>` becomes a post carrying only the
/// id and `<label><rest>`; a `!consume-stranger` message becomes a post under [`STRANGER_ID`].
fn consumed_replacements(posts: &[Post], prefix: &str, label: &str) -> Vec<Post> {
    posts
        .iter()
        .filter_map(|post| {
            if let Some(rest) = after(&post.message, prefix) {
                Some(Post {
                    id: post.id.clone(),
                    message: format!("{label}{rest}"),
                    ..Post::default()
                })
            } else if prefix == "!consume " && post.message == "!consume-stranger" {
                Some(Post {
                    id: STRANGER_ID.to_owned(),
                    message: "a stranger".to_owned(),
                    ..Post::default()
                })
            } else {
                None
            }
        })
        .collect()
}

impl Hooks for Recorder {
    fn implemented(&self) -> Vec<String> {
        let consume = std::env::var_os("HOOK_RECORDER_CONSUME").is_some();
        IMPLEMENTED
            .iter()
            .chain(CONSUMED.iter().filter(|_| consume))
            .map(|s| (*s).to_owned())
            .collect()
    }

    async fn messages_will_be_consumed(
        &self,
        args: Z_MessagesWillBeConsumedArgs,
    ) -> Result<Z_MessagesWillBeConsumedReturns, NotImplemented> {
        self.saw("MessagesWillBeConsumed", &args);
        Ok(Z_MessagesWillBeConsumedReturns {
            a: consumed_replacements(&args.a, "!consume ", CONSUMED_PREFIX),
        })
    }

    async fn messages_will_be_consumed_with_context(
        &self,
        args: Z_MessagesWillBeConsumedWithContextArgs,
    ) -> Result<Z_MessagesWillBeConsumedWithContextReturns, NotImplemented> {
        self.saw("MessagesWillBeConsumedWithContext", &args);
        Ok(Z_MessagesWillBeConsumedWithContextReturns {
            a: consumed_replacements(&args.b, "!consume-ctx ", CONSUMED_CTX_PREFIX),
        })
    }

    async fn message_will_be_posted(
        &self,
        args: Z_MessageWillBePostedArgs,
    ) -> Result<Z_MessageWillBePostedReturns, NotImplemented> {
        self.saw("MessageWillBePosted", &args);
        let message = args.b.as_deref().map_or("", |p| p.message.as_str());
        let answer = if let Some(reason) = after(message, "!reject ") {
            Z_MessageWillBePostedReturns {
                a: None,
                b: reason.to_owned(),
            }
        } else if message == "!dismiss" {
            Z_MessageWillBePostedReturns {
                a: None,
                b: DISMISS.to_owned(),
            }
        } else if let Some(text) = after(message, "!rewrite ") {
            // Only `Message`: everything else is omitted by gob, and the host's merge is what
            // puts the other 25 fields back. A replacement that carried the whole post would
            // pass whether or not the merge worked.
            Z_MessageWillBePostedReturns {
                a: Some(Box::new(Post {
                    message: text.to_owned(),
                    ..Post::default()
                })),
                b: String::new(),
            }
        } else {
            Z_MessageWillBePostedReturns::default()
        };
        Ok(answer)
    }

    async fn message_has_been_posted(
        &self,
        args: Z_MessageHasBeenPostedArgs,
    ) -> Result<Z_MessageHasBeenPostedReturns, NotImplemented> {
        self.saw("MessageHasBeenPosted", &args);
        Ok(Z_MessageHasBeenPostedReturns::default())
    }

    async fn message_will_be_updated(
        &self,
        args: Z_MessageWillBeUpdatedArgs,
    ) -> Result<Z_MessageWillBeUpdatedReturns, NotImplemented> {
        self.saw("MessageWillBeUpdated", &args);
        let new_post = args.b.as_deref();
        let message = new_post.map_or("", |p| p.message.as_str());
        let answer = if let Some(reason) = after(message, "!reject-edit ") {
            Z_MessageWillBeUpdatedReturns {
                a: None,
                b: reason.to_owned(),
            }
        } else if message == "!dismiss-edit" {
            Z_MessageWillBeUpdatedReturns {
                a: None,
                b: DISMISS.to_owned(),
            }
        } else if let Some(text) = after(message, "!rewrite-edit ") {
            // The whole post, because this hook **replaces** rather than merges.
            let mut post = new_post.cloned().unwrap_or_default();
            post.message = text.to_owned();
            Z_MessageWillBeUpdatedReturns {
                a: Some(Box::new(post)),
                b: String::new(),
            }
        } else {
            Z_MessageWillBeUpdatedReturns {
                a: new_post.cloned().map(Box::new),
                b: String::new(),
            }
        };
        Ok(answer)
    }

    async fn message_has_been_updated(
        &self,
        args: Z_MessageHasBeenUpdatedArgs,
    ) -> Result<Z_MessageHasBeenUpdatedReturns, NotImplemented> {
        self.saw("MessageHasBeenUpdated", &args);
        Ok(Z_MessageHasBeenUpdatedReturns::default())
    }

    async fn message_has_been_deleted(
        &self,
        args: Z_MessageHasBeenDeletedArgs,
    ) -> Result<Z_MessageHasBeenDeletedReturns, NotImplemented> {
        self.saw("MessageHasBeenDeleted", &args);
        Ok(Z_MessageHasBeenDeletedReturns::default())
    }

    async fn reaction_has_been_added(
        &self,
        args: Z_ReactionHasBeenAddedArgs,
    ) -> Result<Z_ReactionHasBeenAddedReturns, NotImplemented> {
        self.saw("ReactionHasBeenAdded", &args);
        Ok(Z_ReactionHasBeenAddedReturns::default())
    }

    async fn reaction_has_been_removed(
        &self,
        args: Z_ReactionHasBeenRemovedArgs,
    ) -> Result<Z_ReactionHasBeenRemovedReturns, NotImplemented> {
        self.saw("ReactionHasBeenRemoved", &args);
        Ok(Z_ReactionHasBeenRemovedReturns::default())
    }

    async fn channel_member_will_be_added(
        &self,
        args: Z_ChannelMemberWillBeAddedArgs,
    ) -> Result<Z_ChannelMemberWillBeAddedReturns, NotImplemented> {
        self.saw("ChannelMemberWillBeAdded", &args);
        let channel = args.b.as_deref().map_or("", |m| m.channel_id.as_str());
        let answer = if channel == configured_id("HOOK_RECORDER_REJECT_CHANNEL") {
            Z_ChannelMemberWillBeAddedReturns {
                a: None,
                b: MEMBER_REJECTION.to_owned(),
            }
        } else if channel == configured_id("HOOK_RECORDER_ADMIN_CHANNEL") {
            // Only `SchemeAdmin`: everything else is omitted by gob, and the host's merge is what
            // puts the channel, the user and the notify props back. A replacement carrying the
            // whole member would pass whether or not the merge worked.
            Z_ChannelMemberWillBeAddedReturns {
                a: Some(Box::new(ChannelMember {
                    scheme_admin: true,
                    ..ChannelMember::default()
                })),
                b: String::new(),
            }
        } else {
            Z_ChannelMemberWillBeAddedReturns::default()
        };
        Ok(answer)
    }

    async fn user_has_joined_channel(
        &self,
        args: Z_UserHasJoinedChannelArgs,
    ) -> Result<Z_UserHasJoinedChannelReturns, NotImplemented> {
        self.saw("UserHasJoinedChannel", &args);
        Ok(Z_UserHasJoinedChannelReturns::default())
    }

    async fn user_has_left_channel(
        &self,
        args: Z_UserHasLeftChannelArgs,
    ) -> Result<Z_UserHasLeftChannelReturns, NotImplemented> {
        self.saw("UserHasLeftChannel", &args);
        Ok(Z_UserHasLeftChannelReturns::default())
    }

    async fn team_member_will_be_added(
        &self,
        args: Z_TeamMemberWillBeAddedArgs,
    ) -> Result<Z_TeamMemberWillBeAddedReturns, NotImplemented> {
        self.saw("TeamMemberWillBeAdded", &args);
        let team = args.b.as_deref().map_or("", |m| m.team_id.as_str());
        let answer = if team == configured_id("HOOK_RECORDER_REJECT_TEAM") {
            Z_TeamMemberWillBeAddedReturns {
                a: None,
                b: MEMBER_REJECTION.to_owned(),
            }
        } else if team == configured_id("HOOK_RECORDER_ADMIN_TEAM") {
            Z_TeamMemberWillBeAddedReturns {
                a: Some(Box::new(TeamMember {
                    scheme_admin: true,
                    ..TeamMember::default()
                })),
                b: String::new(),
            }
        } else {
            Z_TeamMemberWillBeAddedReturns::default()
        };
        Ok(answer)
    }

    async fn user_has_joined_team(
        &self,
        args: Z_UserHasJoinedTeamArgs,
    ) -> Result<Z_UserHasJoinedTeamReturns, NotImplemented> {
        self.saw("UserHasJoinedTeam", &args);
        Ok(Z_UserHasJoinedTeamReturns::default())
    }

    async fn user_has_left_team(
        &self,
        args: Z_UserHasLeftTeamArgs,
    ) -> Result<Z_UserHasLeftTeamReturns, NotImplemented> {
        self.saw("UserHasLeftTeam", &args);
        Ok(Z_UserHasLeftTeamReturns::default())
    }

    async fn user_will_log_in(
        &self,
        args: Z_UserWillLogInArgs,
    ) -> Result<Z_UserWillLogInReturns, NotImplemented> {
        self.saw("UserWillLogIn", &args);
        let user = args.b.as_deref().map_or("", |u| u.id.as_str());
        let a = if user == configured_id("HOOK_RECORDER_REJECT_USER") {
            LOGIN_REJECTION.to_owned()
        } else {
            String::new()
        };
        Ok(Z_UserWillLogInReturns { a })
    }

    async fn user_has_logged_in(
        &self,
        args: Z_UserHasLoggedInArgs,
    ) -> Result<Z_UserHasLoggedInReturns, NotImplemented> {
        self.saw("UserHasLoggedIn", &args);
        Ok(Z_UserHasLoggedInReturns::default())
    }

    async fn user_has_been_created(
        &self,
        args: Z_UserHasBeenCreatedArgs,
    ) -> Result<Z_UserHasBeenCreatedReturns, NotImplemented> {
        self.saw("UserHasBeenCreated", &args);
        Ok(Z_UserHasBeenCreatedReturns::default())
    }

    async fn user_has_been_deactivated(
        &self,
        args: Z_UserHasBeenDeactivatedArgs,
    ) -> Result<Z_UserHasBeenDeactivatedReturns, NotImplemented> {
        self.saw("UserHasBeenDeactivated", &args);
        Ok(Z_UserHasBeenDeactivatedReturns::default())
    }

    async fn channel_has_been_created(
        &self,
        args: Z_ChannelHasBeenCreatedArgs,
    ) -> Result<Z_ChannelHasBeenCreatedReturns, NotImplemented> {
        self.saw("ChannelHasBeenCreated", &args);
        Ok(Z_ChannelHasBeenCreatedReturns::default())
    }

    async fn channel_will_be_updated(
        &self,
        args: Z_ChannelWillBeUpdatedArgs,
    ) -> Result<Z_ChannelWillBeUpdatedReturns, NotImplemented> {
        self.saw("ChannelWillBeUpdated", &args);
        let new_channel = args.b.as_deref();
        let header = new_channel.map_or("", |c| c.header.as_str());
        let answer = if let Some(reason) = after(header, "!reject-update ") {
            Z_ChannelWillBeUpdatedReturns {
                a: None,
                b: reason.to_owned(),
            }
        } else if header == "!rewrite-header" {
            let mut channel = new_channel.cloned().unwrap_or_default();
            channel.header = REWRITTEN_HEADER.to_owned();
            Z_ChannelWillBeUpdatedReturns {
                a: Some(Box::new(channel)),
                b: String::new(),
            }
        } else if header == "!partial-header" {
            Z_ChannelWillBeUpdatedReturns {
                a: Some(Box::new(Channel {
                    header: REWRITTEN_HEADER.to_owned(),
                    ..Channel::default()
                })),
                b: String::new(),
            }
        } else {
            Z_ChannelWillBeUpdatedReturns::default()
        };
        Ok(answer)
    }

    async fn draft_will_be_upserted(
        &self,
        args: Z_DraftWillBeUpsertedArgs,
    ) -> Result<Z_DraftWillBeUpsertedReturns, NotImplemented> {
        self.saw("DraftWillBeUpserted", &args);
        let draft = args.b.as_deref();
        let message = draft.map_or("", |d| d.message.as_str());
        let answer = if let Some(reason) = after(message, "!reject-draft ") {
            Z_DraftWillBeUpsertedReturns {
                a: None,
                b: reason.to_owned(),
            }
        } else if message == "!rewrite-draft" {
            let mut replaced = draft.cloned().unwrap_or_default();
            replaced.message = REWRITTEN_HEADER.to_owned();
            Z_DraftWillBeUpsertedReturns {
                a: Some(Box::new(replaced)),
                b: String::new(),
            }
        } else if message == "!partial-draft" {
            Z_DraftWillBeUpsertedReturns {
                a: Some(Box::new(Draft {
                    message: REWRITTEN_HEADER.to_owned(),
                    ..Draft::default()
                })),
                b: String::new(),
            }
        } else {
            Z_DraftWillBeUpsertedReturns::default()
        };
        Ok(answer)
    }

    async fn scheduled_post_will_be_created(
        &self,
        args: Z_ScheduledPostWillBeCreatedArgs,
    ) -> Result<Z_ScheduledPostWillBeCreatedReturns, NotImplemented> {
        self.saw("ScheduledPostWillBeCreated", &args);
        let post = args.b.as_deref();
        let message = post.map_or("", |p| p.draft.message.as_str());
        let answer = if let Some(reason) = after(message, "!reject-scheduled ") {
            Z_ScheduledPostWillBeCreatedReturns {
                a: None,
                b: reason.to_owned(),
            }
        } else if message == "!rewrite-scheduled" {
            let mut replaced = post.cloned().unwrap_or_default();
            replaced.draft.message = REWRITTEN_HEADER.to_owned();
            Z_ScheduledPostWillBeCreatedReturns {
                a: Some(Box::new(replaced)),
                b: String::new(),
            }
        } else if message == "!partial-scheduled" {
            let mut partial = ScheduledPost::default();
            partial.draft.message = REWRITTEN_HEADER.to_owned();
            Z_ScheduledPostWillBeCreatedReturns {
                a: Some(Box::new(partial)),
                b: String::new(),
            }
        } else {
            Z_ScheduledPostWillBeCreatedReturns::default()
        };
        Ok(answer)
    }

    async fn channel_will_be_archived(
        &self,
        args: Z_ChannelWillBeArchivedArgs,
    ) -> Result<Z_ChannelWillBeArchivedReturns, NotImplemented> {
        self.saw("ChannelWillBeArchived", &args);
        let name = args.b.as_deref().map_or("", |c| c.name.as_str());
        Ok(Z_ChannelWillBeArchivedReturns {
            a: if name.starts_with("hookkeepalive") {
                CHANNEL_REJECTION.to_owned()
            } else {
                String::new()
            },
        })
    }

    async fn channel_will_be_restored(
        &self,
        args: Z_ChannelWillBeRestoredArgs,
    ) -> Result<Z_ChannelWillBeRestoredReturns, NotImplemented> {
        self.saw("ChannelWillBeRestored", &args);
        let name = args.b.as_deref().map_or("", |c| c.name.as_str());
        Ok(Z_ChannelWillBeRestoredReturns {
            a: if name.starts_with("hookkeeparchived") {
                CHANNEL_REJECTION.to_owned()
            } else {
                String::new()
            },
        })
    }

    async fn on_install(
        &self,
        args: Z_OnInstallArgs,
    ) -> Result<Z_OnInstallReturns, NotImplemented> {
        self.saw("OnInstall", &args);
        Ok(Z_OnInstallReturns::default())
    }

    async fn generate_support_data(
        &self,
        args: Z_GenerateSupportDataArgs,
    ) -> Result<Z_GenerateSupportDataReturns, NotImplemented> {
        self.saw("GenerateSupportData", &args);
        let files = SUPPORT_FILES
            .iter()
            .map(|(name, body)| mm_plugin::wire::model::FileData {
                filename: (*name).to_owned(),
                body: body.to_vec(),
            })
            .collect();
        let fail = args
            .a
            .as_deref()
            .is_some_and(|ctx| ctx.user_agent.contains("hookfail"));
        let error =
            fail.then(|| mm_plugin::error::PluginError::Message(SUPPORT_REJECTION.to_owned()));
        Ok(Z_GenerateSupportDataReturns {
            a: files,
            b: mm_plugin::error::encodable_error(error.as_ref()),
        })
    }

    async fn preferences_have_changed(
        &self,
        args: Z_PreferencesHaveChangedArgs,
    ) -> Result<Z_PreferencesHaveChangedReturns, NotImplemented> {
        self.saw("PreferencesHaveChanged", &args);
        Ok(Z_PreferencesHaveChangedReturns::default())
    }

    async fn file_will_be_downloaded(
        &self,
        args: Z_FileWillBeDownloadedArgs,
    ) -> Result<Z_FileWillBeDownloadedReturns, NotImplemented> {
        self.saw("FileWillBeDownloaded", &args);
        let name = args.b.as_deref().map_or("", |f| f.name.as_str());
        let a = if name.starts_with("hookreject") {
            DOWNLOAD_REJECTION.to_owned()
        } else {
            String::new()
        };
        Ok(Z_FileWillBeDownloadedReturns { a })
    }
}

impl mm_plugin::rpc::HooksHttp for Recorder {}
impl mm_plugin::rpc::HooksFileUpload for Recorder {
    async fn file_will_be_uploaded(
        &self,
        context: Option<Box<mm_plugin::wire::plugin::Context>>,
        info: Option<Box<mm_plugin::wire::model::FileInfo>>,
        mut file: mm_plugin::io_rpc::RemoteReader,
        output: goplugin::yamux::Stream,
    ) -> Result<mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns, NotImplemented> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut read = Vec::new();
        let _ = file.read_to_end(&mut read).await;
        let name = info.as_deref().map_or("", |f| f.name.as_str()).to_owned();
        self.record(&json!({
            "hook": "FileWillBeUploaded",
            "args": {
                "A": render_typed(&context),
                "B": render_typed(&info),
                "read": String::from_utf8_lossy(&read),
            },
        }));

        let only_name = |name: &str| {
            Some(Box::new(mm_plugin::wire::model::FileInfo {
                name: name.to_owned(),
                ..Default::default()
            }))
        };
        let mut output = output;
        let answer = if name.starts_with("hookrefuse") {
            mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns {
                a: only_name("renamed-on-reject.txt"),
                b: UPLOAD_REJECTION.to_owned(),
            }
        } else if name.starts_with("hookreplace") || name.starts_with("hookunimage") {
            let _ = output.write_all(REPLACEMENT).await;
            mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns::default()
        } else if name.starts_with("hookrename") {
            mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns {
                a: only_name("renamed.txt"),
                b: String::new(),
            }
        } else {
            mm_plugin::wire::plugin::Z_FileWillBeUploadedReturns::default()
        };
        let _ = output.shutdown().await;
        Ok(answer)
    }
}
impl Plugin for Recorder {}

#[tokio::main]
async fn main() {
    let path =
        std::env::var_os("HOOK_RECORDER_TRANSCRIPT").expect("HOOK_RECORDER_TRANSCRIPT is not set");
    let transcript = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the transcript");
    let plugin = Recorder {
        transcript: Mutex::new(transcript),
    };
    if let Err(e) = client_main(plugin).await {
        eprintln!("hook recorder: {e}");
        std::process::exit(1);
    }
}
