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
//!
//! # A JSON `null` is never an error, anywhere in this body
//!
//! `encoding/json` unmarshals `null` into a pointer, map, slice or interface as nil and into
//! **anything else as a no-op** — so every one of these is a body Go accepts, and each was a 400
//! from this port until the route that reads it was ported and measured them:
//!
//! | body | Go |
//! |---|---|
//! | `null` | the zero struct, so `emails` is empty |
//! | `{"emails":null}` | the same |
//! | `{"emails":[null]}` | `[""]` — **one** email, and the request proceeds |
//! | `{"message":null}`, `{"channelIds":null}`, `{"profiles":null}` | the zero value of each |
//! | `{"profiles":[null]}` | a slice of **one nil pointer** — `len(Profiles) > 0` is true |
//! | `{"profiles":[{"email":null}]}` | a profile with an empty email |
//!
//! serde rejects every one of them, so the fields route through [`null_as_default`] and
//! `profiles` is `Vec<Option<_>>` — Go's `[]*MemberInviteProfile` spelled exactly, which is also
//! what makes `IsValid`'s `profile == nil` branch reachable.

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

    /// Go's `[]*MemberInviteProfile`: a **pointer** element, so a `null` in the array is a nil
    /// entry that still counts toward `len(Profiles)`.
    #[serde(rename = "profiles", skip_serializing_if = "is_none_or_empty_vec")]
    pub profiles: Option<Vec<Option<MemberInviteProfile>>>,
}

/// Port of `model.MemberInviteProfile` (member_invite.go:18) — admin-chosen profile fields
/// applied to the account created from one invitation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberInviteProfile {
    #[serde(rename = "email", deserialize_with = "null_as_default")]
    pub email: String,

    #[serde(rename = "username", deserialize_with = "null_as_default")]
    pub username: String,

    #[serde(rename = "first_name", deserialize_with = "null_as_default")]
    pub first_name: String,

    #[serde(rename = "last_name", deserialize_with = "null_as_default")]
    pub last_name: String,
}

/// `null` is the zero value, not an error — Go's rule for every non-pointer target.
///
/// Used on the scalar fields; the slices use it through [`null_as_strings`], which additionally
/// has to survive a `null` *element*.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A `[]string` the way Go unmarshals one: `null` for the whole field is an empty slice, and a
/// `null` **element** is the empty string, because `null` into a `string` is a no-op.
fn null_as_strings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<Option<String>>>::deserialize(d)?
        .unwrap_or_default()
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect())
}

/// [`null_as_strings`] for a field that keeps Go's nil/empty distinction — `channelIds` is
/// `omitempty`, so "absent" and "present but empty" differ on the way back out.
fn null_as_optional_strings<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Ok(Option::<Vec<Option<String>>>::deserialize(d)?
        .map(|list| list.into_iter().map(Option::unwrap_or_default).collect()))
}

/// The object form, used by [`MemberInvite`]'s hand-written `Deserialize` after the bare-array
/// attempt fails. Go spells this `type TempMemberInvite MemberInvite`, which drops the method set
/// and so avoids infinite recursion; a separate struct is the same trick.
#[derive(Default, Deserialize)]
#[serde(default)]
struct MemberInviteWire {
    #[serde(deserialize_with = "null_as_strings")]
    emails: Vec<String>,
    #[serde(rename = "channelIds", deserialize_with = "null_as_optional_strings")]
    channel_ids: Option<Vec<String>>,
    #[serde(deserialize_with = "null_as_default")]
    message: String,
    #[serde(deserialize_with = "null_as_default")]
    profiles: Option<Vec<Option<MemberInviteProfile>>>,
}

impl<'de> Deserialize<'de> for MemberInvite {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Both attempts need the same input, so it is buffered once. Go re-reads the same `[]byte`
        // for the same reason.
        let value = serde_json::Value::deserialize(d)?;

        // `json.Unmarshal([]byte("null"), &obj)` is a no-op on a struct target, so a body that is
        // exactly `null` is the zero `MemberInvite` and **not** a decode error. Go's array attempt
        // succeeds on it first (`null` into a `[]string` is nil), which lands in the same place.
        if value.is_null() {
            return Ok(MemberInvite::default());
        }

        if let Ok(emails) = serde_json::from_value::<Vec<Option<String>>>(value.clone()) {
            let emails: Vec<String> = emails.into_iter().map(Option::unwrap_or_default).collect();
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
            // Go's `profile == nil` branch — `profile_nil.app_error`, and it **is** reachable:
            // `{"profiles":[null]}` decodes to one nil pointer on both sides.
            let Some(profile) = profile else {
                return Err(err("profile_nil", String::new()));
            };
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
mod go_parity {
    use super::*;

    /// Every row of `fixtures/behaviour_member_invite.json`, which is `json.Unmarshal` into
    /// `model.MemberInvite` over 32 bodies — the bare-array form, the object form, `null` in every
    /// position it can occupy, and ten genuine decode errors.
    ///
    /// `serde_json::from_slice` is the right counterpart to `json.Unmarshal`: both consume the
    /// **whole** input, so `trailing_garbage` is an error on either side. The handler uses a
    /// streaming decoder instead (`json.NewDecoder(…).Decode`, which stops at the first value), and
    /// that difference is asserted in `mm_api::team_admin`, not here.
    ///
    /// **`emails` nil and `emails` empty are compared as one.** Go tags the field without
    /// `omitempty`, so a nil slice marshals to `null` and an empty one to `[]`; the only thing any
    /// caller asks is `len(Emails) == 0`, which cannot tell them apart either.
    #[test]
    fn decoding_matches_go_body_for_body() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            body: String,
            err: bool,
            emails: Option<Vec<String>>,
            channel_ids: Option<Vec<String>>,
            message: String,
            profile_count: usize,
            profiles_present: Option<Vec<bool>>,
            first_email: String,
            first_username: String,
            first_first_name: String,
            first_last_name: String,
        }
        #[derive(serde::Deserialize)]
        struct Corpus {
            decode: Vec<Case>,
        }

        let raw = include_str!("../../../fixtures/behaviour_member_invite.json");
        let corpus: Corpus = serde_json::from_str(raw).expect("the corpus decodes");
        assert!(corpus.decode.len() >= 30, "the corpus is the oracle");

        for case in corpus.decode {
            let parsed = serde_json::from_str::<MemberInvite>(&case.body);
            if case.err {
                assert!(
                    parsed.is_err(),
                    "{}: Go refused {:?} and we did not",
                    case.name,
                    case.body
                );
                continue;
            }
            let invite = parsed.unwrap_or_else(|e| {
                panic!(
                    "{}: Go accepted {:?} and we did not: {e}",
                    case.name, case.body
                )
            });
            assert_eq!(
                invite.emails,
                case.emails.unwrap_or_default(),
                "{}: emails",
                case.name
            );
            assert_eq!(
                invite.channel_ids, case.channel_ids,
                "{}: channelIds",
                case.name
            );
            assert_eq!(invite.message, case.message, "{}: message", case.name);

            let profiles = invite.profiles.unwrap_or_default();
            assert_eq!(
                profiles.len(),
                case.profile_count,
                "{}: profile count",
                case.name
            );
            let present: Vec<bool> = profiles.iter().map(Option::is_some).collect();
            assert_eq!(
                present,
                case.profiles_present.unwrap_or_default(),
                "{}: nil profiles",
                case.name
            );

            let first = profiles.first().and_then(Option::as_ref);
            assert_eq!(
                first.map(|p| p.email.as_str()).unwrap_or_default(),
                case.first_email,
                "{}: first profile email",
                case.name
            );
            assert_eq!(
                first.map(|p| p.username.as_str()).unwrap_or_default(),
                case.first_username,
                "{}: first profile username",
                case.name
            );
            assert_eq!(
                first.map(|p| p.first_name.as_str()).unwrap_or_default(),
                case.first_first_name,
                "{}: first profile first_name",
                case.name
            );
            assert_eq!(
                first.map(|p| p.last_name.as_str()).unwrap_or_default(),
                case.first_last_name,
                "{}: first profile last_name",
                case.name
            );
        }
    }

    /// `IsValid`'s `profile == nil` branch, which only became reachable when `profiles` became
    /// `Vec<Option<_>>`. It fires **before** any of the email or username rules.
    #[test]
    fn a_nil_profile_is_its_own_error() {
        let invite = MemberInvite {
            emails: vec!["a@example.com".to_owned()],
            profiles: Some(vec![None]),
            ..Default::default()
        };
        let err = invite.is_valid().expect_err("a nil profile is refused");
        assert_eq!(err.id, "model.member.is_valid.profile_nil.app_error");
        assert_eq!(err.status_code, 400);

        // And it precedes the email rule: the non-nil profile here names an uninvited address,
        // which would be `profile_email` if the nil were skipped rather than refused.
        let invite = MemberInvite {
            emails: vec!["a@example.com".to_owned()],
            profiles: Some(vec![
                None,
                Some(MemberInviteProfile {
                    email: "elsewhere@example.com".to_owned(),
                    username: "someone".to_owned(),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        };
        assert_eq!(
            invite.is_valid().expect_err("still refused").id,
            "model.member.is_valid.profile_nil.app_error"
        );
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
    fn member_invite_round_trips_the_fixture() {
        assert_fixture_round_trips!(MemberInvite, "member_invite");
    }
    #[test]
    fn member_invite_profile_round_trips_the_fixture() {
        assert_fixture_round_trips!(MemberInviteProfile, "member_invite_profile");
    }
}
