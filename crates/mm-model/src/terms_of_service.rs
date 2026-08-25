//! Port of `model/terms_of_service.go` — the custom terms a server can require.

use serde::{Deserialize, Serialize};

use crate::post::POST_MESSAGE_MAX_RUNES_V2;
use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id};

/// Port of `model.TermsOfService` (terms_of_service.go:9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TermsOfService {
    #[serde(rename = "id")]
    pub id: String,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    /// The admin who published this revision.
    #[serde(rename = "user_id")]
    pub user_id: String,

    /// Capped at [`POST_MESSAGE_MAX_RUNES_V2`] — the terms are bounded by the *post* limit,
    /// because they are rendered as a post body.
    #[serde(rename = "text")]
    pub text: String,
}

impl TermsOfService {
    /// Port of `(*TermsOfService).IsValid` (terms_of_service.go:16).
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            // Note: the id branch passes an **empty** terms id, so its error carries no details.
            return Err(invalid_terms_of_service_error("id", ""));
        }

        if self.create_at == 0 {
            return Err(invalid_terms_of_service_error("create_at", &self.id));
        }

        if !is_valid_id(&self.user_id) {
            return Err(invalid_terms_of_service_error("user_id", &self.id));
        }

        if self.text.chars().count() > POST_MESSAGE_MAX_RUNES_V2 {
            return Err(invalid_terms_of_service_error("text", &self.id));
        }

        Ok(())
    }

    /// Port of `(*TermsOfService).PreSave` (terms_of_service.go:45).
    ///
    /// **`create_at` is set unconditionally**, unlike almost every other `PreSave` in the
    /// package, which only fills a zero timestamp. Re-saving a row rewrites its creation time.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
    }
}

/// Port of `model.InvalidTermsOfServiceError` (terms_of_service.go:34).
///
/// Every branch carries `MaxLength` in its i18n params — including the ones that have nothing to
/// do with length — because Go builds the params unconditionally.
pub fn invalid_terms_of_service_error(
    field_name: &str,
    terms_of_service_id: &str,
) -> Box<AppError> {
    let details = if terms_of_service_id.is_empty() {
        String::new()
    } else {
        format!("terms_of_service_id={terms_of_service_id}")
    };

    let mut params = std::collections::HashMap::new();
    params.insert(
        "MaxLength".to_string(),
        serde_json::Value::from(POST_MESSAGE_MAX_RUNES_V2 as i64),
    );

    Box::new(AppError::new(
        "TermsOfService.IsValid",
        format!("model.terms_of_service.is_valid.{field_name}.app_error"),
        Some(params),
        details,
        400,
    ))
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
    fn terms_of_service_round_trips_the_fixture() {
        assert_fixture_round_trips!(TermsOfService, "terms_of_service");
    }
}
