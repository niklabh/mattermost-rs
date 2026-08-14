//! Port of `server/public/model/mm_blocks_actions.go` — the server-side definitions behind the
//! `post.props.mm_blocks_actions` registry.
//!
//! None of these types carries a `json:` tag: the registry itself is an untyped
//! `map[string]any` inside `Post.Props`, and everything here is a **coercion** out of it. That
//! shape is what makes the file dangerous to port from a reading, the same way
//! [`crate::post_interactive_blocks`] was: every type mismatch is a silent `nil`, so a wrong key
//! name produces "no such action" rather than an error, and "no such action" is a legitimate
//! answer.
//!
//! Deferred, and only these two:
//!
//! | Go | why not yet |
//! |---|---|
//! | `AddMmBlocksActionCookies` (:188) | AES-GCM through `EncryptPostActionCookie` ([D-046]) |
//!
//! `StripMmBlocksActionSecrets` (:243) landed a session early, with
//! `Post::strip_action_integrations` — it is defined here now, where Go puts it.

use crate::go_url::go_parse;
use crate::integration_action::{
    MM_BLOCKS_ACTION_COOKIE_KIND, MmBlocksActionCookie, POST_ACTION_TYPE_BUTTON, PostAction,
    PostActionCookie, PostActionIntegration,
};
use crate::post::{POST_PROPS_MM_BLOCKS_ACTIONS, Post};
use crate::utils::{StringInterface, StringMap};

/// Port of `model.MmBlocksActionTypeExternal` / `…OpenURL` (mm_blocks_actions.go:28).
pub const MM_BLOCKS_ACTION_TYPE_EXTERNAL: &str = "external";
pub const MM_BLOCKS_ACTION_TYPE_OPEN_URL: &str = "openURL";

/// Port of `model.ErrMmBlocksActionNotFound` (mm_blocks_actions.go:18), plus the URL-merge
/// failure `ResolveMmBlocksAction` propagates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MmBlocksActionError {
    /// Go's message is `mm_blocks action_id=<id>: mm_blocks action not found`.
    #[error("mm_blocks action_id={0}: mm_blocks action not found")]
    NotFound(String),
    #[error("parse url: {0}")]
    ParseUrl(#[from] crate::go_url::UrlError),
}

/// Port of `model.MmBlocksActionResolved` (mm_blocks_actions.go:22). Exactly one of the two URL
/// fields is set, chosen by the spec's type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MmBlocksActionResolved {
    pub open_url_goto: String,
    pub external_url: String,
    pub context: StringInterface,
}

/// Port of `model.MmBlocksActionSpec` (mm_blocks_actions.go:34). No `json:` tags — this is the
/// typed view of one `props.mm_blocks_actions[id]` object, not a wire type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MmBlocksActionSpec {
    pub spec_type: String,
    pub url: String,
    /// Static per-action query, merged into [`Self::url`] before dispatch.
    pub query: StringMap,
    /// Only an `external` spec carries one; `openURL` leaves it empty.
    pub context: StringInterface,
}

/// Port of `coerceToStringAnyMap` (mm_blocks_actions.go:289). A JSON object and nothing else —
/// an array, a string or a number is a silent miss.
fn coerce_to_string_any_map(v: Option<&serde_json::Value>) -> Option<&StringInterface> {
    v?.as_object()
}

/// Port of `MmBlocksContextMap` (mm_blocks_actions.go:136).
///
/// Parses a context string as a JSON **object**, falling back to wrapping the raw string under
/// the key `context`. Three inputs land in the fallback for reasons worth stating, because each
/// looks like it should decode:
///
/// - `null` decodes without error into a *nil* map, and the `m != nil` guard rejects it;
/// - `[1,2]` and `"a string"` are valid JSON that is not an object, so the decode errors;
/// - `{}` is an object and is **not** wrapped — it comes back as an empty map.
pub fn mm_blocks_context_map(context_string: &str) -> Option<StringInterface> {
    if context_string.is_empty() {
        return None;
    }
    if let Ok(serde_json::Value::Object(m)) =
        serde_json::from_str::<serde_json::Value>(context_string)
    {
        return Some(m);
    }
    let mut out = StringInterface::new();
    out.insert(
        "context".to_string(),
        serde_json::Value::String(context_string.to_string()),
    );
    Some(out)
}

/// Port of `contextMapFromProp` (mm_blocks_actions.go:256). A string goes through
/// [`mm_blocks_context_map`]; an object is taken as-is; anything else is a miss.
fn context_map_from_prop(v: Option<&serde_json::Value>) -> Option<StringInterface> {
    match v? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => mm_blocks_context_map(s),
        // Go clones here so a caller cannot mutate the live post props through the returned map.
        // Rust's ownership makes the clone the only option anyway.
        other => other.as_object().cloned(),
    }
}

/// Port of `stringMapFromPropValue` (mm_blocks_actions.go:272).
///
/// **Non-string values are dropped individually, not fatally.** `{"a":"1","b":2}` yields
/// `{"a":"1"}`; only an all-non-string map collapses to nothing. An empty result and a missing
/// key are indistinguishable, which is what makes the `len(spec.Query) > 0` guard in `GetAction`
/// the same test as "was there a usable query".
fn string_map_from_prop_value(v: Option<&serde_json::Value>) -> StringMap {
    let Some(m) = coerce_to_string_any_map(v) else {
        return StringMap::new();
    };
    if m.is_empty() {
        return StringMap::new();
    }
    m.iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// Port of `mmBlocksEntryMapToSpec` (mm_blocks_actions.go:63).
///
/// An empty or unrecognised `type` is `None`, and `url` is *not* required here — an `external`
/// entry with no URL yields a spec whose URL is empty, which the callers then reject
/// individually. `openURL` never reads `context`.
pub fn mm_blocks_entry_map_to_spec(entry_map: &StringInterface) -> Option<MmBlocksActionSpec> {
    let typ = entry_map.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ.is_empty() {
        return None;
    }

    let url = entry_map
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let query = string_map_from_prop_value(entry_map.get("query"));

    match typ {
        MM_BLOCKS_ACTION_TYPE_EXTERNAL => Some(MmBlocksActionSpec {
            spec_type: typ.to_string(),
            url,
            query,
            context: context_map_from_prop(entry_map.get("context")).unwrap_or_default(),
        }),
        MM_BLOCKS_ACTION_TYPE_OPEN_URL => Some(MmBlocksActionSpec {
            spec_type: typ.to_string(),
            url,
            query,
            context: StringInterface::new(),
        }),
        _ => None,
    }
}

impl Post {
    /// Port of `(*Post).GetMmBlocksActionSpec` (mm_blocks_actions.go:42).
    pub fn get_mm_blocks_action_spec(&self, action_id: &str) -> Option<MmBlocksActionSpec> {
        if action_id.is_empty() {
            return None;
        }
        let actions_top = coerce_to_string_any_map(self.get_prop(POST_PROPS_MM_BLOCKS_ACTIONS))?;
        // Go tests `!ok || entry == nil` — an entry present but null is the same as absent.
        let entry = actions_top.get(action_id)?;
        let entry_map = coerce_to_string_any_map(Some(entry))?;
        mm_blocks_entry_map_to_spec(entry_map)
    }

    /// Port of `(*Post).StripMmBlocksActionSecrets` (mm_blocks_actions.go:243).
    ///
    /// Removes the server-only registry before a post goes on the wire. A **string** value is
    /// kept: `AddMmBlocksActionCookies` has already replaced the registry with one opaque
    /// encrypted blob, and that blob is what the client needs. Anything else — the plaintext
    /// map, a number, an array — is deleted. An explicit JSON `null` is kept, because
    /// [`Post::get_prop`] reads it as absent and the guard returns early.
    pub fn strip_mm_blocks_action_secrets(&mut self) {
        match self.get_prop(POST_PROPS_MM_BLOCKS_ACTIONS) {
            None => (),
            Some(serde_json::Value::String(_)) => (),
            Some(_) => self.del_prop(POST_PROPS_MM_BLOCKS_ACTIONS),
        }
    }
}

impl MmBlocksActionCookie {
    /// Port of `(*MmBlocksActionCookie).ActionSpec` (mm_blocks_actions.go:85).
    ///
    /// Go's `Actions` is a `map[string]map[string]any`, so its coercion step can only fail on a
    /// nil entry. Ours is typed the same way and a JSON `null` entry fails to *decode* rather
    /// than coercing to nothing — the standing [D-033] convention.
    pub fn action_spec(&self, action_id: &str) -> Option<MmBlocksActionSpec> {
        if action_id.is_empty() {
            return None;
        }
        mm_blocks_entry_map_to_spec(self.actions.as_ref()?.get(action_id)?)
    }
}

/// Port of `ResolveMmBlocksAction` (mm_blocks_actions.go:101).
///
/// `openURL` returns a goto target with the static query merged in; `external` merges the static
/// query and then the per-click one **on top**, so a client key overrides a spec key of the same
/// name. Any other type — and an empty URL on either — is "not found" rather than a distinct
/// error.
pub fn resolve_mm_blocks_action(
    spec: Option<&MmBlocksActionSpec>,
    action_id: &str,
    client_query: &StringMap,
) -> Result<MmBlocksActionResolved, MmBlocksActionError> {
    let not_found = || MmBlocksActionError::NotFound(action_id.to_string());

    let spec = spec.ok_or_else(not_found)?;
    match spec.spec_type.as_str() {
        MM_BLOCKS_ACTION_TYPE_OPEN_URL => {
            if spec.url.is_empty() {
                return Err(not_found());
            }
            Ok(MmBlocksActionResolved {
                open_url_goto: merge_query_into_url(&spec.url, &spec.query)?,
                ..Default::default()
            })
        }
        MM_BLOCKS_ACTION_TYPE_EXTERNAL => {
            if spec.url.is_empty() {
                return Err(not_found());
            }
            let upstream = merge_query_into_url(&spec.url, &spec.query)?;
            Ok(MmBlocksActionResolved {
                external_url: merge_query_into_url(&upstream, client_query)?,
                context: spec.context.clone(),
                ..Default::default()
            })
        }
        _ => Err(not_found()),
    }
}

/// Port of `MergeQueryIntoURL` (mm_blocks_actions.go:148).
///
/// **An empty map short-circuits and returns the input verbatim**, which is not a micro-
/// optimisation: it is the difference between a URL being normalised and being passed through
/// untouched. `MergeQueryIntoURL("http://x/a%41b", nil)` is `http://x/a%41b`, while merging even
/// one key makes it `http://x/aAb` — Go's `URL.String()` re-encodes every component with the
/// canonical escaping for its position. It also means a *malformed* URL is returned unchanged
/// rather than reported, because `Parse` is never reached.
///
/// Existing keys are overwritten wholesale by `Values.Set`, so a repeated `?k=1&k=2` collapses
/// to the single merged value, and the result is re-encoded sorted by key.
pub fn merge_query_into_url(raw_url: &str, q: &StringMap) -> Result<String, MmBlocksActionError> {
    if q.is_empty() {
        return Ok(raw_url.to_string());
    }
    let mut url = go_parse(raw_url)?;
    let mut values = url.query();
    for (k, v) in q {
        values.set(k, v);
    }
    url.raw_query = values.encode();
    Ok(url.to_go_string())
}

/// The two ways [`parse_decrypted_action_cookie_payload`] can fail. Go returns
/// `encoding/json`'s error for both; the second is split out because serde would otherwise
/// *succeed* where Go fails — a derived `Deserialize` accepts a JSON **array** as a struct,
/// taking its elements as the fields in declaration order. Same trap as `Post::attachments`.
#[derive(Debug, thiserror::Error)]
pub enum CookiePayloadError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("json: cannot unmarshal {0} into Go value of type model.PostActionCookie")]
    NotAnObject(&'static str),
}

/// Port of `ParseDecryptedActionCookiePayload` (mm_blocks_actions.go:166).
///
/// Exactly one of the two is `Some` on success, chosen by a `kind` probe. The probe decodes into
/// a one-field struct, so it tolerates every other key — but three inputs are worth stating:
///
/// - a bare **`null`** succeeds and yields a *zero legacy cookie*. Go's `json.Unmarshal` returns
///   early on a JSON null without writing to the destination and reports no error, so both
///   decodes "succeed" on an empty document. Same shape as [D-023] on `time.Time`.
/// - a number, string, bool or **array** is an error, because the destination is a struct.
/// - a non-string `kind` is an error from the probe, before either cookie is attempted.
pub fn parse_decrypted_action_cookie_payload(
    decrypted: &str,
) -> Result<(Option<PostActionCookie>, Option<MmBlocksActionCookie>), CookiePayloadError> {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        kind: String,
    }

    let doc: serde_json::Value = serde_json::from_str(decrypted)?;
    match &doc {
        serde_json::Value::Null => return Ok((Some(PostActionCookie::default()), None)),
        serde_json::Value::Object(_) => {}
        serde_json::Value::Array(_) => return Err(CookiePayloadError::NotAnObject("array")),
        serde_json::Value::String(_) => return Err(CookiePayloadError::NotAnObject("string")),
        serde_json::Value::Number(_) => return Err(CookiePayloadError::NotAnObject("number")),
        serde_json::Value::Bool(_) => return Err(CookiePayloadError::NotAnObject("bool")),
    }

    let probe: Probe = serde_json::from_value(doc.clone())?;
    if probe.kind == MM_BLOCKS_ACTION_COOKIE_KIND {
        return Ok((None, Some(serde_json::from_value(doc)?)));
    }
    Ok((Some(serde_json::from_value(doc)?), None))
}

impl Post {
    /// Port of `(*Post).GetAction` (integration_action.go:1057).
    ///
    /// Lives here rather than in [`crate::integration_action`] because its second half is all
    /// mm_blocks: an attachment action wins if one matches, and otherwise an `external` mm_blocks
    /// spec is **synthesised** into a `PostAction` so the click pipeline does not have to branch
    /// on where the action came from.
    ///
    /// Three details are easy to lose:
    ///
    /// - the static query is merged into the URL **here**, and the per-click query is merged on
    ///   top by the caller, so a per-click key overrides a static one;
    /// - a malformed spec URL returns `None`, routing the caller through its ordinary
    ///   "action not found" 404 rather than firing a request with the query missing;
    /// - only `external` is synthesised. An `openURL` spec is not an action the server dispatches.
    pub fn get_action(&self, id: &str) -> Option<PostAction> {
        for attachment in self.attachments() {
            for action in attachment.actions {
                if action.id == id {
                    return Some(action);
                }
            }
        }

        let spec = self.get_mm_blocks_action_spec(id)?;
        if spec.spec_type != MM_BLOCKS_ACTION_TYPE_EXTERNAL || spec.url.is_empty() {
            return None;
        }

        let url = if spec.query.is_empty() {
            spec.url.clone()
        } else {
            merge_query_into_url(&spec.url, &spec.query).ok()?
        };

        Some(PostAction {
            id: id.to_string(),
            action_type: POST_ACTION_TYPE_BUTTON.to_string(),
            integration: Some(PostActionIntegration {
                url,
                context: spec.context,
            }),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_mm_blocks_actions.json"
        ))
        .unwrap()
    }

    fn section(o: &Value, key: &str) -> Vec<Value> {
        o.get(key).unwrap().as_array().unwrap().clone()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_str(&s(v, key)).unwrap()
    }

    /// Go's nil map and empty map marshal differently (`null` vs `{}`) and our
    /// [`StringInterface`] collapses them. That is safe here and nowhere near as safe as it
    /// sounds elsewhere: every consumer of a spec's `context` and `query` tests `len() > 0` or
    /// puts it behind an `omitempty`, so the two are indistinguishable downstream. The fixture
    /// records the distinction anyway, and this is where it is deliberately dropped.
    fn expected_map(case: &Value, key: &str, nil_key: &str) -> StringInterface {
        if b(case, nil_key) {
            return StringInterface::new();
        }
        serde_json::from_str(&s(case, key)).unwrap()
    }

    #[test]
    fn mm_blocks_context_map_matches_go() {
        for case in section(&oracle(), "context_map") {
            let name = s(&case, "name");
            let ours = mm_blocks_context_map(&s(&case, "in"));

            assert_eq!(ours.is_none(), b(&case, "nil"), "{name}: nil");
            if let Some(ours) = ours {
                let want: StringInterface = serde_json::from_str(&s(&case, "out")).unwrap();
                assert_eq!(ours, want, "{name}");
            }
        }
    }

    #[test]
    fn get_mm_blocks_action_spec_matches_go() {
        for case in section(&oracle(), "entry_to_spec") {
            let name = s(&case, "name");
            let post = post_from(&case, "post");
            let ours = post.get_mm_blocks_action_spec(&s(&case, "action_id"));

            assert_eq!(ours.is_none(), b(&case, "nil"), "{name}: nil");
            let Some(ours) = ours else { continue };

            assert_eq!(ours.spec_type, s(&case, "type"), "{name}: type");
            assert_eq!(ours.url, s(&case, "url"), "{name}: url");

            let want_query: StringMap = match case.get("query").unwrap() {
                Value::Null => StringMap::new(),
                v => serde_json::from_value(v.clone()).unwrap(),
            };
            assert_eq!(ours.query, want_query, "{name}: query");
            assert_eq!(
                ours.context,
                expected_map(&case, "context", "context_nil"),
                "{name}: context"
            );
        }
    }

    /// The synthesised action is what the click pipeline dispatches, so it is asserted as
    /// **marshalled JSON** rather than field by field — a shape drift and a logic drift then
    /// fail the same test.
    #[test]
    fn get_action_matches_go() {
        for case in section(&oracle(), "get_action") {
            let name = s(&case, "name");
            let post = post_from(&case, "post");
            let ours = post.get_action(&s(&case, "action_id"));

            assert_eq!(ours.is_none(), b(&case, "nil"), "{name}: nil");
            let want: Value = serde_json::from_str(&s(&case, "action")).unwrap();
            let got = match &ours {
                Some(action) => serde_json::to_value(action).unwrap(),
                None => Value::Null,
            };
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn cookie_action_spec_matches_go() {
        for case in section(&oracle(), "cookie_action_spec") {
            let name = s(&case, "name");
            let cookie: MmBlocksActionCookie = serde_json::from_str(&s(&case, "cookie")).unwrap();
            let ours = cookie.action_spec(&s(&case, "action_id"));

            assert_eq!(ours.is_none(), b(&case, "nil"), "{name}: nil");
            if let Some(ours) = ours {
                assert_eq!(ours.spec_type, s(&case, "type"), "{name}: type");
                assert_eq!(ours.url, s(&case, "url"), "{name}: url");
            }
        }
    }

    #[test]
    fn resolve_mm_blocks_action_matches_go() {
        for case in section(&oracle(), "resolve") {
            let name = s(&case, "name");

            let spec = if b(&case, "spec_nil") {
                None
            } else {
                let raw: Value = serde_json::from_str(&s(&case, "spec")).unwrap();
                Some(MmBlocksActionSpec {
                    spec_type: raw
                        .get("Type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: raw
                        .get("URL")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    query: match raw.get("Query") {
                        Some(Value::Object(_)) => {
                            serde_json::from_value(raw.get("Query").unwrap().clone()).unwrap()
                        }
                        _ => StringMap::new(),
                    },
                    context: match raw.get("Context") {
                        Some(Value::Object(m)) => m.clone(),
                        _ => StringInterface::new(),
                    },
                })
            };

            let client_query: StringMap = match case.get("client_query").unwrap() {
                Value::Null => StringMap::new(),
                v => serde_json::from_value(v.clone()).unwrap(),
            };

            let ours =
                resolve_mm_blocks_action(spec.as_ref(), &s(&case, "action_id"), &client_query);
            assert_eq!(ours.is_ok(), b(&case, "ok"), "{name}: ok");
            let Ok(ours) = ours else { continue };

            assert_eq!(
                ours.open_url_goto,
                s(&case, "open_url_goto"),
                "{name}: open_url_goto"
            );
            assert_eq!(
                ours.external_url,
                s(&case, "external_url"),
                "{name}: external_url"
            );
            let want_context: StringInterface = match s(&case, "context").as_str() {
                "null" | "" => StringInterface::new(),
                blob => serde_json::from_str(blob).unwrap(),
            };
            assert_eq!(ours.context, want_context, "{name}: context");
        }
    }

    #[test]
    fn parse_decrypted_action_cookie_payload_matches_go() {
        for case in section(&oracle(), "parse_cookie") {
            let name = s(&case, "name");
            let ours = parse_decrypted_action_cookie_payload(&s(&case, "in"));

            assert_eq!(ours.is_ok(), b(&case, "ok"), "{name}: ok");
            let Ok((legacy, mm_blocks)) = ours else {
                continue;
            };

            assert_eq!(
                legacy.is_none(),
                b(&case, "legacy_nil"),
                "{name}: legacy_nil"
            );
            assert_eq!(
                mm_blocks.is_none(),
                b(&case, "mm_blocks_nil"),
                "{name}: mm_blocks_nil"
            );
            if let Some(legacy) = legacy {
                let want: Value = serde_json::from_str(&s(&case, "legacy")).unwrap();
                assert_eq!(
                    serde_json::to_value(&legacy).unwrap(),
                    want,
                    "{name}: legacy"
                );
            }
            if let Some(mm_blocks) = mm_blocks {
                let want: Value = serde_json::from_str(&s(&case, "mm_blocks")).unwrap();
                assert_eq!(
                    serde_json::to_value(&mm_blocks).unwrap(),
                    want,
                    "{name}: mm_blocks"
                );
            }
        }
    }

    /// The one case in the corpus where the merge is skipped entirely — and the reason it
    /// matters is that skipping it also skips the normalisation. Driven directly here because
    /// `GetAction` reaches it only through a spec with no query.
    #[test]
    fn an_empty_query_returns_the_url_verbatim() {
        for raw in [
            "https://example.com/a%41b?z=1",
            "not a url at all",
            "https://a[b/",
            "",
        ] {
            assert_eq!(
                merge_query_into_url(raw, &StringMap::new()).unwrap(),
                raw,
                "{raw}"
            );
        }
    }
}
