//! Port of `model/github_release.go` — the slice of GitHub's release payload the server reads.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, NO_TRANSLATION};

/// Port of `model.GithubReleaseInfo` (github_release.go:9).
///
/// **`Url` is tagged `html_url`.** This type is decoded from *GitHub's* API response, not from a
/// Mattermost client, so the tags are GitHub's names — which is also why `created_at` and
/// `published_at` are RFC 3339 **strings** rather than this crate's usual epoch milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubReleaseInfo {
    #[serde(rename = "id")]
    pub id: i64,

    #[serde(rename = "tag_name")]
    pub tag_name: String,

    #[serde(rename = "name")]
    pub name: String,

    /// RFC 3339, GitHub's format — not epoch milliseconds.
    #[serde(rename = "created_at")]
    pub created_at: String,

    /// RFC 3339.
    #[serde(rename = "published_at")]
    pub published_at: String,

    #[serde(rename = "body")]
    pub body: String,

    /// Tagged **`html_url`**, not `url`.
    #[serde(rename = "html_url")]
    pub url: String,
}

impl GithubReleaseInfo {
    /// Port of `(*GithubReleaseInfo).IsValid` (github_release.go:19).
    ///
    /// One branch, and it is a **500**, not a 400: an id-less release means GitHub answered with
    /// something the server did not expect, which is not the caller's fault. The error id is
    /// [`NO_TRANSLATION`], so the message is never localised.
    pub fn is_valid(&self) -> AppResult {
        if self.id == 0 {
            return Err(Box::new(AppError::new(
                "GithubReleaseInfo.IsValid",
                NO_TRANSLATION,
                None,
                "empty ID",
                500,
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn github_release_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(GithubReleaseInfo, "github_release_info");
    }
}
