//! Port of `server/public/model/reaction.go`.
//!
//! Eight fields and three small methods. The thing to carry away is that **reacting with an
//! emoji is not the same as creating one**: `Reaction::is_valid` checks the name against a
//! pattern and the 64-byte limit but never against the system-emoji table, so `grinning` is a
//! perfectly legal reaction and an illegal custom emoji. Two validators, one shared constant,
//! different rules — see [`crate::emoji::is_valid_emoji_name`] for the other half.
//!
//! Pinned by `fixtures/reaction.json` and `fixtures/behaviour_reaction.json`.

use serde::{Deserialize, Serialize};

use crate::emoji::EMOJI_NAME_MAX_LENGTH;
use crate::utils::{
    AppError, AppResult, get_millis, is_valid_alpha_num_hyphen_underscore_plus, is_valid_id,
};

/// Port of `model.Reaction` (reaction.go:11).
///
/// `remote_id` is a `*string` with **no** `omitempty`, so the key is always present and a nil
/// pointer serialises as `null` — `Option<String>` with no skip predicate. `PreSave` and
/// `PreUpdate` both materialise it to `Some("")`, so a nil only survives on a reaction that has
/// been through neither.
///
/// The Go struct also carries `xml:` tags, which nothing in the migration targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reaction {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "emoji_name")]
    pub emoji_name: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "remote_id")]
    pub remote_id: Option<String>,

    /// Never validated — see [`Reaction::is_valid`].
    #[serde(rename = "channel_id")]
    pub channel_id: String,
}

impl Reaction {
    /// Port of `(*Reaction).IsValid` (reaction.go:22).
    ///
    /// Checks `user_id`, `post_id`, `emoji_name` and the two timestamps — and nothing else.
    /// **`channel_id` and `delete_at` are not validated at all**, so `channel_id: "nope"` and an
    /// empty one both pass, as does a reaction already marked deleted.
    ///
    /// The emoji-name rule is the 64-**byte** limit plus `^[a-zA-Z0-9+_-]+$`, with **no**
    /// system-emoji check. Go compiles that pattern inline at reaction.go:31 rather than calling
    /// `IsValidAlphaNumHyphenUnderscorePlus`, and writes the character class differently
    /// (`[a-zA-Z0-9\-\+_]`). The two agree on all 32 oracle inputs — see
    /// `go_parity::the_two_emoji_name_regexes_agree` — so the shared validator is reused here on
    /// evidence rather than on inspection.
    ///
    /// Note the two timestamp failures carry **no detail**, while the three before them do.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", format!("user_id={}", self.user_id)));
        }

        if !is_valid_id(&self.post_id) {
            return Err(err("post_id", format!("post_id={}", self.post_id)));
        }

        if self.emoji_name.is_empty()
            || self.emoji_name.len() > EMOJI_NAME_MAX_LENGTH
            || !is_valid_alpha_num_hyphen_underscore_plus(&self.emoji_name)
        {
            return Err(err("emoji_name", format!("emoji_name={}", self.emoji_name)));
        }

        if self.create_at == 0 {
            return Err(err("create_at", String::new()));
        }

        if self.update_at == 0 {
            return Err(err("update_at", String::new()));
        }

        Ok(())
    }

    /// Port of `(*Reaction).PreSave` (reaction.go:48).
    ///
    /// **Preserves a non-zero `create_at`** (like `User` and `Channel`, unlike `Team`, `Session`
    /// and `Emoji`), always refreshes `update_at`, and zeroes `delete_at` — so saving a
    /// previously deleted reaction undeletes it.
    ///
    /// Go reads the clock **twice**: `create_at` is filled from one `GetMillis()` and
    /// `update_at` from another, so a brand-new reaction can end up with `update_at` a
    /// millisecond ahead of `create_at`. `Emoji::pre_save` copies one into the other instead.
    /// Reproduced as two calls, because a store that round-trips both fields would otherwise
    /// disagree with Go about whether they can differ.
    pub fn pre_save(&mut self) {
        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = get_millis();
        self.delete_at = 0;

        if self.remote_id.is_none() {
            self.remote_id = Some(String::new());
        }
    }

    /// Port of `(*Reaction).PreUpdate` (reaction.go:60).
    ///
    /// Refreshes `update_at` and materialises `remote_id`. Unlike [`Self::pre_save`] it leaves
    /// `delete_at` alone, so updating a deleted reaction keeps it deleted.
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();

        if self.remote_id.is_none() {
            self.remote_id = Some(String::new());
        }
    }

    /// Port of `(*Reaction).GetRemoteID` (reaction.go:68).
    ///
    /// Collapses nil and empty to the same `""`, so it cannot distinguish "never set" from
    /// "explicitly local". Read [`Self::remote_id`] directly when that matters.
    pub fn get_remote_id(&self) -> &str {
        self.remote_id.as_deref().unwrap_or("")
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "Reaction.IsValid",
        format!("model.reaction.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid() -> Reaction {
        Reaction {
            user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            post_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            emoji_name: "custom_emoji".into(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            delete_at: 0,
            remote_id: Some("g1ku9ozj3bhub3hs89bqu1m3gy".into()),
            channel_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
        }
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/reaction.json");
        let parsed: Reaction = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
        parsed.is_valid().unwrap();
    }

    #[test]
    fn a_nil_remote_id_serialises_as_null_not_omitted() {
        let mut r = valid();
        r.remote_id = None;
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["remote_id"], Value::Null);
        assert!(json.as_object().unwrap().contains_key("remote_id"));
    }

    #[test]
    fn a_system_emoji_name_is_a_legal_reaction() {
        // ...and an illegal custom emoji. The asymmetry is the point of this file.
        let mut r = valid();
        r.emoji_name = "grinning".into();
        r.is_valid().unwrap();
        assert!(crate::emoji::is_valid_emoji_name("grinning").is_err());
    }

    #[test]
    fn channel_id_and_delete_at_are_never_validated() {
        let mut r = valid();
        r.channel_id = "nope".into();
        r.is_valid().unwrap();

        r.channel_id = String::new();
        r.is_valid().unwrap();

        r.delete_at = 1_700_000_000_000;
        r.is_valid().unwrap();
    }

    #[test]
    fn the_emoji_name_limit_is_bytes_and_the_pattern_is_ascii_only() {
        let mut r = valid();
        r.emoji_name = "a".repeat(64);
        r.is_valid().unwrap();

        r.emoji_name = "a".repeat(65);
        assert!(r.is_valid().is_err());

        // 32 two-byte characters is exactly 64 bytes, so it clears the length check — and then
        // fails the pattern, which is ASCII-only.
        r.emoji_name = "é".repeat(32);
        assert!(r.is_valid().is_err());
    }

    #[test]
    fn pre_save_keeps_create_at_but_pre_update_keeps_delete_at() {
        let mut r = valid();
        r.delete_at = 1_700_000_000_001;
        r.pre_save();
        assert_eq!(
            r.create_at, 1_700_000_000_000,
            "create_at should be preserved"
        );
        assert_eq!(r.delete_at, 0, "pre_save undeletes");

        let mut r = valid();
        r.delete_at = 1_700_000_000_001;
        r.pre_update();
        assert_eq!(
            r.delete_at, 1_700_000_000_001,
            "pre_update leaves delete_at"
        );
    }

    #[test]
    fn both_pre_hooks_materialise_a_nil_remote_id() {
        let mut r = valid();
        r.remote_id = None;
        r.pre_save();
        assert_eq!(r.remote_id.as_deref(), Some(""));

        let mut r = valid();
        r.remote_id = None;
        r.pre_update();
        assert_eq!(r.remote_id.as_deref(), Some(""));

        // ...but an existing one is left alone.
        let mut r = valid();
        r.pre_save();
        assert_eq!(r.remote_id.as_deref(), Some("g1ku9ozj3bhub3hs89bqu1m3gy"));
    }

    #[test]
    fn get_remote_id_collapses_nil_and_empty() {
        let mut r = valid();
        r.remote_id = None;
        assert_eq!(r.get_remote_id(), "");

        r.remote_id = Some(String::new());
        assert_eq!(r.get_remote_id(), "");

        r.remote_id = Some("cluster-a".into());
        assert_eq!(r.get_remote_id(), "cluster-a");
    }
}

/// Parity tests driven by `fixtures/behaviour_reaction.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_reaction.json")).unwrap()
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let reaction: Reaction = serde_json::from_value(case["reaction"].clone()).unwrap();
            let want = case["error_id"].as_str().unwrap();
            match reaction.is_valid() {
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

    /// The evidence that lets `is_valid` reuse `is_valid_alpha_num_hyphen_underscore_plus`
    /// instead of transcribing reaction.go's inline pattern. Go ran both over these inputs; if
    /// they ever diverge, this fails and the shared validator has to be replaced with a local
    /// one.
    #[test]
    fn the_two_emoji_name_regexes_agree() {
        let oracle = oracle();
        let cases = oracle["regex_equivalence"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["in"].as_str().unwrap();
            let local = case["local"].as_bool().unwrap();
            let shared = case["shared"].as_bool().unwrap();
            assert_eq!(local, shared, "Go's two patterns disagree on {input:?}");
            assert_eq!(
                is_valid_alpha_num_hyphen_underscore_plus(input),
                local,
                "input {input:?}"
            );
        }
    }

    /// `PreSave`/`PreUpdate` read the clock, so the fixture records the invariants that survive
    /// it: which timestamps move, what happens to `delete_at`, and how `remote_id` is filled.
    #[test]
    fn pre_save_and_pre_update_match_go() {
        let oracle = oracle();
        for key in ["pre_save", "pre_update"] {
            let cases = oracle[key].as_array().unwrap();
            assert!(!cases.is_empty());
            for case in cases {
                let name = case["name"].as_str().unwrap();
                let in_create = case["in_create_at"].as_i64().unwrap();
                let in_update = case["in_update_at"].as_i64().unwrap();

                let mut reaction = Reaction {
                    user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
                    post_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
                    emoji_name: "custom_emoji".into(),
                    create_at: in_create,
                    update_at: in_update,
                    delete_at: case["in_delete_at"].as_i64().unwrap(),
                    remote_id: if case["in_remote_nil"].as_bool().unwrap() {
                        None
                    } else {
                        Some("cluster-a".into())
                    },
                    channel_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
                };

                if key == "pre_save" {
                    reaction.pre_save();
                } else {
                    reaction.pre_update();
                }

                assert_eq!(
                    in_create != 0 && reaction.create_at == in_create,
                    case["create_at_preserved"].as_bool().unwrap(),
                    "case {name}"
                );
                assert_eq!(
                    reaction.create_at != in_create,
                    case["create_at_changed"].as_bool().unwrap(),
                    "case {name}"
                );
                assert_eq!(
                    reaction.update_at != in_update,
                    case["update_at_changed"].as_bool().unwrap(),
                    "case {name}"
                );
                assert_eq!(
                    reaction.delete_at,
                    case["out_delete_at"].as_i64().unwrap(),
                    "case {name}"
                );
                assert_eq!(
                    reaction.remote_id.is_none(),
                    case["out_remote_nil"].as_bool().unwrap(),
                    "case {name}"
                );
                assert_eq!(
                    reaction.get_remote_id(),
                    case["out_remote"].as_str().unwrap(),
                    "case {name}"
                );
            }
        }
    }

    #[test]
    fn get_remote_id_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_remote_id"].as_object().unwrap();

        let probe = |remote: Option<String>| Reaction {
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
