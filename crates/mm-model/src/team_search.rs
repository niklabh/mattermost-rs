//! Port of `model/team_search.go` — whole file: thirteen fields in three tag conventions, plus
//! `IsPaginated`.
//!
//! # Three conventions in one struct
//!
//! | shape | fields | document |
//! |---|---|---|
//! | `string`, no `omitempty` | `term` | always present, `""` when empty |
//! | `*T` with `omitempty` | seven | absent when nil; **`0` and `false` still appear** |
//! | `json:"-"` | four | never on the wire, either direction |
//!
//! The middle row is [D-075]'s shape and the reason those seven are `Option<T>`: a pointer to
//! zero is a *set* value and Go does not drop it, so `Some(0)` and `None` are different documents.
//!
//! # The four dashed fields are a security boundary, not tidiness
//!
//! `IncludePolicyEnforced` carries Go's own comment: "Server-controlled (never decoded from a
//! request) so a caller can't surface governed teams it isn't entitled to see." `#[serde(skip)]`
//! reproduces both directions — a request body naming the field is ignored, and the value never
//! leaks outward. The corpus offers all four under both their Go field names and their plausible
//! snake_case spellings to prove none of them decodes.
//!
//! # `IsPaginated` needs both pointers, and a pointer to zero counts
//!
//! `Page != nil && PerPage != nil`. So `page=0, per_page=0` **is** paginated — page zero is the
//! first page, not a missing one — while `page=5` alone is not. Measured across all four
//! nil-ness combinations.

use serde::{Deserialize, Serialize};

/// Port of `model.TeamSearch` (team_search.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamSearch {
    /// No `omitempty` — the only tagged field that is always present.
    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "page", skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,

    #[serde(rename = "per_page", skip_serializing_if = "Option::is_none")]
    pub per_page: Option<i64>,

    #[serde(rename = "allow_open_invite", skip_serializing_if = "Option::is_none")]
    pub allow_open_invite: Option<bool>,

    #[serde(rename = "group_constrained", skip_serializing_if = "Option::is_none")]
    pub group_constrained: Option<bool>,

    #[serde(
        rename = "include_group_constrained",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_group_constrained: Option<bool>,

    #[serde(rename = "policy_id", skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,

    #[serde(
        rename = "exclude_policy_constrained",
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_policy_constrained: Option<bool>,

    /// `json:"-"` — server-controlled.
    #[serde(skip)]
    pub include_policy_id: Option<bool>,

    /// `json:"-"` — server-controlled. Go's comment calls out that a caller must not be able to
    /// set this: it widens a listing to teams governed by an access-control policy.
    #[serde(skip)]
    pub include_policy_enforced: Option<bool>,

    /// `json:"-"` — server-controlled.
    #[serde(skip)]
    pub include_deleted: Option<bool>,

    /// `json:"-"` — server-controlled.
    #[serde(skip)]
    pub team_type: Option<String>,
}

impl TeamSearch {
    /// Port of `(*TeamSearch).IsPaginated` (team_search.go:26).
    ///
    /// Both pointers, not either — and it is the *presence* that counts, so a page of zero is
    /// paginated.
    pub fn is_paginated(&self) -> bool {
        self.page.is_some() && self.per_page.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_small_types.json")).unwrap()
    }

    #[test]
    fn serialization_parity_with_the_fixture() {
        let raw = include_str!("../../../fixtures/team_search.json");
        let search: TeamSearch = serde_json::from_str(raw).unwrap();
        let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&search).unwrap(), expected);
    }

    /// Seven `omitempty` pointers plus four dashed fields, so the zero value is one key.
    #[test]
    fn the_zero_value_is_term_alone() {
        assert_eq!(
            serde_json::to_string(&TeamSearch::default()).unwrap(),
            r#"{"term":""}"#
        );
        assert_eq!(
            corpus()["zero_values"]["team_search"].as_str(),
            Some(r#"{"term":""}"#)
        );
    }

    /// **Go's own answers for all four nil-ness combinations**, including the wire form.
    #[test]
    fn go_parity_is_paginated_and_the_wire_form() {
        let corpus = corpus();
        let rows = corpus["team_search"].as_array().unwrap();

        let build = |name: &str| -> TeamSearch {
            let (page, per_page) = match name {
                "both_nil" => (None, None),
                "page_only" => (Some(5), None),
                "per_page_only" => (None, Some(5)),
                "both_set" => (Some(5), Some(5)),
                "both_zero" => (Some(0), Some(0)),
                "page_zero_per_page_nil" => (Some(0), None),
                other => panic!("unknown corpus row {other}"),
            };
            TeamSearch {
                term: "term".to_owned(),
                page,
                per_page,
                ..Default::default()
            }
        };

        let mut saw_zero_paginated = false;
        for row in rows {
            let name = row["name"].as_str().unwrap();
            let ours = build(name);
            assert_eq!(
                ours.is_paginated(),
                row["is_paginated"].as_bool().unwrap(),
                "is_paginated for {name}"
            );
            let expected: serde_json::Value =
                serde_json::from_str(row["wire"].as_str().unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(&ours).unwrap(),
                expected,
                "wire {name}"
            );

            if name == "both_zero" {
                assert!(ours.is_paginated(), "page zero is the first page");
                saw_zero_paginated = true;
            }
        }
        assert!(
            saw_zero_paginated,
            "the corpus must contain the pointer-to-zero case or `&&` and a truthiness check \
             are indistinguishable"
        );
    }

    /// **The four `json:"-"` fields never decode**, under either spelling. This is the security
    /// property Go's comment describes, not a formatting preference.
    #[test]
    fn go_parity_the_dashed_fields_never_decode() {
        let corpus = corpus();
        let row = corpus["team_search_wire"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "dashed_fields")
            .expect("the corpus offers all four dashed fields");

        let doc = row["in"].as_str().unwrap();
        let decoded: TeamSearch = serde_json::from_str(doc).unwrap();

        assert!(decoded.include_policy_id.is_none());
        assert!(decoded.include_policy_enforced.is_none());
        assert!(decoded.include_deleted.is_none());
        assert!(decoded.team_type.is_none());

        // And Go agrees, field by field.
        assert_eq!(row["include_policy_id_nil"].as_bool(), Some(true));
        assert_eq!(row["include_policy_enforced_nil"].as_bool(), Some(true));
        assert_eq!(row["include_deleted_nil"].as_bool(), Some(true));
        assert_eq!(row["team_type_nil"].as_bool(), Some(true));

        // Nor do they leak outward, whatever they hold.
        let loaded = TeamSearch {
            include_policy_enforced: Some(true),
            team_type: Some("O".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&loaded).unwrap(),
            r#"{"term":""}"#,
            "a server-controlled field must not reach a client"
        );
    }

    /// Every decode probe in the corpus, re-encoded and compared. Catches a tag typo in any of
    /// the eight wire fields at once.
    #[test]
    fn go_parity_every_decode_probe_round_trips() {
        let corpus = corpus();
        for row in corpus["team_search_wire"].as_array().unwrap() {
            let name = row["name"].as_str().unwrap();
            let decoded: TeamSearch = serde_json::from_str(row["in"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let expected: serde_json::Value =
                serde_json::from_str(row["out"].as_str().unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&decoded).unwrap(), expected, "{name}");
            assert_eq!(
                decoded.is_paginated(),
                row["is_paginated"].as_bool().unwrap(),
                "{name}"
            );
        }
    }
}
