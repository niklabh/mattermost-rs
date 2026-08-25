//! Port of `model/compliance_post.go` — one CSV row of a compliance export.
//!
//! # `cleanComplianceStrings` is a CSV-injection guard
//!
//! Any field whose value starts (after whitespace) with `=`, `+` or `-` is prefixed with a single
//! quote, so a spreadsheet opens it as text rather than evaluating it as a formula. It is applied
//! to the nine free-text columns and **not** to the ids, timestamps or props — reproduced exactly,
//! because widening it would change every exported file and narrowing it would reintroduce the
//! vulnerability.
//!
//! # The header has 21 columns and the struct has 22 fields
//!
//! `UserType` is in the header and in [`CompliancePost::row`], but it is **derived** from `IsBot`
//! rather than stored — so the two lists line up only if you know that.

use crate::utils::get_time_for_millis;

/// Port of `model.CompliancePost` (compliance_post.go:8). No tags of any kind: this type exists
/// to be flattened into CSV.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompliancePost {
    pub team_name: String,
    pub team_display_name: String,

    pub channel_name: String,
    pub channel_display_name: String,
    pub channel_type: String,

    pub user_username: String,
    pub user_email: String,
    pub user_nickname: String,

    pub post_id: String,
    /// Epoch milliseconds.
    pub post_create_at: i64,
    pub post_update_at: i64,
    pub post_delete_at: i64,
    pub post_root_id: String,
    pub post_original_id: String,
    pub post_message: String,
    pub post_type: String,
    /// The raw props JSON text, exported verbatim and **not** cleaned.
    pub post_props: String,
    pub post_hashtags: String,
    /// Space-separated ids.
    pub post_file_ids: String,

    pub is_bot: bool,
}

/// Port of `model.CompliancePostHeader` (compliance_post.go:41) — the 21 CSV column names, in
/// order. `UserType` sits between `UserNickname` and `PostId`.
pub fn compliance_post_header() -> Vec<&'static str> {
    vec![
        "TeamName",
        "TeamDisplayName",
        "ChannelName",
        "ChannelDisplayName",
        "ChannelType",
        "UserUsername",
        "UserEmail",
        "UserNickname",
        "UserType",
        "PostId",
        "PostCreateAt",
        "PostUpdateAt",
        "PostDeleteAt",
        "PostRootId",
        "PostOriginalId",
        "PostMessage",
        "PostType",
        "PostProps",
        "PostHashtags",
        "PostFileIds",
    ]
}

/// Port of `cleanComplianceStrings` (compliance_post.go:67).
///
/// The Go pattern is `^\s*(=|\+|\-)`, so leading whitespace before the operator still triggers
/// the guard. `\s` in Go's RE2 is ASCII `[\t\n\f\r ]` — **not** Unicode whitespace — so a
/// value beginning with a non-breaking space followed by `=` is *not* quoted. Reproduced with
/// `is_ascii_whitespace`, which is the same set plus `\x0b`… and that is where the two differ:
/// Rust's `is_ascii_whitespace` excludes `\x0b` too, so the sets are identical.
pub fn clean_compliance_strings(input: &str) -> String {
    let trimmed = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.starts_with(['=', '+', '-']) {
        let mut out = String::with_capacity(input.len() + 1);
        out.push('\'');
        out.push_str(input);
        return out;
    }
    input.to_string()
}

/// Go's `time.Unix(0, millis * 1e6).Format(time.RFC3339)` — the **local** time zone, seconds
/// precision, `Z` for a zero offset.
///
/// A millisecond value that cannot be represented yields the empty string rather than a panic;
/// Go cannot reach that state from an `int64` of milliseconds.
fn format_rfc3339_millis(millis: i64) -> String {
    match get_time_for_millis(millis) {
        Some(t) => t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => String::new(),
    }
}

impl CompliancePost {
    /// Port of `(*CompliancePost).Row` (compliance_post.go:74).
    ///
    /// Three derived columns:
    ///
    /// - `PostUpdateAt` is **empty when it equals `PostCreateAt`**, so an unedited post shows no
    ///   update time;
    /// - `PostDeleteAt` is empty unless positive;
    /// - `UserType` is `bot` or `user`, derived from `is_bot`.
    pub fn row(&self) -> Vec<String> {
        let post_delete_at = if self.post_delete_at > 0 {
            format_rfc3339_millis(self.post_delete_at)
        } else {
            String::new()
        };

        let post_update_at = if self.post_update_at != self.post_create_at {
            format_rfc3339_millis(self.post_update_at)
        } else {
            String::new()
        };

        let user_type = if self.is_bot { "bot" } else { "user" };

        vec![
            clean_compliance_strings(&self.team_name),
            clean_compliance_strings(&self.team_display_name),
            clean_compliance_strings(&self.channel_name),
            clean_compliance_strings(&self.channel_display_name),
            clean_compliance_strings(&self.channel_type),
            clean_compliance_strings(&self.user_username),
            clean_compliance_strings(&self.user_email),
            clean_compliance_strings(&self.user_nickname),
            user_type.to_string(),
            self.post_id.clone(),
            format_rfc3339_millis(self.post_create_at),
            post_update_at,
            post_delete_at,
            self.post_root_id.clone(),
            self.post_original_id.clone(),
            clean_compliance_strings(&self.post_message),
            self.post_type.clone(),
            self.post_props.clone(),
            self.post_hashtags.clone(),
            self.post_file_ids.clone(),
        ]
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// `Row` is a CSV line, so column **order and count** are the whole contract, and
    /// `cleanComplianceStrings` is a CSV-injection guard: a cell starting `=`, `+` or `-` gets a
    /// leading `'`. The corpus covers each trigger character, the whitespace variants Go does
    /// *not* strip first, and the two fields the guard is deliberately not applied to.
    #[test]
    fn header_and_row_match_go() {
        let oracle = oracle();
        let cases = oracle["compliance_post_row"].as_array().unwrap();

        let expected_header: Vec<String> =
            serde_json::from_value(cases[0]["header"].clone()).unwrap();
        assert_eq!(compliance_post_header(), expected_header);

        let base = CompliancePost {
            team_name: "core".to_string(),
            team_display_name: "Core".to_string(),
            channel_name: "town-square".to_string(),
            channel_display_name: "Town Square".to_string(),
            channel_type: "O".to_string(),
            user_username: "parity-user".to_string(),
            user_email: "parity@example.com".to_string(),
            user_nickname: "PU".to_string(),
            post_id: "p1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
            post_create_at: 1_700_000_000_000,
            post_update_at: 1_700_000_000_000,
            post_message: "hello".to_string(),
            post_props: r#"{"key":"value"}"#.to_string(),
            post_hashtags: "#tag".to_string(),
            post_file_ids: "f1 f2".to_string(),
            ..Default::default()
        };
        let post = |mut mutate: Box<dyn FnMut(&mut CompliancePost)>| {
            let mut p = base.clone();
            mutate(&mut p);
            p
        };

        let inputs: Vec<(&str, CompliancePost)> = vec![
            ("plain", post(Box::new(|_| {}))),
            (
                "edited",
                post(Box::new(|p| p.post_update_at = 1_700_000_005_000)),
            ),
            (
                "deleted",
                post(Box::new(|p| p.post_delete_at = 1_700_000_009_000)),
            ),
            ("bot", post(Box::new(|p| p.is_bot = true))),
            (
                "formula_message",
                post(Box::new(|p| p.post_message = "=SUM(A1:A2)".to_string())),
            ),
            (
                "plus_message",
                post(Box::new(|p| p.post_message = "+1".to_string())),
            ),
            (
                "minus_message",
                post(Box::new(|p| p.post_message = "-1".to_string())),
            ),
            (
                "leading_space_formula",
                post(Box::new(|p| p.post_message = "  =cmd".to_string())),
            ),
            (
                "tab_formula",
                post(Box::new(|p| p.post_message = "\t=cmd".to_string())),
            ),
            (
                "nbsp_formula",
                post(Box::new(|p| p.post_message = "\u{a0}=cmd".to_string())),
            ),
            (
                "formula_username",
                post(Box::new(|p| p.user_username = "=evil".to_string())),
            ),
            (
                "formula_props_untouched",
                post(Box::new(|p| p.post_props = "=notcleaned".to_string())),
            ),
            (
                "fractional_millis",
                post(Box::new(|p| {
                    p.post_create_at = 1_700_000_000_123;
                    p.post_update_at = 1_700_000_000_456;
                })),
            ),
        ];
        assert_eq!(
            cases.len(),
            inputs.len() + 1,
            "header row plus one per case"
        );

        for (case, (name, p)) in cases[1..].iter().zip(inputs.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            let expected: Vec<String> = serde_json::from_value(case["row"].clone()).unwrap();
            assert_eq!(p.row(), expected, "Row({name})");
        }
    }
}
