//! Port of `server/public/model/mention_map.go` (80 lines) — **whole file**.
//!
//! Two `map[string]string` newtypes and the codec that moves them through URL query parameters.
//! Clients send a resolved mention→id map alongside a post so the server does not have to
//! re-resolve `@alice` or `~town-square`; the map rides in the query string as two parallel
//! repeated parameters, paired **by index**.
//!
//! # Two named types, not one alias
//!
//! Go declares `UserMentionMap` and `ChannelMentionMap` as distinct types over the same
//! underlying map, and the only difference between them is which pair of query keys they read
//! and write. A single alias would compile and would let a channel map encode itself under
//! `user_mentions`, which no test would catch, so both are `#[serde(transparent)]` newtypes —
//! the same shape [`crate::channel_list`] uses for Go's named slices.
//!
//! # The four key names are unexported in Go
//!
//! `userMentionsKey` and its three siblings are lowercase constants, so the oracle cannot read
//! them from the package. It recovers them the way the API does — by encoding a one-entry map —
//! and the Rust constants are asserted against that rather than transcribed on trust. Same
//! technique `version.go`'s unexported release table needed, one level cheaper.
//!
//! # `ToURLValues` output order is random in Go
//!
//! `mentionsToURLValues` ranges a Go map, and Go randomises map iteration. `Values::encode` sorts
//! by *key*, but there are only two keys here and the slice under each preserves insertion order
//! — so a two-entry mention map encodes two different ways from one input, and a three-entry one
//! six ways. Ours is a `BTreeMap`, so it always emits the sorted-by-mention order.
//!
//! That is a **narrowing**, not a divergence in the usual sense: every ordering Go can produce
//! decodes back to the same map, because [`Self::from_url_values`] pairs by index and the two
//! slices are permuted together. The oracle asserts exactly that — `round_trips` is true for
//! every corpus entry — and pins our ordering against a Go-built sorted encoding. See [D-063].
//!
//! # Three shapes of "missing", and only two of them are errors
//!
//! | query | Go |
//! |---|---|
//! | neither key present | **success**, an empty (non-nil) map |
//! | one key present, the other absent | error naming the absent one |
//! | both present, different lengths | error naming both |
//! | both present, both zero-length | **success**, an empty map |
//!
//! Reading the first row as an error would turn every mention-free request into a 400.

use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Serialize};

use crate::go_url::Values;
use crate::utils::StringMap;

/// Port of `model.userMentionsKey` (mention_map.go:15). Unexported in Go; pinned by the oracle.
pub const USER_MENTIONS_KEY: &str = "user_mentions";
/// Port of `model.userMentionsIdsKey` (mention_map.go:16).
pub const USER_MENTIONS_IDS_KEY: &str = "user_mentions_ids";
/// Port of `model.channelMentionsKey` (mention_map.go:17).
pub const CHANNEL_MENTIONS_KEY: &str = "channel_mentions";
/// Port of `model.channelMentionsIdsKey` (mention_map.go:18).
pub const CHANNEL_MENTIONS_IDS_KEY: &str = "channel_mentions_ids";

/// The failure modes of `mentionsFromURLValues` (mention_map.go:37).
///
/// Go returns a bare `fmt.Errorf` for the first three; the messages are reproduced verbatim
/// because they reach a client in a 400 body. The fourth has no Go counterpart — see [D-064].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MentionMapError {
    #[error("{0} key not found")]
    KeyNotFound(&'static str),

    #[error("keys {0} and {1} have different length")]
    DifferentLength(&'static str, &'static str),

    #[error("key {mention} has two different values: {old_id} and {id}")]
    Conflict {
        mention: String,
        old_id: String,
        id: String,
    },

    /// A query parameter that is not valid UTF-8. Go's `url.Values` holds `string`, which is a
    /// byte sequence, so `?user_mentions=%80` builds a perfectly good map there. Rust's `String`
    /// cannot. See [D-064].
    #[error("{0} value is not valid UTF-8")]
    NotUtf8(&'static str),
}

/// Port of `model.UserMentionMap` (mention_map.go:11).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserMentionMap(pub StringMap);

/// Port of `model.ChannelMentionMap` (mention_map.go:12).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelMentionMap(pub StringMap);

impl UserMentionMap {
    /// Port of `model.UserMentionMapFromURLValues` (mention_map.go:21).
    pub fn from_url_values(values: &Values) -> Result<Self, MentionMapError> {
        mentions_from_url_values(values, USER_MENTIONS_KEY, USER_MENTIONS_IDS_KEY).map(Self)
    }

    /// Port of `(UserMentionMap).ToURLValues` (mention_map.go:25).
    pub fn to_url_values(&self) -> Values {
        mentions_to_url_values(&self.0, USER_MENTIONS_KEY, USER_MENTIONS_IDS_KEY)
    }
}

impl ChannelMentionMap {
    /// Port of `model.ChannelMentionMapFromURLValues` (mention_map.go:29).
    pub fn from_url_values(values: &Values) -> Result<Self, MentionMapError> {
        mentions_from_url_values(values, CHANNEL_MENTIONS_KEY, CHANNEL_MENTIONS_IDS_KEY).map(Self)
    }

    /// Port of `(ChannelMentionMap).ToURLValues` (mention_map.go:33).
    pub fn to_url_values(&self) -> Values {
        mentions_to_url_values(&self.0, CHANNEL_MENTIONS_KEY, CHANNEL_MENTIONS_IDS_KEY)
    }
}

/// Port of `model.mentionsFromURLValues` (mention_map.go:37).
///
/// The `ok` flags are Go's two-value map read, which [`Values::get_all`] reproduces — a key
/// present with a zero-length slice is *present*, and takes the length comparison rather than the
/// not-found branch.
fn mentions_from_url_values(
    values: &Values,
    mention_key: &'static str,
    id_key: &'static str,
) -> Result<StringMap, MentionMapError> {
    let mentions = values.get_all(mention_key);
    let ids = values.get_all(id_key);

    let (mentions, ids) = match (mentions, ids) {
        // Neither key present: success with an empty map, NOT an error.
        (None, None) => return Ok(StringMap::new()),
        (None, Some(_)) => return Err(MentionMapError::KeyNotFound(mention_key)),
        (Some(_), None) => return Err(MentionMapError::KeyNotFound(id_key)),
        (Some(mentions), Some(ids)) => (mentions, ids),
    };

    if mentions.len() != ids.len() {
        return Err(MentionMapError::DifferentLength(mention_key, id_key));
    }

    let mut mentions_map = StringMap::new();
    for (mention, id) in mentions.iter().zip(ids) {
        // Go's url.Values holds byte strings; ours must be UTF-8. [D-064].
        let mention =
            std::str::from_utf8(mention).map_err(|_| MentionMapError::NotUtf8(mention_key))?;
        let id = std::str::from_utf8(id).map_err(|_| MentionMapError::NotUtf8(id_key))?;

        // Go: `if oldId, ok := mentionsMap[mention]; ok && oldId != id`. A repeat with the SAME
        // id is not an error — it collapses silently.
        if let Some(old_id) = mentions_map.get(mention) {
            if old_id != id {
                return Err(MentionMapError::Conflict {
                    mention: mention.to_string(),
                    old_id: old_id.clone(),
                    id: id.to_string(),
                });
            }
        }

        mentions_map.insert(mention.to_string(), id.to_string());
    }

    Ok(mentions_map)
}

/// Port of `model.mentionsToURLValues` (mention_map.go:71).
///
/// Go ranges its map, so its pair order is random; a `BTreeMap` makes ours sorted by mention.
/// The two agree on content and on the round trip, never necessarily on `Values::encode`'s
/// output for two or more entries. See the module docs and [D-063].
fn mentions_to_url_values(
    mentions: &StringMap,
    mention_key: &'static str,
    id_key: &'static str,
) -> Values {
    let mut values = Values::new();

    for (mention, id) in mentions {
        values.add(mention_key.as_bytes(), mention.as_bytes());
        values.add(id_key.as_bytes(), id.as_bytes());
    }

    values
}

impl Deref for UserMentionMap {
    type Target = StringMap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for UserMentionMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for ChannelMentionMap {
    type Target = StringMap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChannelMentionMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromIterator<(String, String)> for UserMentionMap {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl FromIterator<(String, String)> for ChannelMentionMap {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> Values {
        let mut v = Values::new();
        for (k, value) in pairs {
            v.add(k.as_bytes(), value.as_bytes());
        }
        v
    }

    #[test]
    fn neither_key_is_success_not_an_error() {
        let v = values(&[("page", "1")]);
        assert_eq!(
            UserMentionMap::from_url_values(&v).unwrap().0,
            StringMap::new()
        );
    }

    #[test]
    fn one_key_without_the_other_names_the_missing_one() {
        assert_eq!(
            UserMentionMap::from_url_values(&values(&[(USER_MENTIONS_KEY, "a")])).unwrap_err(),
            MentionMapError::KeyNotFound(USER_MENTIONS_IDS_KEY)
        );
        assert_eq!(
            UserMentionMap::from_url_values(&values(&[(USER_MENTIONS_IDS_KEY, "i")])).unwrap_err(),
            MentionMapError::KeyNotFound(USER_MENTIONS_KEY)
        );
    }

    #[test]
    fn a_repeat_is_an_error_only_when_the_ids_disagree() {
        let same = values(&[
            (USER_MENTIONS_KEY, "a"),
            (USER_MENTIONS_KEY, "a"),
            (USER_MENTIONS_IDS_KEY, "id"),
            (USER_MENTIONS_IDS_KEY, "id"),
        ]);
        assert_eq!(UserMentionMap::from_url_values(&same).unwrap().len(), 1);

        let differ = values(&[
            (USER_MENTIONS_KEY, "a"),
            (USER_MENTIONS_KEY, "a"),
            (USER_MENTIONS_IDS_KEY, "one"),
            (USER_MENTIONS_IDS_KEY, "two"),
        ]);
        assert_eq!(
            UserMentionMap::from_url_values(&differ)
                .unwrap_err()
                .to_string(),
            "key a has two different values: one and two"
        );
    }

    #[test]
    fn the_two_types_read_different_keys() {
        let v = values(&[
            (USER_MENTIONS_KEY, "u"),
            (USER_MENTIONS_IDS_KEY, "ui"),
            (CHANNEL_MENTIONS_KEY, "c"),
            (CHANNEL_MENTIONS_IDS_KEY, "ci"),
        ]);
        assert_eq!(
            UserMentionMap::from_url_values(&v)
                .unwrap()
                .get("u")
                .unwrap(),
            "ui"
        );
        assert_eq!(
            ChannelMentionMap::from_url_values(&v)
                .unwrap()
                .get("c")
                .unwrap(),
            "ci"
        );
    }

    #[test]
    fn it_serialises_as_a_bare_object() {
        let m: UserMentionMap = [("a".to_string(), "b".to_string())].into_iter().collect();
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"a":"b"}"#);
    }
}

/// Parity tests driven by `fixtures/behaviour_mention_map.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_mention_map.json")).unwrap()
    }

    /// Rebuilds a `url.Values` from the fixture's `map[string][]string`. A key present with an
    /// empty array survives, which is the point — that state drives a different branch.
    fn values_of(v: &Value) -> Values {
        let mut values = Values::new();
        let Some(map) = v.as_object() else {
            return values;
        };
        for (key, list) in map {
            // Go's `values[k] = []string{…}`, including the zero-length case that `add` cannot
            // reach and that the `ok`-then-`len` branch treats as present.
            values.set_all(
                key,
                list.as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item.as_str().unwrap().as_bytes().to_vec())
                    .collect(),
            );
        }
        values
    }

    fn string_map_of(v: &Value) -> StringMap {
        v.as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The four constants are unexported in Go, so this is the only thing standing between the
    /// port and a silent typo in a query key.
    #[test]
    fn the_key_constants_match_go() {
        let oracle = oracle();
        let keys = &oracle["keys"];

        assert_eq!(
            keys["user_keys"].as_array().unwrap(),
            &[
                Value::String(USER_MENTIONS_KEY.to_string()),
                Value::String(USER_MENTIONS_IDS_KEY.to_string()),
            ]
        );
        assert_eq!(
            keys["channel_keys"].as_array().unwrap(),
            &[
                Value::String(CHANNEL_MENTIONS_KEY.to_string()),
                Value::String(CHANNEL_MENTIONS_IDS_KEY.to_string()),
            ]
        );

        // The encodings the names were recovered from, reproduced end to end.
        let user: UserMentionMap = [("m".to_string(), "i".to_string())].into_iter().collect();
        assert_eq!(
            user.to_url_values().encode(),
            keys["user_encoded"].as_str().unwrap()
        );
        let channel: ChannelMentionMap = [("m".to_string(), "i".to_string())].into_iter().collect();
        assert_eq!(
            channel.to_url_values().encode(),
            keys["channel_encoded"].as_str().unwrap()
        );
    }

    #[test]
    fn from_url_values_matches_go() {
        let oracle = oracle();
        let cases = oracle["from_values"].as_array().unwrap();
        assert!(cases.len() > 20, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let values = values_of(&case["in"]);

            match UserMentionMap::from_url_values(&values) {
                Ok(map) => {
                    assert_eq!(case["user_err"].as_str().unwrap(), "", "{name}: Go errored");
                    assert_eq!(map.0, string_map_of(&case["user"]), "{name}: user map");
                    // Go returns nil only on an error path; a success is always allocated.
                    assert!(
                        !case["user_nil"].as_bool().unwrap(),
                        "{name}: Go returned nil"
                    );
                }
                Err(err) => {
                    assert_eq!(
                        err.to_string(),
                        case["user_err"].as_str().unwrap(),
                        "{name}"
                    );
                    assert!(
                        case["user_nil"].as_bool().unwrap(),
                        "{name}: Go returned a map"
                    );
                }
            }

            match ChannelMentionMap::from_url_values(&values) {
                Ok(map) => {
                    assert_eq!(
                        case["channel_err"].as_str().unwrap(),
                        "",
                        "{name}: Go errored"
                    );
                    assert_eq!(
                        map.0,
                        string_map_of(&case["channel"]),
                        "{name}: channel map"
                    );
                    assert!(!case["channel_nil"].as_bool().unwrap(), "{name}");
                }
                Err(err) => {
                    assert_eq!(
                        err.to_string(),
                        case["channel_err"].as_str().unwrap(),
                        "{name}"
                    );
                    assert!(case["channel_nil"].as_bool().unwrap(), "{name}");
                }
            }
        }
    }

    #[test]
    fn to_url_values_matches_go() {
        let oracle = oracle();
        let cases = oracle["to_values"].as_array().unwrap();
        assert!(!cases.is_empty());

        let mut deterministic = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let map = string_map_of(&case["in"]);
            let user = UserMentionMap(map.clone());
            let channel = ChannelMentionMap(map.clone());

            // Our BTreeMap emits sorted-by-mention order, which is what `encode_sorted` is.
            assert_eq!(
                user.to_url_values().encode(),
                case["user_encode_sorted"].as_str().unwrap(),
                "{name}: user"
            );
            assert_eq!(
                channel.to_url_values().encode(),
                case["channel_encode_sorted"].as_str().unwrap(),
                "{name}: channel"
            );

            // Where Go's own iteration cannot reorder anything, the sorted construction must
            // equal what Go actually emitted — otherwise `encode_sorted` is a fiction.
            if case["deterministic"].as_bool().unwrap() {
                assert_eq!(
                    case["user_encode_sorted"], case["user_encode_actual"],
                    "{name}: Go disagreed with the sorted construction"
                );
                assert_eq!(
                    case["channel_encode_sorted"], case["channel_encode_actual"],
                    "{name}"
                );
                deterministic += 1;
            }

            // The property that makes Go's randomness harmless: every ordering decodes back.
            assert!(
                case["round_trips"].as_bool().unwrap(),
                "{name}: Go lost data"
            );
            assert_eq!(
                UserMentionMap::from_url_values(&user.to_url_values())
                    .unwrap()
                    .0,
                map,
                "{name}: round trip"
            );
        }
        assert!(deterministic >= 8, "lost the deterministic cases");
    }

    /// [D-063] stated as a test rather than only as prose: Go can emit an ordering we never do,
    /// and it decodes to the same map. Built by hand because the oracle may not record a random
    /// value ([D-032]).
    #[test]
    fn a_reversed_go_ordering_decodes_to_the_same_map() {
        let map: StringMap = [
            ("alice".to_string(), "id-a".to_string()),
            ("bob".to_string(), "id-b".to_string()),
        ]
        .into_iter()
        .collect();

        let mut reversed = Values::new();
        for (mention, id) in map.iter().rev() {
            reversed.add(USER_MENTIONS_KEY.as_bytes(), mention.as_bytes());
            reversed.add(USER_MENTIONS_IDS_KEY.as_bytes(), id.as_bytes());
        }

        // A different encoding from ours...
        assert_ne!(
            reversed.encode(),
            UserMentionMap(map.clone()).to_url_values().encode()
        );
        // ...and the same map back.
        assert_eq!(UserMentionMap::from_url_values(&reversed).unwrap().0, map);
    }
}
