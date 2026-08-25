//! Port of `model/temporary_post.go` — the message body of an ephemeral post, stored apart from
//! the post itself so it can be purged on expiry.

use serde::{Deserialize, Serialize};

use crate::post::Post;
use crate::utils::StringArray;

/// Port of `model.TemporaryPost` (temporary_post.go:5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemporaryPost {
    #[serde(rename = "id")]
    pub id: String,

    /// The post's `type`, copied so a purge can tell system posts apart without a join.
    #[serde(rename = "type")]
    pub type_: String,

    /// Epoch milliseconds.
    #[serde(rename = "expire_at")]
    pub expire_at: i64,

    #[serde(rename = "message")]
    pub message: String,

    #[serde(rename = "file_ids")]
    pub file_ids: Option<StringArray>,
}

impl TemporaryPost {
    /// Port of `(*TemporaryPost).IsValid` (temporary_post.go:13).
    ///
    /// **One branch.** It does not check `expire_at`, so a temporary post with no expiry — which
    /// never gets purged — is valid. Returns a bare `error` in Go, not an `*AppError`, so it
    /// carries no status code and no i18n id.
    pub fn is_valid(&self) -> Result<(), TemporaryPostError> {
        if self.id.is_empty() {
            return Err(TemporaryPostError::MissingId);
        }

        Ok(())
    }
}

/// The one error `temporary_post.go` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TemporaryPostError {
    #[error("id is required")]
    MissingId,
}

/// Port of `model.CreateTemporaryPost` (temporary_post.go:21).
///
/// **Destructive on the post**: the message and file ids are *moved* out, and the post is left
/// with an empty message and an **empty** (not nil) file-id list — so it still marshals as `[]`,
/// not `null`. Go returns the same pointer it was handed; here the mutation is visible in the
/// signature.
///
/// Go's third return value is always `nil`, so there is no `Result` to propagate.
pub fn create_temporary_post(post: &mut Post, expire_at: i64) -> TemporaryPost {
    let temporary_post = TemporaryPost {
        id: post.id.clone(),
        type_: post.post_type.clone(),
        expire_at,
        message: std::mem::take(&mut post.message),
        file_ids: post.file_ids.take(),
    };

    post.file_ids = Some(StringArray::new());

    temporary_post
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
    fn temporary_post_round_trips_the_fixture() {
        assert_fixture_round_trips!(TemporaryPost, "temporary_post");
    }
}
