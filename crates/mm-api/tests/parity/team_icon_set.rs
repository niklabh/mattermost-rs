//! Cross-server parity for `POST /api/v4/teams/{team_id}/image` — the refusals this server
//! answers, and the upload it hands to Go before anything is written.
//!
//! ```sh
//! scripts/parity.sh --test parity team_icon_set
//! ```

use crate::common;

use common::{
    GO, RUST, TINY_PNG, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    create_team, fixture_pool, go_minted_token, stack_enabled,
};

const BOUNDARY: &str = "mmrsteamiconboundary";

fn multipart_body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for (name, filename, bytes) in parts {
        let disposition = match filename {
            Some(filename) => format!("form-data; name=\"{name}\"; filename=\"{filename}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: {disposition}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    team_id: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, Option<String>, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/teams/{team_id}/image"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

async fn both_refuse(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    content_type: &str,
    body: &[u8],
    status: u16,
    id: &str,
) {
    let (go_status, _, go) = post(client, GO, token, team_id, content_type, body.to_vec()).await;
    let (rs_status, served, rs) =
        post(client, RUST, token, team_id, content_type, body.to_vec()).await;
    assert_eq!(
        go_status,
        status,
        "Go {id}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {id}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(served.as_deref(), Some("rust"), "{id}: served here");
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], id);
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/teams/{team_id}/image");
}

fn multipart() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// A `Content-Length` of 200 MiB over a short body, written onto a raw socket so the framing
/// is this test's choice: the declared-length branch, reachable without moving 200 MiB. The
/// write half is closed after the body so a server that read on would see EOF rather than hang.
/// Returns `(status, x-mmrs-served-by, body)`.
async fn declared_oversize_post(
    base: &str,
    token: &str,
    team_id: &str,
    body: &[u8],
) -> (u16, Option<String>, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = base
        .rsplit_once('/')
        .map(|(_, authority)| authority)
        .unwrap_or(base);
    let path = format!("/api/v4/teams/{team_id}/image");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: {}\r\nContent-Length: 209715201\r\nConnection: close\r\n\r\n",
        multipart(),
    );
    let raw = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(authority)
            .await
            .unwrap_or_else(|e| panic!("{authority} refuses a socket: {e}"));
        stream.write_all(request.as_bytes()).await.expect("headers");
        stream.write_all(body).await.expect("the body");
        stream.shutdown().await.expect("the write half closes");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .expect("the answer reads");
        raw
    })
    .await
    .unwrap_or_else(|_| panic!("{base}{path} never answered; it waited for the body"));

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("a status line");
    let served = lines
        .filter_map(|line| line.split_once(": "))
        .find(|(name, _)| name.eq_ignore_ascii_case("x-mmrs-served-by"))
        .map(|(_, value)| value.trim().to_owned());
    (status, served, raw[split + 4..].to_vec())
}

/// The refusals in order — the id, the permission (an unknown team included), the parse, the
/// missing part, the admin's unknown team as the 400 — and the accepted upload handed to Go,
/// which writes the icon and stamps the team.
#[tokio::test]
async fn the_refusals_are_served_and_the_upload_is_gos() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "tis").await;
    let member = create_plain_user(&client, &admin, &team, "tis").await;
    let nobody = mm_model::utils::new_id();
    let image = multipart_body(&[("image", Some("p.png"), TINY_PNG)]);

    both_refuse(
        &client,
        &member.token,
        "abc",
        &multipart(),
        &image,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;
    both_refuse(
        &client,
        &member.token,
        &team,
        &multipart(),
        &image,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both_refuse(
        &client,
        &member.token,
        &nobody,
        &multipart(),
        &image,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both_refuse(
        &client,
        &admin,
        &team,
        "application/json",
        b"{}",
        400,
        "api.team.set_team_icon.parse.app_error",
    )
    .await;
    both_refuse(
        &client,
        &admin,
        &team,
        &multipart(),
        &multipart_body(&[("picture", Some("p.png"), TINY_PNG)]),
        400,
        "api.team.set_team_icon.no_file.app_error",
    )
    .await;
    // The declared length is checked before the body is read: a **400** here, where the
    // profile route answers 413 for the same thing.
    let (go_status, _, go) = declared_oversize_post(GO, &admin, &team, &image).await;
    let (rs_status, served, rs) = declared_oversize_post(RUST, &admin, &team, &image).await;
    assert_eq!(go_status, 400, "Go: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 400, "Rust: {}", String::from_utf8_lossy(&rs));
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], "api.team.set_team_icon.too_large.app_error");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/teams/{team_id}/image");
    // The missing part is refused before the team is looked up.
    both_refuse(
        &client,
        &admin,
        &nobody,
        &multipart(),
        &multipart_body(&[("picture", Some("p.png"), TINY_PNG)]),
        400,
        "api.team.set_team_icon.no_file.app_error",
    )
    .await;
    // The admin passes the permission for any team id, so the team lookup's 400 is reachable.
    both_refuse(
        &client,
        &admin,
        &nobody,
        &multipart(),
        &image,
        400,
        "api.team.set_team_icon.get_team.app_error",
    )
    .await;

    // The accepted upload: forwarded, and Go writes it.
    let before: i64 = sqlx::query_scalar("SELECT lastteamiconupdate FROM teams WHERE id = $1")
        .bind(&team)
        .fetch_one(&pool)
        .await
        .expect("the team row");
    assert_eq!(before, 0);
    let (status, served, body) =
        post(&client, RUST, &admin, &team, &multipart(), image.clone()).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(served.as_deref(), Some("go"), "the write is Go's");
    assert_eq!(body, br#"{"status":"OK"}"#);
    let after: i64 = sqlx::query_scalar("SELECT lastteamiconupdate FROM teams WHERE id = $1")
        .bind(&team)
        .fetch_one(&pool)
        .await
        .expect("the team row");
    assert!(after > 0, "Go stamped the icon");
}
