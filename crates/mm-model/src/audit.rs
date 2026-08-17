//! Port of `model/audit.go` (14 lines) **and** `model/audits.go` (14) — **both whole**.
//!
//! [`Audit`] is a plain tagged struct with nothing to measure beyond its keys. [`Audits`] is where
//! the content is, and its `Etag` is unlike every other list etag in the tree.
//!
//! ```go
//! func (o Audits) Etag() string {
//!     if len(o) > 0 { return Etag(o[0].CreateAt) }   // the first is always the most current
//!     return ""
//! }
//! ```
//!
//! # Three things that differ from every other list `Etag`
//!
//! 1. **An empty list returns `""`, not a versioned etag.**
//!    [`crate::channel_list::ChannelList::etag`] on an empty list returns `11.11.0.0.0.0.0`; this
//!    returns the empty string, which is not an etag at all. A caller passing it straight into an
//!    `ETag:` header sends an empty header.
//! 2. **It reads element `[0]` rather than scanning for a maximum.** Every other list etag walks
//!    the whole slice. This one trusts the caller's ordering — Go's comment asserts "the first in
//!    the list is always the most current" rather than the code establishing it. Measured: an
//!    ascending list yields the etag of its **oldest** row, and an unsorted list yields neither
//!    the newest nor the oldest.
//! 3. **One component, not four.** `Etag(o[0].CreateAt)` passes a single value, so the result is
//!    `<version>.<create_at>`.
//!
//! Point 2 is the one worth carrying forward: the correctness of this etag is a property of the
//! **query that produced the list**, not of the function. Whoever ports the audit store has to
//! preserve the `ORDER BY CreateAt DESC` or the cache silently stops invalidating.
//!
//! # `Audits` is `[]Audit`, not `[]*Audit`
//!
//! The first list in the tree whose element is a value rather than a pointer, so there is **no**
//! [D-033] instance here. It has its own divergence instead: `[null]` gives Go a **zero-valued
//! `Audit`** rather than an error, which is [D-075] widened from `[]string` to any non-pointer
//! element.

use serde::{Deserialize, Serialize};

use crate::utils::etag;

/// Port of `model.Audit` (audit.go:6).
///
/// No `omitempty` anywhere, so the zero value is seven keys rather than `{}`, and nothing is
/// validated — `ip_address` is a plain string that holds IPv4, IPv6 or anything else.
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audit {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "action")]
    pub action: String,

    #[serde(rename = "extra_info")]
    pub extra_info: String,

    #[serde(rename = "ip_address")]
    pub ip_address: String,

    #[serde(rename = "session_id")]
    pub session_id: String,
}

/// Port of `model.Audits` (audits.go:6) — Go's `[]Audit`.
///
/// A `#[serde(transparent)]` newtype, so the JSON is a bare array. The element is a **value**, not
/// a pointer — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Audits(pub Vec<Audit>);

impl Audits {
    /// Port of `(Audits).Etag` (audits.go:8).
    ///
    /// **Returns the empty string for an empty list**, not a versioned etag — the one list etag in
    /// the crate that does. And it reads `self.0[0]` rather than scanning, so the answer is only
    /// "the newest audit" when the caller sorted descending. Both are Go's; see the module docs.
    pub fn etag(&self) -> String {
        match self.0.first() {
            Some(first) => etag(&[&first.create_at]),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    fn audit(id: &str, create_at: i64) -> Audit {
        Audit {
            id: id.into(),
            create_at,
            user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            action: "/api/v4/users/login".into(),
            extra_info: "success".into(),
            ip_address: "10.0.0.1".into(),
            session_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
        }
    }

    /// The one list etag in the crate that is not a versioned string when the list is empty.
    #[test]
    fn an_empty_list_etags_to_the_empty_string() {
        assert_eq!(Audits::default().etag(), "");
        assert_eq!(Audits(Vec::new()).etag(), "");
    }

    /// It reads element zero. For an ascending list that is the **oldest** row.
    #[test]
    fn the_etag_follows_position_not_recency() {
        let descending = Audits(vec![audit("a", 300), audit("b", 200), audit("c", 100)]);
        assert_eq!(descending.etag(), "11.11.0.300");

        let ascending = Audits(vec![audit("a", 100), audit("b", 200), audit("c", 300)]);
        assert_eq!(ascending.etag(), "11.11.0.100", "the oldest row wins");

        let unsorted = Audits(vec![audit("a", 200), audit("b", 300), audit("c", 100)]);
        assert_eq!(unsorted.etag(), "11.11.0.200", "neither newest nor oldest");
    }

    /// One component after the version, where the channel lists produce four.
    #[test]
    fn the_etag_has_a_single_component() {
        let one = Audits(vec![audit("a", 1700000000000)]);
        assert_eq!(one.etag(), "11.11.0.1700000000000");
        assert_eq!(one.etag().matches('.').count(), 3, "11 . 11 . 0 . value");
    }

    #[test]
    fn the_zero_audit_is_seven_keys() {
        assert_eq!(
            go_json_marshal(&Audit::default()).unwrap(),
            r#"{"id":"","create_at":0,"user_id":"","action":"","extra_info":"","ip_address":"","session_id":""}"#
        );
    }

    #[test]
    fn the_list_is_a_bare_array() {
        let audits = Audits(vec![audit("a", 1)]);
        let json = go_json_marshal(&audits).unwrap();
        assert!(json.starts_with(r#"[{"id":"a","#), "{json}");
        assert_eq!(go_json_marshal(&Audits::default()).unwrap(), "[]");
    }

    /// Nothing validates, including the address field.
    #[test]
    fn nothing_is_validated() {
        let ipv6 = Audit {
            ip_address: "2001:db8::1".into(),
            ..Default::default()
        };
        let back: Audit = serde_json::from_str(&go_json_marshal(&ipv6).unwrap()).unwrap();
        assert_eq!(ipv6, back);
    }
}

/// Serialization parity against `fixtures/audit.json` — the reflection-populated oracle, every
/// field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/audit.json");
        let decoded: Audit = serde_json::from_str(raw).unwrap();
        assert!(!decoded.id.is_empty() && decoded.create_at != 0);
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_audit.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_audit.json")).unwrap()
    }

    #[test]
    fn the_wire_keys_match_go() {
        let oracle = oracle();
        let theirs: Vec<&str> = oracle["audit_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(
            theirs,
            [
                "id",
                "create_at",
                "user_id",
                "action",
                "extra_info",
                "ip_address",
                "session_id"
            ]
        );
    }

    #[test]
    fn the_wire_formats_match_go() {
        let oracle = oracle();

        let cases = oracle["audit_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 7, "the audit corpus changed size");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            let want = case["json"].as_str().unwrap();
            let decoded: Audit =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "audit/{name}");
        }

        let cases = oracle["audits_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 4, "the audits corpus changed size");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            let want = case["json"].as_str().unwrap();

            if case["nil"].as_bool().unwrap() {
                // Go's nil slice is `null`; our newtype wraps a `Vec` with no `Option`, so it has
                // no nil state. The Go producer always appends onto a fresh slice.
                assert_eq!(want, "null", "{name}");
                continue;
            }

            let decoded: Audits =
                serde_json::from_str(want).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(go_json_marshal(&decoded).unwrap(), want, "audits/{name}");
            assert_eq!(
                decoded.0.len() as u64,
                case["len"].as_u64().unwrap(),
                "{name}"
            );
        }
    }

    /// The file's content. Three properties, each asserted against Go's own answer rather than
    /// against a reading of the four-line function.
    #[test]
    fn the_etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["etag"].as_array().unwrap();
        assert_eq!(cases.len(), 10, "the etag corpus changed size");

        let (mut empties, mut first_not_max) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            // Rebuilt from the recorded create_at values rather than from a Rust literal, so the
            // corpus drives the input as well as the expectation.
            let len = case["len"].as_u64().unwrap() as usize;
            let audits = if len == 0 {
                Audits(Vec::new())
            } else {
                // Only element [0] can affect the answer, but the rest must exist for the
                // position-versus-maximum assertion below to mean anything.
                let first = case["first_create_at"].as_i64().unwrap();
                let max = case["max_create_at"].as_i64().unwrap();
                let mut rows = vec![Audit {
                    create_at: first,
                    ..Default::default()
                }];
                for _ in 1..len {
                    rows.push(Audit {
                        create_at: max,
                        ..Default::default()
                    });
                }
                Audits(rows)
            };

            assert_eq!(audits.etag(), case["etag"].as_str().unwrap(), "{name}");

            if case["is_empty_string"].as_bool().unwrap() {
                empties += 1;
                assert_eq!(len, 0, "{name}: only an empty list may etag to \"\"");
                assert!(
                    audits.etag().is_empty(),
                    "{name}: we produced a versioned etag where Go produced nothing"
                );
                continue;
            }

            // The property the four-line function hides: the answer tracks position, not recency.
            if !case["first_is_max"].as_bool().unwrap() {
                first_not_max += 1;
                let max = case["max_create_at"].as_i64().unwrap();
                assert!(
                    !audits.etag().ends_with(&format!(".{max}")),
                    "{name}: the etag tracked the maximum — the scan-versus-position \
                     behaviour changed"
                );
            }
        }

        assert_eq!(empties, 2, "the empty-list cases changed count");
        assert_eq!(
            first_not_max, 3,
            "the corpus stopped exercising an unsorted list"
        );
    }

    /// `Audits` is `[]Audit`, so there is no [D-033] here — but `[null]` is still a divergence,
    /// and a different one: Go builds a **zero-valued `Audit`** where a pointer slice would have
    /// stored nil. That is [D-075] widened from `[]string` to any non-pointer element.
    #[test]
    fn a_null_element_becomes_a_zero_audit_in_go() {
        let oracle = oracle();
        let case = &oracle["null_element"];
        assert!(!case["panicked"].as_bool().unwrap());

        assert!(
            !case["element_is_pointer"].as_bool().unwrap(),
            "Audits became a pointer slice — [D-033] now applies and this test is the wrong one"
        );
        assert!(case["ok"].as_bool().unwrap(), "Go used to accept [null]");
        assert!(
            case["json_after"]
                .as_str()
                .unwrap()
                .starts_with(r#"[{"id":"","create_at":0"#),
            "Go stopped zero-filling: {}",
            case["json_after"]
        );

        assert!(
            serde_json::from_str::<Audits>("[null]").is_err(),
            "expected the documented [D-075] divergence"
        );
    }
}
