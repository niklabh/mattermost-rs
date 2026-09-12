//! Cross-server parity for the three admin channel-listing routes —
//! `GET /api/v4/channels`, `POST /api/v4/channels/search` and
//! `POST /api/v4/channels/group/search`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_search_all
//! ```
//!
//! # The fourth route of this family is not here, and cannot be
//!
//! `GET /api/v4/teams/{team_id}/channels/managed_categories` is registered by Go **only when
//! `FeatureFlags.ManagedChannelCategories` is set** (api4/channel.go:72), and that flag defaults
//! to false (feature_flags.go:202). So on this stack the path is not a route at all: gorilla's
//! `NotFoundHandler` answers `api.context.404.app_error`, not the handler's 501. Registering it
//! here would replace a 404 with a 501 and *break* parity, so it stays forwarded and
//! [`the_managed_categories_route_is_a_404_on_both`] pins that.
//!
//! # What the fixture has to discriminate
//!
//! Two things a port gets wrong silently:
//!
//! - **The sort key is `c.DisplayName, t.DisplayName`** with no third key. So the fixture carries
//!   two channels with the *same* display name on two teams whose display names differ, and a
//!   third whose display name sorts before both while its `Name` sorts after — otherwise a port
//!   that ordered by `Name`, or dropped the team key, would be indistinguishable.
//! - **`include_total_count` and body pagination change the response's top-level type**, from an
//!   array to `{"channels":…,"total_count":…}`. Every default-only test is blind to that.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id, plain_username,
    post_both_raw_stable, purge_api_fixtures,
};

/// The stem every fixture channel's name and display name carries, so one search term reaches
/// exactly this fixture and nothing another suite created.
const STEM: &str = "mmrscastx";

struct Fixture {
    /// Display name `mmrs parity casta`.
    team_a: String,
    /// Display name `mmrs parity castb` — sorts after `team_a`.
    team_b: String,
    /// On `team_a`, display name `a<STEM> alpha` and name `mmrs-parity-<STEM>zzz` — the display
    /// name sorts first while the name sorts last, so ordering on the wrong column is visible.
    ///
    /// The fixture's other four channels — two sharing a display name across `team_a` and
    /// `team_b`, one private, one archived — are identified by what they are rather than by id,
    /// so only this one is kept.
    alpha: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_a = create_team(client, token, "casta").await;
            let team_b = create_team(client, token, "castb").await;

            // `shared_b` first: see the struct field.
            let _shared_b = create_channel_named(
                client,
                token,
                &team_b,
                &format!("mmrs-parity-{STEM}bbb"),
                &format!("z{STEM} shared"),
                "O",
            )
            .await;
            let _shared_a = create_channel_named(
                client,
                token,
                &team_a,
                &format!("mmrs-parity-{STEM}aaa"),
                &format!("z{STEM} shared"),
                "O",
            )
            .await;
            let alpha = create_channel_named(
                client,
                token,
                &team_a,
                &format!("mmrs-parity-{STEM}zzz"),
                &format!("a{STEM} alpha"),
                "O",
            )
            .await;
            let _private = create_channel_named(
                client,
                token,
                &team_a,
                &format!("mmrs-parity-{STEM}ppp"),
                &format!("m{STEM} private"),
                "P",
            )
            .await;
            let archived = create_channel_named(
                client,
                token,
                &team_a,
                &format!("mmrs-parity-{STEM}ddd"),
                &format!("n{STEM} archived"),
                "O",
            )
            .await;
            archive_channel(client, token, &archived).await;

            let plain = create_plain_user(client, token, &team_a, "castplain").await;

            Fixture {
                team_a,
                team_b,
                alpha,
                plain_token: plain.token,
            }
        })
        .await
}

/// `POST /channels` with the name, display name and type all spelled out.
async fn create_channel_named(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    name: &str,
    display_name: &str,
    channel_type: &str,
) -> String {
    purge_api_fixtures().await;
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": name,
            "display_name": display_name,
            "type": channel_type,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn archive_channel(client: &reqwest::Client, token: &str, channel_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving the channel failed"
    );
}

/// The `display_name`s of a search response, in the order they arrived.
fn display_names(body: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(body).expect("the body is JSON");
    let list = match &value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(map) => map["channels"]
            .as_array()
            .expect("the wrapper carries a list")
            .clone(),
        other => panic!("unexpected response shape: {other}"),
    };
    list.iter()
        .map(|c| c["display_name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The `(display_name, team_display_name)` sort key of every row, in the order it arrived.
fn sort_keys(body: &[u8]) -> Vec<(String, String)> {
    rows_of(body)
        .iter()
        .map(|c| {
            (
                c["display_name"].as_str().unwrap_or_default().to_owned(),
                c["team_display_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

fn rows_of(body: &[u8]) -> Vec<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(body).expect("the body is JSON");
    match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(map) => map["channels"]
            .as_array()
            .expect("the wrapper carries a list")
            .clone(),
        other => panic!("unexpected response shape: {other}"),
    }
}

/// Compare two channel lists that Go's `ORDER BY` cannot fully separate.
///
/// # Why byte equality is the wrong assertion for the unfiltered list
///
/// `getAllChannels` orders by `c.DisplayName, Teams.DisplayName` and **has no third key**, so two
/// channels with the same display name on the same team are tied and each server returns them in
/// whatever order its plan produced. The development database has such pairs — other suites
/// create several channels named `mmrs admin go` on `Slice Team` and archive them — so a byte
/// comparison of the whole table fails on ties that Go itself does not define. Measured: 200 rows
/// in the same multiset, differing only inside one tie group.
///
/// Adding `c.Id` to the port's `ORDER BY` would make the comparison pass and would be **wrong**:
/// it would make this server more deterministic than the one it has to match, and a client paging
/// through the list would then see an order Go never produces.
///
/// So this asserts the two things Go *does* define, and nothing it does not:
///
/// 1. **The sequence of sort keys is identical** — which pins both keys, their order, and their
///    direction. A port that dropped `Teams.DisplayName`, reversed either key, or sorted on
///    `Name` instead fails here.
/// 2. **Each tie group holds the same rows**, compared as a set of whole JSON objects, so no
///    field of any channel can differ and no row can appear or vanish.
///
/// Byte equality is still tried first and is what normally holds.
fn assert_lists_agree_modulo_ties(go: &[u8], rust: &[u8], label: &str) {
    if go == rust {
        return;
    }
    let go_keys = sort_keys(go);
    let rust_keys = sort_keys(rust);
    assert_eq!(
        rust_keys, go_keys,
        "{label}: the sort keys themselves diverged"
    );
    assert!(
        go_keys.windows(2).all(|w| w[0] <= w[1]),
        "{label}: Go's own answer is not sorted by (display_name, team_display_name)"
    );

    let mut go_group: Vec<String> = Vec::new();
    let mut rust_group: Vec<String> = Vec::new();
    let go_rows = rows_of(go);
    let rust_rows = rows_of(rust);
    for index in 0..go_keys.len() {
        go_group.push(go_rows[index].to_string());
        rust_group.push(rust_rows[index].to_string());
        let boundary = index + 1 == go_keys.len() || go_keys[index] != go_keys[index + 1];
        if boundary {
            go_group.sort();
            rust_group.sort();
            assert_eq!(
                rust_group, go_group,
                "{label}: the rows tied at {:?} are not the same rows",
                go_keys[index]
            );
            go_group.clear();
            rust_group.clear();
        }
    }
}

fn search(term: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "term": term })).expect("serialises")
}

// ---------------------------------------------------------------------------
// GET /api/v4/channels — getAllChannels
// ---------------------------------------------------------------------------

/// The default list, its paging, and the two query flags that filter it.
///
/// `fetch_both_stable` rather than a single pair: this route reports **every** channel on the
/// server, so any other suite creating one inside the window is a byte difference that is not a
/// divergence.
#[tokio::test]
async fn the_unfiltered_list_and_its_pages_match_go() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    for path in [
        "/api/v4/channels",
        "/api/v4/channels?per_page=5",
        "/api/v4/channels?page=1&per_page=5",
        "/api/v4/channels?per_page=200",
        "/api/v4/channels?include_deleted=true&per_page=200",
        "/api/v4/channels?exclude_default_channels=true&per_page=200",
        // Both at once: the archived channels appear *and* town-square/off-topic do not.
        "/api/v4/channels?include_deleted=true&exclude_default_channels=true&per_page=200",
    ] {
        let (go, rust) = fetch_both_stable(&client, &token, path).await;
        assert_lists_agree_modulo_ties(&go, &rust, path);
    }
}

/// `?include_total_count=true` is a different **type** on the wire, and the count it carries is
/// not the length of the list it carries.
#[tokio::test]
async fn include_total_count_wraps_the_list_in_an_object() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    let path = "/api/v4/channels?per_page=3&include_total_count=true";
    let (go, rust) = fetch_both_stable(&client, &token, path).await;
    assert_lists_agree_modulo_ties(&go, &rust, path);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rust).expect("JSON")["total_count"],
        serde_json::from_slice::<serde_json::Value>(&go).expect("JSON")["total_count"],
        "the count must agree even when a tie reorders the page"
    );

    // The shape, asserted rather than inferred: a bare array here would compare equal to Go only
    // if Go had also regressed.
    let value: serde_json::Value = serde_json::from_slice(&rust).expect("JSON");
    let object = value.as_object().expect("include_total_count wraps");
    assert_eq!(object.len(), 2, "exactly `channels` and `total_count`");
    let listed = object["channels"].as_array().expect("a list").len();
    let total = object["total_count"].as_i64().expect("a number");
    assert_eq!(listed, 3, "per_page=3 caps the page");
    assert!(
        total > listed as i64,
        "the count is the whole table, not the page: {total} vs {listed}"
    );

    // Without the flag the very same query is a bare array.
    let (_, plain) = fetch_both_stable(&client, &token, "/api/v4/channels?per_page=3").await;
    assert!(
        serde_json::from_slice::<serde_json::Value>(&plain)
            .expect("JSON")
            .is_array(),
        "the default response is a bare list"
    );
}

/// `?include_total_count=true` **with** `?include_deleted=true`: the two independent filters have
/// to reach the count query as well as the list query, and the count has to move when they do.
#[tokio::test]
async fn the_total_count_follows_the_same_filters_as_the_list() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    let count_for = async |path: &str| -> i64 {
        let (go, rust) = fetch_both_stable(&client, &token, path).await;
        assert_lists_agree_modulo_ties(&go, &rust, path);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&rust).expect("JSON")["total_count"],
            serde_json::from_slice::<serde_json::Value>(&go).expect("JSON")["total_count"],
            "{path}: the count diverged"
        );
        serde_json::from_slice::<serde_json::Value>(&rust).expect("JSON")["total_count"]
            .as_i64()
            .expect("a number")
    };

    let live = count_for("/api/v4/channels?per_page=1&include_total_count=true").await;
    let with_archived =
        count_for("/api/v4/channels?per_page=1&include_total_count=true&include_deleted=true")
            .await;
    let without_defaults = count_for(
        "/api/v4/channels?per_page=1&include_total_count=true&exclude_default_channels=true",
    )
    .await;

    assert!(
        with_archived > live,
        "include_deleted must widen the count: {with_archived} vs {live}"
    );
    assert!(
        without_defaults < live,
        "exclude_default_channels must narrow it: {without_defaults} vs {live}"
    );
}

/// The gate: three sysconsole permissions, any of which opens the route, and a plain user holds
/// none of them.
#[tokio::test]
async fn a_plain_user_is_refused_identically() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let (go, rust) =
        fetch_both_raw(&client, &fixture.plain_token, "/api/v4/channels?per_page=5").await;
    assert_eq!(rust.0, 403, "a plain user is refused");
    assert_eq!(rust.0, go.0);
    assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "GET /api/v4/channels");
}

// ---------------------------------------------------------------------------
// POST /api/v4/channels/search — searchAllChannels, system_console branch
// ---------------------------------------------------------------------------

/// Order is wire format: `c.DisplayName, t.DisplayName`, and the fixture is built so that
/// **both** keys are load-bearing.
#[tokio::test]
async fn the_console_search_orders_by_display_name_then_team() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?include_deleted=true",
        &search(STEM),
    )
    .await;
    assert_eq!(rust.0, 200);
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1),
        "the console search diverged"
    );

    // Asserted on our own answer too, so a mutation that reorders is caught even if Go's planner
    // happened to agree with it.
    assert_eq!(
        display_names(&rust.1),
        vec![
            format!("a{STEM} alpha"),
            format!("m{STEM} private"),
            format!("n{STEM} archived"),
            format!("z{STEM} shared"),
            format!("z{STEM} shared"),
        ],
        "display name first, and the `alpha` channel sorts first despite its name sorting last"
    );

    // The two tied display names are broken by the **team** display name, and `casta` < `castb`.
    let value: serde_json::Value = serde_json::from_slice(&rust.1).expect("JSON");
    let tied: Vec<&str> = value
        .as_array()
        .expect("a list")
        .iter()
        .filter(|c| c["display_name"] == serde_json::json!(format!("z{STEM} shared")))
        .map(|c| c["team_id"].as_str().expect("a team id"))
        .collect();
    assert_eq!(
        tied,
        vec![f.team_a.as_str(), f.team_b.as_str()],
        "the tie-break is the team's display name, and `casta` comes first"
    );
}

/// Every boolean in the body that narrows the result set, one request each.
#[tokio::test]
async fn the_console_search_flags_narrow_the_same_way() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bodies: Vec<(&str, serde_json::Value)> = vec![
        ("plain", serde_json::json!({ "term": STEM })),
        (
            "public only",
            serde_json::json!({ "term": STEM, "public": true }),
        ),
        (
            "private only",
            serde_json::json!({ "term": STEM, "private": true }),
        ),
        (
            "both flags is neither",
            serde_json::json!({ "term": STEM, "public": true, "private": true }),
        ),
        (
            "include_deleted in the body",
            serde_json::json!({ "term": STEM, "include_deleted": true }),
        ),
        (
            "deleted beats include_deleted",
            serde_json::json!({ "term": STEM, "deleted": true, "include_deleted": true }),
        ),
        (
            "one team",
            serde_json::json!({ "term": STEM, "team_ids": [f.team_b] }),
        ),
        (
            "two teams",
            serde_json::json!({ "term": STEM, "team_ids": [f.team_a, f.team_b] }),
        ),
        (
            "a team with nothing in it",
            serde_json::json!({ "term": STEM, "team_ids": ["zzzzzzzzzzzzzzzzzzzzzzzzzz"] }),
        ),
        (
            "exclude_group_constrained",
            serde_json::json!({ "term": STEM, "exclude_group_constrained": true }),
        ),
        (
            "group_constrained",
            serde_json::json!({ "term": STEM, "group_constrained": true }),
        ),
        (
            "exclude_remote",
            serde_json::json!({ "term": STEM, "exclude_remote": true }),
        ),
        (
            "exclude_policy_constrained",
            serde_json::json!({ "term": STEM, "exclude_policy_constrained": true }),
        ),
        (
            "search by id finds a channel by its id",
            serde_json::json!({ "term": f.alpha, "include_search_by_id": true }),
        ),
        (
            "without that flag the id finds nothing",
            serde_json::json!({ "term": f.alpha }),
        ),
        (
            "exclude_default_channels",
            serde_json::json!({ "term": "", "exclude_default_channels": true, "team_ids": [f.team_a] }),
        ),
        (
            "an empty term is every channel, not none",
            serde_json::json!({ "term": "", "team_ids": [f.team_a] }),
        ),
        (
            "a term of only wildcards is the same as an empty one",
            serde_json::json!({ "term": "*", "team_ids": [f.team_a] }),
        ),
        (
            "access_control_policy_enforced",
            serde_json::json!({ "term": STEM, "access_control_policy_enforced": true }),
        ),
        (
            "exclude_access_control_policy_enforced",
            serde_json::json!({ "term": STEM, "exclude_access_control_policy_enforced": true }),
        ),
        (
            "not_associated_to_group",
            serde_json::json!({ "term": STEM, "not_associated_to_group": "zzzzzzzzzzzzzzzzzzzzzzzzzz" }),
        ),
        (
            "a multi-word term goes through the fulltext arm",
            serde_json::json!({ "term": format!("z{STEM} shared") }),
        ),
        (
            // `App.SearchAllChannels` trims; without that the LIKE carries the spaces and
            // matches nothing.
            "a padded term is trimmed before it reaches the store",
            serde_json::json!({ "term": format!("   {STEM}   ") }),
        ),
    ];

    for (label, body) in bodies {
        let raw = serde_json::to_vec(&body).expect("serialises");
        let (go, rust) =
            post_both_raw_stable(&client, &token, "/api/v4/channels/search", &raw).await;
        assert_eq!(rust.0, go.0, "{label}: status");
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "{label} diverged"
        );
    }
}

/// `page` and `per_page` in the **body** switch the response to the counted wrapper, and the
/// count is the whole match rather than the page.
#[tokio::test]
async fn body_pagination_switches_the_response_type() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    let paged = serde_json::json!({ "term": STEM, "page": 0, "per_page": 2 });
    let raw = serde_json::to_vec(&paged).expect("serialises");
    let (go, rust) = post_both_raw_stable(&client, &token, "/api/v4/channels/search", &raw).await;
    assert_eq!(rust.0, 200);
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1),
        "the paginated search diverged"
    );

    let value: serde_json::Value = serde_json::from_slice(&rust.1).expect("JSON");
    let object = value.as_object().expect("pagination wraps");
    assert_eq!(object["channels"].as_array().expect("a list").len(), 2);
    assert_eq!(
        object["total_count"].as_i64(),
        Some(4),
        "the count is every live match, not the page"
    );

    // The second page, and the third which runs out.
    for page in [1, 2] {
        let raw = serde_json::to_vec(&serde_json::json!({
            "term": STEM, "page": page, "per_page": 2
        }))
        .expect("serialises");
        let (go, rust) =
            post_both_raw_stable(&client, &token, "/api/v4/channels/search", &raw).await;
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "page {page} diverged"
        );
    }

    // Only one of the two is not pagination at all: a bare array, and no count query.
    for half in [
        serde_json::json!({ "term": STEM, "page": 0 }),
        serde_json::json!({ "term": STEM, "per_page": 2 }),
    ] {
        let raw = serde_json::to_vec(&half).expect("serialises");
        let (go, rust) =
            post_both_raw_stable(&client, &token, "/api/v4/channels/search", &raw).await;
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "half-pagination diverged"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&rust.1)
                .expect("JSON")
                .is_array(),
            "one of page/per_page alone is not pagination"
        );
    }
}

/// `?include_deleted=true` on the query string is OR-ed with the body's field, and `per_page`
/// there is **not** read at all — the body's is.
#[tokio::test]
async fn the_query_string_include_deleted_is_ored_with_the_body() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    let plain = search(STEM);
    let (_, without) =
        post_both_raw_stable(&client, &token, "/api/v4/channels/search", &plain).await;
    let (go, with) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?include_deleted=true",
        &plain,
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(&with.1),
        String::from_utf8_lossy(&go.1),
        "the query-string flag diverged"
    );
    assert_eq!(
        display_names(&without.1).len() + 1,
        display_names(&with.1).len(),
        "the archived fixture channel appears only with the flag"
    );

    // Garbage there is silently false, not a 400 — unlike `system_console`.
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?include_deleted=banana",
        &plain,
    )
    .await;
    assert_eq!(
        rust.0, 200,
        "an unparseable include_deleted is not an error"
    );
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1)
    );
}

/// The console branch's 403 names **one** permission where `getAllChannels`' names three.
#[tokio::test]
async fn the_console_search_refuses_a_plain_user_identically() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let (go, rust) = post_both_raw_stable(
        &client,
        &f.plain_token,
        "/api/v4/channels/search",
        &search(STEM),
    )
    .await;
    assert_eq!(rust.0, 403);
    assert_eq!(rust.0, go.0);
    assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "POST /channels/search");

    // `exclude_policy_constrained` is checked **before** the route's own gate, so the same caller
    // is refused with a different permission id when they set it.
    let body = serde_json::to_vec(&serde_json::json!({
        "term": STEM, "exclude_policy_constrained": true
    }))
    .expect("serialises");
    let (go_first, rust_first) =
        post_both_raw_stable(&client, &f.plain_token, "/api/v4/channels/search", &body).await;
    assert_eq!(rust_first.0, 403);
    assert_error_bodies_match_except_known_gaps(
        &go_first.1,
        &rust_first.1,
        "POST /channels/search with exclude_policy_constrained",
    );
    // **Which permission each refusal names is not observable over HTTP.**
    // `MakePermissionError` puts the list in `detailed_error`, and Go wipes that field before it
    // reaches the wire ([D-092]) — so the two 403s above differ only in `request_id`. Asserting
    // they differ would be asserting that two random ids are not equal. The list still matters
    // (it is what the server log says), and it is pinned by `make_permission_error_matches_go`
    // against a Go-generated oracle; what *this* test can see is the status and the id, and it
    // does.
}

// ---------------------------------------------------------------------------
// POST /api/v4/channels/search — searchAllChannels, system_console=false
// ---------------------------------------------------------------------------

/// The three branches of `system_console=false`, which share no code with the console one.
#[tokio::test]
async fn the_non_console_branches_match_go() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let cases: Vec<(&str, serde_json::Value)> = vec![
        // One valid team id: the team-scoped autocomplete.
        (
            "one team",
            serde_json::json!({ "term": STEM, "team_ids": [f.team_a] }),
        ),
        // …and its two filtered variants.
        (
            "private only",
            serde_json::json!({ "term": STEM, "team_ids": [f.team_a], "private": true }),
        ),
        (
            "exclude_group_constrained",
            serde_json::json!({
                "term": STEM, "team_ids": [f.team_a], "exclude_group_constrained": true
            }),
        ),
        // No team id at all: the cross-team autocomplete, with team data on every row.
        ("no teams", serde_json::json!({ "term": STEM })),
        (
            "an empty team list",
            serde_json::json!({ "term": STEM, "team_ids": [] }),
        ),
        // Two ids is *not* the team branch.
        (
            "two teams",
            serde_json::json!({ "term": STEM, "team_ids": [f.team_a, f.team_b] }),
        ),
        // One malformed id is not the team branch either — and so is not a 400.
        (
            "one malformed id",
            serde_json::json!({ "term": STEM, "team_ids": ["not-an-id"] }),
        ),
    ];

    for (label, body) in cases {
        let raw = serde_json::to_vec(&body).expect("serialises");
        let (go, rust) = post_both_raw_stable(
            &client,
            &token,
            "/api/v4/channels/search?system_console=false",
            &raw,
        )
        .await;
        assert_eq!(rust.0, go.0, "{label}: status");
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "{label} diverged"
        );
    }

    // The cross-team branch carries team data; the team-scoped one does not.
    let (_, cross) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?system_console=false",
        &search(STEM),
    )
    .await;
    let cross: serde_json::Value = serde_json::from_slice(&cross.1).expect("JSON");
    assert!(
        cross.as_array().expect("a list")[0]
            .as_object()
            .expect("an object")
            .contains_key("team_display_name"),
        "the cross-team branch is a ChannelListWithTeamData"
    );

    let raw = serde_json::to_vec(&serde_json::json!({ "term": STEM, "team_ids": [f.team_a] }))
        .expect("serialises");
    let (_, scoped) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?system_console=false",
        &raw,
    )
    .await;
    let scoped: serde_json::Value = serde_json::from_slice(&scoped.1).expect("JSON");
    assert!(
        !scoped.as_array().expect("a list")[0]
            .as_object()
            .expect("an object")
            .contains_key("team_display_name"),
        "the team-scoped branch is a plain ChannelList"
    );
}

/// The team-scoped branch is gated on `view_team`, and the cross-team one on nothing.
#[tokio::test]
async fn the_team_branch_is_gated_and_the_cross_team_branch_is_not() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The plain user is in `team_a` and not in `team_b`.
    let denied = serde_json::to_vec(&serde_json::json!({ "term": STEM, "team_ids": [f.team_b] }))
        .expect("serialises");
    let (go, rust) = post_both_raw_stable(
        &client,
        &f.plain_token,
        "/api/v4/channels/search?system_console=false",
        &denied,
    )
    .await;
    assert_eq!(rust.0, 403, "the team-scoped branch is gated on view_team");
    assert_eq!(rust.0, go.0, "the team gate");
    assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "the view_team refusal");

    // With no team id the same caller is served, because the query is scoped by membership.
    let (go, rust) = post_both_raw_stable(
        &client,
        &f.plain_token,
        "/api/v4/channels/search?system_console=false",
        &search(STEM),
    )
    .await;
    assert_eq!(rust.0, 200, "the cross-team branch has no permission gate");
    assert_eq!(rust.0, go.0);
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1),
    );
}

/// `?system_console` defaults to true, is true when empty, and 400s on anything unparseable.
#[tokio::test]
async fn the_system_console_flag_has_three_answers() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    let body = search(STEM);
    let (go_default, rust_default) =
        post_both_raw_stable(&client, &token, "/api/v4/channels/search", &body).await;
    for (label, path) in [
        ("empty", "/api/v4/channels/search?system_console="),
        (
            "explicit true",
            "/api/v4/channels/search?system_console=true",
        ),
        ("go's `1`", "/api/v4/channels/search?system_console=1"),
        ("go's `T`", "/api/v4/channels/search?system_console=T"),
    ] {
        let (go, rust) = post_both_raw_stable(&client, &token, path, &body).await;
        assert_eq!(rust.0, go.0, "{label}: status");
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "{label} diverged"
        );
        assert_eq!(
            rust.1, rust_default.1,
            "{label} must be the console branch, like the default"
        );
    }
    assert_eq!(rust_default.0, go_default.0);

    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/search?system_console=banana",
        &body,
    )
    .await;
    assert_eq!(rust.0, 400, "an unparseable system_console is the one 400");
    assert_eq!(rust.0, go.0);
    assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "system_console=banana");
}

/// Every way the body can fail to decode, on both search routes.
#[tokio::test]
async fn a_malformed_body_is_the_same_400_on_both_search_routes() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    fixture(&client, &token).await;

    for path in ["/api/v4/channels/search", "/api/v4/channels/group/search"] {
        for (label, body) in [
            ("empty", &b""[..]),
            ("null", &b"null"[..]),
            ("an array", &b"[]"[..]),
            ("a bare string", &b"\"town\""[..]),
            ("a number", &b"7"[..]),
            ("unclosed", &b"{\"term\":"[..]),
            ("a wrong-typed term", &b"{\"term\":7}"[..]),
            ("trailing junk", &b"{\"term\":\"a\"}{}"[..]),
        ] {
            let (go, rust) = post_both_raw_stable(&client, &token, path, body).await;
            assert_eq!(rust.0, go.0, "{path} {label}: status");
            if rust.0 == 400 {
                assert_error_bodies_match_except_known_gaps(
                    &go.1,
                    &rust.1,
                    &format!("{path} {label}"),
                );
            } else {
                // `{"term":"a"}{}` is **not** an error on either server: Go's `json.Decoder`
                // reads one value and never looks at what follows it, so the trailing `{}` is
                // ignored and the search runs. `decode_one_from_json` is that behaviour.
                assert_eq!(
                    rust.0, 200,
                    "{path} {label}: only the decoder's one-value rule"
                );
                assert_eq!(
                    String::from_utf8_lossy(&rust.1),
                    String::from_utf8_lossy(&go.1),
                    "{path} {label} diverged"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/v4/channels/group/search — searchGroupChannels
// ---------------------------------------------------------------------------

struct GroupFixture {
    /// The GM containing `one` and `two`.
    with_two: String,
    /// The GM containing `one` and `three`.
    with_three: String,
    one: String,
    two: String,
    three: String,
    /// `one`'s token. The admin is in hundreds of group messages left by other suites, so the
    /// store's `LIMIT 50` hides this fixture from an unfiltered search run as the admin; `one` is
    /// in exactly the two conversations this module created.
    one_token: String,
}

static GROUPS: tokio::sync::OnceCell<GroupFixture> = tokio::sync::OnceCell::const_new();

async fn group_fixture(client: &reqwest::Client, token: &str) -> &'static GroupFixture {
    GROUPS
        .get_or_init(|| async {
            let f = fixture(client, token).await;
            let one = create_plain_user(client, token, &f.team_a, "castgmone").await;
            let two = create_plain_user(client, token, &f.team_a, "castgmtwo").await;
            let three = create_plain_user(client, token, &f.team_a, "castgmthree").await;
            let me = logged_in_user_id().to_owned();

            let with_two = create_group_channel(client, token, &[&me, &one.id, &two.id]).await;
            let with_three = create_group_channel(client, token, &[&me, &one.id, &three.id]).await;

            GroupFixture {
                with_two,
                with_three,
                one: plain_username("castgmone"),
                two: plain_username("castgmtwo"),
                three: plain_username("castgmthree"),
                one_token: one.token,
            }
        })
        .await
}

async fn create_group_channel(
    client: &reqwest::Client,
    token: &str,
    user_ids: &[&String],
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!(user_ids))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the group channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

fn ids_of(body: &[u8]) -> std::collections::BTreeSet<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("JSON")
        .as_array()
        .expect("a list")
        .iter()
        .map(|c| c["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The term matches **member usernames**, every word has to match, and the empty term is the one
/// short circuit.
#[tokio::test]
async fn the_group_search_matches_usernames_word_by_word() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let g = group_fixture(&client, &token).await;

    // One word that is in only one conversation.
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/group/search",
        &search(&g.two),
    )
    .await;
    assert_eq!(rust.0, 200);
    assert_eq!(ids_of(&rust.1), ids_of(&go.1), "the single-word search");
    assert!(ids_of(&rust.1).contains(&g.with_two));
    assert!(!ids_of(&rust.1).contains(&g.with_three));

    // A word shared by both.
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/group/search",
        &search(&g.one),
    )
    .await;
    assert_eq!(ids_of(&rust.1), ids_of(&go.1));
    let found = ids_of(&rust.1);
    assert!(found.contains(&g.with_two) && found.contains(&g.with_three));

    // Two words: **both** must match the joined roster, so this is an intersection.
    let both = format!("{} {}", g.one, g.three);
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/group/search",
        &search(&both),
    )
    .await;
    assert_eq!(ids_of(&rust.1), ids_of(&go.1));
    assert_eq!(
        ids_of(&rust.1),
        std::iter::once(g.with_three.clone()).collect(),
        "every word has to match"
    );

    // Case is folded before the split.
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/group/search",
        &search(&g.two.to_uppercase()),
    )
    .await;
    assert_eq!(ids_of(&rust.1), ids_of(&go.1));
    assert!(ids_of(&rust.1).contains(&g.with_two), "the term is lowered");

    // A word that matches nothing is an empty list, not an error.
    let (go, rust) = post_both_raw_stable(
        &client,
        &token,
        "/api/v4/channels/group/search",
        &search("mmrsnosuchusernameanywhere"),
    )
    .await;
    assert_eq!(rust.0, 200);
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1),
    );
    assert_eq!(rust.1, b"[]\n", "an empty result is `[]`, not `null`");
}

/// The empty term short-circuits in the app; a term of a single space does **not**, and returns
/// the caller's group messages unfiltered.
#[tokio::test]
async fn the_empty_term_and_a_space_are_different_requests() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let g = group_fixture(&client, &token).await;

    // Run as `one` rather than as the admin: see `GroupFixture::one_token`.
    let (go, rust) = post_both_raw_stable(
        &client,
        &g.one_token,
        "/api/v4/channels/group/search",
        &search(""),
    )
    .await;
    assert_eq!(rust.0, 200);
    assert_eq!(rust.1, b"[]\n", "the empty term never reaches the store");
    assert_eq!(rust.1, go.1);

    let (go, rust) = post_both_raw_stable(
        &client,
        &g.one_token,
        "/api/v4/channels/group/search",
        &search(" "),
    )
    .await;
    assert_eq!(rust.0, 200);
    assert_eq!(ids_of(&rust.1), ids_of(&go.1), "a space is not empty");
    assert_eq!(
        ids_of(&rust.1),
        [g.with_two.clone(), g.with_three.clone()]
            .into_iter()
            .collect(),
        "a whitespace-only term is an unfiltered list of the caller's group messages"
    );
}

/// A caller sees only their **own** group messages: the plain user is not in either fixture GM.
#[tokio::test]
async fn the_group_search_is_scoped_to_the_caller() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let g = group_fixture(&client, &token).await;

    let (go, rust) = post_both_raw_stable(
        &client,
        &f.plain_token,
        "/api/v4/channels/group/search",
        &search(&g.one),
    )
    .await;
    assert_eq!(rust.0, 200, "there is no permission gate on this route");
    assert_eq!(rust.0, go.0);
    assert_eq!(
        String::from_utf8_lossy(&rust.1),
        String::from_utf8_lossy(&go.1),
    );
    assert!(
        !ids_of(&rust.1).contains(&g.with_two),
        "a non-member's search cannot see someone else's conversation"
    );
}

// ---------------------------------------------------------------------------
// The route that is not served
// ---------------------------------------------------------------------------

/// `managed_categories` is not a route on this stack: the feature flag that registers it is off,
/// so Go answers gorilla's own 404 rather than the handler's 501 licence error. This server
/// forwards, so the body is Go's by construction — the test is here to fail loudly if the flag is
/// ever turned on and the route starts existing.
#[tokio::test]
async fn the_managed_categories_route_is_a_404_on_both() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/teams/{}/channels/managed_categories", f.team_a);
    // Not `fetch_both_raw`: that helper asserts `x-mmrs-served-by: rust`, and this route is
    // deliberately **not** served here — the assertion below is that it is forwarded.
    let fetch = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .map(|v| v.to_str().unwrap_or_default().to_owned());
        let status = response.status().as_u16();
        (
            status,
            served_by,
            response.bytes().await.expect("body").to_vec(),
        )
    };
    let go = fetch(GO).await;
    let rust = fetch(RUST).await;

    assert_eq!(go.0, 404, "the feature flag is off, so the route is absent");
    assert_eq!(rust.0, go.0);
    assert_eq!(
        rust.1.as_deref(),
        Some("go"),
        "this route must still forward; registering it would answer 501 where Go answers 404"
    );
    let body: serde_json::Value = serde_json::from_slice(&rust.2).expect("JSON");
    assert_eq!(body["id"], serde_json::json!("api.context.404.app_error"));
    assert!(
        body["detailed_error"]
            .as_str()
            .unwrap_or_default()
            .contains("managed_categories"),
        "gorilla's 404 names the url it could not route"
    );
}

// ---------------------------------------------------------------------------
// The enterprise-only columns, which need rows planted by hand
// ---------------------------------------------------------------------------

/// The retention-policy and access-control filters, and the `policy_id` field.
///
/// # Why this plants rows instead of using the API
///
/// `RetentionPolicies` and `AccessControlPolicies` are written by **licensed** handlers this
/// stack does not serve, so both tables are empty and every one of these filters is a no-op
/// against the live database — a test that only used the API would pass with the predicates
/// deleted. Two rows, written directly, make four filters and one response field observable.
///
/// # The three things this is the only test of
///
/// - **`policy_id` reaches the wire.** It is selected only when the caller holds
///   `sysconsole_read_compliance_data_retention_policy`, and it is `null` in every other channel
///   response in this repository.
/// - **`exclude_policy_constrained` and the two access-control filters actually filter.**
/// - **`GetAllChannelsCount` drops the two access-control fields that `GetAllChannels` keeps**
///   (app/channel.go:2471-2479), so `?include_total_count=true` returns a filtered list beside an
///   unfiltered count. That asymmetry is invisible without an `AccessControlPolicies` row, and it
///   is the kind of thing a port "fixes" by accident.
#[tokio::test]
async fn the_retention_and_access_control_filters_need_planted_rows() {
    if !common::stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let Some(pool) = common::fixture_pool().await else {
        return;
    };

    // `alpha` gets a retention policy; the private fixture channel gets an access-control one.
    let policy_id = "mmrscastxretention00000000";
    let acp_channel = channel_id_by_name(
        &client,
        &token,
        &f.team_a,
        &format!("mmrs-parity-{STEM}ppp"),
    )
    .await;
    plant_retention_policy(&pool, policy_id, &f.alpha).await;
    plant_access_control_policy(&pool, &acp_channel).await;

    let result = async {
        // `policy_id` is on the wire, and only for the policy-constrained channel.
        let (go, rust) =
            post_both_raw_stable(&client, &token, "/api/v4/channels/search", &search(STEM)).await;
        assert_eq!(
            String::from_utf8_lossy(&rust.1),
            String::from_utf8_lossy(&go.1),
            "the search with a planted retention policy diverged"
        );
        let carried: Vec<serde_json::Value> = rows_of(&rust.1)
            .iter()
            .map(|c| c["policy_id"].clone())
            .collect();
        assert!(
            carried.contains(&serde_json::json!(policy_id)),
            "the constrained channel carries its policy id: {carried:?}"
        );
        assert!(
            carried.contains(&serde_json::json!(null)),
            "and every other channel carries null"
        );

        // The three filters, each against Go.
        for (label, path, body) in [
            (
                "exclude_policy_constrained",
                "/api/v4/channels/search",
                serde_json::json!({ "term": STEM, "exclude_policy_constrained": true }),
            ),
            (
                "access_control_policy_enforced",
                "/api/v4/channels/search",
                serde_json::json!({ "term": STEM, "access_control_policy_enforced": true }),
            ),
            (
                "exclude_access_control_policy_enforced",
                "/api/v4/channels/search",
                serde_json::json!({ "term": STEM, "exclude_access_control_policy_enforced": true }),
            ),
        ] {
            let raw = serde_json::to_vec(&body).expect("serialises");
            let (go, rust) = post_both_raw_stable(&client, &token, path, &raw).await;
            assert_eq!(
                String::from_utf8_lossy(&rust.1),
                String::from_utf8_lossy(&go.1),
                "{label} diverged"
            );
            let names = display_names(&rust.1);
            match label {
                "exclude_policy_constrained" => assert!(
                    !names.contains(&format!("a{STEM} alpha")),
                    "{label} must drop the constrained channel: {names:?}"
                ),
                "access_control_policy_enforced" => assert_eq!(
                    names,
                    vec![format!("m{STEM} private")],
                    "{label} keeps only the enforced channel"
                ),
                _ => assert!(
                    !names.contains(&format!("m{STEM} private")),
                    "{label} must drop the enforced channel: {names:?}"
                ),
            }
        }

        // `getAllChannels` has its own copies of the same three predicates, reached by query
        // string rather than by body, and none of them is exercised anywhere else.
        for (label, path, present) in [
            (
                "exclude_policy_constrained",
                "/api/v4/channels?per_page=200&exclude_policy_constrained=true",
                false,
            ),
            (
                "access_control_policy_enforced",
                "/api/v4/channels?per_page=200&access_control_policy_enforced=true",
                false,
            ),
        ] {
            let (go, rust) = fetch_both_stable(&client, &token, path).await;
            assert_lists_agree_modulo_ties(&go, &rust, path);
            let names = display_names(&rust);
            assert_eq!(
                names.contains(&format!("a{STEM} alpha")),
                present,
                "{label}: the retention-constrained channel"
            );
            if label == "access_control_policy_enforced" {
                assert_eq!(
                    names,
                    vec![format!("m{STEM} private")],
                    "{label} keeps only the enforced channel, server-wide"
                );
            }
        }

        // **The asymmetry, in one request.** `GetAllChannels` passes
        // `AccessControlPolicyEnforced` to the store; `GetAllChannelsCount` does not
        // (app/channel.go:2471-2479). So the list narrows to the single enforced channel while
        // `total_count` still counts the whole table — two numbers from one response, which is
        // what makes this churn-proof where comparing two separate requests was not.
        let path = "/api/v4/channels?per_page=200&include_total_count=true\
                    &access_control_policy_enforced=true";
        let (go, rust) = fetch_both_stable(&client, &token, path).await;
        assert_lists_agree_modulo_ties(&go, &rust, path);

        let counted = |body: &[u8]| {
            serde_json::from_slice::<serde_json::Value>(body).expect("JSON")["total_count"]
                .as_i64()
                .expect("a number")
        };
        assert_eq!(
            display_names(&rust),
            vec![format!("m{STEM} private")],
            "the list is narrowed to the one channel an access-control policy enforces"
        );
        assert!(
            counted(&rust) > 1,
            "…and the count is not, because GetAllChannelsCount never sees the flag: {}",
            counted(&rust)
        );
        assert_eq!(
            counted(&rust),
            counted(&go),
            "and Go's unfiltered count is the same unfiltered count"
        );
    }
    .await;

    remove_planted_policies(&pool, policy_id, &f.alpha, &acp_channel).await;
    result
}

/// The id of a channel by its `Name`, through Go.
async fn channel_id_by_name(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    name: &str,
) -> String {
    let response = client
        .get(format!("{GO}/api/v4/teams/{team_id}/channels/name/{name}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "the fixture channel is there"
    );
    let channel: serde_json::Value = response.json().await.expect("the channel decodes");
    channel["id"].as_str().expect("an id").to_owned()
}

async fn plant_retention_policy(pool: &sqlx::PgPool, policy_id: &str, channel_id: &str) {
    sqlx::query("DELETE FROM retentionpolicieschannels WHERE policyid = $1")
        .bind(policy_id)
        .execute(pool)
        .await
        .expect("the stale link goes");
    sqlx::query("DELETE FROM retentionpolicies WHERE id = $1")
        .bind(policy_id)
        .execute(pool)
        .await
        .expect("the stale policy goes");
    sqlx::query(
        "INSERT INTO retentionpolicies (id, displayname, postduration) \
         VALUES ($1, 'mmrs parity retention', 30)",
    )
    .bind(policy_id)
    .execute(pool)
    .await
    .expect("the policy is written");
    sqlx::query("INSERT INTO retentionpolicieschannels (policyid, channelid) VALUES ($1, $2)")
        .bind(policy_id)
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("the link is written");
}

async fn plant_access_control_policy(pool: &sqlx::PgPool, channel_id: &str) {
    sqlx::query("DELETE FROM accesscontrolpolicies WHERE id = $1")
        .bind(channel_id)
        .execute(pool)
        .await
        .expect("the stale policy goes");
    sqlx::query(
        "INSERT INTO accesscontrolpolicies \
             (id, name, type, active, createat, revision, version, data, props) \
         VALUES ($1, $2, 'channel', TRUE, 1, 1, 'v0.1', '{}'::jsonb, NULL)",
    )
    .bind(channel_id)
    .bind(format!("mmrs parity acp {channel_id}"))
    .execute(pool)
    .await
    .expect("the access-control policy is written");
}

async fn remove_planted_policies(
    pool: &sqlx::PgPool,
    policy_id: &str,
    retention_channel: &str,
    acp_channel: &str,
) {
    let _ =
        sqlx::query("DELETE FROM retentionpolicieschannels WHERE policyid = $1 OR channelid = $2")
            .bind(policy_id)
            .bind(retention_channel)
            .execute(pool)
            .await;
    let _ = sqlx::query("DELETE FROM retentionpolicies WHERE id = $1")
        .bind(policy_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM accesscontrolpolicies WHERE id = $1")
        .bind(acp_channel)
        .execute(pool)
        .await;
}
