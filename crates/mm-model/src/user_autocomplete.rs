//! Port of `model/user_autocomplete.go` (user_autocomplete.go:1–19) — **whole file**.
//!
//! Three structs, six fields, every one of them `[]*User`. No methods. The types are identical
//! and the **tags** are not — and they differ *within* the file:
//!
//! | field | tag | nil vs empty |
//! |---|---|---|
//! | `UserAutocompleteInChannel.in_channel` | plain | distinguishable |
//! | `UserAutocompleteInChannel.out_of_channel` | plain | distinguishable |
//! | `UserAutocompleteInTeam.in_team` | plain | distinguishable |
//! | `UserAutocomplete.users` | plain | distinguishable |
//! | `UserAutocomplete.out_of_channel` | **`omitempty`** | collapsed |
//! | `UserAutocomplete.agents` | **`omitempty`** | collapsed |
//!
//! So `out_of_channel` appears in two of the three structs under **different rules**, which makes
//! this the clearest case in the tree for reading the tag per field rather than per type.
//!
//! # The two rules produce two different Rust types
//!
//! Without `omitempty`, nil is `null`, empty is `[]`, and the key is always present — three
//! states, so [`Option<Vec<User>>`].
//!
//! With `omitempty`, Go drops a nil slice **and** an empty one, so the two are indistinguishable
//! on the wire and the faithful type is a plain [`Vec`] with `skip_serializing_if =
//! "Vec::is_empty"`. An `Option` there would invent a distinction Go cannot express — measured:
//! `optional_nil` and `optional_empty` in the corpus produce **byte-identical** documents.
//!
//! Getting this backwards is invisible locally, because the type still round-trips through its
//! own serializer. It surfaces as a missing or spurious key at a client.
//!
//! # `[]*User` is [D-033] six more times
//!
//! A `null` element is a legal document in Go — it stores the nil pointer and re-emits it — and
//! fails our decode. All six fields are affected and each is driven individually, because the
//! entry's table cites fields rather than generalising from one.

use serde::{Deserialize, Serialize};

use crate::user::User;

/// Port of `model.UserAutocompleteInChannel` (user_autocomplete.go:6).
///
/// Neither field carries `omitempty`, so both keys are always present and nil is `null`.
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAutocompleteInChannel {
    #[serde(rename = "in_channel")]
    pub in_channel: Option<Vec<User>>,

    /// **Not** the same field as [`UserAutocomplete::out_of_channel`] despite the shared key:
    /// that one has `omitempty` and this one does not.
    #[serde(rename = "out_of_channel")]
    pub out_of_channel: Option<Vec<User>>,
}

/// Port of `model.UserAutocompleteInTeam` (user_autocomplete.go:11).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAutocompleteInTeam {
    #[serde(rename = "in_team")]
    pub in_team: Option<Vec<User>>,
}

/// Port of `model.UserAutocomplete` (user_autocomplete.go:15).
///
/// The only type in the file that mixes the two tag rules — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAutocomplete {
    /// No `omitempty`, so nil is `null` and empty is `[]`.
    #[serde(rename = "users")]
    pub users: Option<Vec<User>>,

    /// `omitempty`, so Go drops nil **and** empty alike. A plain `Vec` rather than an `Option`,
    /// because the two states are not distinguishable on the wire and an `Option` would invent a
    /// difference Go cannot express.
    #[serde(rename = "out_of_channel", skip_serializing_if = "Vec::is_empty")]
    pub out_of_channel: Vec<User>,

    /// Same rule as [`Self::out_of_channel`].
    #[serde(rename = "agents", skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<User>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    fn user(id: &str, username: &str) -> User {
        User {
            id: id.into(),
            create_at: 100,
            update_at: 200,
            username: username.into(),
            email: format!("{username}@example.com"),
            roles: "system_user".into(),
            locale: "en".into(),
            ..Default::default()
        }
    }

    /// Without `omitempty`, all three states are on the wire.
    #[test]
    fn a_plain_slice_distinguishes_nil_from_empty() {
        let nil = UserAutocompleteInTeam { in_team: None };
        let empty = UserAutocompleteInTeam {
            in_team: Some(Vec::new()),
        };

        assert_eq!(go_json_marshal(&nil).unwrap(), r#"{"in_team":null}"#);
        assert_eq!(go_json_marshal(&empty).unwrap(), r#"{"in_team":[]}"#);
        assert_ne!(nil, empty);
    }

    /// With `omitempty`, they collapse — which is why the field is a `Vec` and not an `Option`.
    #[test]
    fn an_omitempty_slice_collapses_nil_and_empty() {
        let empty = UserAutocomplete::default();
        let json = go_json_marshal(&empty).unwrap();

        assert_eq!(json, r#"{"users":null}"#);
        assert!(!json.contains("out_of_channel"), "{json}");
        assert!(!json.contains("agents"), "{json}");
    }

    /// The two rules side by side in one document: one nil slice is `null`, two are simply gone.
    #[test]
    fn the_two_rules_appear_in_the_same_object() {
        let value = UserAutocomplete {
            users: Some(vec![user("6bdz674pgq767e4jx75w4pf57a", "alice")]),
            out_of_channel: Vec::new(),
            agents: Vec::new(),
        };
        let json = go_json_marshal(&value).unwrap();

        assert!(json.starts_with(r#"{"users":[{"#), "{json}");
        assert!(!json.contains("out_of_channel"), "{json}");
        assert!(!json.contains("agents"), "{json}");
    }

    /// `out_of_channel` is two different fields with two different rules.
    #[test]
    fn out_of_channel_behaves_differently_in_the_two_types() {
        let in_channel = UserAutocompleteInChannel::default();
        assert!(
            go_json_marshal(&in_channel)
                .unwrap()
                .contains(r#""out_of_channel":null"#)
        );

        let autocomplete = UserAutocomplete::default();
        assert!(
            !go_json_marshal(&autocomplete)
                .unwrap()
                .contains("out_of_channel")
        );
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: UserAutocomplete = serde_json::from_str(r#"{"users":[]}"#).unwrap();
        assert_eq!(got.users, Some(Vec::new()));
        assert!(got.out_of_channel.is_empty() && got.agents.is_empty());
    }
}

/// Serialization parity against the reflection-populated fixtures, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixtures() {
        let raw = include_str!("../../../fixtures/user_autocomplete_in_channel.json");
        let decoded: UserAutocompleteInChannel = serde_json::from_str(raw).unwrap();
        assert!(decoded.in_channel.as_ref().is_some_and(|v| !v.is_empty()));
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/user_autocomplete_in_team.json");
        let decoded: UserAutocompleteInTeam = serde_json::from_str(raw).unwrap();
        assert!(decoded.in_team.as_ref().is_some_and(|v| !v.is_empty()));
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );

        let raw = include_str!("../../../fixtures/user_autocomplete.json");
        let decoded: UserAutocomplete = serde_json::from_str(raw).unwrap();
        assert!(decoded.users.as_ref().is_some_and(|v| !v.is_empty()));
        assert!(!decoded.out_of_channel.is_empty() && !decoded.agents.is_empty());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_user_autocomplete.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_user_autocomplete.json"
        ))
        .unwrap()
    }

    fn keys(oracle: &Value, section: &str) -> Vec<String> {
        oracle[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();
        assert_eq!(
            keys(&oracle, "in_channel_keys"),
            ["in_channel", "out_of_channel"]
        );
        assert_eq!(keys(&oracle, "in_team_keys"), ["in_team"]);
        assert_eq!(
            keys(&oracle, "autocomplete_keys"),
            ["users", "out_of_channel", "agents"]
        );
    }

    /// Both keys always present, nil as `null` and empty as `[]`.
    #[test]
    fn the_in_channel_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["in_channel_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the in-channel corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: UserAutocompleteInChannel =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");

            // No omitempty anywhere in this type, so both keys are always there.
            assert!(case["in_channel_present"].as_bool().unwrap(), "{name}");
            assert!(case["out_of_channel_present"].as_bool().unwrap(), "{name}");

            assert_eq!(
                decoded.in_channel.is_none(),
                case["in_channel_is_null"].as_bool().unwrap(),
                "{name}: in_channel"
            );
            assert_eq!(
                decoded.out_of_channel.is_none(),
                case["out_of_channel_is_null"].as_bool().unwrap(),
                "{name}: out_of_channel"
            );
        }
    }

    #[test]
    fn the_in_team_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["in_team_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 3, "the in-team corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: UserAutocompleteInTeam =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");
            assert!(case["in_team_present"].as_bool().unwrap(), "{name}");
            assert_eq!(
                decoded.in_team.is_none(),
                case["in_team_is_null"].as_bool().unwrap(),
                "{name}"
            );
        }
    }

    /// The type that mixes both rules, and the reason the module exists.
    #[test]
    fn the_autocomplete_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["autocomplete_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 8, "the autocomplete corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["json"].as_str().unwrap();
            let decoded: UserAutocomplete =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "{name}");

            // `users` has no omitempty: always present, `null` when nil.
            assert!(
                case["users_present"].as_bool().unwrap(),
                "{name}: users key"
            );
            assert_eq!(
                decoded.users.is_none(),
                case["users_is_null"].as_bool().unwrap(),
                "{name}: users"
            );

            // The other two do: present exactly when non-empty.
            assert_eq!(
                !decoded.out_of_channel.is_empty(),
                case["out_of_channel_present"].as_bool().unwrap(),
                "{name}: out_of_channel key"
            );
            assert_eq!(
                !decoded.agents.is_empty(),
                case["agents_present"].as_bool().unwrap(),
                "{name}: agents key"
            );
        }
    }

    /// The measurement that decides `Vec` over `Option<Vec>`: with `omitempty`, a nil slice and an
    /// empty one produce the **same bytes**. Asserted on Go's own output, so if upstream ever
    /// dropped `omitempty` the two documents would differ and this would fail.
    #[test]
    fn omitempty_makes_nil_and_empty_indistinguishable() {
        let oracle = oracle();
        let cases = oracle["autocomplete_wire"].as_array().unwrap();

        let find = |name: &str| {
            cases
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("{name} is missing from the corpus"))
        };

        let from_nil = find("optional_nil");
        let from_empty = find("optional_empty");

        // Go was handed nil in one and an empty slice in the other...
        assert!(from_nil["out_of_channel_nil"].as_bool().unwrap());
        assert!(!from_empty["out_of_channel_nil"].as_bool().unwrap());
        // ...and produced identical bytes.
        assert_eq!(
            from_nil["json"].as_str().unwrap(),
            from_empty["json"].as_str().unwrap(),
            "omitempty stopped collapsing nil and empty"
        );

        // Which is what makes a single Rust value able to stand for both.
        let ours: UserAutocomplete =
            serde_json::from_str(from_nil["json"].as_str().unwrap()).unwrap();
        assert!(ours.out_of_channel.is_empty() && ours.agents.is_empty());
    }

    /// [D-033] on all six fields, driven individually so the entry's table can cite each.
    #[test]
    fn a_nil_element_is_legal_in_go_and_fails_our_decode() {
        let oracle = oracle();
        let cases = oracle["nil_elements"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "one case per field");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            // Go kept the nil element and re-emitted it.
            assert!(
                case["element_nil"].as_bool().unwrap(),
                "{name}: Go dropped it"
            );
            assert!(
                case["json_after"].as_str().unwrap().contains("null"),
                "{name}"
            );

            let doc = case["in"].as_str().unwrap();
            let failed = match name {
                "in_team" => serde_json::from_str::<UserAutocompleteInTeam>(doc).is_err(),
                "in_channel" | "in_channel_out_of_channel" => {
                    serde_json::from_str::<UserAutocompleteInChannel>(doc).is_err()
                }
                _ => serde_json::from_str::<UserAutocomplete>(doc).is_err(),
            };
            assert!(failed, "{name}: expected the documented [D-033] failure");
        }
    }
}
