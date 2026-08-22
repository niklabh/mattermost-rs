//! Cross-server parity for the read side of `BaseRoutes.ChannelCategories`:
//!
//! ```text
//! GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories
//! GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories/order
//! GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}
//! ```
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity_sidebar_categories
//! ```
//!
//! # The fixture is built to make wrong answers visible
//!
//! Every value in it was chosen because some plausible mistranslation produces the same bytes
//! without it:
//!
//! - **The Channels category's two explicit channels are in the *reverse* of display-name
//!   order** (`SB Charlie` before `SB Bravo`). Sorting by anything but
//!   `SidebarChannels.SortOrder` swaps them.
//! - **It also carries two orphans** — `Town Square` and `Off-Topic`, joined but never filed —
//!   which must appear *after* the explicit two, in display-name order. Dropping the orphan
//!   query loses them entirely; running it first puts them at the front.
//! - **Favorites is empty and stays empty.** It is the control for the orphan query's
//!   `selectChannels`/`selectDMs` guard: without it every channel the user is in would land here
//!   too.
//! - **Direct Messages holds exactly one DM**, so the `D`/`G`-to-DMs and `O`/`P`-to-Channels
//!   dispatch are separately observable. Exactly one, because a DM's display name is empty and
//!   two of them would tie under `ORDER BY DisplayName`.
//! - **`muted` and `collapsed` are true on *different* categories**, and `sorting` differs on
//!   all four. Two booleans that are both false cannot catch a port that reads the wrong column.
//! - **The subject is a freshly created user, not the admin.** The admin has accumulated DMs
//!   from every other suite, whose empty display names tie under the orphan query's `ORDER BY`
//!   and made this comparison order-random. See the parity-suite flake note in `MIGRATION.md`.
//!
//! # Everything is compared as raw bytes
//!
//! Including the framing: `getCategoriesForTeamForUser` and `getCategoryForTeamForUser` write
//! with `w.Write` and `getCategoryOrderForTeamForUser` with `json.NewEncoder`, so one of the
//! three carries a trailing newline and two do not. A `Value` comparison cannot see that.
//!
//! # Fixture rows all begin `mmrssidebar`
//!
//! Deliberately *not* the `mmrs-parity-` prefix the shared purge in `common` clears: that purge
//! runs once per test **binary**, and binaries run concurrently, so sharing the prefix means
//! another suite's start-up can delete this suite's team mid-run. These rows are cleared by
//! [`purge_sidebar_fixtures`] at the start of this binary instead, which nothing else touches.

mod common;

use common::{
    GO, assert_error_bodies_match_except_known_gaps, client, fetch_both, fetch_both_raw,
    go_minted_token, logged_in_user_id, stack_enabled,
};

const PREFIX: &str = "mmrssidebar";

/// Remove every row this suite authors. Runs once, before any fixture is built — an assertion
/// panics past trailing cleanup, so the start of the run is the only place a purge is reliable.
///
/// Selection is by team, not by name: Go authors `town-square`, `off-topic` and three
/// `SidebarCategories` rows on this suite's behalf, and none of them carries the prefix. That is
/// the gap [D-155] records for the shared purge; it is closed here for these rows.
async fn purge_sidebar_fixtures() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrssidebar%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrssidebar%'";
    let channels_of_teams = format!("SELECT id FROM channels WHERE teamid IN ({TEAMS})");
    // A DM is named `<userId>__<userId>` and belongs to no team, so it is reachable only through
    // its members. Cleared before the users are, while the join still resolves.
    let dms_of_users = format!(
        "SELECT channelid FROM channelmembers WHERE userid IN ({USERS}) \
         AND channelid IN (SELECT id FROM channels WHERE type IN ('D', 'G'))"
    );

    for statement in [
        format!(
            "DELETE FROM sidebarchannels WHERE categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS}))"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        format!("DELETE FROM sidebarchannels WHERE userid IN ({USERS})"),
        format!(
            "DELETE FROM posts WHERE channelid IN ({channels_of_teams}) OR channelid IN ({dms_of_users})"
        ),
        format!(
            "DELETE FROM channelmemberhistory WHERE channelid IN ({channels_of_teams}) OR channelid IN ({dms_of_users})"
        ),
        format!(
            "DELETE FROM channelmembers WHERE channelid IN ({channels_of_teams}) OR channelid IN ({dms_of_users})"
        ),
        format!("DELETE FROM channelmembers WHERE userid IN ({USERS})"),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM channels WHERE teamid IN ({TEAMS})"),
        "DELETE FROM channels WHERE type IN ('D', 'G') AND id NOT IN (SELECT channelid FROM channelmembers) AND name LIKE '%\\_\\_%' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users))".to_owned(),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrssidebar%'".to_owned(),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrssidebar%'".to_owned(),
    ] {
        let _ = sqlx::query(&statement).execute(&pool).await;
    }
}

async fn go_post(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "POST {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the body decodes")
}

async fn go_put(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .put(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "PUT {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the body decodes")
}

struct Fixture {
    team_id: String,
    /// A second team the subject is **not** a member of, for the `view_team` gate.
    other_team_id: String,
    /// A third team the subject **is** a member of, so a request naming it clears the
    /// `view_team` gate and the only thing left to refuse is the category's own `TeamId`.
    member_team_id: String,
    subject_id: String,
    subject_token: String,
    admin_token: String,
    admin_id: String,
    custom_category_id: String,
    bravo: String,
    charlie: String,
    dm_id: String,
}

impl Fixture {
    fn default_category(&self, kind: &str) -> String {
        format!("{kind}_{}_{}", self.subject_id, self.team_id)
    }
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture).await
}

async fn build_fixture() -> Fixture {
    purge_sidebar_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let admin_id = logged_in_user_id().to_owned();

    let team = go_post(
        &client,
        &admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}main"),
            "display_name": "SB Main",
            "type": "O",
        }),
    )
    .await;
    let team_id = team["id"].as_str().expect("an id").to_owned();

    let other_team = go_post(
        &client,
        &admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}other"),
            "display_name": "SB Other",
            "type": "O",
        }),
    )
    .await;
    let other_team_id = other_team["id"].as_str().expect("an id").to_owned();

    let member_team = go_post(
        &client,
        &admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}member"),
            "display_name": "SB Member",
            "type": "O",
        }),
    )
    .await;
    let member_team_id = member_team["id"].as_str().expect("an id").to_owned();

    let mut channels = Vec::new();
    for (name, display) in [
        ("alpha", "SB Alpha"),
        ("bravo", "SB Bravo"),
        ("charlie", "SB Charlie"),
    ] {
        let channel = go_post(
            &client,
            &admin_token,
            "/api/v4/channels",
            serde_json::json!({
                "team_id": team_id,
                "name": format!("{PREFIX}-{name}"),
                "display_name": display,
                "type": "O",
            }),
        )
        .await;
        channels.push(channel["id"].as_str().expect("an id").to_owned());
    }
    let (alpha, bravo, charlie) = (
        channels[0].clone(),
        channels[1].clone(),
        channels[2].clone(),
    );

    // The subject: a brand-new user, so its DM list is exactly what this file puts in it.
    let username = format!("{PREFIX}subject");
    let password = "Mmrs-Sidebar-1234";
    let subject = go_post(
        &client,
        &admin_token,
        "/api/v4/users",
        serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": password,
        }),
    )
    .await;
    let subject_id = subject["id"].as_str().expect("an id").to_owned();

    // Joining the team is what creates the three default categories, so everything below
    // depends on this having happened first.
    go_post(
        &client,
        &admin_token,
        &format!("/api/v4/teams/{team_id}/members"),
        serde_json::json!({ "team_id": team_id, "user_id": subject_id }),
    )
    .await;
    go_post(
        &client,
        &admin_token,
        &format!("/api/v4/teams/{member_team_id}/members"),
        serde_json::json!({ "team_id": member_team_id, "user_id": subject_id }),
    )
    .await;
    for channel in [&alpha, &bravo, &charlie] {
        go_post(
            &client,
            &admin_token,
            &format!("/api/v4/channels/{channel}/members"),
            serde_json::json!({ "user_id": subject_id }),
        )
        .await;
    }

    let login = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": password }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "the subject cannot log in");
    let subject_token = login
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned();

    // Exactly one DM, so the Direct Messages category is non-empty and cannot tie with itself.
    let dm = go_post(
        &client,
        &admin_token,
        "/api/v4/channels/direct",
        serde_json::json!([admin_id, subject_id]),
    )
    .await;
    let dm_id = dm["id"].as_str().expect("an id").to_owned();

    let categories = format!("/api/v4/users/{subject_id}/teams/{team_id}/channels/categories");

    // A custom category: `muted` true here and nowhere else, `sorting` `alpha` here and nowhere
    // else, and it holds `SB Alpha` — which therefore stops being an orphan.
    let custom = go_post(
        &client,
        &subject_token,
        &categories,
        serde_json::json!({
            "user_id": subject_id,
            "team_id": team_id,
            "display_name": "SB Custom",
            "muted": true,
            "sorting": "alpha",
            "channel_ids": [alpha],
        }),
    )
    .await;
    let custom_category_id = custom["id"].as_str().expect("an id").to_owned();

    // The Channels category: `collapsed` true here and nowhere else, `sorting` `manual`, and its
    // two explicit channels in the reverse of display-name order.
    let channels_category = format!("{kind}_{subject_id}_{team_id}", kind = "channels");
    go_put(
        &client,
        &subject_token,
        &format!("{categories}/{channels_category}"),
        serde_json::json!({
            "id": channels_category,
            "user_id": subject_id,
            "team_id": team_id,
            "type": "channels",
            "display_name": "Channels",
            "collapsed": true,
            "sorting": "manual",
            "channel_ids": [charlie, bravo],
        }),
    )
    .await;

    Fixture {
        team_id,
        other_team_id,
        member_team_id,
        subject_id,
        subject_token,
        admin_token,
        admin_id,
        custom_category_id,
        bravo,
        charlie,
        dm_id,
    }
}

/// The collection route, byte for byte, asked by the subject about itself.
#[tokio::test]
async fn categories_match_go_for_the_owner() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (go, rust) = fetch_both(&client, &f.subject_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rust),
        "{path}"
    );

    // And the fixture really is the shape the module docs describe — a green byte comparison
    // over a degenerate fixture proves nothing, so the fixture itself is asserted.
    let body: serde_json::Value = serde_json::from_slice(&go).expect("Go's body is JSON");
    let categories = body["categories"].as_array().expect("an array");
    let by_type = |kind: &str| {
        categories
            .iter()
            .find(|c| c["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} category in {body}"))
    };

    let channels = by_type("channels");
    let ids: Vec<&str> = channels["channel_ids"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        ids.len(),
        4,
        "two explicit channels and two orphans: {channels}"
    );
    assert_eq!(
        (ids[0], ids[1]),
        (f.charlie.as_str(), f.bravo.as_str()),
        "the explicit two come first, in SortOrder order — not display-name order"
    );
    assert_eq!(channels["collapsed"], true);
    assert_eq!(channels["muted"], false);
    assert_eq!(channels["sorting"], "manual");

    assert_eq!(
        by_type("favorites")["channel_ids"],
        serde_json::json!([]),
        "favorites must never collect orphans"
    );
    assert_eq!(
        by_type("direct_messages")["channel_ids"],
        serde_json::json!([f.dm_id]),
        "the DM lands in Direct Messages, not in Channels"
    );
    assert_eq!(by_type("custom")["muted"], true);
    assert_eq!(by_type("custom")["collapsed"], false);
    assert_eq!(by_type("custom")["sorting"], "alpha");

    // `order` repeats the ids in the same order the categories appear in.
    let order: Vec<&str> = body["order"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let listed: Vec<&str> = categories.iter().filter_map(|c| c["id"].as_str()).collect();
    assert_eq!(order, listed);
}

/// `json.Marshal` + `w.Write`: no trailing newline, unlike `/order`.
#[tokio::test]
async fn the_collection_body_carries_no_trailing_newline() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (go, rust) = fetch_both(&client, &f.subject_token, &path).await;
    assert_eq!(go.last(), Some(&b'}'), "Go's `w.Write` adds no newline");
    assert_eq!(rust.last(), Some(&b'}'));
}

/// An admin holding `edit_other_users` reads somebody else's sidebar, and gets the same bytes.
#[tokio::test]
async fn categories_match_go_when_an_admin_asks_about_another_user() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (go, rust) = fetch_both(&client, &f.admin_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rust));
}

/// `me` resolves to the session's own id, before validation — the same alias every other route
/// in this port honours.
#[tokio::test]
async fn the_me_alias_answers_as_the_session_user() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let aliased = format!("/api/v4/users/me/teams/{}/channels/categories", f.team_id);
    let explicit = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (go_alias, rust_alias) = fetch_both(&client, &f.subject_token, &aliased).await;
    let (_, rust_explicit) = fetch_both(&client, &f.subject_token, &explicit).await;

    assert_eq!(
        String::from_utf8_lossy(&go_alias),
        String::from_utf8_lossy(&rust_alias)
    );
    assert_eq!(
        String::from_utf8_lossy(&rust_alias),
        String::from_utf8_lossy(&rust_explicit),
        "`me` must be the same answer as the id it stands for"
    );
}

/// `json.NewEncoder(w).Encode`: a **trailing newline**, where the two siblings have none.
#[tokio::test]
async fn the_order_route_matches_go_including_its_trailing_newline() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/order",
        f.subject_id, f.team_id
    );
    let (go, rust) = fetch_both(&client, &f.subject_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rust));
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "the encoder writes a newline: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rust.last(), Some(&b'\n'));

    // It is the ids alone, in the collection route's `order`.
    let collection = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (go_collection, _) = fetch_both(&client, &f.subject_token, &collection).await;
    let collection: serde_json::Value =
        serde_json::from_slice(&go_collection).expect("Go's body is JSON");
    let order: serde_json::Value = serde_json::from_slice(&rust).expect("our body is JSON");
    assert_eq!(order, collection["order"]);
}

/// The singular route on a **custom** category — a real 26-character id.
#[tokio::test]
async fn a_custom_category_matches_go() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{}",
        f.subject_id, f.team_id, f.custom_category_id
    );
    let (go, rust) = fetch_both(&client, &f.subject_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rust));
    assert_eq!(go.last(), Some(&b'}'), "`w.Write`, so no newline");
}

/// The singular route on a **default** category, whose id is `{type}_{userId}_{teamId}`.
///
/// This is the case that decides whether the route is usable at all: the id carries two
/// underscores, so it is outside the `[A-Za-z0-9]+` charset the shared id middleware enforces,
/// and naming the path parameter `{category_id}` would have forwarded every one of these to Go.
#[tokio::test]
async fn a_default_category_id_with_underscores_is_served_not_forwarded() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    for kind in ["favorites", "channels", "direct_messages"] {
        let path = format!(
            "/api/v4/users/{}/teams/{}/channels/categories/{}",
            f.subject_id,
            f.team_id,
            f.default_category(kind)
        );
        // `fetch_both` asserts `x-mmrs-served-by: rust`, which is the forwarding claim.
        let (go, rust) = fetch_both(&client, &f.subject_token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rust),
            "{path}"
        );
    }
}

/// The singular route's answer is the same category the collection route lists.
#[tokio::test]
async fn the_singular_route_agrees_with_the_collection() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let collection = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.team_id
    );
    let (_, listed) = fetch_both(&client, &f.subject_token, &collection).await;
    let listed: serde_json::Value = serde_json::from_slice(&listed).expect("JSON");

    let channels_id = f.default_category("channels");
    let single = format!("{collection}/{channels_id}");
    let (_, one) = fetch_both(&client, &f.subject_token, &single).await;
    let one: serde_json::Value = serde_json::from_slice(&one).expect("JSON");

    let from_list = listed["categories"]
        .as_array()
        .expect("an array")
        .iter()
        .find(|c| c["id"] == channels_id.as_str())
        .expect("the channels category is listed");
    assert_eq!(&one, from_list);
}

// ---------------------------------------------------------------------------
// Refusals. Error bodies are wire format too — the webapp branches on `id`.
// ---------------------------------------------------------------------------

/// Gate one on the two list routes: `SessionHasPermissionToUser`. The subject is not an admin,
/// so it cannot read the admin's sidebar.
#[tokio::test]
async fn reading_another_users_categories_is_refused() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    for suffix in ["", "/order"] {
        let path = format!(
            "/api/v4/users/{}/teams/{}/channels/categories{suffix}",
            f.admin_id, f.team_id
        );
        let ((go_status, go_body), (rust_status, rust_body)) =
            fetch_both_raw(&client, &f.subject_token, &path).await;
        assert_eq!(
            go_status,
            403,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rust_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
        assert_eq!(go["id"], "api.context.permissions.app_error");
    }
}

/// Gate two: `SessionHasPermissionToTeam(view_team)`. The subject is a member of `team_id` and
/// not of `other_team_id`, and asks about **itself** — so only the team gate can refuse.
#[tokio::test]
async fn a_team_the_caller_cannot_see_is_refused() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories",
        f.subject_id, f.other_team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
}

/// **The one place the singular route's gate is observably not the list routes' gate.**
///
/// The subject names *itself* in the path — which `SessionHasPermissionToUser` grants outright,
/// through its self shortcut — and asks for a category belonging to the **admin**.
/// `SessionHasPermissionToCategory` compares `category.UserId` against the path's `user_id` as
/// well as against the session's, so it refuses. A port that reached for the more familiar gate
/// would answer 200 here and hand one user another user's sidebar.
#[tokio::test]
async fn a_category_belonging_to_someone_else_is_refused_even_when_naming_yourself() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let admins_category = format!("channels_{}_{}", f.admin_id, f.team_id);
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{admins_category}",
        f.subject_id, f.team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path} must not hand over another user's category: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// A syntactically valid category id that names no row is a **403**, not a 404: the gate fetches
/// the category itself and denies on the miss, so `GetSidebarCategory`'s own 404 is unreachable
/// here for anyone without `edit_other_users`.
#[tokio::test]
async fn a_category_that_does_not_exist_is_a_403_not_a_404() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/y9i4er48tt8bukijy7i3u5y9ar",
        f.subject_id, f.team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
}

/// The same missing id, asked by an **admin**: `edit_other_users` short-circuits the gate, so
/// the lookup does run and its 404 is reachable after all. The pair pins that the 403 above is
/// the *gate's* answer rather than a status this port invented.
#[tokio::test]
async fn an_admin_reaches_the_404_the_gate_hides_from_everyone_else() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/y9i4er48tt8bukijy7i3u5y9ar",
        f.subject_id, f.team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.admin_token, &path).await;
    assert_eq!(
        go_status,
        404,
        "{path}: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    assert_eq!(go["id"], "app.channel.sidebar_categories.app_error");
}

/// `RequireUserId().RequireTeamId().RequireCategoryId()`, each reported under its own parameter
/// name. All three segments are inside Go's mux charset, so gorilla routes them and the handler
/// answers — none of these is a 404.
#[tokio::test]
async fn malformed_path_segments_match_gos_400s() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let cases = [
        format!(
            "/api/v4/users/notauserid/teams/{}/channels/categories",
            f.team_id
        ),
        format!(
            "/api/v4/users/{}/teams/notateamid/channels/categories",
            f.subject_id
        ),
        format!(
            "/api/v4/users/notauserid/teams/{}/channels/categories/order",
            f.team_id
        ),
        // Passes the `[A-Za-z0-9_-]+` mux class and fails `IsValidCategoryId`.
        format!(
            "/api/v4/users/{}/teams/{}/channels/categories/notacategoryid",
            f.subject_id, f.team_id
        ),
        // `custom_…` looks like a default category id but `custom` is not in Go's alternation.
        format!(
            "/api/v4/users/{}/teams/{}/channels/categories/custom_{}_{}",
            f.subject_id, f.team_id, f.subject_id, f.team_id
        ),
    ];

    for path in cases {
        let ((go_status, go_body), (rust_status, rust_body)) =
            fetch_both_raw(&client, &f.subject_token, &path).await;
        assert_eq!(
            go_status,
            400,
            "{path}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rust_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
        assert_eq!(
            go["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
    }
}

/// The unanchored half of `IsValidCategoryId`: Go's regexp is used with `MatchString`, so a
/// default-category id with junk glued on **passes validation** and reaches the permission gate,
/// which refuses it. A port that anchored the pattern would 400 here where Go 403s.
#[tokio::test]
async fn an_unanchored_category_id_passes_validation_and_is_refused_by_the_gate() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/zz{}",
        f.subject_id,
        f.team_id,
        f.default_category("channels")
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path} should pass RequireCategoryId and fail the gate: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
}

/// `SessionHasPermissionToCategory` compares `category.UserId` **twice**, and this is the request
/// that separates the two comparisons.
///
/// The caller asks for **its own** category while naming *somebody else* in the path. The
/// session-side comparison passes — the category really is the caller's — so only
/// `category.UserId == userID`, the comparison against the path, can refuse it. Dropping that one
/// line survived every other test in this file:
/// `a_category_belonging_to_someone_else_is_refused_even_when_naming_yourself` is the mirror
/// image and fails the *session* comparison instead, so it cannot tell the two apart.
///
/// Nothing in Go's handler checks the path's `user_id` against the session for this route, so
/// this request really does reach the gate rather than being turned away earlier.
#[tokio::test]
async fn naming_someone_else_in_the_path_does_not_get_you_your_own_category() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let own_category = f.default_category("channels");
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{own_category}",
        f.admin_id, f.team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path}: the category is the caller's, but the path names someone else: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// The gate's third comparison, `category.TeamId == teamID`, separated the same way.
///
/// The caller asks for its own category **on the wrong team**, naming a team it really is a
/// member of — so the `view_team` gate that follows would grant, and the category's own `TeamId`
/// is the only thing left to refuse with. Against `other_team_id` this would be a 403 either way
/// and prove nothing; `member_team_id` exists exactly so it does not.
#[tokio::test]
async fn a_category_from_another_team_is_refused_even_on_a_team_you_can_see() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    // First, evidence that the `view_team` gate really would grant for this team: the caller's
    // own categories on it are readable. Without this the 403 below could be the team gate's.
    let readable = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{}",
        f.subject_id,
        f.member_team_id,
        format_args!("channels_{}_{}", f.subject_id, f.member_team_id)
    );
    let (go, rust) = fetch_both(&client, &f.subject_token, &readable).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rust));

    let wrong_team = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{}",
        f.subject_id,
        f.member_team_id,
        f.default_category("channels")
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &wrong_team).await;
    assert_eq!(
        go_status,
        403,
        "{wrong_team}: the category belongs to another team: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &wrong_team);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// And the plainest unauthorised read of all: somebody else's category, named honestly.
///
/// The caller asks for the admin's category with the **admin** in the path, on a team the caller
/// really is a member of — so the `view_team` gate grants and only
/// `category.UserId == session.UserId` stands between the caller and another user's sidebar.
/// Replacing that comparison with a second copy of the path one survived the whole suite until
/// this existed, which is the shape of the bug it would have been: the two comparisons look
/// interchangeable and are not.
#[tokio::test]
async fn another_users_category_is_refused_when_the_path_names_its_real_owner() {
    if !stack_enabled() {
        return;
    }
    let f = fixture().await;
    let client = client();

    let admins_category = format!("channels_{}_{}", f.admin_id, f.team_id);
    let path = format!(
        "/api/v4/users/{}/teams/{}/channels/categories/{admins_category}",
        f.admin_id, f.team_id
    );
    let ((go_status, go_body), (rust_status, rust_body)) =
        fetch_both_raw(&client, &f.subject_token, &path).await;
    assert_eq!(
        go_status,
        403,
        "{path} must not hand the admin's sidebar to another user: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rust_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &path);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}
