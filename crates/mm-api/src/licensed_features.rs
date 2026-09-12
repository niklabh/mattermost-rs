//! Three route families whose entire behaviour on an unlicensed server is one refusal, taken
//! **before every other check**: `api4/content_flagging.go`, the four write halves of
//! `api4/channel_bookmark.go`, and the two post-acknowledgement routes of `api4/post.go`.
//!
//! # Why these three are in one module
//!
//! They share a shape that nothing else migrated so far does. `data_retention.rs` is a family of
//! refusals with *live* checks in front of them — a body decode, a permission, an id — and
//! reproducing that ordering is most of the work. Here the licence test is the **first statement
//! of every handler**, so nothing else on the request is ever consulted: not the body, not the
//! permission, not the id. A caller with no permission and a malformed body gets the same 501 an
//! administrator does.
//!
//! Keeping them together says that out loud. Splitting them into `content_flagging.rs` and four
//! more handlers in `channels.rs` would invite someone to add a permission check "for symmetry"
//! with the neighbours, which would be a divergence on every request.
//!
//! The acknowledgement pair is the sharpest case of that. `acknowledgePost` and
//! `unacknowledgePost` sit in `api4/post.go` between handlers that all begin with
//! `c.RequirePostId().RequireUserId()`, and both of *these* put the licence test **above** that
//! line — so a malformed `{post_id}`, a `{user_id}` the caller may not act for, and a post in a
//! channel the caller cannot read all answer the same 501. Ported beside their shape-mates rather
//! than beside their path-mates in `post_writes.rs`, where the neighbours would have argued for
//! the checks Go skips.
//!
//! # The three gates are not the same test, and the difference is invisible here
//!
//! Content flagging needs `MinimumEnterpriseAdvancedLicense` — a licence **tier**, not merely a
//! licence (license.go:515), so an Enterprise licence below the Advanced tier is still refused.
//! Acknowledgements need `MinimumProfessionalLicense` (license.go:504), a lower rung of the same
//! ladder. Channel bookmarks need only `License() != nil`. All three collapse to "refuse" when
//! there is no licence at all, which is the only case this server answers; a licensed
//! installation is forwarded and Go applies whichever test is really its own.
//!
//! # The acknowledgement pair does not share an error id, and one of them has no id at all
//!
//! The two handlers are four lines apart and their refusals differ:
//!
//! | route | `id` on the wire |
//! |---|---|
//! | `POST   /users/{user_id}/posts/{post_id}/ack` | `<untranslated>` — `model.NoTranslation` |
//! | `DELETE /users/{user_id}/posts/{post_id}/ack` | `license_error.feature_unavailable` |
//!
//! Both carry the same `detailed_error` in Go ("feature is not available for the current
//! license"), and both have it wiped before it reaches a client, so the id is the *only* thing
//! separating them. A port that assumed the pair matched would be wrong on exactly one of the
//! two, in the field clients branch on. `<untranslated>` also contains `<` and `>`, so it
//! reaches the wire as `\u003cuntranslated\u003e` — see [`crate::error::ApiError::into_wire`].
//!
//! Go passes `""` as the `where` for both, not the handler name. `AppError.Where` is
//! `json:"-"`, so this is invisible to a client; the handler names are kept here because they
//! are what a trace needs.
//!
//! # The `GET` on the bookmark collection is gated too, and is already ported
//!
//! `listChannelBookmarksForChannel` opens with the same `License() == nil` test as the four
//! writes; it lives in `channels.rs`, which reached it first, alongside two other channel routes
//! with the same shape and different error ids. So all **five** bookmark routes are refusals, and
//! this module holds four of them. The first version of the parity suite asserted the `GET` was an
//! ordinary read — measured at 501, which is the reason that assertion is now the opposite.
//!
//! # The refusal that is *not* ours
//!
//! `requireContentFlaggingEnabled` has a second arm — `ContentFlaggingSettings.EnableContentFlagging`
//! — answering `api.data_spillage.error.disabled` at the same status. It is reached only after the
//! licence test passes, so this server can never produce it. Recorded because the two ids differ
//! by one word and a reader comparing the two servers' logs will meet it.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::LicenceGate;
use crate::error::ApiError;

/// `requireContentFlaggingAvailable` (api4/content_flagging.go:38).
const CONTENT_FLAGGING_LICENSE_ERROR: &str = "api.data_spillage.error.license";

/// The second arm of `requireContentFlaggingEnabled`, unreachable from here — see the module
/// note. Named so the pair cannot be conflated by someone reading only one of them.
#[cfg(test)]
const CONTENT_FLAGGING_DISABLED_ERROR: &str = "api.data_spillage.error.disabled";

/// `createChannelBookmark` and its three siblings (api4/channel_bookmark.go:28, :122, :239, :343).
const CHANNEL_BOOKMARK_LICENSE_ERROR: &str = "api.channel.bookmark.channel_bookmark.license.error";

/// `acknowledgePost` (api4/post.go:1429) refuses with `model.NoTranslation` as its **id**, so the
/// literal `<untranslated>` lands in both `id` and `message`. Not a placeholder this port chose —
/// it is the id Go sends.
const ACKNOWLEDGE_POST_LICENSE_ERROR: &str = mm_model::utils::NO_TRANSLATION;

/// `unacknowledgePost` (api4/post.go:1468) — four lines below its twin and a different id. See
/// the module note; the pair is the reason these two are not one `refusal!` invocation.
const UNACKNOWLEDGE_POST_LICENSE_ERROR: &str = "license_error.feature_unavailable";

/// Refuse with `id`, or hand the request to Go if this installation has a licence.
///
/// The decision itself is [`crate::channels::licence_gate`], which three channel routes already
/// share — reused rather than re-derived, because "what counts as licensed" is one question with
/// one answer ([`mm_app::license::LicenseState`]) and a second copy of it is a second thing to
/// keep in step.
///
/// `where_` is the Go handler's own name. It is **not on the wire** — `AppError.Where` is not
/// serialised — so it exists for the trace and to keep each route's identity in the source.
async fn refuse_or_forward(
    state: AppState,
    where_: &'static str,
    id: &'static str,
    request: Request,
) -> Response {
    match crate::channels::licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => {
            ApiError::from(AppError::new(where_, id, None, String::new(), 501)).into_response()
        }
        LicenceGate::Failed(err) => err.into_response(),
    }
}

macro_rules! refusal {
    ($fn_name:ident, $go:literal, $id:ident) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is the licence test.")]
        #[tracing::instrument(skip_all, fields(licensed))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            refuse_or_forward(state, $go, $id, request).await
        }
    };
}

// --- content flagging: every route, including the two `/config` ones ---
refusal!(
    get_flagging_configuration,
    "getFlaggingConfiguration",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    get_content_flagging_fields,
    "getContentFlaggingFields",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    get_content_flagging_settings,
    "getContentFlaggingSettings",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    save_content_flagging_settings,
    "saveContentFlaggingSettings",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    get_team_post_flagging_feature_status,
    "getTeamPostFlaggingFeatureStatus",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    search_reviewers,
    "searchReviewers",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    get_flagged_post,
    "getFlaggedPost",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(flag_post, "flagPost", CONTENT_FLAGGING_LICENSE_ERROR);
refusal!(
    get_post_property_values,
    "getPostPropertyValues",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    remove_flagged_post,
    "removeFlaggedPost",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    keep_flagged_post,
    "keepFlaggedPost",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    generate_flagged_post_report,
    "generateFlaggedPostReport",
    CONTENT_FLAGGING_LICENSE_ERROR
);
refusal!(
    assign_flagged_post_reviewer,
    "assignFlaggedPostReviewer",
    CONTENT_FLAGGING_LICENSE_ERROR
);

// --- channel bookmarks: the four writes. The `GET` list is **not** here: it is not licence-gated
// and is already served from `channels.rs`.
refusal!(
    create_channel_bookmark,
    "createChannelBookmark",
    CHANNEL_BOOKMARK_LICENSE_ERROR
);
refusal!(
    update_channel_bookmark,
    "updateChannelBookmark",
    CHANNEL_BOOKMARK_LICENSE_ERROR
);
refusal!(
    update_channel_bookmark_sort_order,
    "updateChannelBookmarkSortOrder",
    CHANNEL_BOOKMARK_LICENSE_ERROR
);
refusal!(
    delete_channel_bookmark,
    "deleteChannelBookmark",
    CHANNEL_BOOKMARK_LICENSE_ERROR
);

// --- the post-acknowledgement pair. Same path, same gate, two different ids.
refusal!(
    acknowledge_post,
    "acknowledgePost",
    ACKNOWLEDGE_POST_LICENSE_ERROR
);
refusal!(
    unacknowledge_post,
    "unacknowledgePost",
    UNACKNOWLEDGE_POST_LICENSE_ERROR
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The three error ids this module can name, and that no two of them are the same string.
    ///
    /// Two of the three differ by one word — `.license` against `.disabled` — and only the first
    /// is reachable from an unlicensed server. A port that reached for the wrong one would answer
    /// the right status with the wrong id, which is exactly what a client branches on.
    #[test]
    fn the_three_error_ids_are_distinct_and_named() {
        assert_eq!(
            CONTENT_FLAGGING_LICENSE_ERROR,
            "api.data_spillage.error.license"
        );
        assert_eq!(
            CONTENT_FLAGGING_DISABLED_ERROR,
            "api.data_spillage.error.disabled"
        );
        assert_eq!(
            CHANNEL_BOOKMARK_LICENSE_ERROR,
            "api.channel.bookmark.channel_bookmark.license.error"
        );
        assert_ne!(
            CONTENT_FLAGGING_LICENSE_ERROR,
            CONTENT_FLAGGING_DISABLED_ERROR
        );
        assert_ne!(
            CONTENT_FLAGGING_LICENSE_ERROR,
            CHANNEL_BOOKMARK_LICENSE_ERROR
        );
    }

    /// The acknowledgement pair's two ids, and that they are **not** the same string.
    ///
    /// This is the whole parity risk of those two routes. They are four lines apart in
    /// `api4/post.go`, they share a gate, a status and a (wiped) detail, and a reader who copied
    /// one into the other would produce a server that is right on `POST` and wrong on `DELETE`
    /// with nothing else on the wire to show it.
    #[test]
    fn the_acknowledgement_pair_refuses_with_two_different_ids() {
        assert_eq!(ACKNOWLEDGE_POST_LICENSE_ERROR, "<untranslated>");
        assert_eq!(
            UNACKNOWLEDGE_POST_LICENSE_ERROR,
            "license_error.feature_unavailable"
        );
        assert_ne!(
            ACKNOWLEDGE_POST_LICENSE_ERROR, UNACKNOWLEDGE_POST_LICENSE_ERROR,
            "the POST and the DELETE do not share an id"
        );
        // The id is `model.NoTranslation` itself, not a string that merely looks like it — if the
        // model constant moved, this route's wire format moves with it.
        assert_eq!(
            ACKNOWLEDGE_POST_LICENSE_ERROR,
            mm_model::utils::NO_TRANSLATION
        );
    }

    /// `<untranslated>` survives to the wire **escaped**, because Go's `json.Marshal` escapes
    /// `<` and `>` and this project reproduces that. Asserted on the real response rather than on
    /// the constant, since the escaping happens in `into_wire` and not here.
    #[test]
    fn the_acknowledge_refusal_escapes_its_angle_brackets() {
        let err = ApiError::from(AppError::new(
            "acknowledgePost",
            ACKNOWLEDGE_POST_LICENSE_ERROR,
            None,
            "feature is not available for the current license".to_owned(),
            501,
        ));
        let (status, body) = err.into_wire();
        assert_eq!(status.as_u16(), 501);
        let body = String::from_utf8(body.expect("a body")).expect("utf8");
        assert!(
            body.contains(r"\u003cuntranslated\u003e"),
            "angle brackets must be escaped as Go escapes them: {body}"
        );
        assert!(
            !body.contains("<untranslated>"),
            "the raw form must not appear: {body}"
        );
        // `detailed_error` is wiped for every error this server sends, so the sentence Go writes
        // into it is not on the wire and cannot be used to tell the pair apart.
        assert!(
            !body.contains("feature is not available"),
            "the detail is wiped: {body}"
        );
    }

    /// The unreachable one is the **config** arm, and it is unreachable because it sits *after*
    /// the licence test. Stated as an assertion about the ordering rather than a comment, so that
    /// moving the two apart in a future Go release fails a test rather than passing silently.
    #[test]
    fn the_disabled_arm_is_the_one_behind_the_licence_test() {
        assert!(
            CONTENT_FLAGGING_DISABLED_ERROR.ends_with(".disabled"),
            "the config arm names the setting, not the licence"
        );
        assert!(CONTENT_FLAGGING_LICENSE_ERROR.ends_with(".license"));
    }
}
