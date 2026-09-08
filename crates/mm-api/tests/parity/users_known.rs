//! Cross-server parity for `GET /api/v4/users/known`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity users_known
//! ```
//!
//! # Compared as a set
//!
//! The query is a `DISTINCT` self-join with **no `ORDER BY`**, so two servers reading the same
//! table may legitimately return the ids in different orders. A byte comparison here would be
//! asserting something neither server promises — the mistake `teams_for_user` made and had to
//! have repaired.
//!
//! # The route has no permission check, and needs none
//!
//! Neither the handler nor the app layer asks anything: the answer is derived from the caller's
//! own memberships, so it can only name people the caller already shares a channel with. That
//! makes the interesting assertions negative ones — who is *not* in the list.

use crate::common;

use common::{
    add_user_to_channel, client, create_channel_typed, create_plain_user, fetch_both,
    fetch_both_stable, go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// Two members of a private channel with `known_a`, and one who is in no channel with it.
    known_a_id: String,
    known_a_token: String,
    known_b_id: String,
    stranger_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            // **Two teams, because "shares no channel" cannot be arranged inside one.** Joining
            // a team auto-joins its `town-square`, and Go refuses to remove anyone from a
            // default channel (`api.channel.remove.default.app_error`) — so any two members of a
            // team already know each other and no amount of leaving undoes it. The stranger goes
            // in a team of its own.
            //
            // **A founder creates them, not the shared fixture user.** Go joins a team's creator
            // to it and to both of its default channels, so building this fixture as the admin
            // would put four more channels into `/users/me/channels` while
            // `channels_for_user` is byte-comparing that list — which it did, once. Creating a
            // team is a `system_user` permission, so a plain user can do it; the admin still
            // adds the members, which does not make the adder one.
            let (shared_team_id, _) =
                common::a_team_and_channel_the_user_is_in(client, token).await;
            let founder = create_plain_user(client, token, &shared_team_id, "knownfounder").await;

            let team_id = common::create_team(client, &founder.token, "knownmain").await;
            let other_team_id = common::create_team(client, &founder.token, "knownother").await;

            let known_a = create_plain_user(client, token, &team_id, "knowna").await;
            let known_b = create_plain_user(client, token, &team_id, "knownb").await;
            let stranger = create_plain_user(client, token, &other_team_id, "knownc").await;

            // Private, so nobody is added to it by accident. `known_a` and `known_b` also share
            // the team's `town-square`; this channel is here so the positive case does not rest
            // on a channel the fixture did not create.
            let channel_id =
                create_channel_typed(client, &founder.token, &team_id, "known", "P").await;
            add_user_to_channel(client, token, &channel_id, &known_a.id).await;
            add_user_to_channel(client, token, &channel_id, &known_b.id).await;
            // Bound only to make the channel; nothing reads the id afterwards, because the
            // assertions are about *who* is known and not about which channel said so.
            let _ = &channel_id;

            Fixture {
                known_a_id: known_a.id,
                known_a_token: known_a.token,
                known_b_id: known_b.id,
                stranger_id: stranger.id,
            }
        })
        .await
}

fn ids_of(body: &[u8]) -> Vec<String> {
    let mut ids: Vec<String> = serde_json::from_slice(body).expect("an array of ids");
    ids.sort();
    ids
}

/// The whole route in one test, and it has to be one test: the second half **adds the stranger
/// to the channel**, which is precisely the state the first half asserts is absent. Split across
/// two `#[tokio::test]`s they race, because the harness runs them concurrently against one
/// database.
#[tokio::test]
async fn the_known_user_ids_follow_shared_channel_membership() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rs) = fetch_both(&client, &f.known_a_token, "/api/v4/users/known").await;
    assert_eq!(
        ids_of(&go),
        ids_of(&rs),
        "the same set, whatever the row order"
    );
    // **Both** sides. This route is compared as a set, so the bytes are never equated — and an
    // assertion on Go's body alone says nothing about ours. A mutation removing our newline
    // survived until this named `rs`.
    assert!(
        go.ends_with(b"\n"),
        "json.NewEncoder().Encode adds a trailing newline"
    );
    assert!(
        rs.ends_with(b"\n"),
        "…and so must ours, which the set comparison above cannot see"
    );

    let ids = ids_of(&go);
    assert!(
        ids.contains(&f.known_b_id),
        "a fellow member of the private channel is known: {ids:?}"
    );
    assert!(
        !ids.contains(&f.known_a_id),
        "the caller is excluded — `ocm.UserId <> cm.UserId`"
    );
    assert!(
        !ids.contains(&f.stranger_id),
        "a team-mate sharing no channel is not known: {ids:?}"
    );

    // The join really is on channel membership. A **direct message** is the instrument: it makes
    // one shared channel out of two users in different teams, without the team membership that
    // would have brought a `town-square` along with it. Without this half the negative assertion
    // above would hold for a port that returned an empty list.
    common::create_direct_channel(&client, &f.known_a_token, &f.known_a_id, &f.stranger_id).await;

    let (go, rs) = fetch_both(&client, &f.known_a_token, "/api/v4/users/known").await;
    assert_eq!(ids_of(&go), ids_of(&rs));
    assert!(
        ids_of(&go).contains(&f.stranger_id),
        "one shared channel is enough"
    );
}

/// `DISTINCT` earns its place on a caller that shares *several* channels with the same people —
/// which the shared fixture user does, many times over.
#[tokio::test]
async fn no_id_appears_twice_however_many_channels_are_shared() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    // **The shared admin's known set is the one thing on this route that other suites move.**
    // Every `create_plain_user` puts someone new in a channel this caller is in, so a plain
    // `fetch_both` compares a Go read taken before that write with a Rust read taken after it.
    // `fetch_both_stable` re-reads Go on the far side and accepts our answer if it matches either,
    // which is the same Go–Rust–Go window the churning list routes already use.
    let (go, rs) = fetch_both_stable(&client, &token, "/api/v4/users/known").await;
    let ids = ids_of(&go);
    assert_eq!(ids, ids_of(&rs), "the same set on both servers");

    let mut deduped = ids.clone();
    deduped.dedup();
    assert_eq!(ids, deduped, "no id appears twice");
    assert!(
        !ids.contains(&logged_in_user_id().to_owned()),
        "and never the caller itself"
    );
    assert!(
        ids.len() > 1,
        "the fixture user shares channels with several people, so this is not vacuous: {ids:?}"
    );
}
