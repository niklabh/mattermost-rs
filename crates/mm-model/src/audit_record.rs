//! Port of `server/public/model/audit_record.go`.
//!
//! The record every audited API call builds. Three things here are not what reading the file
//! suggests, all three measured against Go in `fixtures/behaviour_audit_record.json`:
//!
//! * the field holding the event data is on the wire as **`event`**, while the constant naming it
//!   says `event_data` — see [`AuditRecord`];
//! * [`AuditRecord::add_meta`] panics in Go where every `AddEventParameter*` function does not;
//! * [`EventMeta`] is declared, tagged, and never used by [`AuditRecord`], whose `meta` is an
//!   open map.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, StringInterface};

// --- keys ------------------------------------------------------------------------------------
//
// These name fields in the audit *output*, which is assembled elsewhere; they are not all struct
// tags, and one of them contradicts the tag it appears to describe. Pinned against Go.

/// audit_record.go:7
pub const AUDIT_KEY_ACTOR: &str = "actor";
/// audit_record.go:8
pub const AUDIT_KEY_API_PATH: &str = "api_path";
/// audit_record.go:9
pub const AUDIT_KEY_EVENT: &str = "event";
/// audit_record.go:10 — **not** the tag on [`AuditRecord::event_data`], which is `event`.
pub const AUDIT_KEY_EVENT_DATA: &str = "event_data";
/// audit_record.go:11
pub const AUDIT_KEY_EVENT_NAME: &str = "event_name";
/// audit_record.go:12
pub const AUDIT_KEY_META: &str = "meta";
/// audit_record.go:13
pub const AUDIT_KEY_ERROR: &str = "error";
/// audit_record.go:14
pub const AUDIT_KEY_STATUS: &str = "status";
/// audit_record.go:15
pub const AUDIT_KEY_USER_ID: &str = "user_id";
/// audit_record.go:16
pub const AUDIT_KEY_SESSION_ID: &str = "session_id";
/// audit_record.go:17
pub const AUDIT_KEY_CLIENT: &str = "client";
/// audit_record.go:18
pub const AUDIT_KEY_IP_ADDRESS: &str = "ip_address";
/// audit_record.go:19
pub const AUDIT_KEY_CLUSTER_ID: &str = "cluster_id";

/// audit_record.go:21
pub const AUDIT_STATUS_SUCCESS: &str = "success";
/// audit_record.go:22
pub const AUDIT_STATUS_ATTEMPT: &str = "attempt";
/// audit_record.go:23
pub const AUDIT_STATUS_FAIL: &str = "fail";

// --- the Auditable trait ---------------------------------------------------------------------

/// Port of the `Auditable` interface (audit_record.go:69).
///
/// Go's doc comment is the specification: *"for sensitive object classes, consider implementing
/// Auditable and include whatever the AuditableObject returns. For example: it's likely OK to
/// write a user object to the audit logs, but not the user password in cleartext or hashed
/// form"*. An implementation is a **redaction boundary**, not a serialiser — which is why it is a
/// hand-written map rather than a derive.
///
/// Returns [`StringInterface`] rather than a bare `Value` because Go's return type is
/// `map[string]any`: the result is always an object, and typing it that way stops an
/// implementation returning something that would break `prior_state`.
pub trait Auditable {
    fn auditable(&self) -> StringInterface;
}

impl Auditable for crate::bot::Bot {
    fn auditable(&self) -> StringInterface {
        crate::bot::Bot::auditable(self)
    }
}

impl Auditable for crate::bot::BotPatch {
    fn auditable(&self) -> StringInterface {
        crate::bot::BotPatch::auditable(self)
    }
}

// --- the wire types --------------------------------------------------------------------------

/// Port of `model.AuditRecord` (audit_record.go:27).
///
/// # `event_data` is on the wire as `event`
///
/// The Go field is `EventData AuditEventData \`json:"event"\``, while `AuditKeyEventData` — three
/// lines above it — is the string `"event_data"`. The tag wins for serialisation, so a port that
/// trusts the constant emits a key nothing reads. Measured: Go's key list for this type is
/// `["event_name", "status", "event", "actor", "meta", "error"]`.
///
/// Nothing on this struct carries `omitempty`, so every key is present even when its map is nil —
/// and a nil map is `null`, not `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    #[serde(rename = "event_name")]
    pub event_name: String,

    #[serde(rename = "status")]
    pub status: String,

    /// Named for what it holds; tagged for what Go puts on the wire.
    #[serde(rename = "event")]
    pub event_data: AuditEventData,

    #[serde(rename = "actor")]
    pub actor: AuditEventActor,

    /// `map[string]any` in Go, and an **open** map rather than [`EventMeta`] — see that type.
    /// `Option` because nil and empty are different documents (`null` vs `{}`).
    #[serde(rename = "meta")]
    pub meta: Option<StringInterface>,

    #[serde(rename = "error")]
    pub error: AuditEventError,
}

/// Port of `model.AuditEventData` (audit_record.go:36).
///
/// Note `ResultState`'s tag is `resulting_state`, not `result_state`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEventData {
    /// "Payload and parameters being processed as part of the request".
    #[serde(rename = "parameters")]
    pub parameters: Option<StringInterface>,

    /// "Prior state of the object being modified, nil if no prior state" — so the nil case is
    /// meaningful and must stay distinguishable from an empty map.
    #[serde(rename = "prior_state")]
    pub prior_state: Option<StringInterface>,

    /// "Resulting object after creating or modifying it".
    #[serde(rename = "resulting_state")]
    pub result_state: Option<StringInterface>,

    /// "String representation of the object type. eg. \"post\"".
    #[serde(rename = "object_type")]
    pub object_type: String,
}

/// Port of `model.AuditEventActor` (audit_record.go:44) — "the subject triggering the event".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEventActor {
    #[serde(rename = "user_id")]
    pub user_id: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    #[serde(rename = "client")]
    pub client: String,
    #[serde(rename = "ip_address")]
    pub ip_address: String,
    #[serde(rename = "x_forwarded_for")]
    pub x_forwarded_for: String,
}

/// Port of `model.EventMeta` (audit_record.go:54).
///
/// **Declared, tagged, and never used by [`AuditRecord`]**, whose `meta` field is a bare
/// `map[string]any`. Nothing in `audit_record.go` constructs one. Ported because it is exported
/// and something outside this file may build it — but do not be tempted to make it the type of
/// `AuditRecord::meta`: that would narrow what an arbitrary `add_meta` call can store, which is
/// the opposite of the field's purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMeta {
    #[serde(rename = "api_path")]
    pub api_path: String,
    #[serde(rename = "cluster_id")]
    pub cluster_id: String,
}

/// Port of `model.AuditEventError` (audit_record.go:60).
///
/// The only nested type with `omitempty`, and it is on **both** fields — so a zero-valued error
/// serialises as `{}`, and a 0 status code disappears while a description survives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEventError {
    #[serde(
        rename = "description",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,

    /// Tagged `status_code`, not `code`.
    #[serde(rename = "status_code", default, skip_serializing_if = "is_zero_i32")]
    pub code: i32,
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

// --- behaviour -------------------------------------------------------------------------------

impl AuditRecord {
    /// Port of `(*AuditRecord).Success` (audit_record.go:72).
    pub fn success(&mut self) {
        self.status = AUDIT_STATUS_SUCCESS.to_owned();
    }

    /// Port of `(*AuditRecord).Fail` (audit_record.go:77).
    pub fn fail(&mut self) {
        self.status = AUDIT_STATUS_FAIL.to_owned();
    }

    /// Port of `AddEventParameterToAuditRec` (audit_record.go:83).
    ///
    /// Go constrains the generic to `string | bool | int | int64 | []string | map[string]string`.
    /// This accepts anything convertible to a JSON value, which is a **wider** set — but it
    /// cannot produce a different result for any value Go would accept, since each of those lands
    /// in a `map[string]any` and marshals as its own JSON type either way. See [D-098].
    ///
    /// Creates the parameter map when it is absent, which is Go's behaviour here and the reason
    /// [`AuditRecord::add_meta`] stands out.
    pub fn add_event_parameter(&mut self, key: &str, value: impl Into<serde_json::Value>) {
        self.event_data
            .parameters
            .get_or_insert_with(StringInterface::new)
            .insert(key.to_owned(), value.into());
    }

    /// Port of `AddEventParameterAuditableToAuditRec` (audit_record.go:92).
    pub fn add_event_parameter_auditable(&mut self, key: &str, value: &impl Auditable) {
        self.event_data
            .parameters
            .get_or_insert_with(StringInterface::new)
            .insert(key.to_owned(), serde_json::Value::Object(value.auditable()));
    }

    /// Port of `AddEventParameterAuditableArrayToAuditRec` (audit_record.go:101).
    ///
    /// Go builds the slice with `make([]map[string]any, 0, len(val))`, so an empty input yields
    /// `[]` rather than `null`.
    pub fn add_event_parameter_auditable_array<T: Auditable>(&mut self, key: &str, values: &[T]) {
        let processed: Vec<serde_json::Value> = values
            .iter()
            .map(|value| serde_json::Value::Object(value.auditable()))
            .collect();

        self.event_data
            .parameters
            .get_or_insert_with(StringInterface::new)
            .insert(key.to_owned(), serde_json::Value::Array(processed));
    }

    /// Port of `(*AuditRecord).AddEventPriorState` (audit_record.go:114).
    pub fn add_event_prior_state(&mut self, object: &impl Auditable) {
        self.event_data.prior_state = Some(object.auditable());
    }

    /// Port of `(*AuditRecord).AddEventResultState` (audit_record.go:119).
    pub fn add_event_result_state(&mut self, object: &impl Auditable) {
        self.event_data.result_state = Some(object.auditable());
    }

    /// Port of `(*AuditRecord).AddEventObjectType` (audit_record.go:124).
    pub fn add_event_object_type(&mut self, object_type: &str) {
        self.event_data.object_type = object_type.to_owned();
    }

    /// Port of `(*AuditRecord).AddMeta` (audit_record.go:130).
    ///
    /// # This one panics in Go and does not here
    ///
    /// Go's body is one line — `rec.Meta[name] = val` — with no nil check, while every
    /// `AddEventParameter*` function above lazily creates its map. `Meta` has no constructor
    /// anywhere in `audit_record.go`, so `AddMeta` on a zero-valued record assigns to a nil map
    /// and panics. Measured: `AddMeta_on_nil_map` → `panics: true`, all three parameter adders →
    /// `panics: false`.
    ///
    /// This port creates the map instead, matching what its siblings do. The divergence is in the
    /// safe direction — Go's panic would surface as a 500 and lose the audit record; ours records
    /// the entry — and `CLAUDE.md` forbids a panic in library code regardless. See [D-097].
    pub fn add_meta(&mut self, name: &str, value: serde_json::Value) {
        self.meta
            .get_or_insert_with(StringInterface::new)
            .insert(name.to_owned(), value);
    }

    /// Port of `(*AuditRecord).AddErrorCode` (audit_record.go:136).
    pub fn add_error_code(&mut self, code: i32) {
        self.error.code = code;
    }

    /// Port of `(*AuditRecord).AddErrorDesc` (audit_record.go:141).
    pub fn add_error_desc(&mut self, description: &str) {
        self.error.description = description.to_owned();
    }

    /// Port of `(*AuditRecord).AddAppError` (audit_record.go:146).
    ///
    /// The description is `err.Error()` — the **formatted** string, including `Where` and the
    /// detailed error — not `err.Message`. Measured: an `AppError` whose `Message` is
    /// `"some.error.id"` produces the description `"SomeWhere: some.error.id, detailed bit"`.
    /// Using `Message` would put a bare id in the audit log where Go puts context.
    pub fn add_app_error(&mut self, err: &AppError) {
        self.add_error_code(err.status_code);
        self.add_error_desc(&err.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_record_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/audit_record.json");
        let record: AuditRecord = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&record).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn event_meta_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/event_meta.json");
        let meta: EventMeta = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&meta).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_audit_record.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::bot::Bot;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_audit_record.json"
        ))
        .unwrap()
    }

    /// Top-level keys of a JSON object **in emission order**.
    ///
    /// `serde_json::to_value` cannot be used for this: its `Map` is a `BTreeMap`, so it sorts
    /// keys alphabetically and loses exactly the property under test. Serialising to a string
    /// preserves serde's field order, so the order has to be read back out of the text.
    fn ordered_keys(json: &str) -> Vec<String> {
        let bytes = json.as_bytes();
        let mut keys = Vec::new();
        let mut depth = 0usize;
        let mut i = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut current = String::new();

        while i < bytes.len() {
            let c = bytes[i] as char;
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                    // A string that closes at depth 1 and is followed by ':' is a key.
                    if depth == 1 {
                        let mut j = i + 1;
                        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b':' {
                            keys.push(std::mem::take(&mut current));
                        }
                    }
                    current.clear();
                } else if depth == 1 {
                    current.push(c);
                }
            } else {
                match c {
                    '"' => in_string = true,
                    '{' | '[' => depth += 1,
                    '}' | ']' => depth -= 1,
                    _ => {}
                }
            }
            i += 1;
        }
        keys
    }

    fn bot(user_id: &str, username: &str) -> Bot {
        Bot {
            user_id: user_id.to_owned(),
            username: username.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["AuditKeyActor"], AUDIT_KEY_ACTOR);
        assert_eq!(c["AuditKeyAPIPath"], AUDIT_KEY_API_PATH);
        assert_eq!(c["AuditKeyEvent"], AUDIT_KEY_EVENT);
        assert_eq!(c["AuditKeyEventData"], AUDIT_KEY_EVENT_DATA);
        assert_eq!(c["AuditKeyEventName"], AUDIT_KEY_EVENT_NAME);
        assert_eq!(c["AuditKeyMeta"], AUDIT_KEY_META);
        assert_eq!(c["AuditKeyError"], AUDIT_KEY_ERROR);
        assert_eq!(c["AuditKeyStatus"], AUDIT_KEY_STATUS);
        assert_eq!(c["AuditKeyUserID"], AUDIT_KEY_USER_ID);
        assert_eq!(c["AuditKeySessionID"], AUDIT_KEY_SESSION_ID);
        assert_eq!(c["AuditKeyClient"], AUDIT_KEY_CLIENT);
        assert_eq!(c["AuditKeyIPAddress"], AUDIT_KEY_IP_ADDRESS);
        assert_eq!(c["AuditKeyClusterID"], AUDIT_KEY_CLUSTER_ID);
        assert_eq!(c["AuditStatusSuccess"], AUDIT_STATUS_SUCCESS);
        assert_eq!(c["AuditStatusAttempt"], AUDIT_STATUS_ATTEMPT);
        assert_eq!(c["AuditStatusFail"], AUDIT_STATUS_FAIL);
    }

    /// The constant says `event_data`; the wire says `event`. Asserted against Go's own tag list
    /// so the discrepancy is pinned rather than merely commented.
    #[test]
    fn the_event_key_is_event_not_event_data() {
        let oracle = oracle();
        let keys: Vec<&str> = oracle["keys"]["record"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();

        assert_eq!(
            keys,
            vec!["event_name", "status", "event", "actor", "meta", "error"]
        );
        assert!(
            keys.contains(&AUDIT_KEY_EVENT),
            "the tag is the AuditKeyEvent value"
        );
        assert!(
            !keys.contains(&AUDIT_KEY_EVENT_DATA),
            "AuditKeyEventData names no key on this struct — that is the trap"
        );

        // And our own serialisation emits them in the same order.
        let ours = crate::utils::go_json_marshal(&AuditRecord::default()).unwrap();
        assert_eq!(ordered_keys(&ours), keys);
    }

    #[test]
    fn nested_key_lists_match_go() {
        let oracle = oracle();
        let expect = |name: &str, json: String| {
            let theirs: Vec<&str> = oracle["keys"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap())
                .collect();
            assert_eq!(ordered_keys(&json), theirs, "key mismatch for {name}");
        };

        expect(
            "event_data",
            crate::utils::go_json_marshal(&AuditEventData::default()).unwrap(),
        );
        expect(
            "actor",
            crate::utils::go_json_marshal(&AuditEventActor::default()).unwrap(),
        );
        expect(
            "event_meta",
            crate::utils::go_json_marshal(&EventMeta::default()).unwrap(),
        );
        // AuditEventError's fields are both omitempty, so the zero value has no keys at all —
        // serialise a populated one instead.
        expect(
            "event_error",
            crate::utils::go_json_marshal(&AuditEventError {
                description: "d".to_owned(),
                code: 1,
            })
            .unwrap(),
        );
    }

    #[test]
    fn wire_format_is_byte_exact() {
        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let ours = if name.starts_with("event_meta") {
                let meta: EventMeta = serde_json::from_str(expected).unwrap();
                crate::utils::go_json_marshal(&meta).unwrap()
            } else {
                let record: AuditRecord = serde_json::from_str(expected).unwrap();
                crate::utils::go_json_marshal(&record).unwrap()
            };
            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    #[test]
    fn status_setters_match_go() {
        for case in oracle()["status_setters"].as_array().unwrap() {
            let mut record = AuditRecord {
                status: "whatever".to_owned(),
                ..Default::default()
            };
            match case["name"].as_str().unwrap() {
                "Success" => record.success(),
                "Fail" => record.fail(),
                other => panic!("unmapped: {other}"),
            }
            assert_eq!(record.status, case["status"].as_str().unwrap());
        }
    }

    /// The asymmetry: the parameter adders create their map, `AddMeta` does not.
    ///
    /// Go panics on the last one. We do not — see [D-097] — so this asserts the *recorded* Go
    /// answer for each case and then states our deliberate difference explicitly.
    #[test]
    fn the_nil_map_asymmetry_is_gos_and_our_divergence_is_deliberate() {
        let oracle = oracle();
        let cases = oracle["nil_maps"].as_array().unwrap();
        let panics = |name: &str| -> bool {
            cases
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("missing case {name}"))["panics"]
                .as_bool()
                .unwrap()
        };

        // Go: the three parameter adders are safe on a zero record.
        assert!(!panics("AddEventParameterToAuditRec_on_nil_map"));
        assert!(!panics("AddEventParameterAuditableToAuditRec_on_nil_map"));
        assert!(!panics(
            "AddEventParameterAuditableArrayToAuditRec_on_nil_map"
        ));
        assert!(!panics("AddEventPriorState_on_zero_record"));
        // Go: AddMeta is not.
        assert!(
            panics("AddMeta_on_nil_map"),
            "if upstream adds the nil check, D-097 stops being a divergence"
        );
        assert!(!panics("AddMeta_on_existing_map"));

        // Ours: every one of them works on a zero record, including add_meta.
        let mut record = AuditRecord::default();
        record.add_meta("k", serde_json::Value::String("v".to_owned()));
        assert_eq!(
            record.meta.as_ref().and_then(|m| m.get("k")),
            Some(&serde_json::Value::String("v".to_owned()))
        );
    }

    #[test]
    fn parameters_match_go() {
        let oracle = oracle();
        for case in oracle["parameters"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected: serde_json::Value =
                serde_json::from_str(case["parameters"].as_str().unwrap()).unwrap();

            let mut record = AuditRecord::default();
            match name {
                "mixed_types" => {
                    record.add_event_parameter("a_string", "text");
                    record.add_event_parameter("a_bool", true);
                    record.add_event_parameter("an_int", 42);
                    record.add_event_parameter("an_int64", 9_007_199_254_740_993i64);
                    record.add_event_parameter("a_string_slice", vec!["x", "y"]);
                    record.add_event_parameter("a_string_map", serde_json::json!({"k": "v"}));
                    // Overwriting replaces rather than appending.
                    record.add_event_parameter("a_string", "replaced");
                }
                "auditable_array" => {
                    let bots = [
                        bot("aaaaaaaaaaaaaaaaaaaaaaaaaa", "one"),
                        bot("bbbbbbbbbbbbbbbbbbbbbbbbbb", "two"),
                    ];
                    record.add_event_parameter_auditable_array("bots", &bots);
                }
                "auditable_array_empty" => {
                    let bots: [Bot; 0] = [];
                    record.add_event_parameter_auditable_array("bots", &bots);
                }
                other => panic!("unmapped: {other}"),
            }

            let ours = serde_json::to_value(record.event_data.parameters).unwrap();
            assert_eq!(ours, expected, "parameters mismatch for {name}");
        }
    }

    #[test]
    fn prior_and_result_state_match_go() {
        let oracle = oracle();
        let case = &oracle["states"][0];
        let subject = Bot {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            username: "botusername".to_owned(),
            display_name: "Bot".to_owned(),
            owner_id: "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            create_at: 100,
            update_at: 200,
            ..Default::default()
        };

        let mut record = AuditRecord::default();
        record.add_event_prior_state(&subject);
        record.add_event_result_state(&subject);
        record.add_event_object_type("bot");

        let expected_prior: serde_json::Value =
            serde_json::from_str(case["prior_state"].as_str().unwrap()).unwrap();
        let expected_result: serde_json::Value =
            serde_json::from_str(case["resulting_state"].as_str().unwrap()).unwrap();

        assert_eq!(
            serde_json::to_value(&record.event_data.prior_state).unwrap(),
            expected_prior
        );
        assert_eq!(
            serde_json::to_value(&record.event_data.result_state).unwrap(),
            expected_result
        );
        assert_eq!(
            record.event_data.object_type,
            case["object_type"].as_str().unwrap()
        );
    }

    /// `AddAppError` stores the **formatted** error, not the message.
    #[test]
    fn add_app_error_matches_go() {
        let oracle = oracle();
        let case = oracle["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "from_app_error")
            .unwrap();

        let err = AppError::new("SomeWhere", "some.error.id", None, "detailed bit", 409);
        let mut record = AuditRecord::default();
        record.add_app_error(&err);

        assert_eq!(record.error.code, case["code"].as_i64().unwrap() as i32);
        assert_eq!(
            record.error.description,
            case["description"].as_str().unwrap()
        );
        // The distinction that matters: this is Error(), not Message.
        assert_ne!(
            record.error.description,
            case["app_error_msg"].as_str().unwrap(),
            "using Message would put a bare id where Go puts context"
        );
        assert_eq!(err.to_string(), case["app_error_error"].as_str().unwrap());
    }

    #[test]
    fn explicit_error_setters_match_go() {
        let oracle = oracle();
        let case = oracle["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "explicit")
            .unwrap();

        let mut record = AuditRecord::default();
        record.add_error_code(418);
        record.add_error_desc("teapot");

        assert_eq!(record.error.code, case["code"].as_i64().unwrap() as i32);
        assert_eq!(
            record.error.description,
            case["description"].as_str().unwrap()
        );
    }
}
