//! Cross-server parity for `POST /api/v4/posts` carrying `file_ids` — `attachFilesToPost`.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_files
//! ```
//!
//! A file is uploaded (through Go, on both sides — the rows are shared) and then claimed by a
//! post. What the claim leaves behind is the oracle: the `FileInfo` row's `PostId` and
//! `ChannelId`, the post's `FileIds` column, and — when fewer files attached than were listed —
//! an `Overwrite` that moves `update_at` off `create_at` on a post nobody edited. The `posted`
//! event carries `otherFile` and `image` as the **strings** `"true"`.
//!
//! Each server gets its own uploads, because a file attaches once. Ids therefore differ between
//! the two responses and are normalised by position in the sorted list.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, TINY_PNG, add_user_to_channel, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    purge_api_fixtures, set_fileinfo_column, stack_enabled, upload_file,
};

/// `(message, listed file ids, otherFile, image)` — one `posted` frame's expectation.
type EventCase<'a> = (&'a str, Vec<&'a str>, Option<&'a str>, Option<&'a str>);

/// A well-formed id that is not a file.
const NOT_A_FILE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

struct Fixture {
    channel_id: String,
    reader: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cpf").await;
            let channel_id = create_channel_typed(client, token, &team_id, "cpf", "O").await;
            let reader = create_plain_user(client, token, &team_id, "cpf").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            Fixture { channel_id, reader }
        })
        .await
}

async fn create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: &str,
    file_ids: &[&str],
) -> (u16, bool, serde_json::Value) {
    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "root_id": root_id,
            "file_ids": file_ids,
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    let body = response.json().await.unwrap_or(serde_json::Value::Null);
    (status, served, body)
}

async fn get_post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, serde_json::Value) {
    let response = client
        .get(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    (
        status,
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

/// A post with its ids, clocks and file paths replaced, and each file id replaced by the
/// file's **name** — which upload drew the lexically smaller id is luck, so a placeholder by
/// sorted position would pair `a.png` with `a.txt` on one server and not the other. The
/// name-keyed lists are then sorted; the *order* the server answered is asserted by each test
/// against the sorted ids separately.
fn normalised(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at", "pending_post_id", "root_id"] {
        if obj.contains_key(key) {
            obj.insert(key.to_owned(), serde_json::json!(0));
        }
    }
    let names: std::collections::HashMap<String, String> = obj
        .get("metadata")
        .and_then(|m| m.get("files"))
        .and_then(|f| f.as_array())
        .map(|files| {
            files
                .iter()
                .map(|file| {
                    (
                        file["id"].as_str().unwrap_or_default().to_owned(),
                        file["name"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let placeholder = |id: &str| {
        names
            .get(id)
            .map_or_else(|| format!("unknown:{id}"), |name| format!("file:{name}"))
    };
    if let Some(list) = obj.get_mut("file_ids").and_then(|v| v.as_array_mut()) {
        for id in list.iter_mut() {
            let replaced = placeholder(id.as_str().unwrap_or_default());
            *id = serde_json::Value::String(replaced);
        }
        list.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
    if let Some(files) = obj
        .get_mut("metadata")
        .and_then(|m| m.get_mut("files"))
        .and_then(|f| f.as_array_mut())
    {
        for file in files.iter_mut() {
            let file = file.as_object_mut().expect("a file info object");
            let replaced = placeholder(file["id"].as_str().unwrap_or_default());
            file.insert("id".to_owned(), serde_json::Value::String(replaced));
            for key in ["post_id", "create_at", "update_at"] {
                file.insert(key.to_owned(), serde_json::json!(0));
            }
            for key in ["path", "thumbnail_path", "preview_path"] {
                file.insert(key.to_owned(), serde_json::json!(""));
            }
        }
        files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    }
    post
}

/// `(updateat, fileids)` of the post row.
async fn post_row(post_id: &str) -> (i64, String) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT updateat, fileids FROM posts WHERE id = $1")
        .bind(post_id)
        .fetch_one(&pool)
        .await
        .expect("the post row")
}

/// `(postid, channelid)` of the file row. The upload already wrote `channelid`; the claim
/// repeats it and writes `postid`.
async fn file_row(file_id: &str) -> (String, Option<String>) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT postid, channelid FROM fileinfo WHERE id = $1")
        .bind(file_id)
        .fetch_one(&pool)
        .await
        .expect("the file row")
}

/// `(lastreplyat, replycount)` of the root's `Threads` row.
async fn thread_row(root_id: &str) -> (i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT lastreplyat, replycount FROM threads WHERE postid = $1")
        .bind(root_id)
        .fetch_one(&pool)
        .await
        .expect("the thread row")
}

fn sorted(ids: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    ids.sort();
    ids
}

fn file_ids_of(body: &serde_json::Value) -> serde_json::Value {
    body["file_ids"].clone()
}

/// Two files of the admin's, listed in reverse order: both attach, the post is not overwritten,
/// and `metadata.files` follows the **sorted** `file_ids` that `PreSave` leaves.
#[tokio::test]
async fn two_own_files_attach_and_the_rows_are_reparented() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let png = upload_file(
            &client,
            &token,
            &f.channel_id,
            "a.png",
            "image/png",
            TINY_PNG,
        )
        .await;
        let txt = upload_file(
            &client,
            &token,
            &f.channel_id,
            "a.txt",
            "text/plain",
            b"cpf two files",
        )
        .await;
        let ids = sorted(&[&png, &txt]);
        // Listed in the opposite order to the one the column will hold.
        let listed: Vec<&str> = ids.iter().rev().map(String::as_str).collect();
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf both",
            "",
            &listed,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(file_ids_of(&body), serde_json::json!(ids), "{base}");
        assert_eq!(
            body["update_at"], body["create_at"],
            "{base}: every file attached, so no overwrite"
        );
        let files = body["metadata"]["files"]
            .as_array()
            .expect("metadata.files");
        assert_eq!(
            files
                .iter()
                .map(|file| file["id"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>(),
            ids,
            "{base}: metadata.files in file_ids order"
        );
        for id in &ids {
            assert_eq!(
                file_row(id).await,
                (post_id.to_owned(), Some(f.channel_id.clone())),
                "{base}: {id} re-parented"
            );
        }
        let (update_at, column) = post_row(post_id).await;
        assert_eq!(
            update_at,
            body["update_at"].as_i64().unwrap_or_default(),
            "{base}"
        );
        assert_eq!(column, serde_json::json!(ids).to_string(), "{base}");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1], "the two bodies, ids by position");
}

/// A file the reader uploaded cannot be claimed by the admin: nothing attaches, the post is
/// overwritten with a **nil** list (`"file_ids":null`, the column holding `null`), and a later
/// read decodes that column.
#[tokio::test]
async fn a_file_owned_by_another_user_does_not_attach_and_the_post_is_overwritten() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    let mut reads = Vec::new();
    for base in [GO, RUST] {
        let theirs = upload_file(
            &client,
            &f.reader.token,
            &f.channel_id,
            "theirs.txt",
            "text/plain",
            b"cpf not mine",
        )
        .await;
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf theirs",
            "",
            &[&theirs],
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            file_ids_of(&body),
            serde_json::Value::Null,
            "{base}: a nil list"
        );
        assert!(
            body["metadata"].get("files").is_none(),
            "{base}: no files to describe: {body}"
        );
        assert!(
            body["update_at"].as_i64() > body["create_at"].as_i64(),
            "{base}: the overwrite moved update_at: {body}"
        );
        assert_eq!(
            file_row(&theirs).await.0,
            "",
            "{base}: the reader's file is still unclaimed"
        );
        let (update_at, column) = post_row(post_id).await;
        assert_eq!(
            update_at,
            body["update_at"].as_i64().unwrap_or_default(),
            "{base}"
        );
        assert_eq!(column, "null", "{base}: ArrayToJSON(nil)");
        bodies.push(normalised(&body));

        // Both servers read the row this one wrote.
        for reader in [GO, RUST] {
            let (status, read) = get_post(&client, reader, &token, post_id).await;
            assert_eq!(status, 200, "{reader} reading {base}'s post: {read}");
            assert_eq!(
                file_ids_of(&read),
                serde_json::Value::Null,
                "{reader}: {read}"
            );
            reads.push(normalised(&read));
        }
    }
    assert_eq!(bodies[0], bodies[1]);
    assert!(
        reads.iter().all(|read| read == &reads[0]),
        "every read of a null-file-id post agrees: {reads:#?}"
    );
}

/// Two of the admin's files and one id that is not a file: the files attach, the post is
/// overwritten down to the attached list — in the order the ids were tried — and
/// `metadata.files` describes only those two.
#[tokio::test]
async fn a_partial_attachment_keeps_the_attached_ids_and_bumps_update_at() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let one = upload_file(
            &client,
            &token,
            &f.channel_id,
            "partial1.txt",
            "text/plain",
            b"cpf partial one",
        )
        .await;
        let two = upload_file(
            &client,
            &token,
            &f.channel_id,
            "partial2.txt",
            "text/plain",
            b"cpf partial two",
        )
        .await;
        let ids = sorted(&[&one, &two]);
        // The attached list is written in the order the ids were **tried**, which is the sorted
        // order the captured list holds — not the listed order, which is reversed here.
        let listed: Vec<&str> = std::iter::once(NOT_A_FILE)
            .chain(ids.iter().rev().map(String::as_str))
            .collect();
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf partial",
            "",
            &listed,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(file_ids_of(&body), serde_json::json!(ids), "{base}");
        assert_eq!(
            body["metadata"]["files"].as_array().map(Vec::len),
            Some(2),
            "{base}: {body}"
        );
        assert!(
            body["update_at"].as_i64() > body["create_at"].as_i64(),
            "{base}: {body}"
        );
        for id in &ids {
            assert_eq!(
                file_row(id).await,
                (post_id.to_owned(), Some(f.channel_id.clone())),
                "{base}: {id}"
            );
        }
        let (update_at, column) = post_row(post_id).await;
        assert_eq!(
            update_at,
            body["update_at"].as_i64().unwrap_or_default(),
            "{base}"
        );
        assert_eq!(column, serde_json::json!(ids).to_string(), "{base}");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

#[tokio::test]
async fn a_duplicate_id_attaches_once_and_still_overwrites() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let png = upload_file(
            &client,
            &token,
            &f.channel_id,
            "d.png",
            "image/png",
            TINY_PNG,
        )
        .await;
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf twice",
            "",
            &[&png, &png],
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(file_ids_of(&body), serde_json::json!([png]), "{base}");
        assert!(
            body["update_at"].as_i64() > body["create_at"].as_i64(),
            "{base}: the captured list was two long: {body}"
        );
        assert_eq!(
            file_row(&png).await,
            (post_id.to_owned(), Some(f.channel_id.clone())),
            "{base}"
        );
        let (update_at, column) = post_row(post_id).await;
        assert_eq!(
            update_at,
            body["update_at"].as_i64().unwrap_or_default(),
            "{base}"
        );
        assert_eq!(column, serde_json::json!([png]).to_string(), "{base}");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// `Overwrite` on a reply runs `UPDATE Threads SET LastReplyAt WHERE PostId = <the reply>`,
/// which matches nothing: the thread keeps the reply's `create_at` while the reply's own
/// `update_at` moves.
#[tokio::test]
async fn a_partial_attachment_on_a_reply_leaves_the_thread_row_untouched() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, _, root) =
            create(&client, GO, &token, &f.channel_id, "cpf root", "", &[]).await;
        assert_eq!(status, 201, "{root}");
        let root_id = root["id"].as_str().expect("an id");
        let txt = upload_file(
            &client,
            &token,
            &f.channel_id,
            "reply.txt",
            "text/plain",
            b"cpf reply",
        )
        .await;
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf reply",
            root_id,
            &[&txt, NOT_A_FILE],
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(file_ids_of(&body), serde_json::json!([txt]), "{base}");
        assert!(
            body["update_at"].as_i64() > body["create_at"].as_i64(),
            "{base}: {body}"
        );
        assert_eq!(
            thread_row(root_id).await,
            (body["create_at"].as_i64().unwrap_or_default(), 1),
            "{base}: LastReplyAt is the reply's create_at, not its update_at"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A file whose `CreatorId` is the literal `nouser` — a plugin upload — attaches to anyone's
/// post.
#[tokio::test]
async fn a_nouser_file_attaches_to_another_users_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let theirs = upload_file(
            &client,
            &f.reader.token,
            &f.channel_id,
            "plugin.txt",
            "text/plain",
            b"cpf nouser",
        )
        .await;
        if !set_fileinfo_column(&theirs, "creatorid", "'nouser'").await {
            return;
        }
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.channel_id,
            "cpf nouser",
            "",
            &[&theirs],
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(file_ids_of(&body), serde_json::json!([theirs]), "{base}");
        assert_eq!(body["update_at"], body["create_at"], "{base}: no overwrite");
        assert_eq!(
            file_row(&theirs).await,
            (post_id.to_owned(), Some(f.channel_id.clone())),
            "{base}"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// The `posted` event's `otherFile` and `image` keys, four ways: an image and a text file say
/// both; a text file says `otherFile` only; a post none of whose files attached says neither
/// (the gate is the post-attach list); and an attached but soft-deleted image says `otherFile`
/// only, because `GetForPost` excludes deleted rows while `AttachToPost` did not.
#[tokio::test]
async fn the_posted_event_flags_files_and_images() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [GO, RUST] {
        let png = upload_file(
            &client,
            &token,
            &f.channel_id,
            "e.png",
            "image/png",
            TINY_PNG,
        )
        .await;
        let txt = upload_file(
            &client,
            &token,
            &f.channel_id,
            "e.txt",
            "text/plain",
            b"cpf e",
        )
        .await;
        let txt2 = upload_file(
            &client,
            &token,
            &f.channel_id,
            "e2.txt",
            "text/plain",
            b"cpf e2",
        )
        .await;
        let theirs = upload_file(
            &client,
            &f.reader.token,
            &f.channel_id,
            "e3.txt",
            "text/plain",
            b"cpf e3",
        )
        .await;
        let gone = upload_file(
            &client,
            &token,
            &f.channel_id,
            "g.png",
            "image/png",
            TINY_PNG,
        )
        .await;
        if !set_fileinfo_column(&gone, "deleteat", "1").await {
            return;
        }

        let mut gone_post_id = String::new();
        let cases: [EventCase<'_>; 4] = [
            ("cpf ev both", vec![&png, &txt], Some("true"), Some("true")),
            ("cpf ev text", vec![&txt2], Some("true"), None),
            ("cpf ev none", vec![&theirs], None, None),
            ("cpf ev gone", vec![&gone], Some("true"), None),
        ];
        for (message, listed, other_file, image) in cases {
            let mut socket = SocketProbe::connect(base, &f.reader.token).await;
            let (status, served, body) =
                create(&client, base, &token, &f.channel_id, message, "", &listed).await;
            assert_eq!(status, 201, "{base} {message}: {body}");
            assert_eq!(served, base == RUST, "{base} {message}");
            let post_id = body["id"].as_str().expect("an id").to_owned();
            if message == "cpf ev gone" {
                gone_post_id.clone_from(&post_id);
            }
            let posted_for = |frames: &[serde_json::Value]| {
                frames.iter().any(|f| {
                    f["event"] == "posted"
                        && f["data"]["post"]
                            .as_str()
                            .is_some_and(|p| p.contains(&post_id))
                })
            };
            assert!(
                socket
                    .collect_until(Duration::from_millis(2500), posted_for)
                    .await,
                "{base} {message}: no posted frame: {:?}",
                socket.raw
            );
            let frame = socket
                .events_named("posted")
                .into_iter()
                .find(|f| {
                    f["data"]["post"]
                        .as_str()
                        .is_some_and(|p| p.contains(&post_id))
                })
                .expect("the frame");
            assert_eq!(
                frame["data"].get("otherFile").and_then(|v| v.as_str()),
                other_file,
                "{base} {message}: {frame}"
            );
            assert_eq!(
                frame["data"].get("image").and_then(|v| v.as_str()),
                image,
                "{base} {message}: {frame}"
            );
        }
        // The deleted image attached all the same.
        assert_eq!(
            file_row(&gone).await.0,
            gone_post_id,
            "{base}: AttachToPost does not test DeleteAt"
        );
    }
}
