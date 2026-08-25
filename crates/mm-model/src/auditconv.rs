//! Port of `model/auditconv.go` — reduced projections of model types for the audit log.
//!
//! Each `audit*` type keeps only the fields that are safe and useful to log: no tokens, no
//! secrets, no message bodies. The key names are **not** the models' own JSON keys — `Description`
//! is logged as `desc`, `DisplayName` as `display`, `Extension` as `ext` — so a consumer parsing
//! audit output cannot reuse a model's schema.
//!
//! # `auditCommandArgs` writes `team_id` and `trigger_id` swapped
//!
//! ```go
//! enc.StringKey("team_id", ca.TriggerID)
//! enc.StringKey("trigger_id", ca.TeamID)
//! ```
//!
//! Reproduced. Every slash-command audit entry the Go server has ever written carries the trigger
//! id under `team_id` and vice versa, and a consumer that has learned to read them that way would
//! break if this were "fixed" here.
//!
//! # `auditRemoteCluster.RemoteTeamId` is never populated
//!
//! `newRemoteCluster` copies seven of the eight fields and skips `RemoteTeamId`, so the key is
//! always written as `""`. That is consistent with `RemoteCluster.RemoteTeamId` being deprecated,
//! but the key is still emitted.
//!
//! # There is no `AuditModelTypeConv`
//!
//! Go's entry point is a type switch over `any` that maps 21 model types (and their pointer forms)
//! to these projections. Rust dispatches statically: each projection has a `from_*` constructor,
//! and the caller — which knows the type — calls the right one. A `dyn Any` switch would
//! reproduce the shape without reproducing anything useful.
//!
//! Auditing itself is [D-028] and is not otherwise ported; these are the wire shapes it produces.

use crate::bot::Bot;
use crate::channel::{Channel, ChannelModerationPatch};
use crate::command::Command;
use crate::command_args::CommandArgs;
use crate::emoji::Emoji;
use crate::file_info::FileInfo;
use crate::group::Group;
use crate::incoming_webhook::IncomingWebhook;
use crate::job::Job;
use crate::oauth::OAuthApp;
use crate::outgoing_webhook::OutgoingWebhook;
use crate::post::Post;
use crate::remote_cluster::RemoteCluster;
use crate::role::Role;
use crate::scheme::{Scheme, SchemeRoles};
use crate::session::Session;
use crate::team::Team;
use crate::user::{User, UserPatch};
use crate::utils::StringInterface;

fn s(value: &str) -> serde_json::Value {
    serde_json::Value::String(value.to_string())
}

/// Port of `auditChannel` (auditconv.go:103).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditChannel {
    pub id: String,
    pub name: String,
    pub type_: String,
}

impl AuditChannel {
    /// Port of `newAuditChannel` (auditconv.go:113). Go's nil argument yields the **zero**
    /// projection rather than nothing; `Option` makes that explicit here.
    pub fn from_channel(c: Option<&Channel>) -> Self {
        match c {
            None => Self::default(),
            Some(c) => Self {
                id: c.id.clone(),
                name: c.name.clone(),
                type_: c.channel_type.clone(),
            },
        }
    }

    /// Port of `(auditChannel).MarshalJSONObject` (auditconv.go:123).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("type".into(), s(&self.type_));
        m
    }
}

/// Port of `auditTeam` (auditconv.go:132).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditTeam {
    pub id: String,
    pub name: String,
    pub type_: String,
}

impl AuditTeam {
    /// Port of `newAuditTeam` (auditconv.go:140).
    pub fn from_team(t: Option<&Team>) -> Self {
        match t {
            None => Self::default(),
            Some(t) => Self {
                id: t.id.clone(),
                name: t.name.clone(),
                type_: t.team_type.clone(),
            },
        }
    }

    /// Port of `(auditTeam).MarshalJSONObject` (auditconv.go:150).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("type".into(), s(&self.type_));
        m
    }
}

/// Port of `auditUser` (auditconv.go:159).
///
/// **`Name` is the username**, not the display name — and the email is deliberately absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditUser {
    pub id: String,
    pub name: String,
    pub roles: String,
}

impl AuditUser {
    /// Port of `newAuditUser` (auditconv.go:167).
    pub fn from_user(u: Option<&User>) -> Self {
        match u {
            None => Self::default(),
            Some(u) => Self {
                id: u.id.clone(),
                name: u.username.clone(),
                roles: u.roles.clone(),
            },
        }
    }

    /// Port of `(auditUser).MarshalJSONObject` (auditconv.go:192).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("roles".into(), s(&self.roles));
        m
    }
}

/// Port of `auditUserPatch` (auditconv.go:178).
///
/// **One field, and no `MarshalJSONObject`** — Go never defines one, so this projection can be
/// built but not encoded by the audit path. Reproduced as a plain struct with no `to_map`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditUserPatch {
    pub name: String,
}

impl AuditUserPatch {
    /// Port of `newAuditUserPatch` (auditconv.go:182).
    pub fn from_user_patch(up: Option<&UserPatch>) -> Self {
        match up {
            None => Self::default(),
            Some(up) => Self {
                name: up.username.clone().unwrap_or_default(),
            },
        }
    }
}

/// Port of `auditCommand` (auditconv.go:200).
///
/// Thirteen fields, and **the token is not among them**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditCommand {
    pub id: String,
    pub creator_id: String,
    pub team_id: String,
    pub trigger: String,
    pub method: String,
    pub username: String,
    pub icon_url: String,
    pub auto_complete: bool,
    pub auto_complete_desc: String,
    pub auto_complete_hint: String,
    pub display_name: String,
    pub description: String,
    pub url: String,
}

impl AuditCommand {
    /// Port of `newAuditCommand` (auditconv.go:216).
    pub fn from_command(c: Option<&Command>) -> Self {
        match c {
            None => Self::default(),
            Some(c) => Self {
                id: c.id.clone(),
                creator_id: c.creator_id.clone(),
                team_id: c.team_id.clone(),
                trigger: c.trigger.clone(),
                method: c.method.clone(),
                username: c.username.clone(),
                icon_url: c.icon_url.clone(),
                auto_complete: c.auto_complete,
                auto_complete_desc: c.auto_complete_desc.clone(),
                auto_complete_hint: c.auto_complete_hint.clone(),
                display_name: c.display_name.clone(),
                description: c.description.clone(),
                url: c.url.clone(),
            },
        }
    }

    /// Port of `(auditCommand).MarshalJSONObject` (auditconv.go:235) — note `display`, `desc`
    /// and `url` rather than the model's own key names.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("creator_id".into(), s(&self.creator_id));
        m.insert("team_id".into(), s(&self.team_id));
        m.insert("trigger".into(), s(&self.trigger));
        m.insert("method".into(), s(&self.method));
        m.insert("username".into(), s(&self.username));
        m.insert("icon_url".into(), s(&self.icon_url));
        m.insert("auto_complete".into(), self.auto_complete.into());
        m.insert("auto_complete_desc".into(), s(&self.auto_complete_desc));
        m.insert("auto_complete_hint".into(), s(&self.auto_complete_hint));
        m.insert("display".into(), s(&self.display_name));
        m.insert("desc".into(), s(&self.description));
        m.insert("url".into(), s(&self.url));
        m
    }
}

/// Port of `auditCommandArgs` (auditconv.go:255).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditCommandArgs {
    pub channel_id: String,
    pub team_id: String,
    pub trigger_id: String,
    pub command: String,
}

impl AuditCommandArgs {
    /// Port of `newAuditCommandArgs` (auditconv.go:263).
    ///
    /// **Only the first whitespace-separated field of the command line is logged** — the trigger
    /// word — so the arguments a user typed never reach the audit log. `strings.Fields` splits on
    /// runs of any whitespace and drops empties, which `split_whitespace` matches exactly.
    pub fn from_command_args(ca: Option<&CommandArgs>) -> Self {
        match ca {
            None => Self::default(),
            Some(ca) => Self {
                channel_id: ca.channel_id.clone(),
                team_id: ca.team_id.clone(),
                trigger_id: ca.trigger_id.clone(),
                command: ca
                    .command
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string(),
            },
        }
    }

    /// Port of `(auditCommandArgs).MarshalJSONObject` (auditconv.go:277).
    ///
    /// **`team_id` and `trigger_id` are swapped** — see the module docs.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("channel_id".into(), s(&self.channel_id));
        m.insert("team_id".into(), s(&self.trigger_id));
        m.insert("trigger_id".into(), s(&self.team_id));
        m.insert("command".into(), s(&self.command));
        m
    }
}

/// Port of `auditBot` (auditconv.go:288).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditBot {
    pub user_id: String,
    pub username: String,
    pub displayname: String,
}

impl AuditBot {
    /// Port of `newAuditBot` (auditconv.go:295).
    pub fn from_bot(b: Option<&Bot>) -> Self {
        match b {
            None => Self::default(),
            Some(b) => Self {
                user_id: b.user_id.clone(),
                username: b.username.clone(),
                displayname: b.display_name.clone(),
            },
        }
    }

    /// Port of `(auditBot).MarshalJSONObject` (auditconv.go:305).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("user_id".into(), s(&self.user_id));
        m.insert("username".into(), s(&self.username));
        m.insert("display".into(), s(&self.displayname));
        m
    }
}

/// Port of `auditChannelModerationPatch` (auditconv.go:315).
///
/// Only two of the four moderated roles are logged — guests and members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditChannelModerationPatch {
    pub name: String,
    pub role_guests: bool,
    pub role_members: bool,
}

impl AuditChannelModerationPatch {
    /// Port of `newAuditChannelModerationPatch` (auditconv.go:322).
    ///
    /// **Go dereferences `p.Roles` without a nil check** — `p.Roles.Guests` panics on a patch
    /// whose `roles` is absent. Here a missing `roles` leaves both booleans false, which is the
    /// safe direction and the only difference.
    pub fn from_patch(p: Option<&ChannelModerationPatch>) -> Self {
        match p {
            None => Self::default(),
            Some(p) => Self {
                name: p.name.clone().unwrap_or_default(),
                role_guests: p.roles.as_ref().and_then(|r| r.guests).unwrap_or(false),
                role_members: p.roles.as_ref().and_then(|r| r.members).unwrap_or(false),
            },
        }
    }

    /// Port of `(auditChannelModerationPatch).MarshalJSONObject` (auditconv.go:338).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("name".into(), s(&self.name));
        m.insert("role_guests".into(), self.role_guests.into());
        m.insert("role_members".into(), self.role_members.into());
        m
    }
}

/// Port of `auditEmoji` (auditconv.go:348).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditEmoji {
    pub id: String,
    pub name: String,
}

impl AuditEmoji {
    /// Port of `newAuditEmoji` (auditconv.go:354).
    pub fn from_emoji(e: Option<&Emoji>) -> Self {
        match e {
            None => Self::default(),
            Some(e) => Self {
                id: e.id.clone(),
                name: e.name.clone(),
            },
        }
    }

    /// Port of `(auditEmoji).MarshalJSONObject` (auditconv.go:363).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m
    }
}

/// Port of `auditFileInfo` (auditconv.go:372).
///
/// **The storage path is logged**, unlike on the wire where `FileInfo.Path` carries `json:"-"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFileInfo {
    pub id: String,
    pub post_id: String,
    pub path: String,
    pub name: String,
    pub extension: String,
    pub size: i64,
}

impl AuditFileInfo {
    /// Port of `newAuditFileInfo` (auditconv.go:382).
    pub fn from_file_info(f: Option<&FileInfo>) -> Self {
        match f {
            None => Self::default(),
            Some(f) => Self {
                id: f.id.clone(),
                post_id: f.post_id.clone(),
                path: f.path.clone(),
                name: f.name.clone(),
                extension: f.extension.clone(),
                size: f.size,
            },
        }
    }

    /// Port of `(auditFileInfo).MarshalJSONObject` (auditconv.go:395) — `ext`, not `extension`.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("post_id".into(), s(&self.post_id));
        m.insert("path".into(), s(&self.path));
        m.insert("name".into(), s(&self.name));
        m.insert("ext".into(), s(&self.extension));
        m.insert("size".into(), self.size.into());
        m
    }
}

/// Port of `auditGroup` (auditconv.go:412).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditGroup {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
}

impl AuditGroup {
    /// Port of `newAuditGroup` (auditconv.go:420) — a nil `Group.Name` logs as `""`.
    pub fn from_group(g: Option<&Group>) -> Self {
        match g {
            None => Self::default(),
            Some(g) => Self {
                id: g.id.clone(),
                name: g.get_name().to_string(),
                display_name: g.display_name.clone(),
                description: g.description.clone(),
            },
        }
    }

    /// Port of `(auditGroup).MarshalJSONObject` (auditconv.go:435).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("display".into(), s(&self.display_name));
        m.insert("desc".into(), s(&self.description));
        m
    }
}

/// Port of `auditJob` (auditconv.go:445).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditJob {
    pub id: String,
    pub type_: String,
    pub priority: i64,
    pub start_at: i64,
}

impl AuditJob {
    /// Port of `newAuditJob` (auditconv.go:453).
    pub fn from_job(j: Option<&Job>) -> Self {
        match j {
            None => Self::default(),
            Some(j) => Self {
                id: j.id.clone(),
                type_: j.job_type.clone(),
                priority: j.priority,
                start_at: j.start_at,
            },
        }
    }

    /// Port of `(auditJob).MarshalJSONObject` (auditconv.go:464).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("type".into(), s(&self.type_));
        m.insert("priority".into(), self.priority.into());
        m.insert("start_at".into(), self.start_at.into());
        m
    }
}

/// Port of `auditOAuthApp` (auditconv.go:475).
///
/// **The client secret is not logged**, which is the point of the projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditOAuthApp {
    pub id: String,
    pub creator_id: String,
    pub name: String,
    pub description: String,
    pub is_trusted: bool,
}

impl AuditOAuthApp {
    /// Port of `newAuditOAuthApp` (auditconv.go:484).
    pub fn from_oauth_app(o: Option<&OAuthApp>) -> Self {
        match o {
            None => Self::default(),
            Some(o) => Self {
                id: o.id.clone(),
                creator_id: o.creator_id.clone(),
                name: o.name.clone(),
                description: o.description.clone(),
                is_trusted: o.is_trusted,
            },
        }
    }

    /// Port of `(auditOAuthApp).MarshalJSONObject` (auditconv.go:496) — `trusted`, not
    /// `is_trusted`.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("creator_id".into(), s(&self.creator_id));
        m.insert("name".into(), s(&self.name));
        m.insert("desc".into(), s(&self.description));
        m.insert("trusted".into(), self.is_trusted.into());
        m
    }
}

/// Port of `auditPost` (auditconv.go:508).
///
/// **The message is not logged.**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditPost {
    pub id: String,
    pub channel_id: String,
    pub type_: String,
    pub is_pinned: bool,
}

impl AuditPost {
    /// Port of `newAuditPost` (auditconv.go:516).
    pub fn from_post(p: Option<&Post>) -> Self {
        match p {
            None => Self::default(),
            Some(p) => Self {
                id: p.id.clone(),
                channel_id: p.channel_id.clone(),
                type_: p.post_type.clone(),
                is_pinned: p.is_pinned,
            },
        }
    }

    /// Port of `(auditPost).MarshalJSONObject` (auditconv.go:527) — `pinned`, not `is_pinned`.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("channel_id".into(), s(&self.channel_id));
        m.insert("type".into(), s(&self.type_));
        m.insert("pinned".into(), self.is_pinned.into());
        m
    }
}

/// Port of `auditRole` (auditconv.go:538).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditRole {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub permissions: Vec<String>,
    pub scheme_managed: bool,
    pub built_in: bool,
}

impl AuditRole {
    /// Port of `newAuditRole` (auditconv.go:548).
    ///
    /// The permissions are **copied**, so a later mutation of the role does not change what was
    /// logged. Note Go appends onto a nil slice, so an empty permission list stays nil and
    /// `SliceStringKey` writes `[]`.
    pub fn from_role(r: Option<&Role>) -> Self {
        match r {
            None => Self::default(),
            Some(r) => Self {
                id: r.id.clone(),
                name: r.name.clone(),
                display_name: r.display_name.clone(),
                permissions: r.permissions.clone().unwrap_or_default(),
                scheme_managed: r.scheme_managed,
                built_in: r.built_in,
            },
        }
    }

    /// Port of `(auditRole).MarshalJSONObject` (auditconv.go:561).
    ///
    /// **`schemeManaged` is camelCase** while `builtin` is one lower-case word — the only two
    /// keys in this file that are neither snake_case nor abbreviated.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("display".into(), s(&self.display_name));
        m.insert(
            "perms".into(),
            serde_json::Value::Array(self.permissions.iter().map(|p| s(p)).collect()),
        );
        m.insert("schemeManaged".into(), self.scheme_managed.into());
        m.insert("builtin".into(), self.built_in.into());
        m
    }
}

/// Port of `auditScheme` (auditconv.go:574).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditScheme {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub scope: String,
}

impl AuditScheme {
    /// Port of `newAuditScheme` (auditconv.go:582).
    pub fn from_scheme(sc: Option<&Scheme>) -> Self {
        match sc {
            None => Self::default(),
            Some(sc) => Self {
                id: sc.id.clone(),
                name: sc.name.clone(),
                display_name: sc.display_name.clone(),
                scope: sc.scope.clone(),
            },
        }
    }

    /// Port of `(auditScheme).MarshalJSONObject` (auditconv.go:593).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("name".into(), s(&self.name));
        m.insert("display".into(), s(&self.display_name));
        m.insert("scope".into(), s(&self.scope));
        m
    }
}

/// Port of `auditSchemeRoles` (auditconv.go:604).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditSchemeRoles {
    pub scheme_admin: bool,
    pub scheme_user: bool,
    pub scheme_guest: bool,
}

impl AuditSchemeRoles {
    /// Port of `newAuditSchemeRoles` (auditconv.go:611).
    pub fn from_scheme_roles(sr: Option<&SchemeRoles>) -> Self {
        match sr {
            None => Self::default(),
            Some(sr) => Self {
                scheme_admin: sr.scheme_admin,
                scheme_user: sr.scheme_user,
                scheme_guest: sr.scheme_guest,
            },
        }
    }

    /// Port of `(auditSchemeRoles).MarshalJSONObject` (auditconv.go:621) — the `scheme_` prefix
    /// is **dropped** from all three keys.
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("admin".into(), self.scheme_admin.into());
        m.insert("user".into(), self.scheme_user.into());
        m.insert("guest".into(), self.scheme_guest.into());
        m
    }
}

/// Port of `auditSession` (auditconv.go:631).
///
/// **The token is not logged**; the device id is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditSession {
    pub id: String,
    pub user_id: String,
    pub device_id: String,
}

impl AuditSession {
    /// Port of `newAuditSession` (auditconv.go:638).
    pub fn from_session(se: Option<&Session>) -> Self {
        match se {
            None => Self::default(),
            Some(se) => Self {
                id: se.id.clone(),
                user_id: se.user_id.clone(),
                device_id: se.device_id.clone(),
            },
        }
    }

    /// Port of `(auditSession).MarshalJSONObject` (auditconv.go:648).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("user_id".into(), s(&self.user_id));
        m.insert("device_id".into(), s(&self.device_id));
        m
    }
}

/// Port of `auditIncomingWebhook` (auditconv.go:658).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditIncomingWebhook {
    pub id: String,
    pub channel_id: String,
    pub team_id: String,
    pub display_name: String,
    pub description: String,
}

impl AuditIncomingWebhook {
    /// Port of `newAuditIncomingWebhook` (auditconv.go:667).
    pub fn from_incoming_webhook(h: Option<&IncomingWebhook>) -> Self {
        match h {
            None => Self::default(),
            Some(h) => Self {
                id: h.id.clone(),
                channel_id: h.channel_id.clone(),
                team_id: h.team_id.clone(),
                display_name: h.display_name.clone(),
                description: h.description.clone(),
            },
        }
    }

    /// Port of `(auditIncomingWebhook).MarshalJSONObject` (auditconv.go:679).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("channel_id".into(), s(&self.channel_id));
        m.insert("team_id".into(), s(&self.team_id));
        m.insert("display".into(), s(&self.display_name));
        m.insert("desc".into(), s(&self.description));
        m
    }
}

/// Port of `auditOutgoingWebhook` (auditconv.go:691).
///
/// **The token is not logged**, but the callback URLs are also absent — only the trigger words
/// are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditOutgoingWebhook {
    pub id: String,
    pub channel_id: String,
    pub team_id: String,
    pub trigger_words: Vec<String>,
    pub trigger_when: i64,
    pub display_name: String,
    pub description: String,
    pub content_type: String,
    pub username: String,
}

impl AuditOutgoingWebhook {
    /// Port of `newAuditOutgoingWebhook` (auditconv.go:704).
    pub fn from_outgoing_webhook(h: Option<&OutgoingWebhook>) -> Self {
        match h {
            None => Self::default(),
            Some(h) => Self {
                id: h.id.clone(),
                channel_id: h.channel_id.clone(),
                team_id: h.team_id.clone(),
                trigger_words: h.trigger_words.clone().unwrap_or_default(),
                trigger_when: h.trigger_when,
                display_name: h.display_name.clone(),
                description: h.description.clone(),
                content_type: h.content_type.clone(),
                username: h.username.clone(),
            },
        }
    }

    /// Port of `(auditOutgoingWebhook).MarshalJSONObject` (auditconv.go:720).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("id".into(), s(&self.id));
        m.insert("channel_id".into(), s(&self.channel_id));
        m.insert("team_id".into(), s(&self.team_id));
        m.insert(
            "trigger_words".into(),
            serde_json::Value::Array(self.trigger_words.iter().map(|w| s(w)).collect()),
        );
        m.insert("trigger_when".into(), self.trigger_when.into());
        m.insert("display".into(), s(&self.display_name));
        m.insert("desc".into(), s(&self.description));
        m.insert("content_type".into(), s(&self.content_type));
        m.insert("username".into(), s(&self.username));
        m
    }
}

/// Port of `auditRemoteCluster` (auditconv.go:736).
///
/// **Neither token is logged**, and `remote_team_id` is always empty — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditRemoteCluster {
    pub remote_id: String,
    /// Never populated by the constructor.
    pub remote_team_id: String,
    pub name: String,
    pub display_name: String,
    pub site_url: String,
    pub create_at: i64,
    pub last_ping_at: i64,
    pub creator_id: String,
}

impl AuditRemoteCluster {
    /// Port of `newRemoteCluster` (auditconv.go:748) — note the name: it is the only constructor
    /// in the file without the `newAudit` prefix.
    pub fn from_remote_cluster(r: Option<&RemoteCluster>) -> Self {
        match r {
            None => Self::default(),
            Some(r) => Self {
                remote_id: r.remote_id.clone(),
                // `RemoteTeamId` is deliberately not copied.
                remote_team_id: String::new(),
                name: r.name.clone(),
                display_name: r.display_name.clone(),
                site_url: r.site_url.clone(),
                create_at: r.create_at,
                last_ping_at: r.last_ping_at,
                creator_id: r.creator_id.clone(),
            },
        }
    }

    /// Port of `(auditRemoteCluster).MarshalJSONObject` (auditconv.go:762).
    pub fn to_map(&self) -> StringInterface {
        let mut m = StringInterface::new();
        m.insert("remote_id".into(), s(&self.remote_id));
        m.insert("remote_team_id".into(), s(&self.remote_team_id));
        m.insert("name".into(), s(&self.name));
        m.insert("display_name".into(), s(&self.display_name));
        m.insert("site_url".into(), s(&self.site_url));
        m.insert("create_at".into(), self.create_at.into());
        m.insert("last_ping_at".into(), self.last_ping_at.into());
        m.insert("creator_id".into(), s(&self.creator_id));
        m
    }
}
