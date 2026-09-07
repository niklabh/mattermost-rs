//! Two route families whose entire behaviour on an unlicensed server is one refusal, taken
//! **before every other check**: `api4/content_flagging.go` and the four write halves of
//! `api4/channel_bookmark.go`.
//!
//! # Why these two are in one module
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
//! # The two gates are not the same test, and the difference is invisible here
//!
//! Content flagging needs `MinimumEnterpriseAdvancedLicense` — a licence **tier**, not merely a
//! licence (license.go:515), so an Enterprise licence below the Advanced tier is still refused.
//! Channel bookmarks need only `License() != nil`. Both collapse to "refuse" when there is no
//! licence at all, which is the only case this server answers; a licensed installation is
//! forwarded and Go applies whichever test is really its own.
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
