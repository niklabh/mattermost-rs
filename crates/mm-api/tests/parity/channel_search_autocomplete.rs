//! Cross-server parity for `GET /api/v4/teams/{team_id}/channels/search_autocomplete`
//! (`autocompleteChannelsForTeamForSearch`) — the search box's channel suggestions.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_search_autocomplete
//! ```
//!
//! # The sibling it is not
//!
//! `/autocomplete` (one literal over, `parity/channel_autocomplete.rs`) lists every public
//! channel in the team and refuses a non-member with 403. This one lists **only what the caller
//! has joined**, has **no permission gate at all**, pulls in group messages from outside the
//! team, and appends direct messages under the *other user's username*. Four routes' worth of
//! difference behind two adjacent path segments.
//!
//! # Display names must not tie
//!
//! Go merges the two result sets and sorts them with `sort.Slice`, which is **unstable** — so
//! two channels whose lower-cased display names are equal come back in an order Go itself does
//! not repeat, and no port can match it. Every fixture below has a distinct display name.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, delete_channel, fetch_both_raw,
    fetch_both_stable, go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A second team the plain user is also in, holding a channel they have joined.
    other_team_id: String,
    /// A third team the plain user is **not** in.
    foreign_team_id: String,
    plain_id: String,
    plain_token: String,
    /// The admin's username, which is what a direct message is listed under.
    admin_username: String,
    /// The group message the plain user is in. It belongs to no team.
    gm_display_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// Channels created in the main team. `joined` is in the plain user's sidebar; `unjoined` is a
/// public channel they never joined, which this route must **not** list.
const JOINED: &str = "mmrs-parity-sajoined";
const UNJOINED: &str = "mmrs-parity-saunjoined";
const PRIVATE_JOINED: &str = "mmrs-parity-sapriv";
const ARCHIVED_JOINED: &str = "mmrs-parity-sagone";
const OTHER_TEAM: &str = "mmrs-parity-saother";
/// A joined channel whose **display name shares nothing with its name**. Every other fixture is
/// named `mmrs-parity-<tag>` with the display name `mmrs parity <tag>`, so a term matches both
/// columns at once and the `LIKE` list could be narrowed to `Name` alone without any test
/// noticing — it was, and the mutation survived. This one separates them.
const DISPLAY_ONLY: &str = "mmrs-parity-sadisp";
/// Two words, so a term can span them through `to_tsquery` and through nothing else; and long
/// enough to have a mid-word fragment that only `LIKE` can find.
const DISPLAY_ONLY_NAME: &str = "Zephyrine Quokka";

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let admin_id = logged_in_user_id();
            let team_id = create_team(client, token, "sateam").await;
            let other_team_id = create_team(client, token, "saother").await;
            let foreign_team_id = create_team(client, token, "saforeign").await;

            let plain = create_plain_user(client, token, &team_id, "search").await;
            add_user_to_team(client, token, &other_team_id, &plain.id).await;

            for (tag, kind) in [("sajoined", "O"), ("sapriv", "P"), ("sagone", "O")] {
                let id = create_channel_typed(client, token, &team_id, tag, kind).await;
                add_user_to_channel(client, token, &id, &plain.id).await;
                if tag == "sagone" {
                    // Archived *after* the join, so the membership row survives it.
                    delete_channel(client, token, &id).await;
                }
            }
            // Public, in the same team, never joined — the switcher lists it and this must not.
            create_channel_typed(client, token, &team_id, "saunjoined", "O").await;

            let disp =
                create_channel_named(client, token, &team_id, "sadisp", DISPLAY_ONLY_NAME).await;
            add_user_to_channel(client, token, &disp, &plain.id).await;

            // A channel in the *other* team, joined. Proves the team predicate is real.
            let other = create_channel_typed(client, token, &other_team_id, "saother", "O").await;
            add_user_to_channel(client, token, &other, &plain.id).await;

            // A direct message with the admin, and a group message with the admin and one more
            // user. The GM belongs to no team and must answer under every team's path.
            open_direct_channel(client, &plain.token, &plain.id, admin_id).await;
            let third = create_plain_user(client, token, &team_id, "searchthird").await;
            let gm_display_name =
                open_group_channel(client, &plain.token, &[&plain.id, admin_id, &third.id]).await;

            let admin_username = username_of(client, token, admin_id).await;

            Fixture {
                team_id,
                other_team_id,
                foreign_team_id,
                plain_id: plain.id,
                plain_token: plain.token,
                admin_username,
                gm_display_name,
            }
        })
        .await
}

/// `common::create_channel_typed` derives the display name from the tag, which makes the two
/// columns say the same words. This one sets it explicitly.
async fn create_channel_named(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
    display_name: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": display_name,
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the display-name fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn add_user_to_team(client: &reqwest::Client, token: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "adding {user_id} to {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn open_direct_channel(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Returns the group channel's **stored** display name, which — unlike a DM's — is non-empty and
/// is what this route lists it under.
async fn open_group_channel(client: &reqwest::Client, token: &str, ids: &[&str]) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!(ids))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the GM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["display_name"]
        .as_str()
        .expect("a display name")
        .to_owned()
}

async fn username_of(client: &reqwest::Client, token: &str, user_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let user: serde_json::Value = response.json().await.expect("the user decodes");
    user["username"].as_str().expect("a username").to_owned()
}

fn path(team_id: &str, name: &str) -> String {
    format!("/api/v4/teams/{team_id}/channels/search_autocomplete?name={name}")
}

fn rows(body: &[u8]) -> Vec<(String, String, String)> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("decodes")
        .as_array()
        .expect("an array")
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap_or_default().to_owned(),
                c["type"].as_str().unwrap_or_default().to_owned(),
                c["display_name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn names(body: &[u8]) -> Vec<String> {
    rows(body).into_iter().map(|(n, _, _)| n).collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion, from the caller whose memberships it is about.
#[tokio::test]
async fn the_search_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "mmrs-parity-sa");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let listed = names(&go);
    assert!(
        listed.contains(&JOINED.to_owned()) && listed.contains(&PRIVATE_JOINED.to_owned()),
        "joined channels of both kinds are listed: {listed:?}"
    );
    assert!(
        listed.contains(&ARCHIVED_JOINED.to_owned()),
        "includeDeleted is hardcoded true, so an archived channel is still listed: {listed:?}"
    );
}

/// The difference from `/autocomplete`, stated as a pair. Every channel here needs a
/// `ChannelMembers` row; the switcher next door needs none for a public channel.
#[tokio::test]
async fn a_public_channel_the_caller_never_joined_is_absent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "mmrs-parity-sa");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert!(
        !names(&go).contains(&UNJOINED.to_owned()),
        "{UNJOINED} is public and unjoined, so this route must not list it"
    );

    // And the switcher, on the same term and the same caller, *does* list it — so the assertion
    // above is about this route rather than about a channel that does not exist.
    let switcher = format!(
        "/api/v4/teams/{}/channels/autocomplete?name=mmrs-parity-sa",
        f.team_id
    );
    let (go_switch, _rs) = fetch_both_stable(&client, &f.plain_token, &switcher).await;
    assert!(
        names(&go_switch).contains(&UNJOINED.to_owned()),
        "the sibling route lists it, which is the whole point of the difference"
    );
}

/// A direct message is listed under the **other user's username**, not under its own display
/// name — which is empty — and not under its channel name, which is a pair of ids.
#[tokio::test]
async fn a_direct_message_is_listed_under_the_other_users_username() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, &f.admin_username);
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let dm: Vec<_> = rows(&go).into_iter().filter(|(_, t, _)| t == "D").collect();
    assert_eq!(dm.len(), 1, "exactly the one DM matches: {:?}", rows(&go));
    assert_eq!(
        dm[0].2, f.admin_username,
        "the display name is the other user's username"
    );
    assert!(
        dm[0].0.contains("__"),
        "and its own name is the id pair, which the term never matched: {}",
        dm[0].0
    );

    // The stored display name of a DM is empty, so a port that used it would list a blank.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let stored: (String,) = sqlx::query_as("SELECT displayname FROM channels WHERE name = $1")
            .bind(&dm[0].0)
            .fetch_one(&pool)
            .await
            .expect("the row is there");
        assert_eq!(
            stored.0, "",
            "the substitution is real: the stored display name is empty"
        );
    }
}

/// A group message belongs to no team, and the team predicate lets it through under any team.
#[tokio::test]
async fn a_group_message_answers_under_every_team() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Search for a fragment of the GM's display name, which Go builds from the usernames.
    let term = &f.gm_display_name[..6.min(f.gm_display_name.len())];
    for team in [&f.team_id, &f.other_team_id] {
        let p = path(team, term);
        let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
        assert_eq!(go, rs, "{p} must be byte-identical");
        assert!(
            rows(&go).iter().any(|(_, t, _)| t == "G"),
            "the group message is listed under team {team}: {:?}",
            rows(&go)
        );
    }
}

/// The team predicate, exercised by a channel the caller **has** joined in another team. A group
/// message crosses that line and an ordinary channel does not — the same `WHERE` clause decides
/// both, which is why they belong in one test.
#[tokio::test]
async fn a_joined_channel_in_another_team_is_not_listed_here() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let here = path(&f.team_id, "mmrs-parity-sa");
    let (go_here, rs_here) = fetch_both_stable(&client, &f.plain_token, &here).await;
    assert_eq!(go_here, rs_here, "{here} must be byte-identical");
    assert!(
        !names(&go_here).contains(&OTHER_TEAM.to_owned()),
        "{OTHER_TEAM} is joined, but in a different team: {:?}",
        names(&go_here)
    );

    // Under its own team it is listed, so the exclusion above is the team predicate and not a
    // missing membership row.
    let there = path(&f.other_team_id, "mmrs-parity-sa");
    let (go_there, rs_there) = fetch_both_stable(&client, &f.plain_token, &there).await;
    assert_eq!(go_there, rs_there, "{there} must be byte-identical");
    assert!(
        names(&go_there).contains(&OTHER_TEAM.to_owned()),
        "and under its own team it is: {:?}",
        names(&go_there)
    );
}

/// There is no permission gate on this route at all — a team the caller has nothing to do with
/// answers **200 with an empty list**, where the sibling switcher answers 403.
#[tokio::test]
async fn a_team_the_caller_is_not_in_is_an_empty_list_not_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.foreign_team_id, "mmrs");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 200, "no gate: the membership join does the work");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    assert_eq!(go_body, rs_body, "{p} must be byte-identical");
    assert!(
        !names(&go_body)
            .iter()
            .any(|n| n.starts_with("mmrs-parity-sa")),
        "and nothing of this suite's is in it: {:?}",
        names(&go_body)
    );

    // The sibling refuses the same request, which is the asymmetry worth pinning.
    let switcher = format!(
        "/api/v4/teams/{}/channels/autocomplete?name=mmrs",
        f.foreign_team_id
    );
    let ((switch_status, _), _) = fetch_both_raw(&client, &f.plain_token, &switcher).await;
    assert_eq!(
        switch_status, 403,
        "/autocomplete gates on list_team_channels and this route does not"
    );
}

/// An empty term runs the base query alone — no union, no full text — and still lists the
/// caller's channels.
#[tokio::test]
async fn an_empty_term_lists_the_callers_channels() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bare = path(&f.team_id, "");
    let (go_bare, rs_bare) = fetch_both_stable(&client, &f.plain_token, &bare).await;
    assert_eq!(go_bare, rs_bare, "{bare} must be byte-identical");
    assert!(
        names(&go_bare).contains(&JOINED.to_owned()),
        "the base query alone still lists what the caller joined"
    );

    // `*` sanitises to nothing, so it is the same request — the same rule as the switcher's.
    let starred = path(&f.team_id, "*");
    let (go_star, rs_star) = fetch_both_stable(&client, &f.plain_token, &starred).await;
    assert_eq!(go_star, rs_star, "{starred} must be byte-identical");
    assert_eq!(
        go_star, go_bare,
        "`*` is not a wildcard, it is no term at all"
    );
}

/// The merged list is sorted by lower-cased display name, across both passes.
#[tokio::test]
async fn the_two_passes_are_merged_and_sorted_together() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let display_names: Vec<String> = rows(&go)
        .into_iter()
        .map(|(_, _, d)| d.to_lowercase())
        .collect();
    let mut sorted = display_names.clone();
    sorted.sort();
    assert_eq!(
        display_names, sorted,
        "the DM pass is appended and then the whole list is sorted, not concatenated"
    );

    // The DM really is interleaved rather than trailing, or the assertion above would hold for a
    // list that simply happened to be in order already.
    let kinds: Vec<String> = rows(&go).into_iter().map(|(_, t, _)| t).collect();
    assert!(
        kinds.contains(&"D".to_owned()),
        "the fixture must actually carry a direct message: {kinds:?}"
    );
}

/// `RequireTeamId`: alphanumeric, so the router's charset lets it through, but the wrong length.
#[tokio::test]
async fn a_team_id_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let p = format!("/api/v4/teams/{id}/channels/search_autocomplete?name=x");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = format!("/api/v4/teams/{}/channels/search_autocomplete", f.team_id);
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
    let _ = &f.plain_id;
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = "/api/v4/teams/aaaaaaaaaaaaaaaaaaaaaaaaaa/channels/search_autocomplete?name=x";

    let go = client
        .get(format!("{GO}{p}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{p}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{p}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, p);
}

/// The `LIKE` half of the union, exercised by a term that matches the **display name** and
/// nothing else — and from the middle of a word, so `to_tsquery`, which matches lexeme prefixes,
/// cannot find it either.
///
/// Without this, narrowing the `LIKE` list to `Name` alone survives every other test in the
/// file, because every other fixture's name and display name say the same words.
#[tokio::test]
async fn a_mid_word_display_name_fragment_matches_only_through_the_like_clause() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "ephyrin");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![DISPLAY_ONLY.to_owned()],
        "`ephyrin` is inside the display name, starts no lexeme, and is nowhere in the channel \
         name — only a substring match on DisplayName can find it"
    );
}

/// The full-text half, exercised by two words in the **wrong order**: `to_tsquery` ANDs the two
/// prefixes and does not care about adjacency, while no single column holds that string at all.
#[tokio::test]
async fn a_reversed_two_word_term_matches_only_through_the_fulltext_clause() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "quokka%20zephyrine");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![DISPLAY_ONLY.to_owned()],
        "`{DISPLAY_ONLY_NAME}` reversed matches through the tsvector and through nothing else"
    );
}

/// `strings.TrimSpace` in `AutocompleteChannelsForSearch` (channel.go:3437).
///
/// The term has to be one only `LIKE` can match, for the same reason as the test above it: an
/// untrimmed `  zephyrine  ` still matches through `to_tsquery`, because the full-text term is
/// built by splitting on whitespace and the padding simply disappears.
#[tokio::test]
async fn the_term_is_trimmed_before_it_reaches_the_store() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id, "%20%20ephyrin%20%20");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");
    assert_eq!(
        names(&go),
        vec![DISPLAY_ONLY.to_owned()],
        "the padding is trimmed, so this is the same request as `?name=ephyrin`"
    );
}
