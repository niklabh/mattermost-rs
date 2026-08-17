//! Port of `server/public/model/product_notices.go`.
//!
//! Four `Matches` methods, four small state machines, and **they disagree about what an unknown
//! value means** — which is the content of this file:
//!
//! | method | unknown value |
//! |---|---|
//! | [`NoticeAudience::matches`] | `false` — a switch with no default, falling to `return false` |
//! | [`NoticeInstanceType::matches`] | **`true`** — three `if`s, then `return true` |
//! | [`NoticeClientType::matches`] | exact equality only |
//! | [`NoticeSku::matches`] | exact equality only |
//!
//! So an audience nobody recognises **hides** a notice and an instance type nobody recognises
//! **shows** it. Collapsing the four into one shape with a uniform fallback would silently change
//! who sees a notice, in opposite directions depending on the field. Every value, known and
//! unknown, is driven through each against Go.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

use crate::utils::{StringArray, StringInterface};

// --- the four defined string types --------------------------------------------------------------
//
// Go declares each as `type X string`, which accepts any string — these are not closed enums, and
// modelling them as Rust enums would reject values Go stores happily. Newtypes keep the methods
// discoverable while staying open.

macro_rules! notice_string_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

notice_string_type! {
    /// Port of `model.NoticeAudience` (product_notices.go:88) — "user role, i.e. who will see the
    /// notice. Defaults to 'all'".
    NoticeAudience
}
notice_string_type! {
    /// Port of `model.NoticeClientType` (product_notices.go:117).
    NoticeClientType
}
notice_string_type! {
    /// Port of `model.NoticeInstanceType` (product_notices.go:155).
    NoticeInstanceType
}
notice_string_type! {
    /// Port of `model.NoticeSKU` (product_notices.go:178).
    NoticeSku
}
notice_string_type! {
    /// Port of `model.NoticeAction` (product_notices.go:203).
    NoticeAction
}

/// product_notices.go:108
pub const NOTICE_AUDIENCE_ALL: &str = "all";
/// product_notices.go:109
pub const NOTICE_AUDIENCE_MEMBER: &str = "member";
/// product_notices.go:110
pub const NOTICE_AUDIENCE_SYSADMIN: &str = "sysadmin";
/// product_notices.go:111
pub const NOTICE_AUDIENCE_TEAM_ADMIN: &str = "teamadmin";

/// product_notices.go:133
pub const NOTICE_CLIENT_TYPE_ALL: &str = "all";
/// product_notices.go:134
pub const NOTICE_CLIENT_TYPE_DESKTOP: &str = "desktop";
/// product_notices.go:135
pub const NOTICE_CLIENT_TYPE_MOBILE: &str = "mobile";
/// product_notices.go:136
pub const NOTICE_CLIENT_TYPE_MOBILE_ANDROID: &str = "mobile-android";
/// product_notices.go:137
pub const NOTICE_CLIENT_TYPE_MOBILE_IOS: &str = "mobile-ios";
/// product_notices.go:138
pub const NOTICE_CLIENT_TYPE_WEB: &str = "web";

/// product_notices.go:172
pub const NOTICE_INSTANCE_TYPE_BOTH: &str = "both";
/// product_notices.go:173
pub const NOTICE_INSTANCE_TYPE_CLOUD: &str = "cloud";
/// product_notices.go:174
pub const NOTICE_INSTANCE_TYPE_ON_PREM: &str = "onprem";

/// product_notices.go:194
pub const NOTICE_SKU_E0: &str = "e0";
/// product_notices.go:195
pub const NOTICE_SKU_E10: &str = "e10";
/// product_notices.go:196
pub const NOTICE_SKU_E20: &str = "e20";
/// product_notices.go:197
pub const NOTICE_SKU_ALL: &str = "all";
/// product_notices.go:198
pub const NOTICE_SKU_TEAM: &str = "team";

/// product_notices.go:206 — the only `NoticeAction`.
pub const NOTICE_ACTION_URL: &str = "url";

impl NoticeAudience {
    /// Port of `(*NoticeAudience).Matches` (product_notices.go:95).
    ///
    /// A `switch` over the four known values with **no default**, falling through to
    /// `return false` — so an unrecognised audience matches nobody and the notice is hidden.
    /// That is the opposite of [`NoticeInstanceType::matches`], which returns `true` for an
    /// unknown value. Measured both ways.
    pub fn matches(&self, sys_admin: bool, team_admin: bool) -> bool {
        match self.as_str() {
            NOTICE_AUDIENCE_ALL => true,
            NOTICE_AUDIENCE_MEMBER => !sys_admin && !team_admin,
            NOTICE_AUDIENCE_SYSADMIN => sys_admin,
            NOTICE_AUDIENCE_TEAM_ADMIN => team_admin,
            _ => false,
        }
    }
}

impl NoticeClientType {
    /// Port of `(*NoticeClientType).Matches` (product_notices.go:124).
    ///
    /// `all` matches anything, `mobile` is an **alias** for the two concrete mobile types, and
    /// everything else — including an unrecognised value — matches only itself. Note the alias is
    /// one-directional: `mobile` matches `mobile-ios`, but `mobile-ios` does **not** match
    /// `mobile`.
    pub fn matches(&self, other: &NoticeClientType) -> bool {
        match self.as_str() {
            NOTICE_CLIENT_TYPE_ALL => true,
            NOTICE_CLIENT_TYPE_MOBILE => {
                other.as_str() == NOTICE_CLIENT_TYPE_MOBILE_IOS
                    || other.as_str() == NOTICE_CLIENT_TYPE_MOBILE_ANDROID
            }
            _ => self == other,
        }
    }
}

impl NoticeInstanceType {
    /// Port of `(*NoticeInstanceType).Matches` (product_notices.go:160).
    ///
    /// Written as three `if`s and a trailing `return true`, so an **unrecognised instance type
    /// matches** — the permissive fallback, and the opposite of [`NoticeAudience::matches`].
    /// Reproduced as Go writes it rather than as a match, so the fallthrough stays visible.
    pub fn matches(&self, is_cloud: bool) -> bool {
        if self.as_str() == NOTICE_INSTANCE_TYPE_BOTH {
            return true;
        }
        if self.as_str() == NOTICE_INSTANCE_TYPE_CLOUD && !is_cloud {
            return false;
        }
        if self.as_str() == NOTICE_INSTANCE_TYPE_ON_PREM && is_cloud {
            return false;
        }
        true
    }
}

impl NoticeSku {
    /// Port of `(*NoticeSKU).Matches` (product_notices.go:185).
    ///
    /// The argument is the server's licence SKU as a plain string, where `""` means *no licence*.
    /// `e0` and `team` both mean "unlicensed", so they match the empty string and nothing else —
    /// `NoticeSKUE0.Matches("e0")` is **false**, which is the trap here.
    pub fn matches(&self, sku: &str) -> bool {
        match self.as_str() {
            NOTICE_SKU_ALL => true,
            NOTICE_SKU_E0 | NOTICE_SKU_TEAM => sku.is_empty(),
            other => sku == other,
        }
    }
}

/// Port of `NoticeClientTypeFromString` (product_notices.go:141).
///
/// # It rejects two of its own constants
///
/// The switch accepts `web`, `mobile-ios`, `mobile-android` and `desktop` — but **not** `mobile`
/// or `all`, both of which are declared `NoticeClientType` constants. Anything else, those two
/// included, is an error.
///
/// On failure Go returns `NoticeClientTypeAll` *alongside* the error, so a caller that ignores the
/// error gets `all` — the permissive value — rather than a zero one. The `Result` here carries the
/// same value in its error arm so that behaviour is available rather than lost.
pub fn notice_client_type_from_string(value: &str) -> Result<NoticeClientType, NoticeClientType> {
    match value {
        NOTICE_CLIENT_TYPE_WEB => Ok(NoticeClientType::new(NOTICE_CLIENT_TYPE_WEB)),
        NOTICE_CLIENT_TYPE_MOBILE_IOS => Ok(NoticeClientType::new(NOTICE_CLIENT_TYPE_MOBILE_IOS)),
        NOTICE_CLIENT_TYPE_MOBILE_ANDROID => {
            Ok(NoticeClientType::new(NOTICE_CLIENT_TYPE_MOBILE_ANDROID))
        }
        NOTICE_CLIENT_TYPE_DESKTOP => Ok(NoticeClientType::new(NOTICE_CLIENT_TYPE_DESKTOP)),
        // Go: `return NoticeClientTypeAll, errors.New("Invalid client type supplied")`.
        _ => Err(NoticeClientType::new(NOTICE_CLIENT_TYPE_ALL)),
    }
}

/// The message Go pairs with the error from [`notice_client_type_from_string`].
pub const INVALID_CLIENT_TYPE_ERROR: &str = "Invalid client type supplied";

// --- the wire types ------------------------------------------------------------------------------

/// Port of `model.Conditions` (product_notices.go:41).
///
/// **Every** field is `omitempty`, so a zero-valued `Conditions` serialises as `{}` — which
/// [`ProductNotice`] still transmits, because its own `conditions` key is not omitempty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conditions {
    #[serde(rename = "audience", default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<NoticeAudience>,

    /// "Only show the notice on specific clients. Defaults to 'all'".
    #[serde(
        rename = "clientType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_type: Option<NoticeClientType>,

    #[serde(
        rename = "desktopVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub desktop_version: Option<StringArray>,

    #[serde(
        rename = "displayDate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_date: Option<String>,

    #[serde(
        rename = "instanceType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub instance_type: Option<NoticeInstanceType>,

    #[serde(
        rename = "mobileVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mobile_version: Option<StringArray>,

    #[serde(
        rename = "numberOfPosts",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub number_of_posts: Option<i64>,

    #[serde(
        rename = "numberOfUsers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub number_of_users: Option<i64>,

    #[serde(
        rename = "serverConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_config: Option<StringInterface>,

    #[serde(
        rename = "serverVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_version: Option<StringArray>,

    #[serde(rename = "sku", default, skip_serializing_if = "Option::is_none")]
    pub sku: Option<NoticeSku>,

    #[serde(
        rename = "userConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user_config: Option<StringInterface>,

    /// The only snake_case key on this struct — everything else is camelCase.
    #[serde(
        rename = "deprecating_dependency",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub deprecating_dependency: Option<ExternalDependency>,
}

/// Port of `model.ProductNotice` (product_notices.go:26).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductNotice {
    #[serde(rename = "conditions")]
    pub conditions: Conditions,

    /// "Unique identifier for this notice. Can be a running number."
    #[serde(rename = "id")]
    pub id: String,

    /// "Notice message data, organized by locale."
    #[serde(rename = "localizedMessages")]
    pub localized_messages: Option<std::collections::BTreeMap<String, NoticeMessageInternal>>,

    /// The only `omitempty` field. A pointer to `false` is not nil, so three states.
    #[serde(
        rename = "repeatable",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub repeatable: Option<bool>,
}

/// Port of `model.NoticeMessageInternal` (product_notices.go:58).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeMessageInternal {
    #[serde(rename = "action", default, skip_serializing_if = "Option::is_none")]
    pub action: Option<NoticeAction>,

    #[serde(
        rename = "actionParam",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub action_param: Option<String>,

    #[serde(
        rename = "actionText",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub action_text: Option<String>,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "image", default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    #[serde(rename = "title")]
    pub title: String,
}

/// Port of `model.NoticeMessage` (product_notices.go:68).
///
/// # The embed emits first, so `Serialize` is hand-written
///
/// `NoticeMessageInternal` is an **anonymous field**, and Go inlines its keys ahead of the
/// struct's own. Measured, Go's order is:
///
/// ```text
/// action actionParam actionText description image title id sysAdminOnly teamAdminOnly
/// ```
///
/// serde's `#[serde(flatten)]` emits flattened keys **last**, which is [D-067] — the same problem
/// `ScheduledPost` has, solved the same way. `Deserialize` still derives with `flatten`, since
/// input order does not matter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NoticeMessage {
    #[serde(flatten)]
    pub internal: NoticeMessageInternal,

    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "sysAdminOnly")]
    pub sys_admin_only: bool,

    #[serde(rename = "teamAdminOnly")]
    pub team_admin_only: bool,
}

impl Serialize for NoticeMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Count the embedded fields that survive omitempty, plus its two mandatory ones, plus
        // this struct's three.
        let optional = usize::from(self.internal.action.is_some())
            + usize::from(self.internal.action_param.is_some())
            + usize::from(self.internal.action_text.is_some())
            + usize::from(self.internal.image.is_some());
        let mut map = serializer.serialize_map(Some(optional + 5))?;

        // The embed, first — see the note above.
        if let Some(action) = &self.internal.action {
            map.serialize_entry("action", action)?;
        }
        if let Some(param) = &self.internal.action_param {
            map.serialize_entry("actionParam", param)?;
        }
        if let Some(text) = &self.internal.action_text {
            map.serialize_entry("actionText", text)?;
        }
        map.serialize_entry("description", &self.internal.description)?;
        if let Some(image) = &self.internal.image {
            map.serialize_entry("image", image)?;
        }
        map.serialize_entry("title", &self.internal.title)?;

        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("sysAdminOnly", &self.sys_admin_only)?;
        map.serialize_entry("teamAdminOnly", &self.team_admin_only)?;
        map.end()
    }
}

/// Port of `model.ProductNoticeViewState` (product_notices.go:209) — "definition of the table
/// keeping the 'viewed' state of each in-product notice per user".
///
/// **No `json:` tags at all**, so every wire key is the Go field name in PascalCase — the
/// `wrangler.go` shape. `Viewed` is `int32`, not `int64`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductNoticeViewState {
    #[serde(rename = "UserId")]
    pub user_id: String,
    #[serde(rename = "NoticeId")]
    pub notice_id: String,
    #[serde(rename = "Viewed")]
    pub viewed: i32,
    #[serde(rename = "Timestamp")]
    pub timestamp: i64,
}

/// Port of `model.ExternalDependency` (product_notices.go:216).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDependency {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "minimum_version")]
    pub minimum_version: String,
}

/// Port of `model.ProductNotices` (product_notices.go:13).
///
/// "Order is important and is used to resolve priorities."
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProductNotices(pub Vec<ProductNotice>);

/// Port of `model.NoticeMessages` (product_notices.go:66).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoticeMessages(pub Vec<NoticeMessage>);

impl std::ops::Deref for ProductNotices {
    type Target = Vec<ProductNotice>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::Deref for NoticeMessages {
    type Target = Vec<NoticeMessage>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ProductNotice {
    /// Port of `(*ProductNotice).SysAdminOnly` (product_notices.go:33).
    ///
    /// A nil audience is `false` — the pointer is checked before it is dereferenced here, unlike
    /// in [`NoticeAudience::matches`], whose Go receiver is dereferenced unguarded.
    pub fn sys_admin_only(&self) -> bool {
        self.conditions
            .audience
            .as_ref()
            .is_some_and(|a| a.as_str() == NOTICE_AUDIENCE_SYSADMIN)
    }

    /// Port of `(*ProductNotice).TeamAdminOnly` (product_notices.go:37).
    pub fn team_admin_only(&self) -> bool {
        self.conditions
            .audience
            .as_ref()
            .is_some_and(|a| a.as_str() == NOTICE_AUDIENCE_TEAM_ADMIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_notice_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/product_notice.json");
        let v: ProductNotice = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn notice_message_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/notice_message.json");
        let v: NoticeMessage = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn view_state_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/product_notice_view_state.json");
        let v: ProductNoticeViewState = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn external_dependency_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/external_dependency.json");
        let v: ExternalDependency = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_product_notices.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_product_notices.json"
        ))
        .unwrap()
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["NoticeAudienceAll"], NOTICE_AUDIENCE_ALL);
        assert_eq!(c["NoticeAudienceMember"], NOTICE_AUDIENCE_MEMBER);
        assert_eq!(c["NoticeAudienceSysadmin"], NOTICE_AUDIENCE_SYSADMIN);
        assert_eq!(c["NoticeAudienceTeamAdmin"], NOTICE_AUDIENCE_TEAM_ADMIN);
        assert_eq!(c["NoticeClientTypeAll"], NOTICE_CLIENT_TYPE_ALL);
        assert_eq!(c["NoticeClientTypeDesktop"], NOTICE_CLIENT_TYPE_DESKTOP);
        assert_eq!(c["NoticeClientTypeMobile"], NOTICE_CLIENT_TYPE_MOBILE);
        assert_eq!(
            c["NoticeClientTypeMobileAndroid"],
            NOTICE_CLIENT_TYPE_MOBILE_ANDROID
        );
        assert_eq!(
            c["NoticeClientTypeMobileIos"],
            NOTICE_CLIENT_TYPE_MOBILE_IOS
        );
        assert_eq!(c["NoticeClientTypeWeb"], NOTICE_CLIENT_TYPE_WEB);
        assert_eq!(c["NoticeInstanceTypeBoth"], NOTICE_INSTANCE_TYPE_BOTH);
        assert_eq!(c["NoticeInstanceTypeCloud"], NOTICE_INSTANCE_TYPE_CLOUD);
        assert_eq!(c["NoticeInstanceTypeOnPrem"], NOTICE_INSTANCE_TYPE_ON_PREM);
        assert_eq!(c["NoticeSKUE0"], NOTICE_SKU_E0);
        assert_eq!(c["NoticeSKUE10"], NOTICE_SKU_E10);
        assert_eq!(c["NoticeSKUE20"], NOTICE_SKU_E20);
        assert_eq!(c["NoticeSKUAll"], NOTICE_SKU_ALL);
        assert_eq!(c["NoticeSKUTeam"], NOTICE_SKU_TEAM);
        assert_eq!(c["URL"], NOTICE_ACTION_URL);
    }

    /// Byte-exact against Go, which means going through `go_json_marshal` rather than
    /// `serde_json::to_string`.
    ///
    /// Go's `encoding/json` HTML-escapes `<`, `>` and `&` into `\u003c`, `\u003e` and `\u0026`;
    /// serde_json does not ([D-022]). This corpus reaches that path because `Conditions` holds
    /// semver ranges — `">=1.2.3"`, `"<v5.19"` — and display-date expressions, all of which are
    /// exactly the strings that differ. A test using plain `to_string` passes on every other
    /// document in the file and fails only on the realistic ones.
    #[test]
    fn wire_format_is_byte_exact() {
        use crate::utils::go_json_marshal;

        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let ours = match name {
                n if n.starts_with("notice_") && !n.starts_with("notices_") => {
                    let v: ProductNotice = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "message_full" | "message_zero" => {
                    let v: NoticeMessage = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "message_internal_zero" | "message_internal_full" => {
                    let v: NoticeMessageInternal = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "view_state" | "view_state_zero" => {
                    let v: ProductNoticeViewState = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "external_dependency" => {
                    let v: ExternalDependency = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "notices_list" | "notices_list_empty" => {
                    let v: ProductNotices = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                "messages_list" => {
                    let v: NoticeMessages = serde_json::from_str(expected).unwrap();
                    go_json_marshal(&v).unwrap()
                }
                other => panic!("unmapped wire case: {other}"),
            };
            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    /// The embed's keys come first. Asserted on its own because serde's default `flatten`
    /// behaviour is the opposite and would pass every field-value check while failing this.
    #[test]
    fn the_embedded_message_fields_are_emitted_first() {
        let oracle = oracle();
        let expected = oracle["wire"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "message_full")
            .unwrap()["json"]
            .as_str()
            .unwrap();

        let message: NoticeMessage = serde_json::from_str(expected).unwrap();
        let ours = crate::utils::go_json_marshal(&message).unwrap();
        assert_eq!(ours, expected);

        // And the property stated directly: `id` must come after `title`, not before `action`.
        let id_at = ours.find("\"id\"").expect("id present");
        let title_at = ours.find("\"title\"").expect("title present");
        let action_at = ours.find("\"action\"").expect("action present");
        assert!(
            action_at < title_at && title_at < id_at,
            "embed emits first"
        );
    }

    #[test]
    fn audience_matches_go() {
        for case in oracle()["audience_matches"].as_array().unwrap() {
            let audience = NoticeAudience::new(case["audience"].as_str().unwrap());
            let sys_admin = case["sys_admin"].as_bool().unwrap();
            let team_admin = case["team_admin"].as_bool().unwrap();
            assert_eq!(
                audience.matches(sys_admin, team_admin),
                case["matches"].as_bool().unwrap(),
                "audience {:?} with sys={sys_admin} team={team_admin}",
                audience.as_str()
            );
        }
    }

    #[test]
    fn client_type_matches_go() {
        for case in oracle()["client_matches"].as_array().unwrap() {
            let client = NoticeClientType::new(case["client"].as_str().unwrap());
            let other = NoticeClientType::new(case["other"].as_str().unwrap());
            assert_eq!(
                client.matches(&other),
                case["matches"].as_bool().unwrap(),
                "client {:?} against {:?}",
                client.as_str(),
                other.as_str()
            );
        }
    }

    #[test]
    fn instance_type_matches_go() {
        for case in oracle()["instance_matches"].as_array().unwrap() {
            let instance = NoticeInstanceType::new(case["instance"].as_str().unwrap());
            let is_cloud = case["is_cloud"].as_bool().unwrap();
            assert_eq!(
                instance.matches(is_cloud),
                case["matches"].as_bool().unwrap(),
                "instance {:?} with is_cloud={is_cloud}",
                instance.as_str()
            );
        }
    }

    #[test]
    fn sku_matches_go() {
        for case in oracle()["sku_matches"].as_array().unwrap() {
            let sku = NoticeSku::new(case["sku"].as_str().unwrap());
            let other = case["other"].as_str().unwrap();
            assert_eq!(
                sku.matches(other),
                case["matches"].as_bool().unwrap(),
                "sku {:?} against {other:?}",
                sku.as_str()
            );
        }
    }

    /// The disagreement, stated as its own claim: an unknown audience hides a notice and an
    /// unknown instance type shows one.
    #[test]
    fn unknown_values_fall_opposite_ways() {
        assert!(
            !NoticeAudience::new("unknown").matches(true, true),
            "an unrecognised audience matches nobody"
        );
        assert!(
            NoticeInstanceType::new("unknown").matches(true),
            "an unrecognised instance type matches everybody"
        );
        // And the zero values behave the same as any other unknown.
        assert!(!NoticeAudience::default().matches(true, true));
        assert!(NoticeInstanceType::default().matches(true));
    }

    /// `e0` and `team` mean "unlicensed", so they match `""` and not their own names.
    #[test]
    fn the_unlicensed_skus_match_the_empty_string_not_themselves() {
        assert!(NoticeSku::new(NOTICE_SKU_E0).matches(""));
        assert!(!NoticeSku::new(NOTICE_SKU_E0).matches("e0"));
        assert!(NoticeSku::new(NOTICE_SKU_TEAM).matches(""));
        assert!(!NoticeSku::new(NOTICE_SKU_TEAM).matches("team"));
        // A licensed SKU matches its own name and not the empty string.
        assert!(NoticeSku::new(NOTICE_SKU_E10).matches("e10"));
        assert!(!NoticeSku::new(NOTICE_SKU_E10).matches(""));
    }

    #[test]
    fn client_type_from_string_matches_go() {
        for case in oracle()["client_from_str"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let expected_value = case["value"].as_str().unwrap();
            let expected_ok = case["ok"].as_bool().unwrap();

            match notice_client_type_from_string(input) {
                Ok(value) => {
                    assert!(expected_ok, "{input:?} should have failed");
                    assert_eq!(value.as_str(), expected_value);
                }
                Err(fallback) => {
                    assert!(!expected_ok, "{input:?} should have succeeded");
                    assert_eq!(
                        fallback.as_str(),
                        expected_value,
                        "{input:?}: Go returns the value alongside the error"
                    );
                }
            }
        }
    }

    /// Two declared constants are not accepted by the function that parses them.
    #[test]
    fn from_string_rejects_mobile_and_all() {
        for rejected in [NOTICE_CLIENT_TYPE_MOBILE, NOTICE_CLIENT_TYPE_ALL] {
            let result = notice_client_type_from_string(rejected);
            assert!(
                result.is_err(),
                "{rejected:?} is a declared constant and still rejected"
            );
            assert_eq!(
                result.unwrap_err().as_str(),
                NOTICE_CLIENT_TYPE_ALL,
                "the failure value is `all`, not empty"
            );
        }
    }

    #[test]
    fn admin_only_matches_go() {
        for case in oracle()["admin_only"].as_array().unwrap() {
            let name = case["audience"].as_str().unwrap();
            let notice = ProductNotice {
                conditions: Conditions {
                    audience: if name == "nil" {
                        None
                    } else {
                        Some(NoticeAudience::new(name))
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                notice.sys_admin_only(),
                case["sys_admin_only"].as_bool().unwrap(),
                "sys_admin_only for {name}"
            );
            assert_eq!(
                notice.team_admin_only(),
                case["team_admin_only"].as_bool().unwrap(),
                "team_admin_only for {name}"
            );
        }
    }

    /// Go's `Matches` receivers are pointers dereferenced without a nil check, so all four panic
    /// on nil. Ours take `&self`, which makes that unrepresentable — the same shape as [D-095].
    #[test]
    fn gos_matches_receivers_all_panic_on_nil() {
        for case in oracle()["nil_receivers"].as_array().unwrap() {
            assert!(
                case["panics"].as_bool().unwrap(),
                "{} should panic on a nil receiver in Go",
                case["name"].as_str().unwrap()
            );
        }
        // Ours cannot be called on nothing, so there is nothing to reproduce — recorded in the
        // ledger rather than asserted here.
    }
}
