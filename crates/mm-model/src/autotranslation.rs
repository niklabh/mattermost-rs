//! Port of `model/autotranslation.go` — a stored translation of one object.
//!
//! # `IsValid` is state-dependent
//!
//! Only three fields are always required (`object_id`, `object_type`, `lang`). Everything else is
//! required **conditionally**: a `ready` translation must carry a provider, a valid type, and the
//! payload matching that type; an `unavailable` one must carry a provider *to say why it failed*;
//! `processing` and `skipped` require neither. A validator written as a flat field list would
//! reject rows the Go server accepts.
//!
//! # The context plumbing is not ported
//!
//! `WithAutoTranslationPath` / `GetAutoTranslationPath` stash an [`AutoTranslationPath`] in a
//! `context.Context` for metrics and per-path timeouts. Rust has no ambient request context, so
//! the value travels as an argument; the **vocabulary** is what has to agree with Go and it is
//! ported in full.

use serde::{Deserialize, Serialize};

use crate::post_metadata::PostTranslation;
use crate::serde_helpers::{is_empty_str, is_none, is_none_or_empty_map, is_zero_i64};
use crate::utils::{AppError, AppResult, StringInterface, is_valid_id};

/// Port of `model.TranslationObjectTypePost` (autotranslation.go:12) — the only object type.
pub const TRANSLATION_OBJECT_TYPE_POST: &str = "post";

/// Port of `model.TranslationType` (autotranslation.go:16).
pub const TRANSLATION_TYPE_STRING: &str = "string";
pub const TRANSLATION_TYPE_OBJECT: &str = "object";

/// Port of `model.TranslationState` (autotranslation.go:23).
///
/// `skipped` covers two cases Go's comment spells out: source and destination language are the
/// same, **or** the content was entirely masked.
pub const TRANSLATION_STATE_READY: &str = "ready";
pub const TRANSLATION_STATE_SKIPPED: &str = "skipped";
pub const TRANSLATION_STATE_PROCESSING: &str = "processing";
pub const TRANSLATION_STATE_UNAVAILABLE: &str = "unavailable";

/// The `Meta` key `ToPostTranslation` reads the source language from.
pub const TRANSLATION_META_SRC_LANG: &str = "src_lang";

/// Port of `model.Translation` (autotranslation.go:33).
///
/// Seven of the thirteen fields carry `omitempty`; `object_id`, `object_type`, `lang`,
/// `provider`, `type`, `text` and `state` are always written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Translation {
    #[serde(rename = "object_id")]
    pub object_id: String,

    #[serde(rename = "object_type")]
    pub object_type: String,

    /// Denormalised from the object, for query efficiency.
    #[serde(rename = "channel_id", skip_serializing_if = "is_empty_str")]
    pub channel_id: String,

    /// The **destination** language. The source language lives in `meta.src_lang`.
    #[serde(rename = "lang")]
    pub lang: String,

    #[serde(rename = "provider")]
    pub provider: String,

    #[serde(rename = "type")]
    pub type_: String,

    /// Used when `type` is `string`.
    #[serde(rename = "text")]
    pub text: String,

    /// Used when `type` is `object`. A `json.RawMessage` in Go.
    #[serde(rename = "object_json", skip_serializing_if = "is_none")]
    pub object_json: Option<serde_json::Value>,

    #[serde(rename = "confidence", skip_serializing_if = "is_none")]
    pub confidence: Option<f64>,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(rename = "meta", skip_serializing_if = "is_none_or_empty_map")]
    pub meta: Option<StringInterface>,

    /// A hash of the normalised source text, used to detect that a re-translation is unnecessary.
    #[serde(rename = "norm_hash", skip_serializing_if = "is_empty_str")]
    pub norm_hash: String,

    /// Epoch milliseconds.
    #[serde(rename = "update_at", skip_serializing_if = "is_zero_i64")]
    pub update_at: i64,
}

impl Translation {
    /// Port of `(*Translation).Clone` (autotranslation.go:50) — a genuinely deep copy: the
    /// confidence pointer, the meta map and the raw JSON are each copied rather than shared.
    /// `Clone` here is the same thing, since none of the three is shared behind a pointer.
    pub fn deep_copy(&self) -> Translation {
        self.clone()
    }

    /// Port of `(*Translation).ToPostTranslation` (autotranslation.go:88) — the canonical
    /// conversion, so post metadata is populated the same way everywhere.
    ///
    /// Note what it does **not** copy: `Type` is dropped, so the resulting `PostTranslation` has
    /// an empty `type_` and the caller distinguishes text from object by which field is set.
    /// A non-string `meta.src_lang` yields an empty source language rather than an error.
    pub fn to_post_translation(&self) -> PostTranslation {
        let source_lang = self
            .meta
            .as_ref()
            .and_then(|m| m.get(TRANSLATION_META_SRC_LANG))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut pt = PostTranslation {
            state: self.state.clone(),
            source_lang,
            ..Default::default()
        };

        if self.type_ == TRANSLATION_TYPE_OBJECT {
            pt.object = self.object_json.clone();
        } else {
            pt.text = self.text.clone();
        }

        pt
    }

    /// Port of `(*Translation).IsValid` (autotranslation.go:113). See the module docs.
    ///
    /// Every branch is a **400 built with a literal status code** rather than
    /// `http.StatusBadRequest`, and every one carries a plain-English `detailed_error`. Go's nil
    /// receiver has its own error id, `…is_valid.nil.app_error`, which is unrepresentable on
    /// `&self`.
    pub fn is_valid(&self) -> AppResult {
        if self.object_id.is_empty() || !is_valid_id(&self.object_id) {
            return Err(err("object_id", "invalid object id"));
        }
        if self.object_type.is_empty() {
            return Err(err("object_type", "object type is empty"));
        }
        if self.lang.is_empty() {
            return Err(err("lang", "lang is empty"));
        }

        if self.state == TRANSLATION_STATE_READY {
            if self.provider.is_empty() {
                return Err(err("provider", "provider is empty for ready state"));
            }
            if self.type_.is_empty() {
                return Err(err("type", "type is empty"));
            }
            if self.type_ != TRANSLATION_TYPE_STRING && self.type_ != TRANSLATION_TYPE_OBJECT {
                return Err(err("type_invalid", "invalid type"));
            }
            if self.type_ == TRANSLATION_TYPE_STRING && self.text.is_empty() {
                return Err(err("text", "text is empty"));
            }
            // Go measures `len(t.ObjectJSON)`, so a present-but-empty raw message fails too.
            if self.type_ == TRANSLATION_TYPE_OBJECT && self.object_json.is_none() {
                return Err(err("object_json", "object json is empty"));
            }
        }

        if self.state == TRANSLATION_STATE_UNAVAILABLE && self.provider.is_empty() {
            return Err(err("provider", "provider is empty for unavailable state"));
        }

        Ok(())
    }
}

fn err(field: &str, details: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Translation.IsValid",
        format!("model.translation.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.ContextKeyAutoTranslationPath` (autotranslation.go:155). Kept as a constant so
/// a caller threading this through a map uses Go's key.
pub const CONTEXT_KEY_AUTO_TRANSLATION_PATH: &str = "autotranslation_path";

/// Port of `model.AutoTranslationPath` (autotranslation.go:178) — which code path asked for the
/// translation, for metrics and per-path timeouts.
pub const AUTO_TRANSLATION_PATH_CREATE: &str = "create";
pub const AUTO_TRANSLATION_PATH_EDIT: &str = "edit";
/// An on-demand fetch for an older object.
pub const AUTO_TRANSLATION_PATH_FETCH: &str = "fetch";
pub const AUTO_TRANSLATION_PATH_WEBSOCKET: &str = "websocket";
pub const AUTO_TRANSLATION_PATH_PUSH_NOTIFICATION: &str = "push_notification";
pub const AUTO_TRANSLATION_PATH_EMAIL_NOTIFICATION: &str = "email_notification";
/// The fallback `GetAutoTranslationPath` returns when no path was set.
pub const AUTO_TRANSLATION_PATH_UNKNOWN: &str = "unknown";

/// Port of `model.ErrAutoTranslationNotAvailable` (autotranslation.go:163) — a **typed** error
/// callers match on to degrade gracefully when the feature is unlicensed, flagged off, or
/// unconfigured.
///
/// The message has two forms, with and without a reason, and both are reproduced verbatim: a
/// caller comparing strings would otherwise miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrAutoTranslationNotAvailable {
    reason: String,
}

impl ErrAutoTranslationNotAvailable {
    /// Port of `model.NewErrAutoTranslationNotAvailable` (autotranslation.go:174).
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for ErrAutoTranslationNotAvailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reason.is_empty() {
            f.write_str("auto-translation feature not available")
        } else {
            write!(f, "auto-translation feature not available: {}", self.reason)
        }
    }
}

impl std::error::Error for ErrAutoTranslationNotAvailable {}

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
    fn translation_round_trips_the_fixture() {
        assert_fixture_round_trips!(Translation, "translation");
    }
}
