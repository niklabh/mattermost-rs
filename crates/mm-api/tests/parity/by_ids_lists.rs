//! Cross-server parity for the three by-ids list routes that landed together:
//!
//! - `POST /api/v4/channels/{channel_id}/members/ids` — `getChannelMembersByIds`
//! - `POST /api/v4/teams/{team_id}/members/ids` — `getTeamMembersByIds`
//! - `POST /api/v4/teams/{team_id}/channels/ids` — `getPublicChannelsByIdsForTeam`
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity by_ids_lists
//! ```
//!
//! # Why one file
//!
//! They are one shape — `model.SortedArrayFromJSON` over the body, a gate, one store call — and
//! the assertions that matter are the places where that shape gives **different** answers. Split
//! across three files each difference would be a claim in a comment; here it is one test with
//! both halves in it. Three such pairs:
//!
//! | | channel members | team members | public channels |
//! |---|---|---|---|
//! | nothing matched | `[]`, 200 | `[]`, 200 | **404** |
//! | departed/deleted row | returned | **filtered** (`DeleteAt = 0`) | n/a |
//! | trailing newline | yes (`Encode`) | **no** (`Marshal` + `Write`) | yes |
//!
//! # The fixture is built by a founder, not by the shared admin
//!
//! Go joins a team's creator to it and to both default channels, so creating fixtures as the
//! shared user lengthens `/users/me/channels` under whichever suite is byte-comparing it. The
//! founder is a plain user; creating a team is a `system_user` permission.

use crate::common;

use common::{
    add_user_to_channel, assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_plain_user, create_team, go_minted_token, post_both_raw, purge_api_fixtures,
    remove_user_from_team, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A private channel holding `member_a` and `member_b` — private because a public channel
    /// cannot test a refusal (`read_public_channel` on the team admits any team member).
    channel_id: String,
    member_a_id: String,
    member_a_token: String,
    member_b_id: String,
    /// In the team, in no channel of it but the defaults, and never in `channel_id`.
    outsider_id: String,
    outsider_token: String,
    /// Removed from the team after the fixture was built: a `TeamMembers` row with a non-zero
    /// `DeleteAt`, which is the only thing the team route's extra predicate can be tested with.
    departed_id: String,
    /// A user in a **different** team, so it is refused `view_team` on `team_id` — and, asked
    /// for by id, is what the team route's `TeamId` predicate has to exclude.
    stranger_id: String,
    stranger_token: String,
    /// Two living public channels of `team_id`, display-name ordered `aaa` before `zzz`.
    public_aaa_id: String,
    public_zzz_id: String,
    /// A private channel of `team_id`: it has no `PublicChannels` row, so the channels route
    /// never returns it.
    private_id: String,
    /// A living public channel of the **other** team, asked for alongside `team_id`'s own so
    /// the team predicate has something to exclude.
    other_public_id: String,
    /// A public channel of `team_id` that was archived after creation: the `PublicChannels` row
    /// survives with a non-zero `DeleteAt`, which is the only thing that predicate can be
    /// tested against.
    archived_public_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (shared_team_id, _) =
                common::a_team_and_channel_the_user_is_in(client, token).await;
            let founder = create_plain_user(client, token, &shared_team_id, "byidsfnd").await;

            let team_id = create_team(client, &founder.token, "byids").await;
            let other_team_id = create_team(client, &founder.token, "byidsother").await;

            let member_a = create_plain_user(client, token, &team_id, "byidsa").await;
            let member_b = create_plain_user(client, token, &team_id, "byidsb").await;
            let outsider = create_plain_user(client, token, &team_id, "byidsout").await;
            let departed = create_plain_user(client, token, &team_id, "byidsdep").await;
            let stranger = create_plain_user(client, token, &other_team_id, "byidsstr").await;

            let channel_id =
                create_channel_typed(client, &founder.token, &team_id, "byidschan", "P").await;
            add_user_to_channel(client, token, &channel_id, &member_a.id).await;
            add_user_to_channel(client, token, &channel_id, &member_b.id).await;

            let public_aaa_id =
                create_channel_typed(client, &founder.token, &team_id, "byidsaaa", "O").await;
            let public_zzz_id =
                create_channel_typed(client, &founder.token, &team_id, "byidszzz", "O").await;
            let private_id =
                create_channel_typed(client, &founder.token, &team_id, "byidspriv", "P").await;
            let other_public_id =
                create_channel_typed(client, &founder.token, &other_team_id, "byidsoth", "O").await;
            let archived_public_id =
                create_channel_typed(client, &founder.token, &team_id, "byidsarch", "O").await;
            common::delete_channel(client, token, &archived_public_id).await;

            // Last, so every membership assertion above was made against a whole team.
            remove_user_from_team(client, token, &team_id, &departed.id).await;

            Fixture {
                team_id,
                channel_id,
                member_a_id: member_a.id,
                member_a_token: member_a.token,
                member_b_id: member_b.id,
                outsider_id: outsider.id,
                outsider_token: outsider.token,
                departed_id: departed.id,
                stranger_id: stranger.id,
                stranger_token: stranger.token,
                public_aaa_id,
                public_zzz_id,
                private_id,
                other_public_id,
                archived_public_id,
            }
        })
        .await
}

fn body_of(ids: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&ids).expect("a JSON array")
}

/// The `id` field of the two servers' error bodies, with everything else compared as usual.
fn shared_error_id(go: &[u8], rs: &[u8], context: &str) -> String {
    let go = assert_error_bodies_match_except_known_gaps(go, rs, context);
    go["id"].as_str().expect("an id").to_owned()
}

fn ids_in(body: &[u8], key: &str) -> Vec<String> {
    let rows: serde_json::Value = serde_json::from_slice(body).expect("an array");
    let mut ids: Vec<String> = rows
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row[key].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------------------------
// POST /channels/{channel_id}/members/ids
// ---------------------------------------------------------------------------------------------

/// The happy path, byte-for-byte, plus the three things about the list a set comparison would
/// hide: the trailing newline, that an unknown id is silently absent rather than an error, and
/// that a **repeated** id is one row — `RemoveDuplicateStrings` de-dups before the query runs.
#[tokio::test]
async fn channel_members_by_ids_matches_go_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/channels/{}/members/ids", f.channel_id);

    let both = body_of(&[&f.member_a_id, &f.member_b_id]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &both).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs, "the whole body, key order and newline included");
    assert!(go.ends_with(b"\n"), "json.NewEncoder adds a newline");
    assert_eq!(ids_in(&go, "user_id").len(), 2);

    // A 26-character id that names no user: absent, not a 404 and not a null row.
    let missing = "abcdefghijklmnopqrstuvwxyz";
    let with_missing = body_of(&[&f.member_a_id, missing]);
    let ((_, go), (_, rs)) = post_both_raw(&client, &token, &path, &with_missing).await;
    assert_eq!(go, rs);
    assert_eq!(ids_in(&go, "user_id"), vec![f.member_a_id.clone()]);

    // The same id twice is one row on both servers — the de-duplication in
    // `SortedArrayFromJSON`, which is the only reason a client cannot inflate the answer.
    let doubled = body_of(&[&f.member_a_id, &f.member_a_id]);
    let ((_, go), (_, rs)) = post_both_raw(&client, &token, &path, &doubled).await;
    assert_eq!(go, rs);
    assert_eq!(ids_in(&go, "user_id"), vec![f.member_a_id.clone()]);

    // Nothing matched: `[]` with a 200. Its sibling `POST /teams/{id}/channels/ids` answers 404
    // for exactly this shape — asserted below, in one place, so the pair is visible.
    let none = body_of(&[missing]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &none).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs);
    assert_eq!(go, b"[]\n", "an empty list, not null");
}

/// `SanitizeForCurrentUser` blanks every row but the caller's own, **mid-list** — so the same
/// request answers differently depending on who asks. Both directions, both servers.
///
/// What it blanks is narrower than the name suggests: `LastViewedAt` and `LastUpdateAt`, and
/// nothing else. Another member's `msg_count` and `mention_count` are on the wire.
#[tokio::test]
async fn channel_members_by_ids_sanitises_every_row_but_the_callers_own() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/channels/{}/members/ids", f.channel_id);
    let both = body_of(&[&f.member_a_id, &f.member_b_id]);

    let ((_, go), (_, rs)) = post_both_raw(&client, &f.member_a_token, &path, &both).await;
    assert_eq!(go, rs, "as member_a");

    let rows: serde_json::Value = serde_json::from_slice(&go).expect("an array");
    let rows = rows.as_array().expect("an array");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let is_self = row["user_id"].as_str() == Some(f.member_a_id.as_str());
        let blanked = row["last_viewed_at"] == serde_json::json!(-1)
            && row["last_update_at"] == serde_json::json!(-1);
        assert_eq!(
            blanked, !is_self,
            "every row but the caller's own has its two timestamps blanked to -1: {row}"
        );
        // **Only the timestamps.** `SanitizeForCurrentUser` (channel_member.go:95) sets
        // `LastViewedAt` and `LastUpdateAt` and nothing else, so another member's unread
        // counters come back intact — the name reads as if it blanks everything sensitive and
        // it blanks exactly two fields. A port that also cleared the counters would look more
        // careful and would be wrong.
        assert_ne!(
            row["msg_count"],
            serde_json::json!(-1),
            "the counters survive the sanitiser: {row}"
        );
        assert_ne!(
            row["mention_count"],
            serde_json::json!(-1),
            "the counters survive the sanitiser: {row}"
        );
    }
}

/// The gate is `read_channel` and it runs **after** both body 400s — so a caller with no rights
/// to the channel still gets the 400 for a bad body. That ordering is the whole test: it is
/// invisible in any single response and only shows up when the two are compared.
#[tokio::test]
async fn channel_members_by_ids_refuses_a_non_member_but_only_after_the_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/channels/{}/members/ids", f.channel_id);

    // A well-formed body from someone with no read on the private channel: 403 `read_channel`.
    let ok_body = body_of(&[&f.member_a_id]);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.outsider_token, &path, &ok_body).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let id = shared_error_id(&go, &rs, "outsider, good body");
    assert_eq!(id, "api.context.permissions.app_error");

    // The same caller with an empty array: the **400** wins, because the gate is last.
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.outsider_token, &path, b"[]").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let id = shared_error_id(&go, &rs, "outsider, empty body");
    assert_eq!(id, "api.context.invalid_body_param.app_error");
}

/// The four body shapes, and which of the two 400s each one earns. `null` is the one a reader
/// gets wrong: it decodes **successfully** into a nil slice, so Go returns no error and it lands
/// on `invalid_body_param` beside `[]`, not on the parse branch beside `{}`.
#[tokio::test]
async fn channel_members_by_ids_splits_the_two_body_400s_where_go_does() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/channels/{}/members/ids", f.channel_id);

    for (body, expected) in [
        (&b"[]"[..], "api.context.invalid_body_param.app_error"),
        (&b"null"[..], "api.context.invalid_body_param.app_error"),
        (&b"{}"[..], "api.payload.parse.error"),
        (&b"[1]"[..], "api.payload.parse.error"),
        (&b"\"x\""[..], "api.payload.parse.error"),
        (&b""[..], "api.payload.parse.error"),
    ] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, body).await;
        assert_eq!(
            (go_status, rs_status),
            (400, 400),
            "body {:?}",
            String::from_utf8_lossy(body)
        );
        let id = shared_error_id(
            &go,
            &rs,
            &format!("body {:?}", String::from_utf8_lossy(body)),
        );
        assert_eq!(id, expected, "body {:?}", String::from_utf8_lossy(body));
    }
}

/// `RequireChannelId` runs before the body is read, so a malformed path beats a malformed body.
/// The reverse order would be invisible for a good body and wrong for this one.
#[tokio::test]
async fn channel_members_by_ids_validates_the_path_before_the_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, "/api/v4/channels/short/members/ids", b"{}").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let id = shared_error_id(&go, &rs, "short channel id with a broken body");
    assert_eq!(
        id, "api.context.invalid_url_param.app_error",
        "the *url* param error, not the body one"
    );
}

// ---------------------------------------------------------------------------------------------
// POST /teams/{team_id}/members/ids
// ---------------------------------------------------------------------------------------------

/// The happy path byte-for-byte, and the two ways this route differs from its channel twin:
/// **no trailing newline** (`json.Marshal` + `w.Write`, not `Encode`), and a departed member is
/// **absent** where the channel route would have returned the row.
#[tokio::test]
async fn team_members_by_ids_matches_go_and_filters_the_departed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/members/ids", f.team_id);

    // `stranger` is a member of the *other* team, so it exercises the `TeamId` predicate: a
    // query that dropped it would return the stranger's other-team row for the same user id.
    let body = body_of(&[
        &f.member_a_id,
        &f.member_b_id,
        &f.departed_id,
        &f.stranger_id,
    ]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &body).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs, "the whole body, key order included");
    assert!(
        !go.ends_with(b"\n"),
        "team.go:966 writes the marshalled bytes; there is no encoder and no newline"
    );

    let returned = ids_in(&go, "user_id");
    assert!(returned.contains(&f.member_a_id), "{returned:?}");
    assert!(returned.contains(&f.member_b_id), "{returned:?}");
    assert!(
        !returned.contains(&f.departed_id),
        "a non-zero TeamMembers.DeleteAt is filtered here, unlike ChannelMembers: {returned:?}"
    );
    assert!(
        !returned.contains(&f.stranger_id),
        "the TeamId predicate keeps another team's membership out: {returned:?}"
    );

    // Nothing matched: `[]` with a 200, the channel route's answer and not the channels one's.
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, &path, &body_of(&[&f.departed_id])).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs);
    assert_eq!(go, b"[]");
}

/// `SanitizeRoleData` runs for a caller without `manage_team_roles` and blanks every row but its
/// own — `delete_at: -1`, mid-list, the same shape as the channel route's sanitiser. The admin
/// holds the permission and sees the rows whole, which is what makes the plain user's answer a
/// difference rather than a constant.
#[tokio::test]
async fn team_members_by_ids_sanitises_role_data_for_a_plain_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/members/ids", f.team_id);
    let body = body_of(&[&f.member_a_id, &f.member_b_id]);

    let ((_, go), (_, rs)) = post_both_raw(&client, &f.member_a_token, &path, &body).await;
    assert_eq!(go, rs, "as member_a");

    let rows: serde_json::Value = serde_json::from_slice(&go).expect("an array");
    let rows = rows.as_array().expect("an array");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let is_self = row["user_id"].as_str() == Some(f.member_a_id.as_str());
        let blanked = row["delete_at"] == serde_json::json!(-1);
        assert_eq!(
            blanked, !is_self,
            "sanitised unless it is the caller: {row}"
        );
    }

    // The admin holds `manage_team_roles`, so nothing is blanked for it.
    let ((_, go_admin), (_, rs_admin)) = post_both_raw(&client, &token, &path, &body).await;
    assert_eq!(go_admin, rs_admin, "as the admin");
    let rows: serde_json::Value = serde_json::from_slice(&go_admin).expect("an array");
    for row in rows.as_array().expect("an array") {
        assert_ne!(
            row["delete_at"],
            serde_json::json!(-1),
            "a caller with manage_team_roles sees the row whole: {row}"
        );
    }
}

/// `view_team` gates the route, and it too runs after both body 400s.
#[tokio::test]
async fn team_members_by_ids_refuses_a_stranger_after_the_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/members/ids", f.team_id);

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &f.stranger_token,
        &path,
        &body_of(&[&f.member_a_id]),
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_eq!(
        shared_error_id(&go, &rs, "stranger, good body"),
        "api.context.permissions.app_error"
    );

    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.stranger_token, &path, b"null").await;
    assert_eq!((go_status, rs_status), (400, 400));
    assert_eq!(
        shared_error_id(&go, &rs, "stranger, null body"),
        "api.context.invalid_body_param.app_error"
    );
}

// ---------------------------------------------------------------------------------------------
// POST /teams/{team_id}/channels/ids
// ---------------------------------------------------------------------------------------------

/// The happy path byte-for-byte, in `PublicChannels.DisplayName` order, and the two exclusions
/// that come from the query reading the shadow table rather than `Channels`: a **private**
/// channel of the same team is not there, and neither is a public channel of another team.
#[tokio::test]
async fn public_channels_by_ids_matches_go_in_display_name_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/channels/ids", f.team_id);

    // Requested zzz-first; the answer must come back aaa-first, so the ordering is the query's
    // and not the request's. `SortedArrayFromJSON` sorts by **id**, which is unrelated to
    // display name, so this cannot pass by accident.
    let body = body_of(&[
        &f.public_zzz_id,
        &f.public_aaa_id,
        &f.private_id,
        &f.other_public_id,
        &f.archived_public_id,
    ]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &body).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs, "the whole body, order and newline included");
    assert!(go.ends_with(b"\n"), "json.NewEncoder adds a newline");

    let rows: serde_json::Value = serde_json::from_slice(&go).expect("an array");
    let ordered: Vec<&str> = rows
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(
        ordered,
        vec![f.public_aaa_id.as_str(), f.public_zzz_id.as_str()],
        "display-name order, with three exclusions in one answer: the private channel (no          PublicChannels row), the other team's public channel (the TeamId predicate) and the          archived one (the shadow row's DeleteAt)"
    );
}

/// **The divergence that makes this family worth one file.** Zero matches is a 404 here, where
/// both member routes answer `[]` with a 200 — and the 404 has nothing to do with the team
/// existing: it comes from the store's `len(data) == 0`. Three ways to produce it, all 404.
#[tokio::test]
async fn public_channels_by_ids_is_a_404_where_its_siblings_are_an_empty_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/channels/ids", f.team_id);

    for (label, body) in [
        ("only a private channel", body_of(&[&f.private_id])),
        (
            "only an archived public channel",
            body_of(&[&f.archived_public_id]),
        ),
        (
            "only another team's public channel",
            body_of(&[&f.other_public_id]),
        ),
        (
            "an id that names nothing",
            body_of(&["abcdefghijklmnopqrstuvwxyz"]),
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &body).await;
        assert_eq!((go_status, rs_status), (404, 404), "{label}");
        assert_eq!(
            shared_error_id(&go, &rs, label),
            "app.channel.get_channels_by_ids.not_found.app_error",
            "{label}"
        );
    }

    // A well-formed team id that names no team: also a 404, and also from the empty result —
    // the handler never fetches the team. The admin passes the gate on a non-existent team
    // because `manage_system` grants outright.
    let nowhere = "/api/v4/teams/abcdefghijklmnopqrstuvwxyz/channels/ids";
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, nowhere, &body_of(&[&f.public_aaa_id])).await;
    assert_eq!((go_status, rs_status), (404, 404));
    assert_eq!(
        shared_error_id(&go, &rs, "a team that does not exist"),
        "app.channel.get_channels_by_ids.not_found.app_error"
    );
}

/// The third 400 nothing else in the family has: every id is run through `IsValidId` **before**
/// the gate, and the failure names `channel_id` — *singular*, a different parameter from the
/// `channel_ids` an empty array earns two lines earlier in the same handler.
#[tokio::test]
async fn public_channels_by_ids_names_two_different_parameters_in_its_two_400s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/channels/ids", f.team_id);

    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, b"[]").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let empty = assert_error_bodies_match_except_known_gaps(&go, &rs, "empty array");
    assert_eq!(empty["id"], "api.context.invalid_body_param.app_error");
    assert!(
        empty["message"]
            .as_str()
            .unwrap_or_default()
            .contains("channel_ids"),
        "the plural parameter: {}",
        empty["message"]
    );

    // One good id and one that is not id-shaped: still a 400, and Go's untranslated message
    // carries the parameter name, which is how the two branches stay distinguishable ([D-092]).
    let mixed = body_of(&[&f.public_aaa_id, "not-an-id"]);
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, &mixed).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go_value = assert_error_bodies_match_except_known_gaps(&go, &rs, "a malformed id");
    assert_eq!(go_value["id"], "api.context.invalid_body_param.app_error");
    assert!(
        go_value["message"]
            .as_str()
            .unwrap_or_default()
            .contains("channel_id"),
        "Go names the singular parameter: {}",
        go_value["message"]
    );
    assert!(
        !go_value["message"]
            .as_str()
            .unwrap_or_default()
            .contains("channel_ids"),
        "…and it is not the plural one the empty array earns: {}",
        go_value["message"]
    );
}

/// `view_team`, after all three 400s. The outsider *is* in the team, so it is served; the
/// stranger is not, so it is refused — which is what proves the gate is the team's and not the
/// channels'.
#[tokio::test]
async fn public_channels_by_ids_gates_on_view_team_not_on_the_channels() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = format!("/api/v4/teams/{}/channels/ids", f.team_id);
    let body = body_of(&[&f.public_aaa_id]);

    // In the team, in none of the channels asked for: served anyway.
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.outsider_token, &path, &body).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go, rs);
    let _ = &f.outsider_id;

    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.stranger_token, &path, &body).await;
    assert_eq!((go_status, rs_status), (403, 403));
    assert_eq!(
        shared_error_id(&go, &rs, "a stranger to the team"),
        "api.context.permissions.app_error"
    );

    // **The same stranger with a malformed id gets the 400, not the 403.** The per-id
    // `IsValidId` loop runs *before* the gate (api4/channel.go:1348 against :1353), and that
    // ordering is invisible to any caller the gate admits — the admin sees the 400 either way.
    // Only a refused caller can tell the two orders apart, which is why this assertion lives
    // here rather than beside the other two 400s. A mutation moving the loop past the gate
    // survived the whole suite until this existed.
    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &f.stranger_token,
        &path,
        &body_of(&[&f.public_aaa_id, "not-an-id"]),
    )
    .await;
    assert_eq!(
        (go_status, rs_status),
        (400, 400),
        "the id loop precedes the gate"
    );
    assert_eq!(
        shared_error_id(&go, &rs, "a stranger with a malformed id"),
        "api.context.invalid_body_param.app_error"
    );
}
