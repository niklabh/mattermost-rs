//! Port of `model/channel_data.go` (channel_data.go:1–18) — **whole file**.
//!
//! Two nillable pointer fields and one `Etag`.
//!
//! # The method guards one pointer and dereferences the other, three lines apart
//!
//! ```go
//! func (o *ChannelData) Etag() string {
//!     var mt int64
//!     if o.Member != nil { mt = o.Member.LastUpdateAt }
//!     return Etag(o.Channel.Id, o.Channel.UpdateAt, o.Channel.LastPostAt, mt)
//! }
//! ```
//!
//! `Member` gets a nil check and `Channel` does not, so a nil member yields an etag whose fourth
//! component is `0` and a nil channel **crashes the Go server**. That is reachable rather than
//! exotic: neither field has `omitempty`, so `{}`, `{"channel":null}` and any document carrying
//! only a member all decode to a nil channel, and `ChannelData{}` from any code path has both nil.
//! Measured under `recover`, not inferred — see [D-072].
//!
//! Ours answers instead, with the etag Go itself produces for a **zero-valued** channel
//! (`11.11.0..0.0.<mt>`). That value is measured rather than invented: the corpus runs a
//! `&Channel{}` through the same method in Go and records the answer.
//!
//! # Only four fields reach the etag, and one of them is the member's
//!
//! Three come from the channel — `id`, `update_at`, `last_post_at` — and exactly one from the
//! member, `last_update_at`. Everything else is invisible to cache invalidation. Measured across
//! nine mutations that change **nothing**: the member's `roles`, `last_viewed_at`, `msg_count`,
//! `mention_count` and `notify_props`, and the channel's `display_name`, `total_msg_count`,
//! `delete_at` and `create_at`. Each is a real change a client would want to see, and each leaves
//! the etag byte-identical, so a client holding the old one will not refetch.
//!
//! That is upstream's behaviour and not a bug we introduce — but it is the kind of thing an app
//! layer port will be tempted to "fix" by adding fields, which would make the two servers hand
//! out different etags for the same row.

use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::channel_member::ChannelMember;
use crate::utils::etag;

/// Port of `model.ChannelData` (channel_data.go:6).
///
/// Both fields are pointers without `omitempty`, so a nil one is `null` on the wire rather than
/// an absent key, and the zero value is `{"channel":null,"member":null}` rather than `{}`.
///
/// This is a plain struct, **not** an embedded-pointer case like
/// [`crate::post_search_results::PostSearchResults`] — Go names both fields and tags them, so the
/// objects nest and there is nothing to flatten.
///
/// The container carries `#[serde(default)]` because Go leaves an absent field at its zero value
/// — see [D-043].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelData {
    #[serde(rename = "channel")]
    pub channel: Option<Channel>,

    #[serde(rename = "member")]
    pub member: Option<ChannelMember>,
}

impl ChannelData {
    /// Port of `(*ChannelData).Etag` (channel_data.go:11).
    ///
    /// Four components after the version prefix: the channel's `id`, `update_at` and
    /// `last_post_at`, then the member's `last_update_at` — or `0` when there is no member, which
    /// is Go's own guard.
    ///
    /// **Go panics when `channel` is nil; this answers with the zero-channel etag** ([D-072]).
    /// The value is not invented — a `&Channel{}` produces exactly it in Go — so a nil channel and
    /// a zero-valued one become indistinguishable here, where in Go one crashes and one does not.
    ///
    /// `Etag` escapes nothing, so an `id` containing a dot silently changes the component count:
    /// `a.b` yields eight dot-separated parts where every other channel yields seven. Reproduced;
    /// see note 5 under `model/channel_list.go`.
    pub fn etag(&self) -> String {
        let member_time = self.member.as_ref().map_or(0, |m| m.last_update_at);

        let (id, update_at, last_post_at) = self
            .channel
            .as_ref()
            .map_or(("", 0, 0), |c| (c.id.as_str(), c.update_at, c.last_post_at));

        etag(&[&id, &update_at, &last_post_at, &member_time])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    fn channel() -> Channel {
        Channel {
            id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            update_at: 200,
            last_post_at: 300,
            ..Default::default()
        }
    }

    #[test]
    fn the_zero_value_is_two_nulls() {
        assert_eq!(
            go_json_marshal(&ChannelData::default()).unwrap(),
            r#"{"channel":null,"member":null}"#
        );
    }

    #[test]
    fn a_nil_member_contributes_a_zero() {
        let data = ChannelData {
            channel: Some(channel()),
            member: None,
        };
        assert_eq!(data.etag(), "11.11.0.qr6kf7ztp7yifxt4wm5xn51bke.200.300.0");
    }

    /// The guard makes no difference for a zero-valued member: both contribute `0`.
    #[test]
    fn a_zero_member_and_a_nil_member_agree() {
        let with_zero = ChannelData {
            channel: Some(channel()),
            member: Some(ChannelMember::default()),
        };
        let with_none = ChannelData {
            channel: Some(channel()),
            member: None,
        };
        assert_eq!(with_zero.etag(), with_none.etag());
    }

    /// The divergence, stated as a value: Go crashes here and we answer.
    #[test]
    fn a_nil_channel_answers_rather_than_panicking() {
        assert_eq!(ChannelData::default().etag(), "11.11.0..0.0.0");

        let member_only = ChannelData {
            channel: None,
            member: Some(ChannelMember {
                last_update_at: 500,
                ..Default::default()
            }),
        };
        assert_eq!(member_only.etag(), "11.11.0..0.0.500");
    }

    /// `Etag` joins with `.` and escapes nothing, so a dotted id adds a component.
    #[test]
    fn a_dotted_id_changes_the_component_count() {
        let data = ChannelData {
            channel: Some(Channel {
                id: "a.b".into(),
                update_at: 200,
                last_post_at: 300,
                ..Default::default()
            }),
            member: None,
        };
        assert_eq!(data.etag(), "11.11.0.a.b.200.300.0");
        assert_eq!(data.etag().split('.').count(), 8, "one more than usual");
    }

    #[test]
    fn a_partial_document_decodes() {
        let got: ChannelData = serde_json::from_str(r#"{"member":null}"#).unwrap();
        assert!(got.channel.is_none() && got.member.is_none());
    }
}

/// Serialization parity against `fixtures/channel_data.json` — the reflection-populated oracle,
/// every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/channel_data.json");
        let decoded: ChannelData = serde_json::from_str(raw).unwrap();

        // Both pointers are populated, so neither nil branch is what this asserts.
        assert!(decoded.channel.is_some() && decoded.member.is_some());
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::from_str::<serde_json::Value>(raw).unwrap()
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_data.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_data.json"
        ))
        .unwrap()
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert_eq!(cases.len(), 6, "the wire corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let out = &case["out"];
            let decoded: ChannelData =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                out["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                decoded.channel.is_none(),
                out["channel_nil"].as_bool().unwrap(),
                "{name}: channel nil"
            );
            assert_eq!(
                decoded.member.is_none(),
                out["member_nil"].as_bool().unwrap(),
                "{name}: member nil"
            );
        }
    }

    /// The asymmetry, driven whole. Three of the twelve documents crash Go; the Rust side asserts
    /// the divergence explicitly rather than skipping them, and pins the answer we give against
    /// the one Go gives for a **zero** channel — which is the same code path there.
    #[test]
    fn etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["etag"].as_array().unwrap();
        assert_eq!(cases.len(), 11, "the etag corpus changed size");

        // What Go answers for `{"channel":{}}`: the value our nil-channel path reproduces.
        let zero_channel_etag = cases
            .iter()
            .find(|c| c["name"] == "both_zero")
            .and_then(|c| c["etag"].as_str())
            .expect("both_zero is missing from the corpus");
        assert_eq!(zero_channel_etag, "11.11.0..0.0.0");

        let mut panics = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let doc = case["in"].as_str().unwrap();
            let decoded: ChannelData =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            if case["panicked"].as_bool().unwrap() {
                panics += 1;
                // Go crashed, so there is no etag to compare against. What is assertable is that
                // it crashed for the reason [D-072] names — a nil channel — and that we answer
                // with the zero-channel form.
                assert!(
                    case["channel_nil"].as_bool().unwrap(),
                    "{name}: Go panicked with a non-nil channel — [D-072] is wrong about why"
                );
                assert!(decoded.channel.is_none(), "{name}");

                let member_time = decoded.member.as_ref().map_or(0, |m| m.last_update_at);
                assert_eq!(
                    decoded.etag(),
                    format!("11.11.0..0.0.{member_time}"),
                    "{name}"
                );
                continue;
            }

            assert_eq!(decoded.etag(), case["etag"].as_str().unwrap(), "{name}");
        }

        assert_eq!(panics, 3, "the nil-channel cases changed count");
    }

    /// Which fields reach the etag, and — more usefully — which do not. Nine of the twelve
    /// mutations are real changes that leave the etag identical.
    #[test]
    fn only_four_fields_reach_the_etag() {
        let oracle = oracle();
        let cases = oracle["etag_parts"].as_array().unwrap();
        assert_eq!(cases.len(), 14, "the mutation corpus changed size");

        // The mutations Go says are invisible. Named rather than counted, so a field becoming
        // visible upstream fails here with the field's name in the message.
        const INVISIBLE: [&str; 9] = [
            "member_roles",
            "member_last_viewed_at",
            "member_msg_count",
            "member_mention_count",
            "member_notify_props",
            "channel_display_name",
            "channel_total_msg_count",
            "channel_delete_at",
            "channel_create_at",
        ];

        let baseline = cases
            .iter()
            .find(|c| c["name"] == "baseline")
            .and_then(|c| c["etag"].as_str())
            .expect("baseline is missing");

        let mut invisible_seen = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let decoded: ChannelData =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            let ours = decoded.etag();
            assert_eq!(ours, case["etag"].as_str().unwrap(), "{name}");
            assert_eq!(
                ours != baseline,
                case["differs_from_baseline"].as_bool().unwrap(),
                "{name}: differs-from-baseline"
            );

            if INVISIBLE.contains(&name) {
                invisible_seen += 1;
                assert_eq!(
                    ours, baseline,
                    "{name} became visible to the etag — upstream changed"
                );
            }
        }

        assert_eq!(invisible_seen, INVISIBLE.len());
    }
}
