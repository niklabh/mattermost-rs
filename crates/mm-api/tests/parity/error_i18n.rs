//! Cross-server parity for the **translated error message** — `AppError.Translate`, which
//! `web.Handler.handleContextError` (handlers.go:431) runs on every error body before it is
//! written, and which [D-092] tracked until it was closed.
//!
//! ```sh
//! scripts/parity.sh --test parity error_i18n
//! ```
//!
//! # What this suite adds over the rest of the parity binary
//!
//! Every other suite calls `assert_error_bodies_match_except_known_gaps`, which since D-092 closed
//! compares `message` too — so error prose is already checked across the whole API, in whatever
//! locale a header-less request resolves to (`DefaultClientLocale`, `en` on this stack). What is
//! *not* covered there is the locale negotiation itself, so this suite drives one error from every
//! status family this server answers against **eight** `Accept-Language` values, including:
//!
//! - `es` and `ja`, which must come back in Spanish and Japanese;
//! - `fr-CA,fr;q=0.9`, where the full tag has no file and the language part does — Go's second
//!   branch, which a port that only tried the whole header would answer in English;
//! - `xx-YY`, where neither does, and the answer is `DefaultClientLocale`;
//! - `pt`, which has no file *of its own* but is a go-i18n fallback tag for `pt-BR` — and which
//!   `GetTranslationsAndLocaleFromRequest` nonetheless resolves to the **default**, because its
//!   three branches test the `locales` map (file stems) and not the bundle's fallbacks.
//!
//! And it covers the ids whose sentence is a **template**: `invalid_url_param` names the
//! parameter and `app.channel.get.existing` names the channel id, so a port that filled the params
//! map from the wrong place renders `<no value>` into an otherwise perfect sentence.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    go_minted_token, stack_enabled,
};

/// The header values, and what each one exercises. Order is not significant.
const ACCEPT_LANGUAGES: &[&str] = &[
    "",               // no header: `DefaultClientLocale`
    "en",             // the exact tag
    "es",             // a non-English exact tag
    "ja",             // and a non-Latin one
    "pt-BR",          // a regional tag with a file of its own
    "pt",             // a fallback tag with no file: the default, not Brazilian Portuguese
    "fr-CA,fr;q=0.9", // the full tag misses, its language part hits
    "xx-YY",          // neither: the default
];

async fn get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    accept_language: &str,
) -> (u16, Option<String>, Vec<u8>) {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if !accept_language.is_empty() {
        request = request.header("Accept-Language", accept_language);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|err| panic!("{base} is unreachable: {err}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    (status, served, body)
}

/// Drive one GET on both servers under every header and compare the whole body.
///
/// Returns each locale's Go message so the caller can assert the *set* is not degenerate: eight
/// identical sentences would pass a byte comparison while proving the negotiation does nothing.
async fn compare_every_locale(
    client: &reqwest::Client,
    path: &str,
    token: Option<&str>,
    expected_status: u16,
    expected_id: &str,
) -> Vec<String> {
    let mut messages = Vec::new();
    for accept_language in ACCEPT_LANGUAGES {
        let (go_status, _, go_body) = get(client, GO, path, token, accept_language).await;
        let (rs_status, served, rs_body) = get(client, RUST, path, token, accept_language).await;
        let context = format!("{path} with Accept-Language {accept_language:?}");
        assert_eq!(
            served.as_deref(),
            Some("rust"),
            "{context}: this route must be served here for the comparison to mean anything"
        );
        assert_eq!(
            (go_status, rs_status),
            (expected_status, expected_status),
            "{context}: go={} rust={}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
        assert_eq!(go["id"], expected_id, "{context}");
        messages.push(
            go["message"]
                .as_str()
                .unwrap_or_else(|| panic!("{context}: Go's message is a string"))
                .to_owned(),
        );
    }
    messages
}

/// The negotiation itself has to *do* something, or every assertion above is vacuous.
///
/// `en`, `es`, `ja`, `pt-BR` and `fr` must all be different sentences, and the three that resolve
/// to the default (`""`, `pt`, `xx-YY`) must equal the `en` one on a stack whose
/// `DefaultClientLocale` is `en`.
fn assert_the_locales_actually_differ(messages: &[String], what: &str) {
    let by = |tag: &str| {
        let at = ACCEPT_LANGUAGES
            .iter()
            .position(|candidate| *candidate == tag)
            .unwrap_or_else(|| panic!("{tag} is one of the headers"));
        messages[at].as_str()
    };
    let distinct: std::collections::BTreeSet<&str> = ["en", "es", "ja", "pt-BR", "fr-CA,fr;q=0.9"]
        .iter()
        .map(|tag| by(tag))
        .collect();
    assert_eq!(
        distinct.len(),
        5,
        "{what}: five languages must be five sentences, got {distinct:?}"
    );
    for defaulting in ["", "pt", "xx-YY"] {
        assert_eq!(
            by(defaulting),
            by("en"),
            "{what}: Accept-Language {defaulting:?} resolves to DefaultClientLocale"
        );
    }
}

/// **401**, no params: `api.context.session_expired.app_error` on a route that requires a session.
#[tokio::test]
async fn the_401_message_is_translated_in_every_locale() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let messages = compare_every_locale(
        &client,
        "/api/v4/users/me/teams",
        None,
        401,
        "api.context.session_expired.app_error",
    )
    .await;
    assert_the_locales_actually_differ(&messages, "401");
}

/// **400 with a template**: `api.context.invalid_url_param.app_error` renders `{{.Name}}`, so the
/// sentence must name `post_id` in each language. A params map filled from the wrong place would
/// put `<no value>` there and fail in all eight.
#[tokio::test]
async fn the_400_message_renders_its_parameter_name() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let messages = compare_every_locale(
        &client,
        "/api/v4/posts/notanid",
        Some(&token),
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;
    assert_the_locales_actually_differ(&messages, "400 invalid_url_param");
    for message in &messages {
        assert!(
            message.contains("post_id"),
            "the rendered sentence names the parameter: {message:?}"
        );
        assert!(
            !message.contains("<no value>"),
            "an unrendered field: {message:?}"
        );
    }
}

/// **400 without a template**, and one raised by a gate rather than by parameter parsing.
#[tokio::test]
async fn the_enterprise_edition_400_is_translated() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    compare_every_locale(
        &client,
        "/api/v4/cloud/products",
        Some(&token),
        400,
        "api.server.cws.needs_enterprise_edition",
    )
    .await;
}

/// **403**: a plain user reaching an administrator's route.
#[tokio::test]
async fn the_403_message_is_translated_in_every_locale() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "i18n403").await;
    let messages = compare_every_locale(
        &client,
        "/api/v4/audits",
        Some(&plain.token),
        403,
        "api.context.permissions.app_error",
    )
    .await;
    assert_the_locales_actually_differ(&messages, "403");
    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **404 with a template**: `app.channel.get.existing.app_error` renders `{{.channel_id}}` — a
/// *lower-case* key, unlike the `Name` of the 400s, and one the store layer fills rather than the
/// handler.
#[tokio::test]
async fn the_404_message_renders_the_channel_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let missing = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let messages = compare_every_locale(
        &client,
        &format!("/api/v4/channels/{missing}"),
        Some(&token),
        404,
        "app.channel.get.existing.app_error",
    )
    .await;
    // Only where the *translation* has the field. `es` drops it — "No se pudo encontrar el canal
    // existente." names no channel at all — which is the translators' choice and Go's output, so
    // the assertion is on English plus the absence of an unrendered field anywhere.
    assert!(
        messages.iter().any(|message| message.contains(missing)),
        "no locale rendered the channel id: {messages:?}"
    );
    for message in &messages {
        assert!(
            !message.contains("<no value>"),
            "an unrendered field: {message:?}"
        );
    }
}

/// **501**, the licence gates — two of them, because their ids come from different files and one
/// of the two sentences is lower-case in English, which is the sort of thing a transcription gets
/// tidy about.
#[tokio::test]
async fn the_501_licence_messages_are_translated() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    compare_every_locale(
        &client,
        "/api/v4/data_retention/policy",
        Some(&token),
        501,
        "ent.data_retention.generic.license.error",
    )
    .await;
    compare_every_locale(
        &client,
        "/api/v4/ldap/groups",
        Some(&token),
        501,
        "api.ldap_groups.license_error",
    )
    .await;
}

/// **413**: a declared `Content-Length` over the emoji cap, which is refused before the body is
/// read — a different code path from every other error here, and the one Go rewrites in
/// `handleContextError` itself when it comes from `http.MaxBytesError`.
#[tokio::test]
async fn the_413_message_is_translated() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    const BOUNDARY: &str = "mmrsi18n413boundary";
    // 512 KiB + 1, so the framed body's `Content-Length` is over the cap.
    let oversized = vec![b'x'; (1 << 19) + 1];
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; \
             filename=\"e.png\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&oversized);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");

    for accept_language in ["", "es", "ja"] {
        let mut go_request = client
            .post(format!("{GO}/api/v4/emoji"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", content_type.clone())
            .body(body.clone());
        let mut rs_request = client
            .post(format!("{RUST}/api/v4/emoji"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", content_type.clone())
            .body(body.clone());
        if !accept_language.is_empty() {
            go_request = go_request.header("Accept-Language", accept_language);
            rs_request = rs_request.header("Accept-Language", accept_language);
        }
        let go_response = go_request.send().await.expect("Go is reachable");
        let go_status = go_response.status().as_u16();
        let go_body = go_response.bytes().await.expect("a body").to_vec();
        let rs_response = rs_request.send().await.expect("mm-api is reachable");
        let rs_status = rs_response.status().as_u16();
        let served = rs_response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let rs_body = rs_response.bytes().await.expect("a body").to_vec();

        assert_eq!(served.as_deref(), Some("rust"));
        assert_eq!(
            (go_status, rs_status),
            (413, 413),
            "go={} rust={}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        assert_error_bodies_match_except_known_gaps(
            &go_body,
            &rs_body,
            &format!("413 with Accept-Language {accept_language:?}"),
        );
    }
}
