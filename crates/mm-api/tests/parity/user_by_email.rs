//! Cross-server parity for `GET /api/v4/users/email/{email}` — `getUserByEmail`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_by_email
//! ```
//!
//! # It is not `getUser` with a different lookup
//!
//! Two things `getUser` and `getUserByUsername` do, this handler does not, and both show on the
//! wire: it never fills in the terms-of-service fields, and it has no `is_self` case in the
//! sanitiser. So **looking yourself up by email returns the stranger's view of you**, which is
//! strictly less than `/users/me` gives. [`self_by_email_is_the_strangers_view`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id, purge_api_fixtures,
    stack_enabled,
};

struct Fixture {
    plain_email: String,
    plain_token: String,
    admin_email: String,
    /// A user whose stored address is **not** lowercase, which no API can produce.
    shouty_email: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "byemail").await;
            let plain = create_plain_user(client, token, &team_id, "byemail").await;

            // The addresses are read back rather than constructed, so a change to how
            // `create_plain_user` names its users cannot silently make these tests look up
            // nothing.
            let plain_email = email_of(client, token, &plain.id).await;
            let admin_email = email_of(client, token, logged_in_user_id()).await;

            // `Email = lower(?)` compares the *parameter* to the column, so a row whose stored
            // address is not already lowercase is unreachable by email on either server. Go
            // lowercases on save, so only a direct write can make one.
            let shouty = create_plain_user(client, token, &team_id, "byemailup").await;
            let shouty_email = format!("MMRS-{}", email_of(client, token, &shouty.id).await);
            plant_email(&shouty.id, &shouty_email).await;

            Fixture {
                plain_email,
                plain_token: plain.token,
                admin_email,
                shouty_email,
            }
        })
        .await
}

/// Write an address the API would have lowercased.
async fn plant_email(user_id: &str, email: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    let _ = sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
        .bind(user_id)
        .bind(email)
        .execute(&pool)
        .await;
}

async fn email_of(client: &reqwest::Client, token: &str, user_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let user: serde_json::Value = response.json().await.expect("the user decodes");
    user["email"]
        .as_str()
        .expect("an admin read shows the email")
        .to_owned()
}

fn path(email: &str) -> String {
    format!("/api/v4/users/email/{email}")
}

/// Another user's profile, byte for byte.
#[tokio::test]
async fn another_user_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_email);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["email"], f.plain_email.as_str());
    assert!(
        parsed.get("terms_of_service_id").is_none(),
        "this handler has no terms-of-service branch at all: {parsed}"
    );
}

/// **The headline.** A caller looking *themselves* up by email gets less than `/users/me` gives:
/// no terms-of-service fields, and the non-admin sanitiser rather than `Sanitize({})`.
#[tokio::test]
async fn self_by_email_is_the_strangers_view() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_email);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 200, "the plain user reads their own address");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );

    let by_email: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let me = client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {}", f.plain_token))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("JSON");

    assert_eq!(by_email["id"], me["id"], "the same user, both ways");
    assert!(
        me.get("notify_props").is_some(),
        "`/users/me` keeps every field: {me}"
    );
    assert!(
        by_email.get("notify_props").is_none(),
        "and this route does not, because there is no is_self branch: {by_email}"
    );
    assert!(
        by_email.get("terms_of_service_id").is_none(),
        "nor the terms-of-service fields: {by_email}"
    );
}

/// `SanitizeEmail` lowercases before validating, so the segment's case does not matter.
#[tokio::test]
async fn the_address_is_lowercased_before_it_is_looked_up() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let shouted = f.plain_email.to_uppercase();
    let p = path(&shouted);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "{p}: lowercased, then found");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["email"], f.plain_email.as_str());
}

/// Everything `net/mail.ParseAddress` refuses, plus the two forms it accepts and Go does not.
#[tokio::test]
async fn an_invalid_address_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for bad in [
        "notanemail",
        "no@",
        "@nolocal.example",
        // `mail.ParseAddress` accepts these two and `IsValidEmail` rejects them afterwards,
        // because `addr.Address != input`.
        "<billy@example.com>",
        "Billy Bob <billy@example.com>",
        // The route's `.+` matches a slash, so this reaches the handler as one address.
        "a/b",
        // `POST /users/email/verify` is a different route; a GET falls through to this one.
        "verify",
    ] {
        let p = path(bad);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p} must be rejected by Go");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// An address that is well formed and names nobody.
#[tokio::test]
async fn an_unknown_address_is_a_404_naming_the_missing_account() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let p = path("nobody-at-all@mmrs-parity.example");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(
        go["id"], "app.user.missing_account.const",
        "the same id an unknown *user id* gets, not a by-email-specific one"
    );
}

/// The etag round-trips into a 304 with no body, on both.
#[tokio::test]
async fn the_etag_answers_304() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_email);
    for base in [GO, RUST] {
        let first = client
            .get(format!("{base}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(first.status().as_u16(), 200, "{base}{p}");
        let etag = first
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .expect("an etag")
            .to_owned();

        let second = client
            .get(format!("{base}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &etag)
            .send()
            .await
            .expect("reachable");
        assert_eq!(second.status().as_u16(), 304, "{base}{p} with its own etag");
        assert!(
            second.bytes().await.expect("body reads").is_empty(),
            "{base}{p}: a 304 carries no body"
        );
    }
}

/// The admin's own address, read by a plain user: served, and sanitised the same way on both.
#[tokio::test]
async fn a_plain_caller_reads_an_admin_address() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.admin_email);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 200, "ShowEmailAddress is on for this deployment");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    // `ClearNonProfileFields` drops `notify_props` outright; `Sanitize`'s populated-map mode
    // blanks `auth_data` and `auth_service` in place rather than removing them, because neither
    // carries `omitempty`. Two different erasures, both measured.
    assert!(
        parsed.get("notify_props").is_none(),
        "a non-admin viewer sees no notify props: {parsed}"
    );
    assert_eq!(
        parsed["auth_data"], "",
        "and auth data is blanked, not dropped"
    );
    assert_eq!(parsed["auth_service"], "");
    assert_eq!(
        parsed["email"],
        f.admin_email.as_str(),
        "but the email is there, because ShowEmailAddress is on"
    );
}

/// No session is a 401 on both, before the address is even parsed.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = path("whoever@example.com");
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{p}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{p}");
    }
}

/// Every other method on this path is Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let p = path("whoever@example.com");
    for method in [reqwest::Method::POST, reqwest::Method::PUT] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}

/// A stored address that is not lowercase is unreachable by this route, on both servers.
///
/// `GetByEmail` is `Where("Email = lower(?)", email)` — the **parameter** is lowered, not the
/// column — and `SanitizeEmail` has already lowered the segment, so the comparison is
/// lowercase-against-stored. A row written with capitals matches nothing, whichever case the
/// caller sends. Go cannot create such a row; a direct write can, and both servers then answer
/// the same 404.
#[tokio::test]
async fn an_address_stored_with_capitals_is_unreachable() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for form in [f.shouty_email.clone(), f.shouty_email.to_lowercase()] {
        let p = path(&form);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 404, "{p}: neither case finds the row");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "app.user.missing_account.const");
    }
}
