//! Cross-server parity for **reading** a post whose message has a link: `getEmbedsAndImages` with
//! `isNewPost` false on every read route that prepares a post, an edit, a create whose permalink
//! points at such a post, and the requests each server makes of the website while doing it.
//!
//! ```sh
//! scripts/parity.sh --test parity post_link_reads
//! ```
//!
//! Runs on `parity::post_create_links`' pair — `scripts/go-links.sh` (Go's port + 50) and the
//! mm-api on 8116 forwarding to it — and its mock website, whose lock this suite takes too.
//!
//! # Planted by one server, read by both
//!
//! Every link preview is fetched once, when its post is created, and leaves a `LinkMetadata` row
//! and an entry in **the creating process's** link cache. A read then answers from that cache or,
//! in any other process, from the row. So each case is planted twice: through Go, whose reads
//! come from its cache while ours come from the row, and through this server, the reverse. After
//! planting, every page but the long one is replaced by a 404, so a read that fetched instead of
//! using the row would both show in the request log and change the preview.
//!
//! The one URL with no row is longer than `LinkMetadataMaxURLLength`: its save fails, so the
//! process that did not create the post fetches it on its first read — in Go as in a second Go
//! node — and the two cold reads must make the same request.

use crate::common;

use super::post_create_links::{
    Fixture, WEBSITE, animated_gif, compare, create, create_at, fixture, links_go, nonce,
    normalised_text, og_page, png, saved_props,
};
use common::{client, create_channel_typed, fixture_pool, go_minted_token, stack_enabled};

/// A post body both servers answer with, and whether this server served it.
async fn ask(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, String, bool) {
    let body = body.map(|b| serde_json::to_vec(&b).unwrap());
    let (status, bytes, served_by) =
        common::request_raw(client, base, method, Some(token), path, body.as_deref()).await;
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        served_by.as_deref() == Some("rust"),
    )
}

/// The cases of one planting: every post id, and the URLs they link to.
struct Planted {
    channel_id: String,
    team_id: String,
    /// The word every message carries, for search.
    word: String,
    og: String,
    image: String,
    plain: String,
    none: String,
    long: String,
    long_url: String,
    permalink: String,
    svg: String,
    all: Vec<String>,
}

/// The website's pages for nonce `n`, returning `(og, image, plain, long)` URLs.
fn serve_pages(f: &Fixture, n: &str) -> (String, String, String, String) {
    f.site.page(
        &format!("/og/{n}"),
        200,
        Some("text/html; charset=utf-8"),
        og_page(n).as_bytes(),
    );
    f.site
        .page(&format!("/img/{n}/z.png"), 200, Some("image/png"), &png());
    f.site
        .page(&format!("/img/{n}/p.png"), 200, Some("image/png"), &png());
    f.site
        .page(&format!("/img/{n}/m.svg"), 200, Some("image/png"), &png());
    f.site.page(
        &format!("/img/{n}/anim.gif"),
        200,
        Some("image/gif"),
        &animated_gif(),
    );
    f.site.page(
        &format!("/bare/{n}"),
        200,
        Some("text/html"),
        b"<html><head><title>no og</title></head></html>",
    );
    // The path alone carries the length: the mock matches on the path.
    let long_path = format!("/long/{n}/{}", "l".repeat(2100));
    f.site.page(
        &long_path,
        200,
        Some("text/html"),
        br#"<meta property="og:title" content="Long"><meta property="og:type" content="website">"#,
    );
    let base = &f.site.base;
    (
        format!("{base}/og/{n}"),
        format!("{base}/img/{n}/p.png"),
        format!("{base}/bare/{n}"),
        format!("{base}{long_path}"),
    )
}

/// Every page but the long one now answers 404, so a fetch on a read changes the preview.
fn withdraw_pages(f: &Fixture, n: &str) {
    for path in [
        format!("/og/{n}"),
        format!("/img/{n}/z.png"),
        format!("/img/{n}/p.png"),
        format!("/img/{n}/anim.gif"),
        format!("/img/{n}/m.svg"),
        format!("/bare/{n}"),
    ] {
        f.site
            .page(&path, 404, Some("text/plain"), b"withdrawn after planting");
    }
}

async fn team_id_of(client: &reqwest::Client, token: &str, channel_id: &str) -> String {
    let (status, body, _) = ask(
        client,
        common::GO,
        reqwest::Method::GET,
        token,
        &format!("/api/v4/channels/{channel_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    value["team_id"].as_str().unwrap().to_owned()
}

/// Create the cases through `base`, each at its own millisecond inside one hour, in a fresh
/// channel; flag and pin two of them through the main Go server.
async fn plant(
    client: &reqwest::Client,
    token: &str,
    f: &Fixture,
    base: &str,
    tag: &str,
) -> Planted {
    let n = nonce();
    let (og, image, plain, long_url) = serve_pages(f, &n);
    let none = format!("http://127.0.0.1:9/cplr/{n}");
    let word = format!("cplrw{n}");
    let channel_id = create_channel_typed(
        client,
        token,
        &f_team(client, token, f).await,
        &format!("cplr{tag}{n}"),
        "O",
    )
    .await;
    let team_id = team_id_of(client, token, &channel_id).await;
    let at = create_at();

    let mut next = 0;
    let mut post = async |message: String, root_id: &str| -> String {
        next += 1;
        let (status, served, body) = create(
            client,
            base,
            token,
            serde_json::json!({
                "channel_id": channel_id,
                "message": message,
                "root_id": root_id,
                "create_at": at + next * 1000,
            }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base != links_go(), "{base}: served by");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        value["id"].as_str().unwrap().to_owned()
    };

    let og_post = post(format!("{word} og {og}"), "").await;
    let image_post = post(format!("{word} image {image}"), "").await;
    let plain_post = post(format!("{word} plain {plain}"), "").await;
    let none_post = post(format!("{word} refused {none}"), "").await;
    let long_post = post(format!("{word} long {long_url}"), "").await;
    let permalink_post = post(
        format!(
            "{word} permalink {}/{}/pl/{og_post}",
            links_go(),
            f.team_name
        ),
        "",
    )
    .await;
    let reply = post(format!("{word} reply ![md]({image})"), &og_post).await;
    // A PNG behind an `.svg` URL: measured fresh on the create, filtered from the cache and the
    // row alike (`filterSVGImage`) on every read.
    let svg_post = post(
        format!("{word} svg ![s]({}/img/{n}/m.svg)", f.site.base),
        "",
    )
    .await;

    // Flag the OpenGraph post and pin the image post, through the main server.
    let (status, body, _) = ask(
        client,
        common::GO,
        reqwest::Method::PUT,
        token,
        &format!("/api/v4/users/{}/preferences", common::logged_in_user_id()),
        Some(serde_json::json!([{
            "user_id": common::logged_in_user_id(),
            "category": "flagged_post",
            "name": og_post,
            "value": "true",
        }])),
    )
    .await;
    assert_eq!(status, 200, "flagging: {body}");
    let (status, body, _) = ask(
        client,
        common::GO,
        reqwest::Method::POST,
        token,
        &format!("/api/v4/posts/{image_post}/pin"),
        None,
    )
    .await;
    assert_eq!(status, 200, "pinning: {body}");

    withdraw_pages(f, &n);
    f.site.take_seen();
    Planted {
        channel_id,
        team_id,
        word,
        all: vec![
            og_post.clone(),
            image_post.clone(),
            plain_post.clone(),
            none_post.clone(),
            long_post.clone(),
            permalink_post.clone(),
            reply,
            svg_post.clone(),
        ],
        og: og_post,
        image: image_post,
        plain: plain_post,
        none: none_post,
        long: long_post,
        long_url,
        permalink: permalink_post,
        svg: svg_post,
    }
}

/// The fixture's team, by name.
async fn f_team(client: &reqwest::Client, token: &str, f: &Fixture) -> String {
    team_id_of(client, token, &f.channel_id).await
}

/// One read through both servers: equal status and body, served here, and the paths each
/// server fetched from the website while answering.
async fn read_both(
    client: &reqwest::Client,
    token: &str,
    f: &Fixture,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (serde_json::Value, Vec<String>, Vec<String>) {
    f.site.take_seen();
    let go = ask(
        client,
        &links_go(),
        method.clone(),
        token,
        path,
        body.clone(),
    )
    .await;
    let go_seen: Vec<String> = f.site.take_seen().into_iter().map(|s| s.path).collect();
    let ours = ask(client, &f.rust, method, token, path, body).await;
    let our_seen: Vec<String> = f.site.take_seen().into_iter().map(|s| s.path).collect();
    assert_eq!(go.0, 200, "{path}: Go {}", go.1);
    assert!(ours.2, "{path}: must be served here, not forwarded");
    assert_eq!(
        (ours.0, ours.1.as_str()),
        (go.0, go.1.as_str()),
        "{path}: the bodies differ"
    );
    (serde_json::from_str(&go.1).unwrap(), go_seen, our_seen)
}

/// Every read route over one planting. `creator_is_go` says which server's cache is warm, and
/// so which one fetches the long URL on its first read.
async fn read_every_route(
    client: &reqwest::Client,
    token: &str,
    f: &Fixture,
    p: &Planted,
    creator_is_go: bool,
) {
    let long_path = p.long_url.trim_start_matches(&f.site.base).to_owned();

    // The long URL first: the cold process fetches it, once, and caches the answer.
    let (body, go_seen, our_seen) = read_both(
        client,
        token,
        f,
        reqwest::Method::GET,
        &format!("/api/v4/posts/{}", p.long),
        None,
    )
    .await;
    assert_eq!(body["metadata"]["embeds"][0]["type"], "opengraph", "{body}");
    let (warm, cold) = if creator_is_go {
        (&go_seen, &our_seen)
    } else {
        (&our_seen, &go_seen)
    };
    assert!(
        warm.is_empty(),
        "the creating process has it cached: {warm:?}"
    );
    assert_eq!(cold, &vec![long_path], "the other process fetches it once");

    // Then every post, one at a time: answered from the row or the cache, never fetched.
    for (id, expected) in [
        (&p.og, "opengraph"),
        (&p.image, "image"),
        (&p.plain, "link"),
        // The failed fetch left no embed on the create, and a `none` row (or a cached nothing)
        // that every read turns into a plain link.
        (&p.none, "link"),
        (&p.long, "opengraph"),
        (&p.permalink, "permalink"),
    ] {
        let (body, go_seen, our_seen) = read_both(
            client,
            token,
            f,
            reqwest::Method::GET,
            &format!("/api/v4/posts/{id}"),
            None,
        )
        .await;
        assert_eq!(body["metadata"]["embeds"][0]["type"], expected, "{body}");
        assert_eq!((go_seen, our_seen), (vec![], vec![]), "{expected}: fetched");
    }
    let (body, _, _) = read_both(
        client,
        token,
        f,
        reqwest::Method::GET,
        &format!("/api/v4/posts/{}", p.og),
        None,
    )
    .await;
    assert_eq!(
        body["metadata"]["images"].as_object().map(|m| m.len()),
        Some(2),
        "both of the page's measurable images, from their rows: {body}"
    );
    let (body, _, _) = read_both(
        client,
        token,
        f,
        reqwest::Method::GET,
        &format!("/api/v4/posts/{}", p.permalink),
        None,
    )
    .await;
    assert_eq!(
        body["metadata"]["embeds"][0]["data"]["post"]["metadata"]["embeds"][0]["type"], "opengraph",
        "the previewed post carries its own preview, from its row: {body}"
    );

    let (body, go_seen, our_seen) = read_both(
        client,
        token,
        f,
        reqwest::Method::GET,
        &format!("/api/v4/posts/{}", p.svg),
        None,
    )
    .await;
    assert!(
        body["metadata"].get("images").is_none(),
        "an .svg URL is filtered on a read: {body}"
    );
    assert_eq!((go_seen, our_seen), (vec![], vec![]), "svg: fetched");

    let lists = [
        (
            reqwest::Method::GET,
            format!("/api/v4/posts/{}/thread", p.og),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/channels/{}/posts", p.channel_id),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/channels/{}/posts?since=1", p.channel_id),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/channels/{}/pinned", p.channel_id),
            None,
        ),
        (
            reqwest::Method::GET,
            format!(
                "/api/v4/users/{}/posts/flagged?channel_id={}",
                common::logged_in_user_id(),
                p.channel_id
            ),
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/v4/posts/ids".to_owned(),
            Some(serde_json::json!(p.all)),
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/teams/{}/posts/search", p.team_id),
            Some(serde_json::json!({ "terms": p.word })),
        ),
    ];
    for (method, path, body) in lists {
        let (value, go_seen, our_seen) = read_both(client, token, f, method, &path, body).await;
        assert_eq!((go_seen, our_seen), (vec![], vec![]), "{path}: fetched");
        let text = value.to_string();
        // `getPostsByIds` runs `PreparePostForClient` alone: no route previews less.
        let previews = !path.ends_with("/posts/ids");
        assert_eq!(
            text.contains("\"type\":\"opengraph\"") || text.contains("\"type\":\"image\""),
            previews,
            "{path}: whether the list carries a preview: {text}"
        );
    }
}

/// Planted through Go: Go reads from its cache, this server from the rows.
#[tokio::test]
async fn reads_of_posts_go_created_are_answered_from_the_rows() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let planted = plant(&client, &token, f, &links_go(), "g").await;
    read_every_route(&client, &token, f, &planted, true).await;
}

/// Planted through this server: Go reads from the rows, this server from its cache.
#[tokio::test]
async fn reads_of_posts_this_server_created_are_answered_from_the_rows() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let planted = plant(&client, &token, f, &f.rust.clone(), "r").await;
    read_every_route(&client, &token, f, &planted, false).await;
}

/// A viewer who cannot read the previewed post's channel gets neither the embed nor the prop on
/// a read (`SanitizePostMetadataForUser`), and one who can gets both.
#[tokio::test]
async fn a_read_permalink_preview_is_sanitised_for_the_viewer() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();

    let (status, _, body) = create(
        &client,
        &links_go(),
        &token,
        serde_json::json!({ "channel_id": f.private_id, "message": format!("cplr secret {n}") }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let secret: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (status, _, body) = create(
        &client,
        &links_go(),
        &token,
        serde_json::json!({
            "channel_id": f.channel_id,
            "message": format!("cplr points at {}/{}/pl/{}", links_go(), f.team_name, secret["id"].as_str().unwrap()),
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let pointer: serde_json::Value = serde_json::from_str(&body).unwrap();
    let path = format!("/api/v4/posts/{}", pointer["id"].as_str().unwrap());

    let (admin_view, _, _) = read_both(&client, &token, f, reqwest::Method::GET, &path, None).await;
    assert_eq!(admin_view["metadata"]["embeds"][0]["type"], "permalink");
    let (reader_view, _, _) = read_both(
        &client,
        &f.reader.token,
        f,
        reqwest::Method::GET,
        &path,
        None,
    )
    .await;
    assert!(
        reader_view["metadata"].get("embeds").is_none()
            && reader_view["props"].get("previewed_post").is_none(),
        "the reader may not read the private channel: {reader_view}"
    );
}

/// `CreatePost` with a permalink to a post that has a link of its own: the referenced post is
/// prepared on the read path, its preview from its row, and nothing is fetched.
#[tokio::test]
async fn a_permalink_to_a_post_with_a_link_is_created_from_its_row() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    let (og, _, _, _) = serve_pages(f, &n);
    let (status, _, body) = create(
        &client,
        &links_go(),
        &token,
        serde_json::json!({ "channel_id": f.channel_id, "message": format!("cplr target {og}") }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let target: serde_json::Value = serde_json::from_str(&body).unwrap();
    withdraw_pages(f, &n);

    let message = format!(
        "cplr pointing {}/{}/pl/{}",
        links_go(),
        f.team_name,
        target["id"].as_str().unwrap()
    );
    // `compare` asserts the bodies and the (empty) request logs agree.
    let (body, _) = compare(&client, &token, f, &message, &[]).await;
    let preview = &body["metadata"]["embeds"][0];
    assert_eq!(preview["type"], "permalink", "{body}");
    assert_eq!(
        preview["data"]["post"]["metadata"]["embeds"][0]["type"], "opengraph",
        "{body}"
    );
    assert!(f.site.take_seen().is_empty(), "nothing is fetched");
}

/// The rows an edit of `post_id` left in the history.
async fn history_rows(post_id: &str) -> i64 {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_scalar("SELECT count(*) FROM posts WHERE originalid = $1")
        .bind(post_id)
        .fetch_one(&pool)
        .await
        .expect("the history count")
}

/// An edit that adds a link is served, its preview from the row; one whose first link is a
/// permalink is forwarded **before** the write, so only Go writes it.
#[tokio::test]
async fn an_edit_with_a_link_is_served_and_one_with_a_permalink_forwarded_before_its_write() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let n = nonce();
    let (og, _, _, _) = serve_pages(f, &n);
    let at = create_at();
    // The row the edits read: a post linking to the page, in the same hour.
    let (status, _, body) = create(
        &client,
        &links_go(),
        &token,
        serde_json::json!({ "channel_id": f.channel_id, "message": format!("cplr seed {og}"), "create_at": at }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let seed_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    withdraw_pages(f, &n);
    f.site.take_seen();

    let mut bodies = Vec::new();
    for base in [links_go(), f.rust.clone()] {
        let (status, _, body) = create(
            &client,
            common::GO,
            &token,
            serde_json::json!({ "channel_id": f.channel_id, "message": "cplr to edit", "create_at": at + 1000 }),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (status, body, served) = ask(
            &client,
            &base,
            reqwest::Method::PUT,
            &token,
            &format!("/api/v4/posts/{id}/patch"),
            Some(serde_json::json!({ "message": format!("cplr edited {og}") })),
        )
        .await;
        assert_eq!(status, 200, "{base}: {body}");
        assert_eq!(served, base == f.rust, "{base}: served by");
        assert!(f.site.take_seen().is_empty(), "{base}: the edit fetched");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value["metadata"]["embeds"][0]["type"], "opengraph",
            "{value}"
        );
        let edit_at = value["edit_at"].as_i64().unwrap();
        bodies.push(normalised_text(&body).replacen(
            &format!("\"edit_at\":{edit_at}"),
            "\"edit_at\":0",
            1,
        ));

        let (status, body, served) = ask(
            &client,
            &base,
            reqwest::Method::PUT,
            &token,
            &format!("/api/v4/posts/{id}/patch"),
            Some(serde_json::json!({
                "message": format!("cplr now a permalink {}/{}/pl/{id}", links_go(), f.team_name)
            })),
        )
        .await;
        assert_eq!(status, 200, "{base}: {body}");
        assert!(!served, "{base}: a permalink edit is Go's");
        // Go's own count is three: the edit, then `addPostPreviewProp`'s second `Update` for
        // the preview. A write here before forwarding would make it four.
        assert_eq!(
            history_rows(&id).await,
            3,
            "{base}: the permalink edit is written once, by Go"
        );
    }
    assert_eq!(bodies[0], bodies[1], "the edit bodies differ");

    // A post carrying `previewed_post` forwards whatever its new first link: Go's publish takes
    // the prop off the answer, which the edit path here does not reproduce.
    let (status, _, body) = create(
        &client,
        &links_go(),
        &token,
        serde_json::json!({
            "channel_id": f.channel_id,
            "message": format!("cplr carries a preview {}/{}/pl/{seed_id}", links_go(), f.team_name),
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let carrier = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        saved_props(&carrier).await["previewed_post"],
        seed_id.as_str()
    );
    let (status, body, served) = ask(
        &client,
        &f.rust,
        reqwest::Method::PUT,
        &token,
        &format!("/api/v4/posts/{carrier}/patch"),
        Some(serde_json::json!({ "message": format!("cplr no longer a permalink {og}") })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!served, "an edit of a post carrying previewed_post is Go's");
}

/// `createEphemeralPost` prepares the post a second time in the handler, and
/// `getEmbedsAndImages` appends: the answer carries the embed twice, the second from the link
/// cache the first pass filled, so each server fetches the page and its images once.
#[tokio::test]
async fn an_ephemeral_post_with_a_link_is_prepared_twice() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut texts = Vec::new();
    let mut fetched = Vec::new();
    for base in [links_go(), f.rust.clone()] {
        // A page of its own per server: neither process's cache has seen it.
        let n = nonce();
        let (og, _, _, _) = serve_pages(f, &n);
        f.site.take_seen();
        let (status, body, served) = ask(
            &client,
            &base,
            reqwest::Method::POST,
            &token,
            "/api/v4/posts/ephemeral",
            Some(serde_json::json!({
                "user_id": common::logged_in_user_id(),
                "post": { "channel_id": f.channel_id, "message": format!("cplr ephemeral {og}") },
            })),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == f.rust, "{base}: served by");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value["metadata"]["embeds"].as_array().map(Vec::len),
            Some(2),
            "{base}: {value}"
        );
        texts.push(normalised_text(&body).replace(&n, "N"));
        fetched.push(
            f.site
                .take_seen()
                .into_iter()
                .map(|s| s.path.replace(&n, "N"))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(texts[0], texts[1], "the ephemeral answers differ");
    assert_eq!(
        fetched[0], fetched[1],
        "the two servers fetched differently"
    );
}

/// A stored `previewed_post` prop bypasses both the link cache and the `LinkMetadata` row, so a
/// post that carries one while its first link is an ordinary URL — an edit through Go that kept
/// the prop — is fetched afresh on every read, by both servers, and the fetch finds the page
/// withdrawn.
#[tokio::test]
async fn a_previewed_post_prop_bypasses_the_cache_and_the_row() {
    if !stack_enabled() {
        return;
    }
    let _website = WEBSITE.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    // Created through each server in turn: the creator has the link cached, the other has only
    // the row, and the prop must bypass both.
    for creator in [links_go(), f.rust.clone()] {
        let n = nonce();
        let (og, _, _, _) = serve_pages(f, &n);
        let (status, _, body) = create(
        &client,
        &creator,
        &token,
        serde_json::json!({ "channel_id": f.channel_id, "message": format!("cplr bypass {og}") }),
    )
    .await;
        assert_eq!(status, 201, "{body}");
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["metadata"]["embeds"][0]["type"], "opengraph");
        let id = created["id"].as_str().unwrap().to_owned();
        let pool = fixture_pool().await.expect("DATABASE_URL");
        sqlx::query("UPDATE posts SET props = $1 WHERE id = $2")
            .bind(serde_json::json!({ "previewed_post": id }))
            .bind(&id)
            .execute(&pool)
            .await
            .expect("planting the prop");
        withdraw_pages(f, &n);

        let (body, go_seen, our_seen) = read_both(
            &client,
            &token,
            f,
            reqwest::Method::GET,
            &format!("/api/v4/posts/{id}"),
            None,
        )
        .await;
        assert_eq!(
            body["metadata"]["embeds"][0]["type"], "link",
            "the withdrawn page, fetched: {body}"
        );
        assert_eq!(go_seen, our_seen, "both servers fetch the same");
        assert!(
            go_seen.contains(&format!("/og/{n}")),
            "the page is fetched: {go_seen:?}"
        );
    }
}
