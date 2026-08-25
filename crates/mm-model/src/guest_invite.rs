//! Port of `model/guest_invite.go` — the body of the invite-guests route.

use serde::{Deserialize, Serialize};

use crate::user::USER_EMAIL_MAX_LENGTH;
use crate::utils::{AppError, AppResult, ID_LENGTH, is_valid_email};

/// Port of `model.GuestsInvite` (guest_invite.go:7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestsInvite {
    #[serde(rename = "emails")]
    pub emails: Vec<String>,

    #[serde(rename = "channels")]
    pub channels: Vec<String>,

    /// Free text sent with the invitation. **Unvalidated and unbounded** — `IsValid` never looks
    /// at it, and it is also the one field `Auditable` leaves out.
    #[serde(rename = "message")]
    pub message: String,
}

impl GuestsInvite {
    /// Port of `(*GuestsInvite).IsValid` (guest_invite.go:21).
    ///
    /// Three things worth not "improving":
    ///
    /// - the email length cap is `len(email) > UserEmailMaxLength`, i.e. **bytes**, not runes.
    /// - the channel check is a bare `len(channel) != 26` — it does not call `IsValidId`, so a
    ///   26-character string of any bytes passes here and fails later in the store.
    /// - the error ids are `model.guest.is_valid.*` (singular `guest`), and the per-item branches
    ///   drop to the singular: `emails` then `email`, `channels` then `channel`.
    pub fn is_valid(&self) -> AppResult {
        if self.emails.is_empty() {
            return Err(err("emails", String::new()));
        }

        for email in &self.emails {
            if email.len() > USER_EMAIL_MAX_LENGTH || email.is_empty() || !is_valid_email(email) {
                return Err(err("email", format!("email={email}")));
            }
        }

        if self.channels.is_empty() {
            return Err(err("channels", String::new()));
        }

        for channel in &self.channels {
            if channel.len() != ID_LENGTH {
                return Err(err("channel", format!("channel={channel}")));
            }
        }

        Ok(())
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "GuestsInvite.IsValid",
        format!("model.guest.is_valid.{field}.app_error"),
        None,
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
    fn guests_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(GuestsInvite, "guests_invite");
    }
}
