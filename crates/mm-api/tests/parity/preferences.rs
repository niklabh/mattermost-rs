//! Cross-server parity for `PUT /api/v4/users/{me,{user_id}}/preferences` — **the first write**.
//!
//! A write has a property a read does not: the other server has to agree afterwards. So these
//! tests write through one server and read back through both.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_preferences
//! ```

use crate::common;

use common::{GO, RUST, client, go_minted_token, logged_in_user_id, stack_enabled};

const PATH: &str = "/api/v4/users/me/preferences";
const CATEGORY: &str = "display_settings";
// Each test uses its OWN preference name. They run in parallel, and two tests that flip the same
// key race each other — which is a test bug that reads exactly like a cross-server visibility
// failure, so it is worth removing rather than debugging twice.
const NAME: &str = "use_military_time";
const NAME_RUST_WRITE: &str = "mmrs_parity_rust_write";
const NAME_GO_WRITE: &str = "mmrs_parity_go_write";

fn body(name: &str, value: &str) -> serde_json::Value {
    serde_json::json!([{
        "user_id": logged_in_user_id(),
        "category": CATEGORY,
        "name": name,
        "value": value,
    }])
}

async fn put(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    value: &str,
) -> (u16, Vec<u8>) {
    let response = client
        .put(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body(name, value))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    (status, response.bytes().await.expect("body").to_vec())
}

/// Read the preference back through a given server.
async fn read_back(client: &reqwest::Client, base: &str, token: &str, name: &str) -> String {
    let response = client
        .get(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("readable");
    let prefs: Vec<serde_json::Value> = response.json().await.expect("decodes");
    prefs
        .into_iter()
        .find(|p| p["name"] == name)
        .map(|p| p["value"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_else(|| "<absent>".to_owned())
}

/// The success body is Go's `ReturnStatusOK` — `{"status":"OK"}`, no newline.
#[tokio::test]
async fn the_success_body_is_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let (go_status, go_body) = put(&client, GO, &token, NAME, "true").await;
    let (rs_status, rs_body) = put(&client, RUST, &token, NAME, "true").await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the OK body must match Go's ReturnStatusOK exactly"
    );
    assert_ne!(rs_body.last(), Some(&b'\n'), "w.Write appends no newline");
}

/// **The point of migrating a write.** A value written through Rust must be visible to the Go
/// server, which is what makes the two servers usable at once.
///
/// Measured rather than assumed: this is where [D-087]'s stale-on-write would show up if
/// preferences were cached the way users are. They are not — Go reflects the write immediately.
#[tokio::test]
async fn a_write_through_rust_is_visible_to_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    // Write a value the other direction from whatever is there, so a no-op cannot pass.
    let before = read_back(&client, GO, &token, NAME_RUST_WRITE).await;
    let target = if before == "true" { "false" } else { "true" };

    let (status, _) = put(&client, RUST, &token, NAME_RUST_WRITE, target).await;
    assert_eq!(status, 200);

    assert_eq!(
        read_back(&client, GO, &token, NAME_RUST_WRITE).await,
        target,
        "the Go server must see a write made through Rust"
    );
    assert_eq!(
        read_back(&client, RUST, &token, NAME_RUST_WRITE).await,
        target
    );
}

/// And the reverse direction, which is the one the Strangler Fig depends on for every route that
/// has *not* been migrated.
#[tokio::test]
async fn a_write_through_go_is_visible_to_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let before = read_back(&client, RUST, &token, NAME_GO_WRITE).await;
    let target = if before == "true" { "false" } else { "true" };

    let (status, _) = put(&client, GO, &token, NAME_GO_WRITE, target).await;
    assert_eq!(status, 200);

    assert_eq!(
        read_back(&client, RUST, &token, NAME_GO_WRITE).await,
        target,
        "we must see a write made through the Go server"
    );
}

/// A foreign `user_id` in the body is 403 on both servers, and the error bodies agree on
/// everything except the translated message.
#[tokio::test]
async fn a_foreign_user_id_is_rejected_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let foreign = serde_json::json!([{
        "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "category": CATEGORY,
        "name": NAME,
        "value": "true",
    }]);

    let send = async |base: &str| {
        let response = client
            .put(format!("{base}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&foreign)
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("decodes");
        (status, body)
    };

    let (go_status, go_body) = send(GO).await;
    let (rs_status, rs_body) = send(RUST).await;

    assert_eq!(go_status, 403);
    assert_eq!(
        rs_status, 403,
        "a foreign user id must be forbidden here too"
    );
    assert_eq!(
        rs_body["id"], go_body["id"],
        "the error id is what clients branch on"
    );
    assert_eq!(rs_body["status_code"], go_body["status_code"]);

    // `detailed_error` is wiped by Go unless developer mode is on, and we reproduce that
    // unconditionally — so it must be empty on both sides, not merely present on both.
    assert_eq!(rs_body["detailed_error"], "");
    assert_eq!(go_body["detailed_error"], "");

    // Same key set, so a field cannot appear on one side only.
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&rs_body), keys(&go_body));

    assert!(
        !rs_body["request_id"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "every error carries a request id, as Go's does"
    );

    // The one field that legitimately differs: Go translates the id through its i18n bundle and
    // we emit the id itself. See D-092.
    assert_eq!(rs_body["message"], rs_body["id"]);
    assert_ne!(go_body["message"], go_body["id"]);
}

/// Both of Go's batch bounds, checked against Go itself.
#[tokio::test]
async fn the_batch_bounds_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    let send = async |base: &str, payload: serde_json::Value| {
        client
            .put(format!("{base}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&payload)
            .send()
            .await
            .expect("reachable")
            .status()
            .as_u16()
    };

    // Empty is an error, not a successful no-op.
    let empty = serde_json::json!([]);
    assert_eq!(send(RUST, empty.clone()).await, send(GO, empty).await);

    // 101 entries is over `maxUpdatePreferences`.
    let too_many: Vec<serde_json::Value> = (0..101)
        .map(|i| {
            serde_json::json!({
                "user_id": logged_in_user_id(), "category": CATEGORY,
                "name": format!("bulk_{i}"), "value": "1",
            })
        })
        .collect();
    let too_many = serde_json::Value::Array(too_many);
    assert_eq!(
        send(RUST, too_many.clone()).await,
        send(GO, too_many).await,
        "the 100-entry cap must be enforced on both sides"
    );
}

/// The route is served here, not forwarded.
#[tokio::test]
async fn ordinary_categories_are_served_by_rust() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let response = client
        .put(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body(NAME, "true"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust")
    );
}

/// A path with a migrated method must still forward the others.
///
/// axum matches the path before the method, so registering `PUT` here once made `GET` return 405
/// from our own router instead of reaching the proxy — breaking a route that had been working.
/// `GET` is migrated now, so the probe is `POST .../preferences/delete`: its path matches the
/// migrated `GET /users/{user_id}/preferences/{category}` route with `delete` as the category,
/// and only the method fallback keeps the `POST` reaching Go. Same regression, next method over.
#[tokio::test]
async fn an_unmigrated_method_on_a_migrated_path_still_reaches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    // **This used to probe `POST /preferences/delete`, which is now migrated.** The point of the
    // test is the method fallback, not that particular route, so it moved to a method Go does not
    // register on a path this server does serve: `DELETE /users/me/preferences`. Go answers its
    // 404 page; what matters is that the request reached Go at all rather than meeting a 405 from
    // axum's method router.
    let response = client
        .delete(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");

    assert_eq!(
        response.status(),
        404,
        "DELETE is not a method Go registers here, so Go's own 404 is the answer — a 405 would \
         mean the method fallback is gone"
    );
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "and it must be Go that answered"
    );
}

// ---------------------------------------------------------------------------------------------
// `PUT /users/{user_id}/preferences` — the spelling the webapp sends (2026-09-19)
// ---------------------------------------------------------------------------------------------

/// `PUT` a raw batch to `/users/{user_id}/preferences` on `base`: status, body, served-by-Rust.
async fn put_for(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    batch: &serde_json::Value,
) -> (u16, Vec<u8>, bool) {
    let response = client
        .put(format!("{base}/api/v4/users/{user_id}/preferences"))
        .header("Authorization", format!("Bearer {token}"))
        .json(batch)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, response.bytes().await.expect("body").to_vec(), rust)
}

fn one(user_id: &str, category: &str, name: &str, value: &str) -> serde_json::Value {
    serde_json::json!([{
        "user_id": user_id, "category": category, "name": name, "value": value,
    }])
}

/// The browser's own spelling is served here, and answers as Go does — including for another
/// user's id, the 403 naming `edit_other_users`, which the `me` literal cannot reach.
#[tokio::test]
async fn the_explicit_id_spelling_is_served_and_matches() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "prefidteam").await;
    let user = common::create_plain_user(&client, &admin, &team, "prefid").await;

    let batch = one(&user.id, CATEGORY, "mmrs_parity_explicit_id", "true");
    let (go_status, go_body, _) = put_for(&client, GO, &user.token, &user.id, &batch).await;
    let (rs_status, rs_body, served) = put_for(&client, RUST, &user.token, &user.id, &batch).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!((rs_status, &rs_body), (go_status, &go_body));
    assert!(served, "PUT /users/{{user_id}}/preferences is served here");

    // Somebody else's id: the permission check precedes the body.
    let foreign = logged_in_user_id();
    let batch = one(foreign, CATEGORY, "mmrs_parity_explicit_id", "true");
    let (go_status, go_body, _) = put_for(&client, GO, &user.token, foreign, &batch).await;
    let (rs_status, rs_body, served) = put_for(&client, RUST, &user.token, foreign, &batch).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    assert!(served);
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "foreign");
    assert_eq!(body["id"], "api.context.permissions.app_error");

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// `direct_channel_show` and `group_channel_show` used to be forwarded for a sidebar sync that
/// Go's store never runs for them ([D-091]). Now served, and the row is the same either way.
#[tokio::test]
async fn dm_visibility_is_served_here() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "prefdmteam").await;
    let user = common::create_plain_user(&client, &admin, &team, "prefdm").await;

    for category in ["direct_channel_show", "group_channel_show"] {
        let batch = one(&user.id, category, logged_in_user_id(), "false");
        let (go_status, go_body, _) = put_for(&client, GO, &user.token, &user.id, &batch).await;
        let (rs_status, rs_body, served) =
            put_for(&client, RUST, &user.token, &user.id, &batch).await;
        assert_eq!(
            go_status,
            200,
            "{category}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!((rs_status, &rs_body), (go_status, &go_body), "{category}");
        assert!(served, "{category} is no longer forwarded");
    }

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// Each user's Favorites, per team, as channel ids in sort order — read through **Go**, so the
/// comparison is about what each server wrote, not about how each reads it. `names` turns ids
/// that differ per user (the DM, the teams) into labels.
async fn favourites(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    teams: &[(&str, &str)],
    names: &[(&str, &str)],
) -> Vec<String> {
    let mut out = Vec::new();
    for (team_label, team_id) in teams {
        let categories: serde_json::Value = client
            .get(format!(
                "{GO}/api/v4/users/{user_id}/teams/{team_id}/channels/categories"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("categories decode");
        let favourites = categories["categories"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|c| c["type"] == "favorites")
            .expect("a Favorites category");
        let ids: Vec<String> = favourites["channel_ids"]
            .as_array()
            .expect("channel ids")
            .iter()
            .map(|id| {
                let id = id.as_str().expect("an id");
                names
                    .iter()
                    .find(|(real, _)| *real == id)
                    .map_or_else(|| id.to_owned(), |(_, label)| (*label).to_owned())
            })
            .collect();
        out.push(format!("{team_label}: {}", ids.join(",")));
    }
    out
}

/// **`UpdateSidebarChannelsByPreferences`, byte for byte through the same sequence.** The same
/// seven writes go through Go for one user and through Rust for another; the Favorites of both,
/// in both teams, must end up identical.
///
/// The sequence is built to separate the decisions a port could get wrong:
/// - two team channels favourited in turn — the second must sort **first** (`MIN - 10`);
/// - a repeat of the first — already present, so no second row;
/// - a DM — no team, so it joins the Favorites of **both** teams, where a team channel joins
///   only its own;
/// - `"false"` removes, while `"FALSE"` and `""` favourite: only the exact string unfavourites;
/// - a channel id that does not exist — the preference is saved, then the sync fails, a 500.
#[tokio::test]
async fn favourites_follow_favorite_channel_preferences_as_on_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team_a = common::create_team(&client, &admin, "preffava").await;
    let team_b = common::create_team(&client, &admin, "preffavb").await;
    let one_ch = common::create_channel(&client, &admin, &team_a, "preffav-one").await;
    let two_ch = common::create_channel(&client, &admin, &team_a, "preffav-two").await;
    let three_ch = common::create_channel(&client, &admin, &team_a, "preffav-three").await;
    let four_ch = common::create_channel(&client, &admin, &team_a, "preffav-four").await;
    let missing = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    let mut results = Vec::new();
    for (base, tag) in [(GO, "preffavgo"), (RUST, "preffavrs")] {
        let user = common::create_plain_user(&client, &admin, &team_a, tag).await;
        let joined = client
            .post(format!("{GO}/api/v4/teams/{team_b}/members"))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&serde_json::json!({ "team_id": team_b, "user_id": user.id }))
            .send()
            .await
            .expect("Go answers");
        assert!(joined.status().is_success(), "joining the second team");
        for channel in [&one_ch, &two_ch, &three_ch, &four_ch] {
            common::add_user_to_channel(&client, &admin, channel, &user.id).await;
        }
        let dm = common::create_direct_channel(&client, &user.token, &user.id, logged_in_user_id())
            .await;

        let mut statuses = Vec::new();
        for (channel, value) in [
            (one_ch.as_str(), "true"),
            (two_ch.as_str(), "true"),
            (one_ch.as_str(), "true"),
            (dm.as_str(), "true"),
            (two_ch.as_str(), "false"),
            (three_ch.as_str(), "FALSE"),
            (four_ch.as_str(), ""),
        ] {
            let batch = one(&user.id, "favorite_channel", channel, value);
            let (status, body, served) =
                put_for(&client, base, &user.token, &user.id, &batch).await;
            assert_eq!(
                status,
                200,
                "{base} {value}: {}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(served, base == RUST, "{base}: who answered");
            statuses.push(status);
        }

        // The sync's failure: the preference is written first, so it survives the 500.
        let batch = one(&user.id, "favorite_channel", missing, "true");
        let (status, body, _) = put_for(&client, base, &user.token, &user.id, &batch).await;
        let error: serde_json::Value = serde_json::from_slice(&body).expect("an error body");
        let saved = client
            .get(format!(
                "{GO}/api/v4/users/{}/preferences/favorite_channel/name/{missing}",
                user.id
            ))
            .header("Authorization", format!("Bearer {}", user.token))
            .send()
            .await
            .expect("Go answers")
            .status()
            .as_u16();

        let names = [
            (one_ch.as_str(), "one"),
            (two_ch.as_str(), "two"),
            (three_ch.as_str(), "three"),
            (four_ch.as_str(), "four"),
            (dm.as_str(), "dm"),
        ];
        let favs = favourites(
            &client,
            &user.token,
            &user.id,
            &[("a", &team_a), ("b", &team_b)],
            &names,
        )
        .await;
        results.push((statuses, status, error["id"].clone(), saved, favs));
        common::delete_plain_user(&client, &admin, &user.id).await;
    }

    let (go, rust) = (&results[0], &results[1]);
    assert_eq!(
        go.4,
        vec!["a: four,three,dm,one", "b: dm"],
        "the fixture must discriminate: Go's own answer"
    );
    assert_eq!(
        rust, go,
        "the same writes, the same sidebar and the same failure"
    );
    assert_eq!(go.1, 500);
    assert_eq!(
        go.2,
        "api.preference.update_preferences.update_sidebar.app_error"
    );
    assert_eq!(go.3, 200, "the preference outlived the failed sync");
}

/// `flagged_post` is served now, with Go's checks in Go's order: the post must exist and not be
/// deleted (400 naming `preference.name`), and its channel must be readable (403 naming
/// `read_channel_content`). A readable post saves.
#[tokio::test]
async fn flagged_post_checks_the_post_and_its_channel_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "prefflagteam").await;
    let user = common::create_plain_user(&client, &admin, &team, "prefflag").await;
    let open = common::create_channel(&client, &admin, &team, "prefflag-open").await;
    let private = common::create_channel_typed(&client, &admin, &team, "prefflag-priv", "P").await;
    common::add_user_to_channel(&client, &admin, &open, &user.id).await;
    let readable = common::post_message(&client, &admin, &open, "flag me", None).await;
    let hidden = common::post_message(&client, &admin, &private, "not yours", None).await;
    let deleted = common::post_message(&client, &admin, &open, "gone", None).await;
    common::delete_post(&client, &admin, &deleted).await;

    for (label, post, status, id) in [
        (
            "missing",
            "zzzzzzzzzzzzzzzzzzzzzzzzzz",
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "deleted",
            deleted.as_str(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "unreadable",
            hidden.as_str(),
            403,
            "api.context.permissions.app_error",
        ),
    ] {
        // A good entry first: the refusal must still write nothing, since the checks run before
        // the save.
        let batch = serde_json::json!([
            { "user_id": user.id, "category": CATEGORY, "name": "mmrs_flag_probe", "value": label },
            { "user_id": user.id, "category": "flagged_post", "name": post, "value": "true" },
        ]);
        let (go_status, go_body, _) = put_for(&client, GO, &user.token, &user.id, &batch).await;
        let (rs_status, rs_body, served) =
            put_for(&client, RUST, &user.token, &user.id, &batch).await;
        assert_eq!(
            go_status,
            status,
            "{label}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, status, "{label}");
        assert!(served, "{label}: served here");
        let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        assert_eq!(body["id"], id, "{label}");
    }
    let probe = client
        .get(format!(
            "{GO}/api/v4/users/{}/preferences/{CATEGORY}/name/mmrs_flag_probe",
            user.id
        ))
        .header("Authorization", format!("Bearer {}", user.token))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        probe.status(),
        400,
        "no refused batch wrote its first entry"
    );

    let batch = one(&user.id, "flagged_post", &readable, "true");
    let (go_status, go_body, _) = put_for(&client, GO, &user.token, &user.id, &batch).await;
    let (rs_status, rs_body, served) = put_for(&client, RUST, &user.token, &user.id, &batch).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!((rs_status, &rs_body), (go_status, &go_body));
    assert!(served);

    common::delete_plain_user(&client, &admin, &user.id).await;
    common::delete_channel(&client, &admin, &private).await;
    common::delete_channel(&client, &admin, &open).await;
}
