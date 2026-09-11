//! Cross-server parity for `GET /api/v4/bots` and `GET /api/v4/bots/{bot_user_id}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity bots
//! ```
//!
//! # This suite could not exist a session ago
//!
//! The published `11.11.0-rc1` image answered `getBots` with a `system_owned` field that does not
//! occur anywhere in the pinned source, so matching the reference and matching the forward target
//! were different things and [D-167] recorded the route as blocked. The target is now built from
//! the pinned SHA and the field is gone. If it ever comes back, this suite is where it shows.
//!
//! # The property worth the file
//!
//! **A refused read and a missing bot are the same 404**, byte for byte — Go's own comment says
//! "pretend like the bot doesn't exist at all, to avoid revealing that the user is a bot". A test
//! that only checked the status would pass on a 403 too, so the bodies are compared.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, client, create_plain_user, create_team, fetch_both_raw,
    fetch_both_stable, go_minted_token, stack_enabled,
};

const LIST: &str = "/api/v4/bots";
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

async fn assert_served_by_rust(client: &reqwest::Client, token: &str, path: &str) {
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{path} was forwarded, so this suite would be comparing Go with Go"
    );
}

/// The list, byte for byte, and the field the version skew used to add.
#[tokio::test]
async fn the_bot_list_matches_byte_for_byte_and_carries_no_system_owned() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let (go, rs) = fetch_both_stable(&client, &token, LIST).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{LIST}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a newline"
    );
    assert_served_by_rust(&client, &token, LIST).await;

    let bots: serde_json::Value = serde_json::from_slice(&rs).expect("an array");
    let bots = bots.as_array().expect("an array");
    assert!(
        !bots.is_empty(),
        "this deployment ships bots, so the comparison is not vacuous"
    );

    for bot in bots {
        let object = bot.as_object().expect("an object");
        assert!(
            !object.contains_key("system_owned"),
            "the pinned source has no such field — is the forward target the published image \
             again? See scripts/go-server.sh: {bot}"
        );
        // `model.Bot` has six required keys; `display_name`, `description` and
        // `last_icon_update` carry `omitempty` and are absent when zero.
        for key in [
            "user_id",
            "username",
            "owner_id",
            "create_at",
            "update_at",
            "delete_at",
        ] {
            assert!(object.contains_key(key), "{key} is on the wire: {bot}");
        }
    }

    // The `omitempty` trio is real on this data rather than assumed: the system bot has no
    // description, so its key is absent while the calls bot's is present.
    let has_description = bots.iter().any(|b| b.get("description").is_some());
    let omits_description = bots.iter().any(|b| b.get("description").is_none());
    assert!(
        has_description && omits_description,
        "both sides of `description,omitempty` must appear or this proves nothing: {bots:?}"
    );
}

/// The single-bot route, on an id learned from the list, and its `Etag`/`If-None-Match` pair.
#[tokio::test]
async fn a_single_bot_matches_and_revalidates_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let (_, list) = fetch_both_stable(&client, &token, LIST).await;
    let bots: serde_json::Value = serde_json::from_slice(&list).expect("an array");
    let id = bots[0]["user_id"].as_str().expect("an id").to_owned();
    let path = format!("/api/v4/bots/{id}");

    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert!(rs.ends_with(b"\n"), "the encoder's newline");

    // **No `ETag` on the 200.** `HandleEtag` sets the header only on the 304, which is the
    // counter-intuitive half and the one a port gets wrong.
    let one = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert!(
        one.headers().get("etag").is_none(),
        "Go writes no ETag on a 200 from this route"
    );

    // The etag is `model.Etag(UserId, UpdateAt)`; a client that has it gets a 304 with the header.
    let bot: serde_json::Value = serde_json::from_slice(&rs).expect("decodes");
    let etag = format!(
        "{}.{}.{}",
        mm_model::utils::CURRENT_VERSION,
        bot["user_id"].as_str().expect("an id"),
        bot["update_at"].as_i64().expect("a timestamp")
    );

    let conditional = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &etag)
            .send()
            .await
            .expect("reachable");
        (
            response.status().as_u16(),
            response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            response.bytes().await.expect("reads").to_vec(),
        )
    };
    let (go_status, go_etag, go_body) = conditional(common::GO).await;
    let (rs_status, rs_etag, rs_body) = conditional(RUST).await;
    assert_eq!(go_status, 304, "the etag we built is the one Go computes");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_etag, rs_etag, "the 304 carries the ETag on both");
    assert_eq!(go_etag.as_deref(), Some(etag.as_str()));
    assert!(
        go_body.is_empty() && rs_body.is_empty(),
        "a 304 has no body"
    );

    // **A wrong etag is a 200, not a 304.** `HandleEtag` compares the strings; a port that only
    // checked whether the header was *present* would 304 every conditional request and hand a
    // stale bot to any client that had ever seen one — a mutation doing exactly that survived the
    // first run of this suite. The near-misses are the things a real HTTP etag comparison would
    // accept: a weak prefix, a quoted form, and the same etag with a bumped `UpdateAt`.
    let stale = format!(
        "{}.{}.{}",
        mm_model::utils::CURRENT_VERSION,
        bot["user_id"].as_str().expect("an id"),
        bot["update_at"].as_i64().expect("a timestamp") + 1
    );
    let weak = format!("W/{etag}");
    let quoted = format!("\"{etag}\"");
    for wrong in [stale.as_str(), weak.as_str(), quoted.as_str(), ""] {
        let wrong_conditional = async |base: &str| {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("If-None-Match", wrong)
                .send()
                .await
                .expect("reachable");
            (
                response.status().as_u16(),
                response.bytes().await.expect("reads").to_vec(),
            )
        };
        let (go_status, go_body) = wrong_conditional(common::GO).await;
        let (rs_status, rs_body) = wrong_conditional(RUST).await;
        assert_eq!(
            go_status, 200,
            "{wrong:?} is not the etag, so Go sends a body"
        );
        assert_eq!(rs_status, go_status, "{wrong:?}");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{wrong:?}"
        );
    }
}

/// **The security property.** An id that does not exist and a bot the caller may not read give
/// the same 404 — same id, same params, same status.
#[tokio::test]
async fn a_refused_bot_is_indistinguishable_from_a_missing_one() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;

    // The miss, as the admin.
    let missing = format!("/api/v4/bots/{NOWHERE}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, &missing).await;
    assert_eq!(rs_status, go_status, "{missing}");
    assert_eq!(go_status, 404);
    let miss = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &missing);
    assert_eq!(miss["id"], "store.sql_bot.get.missing.app_error");
    assert_served_by_rust(&client, &admin, &missing).await;

    // The refusal, as a plain user asking about a bot that *does* exist and that they do not own.
    let (_, list) = fetch_both_stable(&client, &admin, LIST).await;
    let bots: serde_json::Value = serde_json::from_slice(&list).expect("an array");
    let id = bots[0]["user_id"].as_str().expect("an id").to_owned();

    let team = create_team(&client, &admin, "bots").await;
    let user = create_plain_user(&client, &admin, &team, "bots").await;
    let existing = format!("/api/v4/bots/{id}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &existing).await;
    assert_eq!(rs_status, go_status, "{existing}");
    assert_eq!(
        go_status, 404,
        "a plain user must not be told this id is a bot"
    );
    let refusal = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &existing);

    // The two answers differ only in the id they name, which is the caller's own input.
    assert_eq!(refusal["id"], miss["id"]);
    assert_eq!(refusal["status_code"], miss["status_code"]);

    // And the list route refuses that same user outright — the opposite answer, same file.
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, LIST).await;
    assert_eq!(rs_status, go_status, "{LIST}");
    assert_eq!(go_status, 403, "the list has no id to hide");
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, LIST);
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// The three query flags, each compared on both servers. `only_orphaned` is the one that empties
/// the page — every bot here has a live owner or a plugin id, and neither is a deleted user.
#[tokio::test]
async fn the_query_flags_select_the_same_pages() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for query in [
        "?include_deleted=true",
        "?only_orphaned=true",
        "?include_deleted=true&only_orphaned=true",
        "?per_page=1",
        "?page=1&per_page=1",
        "?page=99",
        // `strconv.ParseBool` discards its error, so a bare key and a nonsense value are both
        // false — the same rule every other boolean flag on this API follows.
        "?include_deleted",
        "?include_deleted=yes",
    ] {
        let path = format!("{LIST}{query}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }

    // An empty page is `[]` and never `null` — the store's `bots := []*model.Bot{}`.
    let (_, empty) =
        fetch_both_stable(&client, &token, &format!("{LIST}?only_orphaned=true")).await;
    assert_eq!(
        empty, b"[]\n",
        "the empty-slice initialiser, plus the encoder's newline"
    );

    // `page=99` is empty for the same reason, through a different route into the same query.
    let (_, past) = fetch_both_stable(&client, &token, &format!("{LIST}?page=99")).await;
    assert_eq!(past, b"[]\n");

    // A page of one is not the whole list, so pagination is doing something.
    let (_, first) = fetch_both_stable(&client, &token, &format!("{LIST}?per_page=1")).await;
    let one: serde_json::Value = serde_json::from_slice(&first).expect("decodes");
    assert_eq!(one.as_array().map(Vec::len), Some(1));
}

/// `RequireBotUserId`'s 400, and the router charset above it.
#[tokio::test]
async fn the_id_checks_agree() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // In the mux charset, not an id: the URL-param 400, not the body-param one.
    let path = "/api/v4/bots/short";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    assert_eq!(parsed["detailed_error"], "");

    // Outside the mux charset: gorilla never routed it, so it is forwarded and Go answers its own
    // 404 — byte-identical, because a forwarded response *is* Go's.
    let path = "/api/v4/bots/has.dot";
    let go = client
        .get(format!("{}{path}", common::GO))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    let go_status = go.status().as_u16();
    let go_body = go.bytes().await.expect("reads").to_vec();

    let rs = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    let served_by = rs
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let rs_status = rs.status().as_u16();
    let rs_body = rs.bytes().await.expect("reads").to_vec();

    assert_eq!(served_by.as_deref(), Some("go"), "{path} must be forwarded");
    assert_eq!(go_status, 404, "gorilla's NotFoundHandler");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
}

/// **`read_bots` without `read_others_bots` is the caller no stock role can be.**
///
/// Every branch that distinguishes the two permissions is unreachable otherwise: the system admin
/// holds both, so `getBots` always filters by no owner and `getBot`'s owner arm is never the one
/// that admits. Five mutations survived the first run of this suite for exactly that reason —
/// including one that let *any* caller read *any* bot. The role is planted; the bots are planted;
/// this is the fixture that makes the cascade testable.
#[tokio::test]
async fn read_bots_alone_admits_only_the_callers_own_bots() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    // `unplant_bots` sweeps every `mmrsbot%` row, including the writes suite's. See the lock.
    let _bots = common::BOT_FIXTURES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "botsown").await;

    let Some(role) = common::plant_role("botsown", "read_bots").await else {
        return; // no DATABASE_URL — the planted fixtures cannot be built
    };
    let user = create_plain_user(&client, &admin, &team, "botsown").await;
    common::set_user_roles(&user.id, &format!("system_user {role}")).await;
    let token = common::login_plain_user(&client, "botsown").await;

    let Some(mine) = common::plant_bot("own", &user.id, 0).await else {
        return;
    };
    let Some(theirs) = common::plant_bot("other", common::logged_in_user_id(), 0).await else {
        return;
    };

    // The list is **narrowed to this caller's own bots**, not refused and not the whole table.
    let (go, rs) = fetch_both_stable(&client, &token, LIST).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{LIST} as a read_bots-only caller"
    );
    let listed: serde_json::Value = serde_json::from_slice(&rs).expect("an array");
    let ids: Vec<&str> = listed
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|bot| bot["user_id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![mine.as_str()],
        "read_bots sees its own bot and nothing else: {listed}"
    );

    // The single-bot route admits the one they own...
    let path = format!("/api/v4/bots/{mine}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 200, "the owner arm admits with read_bots");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    // ...and hides the one they do not, with the same 404 a missing id gets.
    let path = format!("/api/v4/bots/{theirs}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        go_status,
        404,
        "read_bots is not read_others_bots: {}",
        String::from_utf8_lossy(&go)
    );
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "store.sql_bot.get.missing.app_error");

    // The admin, holding `read_others_bots`, sees both — so the narrowing above is the
    // permission's doing and not an empty table.
    let (_, all) = fetch_both_stable(&client, &admin, LIST).await;
    let all: serde_json::Value = serde_json::from_slice(&all).expect("an array");
    let admin_ids: Vec<&str> = all
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|bot| bot["user_id"].as_str())
        .collect();
    assert!(admin_ids.contains(&mine.as_str()) && admin_ids.contains(&theirs.as_str()));

    common::unplant_bots().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// A **deleted** bot, which no REST route can create, and the flag that reveals it.
#[tokio::test]
async fn include_deleted_reveals_a_deleted_bot_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    // `unplant_bots` sweeps every `mmrsbot%` row, including the writes suite's. See the lock.
    let _bots = common::BOT_FIXTURES.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = common::logged_in_user_id();

    let Some(gone) = common::plant_bot("gone", me, 1788600001000).await else {
        return; // no DATABASE_URL
    };

    let list_ids = async |query: &str| -> Vec<String> {
        let (go, rs) = fetch_both_stable(&client, &token, &format!("{LIST}{query}")).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{LIST}{query}"
        );
        let parsed: serde_json::Value = serde_json::from_slice(&rs).expect("an array");
        parsed
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|bot| bot["user_id"].as_str().map(str::to_owned))
            .collect()
    };

    let without = list_ids("").await;
    let with = list_ids("?include_deleted=true").await;
    assert!(
        !without.contains(&gone),
        "a deleted bot is absent by default"
    );
    assert!(with.contains(&gone), "and present with the flag");
    // The flag **widens**: it does not select only deleted bots. Asserted as a subset rather than
    // as `with.len() == without.len() + 1`, because that arithmetic quietly assumes this test is
    // the only source of a deleted bot on the installation — and a *sibling test that panics*
    // breaks the assumption without any concurrency being involved. Measured: when
    // `bot_writes::disable_and_enable_flip_both_rows_and_only_one_is_idempotent` failed an
    // assertion it never reached its `unplant_bot` cleanup, leaving two disabled bots behind, and
    // this test then failed too — one bug reported as two. `BOT_FIXTURES` does not help here; the
    // residue outlives the lock.
    assert!(
        without.iter().all(|id| with.contains(id)),
        "every bot listed without the flag must still be listed with it: {with:?} vs {without:?}"
    );

    // The single-bot route carries the same flag with the same meaning.
    let path = format!("/api/v4/bots/{gone}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 404, "not found without the flag");
    common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);

    let path = format!("/api/v4/bots/{gone}?include_deleted=true");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 200, "found with it");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    common::unplant_bots().await;
}
