//! Port of `server/public/model/integration_action.go` — **chunks 1 to 3 of several**.
//!
//! The file is 1,390 lines. Chunk 1 is `PostAction` and its immediate satellites, which is what
//! `MessageAttachment.Actions` needs; chunk 2 is the whole `Dialog` family — ten wire types,
//! four validators, the date/datetime rules and `IsValidLookupURL`; chunk 3 is the `Post`
//! methods that walk `props.attachments`, which unblock `Post::to_json`/`encode_json`.
//! Deferred to later chunks, each for its own reason:
//!
//! | Go | why not yet |
//! |---|---|
//! | `GenerateTriggerId` / `DecodeAndVerifyTriggerId` | ECDSA signing; needs a crypto decision ([D-046]) |
//! | `EncryptPostActionCookie` / `AddPostActionCookies` | AES-GCM; same decision ([D-046]) |
//! | `GetAction` | needs `MergeQueryIntoURL`, i.e. a `net/url` parser that re-emits ([D-047]) |
//! | `ValidateMmBlocksActions`, `ValidateActionQuery`, `validateIntegrationURL` | the mm_blocks surface, which needs the markdown parser ([D-044]) |
//!
//! **`IsValid` here does not look like the rest of the tree.** Every other ported validator
//! returns a single `*AppError` at the first failure; these return a `*multierror.Error` that
//! accumulates every failure. The message list and its order are the observable — see
//! [`crate::utils::MultiError`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::message_attachment::{MessageAttachment, hex_color_regex};
use crate::post::{POST_PROPS_ATTACHMENTS, Post};
use crate::utils::{
    MultiError, StringInterface, StringMap, go_quote, go_to_lower, is_valid_http_url, is_valid_id,
    new_id,
};

// --- constants ---------------------------------------------------------------------------

pub const POST_ACTION_TYPE_BUTTON: &str = "button";
pub const POST_ACTION_TYPE_SELECT: &str = "select";

pub const POST_ACTION_DATA_SOURCE_USERS: &str = "users";
pub const POST_ACTION_DATA_SOURCE_CHANNELS: &str = "channels";

pub const MAX_MM_BLOCKS_ACTIONS_PER_POST: usize = 50;
pub const MAX_MM_BLOCKS_ACTION_KEY_LENGTH: usize = 64;

pub const MAX_ACTION_QUERY_ENTRIES: usize = 50;
pub const MAX_ACTION_QUERY_KEY_LENGTH: usize = 128;
pub const MAX_ACTION_QUERY_VALUE_LENGTH: usize = 2048;

/// Integration-format values for [`DoPostActionRequest::integration_format`] (client → server).
pub const POST_ACTION_INTEGRATION_FORMAT_ATTACHMENT: &str = "attachment";
pub const POST_ACTION_INTEGRATION_FORMAT_APPS_BINDING: &str = "apps_binding";
pub const POST_ACTION_INTEGRATION_FORMAT_BLOCK: &str = "block";
pub const POST_ACTION_INTEGRATION_FORMAT_CARD: &str = "card";
pub const POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK: &str = "mm_block";

/// The JSON `kind` discriminator for [`MmBlocksActionCookie`] payloads.
pub const MM_BLOCKS_ACTION_COOKIE_KIND: &str = "mm_blocks_actions";

/// Port of `model.PostActionRetainPropKeys` (integration_action.go:67) — the props carried
/// across an interactive-action update.
pub const POST_ACTION_RETAIN_PROP_KEYS: [&str; 5] = [
    crate::post::POST_PROPS_FROM_WEBHOOK,
    crate::post::POST_PROPS_FROM_BOT,
    crate::post::POST_PROPS_FROM_PLUGIN,
    crate::post::POST_PROPS_OVERRIDE_USERNAME,
    crate::post::POST_PROPS_OVERRIDE_ICON_URL,
];

/// The style words [`PostAction::is_valid`] accepts besides a hex colour.
const VALID_ACTION_STYLES: [&str; 6] =
    ["default", "primary", "success", "good", "warning", "danger"];

fn is_false(b: &bool) -> bool {
    !*b
}

fn string_interface_is_empty(m: &StringInterface) -> bool {
    m.is_empty()
}

// --- PostAction ----------------------------------------------------------------------------

/// Port of `model.PostAction` (integration_action.go:162).
///
/// Every field carries `omitempty`, so a zero `PostAction` serialises as `{}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostAction {
    /// A unique action id. Generated automatically when unset.
    #[serde(rename = "id", default, skip_serializing_if = "String::is_empty")]
    pub id: String,

    /// `button` or `select`.
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub action_type: String,

    /// The text on the button, or the select placeholder.
    #[serde(rename = "name", default, skip_serializing_if = "String::is_empty")]
    pub name: String,

    #[serde(rename = "tooltip", default, skip_serializing_if = "String::is_empty")]
    pub tooltip: String,

    #[serde(rename = "disabled", default, skip_serializing_if = "is_false")]
    pub disabled: bool,

    /// `default`, `primary`, `success`, `good`, `warning`, `danger`, or a six-digit hex colour.
    #[serde(rename = "style", default, skip_serializing_if = "String::is_empty")]
    pub style: String,

    /// Empty means the select is populated from [`Self::options`]; otherwise `users` or
    /// `channels`.
    #[serde(
        rename = "data_source",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub data_source: String,

    /// Go's `[]*PostActionOptions` with `omitempty`, which drops a nil slice **and** an empty
    /// one — so the two are indistinguishable on the wire and a plain `Vec` is faithful.
    #[serde(rename = "options", default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PostActionOptions>,

    /// The option pre-selected in a select box. No effect on other action types.
    #[serde(
        rename = "default_option",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub default_option: String,

    /// Integrations carry private plugin data in `context`; they are stripped from posts sent
    /// to a client, or encrypted into [`Self::cookie`].
    #[serde(
        rename = "integration",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub integration: Option<PostActionIntegration>,

    /// `db:"-"` in Go — on the wire, never persisted.
    #[serde(rename = "cookie", default, skip_serializing_if = "String::is_empty")]
    pub cookie: String,
}

impl PostAction {
    /// Port of `(*PostAction).IsValid` (integration_action.go:206).
    ///
    /// Accumulates **every** failure rather than returning the first, and the order of the
    /// messages is the order of the checks: name, style, type (and its nested option checks),
    /// then integration. An empty integration URL produces **two** messages, because the
    /// emptiness check and the shape check are independent `if`s.
    ///
    /// Go's `select action contains nil option` branch is unreachable here: `Options` is
    /// `[]*PostActionOptions` in Go and `Vec<PostActionOptions>` here, so a `null` element
    /// fails at decode time instead. That is the standing [D-033] convention.
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();

        if self.name.is_empty() {
            errs.push("action must have a name");
        }

        if !self.style.is_empty()
            && !VALID_ACTION_STYLES.contains(&self.style.as_str())
            && !hex_color_regex().is_match(&self.style)
        {
            errs.push(format!(
                "invalid style '{}' - must be one of [default, primary, success, good, warning, danger] or a hex color",
                self.style
            ));
        }

        match self.action_type.as_str() {
            POST_ACTION_TYPE_BUTTON => {
                if !self.options.is_empty() {
                    errs.push("button action must not have options");
                }
                if !self.data_source.is_empty() {
                    errs.push("button action must not have a data source");
                }
            }
            POST_ACTION_TYPE_SELECT => {
                if !self.data_source.is_empty() {
                    if self.data_source != POST_ACTION_DATA_SOURCE_USERS
                        && self.data_source != POST_ACTION_DATA_SOURCE_CHANNELS
                    {
                        errs.push(format!(
                            "invalid data_source '{}' for select action",
                            self.data_source
                        ));
                    }
                    if !self.options.is_empty() {
                        errs.push("select action cannot have both DataSource and Options set");
                    }
                } else if self.options.is_empty() {
                    errs.push("select action must have either DataSource or Options set");
                } else {
                    for (i, opt) in self.options.iter().enumerate() {
                        if let Err(e) = opt.is_valid() {
                            errs.extend(e.prefixed(&format!("option at index {i} is invalid:")));
                        }
                    }
                }
            }
            _ => errs.push(format!(
                "invalid action type: must be '{POST_ACTION_TYPE_BUTTON}' or '{POST_ACTION_TYPE_SELECT}'"
            )),
        }

        match self.integration.as_ref() {
            None => errs.push("action must have integration settings"),
            Some(integration) => {
                if integration.url.is_empty() {
                    errs.push("action must have an integration URL");
                }
                // A plugin-relative path is accepted alongside a real HTTP URL. Note `./plugins/`
                // is not — the test is a literal prefix. The "an valid" wording is Go's.
                if !(integration.url.starts_with("/plugins/")
                    || integration.url.starts_with("plugins/")
                    || is_valid_http_url(&integration.url))
                {
                    errs.push("action must have an valid integration URL");
                }
            }
        }

        errs.into_result()
    }

    /// Port of `(*PostAction).Equals` (integration_action.go:272).
    ///
    /// **It never compares `tooltip`, `disabled` or `style`**, so two actions differing only in
    /// those are "equal". Reproduced verbatim — see [D-038].
    ///
    /// Go also indexes `Options[k]` without a nil check and panics on a nil element; a
    /// `Vec<PostActionOptions>` cannot hold one, so that crash is unreachable here.
    pub fn equals(&self, input: &PostAction) -> bool {
        if self.id != input.id
            || self.action_type != input.action_type
            || self.name != input.name
            || self.data_source != input.data_source
            || self.default_option != input.default_option
            || self.cookie != input.cookie
        {
            return false;
        }

        if self.options.len() != input.options.len() {
            return false;
        }
        for (a, b) in self.options.iter().zip(input.options.iter()) {
            if a.text != b.text || a.value != b.value {
                return false;
            }
        }

        match (self.integration.as_ref(), input.integration.as_ref()) {
            (a, None) => a.is_none(),
            (None, Some(_)) => false,
            (Some(a), Some(b)) => {
                a.url == b.url
                    && a.context.len() == b.context.len()
                    // Go compares two `any`s that came out of encoding/json, where every
                    // number is a float64 — so `1` and `1.0` are equal. A plain `==` on
                    // serde_json::Value would say otherwise.
                    && a.context.iter().all(|(k, v)| {
                        b.context
                            .get(k)
                            .is_some_and(|bv| crate::utils::json_values_equal_like_go(v, bv))
                    })
            }
        }
    }
}

/// Port of `model.PostActionOptions` (integration_action.go:390). Neither field carries
/// `omitempty`, so both keys are always present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionOptions {
    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "value")]
    pub value: String,
}

impl PostActionOptions {
    /// Port of `(*PostActionOptions).IsValid` (integration_action.go:395). Emptiness only — a
    /// single space counts as set.
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();
        if self.text.is_empty() {
            errs.push("text is required");
        }
        if self.value.is_empty() {
            errs.push("value is required");
        }
        errs.into_result()
    }
}

/// Port of `model.PostActionIntegration` (integration_action.go:408).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionIntegration {
    /// The endpoint the action is sent to. May be a plugin-relative path.
    #[serde(rename = "url", default, skip_serializing_if = "String::is_empty")]
    pub url: String,

    /// Private plugin data. `omitempty` drops a nil map and an empty one alike, so a plain map
    /// with an emptiness predicate is faithful.
    #[serde(
        rename = "context",
        default,
        skip_serializing_if = "string_interface_is_empty"
    )]
    pub context: StringInterface,
}

/// Port of `model.DoPostActionRequest` (integration_action.go:116).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DoPostActionRequest {
    #[serde(
        rename = "selected_option",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub selected_option: String,

    #[serde(rename = "cookie", default, skip_serializing_if = "String::is_empty")]
    pub cookie: String,

    #[serde(rename = "query", default, skip_serializing_if = "Option::is_none")]
    pub query: Option<StringMap>,

    /// Which format originally carried the action. Empty means a legacy client and is treated
    /// as `attachment` — see [`normalize_post_action_integration_format`].
    #[serde(
        rename = "integration_format",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub integration_format: String,
}

/// Port of `model.NormalizePostActionIntegrationFormat` (integration_action.go:135).
///
/// `TrimSpace` then `ToLower`, then a whitelist; anything unrecognised — including an empty
/// string and a whitespace-only one — becomes `attachment`.
pub fn normalize_post_action_integration_format(s: &str) -> &'static str {
    // `strings.ToLower`, not `str::to_lowercase` — see [`go_to_lower`].
    match go_to_lower(s.trim()).as_str() {
        POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK => POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK,
        POST_ACTION_INTEGRATION_FORMAT_APPS_BINDING => POST_ACTION_INTEGRATION_FORMAT_APPS_BINDING,
        POST_ACTION_INTEGRATION_FORMAT_BLOCK => POST_ACTION_INTEGRATION_FORMAT_BLOCK,
        POST_ACTION_INTEGRATION_FORMAT_CARD => POST_ACTION_INTEGRATION_FORMAT_CARD,
        _ => POST_ACTION_INTEGRATION_FORMAT_ATTACHMENT,
    }
}

// Go's TrimSpace cuts Unicode whitespace, which `str::trim` also does.

/// Port of `model.PostActionCookie` (integration_action.go:362). Serialised and encrypted into
/// [`PostAction::cookie`] so the server can recover action metadata for ephemeral posts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionCookie {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub cookie_type: String,

    #[serde(rename = "post_id", default, skip_serializing_if = "String::is_empty")]
    pub post_id: String,

    #[serde(
        rename = "root_post_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub root_post_id: String,

    #[serde(
        rename = "channel_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub channel_id: String,

    #[serde(
        rename = "data_source",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub data_source: String,

    #[serde(
        rename = "integration",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub integration: Option<PostActionIntegration>,

    #[serde(
        rename = "retain_props",
        default,
        skip_serializing_if = "string_interface_is_empty"
    )]
    pub retain_props: StringInterface,

    #[serde(
        rename = "remove_props",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remove_props: Vec<String>,
}

/// Port of `model.MmBlocksActionCookie` (integration_action.go:380).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MmBlocksActionCookie {
    #[serde(rename = "kind", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,

    #[serde(rename = "post_id", default, skip_serializing_if = "String::is_empty")]
    pub post_id: String,

    #[serde(
        rename = "root_post_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub root_post_id: String,

    #[serde(
        rename = "channel_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub channel_id: String,

    #[serde(
        rename = "retain_props",
        default,
        skip_serializing_if = "string_interface_is_empty"
    )]
    pub retain_props: StringInterface,

    #[serde(
        rename = "remove_props",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remove_props: Vec<String>,

    /// The only field here **without** `omitempty`, so a nil map is `null` and the key is
    /// always present. A `BTreeMap` because Go sorts map keys when marshalling.
    ///
    /// `default` is needed as well as the missing `omitempty`: an absent key zero-fills in Go,
    /// and `ParseDecryptedActionCookiePayload` decodes cookies that were written without one.
    #[serde(rename = "actions", default)]
    pub actions: Option<BTreeMap<String, StringInterface>>,
}

/// Port of `model.PostActionIntegrationRequest` (integration_action.go:415).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionIntegrationRequest {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "user_name")]
    pub user_name: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "channel_name")]
    pub channel_name: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// The Go field is `TeamName`; the wire key is `team_domain`.
    #[serde(rename = "team_domain")]
    pub team_name: String,

    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "trigger_id")]
    pub trigger_id: String,

    #[serde(rename = "type")]
    pub request_type: String,

    #[serde(rename = "data_source")]
    pub data_source: String,

    #[serde(
        rename = "context",
        default,
        skip_serializing_if = "string_interface_is_empty"
    )]
    pub context: StringInterface,
}

/// Port of `model.PostActionIntegrationResponse` (integration_action.go:429).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionIntegrationResponse {
    /// No `omitempty`, so a nil post is `null` and the key is always present.
    #[serde(rename = "update")]
    pub update: Option<Post>,

    #[serde(rename = "ephemeral_text")]
    pub ephemeral_text: String,

    /// Set to skip the Slack-compatibility handling of the text.
    #[serde(rename = "skip_slack_parsing")]
    pub skip_slack_parsing: bool,

    #[serde(
        rename = "goto_location",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub goto_location: String,
}

/// Port of `model.PostActionAPIResponse` (integration_action.go:436).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostActionAPIResponse {
    /// Kept for backwards compatibility, per Go's comment.
    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "trigger_id")]
    pub trigger_id: String,

    #[serde(
        rename = "goto_location",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub goto_location: String,
}

/// Port of `model.ExecuteDialogActionResponse` (integration_action.go:442).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecuteDialogActionResponse {
    #[serde(rename = "trigger_id")]
    pub trigger_id: String,
}

/// Port of `model.PostActionPreserve` (integration_action.go:76). No `json:` tags — internal
/// state carried across an integration response, not a wire type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PostActionPreserve {
    pub retain: StringInterface,
    pub remove: Vec<String>,
    pub original_props: Option<StringInterface>,
    pub original_is_pinned: bool,
    pub original_has_reactions: bool,
    pub root_post_id: String,
}

impl Post {
    /// Port of `(*Post).PostActionPreserveState` (integration_action.go:86).
    ///
    /// Note this partitions on **key membership**, not on the value: a prop stored as an
    /// explicit `null` is *retained*, not removed. `Post::preserve_identity_props_from` reads
    /// the same class of props through `GetProp` and therefore treats a stored null the
    /// opposite way. Both behaviours are pinned.
    pub fn post_action_preserve_state(&self) -> PostActionPreserve {
        let mut retain = StringInterface::new();
        let mut remove = Vec::new();

        for key in POST_ACTION_RETAIN_PROP_KEYS {
            match self.props.as_ref().and_then(|p| p.get(key)) {
                Some(value) => {
                    retain.insert(key.to_string(), value.clone());
                }
                None => remove.push(key.to_string()),
            }
        }

        let root_post_id = if self.root_id.is_empty() {
            self.id.clone()
        } else {
            self.root_id.clone()
        };

        PostActionPreserve {
            retain,
            remove,
            original_props: self.props.clone(),
            original_is_pinned: self.is_pinned,
            original_has_reactions: self.has_reactions,
            root_post_id,
        }
    }

    /// Port of `(*Post).StripActionIntegrations` (integration_action.go:1044).
    ///
    /// Removes the private `integration` block from every interactive action before a post
    /// reaches a client, and drops the mm_blocks action registry unless it has already been
    /// encrypted into a cookie.
    ///
    /// **It rewrites `props.attachments` rather than editing it.** The value it stores is
    /// whatever [`Post::attachments`] decoded, so the client's original payload is normalised
    /// away: unknown keys vanish, a wrongly-typed element is dropped, nil actions and fields
    /// are stripped, and every omitted key comes back at its zero value. Three consequences
    /// are worth knowing before calling this and none is visible in the Go source:
    ///
    /// - **`{"attachments": []}` becomes `{"attachments": null}`.** Go's `Attachments()`
    ///   returns a *nil* slice when nothing decodes, and a nil Go slice marshals as `null`.
    /// - **A non-array `attachments` is replaced by `null`**, so `"attachments": "nope"`
    ///   survives `Attachments()` untouched and does not survive this.
    /// - **An `attachments` prop holding an explicit JSON `null` is left alone**, because
    ///   `GetProp` cannot distinguish it from an absent key. It reads the same on the wire.
    pub fn strip_action_integrations(&mut self) {
        let mut attachments = self.attachments();

        // Go stores the slice first and nils the integrations afterwards, which works only
        // because the stored value aliases the same backing array. Rust owns its values, so the
        // two steps swap order here; the stored result is identical either way.
        for attachment in &mut attachments {
            for action in &mut attachment.actions {
                action.integration = None;
            }
        }

        if self.get_prop(POST_PROPS_ATTACHMENTS).is_some() {
            self.add_prop(POST_PROPS_ATTACHMENTS, attachments_prop_value(&attachments));
        }

        self.strip_mm_blocks_action_secrets();
    }

    /// Port of `(*Post).GenerateActionIds` (integration_action.go:1246).
    ///
    /// Mints an id for every interactive action that has none, so a click can be routed back to
    /// the action that produced it. Called from [`Post::pre_commit`], which is what [D-035]
    /// was waiting on.
    ///
    /// The emptiness test is exact: an id of `"  "` or `"x"` is *kept*, however unusable it is.
    /// Like [`Self::strip_action_integrations`] it rewrites `props.attachments` with the
    /// decoded slice, so the normalisation described there applies here too — including for a
    /// post with no interactive actions at all, which is why `{"attachments": []}` comes out as
    /// `{"attachments": null}` after an ordinary `pre_save`.
    pub fn generate_action_ids(&mut self) {
        // Go's second type assertion cannot succeed when the prop is absent, because the first
        // branch is what stores the natively-typed slice. An absent prop is therefore a no-op.
        if self.get_prop(POST_PROPS_ATTACHMENTS).is_none() {
            return;
        }

        let mut attachments = self.attachments();
        for attachment in &mut attachments {
            for action in &mut attachment.actions {
                if action.id.is_empty() {
                    action.id = new_id();
                }
            }
        }

        self.add_prop(POST_PROPS_ATTACHMENTS, attachments_prop_value(&attachments));
    }
}

/// The value the two rewriters store back into `props.attachments`.
///
/// Go stores the `[]*MessageAttachment` that `Attachments()` returned, and that slice is **nil**
/// whenever nothing decoded — a nil Go slice marshals as `null`, not `[]`. So an empty list has
/// to become [`serde_json::Value::Null`] here or the wire drifts on every post whose attachments
/// were empty, absent-shaped or malformed.
///
/// `serde_json::to_value` cannot fail for a [`MessageAttachment`]: its only two error paths are a
/// map with non-string keys and a non-finite `f64`, and the type can express neither. The
/// fallback is unreachable rather than lossy, and it is written as a fallback rather than an
/// `expect` because library code in this crate does not panic.
fn attachments_prop_value(attachments: &[MessageAttachment]) -> serde_json::Value {
    if attachments.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::to_value(attachments).unwrap_or(serde_json::Value::Null)
}

// --- the Dialog family (chunk 2) ---------------------------------------------------------------

/// Byte length caps, all measured against `len()` in Go — **bytes, not runes**, so 13 two-byte
/// characters exhaust the 24-byte display-name cap.
pub const DIALOG_TITLE_MAX_LENGTH: usize = 24;
pub const DIALOG_ELEMENT_DISPLAY_NAME_MAX_LENGTH: usize = 24;
pub const DIALOG_ELEMENT_NAME_MAX_LENGTH: usize = 300;
pub const DIALOG_ELEMENT_HELP_TEXT_MAX_LENGTH: usize = 150;
pub const DIALOG_ELEMENT_TEXT_MAX_LENGTH: usize = 150;
pub const DIALOG_ELEMENT_TEXTAREA_MAX_LENGTH: usize = 3000;
pub const DIALOG_ELEMENT_SELECT_MAX_LENGTH: usize = 3000;
pub const DIALOG_ELEMENT_BOOL_MAX_LENGTH: usize = 150;
pub const DIALOG_ELEMENT_FILE_MAX_LENGTH: usize = 300;
/// Minutes between time options in a datetime dropdown. Nothing in the model reads it — it is a
/// client hint, and `IsValid` treats a zero interval as "omitted" rather than as this value.
pub const DEFAULT_TIME_INTERVAL_MINUTES: i64 = 60;
pub const MAX_DIALOG_FILE_IDS: usize = 10;
/// Bounds a defence-in-depth scan of `SubmitDialogRequest.Submission` for id-shaped tokens. Not
/// the file-upload limit — that is [`MAX_DIALOG_FILE_IDS`].
pub const MAX_DIALOG_SUBMISSION_ID_SHAPED_TOKEN_SCAN: usize = 256;

/// Go's reference-time layouts. They are kept as constants because they are exported API, but
/// nothing here passes them to a parser: `time.Parse`'s behaviour is reproduced directly by
/// [`parse_go_iso_date`] and [`parse_go_iso_date_time`], which are pinned against Go.
pub const ISO_DATE_FORMAT: &str = "2006-01-02";
pub const ISO_DATE_TIME_FORMAT: &str = "2006-01-02T15:04:05Z";
pub const ISO_DATE_TIME_WITH_TIMEZONE_FORMAT: &str = "2006-01-02T15:04:05-07:00";
pub const ISO_DATE_TIME_NO_TIMEZONE_FORMAT: &str = "2006-01-02T15:04:05";
pub const ISO_DATE_TIME_NO_SECONDS_FORMAT: &str = "2006-01-02T15:04";

pub const SUBMIT_DIALOG_RESPONSE_TYPE_EMPTY: &str = "";
pub const SUBMIT_DIALOG_RESPONSE_TYPE_OK: &str = "ok";
pub const SUBMIT_DIALOG_RESPONSE_TYPE_FORM: &str = "form";
pub const SUBMIT_DIALOG_RESPONSE_TYPE_NAVIGATE: &str = "navigate";

/// One Go `fmt.Errorf` / `errors.New` message.
///
/// The Dialog validators return a bare `error` in two places and a `*multierror.Error` in the
/// rest. The bare ones cannot be modelled as a one-element [`MultiError`]: that would render as
/// `"1 error occurred:\n\t* …"` where Go renders the message alone. The text **is** the
/// observable — integration developers read it out of the API response — so it is carried as a
/// string rather than re-derived from a variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DialogError(pub String);

impl DialogError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn string_map_is_empty(m: &StringMap) -> bool {
    m.is_empty()
}

/// Port of `model.Dialog` (integration_action.go:446).
///
/// Only `source_url` carries `omitempty`; every other key is always present, and a nil
/// `elements` is `null` rather than `[]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dialog {
    #[serde(rename = "callback_id")]
    pub callback_id: String,

    #[serde(rename = "title")]
    pub title: String,

    #[serde(rename = "introduction_text")]
    pub introduction_text: String,

    #[serde(rename = "icon_url")]
    pub icon_url: String,

    /// `[]DialogElement` — a slice of **values**, not pointers, so unlike every other collection
    /// in the tree it cannot carry a nil element ([D-033] does not apply). No `omitempty`, so
    /// the nil/empty distinction is on the wire.
    #[serde(rename = "elements")]
    pub elements: Option<Vec<DialogElement>>,

    #[serde(rename = "submit_label")]
    pub submit_label: String,

    #[serde(rename = "notify_on_cancel")]
    pub notify_on_cancel: bool,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(rename = "source_url", skip_serializing_if = "String::is_empty")]
    pub source_url: String,
}

/// Port of `model.DialogDateTimeConfig` (integration_action.go:459). Every field carries
/// `omitempty`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialogDateTimeConfig {
    #[serde(rename = "min_date", skip_serializing_if = "String::is_empty")]
    pub min_date: String,

    #[serde(rename = "max_date", skip_serializing_if = "String::is_empty")]
    pub max_date: String,

    #[serde(rename = "time_interval", skip_serializing_if = "is_zero_i64")]
    pub time_interval: i64,

    #[serde(rename = "location_timezone", skip_serializing_if = "String::is_empty")]
    pub location_timezone: String,

    #[serde(rename = "manual_time_entry", skip_serializing_if = "is_false")]
    pub manual_time_entry: bool,

    /// Deprecated in Go in favour of `manual_time_entry`; the two are OR'd, because `omitempty`
    /// makes an explicit `false` indistinguishable from an absent key.
    #[serde(rename = "allow_manual_time_entry", skip_serializing_if = "is_false")]
    pub allow_manual_time_entry: bool,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// Port of `model.DialogElement` (integration_action.go:475).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialogElement {
    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "type")]
    pub element_type: String,

    #[serde(rename = "subtype")]
    pub sub_type: String,

    #[serde(rename = "default")]
    pub default: String,

    #[serde(rename = "placeholder")]
    pub placeholder: String,

    #[serde(rename = "help_text")]
    pub help_text: String,

    #[serde(rename = "optional")]
    pub optional: bool,

    #[serde(rename = "min_length")]
    pub min_length: i64,

    #[serde(rename = "max_length")]
    pub max_length: i64,

    #[serde(rename = "data_source")]
    pub data_source: String,

    #[serde(rename = "data_source_url", skip_serializing_if = "String::is_empty")]
    pub data_source_url: String,

    /// No `omitempty`, so a nil list is `null`.
    #[serde(rename = "options")]
    pub options: Option<Vec<PostActionOptions>>,

    #[serde(rename = "multiselect")]
    pub multi_select: bool,

    #[serde(rename = "allow_multiple", skip_serializing_if = "is_false")]
    pub allow_multiple: bool,

    #[serde(rename = "refresh", skip_serializing_if = "is_false")]
    pub refresh: bool,

    #[serde(rename = "datetime_config", skip_serializing_if = "Option::is_none")]
    pub date_time_config: Option<DialogDateTimeConfig>,

    /// Deprecated in Go; [`Self::effective_date_time_config`] resolves it against
    /// `datetime_config`.
    #[serde(rename = "min_date", skip_serializing_if = "String::is_empty")]
    pub min_date: String,

    /// Deprecated in Go. See [`Self::effective_date_time_config`].
    #[serde(rename = "max_date", skip_serializing_if = "String::is_empty")]
    pub max_date: String,

    /// Deprecated in Go. See [`Self::effective_date_time_config`].
    #[serde(rename = "time_interval", skip_serializing_if = "is_zero_i64")]
    pub time_interval: i64,

    #[serde(rename = "action_button", skip_serializing_if = "Option::is_none")]
    pub action_button: Option<DialogActionButton>,
}

/// Port of `model.DialogActionButton` (integration_action.go:537).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialogActionButton {
    #[serde(rename = "url")]
    pub url: String,

    /// `omitempty` drops a nil map and an empty one alike.
    #[serde(rename = "context", skip_serializing_if = "string_map_is_empty")]
    pub context: StringMap,
}

/// Port of `model.OpenDialogRequest` (integration_action.go:542).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenDialogRequest {
    #[serde(rename = "trigger_id")]
    pub trigger_id: String,

    #[serde(rename = "url")]
    pub url: String,

    #[serde(rename = "dialog")]
    pub dialog: Dialog,
}

/// Port of `model.SubmitDialogRequest` (integration_action.go:548).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubmitDialogRequest {
    #[serde(rename = "type")]
    pub request_type: String,

    #[serde(rename = "url", skip_serializing_if = "String::is_empty")]
    pub url: String,

    #[serde(rename = "callback_id")]
    pub callback_id: String,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// No `omitempty`, so a nil submission is `null`.
    #[serde(rename = "submission")]
    pub submission: Option<StringInterface>,

    #[serde(rename = "cancelled")]
    pub cancelled: bool,

    /// `omitempty` drops a nil slice and an empty one alike.
    #[serde(rename = "file_ids", skip_serializing_if = "Vec::is_empty")]
    pub file_ids: Vec<String>,
}

/// Port of `model.SubmitDialogResponse` (integration_action.go:570). Every field carries
/// `omitempty`, so a zero response is `{}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubmitDialogResponse {
    #[serde(rename = "error", skip_serializing_if = "String::is_empty")]
    pub error: String,

    #[serde(rename = "errors", skip_serializing_if = "string_map_is_empty")]
    pub errors: StringMap,

    /// Go declares this as a plain `string`, not as `SubmitDialogResponseType`, so an unknown
    /// value round-trips and `IsValid` is what rejects it.
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub response_type: String,

    #[serde(rename = "form", skip_serializing_if = "Option::is_none")]
    pub form: Option<Dialog>,
}

/// Port of `model.ExecuteDialogActionRequest` (integration_action.go:577).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecuteDialogActionRequest {
    #[serde(rename = "url")]
    pub url: String,

    #[serde(rename = "context", skip_serializing_if = "string_map_is_empty")]
    pub context: StringMap,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,
}

/// Port of `model.DialogSelectOption` (integration_action.go:613).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DialogSelectOption {
    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "value")]
    pub value: String,
}

/// Port of `model.LookupDialogResponse` (integration_action.go:619).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LookupDialogResponse {
    /// No `omitempty`, so a nil list is `null`.
    #[serde(rename = "items")]
    pub items: Option<Vec<DialogSelectOption>>,
}

impl DialogElement {
    /// Port of `(*DialogElement).EffectiveDateTimeConfig` (integration_action.go:512).
    ///
    /// Merges `datetime_config` over the three deprecated top-level fields. Two details are
    /// load-bearing and both are pinned:
    ///
    /// - an **empty** `min_date`/`max_date` or a **zero** `time_interval` inside the config does
    ///   not override — the deprecated value survives.
    /// - `location_timezone` is copied unconditionally, because it has no deprecated
    ///   counterpart, and `manual_time_entry` is OR'd with the deprecated
    ///   `allow_manual_time_entry` since `omitempty` cannot express an explicit `false`.
    pub fn effective_date_time_config(&self) -> DialogDateTimeConfig {
        let mut cfg = DialogDateTimeConfig {
            min_date: self.min_date.clone(),
            max_date: self.max_date.clone(),
            time_interval: self.time_interval,
            ..Default::default()
        };

        if let Some(config) = self.date_time_config.as_ref() {
            if !config.min_date.is_empty() {
                cfg.min_date = config.min_date.clone();
            }
            if !config.max_date.is_empty() {
                cfg.max_date = config.max_date.clone();
            }
            if config.time_interval != 0 {
                cfg.time_interval = config.time_interval;
            }
            cfg.location_timezone = config.location_timezone.clone();
            cfg.manual_time_entry = config.manual_time_entry || config.allow_manual_time_entry;
        }

        cfg
    }

    /// Port of `(*DialogElement).IsValid` (integration_action.go:761).
    ///
    /// The largest validator in the model package: five shared checks, then a switch with nine
    /// arms, each carrying its own caps and wording. Every failure accumulates, so the **order**
    /// of the messages is the output.
    ///
    /// Three results are worth knowing before reading the arms:
    ///
    /// - **`max_length` defaults to 0**, so any positive `min_length` fails the `min > max`
    ///   check on an otherwise untouched element.
    /// - **The `text`/`textarea` subtype failure reports the element's *type*** — Go interpolates
    ///   `e.Type` into a message about the subtype. Reproduced verbatim.
    /// - **`radio` checks nothing but its default**, and `bool` never length-checks its default.
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();

        if self.min_length < 0 {
            errs.push(format!(
                "min length cannot be a negative number, got {}",
                self.min_length
            ));
        }
        if self.min_length > self.max_length {
            errs.push(format!(
                "min length should be less then max length, got {} > {}",
                self.min_length, self.max_length
            ));
        }

        push_max_length(
            &mut errs,
            "DisplayName",
            &self.display_name,
            DIALOG_ELEMENT_DISPLAY_NAME_MAX_LENGTH,
        );
        push_max_length(
            &mut errs,
            "Name",
            &self.name,
            DIALOG_ELEMENT_NAME_MAX_LENGTH,
        );
        push_max_length(
            &mut errs,
            "HelpText",
            &self.help_text,
            DIALOG_ELEMENT_HELP_TEXT_MAX_LENGTH,
        );

        if self.multi_select && self.element_type != "select" {
            errs.push(format!(
                "multiselect can only be used with select elements, got type {}",
                go_quote(&self.element_type)
            ));
        }
        if self.allow_multiple && self.element_type != "file" {
            errs.push(format!(
                "allow_multiple can only be used with file elements, got type {}",
                go_quote(&self.element_type)
            ));
        }

        match self.element_type.as_str() {
            "text" => self.push_text_arm(&mut errs, DIALOG_ELEMENT_TEXT_MAX_LENGTH),
            "textarea" => self.push_text_arm(&mut errs, DIALOG_ELEMENT_TEXTAREA_MAX_LENGTH),
            "select" => self.push_select_arm(&mut errs),
            "bool" => {
                if !self.default.is_empty() && self.default != "true" && self.default != "false" {
                    errs.push("invalid default of bool");
                }
                push_max_length(
                    &mut errs,
                    "Placeholder",
                    &self.placeholder,
                    DIALOG_ELEMENT_BOOL_MAX_LENGTH,
                );
            }
            "radio" => {
                if !is_default_in_options(&self.default, self.options.as_deref()) {
                    errs.push(format!(
                        "default value {} doesn't exist in options ",
                        go_quote(&self.default)
                    ));
                }
            }
            "date" => {
                let cfg = self.effective_date_time_config();
                push_max_length(
                    &mut errs,
                    "Default",
                    &self.default,
                    DIALOG_ELEMENT_TEXT_MAX_LENGTH,
                );
                push_max_length(
                    &mut errs,
                    "Placeholder",
                    &self.placeholder,
                    DIALOG_ELEMENT_TEXT_MAX_LENGTH,
                );
                push_option(&mut errs, validate_date_format(&self.default));
                push_option(&mut errs, validate_date_format(&cfg.min_date));
                push_option(&mut errs, validate_date_format(&cfg.max_date));
            }
            "datetime" => {
                let cfg = self.effective_date_time_config();
                push_max_length(
                    &mut errs,
                    "Default",
                    &self.default,
                    DIALOG_ELEMENT_TEXT_MAX_LENGTH,
                );
                push_max_length(
                    &mut errs,
                    "Placeholder",
                    &self.placeholder,
                    DIALOG_ELEMENT_TEXT_MAX_LENGTH,
                );
                push_option(&mut errs, validate_date_time_format(&self.default));
                push_option(&mut errs, validate_date_or_date_time_format(&cfg.min_date));
                push_option(&mut errs, validate_date_or_date_time_format(&cfg.max_date));

                // A zero interval means "omitted" and is left alone; the default is never
                // substituted here.
                if cfg.time_interval != 0 {
                    if !(1..=1440).contains(&cfg.time_interval) {
                        errs.push(format!(
                            "time_interval must be between 1 and 1440 minutes, got {}",
                            cfg.time_interval
                        ));
                    } else if 1440 % cfg.time_interval != 0 {
                        errs.push(format!(
                            "time_interval must be a divisor of 1440 (24 hours * 60 minutes) to create valid time intervals, got {}",
                            cfg.time_interval
                        ));
                    }
                }
            }
            "file" => self.push_file_arm(&mut errs),
            "action_button" => match self.action_button.as_ref() {
                None => errs.push("action_button element requires action_button configuration"),
                Some(button) if button.url.is_empty() => {
                    errs.push("action_button requires a non-empty URL");
                }
                Some(button) if !is_valid_lookup_url(&button.url) => {
                    // Go wraps a bare `errors.New("invalid URL")`, which renders as
                    // `<wrap>: <cause>`.
                    errs.push("invalid action_button URL: invalid URL");
                }
                Some(_) => {}
            },
            other => errs.push(format!("invalid element type: {}", go_quote(other))),
        }

        errs.into_result()
    }

    /// The `text` and `textarea` arms differ only in their cap.
    fn push_text_arm(&self, errs: &mut MultiError, max_length: usize) {
        push_max_length(errs, "Default", &self.default, max_length);
        push_max_length(errs, "Placeholder", &self.placeholder, max_length);

        // The message interpolates the element's **type**, not its subtype — upstream's bug,
        // reproduced.
        if !matches!(
            self.sub_type.as_str(),
            "" | "text" | "email" | "number" | "tel" | "url" | "password"
        ) {
            errs.push(format!("invalid subtype {}", go_quote(&self.element_type)));
        }
    }

    fn push_select_arm(&self, errs: &mut MultiError) {
        push_max_length(
            errs,
            "Default",
            &self.default,
            DIALOG_ELEMENT_SELECT_MAX_LENGTH,
        );
        push_max_length(
            errs,
            "Placeholder",
            &self.placeholder,
            DIALOG_ELEMENT_SELECT_MAX_LENGTH,
        );

        let has_options = self.options.as_ref().is_some_and(|o| !o.is_empty());

        if !self.data_source.is_empty()
            && self.data_source != "users"
            && self.data_source != "channels"
            && self.data_source != "dynamic"
        {
            errs.push(format!(
                "invalid data source {}, allowed are 'users', 'channels', or 'dynamic'",
                go_quote(&self.data_source)
            ));
        }

        if self.data_source == "dynamic" {
            if self.data_source_url.is_empty() {
                errs.push("dynamic data_source requires data_source_url");
            } else if !is_valid_lookup_url(&self.data_source_url) {
                errs.push("invalid data_source_url for dynamic select");
            }
            if has_options {
                errs.push("dynamic select element should not have static options");
            }
        } else if self.data_source.is_empty() {
            // Note this branch is skipped entirely for an *invalid* data source, so a bad
            // source hides a bad default.
            if self.multi_select {
                if !is_multi_select_default_in_options(&self.default, self.options.as_deref()) {
                    errs.push(format!(
                        "multiselect default value {} contains values not in options",
                        go_quote(&self.default)
                    ));
                }
            } else if !is_default_in_options(&self.default, self.options.as_deref()) {
                errs.push(format!(
                    "default value {} doesn't exist in options ",
                    go_quote(&self.default)
                ));
            }
        }
    }

    fn push_file_arm(&self, errs: &mut MultiError) {
        push_max_length(
            errs,
            "Placeholder",
            &self.placeholder,
            DIALOG_ELEMENT_FILE_MAX_LENGTH,
        );
        push_max_length(
            errs,
            "Default",
            &self.default,
            DIALOG_ELEMENT_FILE_MAX_LENGTH,
        );

        if !self.default.is_empty() {
            let mut parsed_ids = Vec::new();
            for id in self.default.split(',') {
                let id = id.trim();
                if id.is_empty() {
                    continue;
                }
                if !is_valid_id(id) {
                    errs.push(format!(
                        "default file ID {} is not a valid ID",
                        go_quote(id)
                    ));
                    continue;
                }
                parsed_ids.push(id);
            }

            if !self.allow_multiple && parsed_ids.len() > 1 {
                errs.push(
                    "default may not contain more than one file ID when allow_multiple is false",
                );
            }
            if parsed_ids.len() > MAX_DIALOG_FILE_IDS {
                errs.push(format!(
                    "default may not contain more than {MAX_DIALOG_FILE_IDS} file IDs, got {}",
                    parsed_ids.len()
                ));
            }
        }

        if self.options.as_ref().is_some_and(|o| !o.is_empty()) {
            errs.push("file elements cannot have options");
        }
        if !self.data_source.is_empty() {
            errs.push("file elements cannot have a data source");
        }
    }
}

impl Dialog {
    /// Port of `(*Dialog).IsValid` (integration_action.go:732).
    ///
    /// **The element failures nest rather than flatten.** Go wraps each one with
    /// `errors.Wrapf(err, "%q field is not valid", name)`, and the wrapped value is a whole
    /// `*multierror.Error` — so one bad element contributes exactly **one** parent message
    /// containing a rendered `"3 errors occurred:\n\t* …"` block. That is the opposite of
    /// `PostAction::is_valid`, which uses `multierror.Prefix` and splices the children in flat.
    ///
    /// The duplicate-name check runs **before** the element's own validation, so a duplicated
    /// invalid element reports the duplicate first.
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();

        if self.title.is_empty() || self.title.len() > DIALOG_TITLE_MAX_LENGTH {
            errs.push(format!("invalid dialog title {}", go_quote(&self.title)));
        }

        // Plain `IsValidHTTPURL` — a `/plugins/` path is **not** accepted here, unlike the
        // element URLs, which go through `is_valid_lookup_url`.
        if !self.icon_url.is_empty() && !is_valid_http_url(&self.icon_url) {
            errs.push("invalid icon url");
        }

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for element in self.elements.iter().flatten() {
            if !seen.insert(element.name.as_str()) {
                errs.push(format!(
                    "duplicate dialog element {}",
                    go_quote(&element.name)
                ));
            }

            if let Err(e) = element.is_valid() {
                errs.push(format!(
                    "{} field is not valid: {e}",
                    go_quote(&element.name)
                ));
            }
        }

        errs.into_result()
    }
}

impl OpenDialogRequest {
    /// Port of `(*OpenDialogRequest).IsValid` (integration_action.go:714).
    ///
    /// The URL is only tested for **emptiness** — `"not a url"` passes. The dialog's failures
    /// are spliced in flat, because `multierror.Append` flattens a `*multierror.Error` argument;
    /// contrast [`Dialog::is_valid`], which wraps and therefore nests.
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();

        if self.url.is_empty() {
            errs.push("empty URL");
        }
        if self.trigger_id.is_empty() {
            errs.push("empty trigger id");
        }
        if let Err(e) = self.dialog.is_valid() {
            errs.extend(e);
        }

        errs.into_result()
    }
}

impl SubmitDialogResponse {
    /// Port of `(*SubmitDialogResponse).IsValid` (integration_action.go:584).
    ///
    /// Returns a single error, not an accumulated list. `error` or a **non-empty** `errors` map
    /// short-circuits to valid and everything else is ignored — including a `type` that would
    /// otherwise be rejected. An *empty* `errors` map does not short-circuit.
    pub fn is_valid(&self) -> Result<(), DialogError> {
        if !self.error.is_empty() || !self.errors.is_empty() {
            return Ok(());
        }

        match self.response_type.as_str() {
            SUBMIT_DIALOG_RESPONSE_TYPE_EMPTY
            | SUBMIT_DIALOG_RESPONSE_TYPE_OK
            | SUBMIT_DIALOG_RESPONSE_TYPE_NAVIGATE => {
                if self.form.is_some() {
                    return Err(DialogError::new(format!(
                        "form field must be nil for type {}",
                        go_quote(&self.response_type)
                    )));
                }
            }
            SUBMIT_DIALOG_RESPONSE_TYPE_FORM => match self.form.as_ref() {
                None => return Err(DialogError::new("form field is required for form type")),
                Some(form) => {
                    if let Err(e) = form.is_valid() {
                        // `errors.Wrap` renders as `<message>: <cause>`, and the cause is a
                        // whole multierror block.
                        return Err(DialogError::new(format!("invalid form: {e}")));
                    }
                }
            },
            other => {
                return Err(DialogError::new(format!(
                    "invalid type {}, must be one of: empty, ok, form, navigate",
                    go_quote(other)
                )));
            }
        }

        Ok(())
    }
}

/// Port of `model.IsValidLookupURL` (integration_action.go:1374).
///
/// A `/plugins/` path is accepted without any URL parsing, subject to a traversal guard that
/// rejects **any** `..` or `//` anywhere in the string — including in a filename like
/// `/plugins/x..y`. Anything else must be a full HTTP URL, and that path applies **no**
/// traversal guard, so `https://example.com/../x` is valid.
pub fn is_valid_lookup_url(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }

    if url.starts_with("/plugins/") {
        // The scan covers the **whole** URL, and that is not the same as scanning the part after
        // the prefix: the prefix ends in `/`, so `/plugins//x` contains `//` across the
        // boundary and is rejected. The oracle caught exactly that shortcut.
        return !url.contains("..") && !url.contains("//");
    }

    is_valid_http_url(url)
}

/// Port of `checkMaxLength` (integration_action.go:1029), which is fallible in two ways and
/// returns at most one of them.
///
/// The **emptiness** rule applies only to the two fields Go names by string comparison, and it
/// returns before the length check.
fn check_max_length(field_name: &str, field: &str, max_length: usize) -> Option<String> {
    if (field_name == "DisplayName" || field_name == "Name") && field.is_empty() {
        return Some(format!("{field_name} cannot be empty"));
    }

    if field.len() > max_length {
        return Some(format!(
            "{field_name} cannot be longer than {max_length} characters, got {}",
            field.len()
        ));
    }

    None
}

fn push_max_length(errs: &mut MultiError, field_name: &str, field: &str, max_length: usize) {
    push_option(errs, check_max_length(field_name, field, max_length));
}

/// `multierror.Append` skips a nil error, so every `Append(multiErr, check(...))` call site in
/// the Go source is conditional despite looking unconditional.
fn push_option(errs: &mut MultiError, message: Option<String>) {
    if let Some(message) = message {
        errs.push(message);
    }
}

/// Port of `isDefaultInOptions` (integration_action.go:918). An empty default is always fine.
fn is_default_in_options(default_value: &str, options: Option<&[PostActionOptions]>) -> bool {
    if default_value.is_empty() {
        return true;
    }

    options
        .unwrap_or_default()
        .iter()
        .any(|o| o.value == default_value)
}

/// Port of `isMultiSelectDefaultInOptions` (integration_action.go:932).
///
/// **Spaces are stripped from the whole string, not trimmed per value**, so an option whose
/// value contains a space can never be matched by a multiselect default.
fn is_multi_select_default_in_options(
    default_value: &str,
    options: Option<&[PostActionOptions]>,
) -> bool {
    if default_value.is_empty() {
        return true;
    }

    let stripped = default_value.replace(' ', "");
    let options = options.unwrap_or_default();

    stripped
        .split(',')
        .filter(|value| !value.is_empty())
        .all(|value| options.iter().any(|o| o.value == value))
}

/// Port of `validateRelativePattern` (integration_action.go:958).
///
/// Three to five **bytes**, a leading `+` or `-`, a trailing unit from `dwmHMS` (case
/// sensitive — `+1h` is invalid where `+1H` is valid), and a middle that `strconv.Atoi` accepts.
/// Atoi takes its own sign, so `++5d` and `+-5d` are both valid patterns.
fn validate_relative_pattern(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 3 || bytes.len() > 5 || (bytes[0] != b'+' && bytes[0] != b'-') {
        return false;
    }

    let last = bytes[bytes.len() - 1];
    if !b"dwmHMS".contains(&last) {
        return false;
    }

    // Go slices bytes, which can split a multi-byte character; an invalid slice cannot be a
    // number either way, so a failed UTF-8 decode is the same answer.
    match std::str::from_utf8(&bytes[1..bytes.len() - 1]) {
        Ok(number_part) => number_part.parse::<i64>().is_ok(),
        Err(_) => false,
    }
}

/// Port of `isValidRelativeFormat` (integration_action.go:974). Case sensitive: `Today` is not
/// `today`.
fn is_valid_relative_format(value: &str) -> bool {
    matches!(value, "today" | "tomorrow" | "yesterday") || validate_relative_pattern(value)
}

/// Port of `validateDateFormat` (integration_action.go:980).
///
/// **A valid datetime is an error here, not a pass** — Go returns a "warning" phrased as an
/// error, carrying the truncated date back to the caller, and `IsValid` accumulates it like any
/// other failure. So `default: "2023-01-02T15:04:05Z"` makes a date element invalid.
fn validate_date_format(date_str: &str) -> Option<String> {
    if date_str.is_empty() || is_valid_relative_format(date_str) {
        return None;
    }

    if parse_go_iso_date(date_str).is_some() {
        return None;
    }

    if let Some((year, month, day)) = parse_go_iso_date_time(date_str) {
        return Some(format!(
            "date field received datetime format {}, only date portion {} will be used. Consider using date format instead",
            go_quote(date_str),
            go_quote(&format!("{year:04}-{month:02}-{day:02}"))
        ));
    }

    Some(format!(
        "invalid date format: {}, expected ISO format (YYYY-MM-DD), datetime format, or relative format",
        go_quote(date_str)
    ))
}

/// Port of `validateDateTimeFormat` (integration_action.go:1003). A plain ISO **date** is not a
/// valid datetime.
fn validate_date_time_format(date_time_str: &str) -> Option<String> {
    if date_time_str.is_empty() || is_valid_relative_format(date_time_str) {
        return None;
    }

    if parse_go_iso_date_time(date_time_str).is_some() {
        return None;
    }

    Some(format!(
        "invalid datetime format: {}, expected ISO format (YYYY-MM-DDTHH:MM:SSZ) or relative format",
        go_quote(date_time_str)
    ))
}

/// Port of `validateDateOrDateTimeFormat` (integration_action.go:1017).
///
/// Note it treats the datetime **warning** as a success: `validateDateFormat` returning the
/// "received datetime format" error means the value parsed, so this function then accepts it via
/// the datetime branch. The union is genuinely "either shape".
fn validate_date_or_date_time_format(value: &str) -> Option<String> {
    if validate_date_format(value).is_none() || validate_date_time_format(value).is_none() {
        return None;
    }

    Some(format!(
        "invalid date or datetime format: {}, expected ISO date (YYYY-MM-DD), datetime (YYYY-MM-DDTHH:MM:SSZ), or relative format",
        go_quote(value)
    ))
}

/// `time.Parse("2006-01-02", value)`, reproduced rather than delegated.
///
/// chrono's `%Y-%m-%d` is not substitutable: it accepts non-padded components. Go's layout
/// requires exactly four digits of year and exactly two each of month and day, rejects leading
/// or trailing text, and then range-checks the calendar date — so `2023-02-29` fails and
/// `2024-02-29` passes.
///
/// `pub(crate)` because `search_params.go` calls `time.Parse` with the same layout six times over
/// and re-transcribing the scanner there would be a second definition to keep in step — the same
/// reasoning that closed the [D-005] borrows.
pub(crate) fn parse_go_iso_date(value: &str) -> Option<(i32, u32, u32)> {
    let mut scan = GoScanner::new(value);
    let date = scan.date()?;
    scan.end()?;
    Some(date)
}

/// `time.Parse` against the four datetime layouts `validateDateTimeFormat` tries, in Go's order.
/// Returns the **wall-clock** date, which is what the warning message reports — an offset is
/// parsed and then ignored rather than normalised to UTC.
fn parse_go_iso_date_time(value: &str) -> Option<(i32, u32, u32)> {
    for zone in [GoZone::LiteralZ, GoZone::Numeric, GoZone::None] {
        let mut scan = GoScanner::new(value);
        if let Some(date) = scan.date_time(true, zone)
            && scan.end().is_some()
        {
            return Some(date);
        }
    }

    let mut scan = GoScanner::new(value);
    let date = scan.date_time(false, GoZone::None)?;
    scan.end()?;
    Some(date)
}

#[derive(Clone, Copy)]
enum GoZone {
    /// The layout ends in a literal `Z` (`2006-01-02T15:04:05Z`). Go does not read this as the
    /// ISO-8601 zone chunk, because that spelling requires `Z07:00`.
    LiteralZ,
    /// `-07:00`: either a literal `Z` **or** a signed `hh:mm` offset. Go's numeric-zone chunk
    /// accepts both, which is why the two layouts overlap.
    Numeric,
    None,
}

/// A byte scanner reproducing the pieces of `time.Parse` these five layouts need.
struct GoScanner<'a> {
    rest: &'a [u8],
}

impl<'a> GoScanner<'a> {
    fn new(value: &'a str) -> Self {
        Self {
            rest: value.as_bytes(),
        }
    }

    fn literal(&mut self, byte: u8) -> Option<()> {
        if self.rest.first() != Some(&byte) {
            return None;
        }
        self.rest = &self.rest[1..];
        Some(())
    }

    /// Go's `getnum(s, true)` — exactly `n` digits.
    fn fixed(&mut self, n: usize) -> Option<u32> {
        if self.rest.len() < n || !self.rest[..n].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let value = std::str::from_utf8(&self.rest[..n]).ok()?.parse().ok()?;
        self.rest = &self.rest[n..];
        Some(value)
    }

    /// Go's `getnum(s, false)` — one digit, or two when the second is also a digit. This is why
    /// `2023-01-02T5:04:05Z` parses while `2023-01-02T15:4:05Z` does not: only the hour is
    /// flexible.
    fn flexible(&mut self) -> Option<u32> {
        let take = match self.rest {
            [a, b, ..] if a.is_ascii_digit() && b.is_ascii_digit() => 2,
            [a, ..] if a.is_ascii_digit() => 1,
            _ => return None,
        };
        let value = std::str::from_utf8(&self.rest[..take]).ok()?.parse().ok()?;
        self.rest = &self.rest[take..];
        Some(value)
    }

    fn date(&mut self) -> Option<(i32, u32, u32)> {
        let year = self.fixed(4)? as i32;
        self.literal(b'-')?;
        let month = self.fixed(2)?;
        self.literal(b'-')?;
        let day = self.fixed(2)?;

        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        Some((year, month, day))
    }

    fn date_time(&mut self, seconds: bool, zone: GoZone) -> Option<(i32, u32, u32)> {
        let date = self.date()?;
        self.literal(b'T')?;

        let hour = self.flexible()?;
        self.literal(b':')?;
        let minute = self.fixed(2)?;
        if hour > 23 || minute > 59 {
            return None;
        }

        if seconds {
            self.literal(b':')?;
            let second = self.fixed(2)?;
            if second > 59 {
                return None;
            }
            self.fraction();
        }

        match zone {
            GoZone::LiteralZ => self.literal(b'Z')?,
            GoZone::Numeric => self.numeric_zone()?,
            GoZone::None => {}
        }

        Some(date)
    }

    /// Go accepts a fractional second the layout never mentions, as long as it directly follows
    /// the seconds field and has at least one digit. **A comma is accepted as well as a period.**
    fn fraction(&mut self) {
        let Some((separator, rest)) = self.rest.split_first() else {
            return;
        };
        if *separator != b'.' && *separator != b',' {
            return;
        }
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return;
        }
        self.rest = &rest[digits..];
    }

    /// Go's `-07:00` chunk accepts a literal `Z` for UTC as well as a signed offset, so the
    /// `…05Z` and `…05-07:00` layouts overlap on `Z`.
    fn numeric_zone(&mut self) -> Option<()> {
        if self.rest.first() == Some(&b'Z') {
            self.rest = &self.rest[1..];
            return Some(());
        }

        let sign = *self.rest.first()?;
        if sign != b'+' && sign != b'-' {
            return None;
        }
        self.rest = &self.rest[1..];

        let hours = self.fixed(2)?;
        self.literal(b':')?;
        let minutes = self.fixed(2)?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        Some(())
    }

    /// `time.Parse` fails when anything is left over.
    fn end(&self) -> Option<()> {
        self.rest.is_empty().then_some(())
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button() -> PostAction {
        PostAction {
            name: "Go".into(),
            action_type: POST_ACTION_TYPE_BUTTON.into(),
            integration: Some(PostActionIntegration {
                url: "https://example.com/hook".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_zero_action_serialises_as_an_empty_object() {
        assert_eq!(serde_json::to_string(&PostAction::default()).unwrap(), "{}");
    }

    #[test]
    fn a_valid_button_passes() {
        assert!(button().is_valid().is_ok());
    }

    #[test]
    fn an_empty_integration_url_produces_two_messages() {
        let mut a = button();
        a.integration = Some(PostActionIntegration::default());
        let err = a.is_valid().unwrap_err();
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn a_three_digit_hex_style_is_rejected_here_and_accepted_by_channel() {
        let mut a = button();
        a.style = "#abc".into();
        assert!(a.is_valid().is_err());
        a.style = "#aabbcc".into();
        assert!(a.is_valid().is_ok());
    }

    #[test]
    fn equals_ignores_tooltip_disabled_and_style() {
        let a = button();
        let mut b = button();
        b.tooltip = "different".into();
        b.disabled = true;
        b.style = "danger".into();
        assert!(a.equals(&b));
    }

    #[test]
    fn options_is_valid_reports_both_fields() {
        let err = PostActionOptions::default().is_valid().unwrap_err();
        assert_eq!(err.messages(), ["text is required", "value is required"]);
    }
}

/// Asserted against `fixtures/behaviour_integration_action.json`, produced by
/// `reference/dump/behaviour_integration_action.go`.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_integration_action.json"
        ))
        .unwrap()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    fn messages(v: &Value, key: &str) -> Vec<String> {
        serde_json::from_value(v.get(key).unwrap().clone()).unwrap()
    }

    #[test]
    fn the_constants_match_go() {
        let o = oracle();
        let c = o.get("constants").unwrap();

        for (key, ours) in [
            ("post_action_type_button", POST_ACTION_TYPE_BUTTON),
            ("post_action_type_select", POST_ACTION_TYPE_SELECT),
            (
                "post_action_data_source_users",
                POST_ACTION_DATA_SOURCE_USERS,
            ),
            (
                "post_action_data_source_chans",
                POST_ACTION_DATA_SOURCE_CHANNELS,
            ),
            (
                "format_attachment",
                POST_ACTION_INTEGRATION_FORMAT_ATTACHMENT,
            ),
            (
                "format_apps_binding",
                POST_ACTION_INTEGRATION_FORMAT_APPS_BINDING,
            ),
            ("format_block", POST_ACTION_INTEGRATION_FORMAT_BLOCK),
            ("format_card", POST_ACTION_INTEGRATION_FORMAT_CARD),
            ("format_mm_block", POST_ACTION_INTEGRATION_FORMAT_MM_BLOCK),
            ("mm_blocks_action_cookie_kind", MM_BLOCKS_ACTION_COOKIE_KIND),
        ] {
            assert_eq!(s(c, key), ours, "constant {key}");
        }

        for (key, ours) in [
            (
                "max_mm_blocks_actions_per_post",
                MAX_MM_BLOCKS_ACTIONS_PER_POST,
            ),
            (
                "max_mm_blocks_action_key_len",
                MAX_MM_BLOCKS_ACTION_KEY_LENGTH,
            ),
            ("max_action_query_entries", MAX_ACTION_QUERY_ENTRIES),
            ("max_action_query_key_length", MAX_ACTION_QUERY_KEY_LENGTH),
            (
                "max_action_query_value_length",
                MAX_ACTION_QUERY_VALUE_LENGTH,
            ),
        ] {
            assert_eq!(
                c.get(key).unwrap().as_u64().unwrap() as usize,
                ours,
                "constant {key}"
            );
        }

        let want: Vec<String> = messages(c, "post_action_retain_prop_keys");
        assert_eq!(POST_ACTION_RETAIN_PROP_KEYS.to_vec(), want);
    }

    /// `MultiError`'s `Display` reproduces `multierror.ListFormatFunc` — including the
    /// singular/plural split and the trailing blank line — and `prefixed` reproduces
    /// `multierror.Prefix`, which flattens rather than nests.
    #[test]
    fn the_multierror_layout_matches_go() {
        let o = oracle();
        for case in o.get("multierror_format").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let want = s(case, "error");

            let got = if name == "prefixed" {
                let mut inner = MultiError::new();
                inner.push("text is required");
                inner.push("value is required");
                let prefixed = inner.prefixed("option at index 0 is invalid:");
                assert_eq!(prefixed.messages(), messages(case, "messages").as_slice());
                prefixed
            } else {
                let mut m = MultiError::new();
                for input in messages(case, "inputs") {
                    m.push(input);
                }
                m
            };

            assert_eq!(
                got.len(),
                case.get("count").unwrap().as_u64().unwrap() as usize
            );
            assert_eq!(got.to_string(), want, "{name}");
        }
    }

    /// 41 cases. Each embeds the action as Go-marshalled JSON, so a wire drift and a logic
    /// drift fail the same test, and asserts the **full ordered message list** rather than
    /// just the first failure.
    #[test]
    fn post_action_is_valid_matches_go() {
        let o = oracle();
        for case in o.get("post_action_is_valid").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let want = messages(case, "messages");

            // Go's `Options` is `[]*PostActionOptions` and can hold a nil element; ours cannot,
            // so those two cases fail at decode time instead. Standing [D-033] convention,
            // asserted rather than skipped.
            if want
                .iter()
                .any(|m| m == "select action contains nil option")
            {
                let decoded: Result<PostAction, _> =
                    serde_json::from_value(case.get("action").unwrap().clone());
                assert!(
                    decoded.is_err(),
                    "{name}: we should fail to decode a nil option"
                );
                continue;
            }

            let action: PostAction =
                serde_json::from_value(case.get("action").unwrap().clone()).unwrap();
            let got = match action.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => e.messages().to_vec(),
            };
            assert_eq!(got, want, "{name}");

            if !want.is_empty() {
                let err = action.is_valid().unwrap_err();
                assert_eq!(err.to_string(), s(case, "error"), "{name}: formatted");
            }
        }
    }

    #[test]
    fn post_action_options_is_valid_matches_go() {
        let o = oracle();
        for case in o
            .get("post_action_options_valid")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let name = s(case, "name");
            let opts = match name.as_str() {
                "both_set" => PostActionOptions {
                    text: "t".into(),
                    value: "v".into(),
                },
                "no_text" => PostActionOptions {
                    text: String::new(),
                    value: "v".into(),
                },
                "no_value" => PostActionOptions {
                    text: "t".into(),
                    value: String::new(),
                },
                "whitespace_text_counts_as_set" => PostActionOptions {
                    text: " ".into(),
                    value: "v".into(),
                },
                _ => PostActionOptions::default(),
            };
            let got = match opts.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => e.messages().to_vec(),
            };
            assert_eq!(got, messages(case, "messages"), "{name}");
        }
    }

    /// 22 cases, including the three fields `Equals` silently ignores.
    #[test]
    fn post_action_equals_matches_go() {
        let o = oracle();
        for case in o.get("post_action_equals").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let a: PostAction = serde_json::from_value(case.get("a").unwrap().clone()).unwrap();
            let b_action: PostAction =
                serde_json::from_value(case.get("b").unwrap().clone()).unwrap();
            assert_eq!(a.equals(&b_action), b(case, "equals"), "{name}");
        }
    }

    /// Go panics on a nil option element; our `Vec<PostActionOptions>` cannot hold one, so the
    /// crash is unreachable. Recorded so the divergence stays visible — [D-038].
    #[test]
    fn equals_panics_in_go_on_a_nil_option() {
        let o = oracle();
        let p = o.get("post_action_equals_panics").unwrap();
        assert!(b(p, "nil_option_on_receiver"));
        assert!(b(p, "nil_option_on_input"));
        assert!(b(p, "nil_option_on_both"));
        assert!(!b(p, "no_options"));

        // Ours simply compares empty option lists.
        assert!(PostAction::default().equals(&PostAction::default()));
    }

    #[test]
    fn normalize_format_matches_go() {
        let o = oracle();
        for (input, want) in o.get("normalize_format").unwrap().as_object().unwrap() {
            assert_eq!(
                normalize_post_action_integration_format(input),
                want.as_str().unwrap(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn post_action_preserve_state_matches_go() {
        let o = oracle();
        for case in o.get("preserve_state").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let props: Option<StringInterface> =
                serde_json::from_value(case.get("props").unwrap().clone()).unwrap();
            let post = Post {
                id: s(case, "post_id"),
                root_id: s(case, "root_id"),
                props,
                is_pinned: b(case, "original_is_pinned"),
                has_reactions: b(case, "original_has_reactions"),
                ..Default::default()
            };
            let st = post.post_action_preserve_state();

            assert_eq!(
                serde_json::to_value(&st.retain).unwrap(),
                *case.get("retain").unwrap(),
                "{name}: retain"
            );
            assert_eq!(st.remove, messages(case, "remove"), "{name}: remove");
            assert_eq!(
                serde_json::to_value(&st.original_props).unwrap(),
                *case.get("original_props").unwrap(),
                "{name}: original props"
            );
            assert_eq!(
                st.root_post_id,
                s(case, "root_post_id"),
                "{name}: root post id"
            );
        }
    }

    /// Byte-exact through `go_json_marshal`, over all 21 types in this chunk.
    #[test]
    fn the_wire_format_matches_go() {
        let o = oracle();
        for case in o.get("wire").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let want = s(case, "json");

            // Each case names its type; decode into it and re-marshal.
            let got = match name.as_str() {
                n if n.starts_with("post_action_options") => {
                    let v: PostActionOptions = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("post_action_integration") => {
                    let v: PostActionIntegration = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("post_action_cookie") => {
                    let v: PostActionCookie = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("post_action") => {
                    let v: PostAction = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("do_post_action_request") => {
                    let v: DoPostActionRequest = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("mm_blocks_cookie") => {
                    let v: MmBlocksActionCookie = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("integration_request") => {
                    let v: PostActionIntegrationRequest = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("integration_response") => {
                    let v: PostActionIntegrationResponse = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("api_response") => {
                    let v: PostActionAPIResponse = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("execute_dialog_response") => {
                    let v: ExecuteDialogActionResponse = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                other => panic!("unhandled wire case {other}"),
            };
            assert_eq!(got, want, "{name}");
        }
    }
}

/// Parity for the Dialog family against `fixtures/behaviour_dialog.json`.
#[cfg(test)]
mod dialog_go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_dialog.json")).unwrap()
    }

    fn cases(section: &str) -> Vec<Value> {
        oracle().get(section).unwrap().as_array().unwrap().to_vec()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn messages(v: &Value, key: &str) -> Vec<String> {
        v.get(key)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect()
    }

    fn ours(result: Result<(), MultiError>) -> Vec<String> {
        match result {
            Ok(()) => Vec::new(),
            Err(e) => e.messages().to_vec(),
        }
    }

    #[test]
    fn the_constants_match_go() {
        let o = oracle();
        let c = o.get("constants").unwrap();

        for (key, ours) in [
            ("dialog_title_max_length", DIALOG_TITLE_MAX_LENGTH),
            (
                "dialog_element_display_name_max_length",
                DIALOG_ELEMENT_DISPLAY_NAME_MAX_LENGTH,
            ),
            (
                "dialog_element_name_max_length",
                DIALOG_ELEMENT_NAME_MAX_LENGTH,
            ),
            (
                "dialog_element_help_text_max_length",
                DIALOG_ELEMENT_HELP_TEXT_MAX_LENGTH,
            ),
            (
                "dialog_element_text_max_length",
                DIALOG_ELEMENT_TEXT_MAX_LENGTH,
            ),
            (
                "dialog_element_textarea_max_length",
                DIALOG_ELEMENT_TEXTAREA_MAX_LENGTH,
            ),
            (
                "dialog_element_select_max_length",
                DIALOG_ELEMENT_SELECT_MAX_LENGTH,
            ),
            (
                "dialog_element_bool_max_length",
                DIALOG_ELEMENT_BOOL_MAX_LENGTH,
            ),
            (
                "dialog_element_file_max_length",
                DIALOG_ELEMENT_FILE_MAX_LENGTH,
            ),
            ("max_dialog_file_ids", MAX_DIALOG_FILE_IDS),
            (
                "max_dialog_submission_id_shaped_token_scan",
                MAX_DIALOG_SUBMISSION_ID_SHAPED_TOKEN_SCAN,
            ),
        ] {
            assert_eq!(
                c.get(key).unwrap().as_u64().unwrap() as usize,
                ours,
                "{key}"
            );
        }

        assert_eq!(
            c.get("default_time_interval_minutes")
                .unwrap()
                .as_i64()
                .unwrap(),
            DEFAULT_TIME_INTERVAL_MINUTES
        );

        for (key, ours) in [
            ("iso_date_format", ISO_DATE_FORMAT),
            ("iso_date_time_format", ISO_DATE_TIME_FORMAT),
            (
                "iso_date_time_with_timezone_format",
                ISO_DATE_TIME_WITH_TIMEZONE_FORMAT,
            ),
            (
                "iso_date_time_no_timezone_format",
                ISO_DATE_TIME_NO_TIMEZONE_FORMAT,
            ),
            (
                "iso_date_time_no_seconds_format",
                ISO_DATE_TIME_NO_SECONDS_FORMAT,
            ),
            (
                "submit_dialog_response_type_empty",
                SUBMIT_DIALOG_RESPONSE_TYPE_EMPTY,
            ),
            (
                "submit_dialog_response_type_ok",
                SUBMIT_DIALOG_RESPONSE_TYPE_OK,
            ),
            (
                "submit_dialog_response_type_form",
                SUBMIT_DIALOG_RESPONSE_TYPE_FORM,
            ),
            (
                "submit_dialog_response_type_navigate",
                SUBMIT_DIALOG_RESPONSE_TYPE_NAVIGATE,
            ),
        ] {
            assert_eq!(c.get(key).unwrap().as_str().unwrap(), ours, "{key}");
        }
    }

    /// Every wire probe, asserted **byte-for-byte** through Go's own escaping rules rather than
    /// as an equal `Value` graph.
    #[test]
    fn the_wire_format_matches_go() {
        for case in cases("wire") {
            let name = s(&case, "name");
            let want = s(&case, "json");

            let got = match name.as_str() {
                n if n.starts_with("dialog_select_option") => {
                    let v: DialogSelectOption = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("dialog") => {
                    let v: Dialog = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("element") => {
                    let v: DialogElement = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("datetime_config") => {
                    let v: DialogDateTimeConfig = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("action_button") => {
                    let v: DialogActionButton = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("open_dialog_request") => {
                    let v: OpenDialogRequest = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("submit_request") => {
                    let v: SubmitDialogRequest = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("submit_response") => {
                    let v: SubmitDialogResponse = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("execute_dialog_action_request") => {
                    let v: ExecuteDialogActionRequest = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                n if n.starts_with("lookup_dialog_response") => {
                    let v: LookupDialogResponse = serde_json::from_str(&want).unwrap();
                    crate::utils::go_json_marshal(&v).unwrap()
                }
                other => panic!("unhandled wire case {other}"),
            };

            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn is_valid_lookup_url_matches_go() {
        for case in cases("lookup_url") {
            let url = s(&case, "url");
            let want = case.get("valid").unwrap().as_bool().unwrap();
            assert_eq!(is_valid_lookup_url(&url), want, "{url:?}");
        }
    }

    #[test]
    fn effective_date_time_config_matches_go() {
        for case in cases("effective_datetime_config") {
            let name = s(&case, "name");
            let element: DialogElement =
                serde_json::from_value(case.get("element").unwrap().clone()).unwrap();
            let want = case.get("config").unwrap().clone();
            let got = serde_json::to_value(element.effective_date_time_config()).unwrap();
            assert_eq!(got, want, "{name}");
        }
    }

    /// The corpus cases carrying a `null` option element, which `Vec<PostActionOptions>` cannot
    /// decode — the standing [D-033] convention.
    const NIL_OPTION_CASES: [&str; 1] = ["select_nil_option_element"];

    #[test]
    fn element_is_valid_matches_go() {
        for case in cases("element_is_valid") {
            let name = s(&case, "name");
            let input = case.get("input").unwrap().clone();

            if NIL_OPTION_CASES.contains(&name.as_str()) {
                assert!(
                    serde_json::from_value::<DialogElement>(input).is_err(),
                    "{name}: expected the nil option to fail at decode time"
                );
                continue;
            }

            let element: DialogElement = serde_json::from_value(input).unwrap();
            assert_eq!(
                ours(element.is_valid()),
                messages(&case, "messages"),
                "{name}"
            );
        }
    }

    /// [D-033]: Go tolerates a nil option element and validates the rest; we reject the whole
    /// document. Asserted rather than skipped.
    #[test]
    fn a_nil_option_element_is_valid_for_go_and_undecodable_for_us() {
        let case = cases("element_is_valid")
            .into_iter()
            .find(|c| s(c, "name") == "select_nil_option_element")
            .unwrap();

        assert!(messages(&case, "messages").is_empty(), "Go accepted it");
        assert!(
            serde_json::from_value::<DialogElement>(case.get("input").unwrap().clone()).is_err()
        );
    }

    #[test]
    fn the_time_interval_rule_matches_go() {
        for case in cases("time_interval") {
            let interval = case.get("interval").unwrap().as_i64().unwrap();
            let element = DialogElement {
                display_name: "Display".into(),
                name: "name".into(),
                element_type: "datetime".into(),
                time_interval: interval,
                ..Default::default()
            };
            assert_eq!(
                ours(element.is_valid()),
                messages(&case, "messages"),
                "interval {interval}"
            );
        }
    }

    /// One corpus of date strings through the three validators that differ, by way of the three
    /// element shapes that reach them. This is where `time.Parse` is pinned.
    #[test]
    fn the_date_validators_match_go() {
        let base = |element_type: &str| DialogElement {
            display_name: "Display".into(),
            name: "name".into(),
            element_type: element_type.into(),
            ..Default::default()
        };

        for case in cases("date_formats") {
            let input = s(&case, "input");

            let mut date = base("date");
            date.default.clone_from(&input);
            assert_eq!(
                ours(date.is_valid()),
                messages(&case, "as_date_default"),
                "{input:?} as a date default"
            );

            let mut datetime = base("datetime");
            datetime.default.clone_from(&input);
            assert_eq!(
                ours(datetime.is_valid()),
                messages(&case, "as_datetime"),
                "{input:?} as a datetime default"
            );

            let mut min_date = base("datetime");
            min_date.date_time_config = Some(DialogDateTimeConfig {
                min_date: input.clone(),
                ..Default::default()
            });
            assert_eq!(
                ours(min_date.is_valid()),
                messages(&case, "as_min_date"),
                "{input:?} as a min date"
            );
        }
    }

    /// A datetime in a date field is a **failure** carrying the truncated date, not a pass. The
    /// truncation is the wall clock, so an offset does not move the date.
    #[test]
    fn a_datetime_in_a_date_field_is_an_error_with_the_truncated_date() {
        let case = cases("date_formats")
            .into_iter()
            .find(|c| s(c, "input") == "2023-01-02T15:04:05-07:00")
            .unwrap();

        let want = messages(&case, "as_date_default");
        assert_eq!(want.len(), 1);
        assert!(
            want[0].contains(r#"only date portion "2023-01-02" will be used"#),
            "{want:?}"
        );
        // …and the same value passes as a datetime.
        assert!(messages(&case, "as_datetime").is_empty());
    }

    #[test]
    fn dialog_is_valid_matches_go() {
        for case in cases("dialog_is_valid") {
            let name = s(&case, "name");
            let dialog: Dialog =
                serde_json::from_value(case.get("input").unwrap().clone()).unwrap();
            assert_eq!(
                ours(dialog.is_valid()),
                messages(&case, "messages"),
                "{name}"
            );
        }
    }

    /// Go nests an element's whole multierror inside one parent message here and splices them
    /// flat in `OpenDialogRequest::is_valid`. Both compositions are asserted on the same shape.
    #[test]
    fn element_failures_nest_in_a_dialog_and_flatten_in_a_request() {
        let nested = cases("dialog_is_valid")
            .into_iter()
            .find(|c| s(c, "name") == "element_with_two_failures")
            .unwrap();
        let want = messages(&nested, "messages");
        assert_eq!(want.len(), 1, "one parent message");
        assert!(want[0].contains("3 errors occurred:"), "{want:?}");

        let dialog: Dialog = serde_json::from_value(nested.get("input").unwrap().clone()).unwrap();
        assert_eq!(ours(dialog.is_valid()), want);

        let flat = cases("open_dialog_request_is_valid")
            .into_iter()
            .find(|c| s(c, "name") == "invalid_dialog")
            .unwrap();
        assert_eq!(messages(&flat, "messages").len(), 2, "spliced in flat");
    }

    #[test]
    fn open_dialog_request_is_valid_matches_go() {
        for case in cases("open_dialog_request_is_valid") {
            let name = s(&case, "name");
            let request: OpenDialogRequest =
                serde_json::from_value(case.get("input").unwrap().clone()).unwrap();
            assert_eq!(
                ours(request.is_valid()),
                messages(&case, "messages"),
                "{name}"
            );
        }
    }

    #[test]
    fn submit_dialog_response_is_valid_matches_go() {
        for case in cases("submit_response_is_valid") {
            let name = s(&case, "name");
            let response: SubmitDialogResponse =
                serde_json::from_value(case.get("input").unwrap().clone()).unwrap();

            let got = match response.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => vec![e.to_string()],
            };
            assert_eq!(got, messages(&case, "messages"), "{name}");
            assert_eq!(
                got.join(""),
                s(&case, "error"),
                "{name}: the rendered error"
            );
        }
    }
}

/// Oracle-driven tests for chunk 3 — the three `Post` methods that walk `props.attachments`.
///
/// Every case asserts against the **whole marshalled post**, not against the attachments alone,
/// so a props-rewrite drift and a wire drift fail the same test.
///
/// One divergence runs through the lot and is pinned by
/// [`post_actions_go_parity::the_rewritten_attachments_differ_from_go_only_in_key_order`]:
/// Go stores a native `[]*MessageAttachment`, whose fields marshal in **declaration** order,
/// while we store a `serde_json::Value` whose object keys are **sorted**. See [D-048].
#[cfg(test)]
mod post_actions_go_parity {
    use super::*;
    use crate::utils::{go_json_marshal, is_valid_id};
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_actions.json"
        ))
        .unwrap()
    }

    fn section(o: &Value, key: &str) -> Vec<Value> {
        o.get(key).unwrap().as_array().unwrap().clone()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_str(&s(v, key)).unwrap()
    }

    /// Compares our marshalled post against Go's. Byte-for-byte whenever the answer carries no
    /// rewritten attachment list, and by value graph when it does — the [D-048] key-order
    /// divergence is asserted on its own rather than being papered over here.
    fn assert_matches_go(name: &str, ours: &Post, want: &str) {
        let got = go_json_marshal(ours).unwrap();
        if want.contains("\"attachments\":[") {
            let got: Value = serde_json::from_str(&got).unwrap();
            let want: Value = serde_json::from_str(want).unwrap();
            assert_eq!(got, want, "{name}");
        } else {
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn strip_action_integrations_matches_go() {
        for case in section(&oracle(), "strip_action_integrations") {
            let mut post = post_from(&case, "post");
            post.strip_action_integrations();
            assert_matches_go(&s(&case, "name"), &post, &s(&case, "out"));
        }
    }

    #[test]
    fn strip_mm_blocks_action_secrets_matches_go() {
        for case in section(&oracle(), "strip_mm_blocks_action_secrets") {
            let mut post = post_from(&case, "post");
            post.strip_mm_blocks_action_secrets();
            assert_matches_go(&s(&case, "name"), &post, &s(&case, "out"));
        }
    }

    /// The ids `GenerateActionIds` mints come from `NewId()`, so the fixture records the output
    /// with every id absent from the input replaced by `<generated>`. This applies the same
    /// substitution to ours, and separately asserts each minted id really is a valid new id —
    /// which is the half the placeholder throws away.
    fn blank_generated_ids(post: &mut Post, known: &[String]) -> usize {
        let Some(Value::Array(attachments)) = post.get_prop(POST_PROPS_ATTACHMENTS).cloned() else {
            return 0;
        };

        let mut count = 0;
        let mut out = Vec::with_capacity(attachments.len());
        for mut attachment in attachments {
            if let Some(Value::Array(actions)) = attachment.get_mut("actions") {
                for action in actions.iter_mut() {
                    let Some(id) = action.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if id.is_empty() || known.iter().any(|k| k == id) {
                        continue;
                    }
                    assert!(is_valid_id(id), "minted id {id:?} is not a valid id");
                    action["id"] = Value::String("<generated>".into());
                    count += 1;
                }
            }
            out.push(attachment);
        }

        post.add_prop(POST_PROPS_ATTACHMENTS, Value::Array(out));
        count
    }

    fn known_action_ids(post: &Post) -> Vec<String> {
        post.attachments()
            .iter()
            .flat_map(|a| a.actions.iter())
            .filter(|action| !action.id.is_empty())
            .map(|action| action.id.clone())
            .collect()
    }

    fn assert_id_minting_matches_go(section_key: &str, run: fn(&mut Post)) {
        for case in section(&oracle(), section_key) {
            let name = s(&case, "name");
            let mut post = post_from(&case, "post");
            let known = known_action_ids(&post);

            run(&mut post);

            let count = blank_generated_ids(&mut post, &known);
            assert_eq!(
                count,
                case.get("generated_count").unwrap().as_u64().unwrap() as usize,
                "{name}: number of minted ids"
            );
            assert_matches_go(&name, &post, &s(&case, "out"));
        }
    }

    #[test]
    fn generate_action_ids_matches_go() {
        assert_id_minting_matches_go("generate_action_ids", |post| post.generate_action_ids());
    }

    /// `PreCommit` is where `GenerateActionIds` is actually reached, and running the same corpus
    /// through it is what closes [D-035].
    #[test]
    fn pre_commit_matches_go() {
        assert_id_minting_matches_go("pre_commit", |post| post.pre_commit());
    }

    /// An empty attachment list is stored as `null`, not `[]` — Go's `Attachments()` returns a
    /// *nil* slice when nothing decodes, and a nil Go slice marshals as `null`. Four inputs
    /// reach it, and each would otherwise have produced `"attachments":[]` on the wire.
    #[test]
    fn an_empty_attachment_list_is_stored_as_null() {
        for post in [
            r#"{"props":{"attachments":[]}}"#,
            r#"{"props":{"attachments":"nope"}}"#,
            r#"{"props":{"attachments":{"a":1}}}"#,
            r#"{"props":{"attachments":7}}"#,
        ] {
            let mut post: Post = serde_json::from_str(post).unwrap();
            post.strip_action_integrations();
            assert_eq!(
                post.props.as_ref().unwrap().get(POST_PROPS_ATTACHMENTS),
                Some(&Value::Null)
            );
        }
    }

    /// [D-048]. Go marshals the stored `[]*MessageAttachment` as a **struct**, so its keys come
    /// out in declaration order; we store a `serde_json::Value`, whose object keys are sorted.
    /// The two documents are equal and the bytes are not. Asserted rather than skipped so the
    /// divergence cannot rot, and so the day it is closed this test says so.
    #[test]
    fn the_rewritten_attachments_differ_from_go_only_in_key_order() {
        let case = section(&oracle(), "strip_action_integrations")
            .into_iter()
            .find(|c| s(c, "name") == "one_action_with_integration")
            .unwrap();

        let mut post = post_from(&case, "post");
        post.strip_action_integrations();
        let ours = go_json_marshal(&post).unwrap();
        let theirs = s(&case, "out");

        assert_ne!(ours, theirs, "the byte order divergence has closed");
        assert!(theirs.contains(r#""attachments":[{"id":0,"fallback":""#));
        assert!(ours.contains(r#""attachments":[{"actions":["#));
        assert_eq!(
            serde_json::from_str::<Value>(&ours).unwrap(),
            serde_json::from_str::<Value>(&theirs).unwrap(),
        );
    }
}
