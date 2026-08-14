//! Port of `server/public/model/preference.go`.
//!
//! The wire type is four strings; the interest is entirely in `IsValid` and `PreUpdate`, both of
//! which decode `Value` as JSON when the category is `theme`. Two properties of that decode
//! drive the whole design of this module:
//!
//! - Go uses **`json.Decoder.Decode`, not `json.Unmarshal`**. A `Decoder` reads the *first* JSON
//!   value in the stream and never looks for EOF, so `{"a":"b"} garbage` decodes cleanly.
//!   `serde_json::from_str` is the `Unmarshal`-shaped function and rejects that, so it is the
//!   wrong tool here — [`decode_theme`] drives a `Deserializer` directly instead.
//!
//! - `IsValid` checks the decode **error** while `PreUpdate` ignores it and uses the **value**,
//!   and Go's decoder produces both at once: a type error is recorded but the key is still
//!   inserted, holding the zero value. So a single decoder has to return the map *and* whether
//!   it failed, which is exactly what [`decode_theme`] does.
//!
//! Pinned by `fixtures/preference.json` and `fixtures/behaviour_preference.json`.

use std::ops::{Deref, DerefMut};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, StringMap, go_json_marshal_string_map, is_valid_id};

// ---------------------------------------------------------------------------
// Categories (preference.go:15-79)
// ---------------------------------------------------------------------------
//
// The primary key of the preference table is (User.Id, Category, Name), so both of those are
// routing keys rather than free text. A typo in one of these constants misfiles a preference
// instead of failing; every one is pinned against Go by `constants_match_go`.

/// `Name` is the channel ID.
pub const PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW: &str = "direct_channel_show";
/// `Name` is the channel ID.
pub const PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW: &str = "group_channel_show";
/// `Name` is the user ID again — Go's own comment says "for whatever reason".
pub const PREFERENCE_CATEGORY_TUTORIAL_STEPS: &str = "tutorial_step";
/// `Name` is the setting name (`formatting`, `send_on_ctrl_enter`, `join_leave`, …).
pub const PREFERENCE_CATEGORY_ADVANCED_SETTINGS: &str = "advanced_settings";
/// The user's saved posts. `Name` is the post ID.
pub const PREFERENCE_CATEGORY_FLAGGED_POST: &str = "flagged_post";
/// `Name` is the channel ID.
pub const PREFERENCE_CATEGORY_FAVORITE_CHANNEL: &str = "favorite_channel";
/// `Name` is [`PREFERENCE_NAME_SHOW_UNREAD_SECTION`] or [`PREFERENCE_LIMIT_VISIBLE_DMS_GMS`].
pub const PREFERENCE_CATEGORY_SIDEBAR_SETTINGS: &str = "sidebar_settings";
/// `Name` is one of the `PREFERENCE_NAME_*` display settings.
pub const PREFERENCE_CATEGORY_DISPLAY_SETTINGS: &str = "display_settings";
/// System admin notices; `Name` is the notice name and is not enumerated.
pub const PREFERENCE_CATEGORY_SYSTEM_NOTICE: &str = "system_notice";
/// Deprecated upstream and unused; kept so a stored row still round-trips.
pub const PREFERENCE_CATEGORY_LAST: &str = "last";
/// `Name` is one of the custom-status preference names.
pub const PREFERENCE_CATEGORY_CUSTOM_STATUS: &str = "custom_status";
/// `Name` is [`PREFERENCE_NAME_EMAIL_INTERVAL`].
pub const PREFERENCE_CATEGORY_NOTIFICATIONS: &str = "notifications";
/// `Name` is [`PREFERENCE_NAME_RECOMMENDED_NEXT_STEPS_HIDE`].
pub const PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS: &str = "recommended_next_steps";
/// Deprecated alias of [`PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS`]. Go defines it as that
/// constant, so the two can never differ; aliased here for the same reason.
pub const PREFERENCE_RECOMMENDED_NEXT_STEPS: &str = PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS;
/// `Name` is the team id the theme is set for. The **only** category whose `Value` is parsed.
pub const PREFERENCE_CATEGORY_THEME: &str = "theme";
/// `Name` is the OAuth client_id; `Value` is the current scope.
pub const PREFERENCE_CATEGORY_AUTHORIZED_OAUTH_APP: &str = "oauth_app";

// ---------------------------------------------------------------------------
// Names (preference.go:81-120)
// ---------------------------------------------------------------------------

pub const PREFERENCE_NAME_ATTACH_APP_LOGS: &str = "attach_app_logs";
pub const PREFERENCE_NAME_COLLAPSED_THREADS_ENABLED: &str = "collapsed_reply_threads";
pub const PREFERENCE_NAME_CHANNEL_DISPLAY_MODE: &str = "channel_display_mode";
pub const PREFERENCE_NAME_COLLAPSE_SETTING: &str = "collapse_previews";
pub const PREFERENCE_NAME_MESSAGE_DISPLAY: &str = "message_display";
pub const PREFERENCE_NAME_COLLAPSE_CONSECUTIVE: &str = "collapse_consecutive_messages";
pub const PREFERENCE_NAME_COLORIZE_USERNAMES: &str = "colorize_usernames";
pub const PREFERENCE_NAME_NAME_FORMAT: &str = "name_format";
pub const PREFERENCE_NAME_USE_MILITARY_TIME: &str = "use_military_time";
pub const PREFERENCE_NAME_SHOW_UNREAD_SECTION: &str = "show_unread_section";

/// The one preference whose `Value` `IsValid` range-checks.
pub const PREFERENCE_LIMIT_VISIBLE_DMS_GMS: &str = "limit_visible_dms_gms";

/// Deprecated upstream.
pub const PREFERENCE_NAME_LAST_CHANNEL: &str = "channel";
/// Deprecated upstream.
pub const PREFERENCE_NAME_LAST_TEAM: &str = "team";

pub const PREFERENCE_NAME_RECENT_CUSTOM_STATUSES: &str = "recent_custom_statuses";
pub const PREFERENCE_NAME_CUSTOM_STATUS_TUTORIAL_STATE: &str = "custom_status_tutorial_state";
pub const PREFERENCE_CUSTOM_STATUS_MODAL_VIEWED: &str = "custom_status_modal_viewed";
pub const PREFERENCE_NAME_EMAIL_INTERVAL: &str = "email_interval";
pub const PREFERENCE_NAME_RECOMMENDED_NEXT_STEPS_HIDE: &str = "hide";

/// The "immediate" email setting is actually 30 seconds.
pub const PREFERENCE_EMAIL_INTERVAL_NO_BATCHING_SECONDS: &str = "30";
pub const PREFERENCE_EMAIL_INTERVAL_BATCHING_SECONDS: &str = "900";
pub const PREFERENCE_EMAIL_INTERVAL_IMMEDIATELY: &str = "immediately";
pub const PREFERENCE_EMAIL_INTERVAL_FIFTEEN: &str = "fifteen";
pub const PREFERENCE_EMAIL_INTERVAL_FIFTEEN_AS_SECONDS: &str = "900";
pub const PREFERENCE_EMAIL_INTERVAL_HOUR: &str = "hour";
pub const PREFERENCE_EMAIL_INTERVAL_HOUR_AS_SECONDS: &str = "3600";
pub const PREFERENCE_CLOUD_USER_EPHEMERAL_INFO: &str = "cloud_user_ephemeral_info";

/// Inclusive upper bound for [`PREFERENCE_LIMIT_VISIBLE_DMS_GMS`]; the lower bound is 1.
pub const PREFERENCE_MAX_LIMIT_VISIBLE_DMS_GMS_VALUE: i64 = 40;

/// Counted in **runes**, unlike the category and name limits which are bytes.
pub const MAX_PREFERENCE_VALUE_LENGTH: usize = 20000;

/// Byte limit shared by `Category` and `Name` — `len()` in Go, so bytes.
const CATEGORY_AND_NAME_MAX_BYTES: usize = 32;

/// The colour Go substitutes for any theme value that fails [`PRE_UPDATE_COLOR_PATTERN`].
const DEFAULT_THEME_COLOR: &str = "#ffffff";

/// Theme keys [`Preference::pre_update`] leaves alone whatever they hold.
const THEME_NON_COLOR_KEYS: &[&str] = &["image", "type", "codeTheme"];

/// Port of `preUpdateColorPattern` (preference.go:166) — three or six hex digits after a `#`.
///
/// Go's RE2 anchors `^`/`$` to the whole text unless the `m` flag is set, and Rust's `regex`
/// crate does the same, so neither accepts a trailing newline. Pinned by the
/// `invalid_trailing_newline` oracle case, because Perl-style `$` would.
static PRE_UPDATE_COLOR_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$").ok());

/// Fails **closed**: an uncompilable pattern would make every value non-matching, so
/// `pre_update` would blank colours rather than wave them through. `regex_compiles` asserts the
/// case cannot arise.
fn is_theme_color(value: &str) -> bool {
    PRE_UPDATE_COLOR_PATTERN
        .as_ref()
        .is_some_and(|re| re.is_match(value))
}

/// Port of `model.Preference` (preference.go:123).
///
/// The primary key is `(user_id, category, name)`; `value` is opaque except for the two
/// categories `IsValid` special-cases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preference {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "category")]
    pub category: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "value")]
    pub value: String,
}

impl Preference {
    /// Port of `(*Preference).IsValid` (preference.go:132).
    ///
    /// The checks in order, with the units Go actually uses:
    ///
    /// | field | rule | unit |
    /// |---|---|---|
    /// | `user_id` | `IsValidId` | 26 bytes |
    /// | `category` | non-empty, ≤ 32 | **bytes** |
    /// | `name` | ≤ 32, may be empty | **bytes** |
    /// | `value` | ≤ 20000 | **runes** |
    ///
    /// The byte/rune split is the trap: 32 two-byte characters is a valid *value* length but an
    /// invalid *category* length.
    ///
    /// Then two category-specific checks — the theme decode and the DM/GM limit range — both of
    /// which are skipped entirely unless the category matches exactly.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.user_id) {
            return Err(err("id", format!("user_id={}", self.user_id)));
        }

        if self.category.is_empty() || self.category.len() > CATEGORY_AND_NAME_MAX_BYTES {
            return Err(err("category", format!("category={}", self.category)));
        }

        if self.name.len() > CATEGORY_AND_NAME_MAX_BYTES {
            return Err(err("name", format!("name={}", self.name)));
        }

        if self.value.chars().count() > MAX_PREFERENCE_VALUE_LENGTH {
            return Err(err("value", format!("value={}", self.value)));
        }

        if self.category == PREFERENCE_CATEGORY_THEME {
            // Only the error matters here; `pre_update` is the caller that wants the map.
            if decode_theme(&self.value).1 {
                return Err(err("theme", format!("value={}", self.value)));
            }
        }

        if self.category == PREFERENCE_CATEGORY_SIDEBAR_SETTINGS
            && self.name == PREFERENCE_LIMIT_VISIBLE_DMS_GMS
        {
            // `strconv.Atoi` — accepts a leading `+`/`-`, rejects whitespace, `0x`, `_` and
            // anything that overflows. Unlike version.go's `SplitVersion`, the error is
            // *checked* here, so an overflowing value is invalid rather than saturating.
            let parsed = self.value.parse::<i64>();
            let in_range = parsed
                .map(|n| (1..=PREFERENCE_MAX_LIMIT_VISIBLE_DMS_GMS_VALUE).contains(&n))
                .unwrap_or(false);
            if !in_range {
                return Err(err(
                    "limit_visible_dms_gms",
                    format!("value={}", self.value),
                ));
            }
        }

        Ok(())
    }

    /// Port of `(*Preference).PreUpdate` (preference.go:168).
    ///
    /// A no-op unless the category is exactly `theme`. For a theme it rewrites `value` to the
    /// re-marshalled map, which makes it a **normaliser**, not just a sanitiser: keys come back
    /// sorted, `<`/`>`/`&` come back HTML-escaped, and anything after the first JSON value is
    /// dropped.
    ///
    /// Its sharpest edge is that the decode error is deliberately ignored — Go's comment says
    /// the invalid value "should get caught by IsValid before saving". So an undecodable theme
    /// leaves `props` nil, and marshalling a nil Go map produces the four bytes `null`, which
    /// are written straight back into `value`. **A theme preference can come out of `pre_update`
    /// holding the literal string `"null"`.** Reproduced exactly; see the oracle cases named
    /// `*_becomes_null`.
    pub fn pre_update(&mut self) {
        if self.category != PREFERENCE_CATEGORY_THEME {
            return;
        }

        let (props, _ignored_error) = decode_theme(&self.value);
        let Some(mut props) = props else {
            // Go's nil map marshals to `null`, not `{}`.
            self.value = "null".to_string();
            return;
        };

        for (name, value) in &mut props {
            if THEME_NON_COLOR_KEYS.contains(&name.as_str()) {
                continue;
            }
            if !is_theme_color(value) {
                *value = DEFAULT_THEME_COLOR.to_string();
            }
        }

        // Go's `json.Marshal` of a map sorts the keys and HTML-escapes; this is the marshaller
        // that reproduces both.
        self.value = go_json_marshal_string_map(Some(&props));
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "Preference.IsValid",
        format!("model.preference.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// `json.NewDecoder(strings.NewReader(value)).Decode(&props)` where `props` is a
/// `map[string]string`, returning **both** of the things Go's call site can look at: the map it
/// left behind, and whether it reported an error.
///
/// `None` for the map is Go's nil map — the receiver was never written. That happens for a
/// syntax error, for an empty document, for a JSON `null` (which Go zeroes the map for), and for
/// any non-object value.
///
/// Three behaviours are not what `serde_json::from_str` would give:
///
/// 1. **Trailing content is allowed.** A `Decoder` reads one value and stops, so
///    `{"a":"b"} garbage` succeeds. This drives the `Deserializer` directly and never calls
///    `end()`, which is what reproduces that.
/// 2. **`null` as a value is not an error.** Go ignores a JSON null when the destination is a
///    primitive, leaving the zero value — so `{"a":null}` decodes to `{"a": ""}` cleanly.
/// 3. **A type error still inserts the key.** `{"a":1}` reports an error *and* leaves
///    `{"a": ""}` behind, which is why `pre_update` can turn a numeric theme value into
///    `#ffffff` rather than dropping it.
fn decode_theme(value: &str) -> (Option<StringMap>, bool) {
    let mut de = serde_json::Deserializer::from_str(value);
    // Deliberately not `de.end()`: Go's Decoder does not require EOF.
    let Ok(parsed) = serde_json::Value::deserialize(&mut de) else {
        return (None, true);
    };

    match parsed {
        // Go zeroes a map destination on JSON null, without error.
        serde_json::Value::Null => (None, false),
        serde_json::Value::Object(fields) => {
            let mut props = StringMap::new();
            let mut had_error = false;
            for (key, field) in fields {
                match field {
                    serde_json::Value::String(s) => {
                        props.insert(key, s);
                    }
                    // A JSON null into a Go string is ignored, leaving the zero value — and the
                    // key is still inserted. No error.
                    serde_json::Value::Null => {
                        props.insert(key, String::new());
                    }
                    // Any other type is an UnmarshalTypeError, which Go *saves* and continues
                    // past, having already stored the zero value under the key.
                    _ => {
                        props.insert(key, String::new());
                        had_error = true;
                    }
                }
            }
            (Some(props), had_error)
        }
        // A non-object value is a type error and leaves the map untouched.
        _ => (None, true),
    }
}

/// Port of `model.Preferences` (preference.go:130) — a bare `[]Preference`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Preferences(pub Vec<Preference>);

impl Deref for Preferences {
    type Target = Vec<Preference>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Preferences {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<Preference>> for Preferences {
    fn from(v: Vec<Preference>) -> Self {
        Self(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid() -> Preference {
        Preference {
            user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            category: PREFERENCE_CATEGORY_DISPLAY_SETTINGS.into(),
            name: PREFERENCE_NAME_USE_MILITARY_TIME.into(),
            value: "true".into(),
        }
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/preference.json");
        let parsed: Preference = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
        parsed.is_valid().unwrap();
    }

    #[test]
    fn category_is_limited_in_bytes_but_value_in_runes() {
        // 32 two-byte characters: 32 runes, 64 bytes. Too long as a category...
        let mut p = valid();
        p.category = "é".repeat(32);
        assert!(p.is_valid().is_err());

        // ...while 20000 of them are a perfectly valid value at 40000 bytes.
        let mut p = valid();
        p.value = "é".repeat(MAX_PREFERENCE_VALUE_LENGTH);
        p.is_valid().unwrap();
    }

    #[test]
    fn an_empty_name_is_valid_but_an_empty_category_is_not() {
        let mut p = valid();
        p.name = String::new();
        p.is_valid().unwrap();

        p.category = String::new();
        assert!(p.is_valid().is_err());
    }

    #[test]
    fn the_theme_decode_accepts_trailing_content() {
        // `json.Decoder.Decode` reads one value and stops. `serde_json::from_str` would reject
        // this, which is why decode_theme drives a Deserializer instead.
        let mut p = valid();
        p.category = PREFERENCE_CATEGORY_THEME.into();
        p.value = r#"{"a":"b"} and then some garbage"#.into();
        p.is_valid().unwrap();
    }

    #[test]
    fn an_undecodable_theme_becomes_the_string_null() {
        let mut p = valid();
        p.category = PREFERENCE_CATEGORY_THEME.into();
        p.value = "garbage".into();
        p.pre_update();
        assert_eq!(p.value, "null");
    }

    #[test]
    fn pre_update_only_touches_themes() {
        let mut p = valid();
        p.value = "garbage".into();
        p.pre_update();
        assert_eq!(p.value, "garbage");
    }

    #[test]
    fn pre_update_normalises_key_order_and_escaping() {
        let mut p = valid();
        p.category = PREFERENCE_CATEGORY_THEME.into();
        p.value = r##"{"z":"#abc","a<b":"#def"}"##.into();
        p.pre_update();
        // `<` comes back HTML-escaped: PreUpdate re-marshals through Go's encoder rules.
        assert_eq!(p.value, r##"{"a\u003cb":"#def","z":"#abc"}"##);
    }

    #[test]
    fn the_three_exempt_keys_keep_any_value() {
        let mut p = valid();
        p.category = PREFERENCE_CATEGORY_THEME.into();
        p.value = r#"{"image":"not a color","other":"not a color"}"#.into();
        p.pre_update();
        assert_eq!(p.value, r##"{"image":"not a color","other":"#ffffff"}"##);
    }

    #[test]
    fn the_dms_gms_limit_is_only_checked_under_its_own_category_and_name() {
        let mut p = valid();
        p.value = "999".into();

        // Right name, wrong category.
        p.category = PREFERENCE_CATEGORY_DISPLAY_SETTINGS.into();
        p.name = PREFERENCE_LIMIT_VISIBLE_DMS_GMS.into();
        p.is_valid().unwrap();

        // Right category, wrong name.
        p.category = PREFERENCE_CATEGORY_SIDEBAR_SETTINGS.into();
        p.name = PREFERENCE_NAME_SHOW_UNREAD_SECTION.into();
        p.is_valid().unwrap();

        // Both right: now it is range-checked.
        p.name = PREFERENCE_LIMIT_VISIBLE_DMS_GMS.into();
        assert!(p.is_valid().is_err());
    }

    #[test]
    fn regex_compiles() {
        // The fail-closed `Option` above is only safe because this cannot be None.
        assert!(PRE_UPDATE_COLOR_PATTERN.is_some());
    }

    #[test]
    fn preferences_is_a_transparent_array() {
        let list = Preferences(vec![valid()]);
        let json = serde_json::to_value(&list).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(list.len(), 1);
    }
}

/// Parity tests driven by `fixtures/behaviour_preference.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_preference.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        let expect = |key: &str, ours: &str| {
            assert_eq!(ours, c[key].as_str().unwrap(), "constant {key}");
        };

        expect(
            "category_direct_channel_show",
            PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW,
        );
        expect(
            "category_group_channel_show",
            PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW,
        );
        expect(
            "category_tutorial_steps",
            PREFERENCE_CATEGORY_TUTORIAL_STEPS,
        );
        expect(
            "category_advanced_settings",
            PREFERENCE_CATEGORY_ADVANCED_SETTINGS,
        );
        expect("category_flagged_post", PREFERENCE_CATEGORY_FLAGGED_POST);
        expect(
            "category_favorite_channel",
            PREFERENCE_CATEGORY_FAVORITE_CHANNEL,
        );
        expect(
            "category_sidebar_settings",
            PREFERENCE_CATEGORY_SIDEBAR_SETTINGS,
        );
        expect(
            "category_display_settings",
            PREFERENCE_CATEGORY_DISPLAY_SETTINGS,
        );
        expect("category_system_notice", PREFERENCE_CATEGORY_SYSTEM_NOTICE);
        expect("category_last", PREFERENCE_CATEGORY_LAST);
        expect("category_custom_status", PREFERENCE_CATEGORY_CUSTOM_STATUS);
        expect("category_notifications", PREFERENCE_CATEGORY_NOTIFICATIONS);
        expect(
            "category_recommended_next_steps",
            PREFERENCE_CATEGORY_RECOMMENDED_NEXT_STEPS,
        );
        expect("recommended_next_steps", PREFERENCE_RECOMMENDED_NEXT_STEPS);
        expect("category_theme", PREFERENCE_CATEGORY_THEME);
        expect(
            "category_authorized_oauth_app",
            PREFERENCE_CATEGORY_AUTHORIZED_OAUTH_APP,
        );

        expect("name_attach_app_logs", PREFERENCE_NAME_ATTACH_APP_LOGS);
        expect(
            "name_collapsed_threads_enabled",
            PREFERENCE_NAME_COLLAPSED_THREADS_ENABLED,
        );
        expect(
            "name_channel_display_mode",
            PREFERENCE_NAME_CHANNEL_DISPLAY_MODE,
        );
        expect("name_collapse_setting", PREFERENCE_NAME_COLLAPSE_SETTING);
        expect("name_message_display", PREFERENCE_NAME_MESSAGE_DISPLAY);
        expect(
            "name_collapse_consecutive",
            PREFERENCE_NAME_COLLAPSE_CONSECUTIVE,
        );
        expect(
            "name_colorize_usernames",
            PREFERENCE_NAME_COLORIZE_USERNAMES,
        );
        expect("name_name_format", PREFERENCE_NAME_NAME_FORMAT);
        expect("name_use_military_time", PREFERENCE_NAME_USE_MILITARY_TIME);
        expect(
            "name_show_unread_section",
            PREFERENCE_NAME_SHOW_UNREAD_SECTION,
        );
        expect("limit_visible_dms_gms", PREFERENCE_LIMIT_VISIBLE_DMS_GMS);
        expect("name_last_channel", PREFERENCE_NAME_LAST_CHANNEL);
        expect("name_last_team", PREFERENCE_NAME_LAST_TEAM);
        expect(
            "name_recent_custom_statuses",
            PREFERENCE_NAME_RECENT_CUSTOM_STATUSES,
        );
        expect(
            "name_custom_status_tutorial",
            PREFERENCE_NAME_CUSTOM_STATUS_TUTORIAL_STATE,
        );
        expect(
            "custom_status_modal_viewed",
            PREFERENCE_CUSTOM_STATUS_MODAL_VIEWED,
        );
        expect("name_email_interval", PREFERENCE_NAME_EMAIL_INTERVAL);
        expect(
            "name_recommended_next_steps_hide",
            PREFERENCE_NAME_RECOMMENDED_NEXT_STEPS_HIDE,
        );

        expect(
            "email_interval_no_batching_seconds",
            PREFERENCE_EMAIL_INTERVAL_NO_BATCHING_SECONDS,
        );
        expect(
            "email_interval_batching_seconds",
            PREFERENCE_EMAIL_INTERVAL_BATCHING_SECONDS,
        );
        expect(
            "email_interval_immediately",
            PREFERENCE_EMAIL_INTERVAL_IMMEDIATELY,
        );
        expect("email_interval_fifteen", PREFERENCE_EMAIL_INTERVAL_FIFTEEN);
        expect(
            "email_interval_fifteen_as_seconds",
            PREFERENCE_EMAIL_INTERVAL_FIFTEEN_AS_SECONDS,
        );
        expect("email_interval_hour", PREFERENCE_EMAIL_INTERVAL_HOUR);
        expect(
            "email_interval_hour_as_seconds",
            PREFERENCE_EMAIL_INTERVAL_HOUR_AS_SECONDS,
        );
        expect(
            "cloud_user_ephemeral_info",
            PREFERENCE_CLOUD_USER_EPHEMERAL_INFO,
        );

        assert_eq!(
            PREFERENCE_MAX_LIMIT_VISIBLE_DMS_GMS_VALUE,
            c["max_limit_visible_dms_gms_value"].as_i64().unwrap()
        );
        assert_eq!(
            MAX_PREFERENCE_VALUE_LENGTH as u64,
            c["max_preference_value_length"].as_u64().unwrap()
        );
    }

    /// Each case embeds the preference as Go-marshalled JSON, so a wire drift and a logic drift
    /// both fail here. The error id *and* its detail are asserted — clients key off both.
    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let preference: Preference =
                serde_json::from_value(case["preference"].clone()).unwrap();
            let want_id = case["error_id"].as_str().unwrap();

            match preference.is_valid() {
                Ok(()) => assert!(
                    want_id.is_empty(),
                    "case {name}: valid, Go returned {want_id}"
                ),
                Err(e) => {
                    assert_eq!(e.id, want_id, "case {name}");
                    assert_eq!(
                        e.detailed_error,
                        case["detailed"].as_str().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(e.status_code, 400, "case {name}");
                }
            }
        }
    }

    #[test]
    fn pre_update_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_update"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut preference = Preference {
                user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                category: case["category"].as_str().unwrap().into(),
                name: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
                value: case["in"].as_str().unwrap().into(),
            };
            preference.pre_update();
            // Byte-for-byte: key order and HTML escaping are both part of what PreUpdate does.
            assert_eq!(
                preference.value,
                case["out"].as_str().unwrap(),
                "case {name}"
            );
        }
    }
}
