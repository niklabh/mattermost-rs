//! Port of `server/public/model/post_acknowledgement.go`.
//!
//! A leaf under `post_metadata.go`. One wire type, an `IsValid` over three ids, and two small
//! helpers.
//!
//! The trap is `remote_id`. Three ported types now carry a `*string` field called `remote_id` —
//! this one, [`crate::reaction::Reaction`] and [`crate::file_info::FileInfo`] — and **only this
//! one has `omitempty`**. So a nil `remote_id` *disappears* here and serialises as `null` in the
//! other two. Same Go type, same field name, different wire shape; the oracle records all three
//! side by side.
//!
//! Pinned by `fixtures/post_acknowledgement.json` and `fixtures/behaviour_post_leaves.json`.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, get_millis, is_valid_id};

/// Port of `model.PostAcknowledgement` (post_acknowledgement.go:8).
///
/// The Go struct also carries `xml:` tags, which nothing in the migration targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostAcknowledgement {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "post_id")]
    pub post_id: String,

    /// Epoch milliseconds. Never validated — see [`Self::is_valid`].
    #[serde(rename = "acknowledged_at")]
    pub acknowledged_at: i64,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// **Has `omitempty`, unlike `Reaction.RemoteId` and `FileInfo.RemoteId`**, so a nil value
    /// is dropped from the JSON rather than written as `null`.
    #[serde(rename = "remote_id", skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

impl PostAcknowledgement {
    /// Port of `(*PostAcknowledgement).IsValid` (post_acknowledgement.go:16).
    ///
    /// Three id checks and nothing else. **`acknowledged_at` is not validated**, so zero and
    /// even negative values are accepted — which matters because [`Self::pre_save`] only fills
    /// it when it is exactly zero.
    ///
    /// Unlike `Reaction`, every failure here carries a detail naming the offending field.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", format!("user_id={}", self.user_id)));
        }

        if !is_valid_id(&self.post_id) {
            return Err(err("post_id", format!("post_id={}", self.post_id)));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", format!("channel_id={}", self.channel_id)));
        }

        Ok(())
    }

    /// Port of `(*PostAcknowledgement).PreSave` (post_acknowledgement.go:39).
    ///
    /// Fills `acknowledged_at` only when it is zero, and touches nothing else. Note it does
    /// **not** materialise `remote_id` the way `Reaction::pre_save` and `FileInfo::pre_save`
    /// do — a nil stays nil, and therefore stays off the wire.
    pub fn pre_save(&mut self) {
        if self.acknowledged_at == 0 {
            self.acknowledged_at = get_millis();
        }
    }

    /// Port of `(*PostAcknowledgement).GetRemoteID` (post_acknowledgement.go:32).
    ///
    /// Collapses nil and empty to `""`, so it cannot distinguish "never set" from "explicitly
    /// local" — same shape as `Reaction::get_remote_id`.
    pub fn get_remote_id(&self) -> &str {
        self.remote_id.as_deref().unwrap_or("")
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "PostAcknowledgement.IsValid",
        // Note the id says `acknowledgement`, not `post_acknowledgement`.
        format!("model.acknowledgement.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid() -> PostAcknowledgement {
        PostAcknowledgement {
            user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            post_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            acknowledged_at: 1_700_000_000_000,
            channel_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
            remote_id: Some("cluster-a".into()),
        }
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/post_acknowledgement.json");
        let parsed: PostAcknowledgement = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
        parsed.is_valid().unwrap();
    }

    #[test]
    fn a_nil_remote_id_is_omitted_here_but_null_elsewhere() {
        let mut ack = valid();
        ack.remote_id = None;
        let json = serde_json::to_value(&ack).unwrap();
        assert!(json.get("remote_id").is_none(), "omitempty should drop it");

        // The same field on Reaction has no omitempty and writes null instead.
        let reaction = crate::reaction::Reaction::default();
        let json = serde_json::to_value(&reaction).unwrap();
        assert_eq!(json["remote_id"], Value::Null);
    }

    #[test]
    fn an_empty_remote_id_survives() {
        let mut ack = valid();
        ack.remote_id = Some(String::new());
        assert_eq!(serde_json::to_value(&ack).unwrap()["remote_id"], "");
    }

    #[test]
    fn acknowledged_at_is_never_validated() {
        let mut ack = valid();
        ack.acknowledged_at = 0;
        ack.is_valid().unwrap();

        ack.acknowledged_at = -1;
        ack.is_valid().unwrap();
    }

    #[test]
    fn pre_save_fills_only_a_zero_timestamp_and_leaves_remote_id_alone() {
        let mut ack = valid();
        ack.acknowledged_at = 0;
        ack.remote_id = None;
        ack.pre_save();
        assert_ne!(ack.acknowledged_at, 0);
        // Unlike Reaction::pre_save, a nil remote_id is not materialised.
        assert_eq!(ack.remote_id, None);

        // A negative value is not zero, so it is kept.
        let mut ack = valid();
        ack.acknowledged_at = -1;
        ack.pre_save();
        assert_eq!(ack.acknowledged_at, -1);
    }

    #[test]
    fn the_error_id_says_acknowledgement_not_post_acknowledgement() {
        let mut ack = valid();
        ack.user_id = String::new();
        assert_eq!(
            ack.is_valid().unwrap_err().id,
            "model.acknowledgement.is_valid.user_id.app_error"
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_post_leaves.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_post_leaves.json")).unwrap()
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["acknowledgement_wire"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["json"].as_str().unwrap();
            let parsed: PostAcknowledgement = serde_json::from_str(input).unwrap();
            assert_eq!(
                crate::utils::go_json_marshal(&parsed).unwrap(),
                case["roundtrip"].as_str().unwrap(),
                "case {name}"
            );
            // Nothing here is lossy, unlike PostEmbed's `data`.
            assert_eq!(input, case["roundtrip"].as_str().unwrap(), "case {name}");
        }
    }

    /// The three `*string` `remote_id` fields, side by side. Only this one has `omitempty`, and
    /// getting that backwards would silently change two other types' wire output.
    #[test]
    fn remote_id_omitempty_differs_across_the_three_types() {
        let oracle = oracle();
        let want = &oracle["remote_id_omitempty_across"];

        let ours = crate::utils::go_json_marshal(&PostAcknowledgement::default()).unwrap();
        assert_eq!(ours, want["post_acknowledgement"].as_str().unwrap());
        assert!(!ours.contains("remote_id"), "ours should omit it");

        let reaction =
            crate::utils::go_json_marshal(&crate::reaction::Reaction::default()).unwrap();
        assert_eq!(reaction, want["reaction"].as_str().unwrap());
        assert!(reaction.contains(r#""remote_id":null"#));

        let file_info =
            crate::utils::go_json_marshal(&crate::file_info::FileInfo::default()).unwrap();
        assert_eq!(file_info, want["file_info"].as_str().unwrap());
        assert!(file_info.contains(r#""remote_id":null"#));
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["acknowledgement_is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let ack: PostAcknowledgement = serde_json::from_value(case["ack"].clone()).unwrap();
            let want = case["error_id"].as_str().unwrap();
            match ack.is_valid() {
                Ok(()) => assert!(want.is_empty(), "case {name}: valid, Go returned {want}"),
                Err(e) => {
                    assert_eq!(e.id, want, "case {name}");
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
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["acknowledgement_pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["in_acknowledged_at"].as_i64().unwrap();

            let mut ack = PostAcknowledgement {
                user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                acknowledged_at: input,
                ..Default::default()
            };
            ack.pre_save();

            assert_eq!(
                input != 0 && ack.acknowledged_at == input,
                case["preserved"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                input == 0 && ack.acknowledged_at != 0,
                case["generated"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                ack.remote_id.is_none(),
                case["out_remote_nil"].as_bool().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn get_remote_id_matches_go() {
        let oracle = oracle();
        let cases = oracle["acknowledgement_remote_id"].as_object().unwrap();

        let probe = |remote: Option<String>| PostAcknowledgement {
            remote_id: remote,
            ..Default::default()
        };
        assert_eq!(probe(None).get_remote_id(), cases["nil"].as_str().unwrap());
        assert_eq!(
            probe(Some(String::new())).get_remote_id(),
            cases["empty"].as_str().unwrap()
        );
        assert_eq!(
            probe(Some("cluster-a".into())).get_remote_id(),
            cases["set"].as_str().unwrap()
        );
    }
}
