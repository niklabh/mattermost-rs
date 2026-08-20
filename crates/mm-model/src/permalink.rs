//! Port of `model/permalink.go` — whole file: two wire types and `NewPreviewPost`.
//!
//! # The constructor guards one argument of three
//!
//! Go checks `post == nil` and returns nil, then dereferences `team` and `channel` without any
//! check at all. Measured, not read: a nil team **panics**, a nil channel **panics**, a nil post
//! returns nil, and `(nil, nil, nil)` returns nil because the post is tested first.
//!
//! The port makes those two panics unrepresentable rather than reproducing them — `team` and
//! `channel` are taken by reference, so there is no nil to pass. That is a deliberate divergence
//! from Go and the only one in this file: reproducing it would mean a `panic!` in library code,
//! which this project forbids, and a caller who would have crashed Go now cannot compile. The
//! *observable* behaviour on every input Go survives is identical. See [D-152].
//!
//! # Neither field carries `omitempty`
//!
//! `Permalink.PreviewPost` is a bare `*PreviewPost` with no `omitempty`, so an absent preview is
//! `{"preview_post":null}` and never `{}`. `PreviewPost.Post` is the same. `Option<T>` **without**
//! a skip predicate is what reproduces that, and it is the opposite of the convention seven of
//! `TeamSearch`'s fields follow.

use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::post::Post;
use crate::team::Team;

/// Port of `model.Permalink` (permalink.go:6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permalink {
    /// No `omitempty` — absent is `null`, never a missing key.
    #[serde(rename = "preview_post")]
    pub preview_post: Option<PreviewPost>,
}

/// Port of `model.PreviewPost` (permalink.go:10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewPost {
    #[serde(rename = "post_id")]
    pub post_id: String,

    /// No `omitempty`, as above.
    #[serde(rename = "post")]
    pub post: Option<Post>,

    #[serde(rename = "team_name")]
    pub team_name: String,

    #[serde(rename = "channel_display_name")]
    pub channel_display_name: String,

    /// `ChannelType` is a defined string type that accepts anything, and there is no `IsValid`
    /// here to narrow it afterwards — so `String`, exactly as [`crate::post_info::PostInfo`] does.
    #[serde(rename = "channel_type")]
    pub channel_type: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,
}

/// Port of `model.NewPreviewPost` (permalink.go:19).
///
/// `team` and `channel` are references because Go dereferences them unguarded — see the module
/// docs and [D-152]. `post` stays optional because Go's nil check is real behaviour a caller
/// depends on.
pub fn new_preview_post(
    post: Option<&Post>,
    team: &Team,
    channel: &Channel,
) -> Option<PreviewPost> {
    let post = post?;
    Some(PreviewPost {
        post_id: post.id.clone(),
        post: Some(post.clone()),
        team_name: team.name.clone(),
        channel_display_name: channel.display_name.clone(),
        channel_type: channel.channel_type.clone(),
        channel_id: channel.id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_small_types.json")).unwrap()
    }

    #[test]
    fn serialization_parity_with_the_fixtures() {
        for raw in [
            include_str!("../../../fixtures/permalink.json"),
            include_str!("../../../fixtures/preview_post.json"),
        ] {
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            // Whichever type the fixture is, the round trip has to be exact.
            let actual = if expected.get("preview_post").is_some() {
                serde_json::to_value(serde_json::from_str::<Permalink>(raw).unwrap()).unwrap()
            } else {
                serde_json::to_value(serde_json::from_str::<PreviewPost>(raw).unwrap()).unwrap()
            };
            assert_eq!(actual, expected);
        }
    }

    /// **No `omitempty` on either pointer**, so an empty preview is `null` and not a dropped key.
    #[test]
    fn go_parity_the_zero_values_carry_null_rather_than_dropping_the_key() {
        let corpus = corpus();
        assert_eq!(
            serde_json::to_string(&Permalink::default()).unwrap(),
            corpus["zero_values"]["permalink"].as_str().unwrap()
        );
        assert_eq!(
            serde_json::to_string(&PreviewPost::default()).unwrap(),
            corpus["zero_values"]["preview_post"].as_str().unwrap()
        );
    }

    /// **The nil-post guard, against Go's own answers**, plus the two cases where Go panics.
    ///
    /// The panicking rows are asserted as panicking — that is what makes the divergence in
    /// [`new_preview_post`]'s signature visible rather than silently assumed.
    #[test]
    fn go_parity_the_constructors_nil_handling() {
        let corpus = corpus();
        let rows = corpus["preview_post"].as_array().unwrap();

        let post = Post {
            id: "p1".to_owned(),
            message: "hello".to_owned(),
            ..Default::default()
        };
        let team = Team {
            name: "core".to_owned(),
            ..Default::default()
        };
        let channel = Channel {
            id: "c1".to_owned(),
            display_name: "Town Square".to_owned(),
            channel_type: "O".to_owned(),
            ..Default::default()
        };

        let mut checked = 0;
        for row in rows {
            let name = row["name"].as_str().unwrap();
            let panicked = row["panicked"].as_bool().unwrap_or(false);

            match name {
                // Go survives these, and so must we — with the same answer.
                "all_present" => {
                    assert!(!panicked);
                    let ours = new_preview_post(Some(&post), &team, &channel)
                        .expect("a present post yields a preview");
                    let expected: serde_json::Value =
                        serde_json::from_str(row["out"].as_str().unwrap()).unwrap();
                    assert_eq!(serde_json::to_value(&ours).unwrap(), expected);
                    assert_eq!(row["nil_result"].as_bool(), Some(false));
                    checked += 1;
                }
                "nil_post" | "all_nil" => {
                    assert!(!panicked, "{name}: the post guard is real");
                    assert_eq!(row["nil_result"].as_bool(), Some(true));
                    assert!(
                        new_preview_post(None, &team, &channel).is_none(),
                        "{name}: a nil post is a nil preview"
                    );
                    checked += 1;
                }
                // Go crashes on these. Ours cannot be called this way at all.
                "nil_team" | "nil_channel" => {
                    assert!(
                        panicked,
                        "{name}: Go dereferences this argument unguarded — if it stopped, the \
                         divergence in our signature needs revisiting ([D-152])"
                    );
                    checked += 1;
                }
                other => panic!("unknown corpus row {other}"),
            }
        }
        assert_eq!(checked, 5, "every corpus row must be accounted for");
    }

    /// The wrapper around a real preview, and around none.
    #[test]
    fn the_permalink_wrapper_round_trips_both_ways() {
        let empty = Permalink { preview_post: None };
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"preview_post":null}"#
        );

        let decoded: Permalink = serde_json::from_str(r#"{"preview_post":null}"#).unwrap();
        assert_eq!(decoded, empty);

        let filled = Permalink {
            preview_post: Some(PreviewPost {
                post_id: "p1".to_owned(),
                channel_type: "O".to_owned(),
                ..Default::default()
            }),
        };
        let round: Permalink =
            serde_json::from_str(&serde_json::to_string(&filled).unwrap()).unwrap();
        assert_eq!(round, filled);
    }

    /// `channel_type` is a `String`, so a value no Go server writes is still a decode and not an
    /// error — the same call `post_info.rs` makes.
    #[test]
    fn an_unknown_channel_type_decodes() {
        let decoded: PreviewPost =
            serde_json::from_str(r#"{"channel_type":"NOT_A_TYPE"}"#).unwrap();
        assert_eq!(decoded.channel_type, "NOT_A_TYPE");
    }
}
