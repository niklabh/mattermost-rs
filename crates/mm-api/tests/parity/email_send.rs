//! Cross-server parity for the e-mail itself: the same action through Go and through mm-api, the
//! two messages captured by the stack's SMTP sink (`common::smtp_sink`), and compared byte for
//! byte once the genuinely random parts are masked.
//!
//! ```sh
//! scripts/parity.sh --test parity email_send
//! ```
//!
//! # What is masked, and nothing else
//!
//! `Date`, `Message-ID` and the multipart boundaries (random on both servers), and a one-shot
//! token where the body carries one — each server mints its own. The token is masked **in place**,
//! character for character and across quoted-printable soft line breaks, so the line structure
//! both servers produced is still compared exactly.
//!
//! The SMTP commands are compared too: `EHLO` names the host from `SiteURL`, `MAIL FROM` carries
//! whatever extensions the client chose to use.

use std::time::Duration;

use crate::common;

use common::smtp_sink::{Captured, normalize_message, smtp_sink};
use common::{GO, RUST, client, go_minted_token, stack_enabled};

const WAIT: Duration = Duration::from_secs(10);

fn address(tag: &str) -> String {
    format!("{}@mmrs.invalid", common::plain_username(tag))
}

/// The sink, or a panic: on a live stack a missing sink means every comparison below would be
/// comparing nothing.
fn sink() -> &'static common::smtp_sink::Sink {
    smtp_sink().expect("the SMTP sink could not bind this stack's port — who holds it?")
}

/// Replace every occurrence of `secret` with `X`s of the same length, matching across `=\r\n`
/// soft line breaks and leaving them where they are.
pub(super) fn mask_across_soft_breaks(text: &str, secret: &str, label: char) -> String {
    let bytes = text.as_bytes();
    let secret = secret.as_bytes();
    let mut out = bytes.to_vec();
    let mut start = 0;
    while start < bytes.len() {
        let (mut i, mut j) = (start, 0);
        let mut positions = Vec::with_capacity(secret.len());
        while j < secret.len() && i < bytes.len() {
            if bytes[i..].starts_with(b"=\r\n") {
                i += 3;
                continue;
            }
            if bytes[i] != secret[j] {
                break;
            }
            positions.push(i);
            i += 1;
            j += 1;
        }
        if j == secret.len() {
            for p in positions {
                out[p] = label as u8;
            }
            start = i;
        } else {
            start += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The one live token of `token_type` whose `Extra` names `email`.
async fn token_for(email: &str, token_type: &str) -> String {
    let pool = common::fixture_pool().await.expect("the stack database");
    sqlx::query_scalar::<_, String>(
        "SELECT token FROM tokens WHERE type = $1 AND extra LIKE $2 ORDER BY createat DESC LIMIT 1",
    )
    .bind(token_type)
    .bind(format!("%{email}%"))
    .fetch_one(&pool)
    .await
    .expect("a token was minted")
}

async fn delete_tokens(email: &str) {
    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query("DELETE FROM tokens WHERE extra LIKE $1")
            .bind(format!("%{email}%"))
            .execute(&pool)
            .await;
    }
}

/// Assert two captured sessions agree: the commands, and the message once normalised.
fn assert_same_mail(go: &Captured, rs: &Captured, context: &str) {
    assert_eq!(
        go.commands, rs.commands,
        "{context}: the SMTP commands differ"
    );
    let (go_data, rs_data) = (
        normalize_message(&go.data_str()),
        normalize_message(&rs.data_str()),
    );
    if go_data != rs_data {
        let first = go_data
            .bytes()
            .zip(rs_data.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(go_data.len().min(rs_data.len()));
        let from = first.saturating_sub(200);
        panic!(
            "{context}: the messages differ at byte {first}\n--- go ---\n{}\n--- rust ---\n{}",
            &go_data[from..(first + 200).min(go_data.len())],
            &rs_data[from..(first + 200).min(rs_data.len())],
        );
    }
}

async fn post_json(base: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Vec<u8>) {
    let mut request = client()
        .post(format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .header("X-Requested-With", "XMLHttpRequest")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    (
        status,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// A fixture account in the default team, its welcome mail (Go sends one on create) discarded.
async fn fixture_user(tag: &str) -> (common::PlainUser, String) {
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = common::create_plain_user(&http, &admin, &team_id, tag).await;
    sink().discard(&address(tag));
    (user, admin)
}

/// `POST /users/password/reset/send` for an ordinary account: both servers mint a recovery token
/// and mail it, and the two mails agree once each server's token is masked.
#[tokio::test]
async fn a_password_reset_mail_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailreset";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    let body = format!(r#"{{"email":"{email}"}}"#);

    let mut mails = Vec::new();
    for base in [GO, RUST] {
        let (status, response) =
            post_json(base, "/api/v4/users/password/reset/send", None, &body).await;
        assert_eq!(
            status,
            200,
            "{base}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(response, br#"{"status":"OK"}"#, "{base}");
        let token = token_for(&email, "password_recovery").await;
        let mut mail = sink
            .take(&email, WAIT)
            .await
            .unwrap_or_else(|| panic!("{base} sent no mail"));
        mail.data = mask_across_soft_breaks(&mail.data_str(), &token, 'T').into_bytes();
        mails.push(mail);
    }
    assert_same_mail(&mails[0], &mails[1], "password reset");

    delete_tokens(&email).await;
    common::delete_plain_user(&client(), &admin, &user.id).await;
}

/// `POST /users/email/verify/send` for an account that has never connected — no `Status` row —
/// sends the verify-your-address mail.
#[tokio::test]
async fn a_verification_mail_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailverify";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query("DELETE FROM status WHERE userid = $1")
            .bind(&user.id)
            .execute(&pool)
            .await;
    }
    let body = format!(r#"{{"email":"{email}"}}"#);

    let mut mails = Vec::new();
    for base in [GO, RUST] {
        let (status, _) = post_json(
            base,
            "/api/v4/users/email/verify/send?r=%2Fsome%2Fplace",
            None,
            &body,
        )
        .await;
        assert_eq!(status, 200, "{base}");
        let token = token_for(&email, "verify_email").await;
        let mut mail = sink
            .take(&email, WAIT)
            .await
            .unwrap_or_else(|| panic!("{base} sent no mail"));
        mail.data = mask_across_soft_breaks(&mail.data_str(), &token, 'T').into_bytes();
        mails.push(mail);
    }
    assert_same_mail(&mails[0], &mails[1], "verification");

    delete_tokens(&email).await;
    common::delete_plain_user(&client(), &admin, &user.id).await;
}

/// The same route for an account that **has** a `Status` row sends the email-*change*
/// verification instead — a different template, a different subject, and `SupportEmail` forced
/// to `feedback@mattermost.com`.
#[tokio::test]
async fn an_email_change_verification_mail_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailchgverify";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    common::set_user_status(&client(), &admin, &user.id, "online", 0).await;
    let body = format!(r#"{{"email":"{email}"}}"#);

    let mut mails = Vec::new();
    for base in [GO, RUST] {
        let (status, _) = post_json(base, "/api/v4/users/email/verify/send", None, &body).await;
        assert_eq!(status, 200, "{base}");
        let token = token_for(&email, "verify_email").await;
        let mut mail = sink
            .take(&email, WAIT)
            .await
            .unwrap_or_else(|| panic!("{base} sent no mail"));
        mail.data = mask_across_soft_breaks(&mail.data_str(), &token, 'T').into_bytes();
        mails.push(mail);
    }
    assert_same_mail(&mails[0], &mails[1], "email change verification");

    delete_tokens(&email).await;
    common::delete_plain_user(&client(), &admin, &user.id).await;
}

/// `POST /email/test` with no body tests the **live** configuration, and mails the caller.
#[tokio::test]
async fn a_test_email_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailtest";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    assert!(
        common::set_user_roles(&user.id, "system_user system_admin").await,
        "promoting the fixture"
    );
    let token = common::login_plain_user(&client(), tag).await;

    let mut mails = Vec::new();
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, response) = post_json(base, "/api/v4/email/test", Some(&token), "").await;
        bodies.push((status, response));
        mails.push(
            sink.take(&email, WAIT)
                .await
                .unwrap_or_else(|| panic!("{base} sent no mail")),
        );
    }
    assert_eq!(bodies[0], bodies[1], "the responses differ");
    assert_same_mail(&mails[0], &mails[1], "test email");

    common::delete_plain_user(&client(), &admin, &user.id).await;
}

/// A self-service password change mails the password-change notice from a background task on
/// both servers; the `method` line is translated with the user's locale.
#[tokio::test]
async fn a_password_change_notice_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailpwchange";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    let token = &user.token;

    let mut mails = Vec::new();
    let passwords = [
        (common::PLAIN_USER_PASSWORD, "Mmrs-Plain-5678"),
        ("Mmrs-Plain-5678", common::PLAIN_USER_PASSWORD),
    ];
    for (base, (current, new)) in [GO, RUST].into_iter().zip(passwords) {
        let response = client()
            .put(format!("{base}/api/v4/users/{}/password", user.id))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Requested-With", "XMLHttpRequest")
            .json(&serde_json::json!({ "current_password": current, "new_password": new }))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 200, "{base}");
        mails.push(
            sink.take(&email, WAIT)
                .await
                .unwrap_or_else(|| panic!("{base} sent no mail")),
        );
    }
    assert_same_mail(&mails[0], &mails[1], "password change");

    common::delete_plain_user(&client(), &admin, &user.id).await;
}

/// Creating an account mails the welcome message (the stack runs with `SendEmailNotifications`
/// on). The two accounts differ, so their addresses are made the same length and the Rust one
/// is rewritten to the Go one before comparing.
#[tokio::test]
async fn a_welcome_mail_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;

    let mut mails = Vec::new();
    let mut ids = Vec::new();
    for (base, tag) in [(GO, "mailwelcomego"), (RUST, "mailwelcomers")] {
        let email = address(tag);
        let response = http
            .post(format!("{base}/api/v4/users"))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&serde_json::json!({
                "email": email,
                "username": common::plain_username(tag),
                "password": common::PLAIN_USER_PASSWORD,
            }))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 201, "{base}");
        let created: serde_json::Value = response.json().await.expect("a user");
        ids.push(created["id"].as_str().unwrap_or_default().to_owned());
        let mut mail = sink
            .take(&email, WAIT)
            .await
            .unwrap_or_else(|| panic!("{base} sent no welcome mail"));
        let rewritten = mail.data_str().replace(&email, &address("mailwelcomego"));
        mail.data = rewritten.into_bytes();
        mail.commands = mail
            .commands
            .iter()
            .map(|c| c.replace(&email, &address("mailwelcomego")))
            .collect();
        mails.push(mail);
    }
    assert_same_mail(&mails[0], &mails[1], "welcome");

    for id in ids {
        common::delete_plain_user(&http, &admin, &id).await;
    }
}

/// Post through `base` as the admin; return the post's id and `create_at`.
async fn post_as_admin(
    base: &str,
    admin: &str,
    channel_id: &str,
    message: &str,
    root_id: Option<&str>,
) -> (String, i64) {
    let mut body = serde_json::json!({ "channel_id": channel_id, "message": message });
    if let Some(root) = root_id {
        body["root_id"] = serde_json::Value::from(root);
    }
    let response = client()
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&body)
        .send()
        .await
        .expect("the server answers");
    assert_eq!(response.status(), 201, "{base}");
    let post: serde_json::Value = response.json().await.expect("a post");
    (
        post["id"].as_str().unwrap_or_default().to_owned(),
        post["create_at"].as_i64().unwrap_or_default(),
    )
}

/// A mention's notification e-mail — markdown to HTML, a `~channel` link, the sender's avatar
/// embedded, the post's id as the `Message-ID` — and a reply's, which names its root in
/// `In-Reply-To` and `References`. The recipient never reads anything, so both servers see them
/// offline: `userAllowsEmail` refuses an online recipient, and "online" lives in each process's
/// own status cache.
#[tokio::test]
async fn a_mentions_notification_email_matches_gos() {
    if !stack_enabled() {
        return;
    }
    let sink = sink();
    let tag = "mailmention";
    let email = address(tag);
    let (user, admin) = fixture_user(tag).await;
    let http = client();
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel = common::create_channel(&http, &admin, &team_id, tag).await;
    common::add_user_to_channel(&http, &admin, &channel, &user.id).await;
    // Being added is a mention too, and mails the added user; drop that mail.
    let _ = sink.take(&email, Duration::from_secs(5)).await;
    let username = common::plain_username(tag);
    let message = format!(
        "@{username} look at **this** in ~town-square: `code` and [a link](https://example.com)\n\n> quoted"
    );

    for attempt in 0..3 {
        let mut mails = Vec::new();
        let mut minutes = Vec::new();
        let mut roots = Vec::new();
        for base in [GO, RUST] {
            let (id, at) = post_as_admin(base, &admin, &channel, &message, None).await;
            let mut mail = sink
                .take(&email, WAIT)
                .await
                .unwrap_or_else(|| panic!("{base} sent no notification mail"));
            mail.data = mask_across_soft_breaks(&mail.data_str(), &id, 'P').into_bytes();
            mails.push(mail);
            minutes.push(at / 60_000);
            roots.push(id);
        }
        // The body prints the post's hour and minute; two posts either side of a minute boundary
        // differ there and nowhere else.
        if minutes[0] != minutes[1] && attempt < 2 {
            continue;
        }
        assert_same_mail(&mails[0], &mails[1], "mention notification");
        let go = mails[0].data_str();
        assert!(
            go.contains("user-avatar.png"),
            "the sender's avatar is embedded"
        );
        assert!(
            go.contains("<strong>this</strong>"),
            "the markdown is rendered"
        );
        assert!(
            go.contains("/channels/town-square"),
            "the channel mention is a link"
        );

        let mut replies = Vec::new();
        for (base, root) in [GO, RUST].into_iter().zip(&roots) {
            let (id, _) = post_as_admin(base, &admin, &channel, &message, Some(root)).await;
            let mut mail = sink
                .take(&email, WAIT)
                .await
                .unwrap_or_else(|| panic!("{base} sent no reply notification"));
            let masked = mask_across_soft_breaks(&mail.data_str(), &id, 'P');
            mail.data = mask_across_soft_breaks(&masked, root, 'R').into_bytes();
            replies.push(mail);
        }
        assert_same_mail(&replies[0], &replies[1], "reply notification");
        assert!(
            replies[0]
                .data_str()
                .contains("In-Reply-To: <RRRRRRRRRRRRRRRRRRRRRRRRRR@"),
            "a reply names its root"
        );
        break;
    }

    common::delete_channel(&http, &admin, &channel).await;
    common::delete_plain_user(&http, &admin, &user.id).await;
}
