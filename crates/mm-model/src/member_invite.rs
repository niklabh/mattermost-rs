//! Port of `model/member_invite.go` — the invite-members request body.
//!
//! # It decodes from two different shapes
//!
//! `UnmarshalJSON` first tries the body as a **bare array of email strings** and, only if that
//! fails, as the object. That is backwards compatibility with the pre-`MemberInvite` API, and it
//! is why `["a@b.c"]` and `{"emails":["a@b.c"]}` are both valid bodies. The array form resets the
//! whole struct first, so no other field can be smuggled in alongside it.
//!
//! Note `channelIds` is **camelCase** while `first_name` and `last_name` on the nested profile
//! are snake_case.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_none_or_empty_vec;
use crate::user::{
    USER_FIRST_NAME_MAX_RUNES, USER_LAST_NAME_MAX_RUNES, is_valid_username, normalize_email,
    normalize_username,
};
use crate::utils::{AppError, AppResult, ID_LENGTH};

/// Port of `model.MemberInvite` (member_invite.go:9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MemberInvite {
    #[serde(rename = "emails")]
    pub emails: Vec<String>,

    /// **camelCase**, and `omitempty`.
    #[serde(rename = "channelIds", skip_serializing_if = "is_none_or_empty_vec")]
    pub channel_ids: Option<Vec<String>>,

    #[serde(rename = "message")]
    pub message: String,

    #[serde(rename = "profiles", skip_serializing_if = "is_none_or_empty_vec")]
    pub profiles: Option<Vec<MemberInviteProfile>>,
}

/// Port of `model.MemberInviteProfile` (member_invite.go:18) — admin-chosen profile fields
/// applied to the account created from one invitation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberInviteProfile {
    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "username")]
    pub username: String,

    #[serde(rename = "first_name")]
    pub first_name: String,

    #[serde(rename = "last_name")]
    pub last_name: String,
}

/// The object form, used by [`MemberInvite`]'s hand-written `Deserialize` after the bare-array
/// attempt fails. Go spells this `type TempMemberInvite MemberInvite`, which drops the method set
/// and so avoids infinite recursion; a separate struct is the same trick.
#[derive(Default, Deserialize)]
#[serde(default)]
struct MemberInviteWire {
    emails: Vec<String>,
    #[serde(rename = "channelIds")]
    channel_ids: Option<Vec<String>>,
    message: String,
    profiles: Option<Vec<MemberInviteProfile>>,
}

impl<'de> Deserialize<'de> for MemberInvite {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Both attempts need the same input, so it is buffered once. Go re-reads the same `[]byte`
        // for the same reason.
        let value = serde_json::Value::deserialize(d)?;

        if let Ok(emails) = serde_json::from_value::<Vec<String>>(value.clone()) {
            // Go assigns `*i = MemberInvite{}` first: the array form yields nothing else.
            return Ok(MemberInvite {
                emails,
                ..Default::default()
            });
        }

        let wire: MemberInviteWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(MemberInvite {
            emails: wire.emails,
            channel_ids: wire.channel_ids,
            message: wire.message,
            profiles: wire.profiles,
        })
    }
}

impl MemberInvite {
    /// Port of `(*MemberInvite).IsValid` (member_invite.go:33).
    ///
    /// The rules on `profiles` are the substance:
    ///
    /// - every profile's email must be one of `emails`, compared **normalised** (trimmed and
    ///   lower-cased), so casing in the profile need not match the invite list;
    /// - no two profiles may name the same email or the same username, again normalised;
    /// - each username must satisfy `IsValidUsername` **after** normalisation;
    /// - first and last names are capped in **runes**.
    ///
    /// The channel check is a bare length test, not `IsValidId` — same as `guest_invite.go`. Note
    /// `message` is never validated, and the error ids are `model.member.is_valid.*` (singular).
    pub fn is_valid(&self) -> AppResult {
        if self.emails.is_empty() {
            return Err(err("emails", String::new()));
        }

        for channel in self.channel_ids.iter().flatten() {
            if channel.len() != ID_LENGTH {
                return Err(err("channel", format!("channel={channel}")));
            }
        }

        let invited_emails: std::collections::HashSet<String> =
            self.emails.iter().map(|e| normalize_email(e)).collect();

        let mut seen_profile_emails = std::collections::HashSet::new();
        let mut seen_usernames = std::collections::HashSet::new();

        for profile in self.profiles.iter().flatten() {
            // Go's `profile == nil` branch — `profile_nil.app_error` — is unreachable here: a
            // `Vec<MemberInviteProfile>` of values cannot hold a nil, and a JSON `null` element
            // fails to decode rather than arriving as one.
            let email = normalize_email(&profile.email);
            if !invited_emails.contains(&email) {
                return Err(err("profile_email", format!("email={}", profile.email)));
            }
            if !seen_profile_emails.insert(email) {
                return Err(err(
                    "profile_email_duplicate",
                    format!("email={}", profile.email),
                ));
            }

            let username = normalize_username(&profile.username);
            if !is_valid_username(&username) {
                return Err(err(
                    "profile_username",
                    format!("username={}", profile.username),
                ));
            }
            if !seen_usernames.insert(username) {
                return Err(err(
                    "profile_username_duplicate",
                    format!("username={}", profile.username),
                ));
            }

            validate_member_invite_profile_names(profile)?;
        }

        Ok(())
    }
}

/// Port of `model.validateMemberInviteProfileNames` (member_invite.go:88).
///
/// Both branches report the **email**, not the offending name.
fn validate_member_invite_profile_names(profile: &MemberInviteProfile) -> AppResult {
    if profile.first_name.chars().count() > USER_FIRST_NAME_MAX_RUNES {
        return Err(err(
            "profile_first_name",
            format!("email={}", profile.email),
        ));
    }
    if profile.last_name.chars().count() > USER_LAST_NAME_MAX_RUNES {
        return Err(err("profile_last_name", format!("email={}", profile.email)));
    }
    Ok(())
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "MemberInvite.IsValid",
        format!("model.member.is_valid.{field}.app_error"),
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
    fn member_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(MemberInvite, "member_invite");
    }
    #[test]
    fn member_invite_profile_round_trips_the_fixture() {
        assert_fixture_round_trips!(MemberInviteProfile, "member_invite_profile");
    }
}
