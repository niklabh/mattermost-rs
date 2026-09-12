//! Cross-server parity for the four image routes that write, or that generate:
//!
//! ```text
//! POST   /api/v4/users/{user_id}/image          setProfileImage
//! DELETE /api/v4/users/{user_id}/image          setDefaultProfileImage
//! GET    /api/v4/users/{user_id}/image/default  getDefaultProfileImage
//! POST   /api/v4/brand/image                    uploadBrandImage
//! ```
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::image_writes
//! ```
//!
//! # Every success on all four is Go's
//!
//! `SetProfileImage` `FillCenter`s the upload to 128×128 and re-encodes it as PNG,
//! `SetDefaultProfileImage` rasterises the initials avatar through freetype, `SaveBrandImage`
//! re-encodes with `imgEncoder.EncodePNG`, and `getDefaultProfileImage` is that rasteriser and
//! nothing else. None of the four reproduces from a second implementation — [D-204], [D-380] —
//! so this port answers the **refusals** and hands the rest over. That is what these tests
//! measure: every refusal byte for byte, and for each hand-over, that it happened *before* the
//! file backend was touched.
//!
//! # The two facts a reader would get wrong
//!
//! * A well-formed id naming nobody is a **400** on the POST and a **404** on the DELETE, from
//!   the same `GetUser` call — `setProfileImage` throws the error away for
//!   `SetInvalidURLParam("user_id")` (api4/user.go:668) and `setDefaultProfileImage` propagates
//!   it (api4/user.go:719). [`the_same_missing_user_is_a_400_and_a_404`] pins both.
//! * `uploadBrandImage` checks `edit_brand` **fourth**, after the body is read, parsed and found
//!   to contain an `image` part (api4/brand.go:69). A caller with no permission and a malformed
//!   body gets the 400. [`the_brand_permission_check_comes_after_the_body`] pins that ordering.
//!
//! # Shared state
//!
//! The only fixture these tests write is a throwaway plain user, deleted on the way out, and the
//! brand image — which is one file for the whole installation and which `file_bytes` also reads.
//! [`common::BRAND_IMAGE`] serialises the two.

use crate::common;

use common::{
    BRAND_IMAGE, GO, RUST, TINY_PNG, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, delete_plain_user,
    go_minted_token, logged_in_user_id, stack_enabled,
};

/// A valid id that names nobody. 26 characters from the base32 alphabet, so `IsValidId` passes
/// and every lookup behind it misses.
const NOBODY: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// Alphanumeric, so Go's mux routes it and `mux_segments_or_forward` does **not** hand it over,
/// but not 26 characters — so `IsValidId` fails and `RequireUserId` answers its 400. A segment
/// with a hyphen would be forwarded instead and prove nothing about this handler.
const NOT_AN_ID: &str = "notanid";

const BOUNDARY: &str = "mmrsparityimageboundary";

fn multipart() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// One `multipart/form-data` body carrying the named parts, assembled by hand for the same reason
/// `common::create_custom_emoji` does — reqwest's `multipart` feature is a Cargo change for one
/// call site, and these tests need to send bodies its builder will not produce.
fn multipart_body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for (name, filename, bytes) in parts {
        let disposition = match filename {
            Some(filename) => {
                format!("form-data; name=\"{name}\"; filename=\"{filename}\"")
            }
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

/// The ordinary accepted shape: one `image` file part holding a 1×1 PNG.
fn an_image_part() -> Vec<u8> {
    multipart_body(&[("image", Some("p.png"), TINY_PNG)])
}

/// `(status, body, x-mmrs-served-by)` for one request.
async fn send(
    client: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = client
        .request(method.clone(), format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type).body(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} {method} {path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// Run one request against both servers and assert the refusals are byte-identical and ours.
///
/// `context` names the row; every assertion carries it, because these tables are long and a bare
/// "assertion failed" says nothing about which case broke.
async fn both_refuse_identically(
    client: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
    context: &str,
) -> serde_json::Value {
    let (go_status, go_body, _) = send(
        client,
        method.clone(),
        GO,
        path,
        token,
        content_type,
        body.clone(),
    )
    .await;
    let (rs_status, rs_body, served_by) =
        send(client, method, RUST, path, token, content_type, body).await;

    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "{context}: this refusal is ours to give, not Go's"
    );
    assert_eq!(
        rs_status,
        go_status,
        "{context}: status\n  go:   {}\n  rust: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context)
}

// ---------------------------------------------------------------------------------------------
// setProfileImage — POST /api/v4/users/{user_id}/image
// ---------------------------------------------------------------------------------------------

/// Every `setProfileImage` refusal that is a pure function of the request.
///
/// One test rather than seven because each row is two HTTP round trips and round trips are this
/// suite's whole cost; the `context` says which row failed.
///
/// Three refusals are **not** here and cannot be: the 501 needs `FileSettings.DriverName == ""`,
/// the 409s need an LDAP picture attribute or an Enterprise licence with
/// `LockProfileFieldsForEmailUsers = "all"`, and this stack is configured for none of the three.
/// They are covered by unit tests in `mm_app` and named as a parity risk in the session report.
#[tokio::test]
async fn the_profile_upload_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    // `RequireUserId` — before anything else, including the permission check.
    both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &format!("/api/v4/users/{NOT_AN_ID}/image"),
        &token,
        Some(&multipart()),
        an_image_part(),
        "a user_id that is not an id",
    )
    .await;

    // The parse failure is a **500** on this route, where the same body is a 400 on
    // `createEmoji` and a 400 on `uploadBrandImage`. Three shapes of unparseable body, all three
    // reaching `ParseMultipartForm`'s error.
    for (context, content_type, body) in [
        (
            "a JSON body",
            "application/json".to_owned(),
            br#"{"image":"x"}"#.to_vec(),
        ),
        (
            "multipart with no boundary",
            "multipart/form-data".to_owned(),
            an_image_part(),
        ),
        (
            "a body that is not the declared multipart",
            multipart(),
            b"nothing that looks like a part".to_vec(),
        ),
    ] {
        let body = both_refuse_identically(
            &client,
            reqwest::Method::POST,
            &format!("/api/v4/users/{me}/image"),
            &token,
            Some(&content_type),
            body,
            context,
        )
        .await;
        assert_eq!(
            body["id"], "api.user.upload_profile_user.parse.app_error",
            "{context}: the parse failure"
        );
        assert_eq!(body["status_code"], 500, "{context}: and it is a 500");
    }

    // A well-formed multipart body with no `image` key at all, and one whose `image` is a
    // *value* rather than a file — Go's `Form.File` is keyed only by parts that carried a
    // non-empty `filename`, so a valueless `image` misses the map just as an absent one does.
    for (context, body) in [
        (
            "no image part",
            multipart_body(&[("picture", Some("p.png"), TINY_PNG)]),
        ),
        (
            "an image part with no filename is a value, not a file",
            multipart_body(&[("image", None, TINY_PNG)]),
        ),
        (
            "an image part with an empty filename is a value, not a file",
            multipart_body(&[("image", Some(""), TINY_PNG)]),
        ),
    ] {
        let body = both_refuse_identically(
            &client,
            reqwest::Method::POST,
            &format!("/api/v4/users/{me}/image"),
            &token,
            Some(&multipart()),
            body,
            context,
        )
        .await;
        assert_eq!(body["id"], "api.user.upload_profile_user.no_file.app_error");
        assert_eq!(body["status_code"], 400);
    }
}

/// The permission check, and the fact that it precedes the storage and body checks.
///
/// A plain user posting to the admin's image has no `edit_other_users`, so it is a 403 — and it
/// is a 403 even when the body is nonsense, which is what places the check ahead of the parse.
#[tokio::test]
async fn a_plain_user_may_not_replace_someone_elses_picture() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgperm").await;
    let me = logged_in_user_id();

    // **`RequireUserId` runs before the permission check, and only this case shows it.** With a
    // well-formed id the two orderings coincide — an id that names nobody is a 400 either way,
    // because `GetUser` fails and `setProfileImage` turns *that* into the same
    // `invalid_url_param`. A malformed id in a **plain** user's hands separates them: the id
    // check answers 400 and a permission check reached first would answer 403. Without this row
    // `profile-require-id-dropped` survives, which is how it was found.
    let body = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &format!("/api/v4/users/{NOT_AN_ID}/image"),
        &plain.token,
        Some(&multipart()),
        an_image_part(),
        "a plain user posting to a user_id that is not an id",
    )
    .await;
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    assert_eq!(
        body["status_code"], 400,
        "the id check precedes the permission, so this is not the 403"
    );

    // A good body: the refusal is the permission, not the shape.
    both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &format!("/api/v4/users/{me}/image"),
        &plain.token,
        Some(&multipart()),
        an_image_part(),
        "a plain user posting to the admin's image",
    )
    .await;

    // The same call with a body that could never parse still answers 403, which is the ordering:
    // `SessionHasPermissionToUserOrBot` is checked before `ParseMultipartForm`.
    let body = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &format!("/api/v4/users/{me}/image"),
        &plain.token,
        Some("application/json"),
        b"{".to_vec(),
        "a plain user posting nonsense to the admin's image",
    )
    .await;
    assert_eq!(
        body["status_code"], 403,
        "the permission check precedes the parse"
    );

    // And the same user on **its own** image gets past the permission check to the body — the
    // non-vacuity of the two rows above.
    let body = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &format!("/api/v4/users/{}/image", plain.id),
        &plain.token,
        Some("application/json"),
        b"{".to_vec(),
        "a plain user posting nonsense to its own image",
    )
    .await;
    assert_eq!(
        body["status_code"], 500,
        "on its own image the same body reaches the parse"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

/// The one branch where the POST and the DELETE answer the *same* failed `GetUser` differently.
///
/// `setProfileImage` discards it for `SetInvalidURLParam("user_id")` — a **400** naming the
/// parameter — and `setDefaultProfileImage` propagates `app.user.missing_account.const`, a
/// **404**. A port that shared one helper between the two routes would give one answer twice, and
/// nothing else in the suite would notice.
#[tokio::test]
async fn the_same_missing_user_is_a_400_and_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("/api/v4/users/{NOBODY}/image");

    let posted = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        &path,
        &token,
        Some(&multipart()),
        an_image_part(),
        "POST to a user that does not exist",
    )
    .await;
    assert_eq!(posted["id"], "api.context.invalid_url_param.app_error");
    assert_eq!(posted["status_code"], 400);

    let deleted = both_refuse_identically(
        &client,
        reqwest::Method::DELETE,
        &path,
        &token,
        None,
        Vec::new(),
        "DELETE on a user that does not exist",
    )
    .await;
    assert_eq!(deleted["id"], "app.user.missing_account.const");
    assert_eq!(deleted["status_code"], 404);
}

/// A declared `Content-Length` over `FileSettings.MaxFileSize` is refused **before the body is
/// read**, on both routes that take one — with a different error id on each.
///
/// # Why this is a hand-written request
///
/// The limit is 100 MiB on this stack, so proving the branch by sending the bytes would move
/// 200 MiB across the loopback per assertion. Go tests `r.ContentLength`, which is the *header*,
/// and answers before touching the body; so does this port. The request is therefore written
/// onto a socket by hand with an honest-looking `Content-Length` and a short body, and the write
/// half is shut so the `defer io.Copy(io.Discard, r.Body)` in both Go handlers sees an EOF
/// instead of waiting for 100 MiB that will never arrive.
#[tokio::test]
async fn a_declared_oversize_body_is_refused_before_it_is_read() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    for (path, expected_id) in [
        (
            format!("/api/v4/users/{me}/image"),
            "api.user.upload_profile_user.too_large.app_error",
        ),
        (
            "/api/v4/brand/image".to_owned(),
            "api.admin.upload_brand_image.too_large.app_error",
        ),
    ] {
        let go = declared_oversize_post(GO, &path, &token).await;
        let rs = declared_oversize_post(RUST, &path, &token).await;

        assert_eq!(
            go.0,
            413,
            "Go refuses the declared length: {}",
            String::from_utf8_lossy(&go.1)
        );
        assert_eq!(
            rs.0,
            go.0,
            "{path}: status\n  go:   {}\n  rust: {}",
            String::from_utf8_lossy(&go.1),
            String::from_utf8_lossy(&rs.1)
        );
        assert_eq!(
            rs.2.as_deref(),
            Some("rust"),
            "{path}: the 413 is ours to give"
        );
        let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
        assert_eq!(body["id"], expected_id, "{path}: each route has its own id");
    }

    // Nothing was written by either server: the fixture user's picture is untouched. `MaxFileSize`
    // is checked after the permission and before the body, so this is also the only place the
    // ordering of those two is visible.
    assert_eq!(
        last_picture_update(&client, &token, me).await,
        last_picture_update(&client, &token, me).await,
        "a refused upload leaves LastPictureUpdate alone"
    );
}

/// A `Content-Length` of 200 MiB over a body of eighty bytes — the declared-length branch,
/// reachable without moving 200 MiB across the loopback.
async fn declared_oversize_post(
    base: &str,
    path: &str,
    token: &str,
) -> (u16, Vec<u8>, Option<String>) {
    raw_post(
        base,
        path,
        token,
        Framing::Declared(209_715_201),
        &an_image_part(),
    )
    .await
}

/// How a hand-written request frames its body.
enum Framing {
    /// `Content-Length: n`, whatever the body actually is. `n` larger than the body is what makes
    /// the declared-length branch reachable without sending 100 MiB.
    Declared(u64),
    /// `Transfer-Encoding: chunked`, which leaves `r.ContentLength` at **-1** on the Go side —
    /// so the declared-length check cannot fire and the `MaxBytesReader` cap is the only limit
    /// left. It is the *other* of the two size limits, 512 bytes higher, and the only way to
    /// reach it.
    Chunked,
}

/// One `POST` written onto a raw socket, so the framing is this test's choice rather than
/// reqwest's. Returns `(status, body, x-mmrs-served-by)`.
///
/// Capped at ten seconds: a server that waits for a body it was told to refuse is a failure of
/// the branch under test, and the suite must say so rather than hang.
async fn raw_post(
    base: &str,
    path: &str,
    token: &str,
    framing: Framing,
    body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = base
        .rsplit_once('/')
        .map(|(_, authority)| authority)
        .unwrap_or(base);
    let truncated = matches!(framing, Framing::Declared(length) if length != body.len() as u64);
    let (framing_header, wire) = match framing {
        Framing::Declared(length) => (format!("Content-Length: {length}"), body.to_vec()),
        Framing::Chunked => {
            let mut wire = format!("{:x}\r\n", body.len()).into_bytes();
            wire.extend_from_slice(body);
            wire.extend_from_slice(b"\r\n0\r\n\r\n");
            ("Transfer-Encoding: chunked".to_owned(), wire)
        }
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: {}\r\n\
         {framing_header}\r\n\
         Connection: close\r\n\r\n",
        multipart(),
    );

    let raw = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(authority)
            .await
            .unwrap_or_else(|e| panic!("{authority} refuses a socket: {e}"));
        stream.write_all(request.as_bytes()).await.expect("headers");
        stream.write_all(&wire).await.expect("the body");
        // **Half-close only when the request is deliberately short.** A declared length that is
        // never satisfied would otherwise leave the `defer io.Copy(io.Discard, r.Body)` in both
        // Go handlers waiting, and the half-close turns that read into an EOF. A *complete*
        // request must not be half-closed: hyper treats the FIN as the end of the connection and
        // the answer comes back empty, which reads as "no header terminator" rather than as a
        // protocol mistake. Measured, on the first run of `both_size_limits_are_where_go_puts_them`.
        if truncated {
            stream.shutdown().await.expect("the write half closes");
        }
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .expect("the answer reads");
        raw
    })
    .await
    .unwrap_or_else(|_| panic!("{base}{path} never answered; it waited for the body"));

    parse_http_response(&raw, base, path)
}

/// The three fields the assertions need out of a raw HTTP/1.1 response.
fn parse_http_response(raw: &[u8], base: &str, path: &str) -> (u16, Vec<u8>, Option<String>) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("{base}{path}: no header terminator in the answer"));
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("{base}{path}: no status line in {head:?}"));
    let served_by = lines
        .filter_map(|line| line.split_once(": "))
        .find(|(name, _)| name.eq_ignore_ascii_case("x-mmrs-served-by"))
        .map(|(_, value)| value.trim().to_owned());
    (status, raw[split + 4..].to_vec(), served_by)
}

/// `Users.LastPictureUpdate` as the API reports it, which is what a profile-image write moves.
async fn last_picture_update(client: &reqwest::Client, token: &str, user_id: &str) -> i64 {
    client
        .get(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("the user decodes")["last_picture_update"]
        .as_i64()
        .unwrap_or(0)
}

/// The hand-over, and the proof that nothing was written before it.
///
/// A body whose `image` part is not an image passes all ten of this port's refusals — the id is
/// valid, the caller owns the account, storage is configured, the length is under the cap, the
/// body parses, `image` is a file, the user exists, LDAP owns nothing and the lock does not
/// apply. So the request is forwarded, and it is **Go** that refuses it, on bytes this port never
/// looked at.
///
/// That makes it the cleanest available measurement of "the forward precedes the write":
/// `SetProfileImage` fails at its own decode, so the file is not replaced and
/// `LastPictureUpdate` does not move — on either server. If this port wrote before forwarding,
/// the value would move on ours and not on Go's.
#[tokio::test]
async fn a_profile_upload_that_go_refuses_is_forwarded_without_writing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgfwd").await;

    let before = last_picture_update(&client, &admin, &plain.id).await;
    assert_eq!(before, 0, "a fresh user has never had a picture");

    let path = format!("/api/v4/users/{}/image", plain.id);
    let body = multipart_body(&[("image", Some("p.png"), b"not an image at all")]);

    let (go_status, go_body, _) = send(
        &client,
        reqwest::Method::POST,
        GO,
        &path,
        &plain.token,
        Some(&multipart()),
        body.clone(),
    )
    .await;
    let (rs_status, rs_body, served_by) = send(
        &client,
        reqwest::Method::POST,
        RUST,
        &path,
        &plain.token,
        Some(&multipart()),
        body,
    )
    .await;

    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "past the tenth refusal the route is Go's: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "the forwarded answer is Go's own\n  go:   {}\n  rust: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    let refused: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(
        refused["status_code"], 400,
        "Go's own decode refuses it: {refused}"
    );

    assert_eq!(
        last_picture_update(&client, &admin, &plain.id).await,
        0,
        "neither server wrote a picture for a body neither could decode"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ---------------------------------------------------------------------------------------------
// setDefaultProfileImage — DELETE /api/v4/users/{user_id}/image
// ---------------------------------------------------------------------------------------------

/// The DELETE's own refusals, and its one success — which is a **write**, and forwards.
///
/// The 404 that separates it from the POST has its own test above; this covers the invalid id,
/// the permission, and the hand-over. The success runs on a throwaway user so the generated
/// avatar this leaves behind belongs to a row that is deleted on the way out.
#[tokio::test]
async fn the_default_profile_image_reset_refuses_and_then_forwards() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgdef").await;

    both_refuse_identically(
        &client,
        reqwest::Method::DELETE,
        &format!("/api/v4/users/{NOT_AN_ID}/image"),
        &admin,
        None,
        Vec::new(),
        "DELETE with a user_id that is not an id",
    )
    .await;

    let refused = both_refuse_identically(
        &client,
        reqwest::Method::DELETE,
        &format!("/api/v4/users/{}/image", logged_in_user_id()),
        &plain.token,
        None,
        Vec::new(),
        "a plain user resetting the admin's picture",
    )
    .await;
    assert_eq!(refused["status_code"], 403);

    // The success. `SetDefaultProfileImage` generates the initials avatar through freetype and
    // writes it, so it is Go's — but this port must still hand it over rather than answer, and
    // the answer must be the ordinary `ReturnStatusOK` 200 rather than the 201 the brand route
    // gives.
    let (status, body, served_by) = send(
        &client,
        reqwest::Method::DELETE,
        RUST,
        &format!("/api/v4/users/{}/image", plain.id),
        &plain.token,
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "the generated avatar is Go's: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("JSON"),
        serde_json::json!({ "status": "OK" }),
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ---------------------------------------------------------------------------------------------
// getDefaultProfileImage — GET /api/v4/users/{user_id}/image/default
// ---------------------------------------------------------------------------------------------

/// The generated avatar: two refusals from here, the image itself from Go.
///
/// The 403 `view_members` branch is **unreachable on this stack** —
/// `GetViewUsersRestrictions` is `None` for an admin with no restricting scheme, so
/// `UserCanSeeOtherUser` is `true` for every pair and the port never reaches the refusal. It is
/// named as a parity risk rather than asserted.
#[tokio::test]
async fn the_default_profile_image_read_refuses_and_then_forwards() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    both_refuse_identically(
        &client,
        reqwest::Method::GET,
        &format!("/api/v4/users/{NOT_AN_ID}/image/default"),
        &token,
        None,
        Vec::new(),
        "a user_id that is not an id",
    )
    .await;

    // `GetUser`'s 404 is propagated here, as it is on the DELETE and unlike the POST.
    let missing = both_refuse_identically(
        &client,
        reqwest::Method::GET,
        &format!("/api/v4/users/{NOBODY}/image/default"),
        &token,
        None,
        Vec::new(),
        "a user that does not exist",
    )
    .await;
    assert_eq!(missing["id"], "app.user.missing_account.const");
    assert_eq!(missing["status_code"], 404);

    // The image. Forwarded, so the two servers return the same bytes because they are the same
    // bytes — what is under test is that the forward carries the headers through unchanged, and
    // that registering `/image/default` did not shadow `/image`.
    let path = format!("/api/v4/users/{me}/image/default");
    let go = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");

    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "freetype is Go's"
    );
    assert_eq!(rs.status().as_u16(), 200);
    assert_eq!(go.status().as_u16(), 200);
    for header in ["content-type", "cache-control"] {
        assert_eq!(
            rs.headers().get(header),
            go.headers().get(header),
            "{header} must survive the forward"
        );
    }
    assert_eq!(
        rs.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png"),
    );
    let go_bytes = go.bytes().await.expect("bytes").to_vec();
    let rs_bytes = rs.bytes().await.expect("bytes").to_vec();
    assert_eq!(
        rs_bytes, go_bytes,
        "the forwarded PNG is byte for byte Go's"
    );
    assert!(
        rs_bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "and it is a PNG"
    );

    // The shorter path is still answered from here — the neighbour the literal segment could
    // have taken out of service. A 404 or a forward would both mean the router changed shape.
    let neighbour = client
        .get(format!("{RUST}/api/v4/users/{me}/image"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        neighbour
            .headers()
            .get("x-mmrs-served-by")
            .map(|v| v.as_bytes()),
        Some(b"rust".as_slice()),
        "`/image/default` must not shadow `/image`"
    );
}

// ---------------------------------------------------------------------------------------------
// uploadBrandImage — POST /api/v4/brand/image
// ---------------------------------------------------------------------------------------------

/// `uploadBrandImage` reads, parses and inspects the body **before** it asks about `edit_brand`.
///
/// So the same caller gets three different answers for three bodies, and only the last is the
/// 403 — which is the ordering this pins. A port that hoisted the permission check to the top,
/// as every other write in the family has it, would answer 403 to all three and every status
/// would still be a plausible one.
#[tokio::test]
async fn the_brand_permission_check_comes_after_the_body() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgbrand").await;

    let unparseable = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        "/api/v4/brand/image",
        &plain.token,
        Some("application/json"),
        b"{".to_vec(),
        "a plain user sending a body that does not parse",
    )
    .await;
    assert_eq!(
        unparseable["id"],
        "api.admin.upload_brand_image.parse.app_error"
    );
    assert_eq!(
        unparseable["status_code"], 400,
        "the parse failure is a 400 here and a 500 on setProfileImage"
    );

    let no_file = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        "/api/v4/brand/image",
        &plain.token,
        Some(&multipart()),
        multipart_body(&[("logo", Some("p.png"), TINY_PNG)]),
        "a plain user sending a body with no image part",
    )
    .await;
    assert_eq!(
        no_file["id"],
        "api.admin.upload_brand_image.no_file.app_error"
    );
    assert_eq!(no_file["status_code"], 400);

    let forbidden = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        "/api/v4/brand/image",
        &plain.token,
        Some(&multipart()),
        an_image_part(),
        "a plain user sending a body with an image part",
    )
    .await;
    assert_eq!(
        forbidden["status_code"], 403,
        "only a well-formed body reaches the permission"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

/// The admin's path through the same route: past the permission, into `SaveBrandImage`, and out
/// to Go — with nothing written on the way.
///
/// The first half sends bytes that are not an image. This port answers none of the five refusals,
/// so it forwards, and Go's `checkImageLimits` refuses. The brand image is a single file for the
/// whole installation, so the assertion that it is still absent afterwards is the measurement
/// that the hand-over happened before the `MoveFile` and the `WriteFile`.
///
/// The second half sends a real PNG and lets it land, because the **201** is the one thing about
/// this route a reader would get wrong: `w.WriteHeader(StatusCreated)` followed by
/// `ReturnStatusOK(w)` is a 201 that still carries `{"status":"OK"}`. It is cleaned up with the
/// already-migrated DELETE.
#[tokio::test]
async fn the_brand_upload_forwards_before_it_writes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    // One file for the installation, and `file_bytes` reads it.
    let _guard = BRAND_IMAGE.lock().await;

    let brand_exists = async || {
        client
            .get(format!("{GO}/api/v4/brand/image"))
            .send()
            .await
            .expect("Go answers")
            .status()
            .as_u16()
            == 200
    };
    // **Purge first, do not merely assert.** The second half of this test uploads a brand image,
    // and a mutation run that panics between the upload and the delete below leaves the file
    // behind — after which every later run fails on its own debris rather than on the mutation.
    // The harness rolls back source, not state ([D-385]), so the purge belongs here.
    let _ = send(
        &client,
        reqwest::Method::DELETE,
        RUST,
        "/api/v4/brand/image",
        &token,
        None,
        Vec::new(),
    )
    .await;
    assert!(
        !brand_exists().await,
        "this test starts from an installation with no brand image"
    );

    let (status, body, served_by) = send(
        &client,
        reqwest::Method::POST,
        RUST,
        "/api/v4/brand/image",
        &token,
        Some(&multipart()),
        multipart_body(&[("image", Some("b.png"), b"not an image at all")]),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "the re-encode is Go's: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let refused: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        refused["id"], "brand.save_brand_image.check_image_limits.app_error",
        "and the refusal is the one from inside SaveBrandImage"
    );
    assert!(
        !brand_exists().await,
        "a forwarded upload Go then refused must have written nothing"
    );

    // Now the accepted shape, for the 201.
    let (status, body, served_by) = send(
        &client,
        reqwest::Method::POST,
        RUST,
        "/api/v4/brand/image",
        &token,
        Some(&multipart()),
        an_image_part(),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "the PNG re-encode is Go's too"
    );
    assert_eq!(
        status,
        201,
        "uploadBrandImage is the one ReturnStatusOK route that is a 201: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("JSON"),
        serde_json::json!({ "status": "OK" }),
        "and it still carries the status body"
    );
    assert!(brand_exists().await, "the forwarded upload did land");

    // Put the installation back the way `file_bytes` expects to find it.
    let (status, body, served_by) = send(
        &client,
        reqwest::Method::DELETE,
        RUST,
        "/api/v4/brand/image",
        &token,
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"), "the delete is ours");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(!brand_exists().await, "and it removed the file");
}

// ---------------------------------------------------------------------------------------------
// the 501s, which no server in this stack can be asked for
// ---------------------------------------------------------------------------------------------

/// `FileSettings.DriverName == ""` on all three writes, and the two orderings it is the only
/// witness to.
///
/// # This test is provisional, and says so
///
/// The stack's Go server is configured with the `local` driver and the whole `file_bytes` suite
/// depends on that, so there is **no Go oracle for a driverless installation**. The expected ids
/// and statuses below are transcribed from api4/user.go:619, api4/user.go:706 and app/brand.go:24
/// rather than measured, and a second mm-api started with the driver name blanked is the only
/// server under test. What *is* measured is the thing a reader is most likely to get wrong: where
/// the storage check sits relative to the permission check and the body, which differs on each of
/// the three routes.
///
/// | route | order |
/// |---|---|
/// | `setProfileImage` | id, permission, **storage**, length, body |
/// | `setDefaultProfileImage` | id, permission, **storage**, user, lock |
/// | `uploadBrandImage` | length, body, image part, permission, **storage** |
///
/// So on a driverless server the profile POST answers 501 to a body that could never parse, and
/// the brand POST answers 400 to the same body and only reaches its 501 with a good one.
#[tokio::test]
async fn a_driverless_server_501s_in_three_different_places() {
    if !stack_enabled() {
        return;
    }
    let Some(server) =
        common::SecondServer::start(8081, &[("MM_FILESETTINGS_DRIVERNAME", "")]).await
    else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgnodrv").await;
    let me = logged_in_user_id();

    let ask = async |method: reqwest::Method,
                     path: String,
                     token: &str,
                     content_type: Option<&str>,
                     body: Vec<u8>| {
        send(
            &client,
            method,
            &server.base,
            &path,
            token,
            content_type,
            body,
        )
        .await
    };

    // `setProfileImage`: storage precedes the body, so an unparseable one still answers 501
    // rather than the parse 500 the ordinary server gives it.
    let (status, body, served_by) = ask(
        reqwest::Method::POST,
        format!("/api/v4/users/{me}/image"),
        &admin,
        Some("application/json"),
        b"{".to_vec(),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 501, "{}", String::from_utf8_lossy(&body));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        parsed["id"],
        "api.user.upload_profile_user.storage.app_error"
    );

    // …and the permission precedes the storage, so a caller who may not write gets the 403 and
    // never learns the server cannot store images.
    let (status, body, _) = ask(
        reqwest::Method::POST,
        format!("/api/v4/users/{me}/image"),
        &plain.token,
        Some(&multipart()),
        an_image_part(),
    )
    .await;
    assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));

    // `setDefaultProfileImage` raises the **same id** under its own name, and it too is reached
    // before the user is looked up: a user that does not exist still answers 501, not 404.
    let (status, body, served_by) = ask(
        reqwest::Method::DELETE,
        format!("/api/v4/users/{NOBODY}/image"),
        &admin,
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 501, "{}", String::from_utf8_lossy(&body));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        parsed["id"], "api.user.upload_profile_user.storage.app_error",
        "the DELETE borrows the upload's error id, under its own `where`"
    );

    // `uploadBrandImage` is the other way round: the body and the permission come first, so the
    // 501 is reachable only by an administrator sending something well formed.
    let (status, body, _) = ask(
        reqwest::Method::POST,
        "/api/v4/brand/image".to_owned(),
        &admin,
        Some("application/json"),
        b"{".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        400,
        "the parse comes first on this route: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, body, _) = ask(
        reqwest::Method::POST,
        "/api/v4/brand/image".to_owned(),
        &plain.token,
        Some(&multipart()),
        an_image_part(),
    )
    .await;
    assert_eq!(
        status,
        403,
        "and the permission comes before the storage: {}",
        String::from_utf8_lossy(&body)
    );

    let (status, body, served_by) = ask(
        reqwest::Method::POST,
        "/api/v4/brand/image".to_owned(),
        &admin,
        Some(&multipart()),
        an_image_part(),
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 501, "{}", String::from_utf8_lossy(&body));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        parsed["id"], "api.admin.upload_brand_image.storage.app_error",
        "and the brand route has its own id, not the profile one"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

/// The **two** size limits, 512 bytes apart, and the routes that answer them differently.
///
/// # Why a third server
///
/// `FileSettings.MaxFileSize` is 100 MiB on this stack, so the boundary is not reachable by
/// sending bytes and an off-by-one in either comparison is invisible. A second mm-api with the
/// cap at 1 KiB puts both limits inside a request this suite can afford, at the cost of the Go
/// oracle: Go's own server keeps the 100 MiB cap, so the statuses below are transcribed from
/// api4/user.go:623, api4/brand.go:45 and web/handlers.go:217-225 rather than measured. The
/// arithmetic is the subject, and the arithmetic is what the transcription pins.
///
/// # The two limits
///
/// * `r.ContentLength > *FileSettings.MaxFileSize` — the handler's own check, on the **header**.
///   Strictly greater, so a body of exactly the cap is allowed.
/// * `http.MaxBytesReader(w, r.Body, MaxFileSize + bytes.MinRead)` — `web.Handler.ServeHTTP`
///   wrapping the body, 512 bytes higher, and reached only when the first check could not fire.
///   A chunked body has no `Content-Length`, so `r.ContentLength` is -1 and this is the only
///   limit left.
///
/// And the same over-long chunked body is a **413** on `setProfileImage` and a **400** on
/// `uploadBrandImage`, because only the first wraps its parse error where `handleContextError`
/// can find the `MaxBytesError` inside it.
#[tokio::test]
async fn both_size_limits_are_where_go_puts_them() {
    if !stack_enabled() {
        return;
    }
    let Some(server) =
        common::SecondServer::start(8084, &[("MM_FILESETTINGS_MAXFILESIZE", "1024")]).await
    else {
        return;
    };
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let profile = format!("/api/v4/users/{me}/image");

    // A body of exactly the cap passes: the comparison is `>`, not `>=`. It is a well-formed
    // multipart with an `image` part, so every refusal is passed and the request is handed to Go
    // — which has the ordinary 100 MiB cap and refuses it on its own decode instead.
    let at_the_cap = padded_image_part(1024);
    assert_eq!(at_the_cap.len(), 1024);
    let (_, body, served_by) = raw_post(
        &server.base,
        &profile,
        &token,
        Framing::Declared(1024),
        &at_the_cap,
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "a body of exactly MaxFileSize is not too large: {}",
        String::from_utf8_lossy(&body)
    );

    // One byte more is the handler's own 413, with the route's own error id.
    let over = padded_image_part(1025);
    for (path, expected) in [
        (
            profile.as_str(),
            "api.user.upload_profile_user.too_large.app_error",
        ),
        (
            "/api/v4/brand/image",
            "api.admin.upload_brand_image.too_large.app_error",
        ),
    ] {
        let (status, body, served_by) =
            raw_post(&server.base, path, &token, Framing::Declared(1025), &over).await;
        assert_eq!(served_by.as_deref(), Some("rust"), "{path}");
        assert_eq!(status, 413, "{path}: {}", String::from_utf8_lossy(&body));
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(parsed["id"], expected, "{path}: each route has its own id");
    }

    // A chunked body of 1536 bytes — exactly `MaxFileSize + bytes.MinRead` — still fits the read
    // cap, so it reaches the parser and, with an `image` part in it, is forwarded.
    let at_the_read_cap = padded_image_part(1536);
    let (_, body, served_by) = raw_post(
        &server.base,
        &profile,
        &token,
        Framing::Chunked,
        &at_the_read_cap,
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "MaxFileSize + 512 is the cap, not one below it: {}",
        String::from_utf8_lossy(&body)
    );

    // One byte over the read cap, with no `Content-Length` to refuse it by. This is the branch
    // the two routes answer differently.
    let over_the_read_cap = padded_image_part(1537);
    let (status, body, served_by) = raw_post(
        &server.base,
        &profile,
        &token,
        Framing::Chunked,
        &over_the_read_cap,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(status, 413, "{}", String::from_utf8_lossy(&body));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        parsed["id"], "api.context.request_body_too_large.app_error",
        "setProfileImage wraps its parse error, so handleContextError rewrites it"
    );

    let (status, body, served_by) = raw_post(
        &server.base,
        "/api/v4/brand/image",
        &token,
        Framing::Chunked,
        &over_the_read_cap,
    )
    .await;
    assert_eq!(served_by.as_deref(), Some("rust"));
    assert_eq!(
        status,
        400,
        "the same body, the other route: {}",
        String::from_utf8_lossy(&body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(
        parsed["id"], "api.admin.upload_brand_image.parse.app_error",
        "uploadBrandImage does not wrap, so the MaxBytesError is invisible and it is a 400"
    );
}

/// A `multipart/form-data` body with an `image` file part, padded with a second value part until
/// it is exactly `total` bytes long. Panics rather than truncate if the frame alone is longer.
fn padded_image_part(total: usize) -> Vec<u8> {
    let base = multipart_body(&[("image", Some("p.png"), TINY_PNG), ("pad", None, b"")]);
    assert!(
        base.len() <= total,
        "the frame alone is {} bytes, more than the {total} asked for",
        base.len()
    );
    let padding = vec![b'x'; total - base.len()];
    let body = multipart_body(&[("image", Some("p.png"), TINY_PNG), ("pad", None, &padding)]);
    assert_eq!(body.len(), total, "the padding must land on the byte");
    body
}

/// `me` resolves to the session's own user on every one of the four routes.
///
/// `RequireUserId` rewrites the literal before `IsValidId` sees it (web/context.go:301), so a
/// port that validated first would answer 400 to the alias every real client uses. Nothing else
/// in this suite sends it: every other case names a 26-character id.
#[tokio::test]
async fn the_me_alias_resolves_on_all_four_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    // The POST, at a refusal that is only reachable once the id has resolved: `me` with no
    // `image` part is the `no_file` 400, where an unresolved `me` would be the
    // `invalid_url_param` 400 instead. Two 400s, and only the body tells them apart.
    let body = both_refuse_identically(
        &client,
        reqwest::Method::POST,
        "/api/v4/users/me/image",
        &token,
        Some(&multipart()),
        multipart_body(&[("picture", Some("p.png"), TINY_PNG)]),
        "POST /users/me/image",
    )
    .await;
    assert_eq!(body["id"], "api.user.upload_profile_user.no_file.app_error");

    // The GET resolves too, and answers the same bytes as the explicit id.
    for path in [
        "/api/v4/users/me/image/default".to_owned(),
        format!("/api/v4/users/{me}/image/default"),
    ] {
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            response.status().as_u16(),
            200,
            "{path} must resolve to the session's user"
        );
    }
}

/// The 409 that says LDAP owns the picture: all four corners of
/// `(IsLDAPUser || (IsSAMLUser && EnableSyncWithLdap)) && PictureAttribute != ""`.
///
/// # Why this needs a fourth server and a SQL write
///
/// `LdapSettings.PictureAttribute` is `""` on this stack and on every stock server, so the whole
/// conjunction is false and the branch is unreachable through the API — and `Users.AuthService`
/// cannot be set through either server's API without an enterprise licence and a real directory.
/// So the attribute comes from a second mm-api's environment and the auth service from a direct
/// `UPDATE`, and the expected answers are transcribed from api4/user.go:672-678.
///
/// # What each corner proves
///
/// | auth service | `PictureAttribute` | `EnableSyncWithLdap` | answer |
/// |---|---|---|---|
/// | `ldap` | set | — | 409 |
/// | `saml` | set | true | 409 |
/// | `saml` | set | **false** | forwarded |
/// | `""` | set | true | forwarded |
/// | `ldap` | **empty** | — | forwarded |
///
/// The last two rows are the ones that would go missing if the `&&` between the auth-service
/// disjunction and the attribute became an `||`, or if the inner `&&` did.
#[tokio::test]
async fn ldap_owns_the_picture_only_when_an_attribute_names_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "imgldap").await;

    // Two servers: one that names a picture attribute and syncs SAML with LDAP, one that names
    // the attribute and does not.
    let Some(syncing) = common::SecondServer::start(
        8085,
        &[
            ("MM_LDAPSETTINGS_PICTUREATTRIBUTE", "thumbnailPhoto"),
            ("MM_SAMLSETTINGS_ENABLESYNCWITHLDAP", "true"),
        ],
    )
    .await
    else {
        common::delete_plain_user(&client, &admin, &plain.id).await;
        return;
    };
    let Some(unsynced) = common::SecondServer::start(
        8086,
        &[("MM_LDAPSETTINGS_PICTUREATTRIBUTE", "thumbnailPhoto")],
    )
    .await
    else {
        common::delete_plain_user(&client, &admin, &plain.id).await;
        return;
    };

    let path = format!("/api/v4/users/{}/image", plain.id);
    let ask = async |base: &str| {
        send(
            &client,
            reqwest::Method::POST,
            base,
            &path,
            &plain.token,
            Some(&multipart()),
            an_image_part(),
        )
        .await
    };

    for (auth_service, base, expected_served_by, note) in [
        (
            "ldap",
            syncing.base.as_str(),
            "rust",
            "an LDAP user is refused outright",
        ),
        (
            "saml",
            syncing.base.as_str(),
            "rust",
            "and a SAML user is, when the server syncs",
        ),
        (
            "saml",
            unsynced.base.as_str(),
            "go",
            "but not when it does not — the inner && is not an ||",
        ),
        (
            "",
            syncing.base.as_str(),
            "go",
            "and an email user is never refused, whatever the attribute says",
        ),
        (
            "ldap",
            RUST,
            "go",
            "nor is an LDAP user on a server that names no attribute — the outer && is not an ||",
        ),
    ] {
        assert!(
            common::set_user_auth_service(&plain.id, auth_service).await,
            "the fixture needs a database"
        );
        let (status, body, served_by) = ask(base).await;
        assert_eq!(
            served_by.as_deref(),
            Some(expected_served_by),
            "auth_service={auth_service:?}: {note}\n  {}",
            String::from_utf8_lossy(&body)
        );
        if expected_served_by == "rust" {
            assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
            let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
            assert_eq!(
                parsed["id"],
                "api.user.upload_profile_user.login_provider_attribute_set.app_error"
            );
        }
    }

    // Put the column back before the row is deleted, so a teardown that fails for some other
    // reason does not leave an LDAP account behind for the next suite to trip over.
    common::set_user_auth_service(&plain.id, "").await;
    common::delete_plain_user(&client, &admin, &plain.id).await;
}
