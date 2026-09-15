//! Cross-server parity for `POST /api/v4/boards` — `createBoard` (api4/board.go:20).
//!
//! ```sh
//! scripts/go-boards.sh start && scripts/parity.sh --test parity boards
//! ```
//!
//! The route exists on Go only with `IntegratedBoards` on, so the served shape is measured
//! against the boards oracle the `views` suite already stands up (`views::lit`), and the flag-off
//! shape — the mux 404 — against the stack's own pair, where ours forwards. Each server creates
//! its own board (the name is unique per server), so the two 201s are compared with the per-row
//! fields removed; the linked property ids in `props` are the same rows on the shared database
//! and must match exactly. A probe on each server's websocket asserts the `board_created` and
//! `view_created` broadcasts.

use std::time::Duration;

use crate::common;
use crate::parity::views::{Lit, lit};

use common::{
    GO, RUST, SocketProbe, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    create_team, delete_plain_user, fixture_pool, go_minted_token, request_raw, stack_enabled,
};

const PATH: &str = "/api/v4/boards";

fn unique(tag: &str) -> String {
    format!("mmrs-parity-{tag}-{}", &mm_model::utils::new_id()[..8])
}

/// Drop the fields two independently created boards cannot share.
fn normalise(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for key in [
            "id",
            "name",
            "create_at",
            "update_at",
            "last_post_at",
            "last_root_post_at",
        ] {
            obj.remove(key);
        }
    }
    value
}

/// A view with its ids and timestamps removed, the kanban column ids included — they are minted
/// per view.
fn normalise_view(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for key in ["id", "channel_id", "create_at", "update_at"] {
            obj.remove(key);
        }
    }
    if let Some(columns) = value
        .pointer_mut("/props/group_by/columns")
        .and_then(serde_json::Value::as_array_mut)
    {
        for column in columns {
            if let Some(obj) = column.as_object_mut() {
                obj.remove("id");
            }
        }
    }
    value
}

async fn frame_named(socket: &mut SocketProbe, name: &str) -> Option<serde_json::Value> {
    let wanted = name.to_owned();
    let found = move |frames: &[serde_json::Value]| frames.iter().any(|f| f["event"] == wanted);
    if !socket
        .collect_until(Duration::from_millis(2500), found)
        .await
    {
        return None;
    }
    socket.events_named(name).into_iter().next()
}

/// With the flag off the mux has no such route: both answer the 404 and ours is Go's.
#[tokio::test]
async fn with_the_flag_off_the_route_is_the_mux_404_from_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let body = serde_json::json!({"type": "BO", "team_id": "x"}).to_string();
    let (go_status, go_body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::POST,
        Some(&token),
        PATH,
        Some(body.as_bytes()),
    )
    .await;
    let (rs_status, rs_body, served_by) = request_raw(
        &client,
        RUST,
        reqwest::Method::POST,
        Some(&token),
        PATH,
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404));
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "the flag-off route must forward"
    );
    let go: serde_json::Value = serde_json::from_slice(&go_body).unwrap();
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).unwrap();
    assert_eq!(go["id"], "api.context.404.app_error");
    assert_eq!(go["detailed_error"], rs["detailed_error"]);
}

/// The stored view and membership rows, read straight from the tables: Go's own `GetChannel`
/// filters to message channel types, so a board is invisible to `GET /channels/{id}` and to
/// `GET /channels/{id}/views` on Go itself (measured), and the rows are the only oracle.
async fn stored_rows(channel_id: &str) -> (serde_json::Value, serde_json::Value, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let view: (String, String, String, i32, Option<serde_json::Value>, i64) = sqlx::query_as(
        "SELECT type, creatorid, title, sortorder, props, deleteat FROM views WHERE channelid = $1",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("one view row");
    let member: (bool, bool, bool, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT schemeguest, schemeuser, schemeadmin, notifyprops FROM channelmembers \
         WHERE channelid = $1",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("one member row");
    let history: (i64,) =
        sqlx::query_as("SELECT count(*) FROM channelmemberhistory WHERE channelid = $1")
            .bind(channel_id)
            .fetch_one(&pool)
            .await
            .expect("the history count");
    let public: (i64,) = sqlx::query_as("SELECT count(*) FROM publicchannels WHERE id = $1")
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .expect("the public count");
    assert_eq!(public.0, 0, "board {channel_id} is in PublicChannels");
    (
        normalise_view(serde_json::json!({
            "type": view.0, "creator_id": view.1, "title": view.2, "sort_order": view.3,
            "props": view.4, "delete_at": view.5,
        })),
        serde_json::json!({
            "scheme_guest": member.0, "scheme_user": member.1, "scheme_admin": member.2,
            "notify_props": member.3,
        }),
        history.0,
    )
}

/// An open board and a private board, one of each per server: 201, the same channel once the
/// per-row fields are dropped, the same linked properties, a kanban view with a column per
/// status option, the creator as channel admin with a join-history row, no `PublicChannels`
/// row, and the two broadcasts.
#[tokio::test]
async fn a_board_is_created_identically_on_both_servers() {
    let Some(lit) = lit().await else {
        return;
    };
    let team = create_team(&lit.http, &lit.token, "board").await;
    let me = common::logged_in_user_id();

    for channel_type in ["BO", "BP"] {
        let mut created: Vec<(
            serde_json::Value,
            serde_json::Value,
            serde_json::Value,
            serde_json::Value,
        )> = Vec::new();
        for base in [lit.go.clone(), lit.rust.clone()] {
            let mut probe = SocketProbe::connect(&base, &lit.token).await;
            let body = serde_json::json!({
                "team_id": team,
                "name": unique("board"),
                "display_name": format!("  Parity {channel_type} board  "),
                "type": channel_type,
                "header": "a & b <c>",
                "creator_id": "ignoredcreator000000000000",
            });
            let (status, bytes) = lit.send(&base, "POST", PATH, Some(&body)).await;
            assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&bytes));
            assert_eq!(
                bytes.last(),
                Some(&b'\n'),
                "{base}: json.NewEncoder's newline"
            );
            assert!(
                bytes.windows(6).any(|w| w == b"\\u0026"),
                "{base}: encoding/json escapes the header's ampersand"
            );
            let channel: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let id = channel["id"].as_str().unwrap().to_owned();
            assert_eq!(
                channel["creator_id"], me,
                "{base}: the creator is the caller"
            );
            assert_eq!(
                channel["display_name"],
                format!("Parity {channel_type} board")
            );
            assert_eq!(
                channel["props"]["board:linked_properties"]
                    .as_array()
                    .map(Vec::len),
                Some(2),
                "{base}: two linked properties"
            );

            let board_created = frame_named(&mut probe, "board_created")
                .await
                .unwrap_or_else(|| panic!("{base}: no board_created frame"));
            assert_eq!(
                board_created["data"],
                serde_json::json!({"channel_id": id, "team_id": team}),
                "{base}"
            );
            assert_eq!(board_created["broadcast"]["user_id"], me, "{base}");
            let view_created = frame_named(&mut probe, "view_created")
                .await
                .unwrap_or_else(|| panic!("{base}: no view_created frame"));
            assert_eq!(view_created["broadcast"]["channel_id"], id, "{base}");
            let event_view: serde_json::Value =
                serde_json::from_str(view_created["data"]["view"].as_str().unwrap()).unwrap();
            assert_eq!(event_view["channel_id"], id, "{base}");

            let (view, member, history) = stored_rows(&id).await;
            assert_eq!(view["type"], "kanban", "{base}");
            assert_eq!(view["title"], "Board", "{base}");
            assert_eq!(history, 1, "{base}: one join-history row");
            assert_eq!(member["scheme_admin"], true, "{base}");

            created.push((normalise(channel), view, normalise_view(event_view), member));
        }
        let (go_channel, go_view, go_event, go_member) = &created[0];
        let (rs_channel, rs_view, rs_event, rs_member) = &created[1];
        assert_eq!(go_channel, rs_channel, "{channel_type}: the boards differ");
        assert_eq!(
            go_view, rs_view,
            "{channel_type}: the stored kanban views differ"
        );
        assert_eq!(
            go_event, rs_event,
            "{channel_type}: the broadcast views differ"
        );
        assert_eq!(
            go_member, rs_member,
            "{channel_type}: the memberships differ"
        );
    }
}

/// There is no team lookup anywhere on the path: an administrator, who holds the create
/// permission on every team, gets a 201 for a board on a team id that does not exist — on both.
#[tokio::test]
async fn a_board_on_an_unknown_team_is_a_201_for_an_admin() {
    let Some(lit) = lit().await else {
        return;
    };
    let mut bodies = Vec::new();
    for base in [lit.go.clone(), lit.rust.clone()] {
        let body = serde_json::json!({
            "team_id": "nosuchteam0000000000000000",
            "name": unique("noteam"),
            "display_name": "No team",
            "type": "BO",
        });
        let (status, bytes) = lit.send(&base, "POST", PATH, Some(&body)).await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&bytes));
        bodies.push(normalise(serde_json::from_slice(&bytes).unwrap()));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// The refusals, in the handler's order: a body that does not decode, a non-board type, no
/// team, no permission (a plain user outside the team, for either board type), a blank display
/// name, no name, and a taken name.
#[tokio::test]
async fn the_refusals_match() {
    let Some(lit) = lit().await else {
        return;
    };
    let team = create_team(&lit.http, &lit.token, "boardref").await;
    let other_team = create_team(&lit.http, &lit.token, "boardoth").await;
    let plain = create_plain_user(&lit.http, &lit.token, &other_team, "boardref").await;

    let taken = unique("taken");
    let (status, _) = lit
        .send(
            &lit.go.clone(),
            "POST",
            PATH,
            Some(&serde_json::json!({"team_id": team, "name": taken, "display_name": "Taken", "type": "BO"})),
        )
        .await;
    assert_eq!(status, 201);

    let cases: Vec<(&str, u16, serde_json::Value)> = vec![
        (
            "bad json",
            400,
            serde_json::Value::String("{not json".to_owned()),
        ),
        ("null body", 400, serde_json::Value::Null),
        (
            "a message channel",
            400,
            serde_json::json!({"team_id": team, "name": unique("o"), "display_name": "O", "type": "O"}),
        ),
        (
            "no team",
            400,
            serde_json::json!({"name": unique("nt"), "display_name": "NT", "type": "BO"}),
        ),
        (
            "blank display name",
            400,
            serde_json::json!({"team_id": team, "name": unique("bd"), "display_name": "   ", "type": "BP"}),
        ),
        (
            "no name",
            400,
            serde_json::json!({"team_id": team, "display_name": "No name", "type": "BO"}),
        ),
        (
            "taken name",
            400,
            serde_json::json!({"team_id": team, "name": taken, "display_name": "Taken again", "type": "BO"}),
        ),
    ];
    for (label, expected, body) in cases {
        let go = send_as(&lit, &lit.go.clone(), &lit.token, &body).await;
        let rs = send_as(&lit, &lit.rust.clone(), &lit.token, &body).await;
        assert_eq!((go.0, rs.0), (expected, expected), "{label}");
        let got = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, label);
        eprintln!("{label}: {}", got["id"]);
    }

    // A plain user outside the team, for both board types: the type-specific permission.
    for channel_type in ["BO", "BP"] {
        let body = serde_json::json!({"team_id": team, "name": unique("pl"), "display_name": "PL", "type": channel_type});
        let go = send_as(&lit, &lit.go.clone(), &plain.token, &body).await;
        let rs = send_as(&lit, &lit.rust.clone(), &plain.token, &body).await;
        assert_eq!((go.0, rs.0), (403, 403), "{channel_type}");
        assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, channel_type);
    }

    delete_plain_user(&lit.http, &lit.token, &plain.id).await;
}

/// `POST` with an arbitrary body — a string is sent raw, so a broken body reaches the decoder.
async fn send_as(lit: &Lit, base: &str, token: &str, body: &serde_json::Value) -> (u16, Vec<u8>) {
    let raw = match body {
        serde_json::Value::String(raw) => raw.clone(),
        other => other.to_string(),
    };
    let response = lit
        .http
        .post(format!("{base}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(raw)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{PATH} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == lit.rust {
        common::assert_served_by_rust(response.headers(), PATH);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}
