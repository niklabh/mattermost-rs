//! Port of `model/upload_session.go` — a resumable file upload in progress.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;
use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id};

/// Port of `model.UploadType` (upload_session.go:11) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UploadType(pub String);

impl UploadType {
    pub const ATTACHMENT: &'static str = "attachment";
    pub const IMPORT: &'static str = "import";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Port of `(UploadType).IsValid` (upload_session.go:76) — a closed set of two.
    pub fn is_valid(&self) -> Result<(), UploadSessionError> {
        match self.0.as_str() {
            Self::ATTACHMENT | Self::IMPORT => Ok(()),
            other => Err(UploadSessionError::InvalidUploadType(other.to_string())),
        }
    }
}

impl From<&str> for UploadType {
    fn from(s: &str) -> Self {
        UploadType(s.to_string())
    }
}

/// Port of `model.IncompleteUploadSuffix` (upload_session.go:16) — appended to the path while the
/// upload is in flight.
pub const INCOMPLETE_UPLOAD_SUFFIX: &str = ".tmp";

/// Port of `model.UploadNoUserID` (upload_session.go:20) — the **fake** user id the API layer
/// uses in local mode, where there is no session user. `IsValid` accepts it in place of a real
/// id, so it is a security-relevant special case, not a placeholder.
pub const UPLOAD_NO_USER_ID: &str = "nouser";

/// Port of `model.UploadSession` (upload_session.go:23).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadSession {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "type")]
    pub type_: UploadType,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// `omitempty` — an `import` upload has no channel.
    #[serde(rename = "channel_id", skip_serializing_if = "is_empty_str")]
    pub channel_id: String,

    #[serde(rename = "filename")]
    pub filename: String,

    /// `json:"-"` — the storage path, never sent. **`IsValid` requires it**, so a session decoded
    /// from JSON cannot be valid; the same trap as `FileInfo.path`.
    #[serde(skip)]
    pub path: String,

    #[serde(rename = "file_size")]
    pub file_size: i64,

    /// Bytes received so far. `file_offset == file_size` means the upload is complete.
    #[serde(rename = "file_offset")]
    pub file_offset: i64,

    /// Set when uploading on behalf of a remote cluster.
    #[serde(rename = "remote_id")]
    pub remote_id: String,

    /// The file id the remote cluster asked for.
    #[serde(rename = "req_file_id")]
    pub req_file_id: String,
}

impl UploadSession {
    /// Port of `(*UploadSession).PreSave` (upload_session.go:63) — both fields only when unset.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
    }

    /// Port of `(*UploadSession).IsValid` (upload_session.go:88).
    ///
    /// Four rules worth stating:
    ///
    /// - `user_id` may be [`UPLOAD_NO_USER_ID`] instead of a real id;
    /// - `channel_id` is required **only** for an `attachment` upload;
    /// - `file_size` must be strictly positive — a zero-byte upload is invalid;
    /// - `file_offset` must be within `0..=file_size`, so an over-long upload is caught here
    ///   rather than at write time.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(err("id", String::new(), None));
        }

        if let Err(inner) = self.type_.is_valid() {
            return Err(err("type", String::new(), Some(inner)));
        }

        if !is_valid_id(&self.user_id) && self.user_id != UPLOAD_NO_USER_ID {
            return Err(err("user_id", details(), None));
        }

        if self.type_.as_str() == UploadType::ATTACHMENT && !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", details(), None));
        }

        if self.create_at == 0 {
            return Err(err("create_at", details(), None));
        }

        if self.filename.is_empty() {
            return Err(err("filename", details(), None));
        }

        if self.file_size <= 0 {
            return Err(err("file_size", details(), None));
        }

        if self.file_offset < 0 || self.file_offset > self.file_size {
            return Err(err("file_offset", details(), None));
        }

        if self.path.is_empty() {
            return Err(err("path", details(), None));
        }

        Ok(())
    }
}

fn err(field: &str, details: String, wrapped: Option<UploadSessionError>) -> Box<AppError> {
    let app_error = AppError::new(
        "UploadSession.IsValid",
        format!("model.upload_session.is_valid.{field}.app_error"),
        None,
        details,
        400,
    );
    Box::new(match wrapped {
        Some(inner) => app_error.wrap(inner),
        None => app_error,
    })
}

/// The one non-`AppError` failure in this file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadSessionError {
    #[error("invalid UploadType {0}")]
    InvalidUploadType(String),
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
    fn upload_session_round_trips_the_fixture() {
        assert_fixture_round_trips!(UploadSession, "upload_session");
    }
}
