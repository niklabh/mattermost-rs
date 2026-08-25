//! Port of `model/remote_cluster.go` — a trusted peer server for shared channels.
//!
//! # `Bitmask.IsBitSet` ignores its argument
//!
//! ```go
//! func (bm *Bitmask) IsBitSet(flag Bitmask) bool {
//!     return *bm != 0
//! }
//! ```
//!
//! It answers "is **any** bit set", not "is *this* bit set" — so `IsOptionFlagSet(AutoInvited)`
//! is true on a remote that only has `AutoShareDMs`. Every caller of `IsOptionFlagSet` inherits
//! that. It is reproduced exactly ([`Bitmask::is_bit_set`]) with the correct predicate offered
//! separately as [`Bitmask::is_flag_set`], because changing the behaviour would silently change
//! which remotes are auto-invited to channels.
//!
//! # Not ported: `Encrypt` / `Decrypt`
//!
//! `RemoteClusterInvite` seals itself with AES-256-GCM under a key derived by **PBKDF2 (600,000
//! iterations, SHA-256) for version ≥ 3 and scrypt (N=32768, r=8, p=1) below that**, salt
//! prepended, nonce prefixed to the ciphertext. `mm-model` has neither an AEAD nor a scrypt
//! dependency, and adding them for a type no migrated route touches is not justified yet — the
//! same call as the AES cookie work in `integration_action.go` ([D-046]). The parameters are
//! recorded here so the port is mechanical when it lands.

use serde::{Deserialize, Serialize};

use crate::go_url::parse_request_uri;
use crate::serde_helpers::{is_empty_str, is_zero_i64};
use crate::utils::{
    AppError, AppResult, get_millis, go_to_lower, is_valid_id, new_id, sanitize_unicode,
};

/// Port of `model.RemoteOfflineAfterMillis` (remote_cluster.go:25) — five minutes.
pub const REMOTE_OFFLINE_AFTER_MILLIS: i64 = 1000 * 60 * 5;
pub const REMOTE_NAME_MIN_LENGTH: usize = 1;
pub const REMOTE_NAME_MAX_LENGTH: usize = 64;

/// Port of `model.SiteURLPending` (remote_cluster.go:29) — the prefix on a site URL that has not
/// been confirmed yet.
pub const SITE_URL_PENDING: &str = "pending_";
/// Port of `model.SiteURLPlugin` (remote_cluster.go:34). **Deprecated**: new registrations store
/// the plugin's site URL directly and identify a plugin remote by `plugin_id`.
pub const SITE_URL_PLUGIN: &str = "plugin_";

/// Port of `model.Bitmask` (remote_cluster.go:46) — a `uint32` of option flags.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Bitmask(pub u32);

impl Bitmask {
    /// Port of `model.BitflagOptionAutoShareDMs` (remote_cluster.go:36) — any new DM or GM is
    /// shared with this remote automatically.
    pub const AUTO_SHARE_DMS: Bitmask = Bitmask(1 << 0);
    /// Port of `model.BitflagOptionAutoInvited` (remote_cluster.go:37) — the remote is invited to
    /// every shared channel automatically.
    pub const AUTO_INVITED: Bitmask = Bitmask(1 << 1);

    /// Port of `(*Bitmask).IsBitSet` (remote_cluster.go:48) — **verbatim, including the bug**.
    /// See the module docs.
    pub fn is_bit_set(&self, _flag: Bitmask) -> bool {
        self.0 != 0
    }

    /// What `IsBitSet` reads as though it does. **Not** a port of anything; offered so a caller
    /// that wants the correct test does not have to write the mask by hand.
    pub fn is_flag_set(&self, flag: Bitmask) -> bool {
        self.0 & flag.0 != 0
    }

    /// Port of `(*Bitmask).SetBit` (remote_cluster.go:52).
    pub fn set_bit(&mut self, flag: Bitmask) {
        self.0 |= flag.0;
    }

    /// Port of `(*Bitmask).UnsetBit` (remote_cluster.go:56).
    pub fn unset_bit(&mut self, flag: Bitmask) {
        self.0 &= !flag.0;
    }
}

/// Port of `validRemoteNameChars` (remote_cluster.go:40) — `^[a-zA-Z0-9\.\-\_]+$`.
fn is_valid_remote_name_chars(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Port of `model.IsValidRemoteName` (remote_cluster.go:198). Lengths are **bytes**.
pub fn is_valid_remote_name(s: &str) -> bool {
    if s.len() < REMOTE_NAME_MIN_LENGTH || s.len() > REMOTE_NAME_MAX_LENGTH {
        return false;
    }
    is_valid_remote_name_chars(s)
}

/// Port of `model.NormalizeRemoteName` (remote_cluster.go:305) — `strings.ToLower`, which is
/// [`go_to_lower`] rather than `str::to_lowercase`.
pub fn normalize_remote_name(name: &str) -> String {
    go_to_lower(name)
}

/// Port of `model.CleanRemoteName` (remote_cluster.go:211) — an arbitrary string to a valid
/// remote name.
///
/// The order matters and is Go's: lower-case, spaces to hyphens, trim, **then** replace every
/// remaining disallowed character with a hyphen, trim hyphens, truncate to 64 **bytes** and trim
/// hyphens again. If the result is still invalid — an empty input, say — a fresh id is
/// substituted, so this never returns something `IsValidRemoteName` rejects.
///
/// **Divergence:** Go truncates with `s[:RemoteNameMaxLength]`, a byte slice that can split a
/// multi-byte character and produce invalid UTF-8. It cannot happen here, because every
/// non-ASCII character has already been replaced by a hyphen at that point — but the port
/// truncates on a character boundary regardless, which is the same answer for every reachable
/// input and cannot panic.
pub fn clean_remote_name(s: &str) -> String {
    let mut s = go_to_lower(&s.replace(' ', "-"));
    s = s.trim().to_string();

    s = s
        .chars()
        .map(|c| {
            if is_valid_remote_name_chars(&c.to_string()) {
                c
            } else {
                '-'
            }
        })
        .collect();

    s = s.trim_matches('-').to_string();

    if s.len() > REMOTE_NAME_MAX_LENGTH {
        let cut = (0..=REMOTE_NAME_MAX_LENGTH)
            .rev()
            .find(|i| s.is_char_boundary(*i))
            .unwrap_or(0);
        s = s[..cut].trim_matches('-').to_string();
    }

    if !is_valid_remote_name(&s) {
        s = new_id();
    }

    s
}

/// Port of `model.RemoteCluster` (remote_cluster.go:61).
///
/// **`Token` and `RemoteToken` are on the wire** with no `omitempty` and no `json:"-"`; it is
/// [`RemoteCluster::sanitize`] that removes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteCluster {
    #[serde(rename = "remote_id")]
    pub remote_id: String,

    /// Deprecated and unused; kept for backwards compatibility.
    #[serde(rename = "remote_team_id")]
    pub remote_team_id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "site_url")]
    pub site_url: String,

    #[serde(rename = "default_team_id")]
    pub default_team_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "last_ping_at")]
    pub last_ping_at: i64,

    #[serde(rename = "last_global_user_sync_at")]
    pub last_global_user_sync_at: i64,

    /// Our token for calling them. Cleared by `sanitize`.
    #[serde(rename = "token")]
    pub token: String,

    /// Their token for calling us. Cleared by `sanitize`.
    #[serde(rename = "remote_token")]
    pub remote_token: String,

    /// A **space-delimited** list, normalised by `fix_topics` to have exactly one space between
    /// entries and a leading and trailing one. `*` means all topics.
    #[serde(rename = "topics")]
    pub topics: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    /// Non-empty when sync messages travel over the plugin API rather than HTTP.
    #[serde(rename = "plugin_id")]
    pub plugin_id: String,

    #[serde(rename = "options")]
    pub options: Bitmask,
}

impl RemoteCluster {
    /// Port of `(*RemoteCluster).PreSave` (remote_cluster.go:95).
    ///
    /// Note the order: `display_name` defaults to the **raw** name before either is sanitised, so
    /// a name needing sanitisation produces a display name that is sanitised but **not**
    /// lower-cased — only `name` gets `NormalizeRemoteName`.
    pub fn pre_save(&mut self) {
        if self.remote_id.is_empty() {
            self.remote_id = new_id();
        }

        if self.display_name.is_empty() {
            self.display_name = self.name.clone();
        }

        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
        self.name = normalize_remote_name(&self.name);

        if self.token.is_empty() {
            self.token = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.fix_topics();
    }

    /// Port of `(*RemoteCluster).PreUpdate` (remote_cluster.go:234) — as `pre_save` without the
    /// id, token and timestamp.
    pub fn pre_update(&mut self) {
        if self.display_name.is_empty() {
            self.display_name = self.name.clone();
        }

        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
        self.name = normalize_remote_name(&self.name);
        self.fix_topics();
    }

    /// Port of `(*RemoteCluster).IsValid` (remote_cluster.go:118).
    ///
    /// **Three different fields share the error id `model.cluster.is_valid.id.app_error`** —
    /// `remote_id`, `creator_id` and `default_team_id` — and only the details string tells them
    /// apart. The ids are also `model.cluster.…`, the same namespace `cluster_discovery.go` uses.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.remote_id) {
            return Err(err("id", format!("id={}", self.remote_id)));
        }

        if !is_valid_remote_name(&self.name) {
            return Err(err("name", format!("name={}", self.name)));
        }

        if self.create_at == 0 {
            return Err(err("create_at", "create_at=0".to_string()));
        }

        if !is_valid_id(&self.creator_id) {
            return Err(err("id", format!("creator_id={}", self.creator_id)));
        }

        if self.site_url.is_empty() {
            return Err(err("site_url", "site_url is empty".to_string()));
        }

        if !self.default_team_id.is_empty() && !is_valid_id(&self.default_team_id) {
            return Err(err(
                "id",
                format!("default_team_id={}", self.default_team_id),
            ));
        }

        Ok(())
    }

    /// Port of `(*RemoteCluster).Sanitize` (remote_cluster.go:145).
    pub fn sanitize(&mut self) {
        self.token.clear();
        self.remote_token.clear();
    }

    /// Port of `(*RemoteCluster).Patch` (remote_cluster.go:162).
    pub fn patch(&mut self, patch: &RemoteClusterPatch) {
        if let Some(display_name) = &patch.display_name {
            self.display_name = display_name.clone();
        }
        if let Some(default_team_id) = &patch.default_team_id {
            self.default_team_id = default_team_id.clone();
        }
    }

    /// Port of `(*RemoteCluster).IsOptionFlagSet` (remote_cluster.go:186) — inherits
    /// `IsBitSet`'s behaviour; see the module docs.
    pub fn is_option_flag_set(&self, flag: Bitmask) -> bool {
        self.options.is_bit_set(flag)
    }

    /// Port of `(*RemoteCluster).SetOptionFlag` (remote_cluster.go:190).
    pub fn set_option_flag(&mut self, flag: Bitmask) {
        self.options.set_bit(flag);
    }

    /// Port of `(*RemoteCluster).UnsetOptionFlag` (remote_cluster.go:194).
    pub fn unset_option_flag(&mut self, flag: Bitmask) {
        self.options.unset_bit(flag);
    }

    /// Port of `(*RemoteCluster).IsOnline` (remote_cluster.go:245).
    pub fn is_online(&self) -> bool {
        self.last_ping_at > get_millis() - REMOTE_OFFLINE_AFTER_MILLIS
    }

    /// Port of `(*RemoteCluster).IsConfirmed` (remote_cluster.go:249).
    ///
    /// A plugin remote is confirmed by definition. Otherwise the site URL must be non-empty and
    /// not `pending_`-prefixed — note Go's inline comment says "empty or pending siteurl are not
    /// confirmed", which is what the inverted condition achieves.
    pub fn is_confirmed(&self) -> bool {
        if self.is_plugin() {
            return true;
        }

        !self.site_url.is_empty() && !self.site_url.starts_with(SITE_URL_PENDING)
    }

    /// Port of `(*RemoteCluster).IsPlugin` (remote_cluster.go:260).
    pub fn is_plugin(&self) -> bool {
        !self.plugin_id.is_empty()
    }

    /// Port of `(*RemoteCluster).GetSiteURL` (remote_cluster.go:264) — the display form.
    ///
    /// **Dead branch reproduced:** the first `if` rewrites a `pending_` URL to `"..."`, and the
    /// second then tests the *rewritten* value for the same prefix — which can no longer match.
    /// So a pending URL displays as `...` and only a `plugin_`-prefixed one displays as `plugin`.
    pub fn get_site_url(&self) -> String {
        let mut site_url = self.site_url.clone();
        if site_url.starts_with(SITE_URL_PENDING) {
            site_url = "...".to_string();
        }
        if site_url.starts_with(SITE_URL_PENDING) || site_url.starts_with(SITE_URL_PLUGIN) {
            site_url = "plugin".to_string();
        }
        site_url
    }

    /// Port of `(*RemoteCluster).fixTopics` (remote_cluster.go:276).
    ///
    /// Normalises to `" a b c "` — **a leading and a trailing space** — so a `LIKE '% topic %'`
    /// query matches the first and last entries too. `""` and `"*"` are left as they are.
    pub fn fix_topics(&mut self) {
        let trimmed = self.topics.trim();
        if trimmed.is_empty() || trimmed == "*" {
            self.topics = trimmed.to_string();
            return;
        }

        let mut sb = String::from(" ");
        for c in self.topics.split(' ') {
            let cc = c.trim();
            if !cc.is_empty() {
                sb.push_str(cc);
                sb.push(' ');
            }
        }
        self.topics = sb;
    }

    /// Port of `(*RemoteCluster).ToRemoteClusterInfo` (remote_cluster.go:291) — the seven fields
    /// safe to send to a client.
    pub fn to_remote_cluster_info(&self) -> RemoteClusterInfo {
        RemoteClusterInfo {
            remote_id: self.remote_id.clone(),
            name: self.name.clone(),
            display_name: self.display_name.clone(),
            create_at: self.create_at,
            delete_at: self.delete_at,
            last_ping_at: self.last_ping_at,
            site_url: self.site_url.clone(),
        }
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "RemoteCluster.IsValid",
        format!("model.cluster.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.RemoteClusterPatch` (remote_cluster.go:150) — two fields only; the tokens, the
/// name and the site URL are not patchable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterPatch {
    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "default_team_id")]
    pub default_team_id: Option<String>,
}

/// Port of `model.RemoteClusterWithPassword` (remote_cluster.go:172) — the embedded pointer is
/// **inlined**, so `password` sits beside `remote_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterWithPassword {
    #[serde(flatten)]
    pub remote_cluster: RemoteCluster,

    #[serde(rename = "password")]
    pub password: String,
}

/// Port of `model.RemoteClusterWithInvite` (remote_cluster.go:177).
///
/// **Not** inlined — the remote is nested under `remote_cluster`, unlike
/// [`RemoteClusterWithPassword`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterWithInvite {
    #[serde(rename = "remote_cluster")]
    pub remote_cluster: Option<Box<RemoteCluster>>,

    /// The base64 of the encrypted [`RemoteClusterInvite`].
    #[serde(rename = "invite")]
    pub invite: String,

    #[serde(rename = "password", skip_serializing_if = "is_empty_str")]
    pub password: String,
}

/// Port of `model.RemoteClusterInfo` (remote_cluster.go:309) — the client-safe subset. No tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterInfo {
    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "last_ping_at")]
    pub last_ping_at: i64,

    /// The only `omitempty` field here.
    #[serde(rename = "site_url", skip_serializing_if = "is_empty_str")]
    pub site_url: String,
}

/// Port of `model.RemoteClusterMsg` (remote_cluster.go:350) — one routed message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterMsg {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "topic")]
    pub topic: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    /// A `json.RawMessage`; the shape depends on `topic`.
    ///
    /// `Option` rather than a bare `Value` because `IsValid` measures `len(m.Payload)` on the
    /// **raw bytes**: an absent key is nil and fails, while an explicit `null` is four bytes and
    /// passes. Only `Option` can tell those apart.
    #[serde(rename = "payload")]
    pub payload: Option<serde_json::Value>,
}

impl RemoteClusterMsg {
    /// Port of `model.NewRemoteClusterMsg` (remote_cluster.go:357).
    pub fn new(topic: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            id: new_id(),
            topic: topic.into(),
            create_at: get_millis(),
            payload: Some(payload),
        }
    }

    /// Port of `(RemoteClusterMsg).IsValid` (remote_cluster.go:366).
    ///
    /// The error ids are **`api.` prefixed**, not `model.`, and the empty-payload branch reuses
    /// the generic `api.context.invalid_body_param.app_error` with `Name: "PayLoad"` — note the
    /// capital L, which reaches the translated message.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(msg_err(
                "RemoteClusterMsg.IsValid",
                "api.remote_cluster.invalid_id.app_error",
                format!("Id={}", self.id),
                None,
            ));
        }

        if self.topic.is_empty() {
            return Err(msg_err(
                "RemoteClusterMsg.IsValid",
                "api.remote_cluster.invalid_topic.app_error",
                "Topic empty".to_string(),
                None,
            ));
        }

        // Go tests `len(m.Payload) == 0` on the raw bytes, so an explicit `null` — four bytes —
        // passes and only an absent payload fails.
        if self.payload.is_none() {
            return Err(msg_err(
                "RemoteClusterMsg.IsValid",
                "api.context.invalid_body_param.app_error",
                String::new(),
                Some(("Name", "PayLoad")),
            ));
        }

        Ok(())
    }
}

fn msg_err(
    where_: &'static str,
    id: &'static str,
    details: String,
    param: Option<(&str, &str)>,
) -> Box<AppError> {
    let params = param.map(|(key, value)| {
        let mut params = std::collections::HashMap::new();
        params.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        params
    });
    Box::new(AppError::new(where_, id, params, details, 400))
}

/// Port of `model.RemoteClusterFrame` (remote_cluster.go:322) — a message plus the remote it is
/// addressed to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterFrame {
    #[serde(rename = "remote_id")]
    pub remote_id: String,

    #[serde(rename = "msg")]
    pub msg: RemoteClusterMsg,
}

impl RemoteClusterFrame {
    /// Port of `(*RemoteClusterFrame).IsValid` (remote_cluster.go:334) — its own check, then the
    /// message's, whose error is returned **unwrapped**.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.remote_id) {
            return Err(msg_err(
                "RemoteClusterFrame.IsValid",
                "api.remote_cluster.invalid_id.app_error",
                format!("RemoteId={}", self.remote_id),
                None,
            ));
        }

        self.msg.is_valid()
    }
}

/// Port of `model.RemoteClusterPing` (remote_cluster.go:383) — the payload of a keep-alive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterPing {
    /// Epoch milliseconds, set by the sender.
    #[serde(rename = "sent_at")]
    pub sent_at: i64,

    /// Epoch milliseconds, set by the receiver — the pair gives a round-trip time.
    #[serde(rename = "recv_at")]
    pub recv_at: i64,
}

/// Port of `model.RemoteClusterInvite` (remote_cluster.go:389) — the sealed hand-shake payload.
/// See the module docs on why `Encrypt`/`Decrypt` are not ported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterInvite {
    #[serde(rename = "remote_id")]
    pub remote_id: String,

    /// Deprecated and unused.
    #[serde(rename = "remote_team_id")]
    pub remote_team_id: String,

    #[serde(rename = "site_url")]
    pub site_url: String,

    #[serde(rename = "token")]
    pub token: String,

    /// Minted by the remote when it accepts.
    #[serde(rename = "refreshed_token", skip_serializing_if = "is_empty_str")]
    pub refreshed_token: String,

    /// **Selects the key-derivation function**: ≥ 3 means PBKDF2, below means scrypt.
    #[serde(rename = "version", skip_serializing_if = "is_zero_i64")]
    pub version: i64,
}

impl RemoteClusterInvite {
    /// Port of `(*RemoteClusterInvite).IsValid` (remote_cluster.go:398).
    ///
    /// The site URL goes through `url.ParseRequestURI`, which **rejects a relative reference** —
    /// so an invite must carry an absolute URL even though `RemoteCluster::is_valid` only
    /// requires a non-empty one.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.remote_id) {
            return Err(invite_err("remote_id", format!("id={}", self.remote_id)));
        }

        if self.token.is_empty() {
            return Err(invite_err("token", "Token empty".to_string()));
        }

        if parse_request_uri(&self.site_url).is_err() {
            return Err(invite_err("site_url", String::new()));
        }

        Ok(())
    }
}

fn invite_err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "RemoteClusterInvite.IsValid",
        format!("model.remote_cluster_invite.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.RemoteClusterAcceptInvite` (remote_cluster.go:521).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteClusterAcceptInvite {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "default_team_id")]
    pub default_team_id: String,

    /// The base64 of the encrypted invite.
    #[serde(rename = "invite")]
    pub invite: String,

    #[serde(rename = "password")]
    pub password: String,
}

/// Port of `model.RemoteClusterQueryFilter` (remote_cluster.go:530). No `json:` tags.
///
/// `OnlyPlugins` and `ExcludePlugins` are independent booleans, so setting both is expressible
/// and returns nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteClusterQueryFilter {
    pub exclude_offline: bool,
    pub in_channel: String,
    pub not_in_channel: String,
    pub topic: String,
    pub creator_id: String,
    pub only_confirmed: bool,
    pub plugin_id: String,
    pub only_plugins: bool,
    pub exclude_plugins: bool,
    /// Matched with `Bitmask::IsBitSet`, so — per the module docs — it behaves as "has any
    /// option set" rather than "has these options".
    pub require_options: Bitmask,
    pub include_deleted: bool,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn remote_cluster_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteCluster, "remote_cluster");
    }
    #[test]
    fn remote_cluster_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterPatch, "remote_cluster_patch");
    }
    #[test]
    fn remote_cluster_with_password_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterWithPassword, "remote_cluster_with_password");
    }
    #[test]
    fn remote_cluster_with_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterWithInvite, "remote_cluster_with_invite");
    }
    #[test]
    fn remote_cluster_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterInfo, "remote_cluster_info");
    }
    #[test]
    fn remote_cluster_frame_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterFrame, "remote_cluster_frame");
    }
    #[test]
    fn remote_cluster_msg_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterMsg, "remote_cluster_msg");
    }
    #[test]
    fn remote_cluster_ping_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterPing, "remote_cluster_ping");
    }
    #[test]
    fn remote_cluster_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterInvite, "remote_cluster_invite");
    }
    #[test]
    fn remote_cluster_accept_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(RemoteClusterAcceptInvite, "remote_cluster_accept_invite");
    }
}

#[cfg(test)]
mod sweep_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The three name functions are pure string manipulation over a corpus of the shapes an
    /// operator actually types — spaces, case, unicode, and both sides of the 64-rune cap.
    ///
    /// `CleanRemoteName` has one non-deterministic branch: when cleaning cannot produce a valid
    /// name it returns a **fresh id**. The corpus flags those rows instead of recording the id,
    /// and this asserts the property (a valid, correctly-sized name) rather than the value.
    #[test]
    fn name_helpers_match_go() {
        let oracle = oracle();
        let cases = oracle["remote_cluster_names"].as_array().unwrap();
        assert!(cases.len() >= 15);

        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                is_valid_remote_name(input),
                case["is_valid"].as_bool().unwrap(),
                "IsValidRemoteName({input:?})"
            );
            assert_eq!(
                normalize_remote_name(input),
                case["normalized"].as_str().unwrap(),
                "NormalizeRemoteName({input:?})"
            );

            let cleaned = clean_remote_name(input);
            assert_eq!(
                is_valid_remote_name(&cleaned),
                case["cleaned_valid"].as_bool().unwrap(),
                "CleanRemoteName({input:?}) must yield a valid name"
            );
            if let Some(expected) = case.get("cleaned").and_then(|v| v.as_str()) {
                assert_eq!(cleaned, expected, "CleanRemoteName({input:?})");
            } else {
                // The NewId fallback: only the shape is reproducible.
                assert_eq!(
                    cleaned.len(),
                    26,
                    "CleanRemoteName({input:?}) should be an id"
                );
            }
        }
    }

    /// `fixTopics` normalises whitespace and the `*` wildcard, and it runs from `PreUpdate` — so
    /// the corpus drives it the same way rather than calling the unexported function.
    #[test]
    fn fix_topics_matches_go() {
        let oracle = oracle();
        let cases = oracle["remote_cluster_topics"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let input = case["in"].as_str().unwrap();
            let mut rc = RemoteCluster {
                remote_id: "r1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
                name: "remote".to_string(),
                topics: input.to_string(),
                create_at: 1_700_000_000_000,
                ..Default::default()
            };
            rc.pre_update();
            assert_eq!(
                rc.topics,
                case["out"].as_str().unwrap(),
                "fixTopics({input:?})"
            );
        }
    }

    /// `GetSiteURL` hides the two internal prefixes; `IsConfirmed` and `IsPlugin` read different
    /// things, which the `plugin_com.example` row exists to prove: a `plugin_` **site url** is
    /// not a plugin remote.
    #[test]
    fn site_url_helpers_match_go() {
        let oracle = oracle();
        let cases = oracle["remote_cluster_site_url"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let rc = RemoteCluster {
                site_url: case["site_url"].as_str().unwrap().to_string(),
                plugin_id: case["plugin_id"].as_str().unwrap().to_string(),
                ..Default::default()
            };
            let label = format!(
                "site_url={:?} plugin_id={:?}",
                rc.site_url.as_str(),
                rc.plugin_id.as_str()
            );
            assert_eq!(
                rc.get_site_url(),
                case["display"].as_str().unwrap(),
                "{label}"
            );
            assert_eq!(
                rc.is_confirmed(),
                case["is_confirmed"].as_bool().unwrap(),
                "{label}"
            );
            assert_eq!(
                rc.is_plugin(),
                case["is_plugin"].as_bool().unwrap(),
                "{label}"
            );
        }
    }
}
