//! Cross-server parity for the three drafts writes and `deletePreferences`.
//!
//! Four routes that share a shape with the reactions group and differ from it in three ways worth
//! testing directly: a success that is **`201` with a body of `null`**, a delete that is **`200`
//! for a row that never existed**, and an event that carries an `omit_connection_id`.
//!
//! Every assertion about a write this server made reads back **through this server** — Go's caches
//! do not see our writes ([D-190]).
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity draft_and_preference
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, client, go_minted_token,
    logged_in_user_id, post_message, stack_enabled,
};

async fn upsert_draft(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    body: &serde_json::Value,
    connection_id: Option<&str>,
) -> (u16, String) {
    let mut request = http
        .post(format!("{base}/api/v4/drafts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body);
    if let Some(connection_id) = connection_id {
        request = request.header("Connection-Id", connection_id);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), "/api/v4/drafts");
    }
    (status, response.text().await.expect("a body"))
}

async fn delete_draft(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    thread_id: Option<&str>,
) -> (u16, String) {
    let path = match thread_id {
        Some(thread_id) => format!(
            "/api/v4/users/{}/channels/{channel_id}/drafts/{thread_id}",
            logged_in_user_id()
        ),
        None => format!(
            "/api/v4/users/{}/channels/{channel_id}/drafts",
            logged_in_user_id()
        ),
    };
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), &path);
    }
    (status, response.text().await.expect("a body"))
}

/// The caller's drafts in a team, read through one server.
async fn drafts_in(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    team_id: &str,
) -> Vec<serde_json::Value> {
    let path = format!(
        "/api/v4/users/{}/teams/{team_id}/drafts",
        logged_in_user_id()
    );
    let response = http
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("drafts are readable");
    let value: serde_json::Value = response.json().await.expect("a JSON body");
    value.as_array().cloned().unwrap_or_default()
}

fn normalise_draft(draft: &serde_json::Value) -> serde_json::Value {
    let mut out = draft.clone();
    let object = out.as_object_mut().expect("a draft object");
    for key in ["create_at", "update_at"] {
        let value = object.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        object.insert(
            key.to_owned(),
            serde_json::json!(if value > 0 { "nonzero" } else { "zero" }),
        );
    }
    object.insert("channel_id".to_owned(), serde_json::json!("<channel>"));
    out
}

#[tokio::test]
async fn a_draft_round_trips_and_both_servers_answer_the_same_shape() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let other = common::create_channel_typed(&http, &token, &team, "draftpar", "O").await;

    // A channel each, so neither server's upsert lands on the other's row.
    let go_body = serde_json::json!({"channel_id": channel, "message": "mmrs draft go"});
    let rust_body = serde_json::json!({"channel_id": other, "message": "mmrs draft rust"});

    let (go_status, go_raw) = upsert_draft(&http, GO, &token, &go_body, None).await;
    let (rust_status, rust_raw) = upsert_draft(&http, RUST, &token, &rust_body, None).await;

    // **201, not 200.** The handler writes `http.StatusCreated` before encoding.
    assert_eq!(go_status, 201, "Go answers Created: {go_raw}");
    assert_eq!(rust_status, go_status, "the upsert status differs");
    assert!(
        go_raw.ends_with('\n'),
        "Go's upsert body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(
        rust_raw.ends_with('\n'),
        go_raw.ends_with('\n'),
        "the upsert body's framing differs"
    );

    let go_draft: serde_json::Value = serde_json::from_str(&go_raw).expect("a draft");
    let rust_draft: serde_json::Value = serde_json::from_str(&rust_raw).expect("a draft");
    let mut go_normalised = normalise_draft(&go_draft);
    let mut rust_normalised = normalise_draft(&rust_draft);
    go_normalised["message"] = serde_json::json!("<message>");
    rust_normalised["message"] = serde_json::json!("<message>");
    assert_eq!(
        go_normalised, rust_normalised,
        "the saved draft differs:\n go: {go_draft}\nrust: {rust_draft}"
    );

    // Every draft carries a `metadata` object even with no files — `prepareDraftWithFileInfos`
    // assigns one on the success path unconditionally.
    assert!(
        rust_draft["metadata"].is_object(),
        "a draft with no files still carries metadata: {rust_draft}"
    );
    assert_eq!(
        rust_draft["delete_at"], 0,
        "delete_at is overwritten from the request"
    );
    assert_eq!(
        rust_draft["user_id"],
        logged_in_user_id(),
        "user_id comes from the session, not the body"
    );

    // Read back through the writing server.
    let ours = drafts_in(&http, RUST, &token, &team).await;
    assert!(
        ours.iter()
            .any(|draft| draft["channel_id"] == other.as_str()),
        "our draft is not in our own listing: {ours:?}"
    );

    // **The answer and the row disagree about `create_at`, exactly as they do for a reaction.**
    // `PreSave` mints a fresh `CreateAt` because the incoming draft has none, and the response is
    // marshalled from that struct — but the conflict clause updates seven columns and `CreateAt`
    // is not among them. So the edit *answers* with a new creation time while the stored row
    // keeps its original. Measured; the first version of this test asserted the answer kept it.
    let edited = serde_json::json!({"channel_id": other, "message": "mmrs draft rust edited"});
    let (status, raw) = upsert_draft(&http, RUST, &token, &edited, None).await;
    assert_eq!(status, 201);
    let edited_draft: serde_json::Value = serde_json::from_str(&raw).expect("a draft");
    assert!(
        edited_draft["create_at"].as_i64() > rust_draft["create_at"].as_i64(),
        "the answer carries the fresh PreSave timestamp"
    );
    assert!(
        edited_draft["update_at"].as_i64() >= rust_draft["update_at"].as_i64(),
        "an edit must move update_at forward"
    );

    let stored = drafts_in(&http, RUST, &token, &team).await;
    let stored = stored
        .iter()
        .find(|draft| draft["channel_id"] == other.as_str())
        .expect("our draft is stored");
    assert_eq!(
        stored["create_at"], rust_draft["create_at"],
        "the stored row keeps its original create_at through the edit"
    );
    assert_eq!(
        stored["message"], "mmrs draft rust edited",
        "and the edit really landed"
    );

    delete_draft(&http, GO, &token, &channel, None).await;
    delete_draft(&http, RUST, &token, &other, None).await;
    common::delete_channel(&http, &token, &other).await;
}

#[tokio::test]
async fn an_empty_message_deletes_the_draft_and_answers_201_null() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    // A channel each, so the two servers' upserts never share a row.
    let go_channel = common::create_channel_typed(&http, &token, &team, "draftemptyg", "O").await;
    let rust_channel = common::create_channel_typed(&http, &token, &team, "draftemptyr", "O").await;

    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        let saved = serde_json::json!({"channel_id": channel, "message": "mmrs draft to empty"});
        let (status, _) = upsert_draft(&http, base, &token, &saved, None).await;
        assert_eq!(status, 201, "{base} saved the draft first");

        // **The interesting one.** Go returns `(nil, nil)` after deleting and the handler encodes
        // the nil pointer, so the answer is `201 Created` with a body of `null` — for a request
        // that destroyed a row. Three things a port gets wrong independently: the status, the
        // body, and the fact that it deleted at all.
        let emptied = serde_json::json!({"channel_id": channel, "message": ""});
        let (status, raw) = upsert_draft(&http, base, &token, &emptied, None).await;
        assert_eq!(status, 201, "{base} answers Created for an empty message");
        assert_eq!(raw, "null\n", "{base} should answer the four bytes null");
    }

    // And the row really is gone, read back through the server that removed it.
    assert!(
        !drafts_in(&http, RUST, &token, &team)
            .await
            .iter()
            .any(|draft| draft["channel_id"] == rust_channel.as_str()),
        "the empty-message upsert did not delete our draft"
    );

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn an_archived_channel_refuses_a_draft() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "draftarch", "O").await;
    common::delete_channel(&http, &token, &channel).await;

    // The channel is fetched *without* a `DeleteAt` filter — Go's `Get(id, true)` is
    // `allowFromCache`, not include-deleted — which is what makes this branch reachable at all.
    // A port that used a deleted-filtering read would answer the channel gate's 400 with the
    // wrong id, and one that skipped the check would write a draft into an archived channel.
    let body = serde_json::json!({"channel_id": channel, "message": "mmrs draft to archived"});
    for base in [GO, RUST] {
        let (status, raw) = upsert_draft(&http, base, &token, &body, None).await;
        assert_eq!(
            status, 400,
            "{base} should refuse a draft to an archived channel: {raw}"
        );
        let body: serde_json::Value = serde_json::from_str(&raw).expect("an AppError");
        assert_eq!(
            body["id"], "api.draft.create_draft.can_not_draft_to_deleted.error",
            "{base} refused with the wrong id"
        );
    }
}

#[tokio::test]
async fn deleting_a_draft_that_is_not_there_is_a_200() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "draftmiss", "O").await;

    // `GetDraft`'s 404 is caught and turned into `ReturnStatusOK` — "if the draft doesn't exist in
    // the server, we don't need to delete". This route is idempotent where its neighbours are not.
    for base in [GO, RUST] {
        let (status, body) = delete_draft(&http, base, &token, &channel, None).await;
        assert_eq!(
            status, 200,
            "{base} should answer OK for a draft that never existed: {body}"
        );
        assert_eq!(body, r#"{"status":"OK"}"#);
    }

    // Save one, delete it twice: the second delete is the same 200.
    upsert_draft(
        &http,
        RUST,
        &token,
        &serde_json::json!({"channel_id": channel, "message": "mmrs draft twice"}),
        None,
    )
    .await;
    for attempt in 1..=2 {
        let (status, body) = delete_draft(&http, RUST, &token, &channel, None).await;
        assert_eq!(status, 200, "delete attempt {attempt}: {body}");
    }

    common::delete_channel(&http, &token, &channel).await;
}

#[tokio::test]
async fn the_thread_form_names_a_different_draft_from_the_channel_form() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "draftthread", "O").await;
    let root = post_message(&http, &token, &channel, "mmrs draft thread root", None).await;

    // A channel draft and a thread draft coexist: they differ only in `RootId`, and the
    // shallower delete route leaves it empty. A port that ignored `thread_id` would delete the
    // wrong one.
    upsert_draft(
        &http,
        RUST,
        &token,
        &serde_json::json!({"channel_id": channel, "message": "mmrs channel draft"}),
        None,
    )
    .await;
    upsert_draft(
        &http,
        RUST,
        &token,
        &serde_json::json!({"channel_id": channel, "root_id": root, "message": "mmrs thread draft"}),
        None,
    )
    .await;

    let both = drafts_in(&http, RUST, &token, &team).await;
    let in_channel: Vec<_> = both
        .iter()
        .filter(|draft| draft["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(in_channel.len(), 2, "two drafts coexist: {in_channel:?}");

    // Delete only the thread one.
    let (status, _) = delete_draft(&http, RUST, &token, &channel, Some(&root)).await;
    assert_eq!(status, 200);
    let after = drafts_in(&http, RUST, &token, &team).await;
    let remaining: Vec<_> = after
        .iter()
        .filter(|draft| draft["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(remaining.len(), 1, "the channel draft survives");
    assert_eq!(
        remaining[0]["root_id"], "",
        "the *thread* draft is the one that went"
    );

    delete_draft(&http, RUST, &token, &channel, None).await;
    common::delete_channel(&http, &token, &channel).await;
}

#[tokio::test]
async fn the_draft_events_omit_the_originating_connection() {
    if !stack_enabled() {
        return;
    }
    // Serialised against every other broadcast-counting test: this one asserts a *count* of
    // frames on the shared admin's stream, which is only true while nothing else writes to
    // that user. See `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = common::create_channel_typed(&http, &token, &team, "draftevent", "O").await;

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    let body = serde_json::json!({"channel_id": channel, "message": "mmrs draft event"});
    upsert_draft(&http, GO, &token, &body, None).await;
    upsert_draft(&http, RUST, &token, &body, None).await;

    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = draft_events(&go_socket, "draft_created", &channel);
    let rust_events = draft_events(&rust_socket, "draft_created", &channel);
    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_socket.raw);
    assert_eq!(
        rust_events.len(),
        1,
        "we published one: {:?}",
        rust_socket.raw
    );

    // Addressed to the **channel and the user both**, which the reaction event is not — the
    // reaction's `user_id` is empty. The hub reads `user_id` before `channel_id`, so this event
    // reaches only its author's other sessions.
    assert_eq!(
        go_events[0]["broadcast"], rust_events[0]["broadcast"],
        "the draft event's addressing differs"
    );
    assert_eq!(rust_events[0]["broadcast"]["channel_id"], channel);
    assert_eq!(rust_events[0]["broadcast"]["user_id"], logged_in_user_id());
    assert_eq!(rust_events[0]["broadcast"]["omit_connection_id"], "");

    // With an `X-Connection-Id`, the event carries it as `omit_connection_id` — which is how a
    // client stops its own tab being told about its own save.
    let connection = "mmrsconnectionid1234567890";
    upsert_draft(&http, GO, &token, &body, Some(connection)).await;
    upsert_draft(&http, RUST, &token, &body, Some(connection)).await;
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_omitting = draft_events(&go_socket, "draft_created", &channel);
    let rust_omitting = draft_events(&rust_socket, "draft_created", &channel);
    assert_eq!(
        go_omitting.last().expect("Go published again")["broadcast"]["omit_connection_id"],
        connection,
        "Go carries the header into the broadcast"
    );
    assert_eq!(
        rust_omitting.last().expect("we published again")["broadcast"]["omit_connection_id"],
        connection,
        "we must carry the header into the broadcast"
    );

    delete_draft(&http, RUST, &token, &channel, None).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;
    assert_eq!(
        draft_events(&rust_socket, "draft_deleted", &channel).len(),
        1,
        "the delete publishes draft_deleted: {:?}",
        rust_socket.raw
    );

    common::delete_channel(&http, &token, &channel).await;
}

/// Draft events on `probe` concerning `channel_id`. The draft is a JSON **string** inside `data`,
/// the same double encoding the reaction events use.
fn draft_events(probe: &SocketProbe, event_type: &str, channel_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named(event_type)
        .into_iter()
        .filter(|frame| {
            frame["data"]["draft"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|draft| draft["channel_id"].as_str().map(|id| id == channel_id))
                .unwrap_or(false)
        })
        .collect()
}

#[tokio::test]
async fn deleting_preferences_agrees_and_removes_only_what_was_named() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id();

    let put = |name: &'static str, value: &'static str| {
        let http = http.clone();
        let token = token.clone();
        async move {
            let response = http
                .put(format!("{RUST}/api/v4/users/me/preferences"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!([{
                    "user_id": me, "category": "mmrs_parity", "name": name, "value": value,
                }]))
                .send()
                .await
                .expect("the preference saves");
            assert_eq!(response.status().as_u16(), 200);
        }
    };

    put("keep", "kept").await;
    put("drop", "dropped").await;

    let path = format!("/api/v4/users/{me}/preferences/delete");
    let body = serde_json::json!([{
        "user_id": me, "category": "mmrs_parity", "name": "drop", "value": "dropped",
    }]);
    let response = http
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("we answer");
    assert_eq!(response.status().as_u16(), 200);
    common::assert_served_by_rust(response.headers(), &path);
    assert_eq!(response.text().await.expect("a body"), r#"{"status":"OK"}"#);

    let remaining = category(&http, RUST, &token, me, "mmrs_parity").await;
    let names: Vec<&str> = remaining
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["keep"], "only the named preference should go");

    // A batch naming a different user is a **403**, and nothing is deleted — the validation loop
    // runs over the whole batch before the delete loop starts.
    let mixed = serde_json::json!([
        {"user_id": me, "category": "mmrs_parity", "name": "keep", "value": "kept"},
        {"user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "category": "mmrs_parity", "name": "keep",
         "value": "kept"},
    ]);
    for base in [GO, RUST] {
        let response = http
            .post(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&mixed)
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status().as_u16(),
            403,
            "{base} should refuse a batch naming another user"
        );
        let body: serde_json::Value = response.json().await.expect("an AppError");
        assert_eq!(
            body["id"], "api.preference.delete_preferences.delete.app_error",
            "{base} refused with the wrong id"
        );
    }
    assert_eq!(
        category(&http, RUST, &token, me, "mmrs_parity").await.len(),
        1,
        "the refused batch must not have deleted its first entry"
    );

    // The bounds: an empty batch is an error, not a no-op.
    for base in [GO, RUST] {
        let response = http
            .post(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!([]))
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status().as_u16(),
            400,
            "{base} should refuse an empty batch"
        );
        let body: serde_json::Value = response.json().await.expect("an AppError");
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }

    // Clean up.
    http.post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": me, "category": "mmrs_parity", "name": "keep", "value": "kept",
        }]))
        .send()
        .await
        .expect("cleanup");
}

async fn category(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    category: &str,
) -> Vec<serde_json::Value> {
    let response = http
        .get(format!(
            "{base}/api/v4/users/{user_id}/preferences/{category}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("preferences are readable");
    let value: serde_json::Value = response.json().await.expect("a JSON body");
    value.as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn deleting_preferences_publishes_both_events() {
    if !stack_enabled() {
        return;
    }
    // Serialised against every other broadcast-counting test: this one asserts a *count* of
    // frames on the shared admin's stream, which is only true while nothing else writes to
    // that user. See `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id();

    http.put(format!("{RUST}/api/v4/users/me/preferences"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": me, "category": "mmrs_parity_ev", "name": "ev", "value": "1",
        }]))
        .send()
        .await
        .expect("the preference saves");

    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    http.post(format!("{RUST}/api/v4/users/{me}/preferences/delete"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": me, "category": "mmrs_parity_ev", "name": "ev", "value": "1",
        }]))
        .send()
        .await
        .expect("we answer");

    rust_socket.collect_for(Duration::from_millis(900)).await;

    // **Two events, and the first carries an empty data map.** Go's own comment says
    // "TODO this needs to be updated to include information on which categories changed". A port
    // that published only `preferences_deleted` would leave the webapp's sidebar stale after
    // unfavouriting a channel.
    let sidebar = rust_socket.events_named("sidebar_category_updated");
    let deleted = rust_socket.events_named("preferences_deleted");
    assert_eq!(
        sidebar.len(),
        1,
        "sidebar_category_updated is published too: {:?}",
        rust_socket.raw
    );
    assert_eq!(
        sidebar[0]["data"],
        serde_json::json!({}),
        "its data map is empty, as Go's is"
    );
    assert_eq!(
        deleted.len(),
        1,
        "preferences_deleted: {:?}",
        rust_socket.raw
    );

    // The batch is a JSON string, like the draft and the reaction.
    let carried: serde_json::Value = deleted[0]["data"]["preferences"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .expect("the preferences are a JSON string");
    assert_eq!(
        carried[0]["name"], "ev",
        "the event carries the batch it deleted"
    );
}
