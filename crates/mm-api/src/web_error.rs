//! How `web.Handler` answers an error on a path that is **not** an API path: the tail of
//! `handleContextError` (web/handlers.go:400) and `utils.RenderWebError` (utils/api.go:56).
//!
//! Under `/api/`, an `AppError` is a JSON body ([`crate::error::ApiError`]). Everywhere else —
//! `/manualtest`, the SPA page, `/files/{id}/public` — Go writes a small HTML page that sends the
//! browser to `<subpath>/error?message=<translated message>&s=<signature>`, where the signature is
//! ECDSA over SHA-256 of `<subpath>/error?<query>` by the server's `AsymmetricSigningKey`. The
//! webapp's error page checks it before showing the message, so the page cannot be faked by a
//! link.
//!
//! Three things make that page Go's rather than merely similar:
//!
//! - **The message is translated**, by the `Accept-Language` negotiation `web.Handler` does before
//!   anything else ([`mm_app::i18n::Translations`]). JSON bodies elsewhere in this port carry the
//!   id ([D-092]); here the text is inside a signed URL, so the id would be a different document.
//! - **The signature is by the same key**: the private half of the `Systems` row Go generated
//!   ([`mm_app::App::asymmetric_signing_key`]). ECDSA is randomised in Go and deterministic
//!   (RFC 6979) here, so the bytes differ on every Go request anyway; both verify.
//! - **Two escapers**, `template.JSEscapeString` inside `template.HTMLEscapeString` for the
//!   `onload` attribute and the latter alone for the other two, ported below and pinned by
//!   `fixtures/behaviour_web_error.json`.
//!
//! # What is not ported
//!
//! A **3xx** `AppError` is `http.Redirect` to the page rather than the page. No handler served
//! through here raises one, so it answers `None` and the caller forwards.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use mm_model::go_url::Values;
use mm_model::utils::AppError;

use crate::serve_content::{build_response, set_header};

/// What `RenderWebError` writes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WebErrorPage {
    /// The page: `Content-Type: text/html`, the status, the body.
    Page { status: u16, body: String },
    /// `http.Error(w, "", 500)` — the signer failed.
    SignFailed,
}

/// Port of `utils.RenderWebError` (utils/api.go:56) for a status outside 300-399, with the
/// signature supplied by `sign` over the SHA-256 digest Go signs. `None` for a redirect status,
/// which is not ported (see the module doc).
pub(crate) fn render_web_error(
    subpath: &str,
    status: u16,
    params: &Values,
    sign: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Option<WebErrorPage> {
    use base64::Engine as _;
    use sha2::Digest as _;

    if (300..400).contains(&status) {
        return None;
    }
    let query_string = params.encode();
    let target = format!(
        "{}?{query_string}",
        mm_model::go_path::join(&[subpath, "error"])
    );
    let digest = sha2::Sha256::digest(target.as_bytes());
    let Some(signature) = sign(&digest) else {
        return Some(WebErrorPage::SignFailed);
    };
    let destination = format!(
        "{target}&s={}",
        base64::engine::general_purpose::URL_SAFE.encode(signature)
    );

    let escaped = template_html_escape_string(&destination);
    let body = format!(
        "<!DOCTYPE html><html><head></head>\n\
         <body onload=\"window.location = '{}'\">\n\
         <noscript><meta http-equiv=\"refresh\" content=\"0; url={escaped}\"></noscript>\n\
         <!-- web error message -->\n\
         <a href=\"{escaped}\" style=\"color: #c0c0c0;\">...</a>\n\
         </body></html>\n",
        template_html_escape_string(&js_escape_string(&destination)),
    );
    Some(WebErrorPage::Page { status, body })
}

/// Port of `template.HTMLEscapeString` (text/template/funcs.go:637): the five markup characters
/// as entities — `"` and `'` numerically — and NUL as U+FFFD.
pub(crate) fn template_html_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push('\u{FFFD}'),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// Port of `template.JSEscapeString` (text/template/funcs.go:669): backslash and both quotes
/// backslash-escaped, `<`, `>`, `&` and `=` as `\u00XX`, other ASCII controls as `\u00XX` with
/// upper-case hex, and a non-ASCII character kept unless `unicode.IsPrint` rejects it, then
/// `\uXXXX` (at least four upper-case digits).
pub(crate) fn js_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u00{:02X}", u32::from(c))),
            c if c.is_ascii() => out.push(c),
            c if mm_model::utils::go_is_print(c) => out.push(c),
            c => out.push_str(&format!("\\u{:04X}", u32::from(c))),
        }
    }
    out
}

/// Where an error came from, for `handleContextError`'s choice between JSON and the page.
pub(crate) struct ErrorContext<'a> {
    /// The request's `Accept-Language`, for the translation.
    pub accept_language: &'a str,
    /// `r.Header.Get("X-Mobile-App") != ""` — the only one of the four JSON conditions a path
    /// served through here can meet (`IsAPICall`, `IsWebhookCall` and `IsOAuthAPICall` are path
    /// tests, and none of these paths is one).
    pub mobile_app: bool,
    /// The request id `ServeHTTP` minted — the `X-Request-Id` header, when it was written.
    pub request_id: &'a str,
}

/// Port of `handleContextError` (web/handlers.go:400) from the zero-status repair on, for a
/// non-API path: the request id, the translation, `WipeDetailed` unless `EnableDeveloper`, the
/// hardened-mode rewrite of a 5xx, then JSON for a mobile client and the signed page for anyone
/// else. `headers` are what `ServeHTTP` had already set; the page replaces the content type.
///
/// `None` hands the request to Go: no translation bundle, no usable signing key, an error whose
/// sentence uses a template construct this port skips, or a redirect status.
pub(crate) async fn handle_context_error(
    state: &crate::AppState,
    context: &ErrorContext<'_>,
    mut headers: HeaderMap,
    mut err: AppError,
) -> Option<Response> {
    if err.status_code == 0 {
        err.status_code = 500;
    }
    err.request_id = context.request_id.to_owned();

    // `c.Err.Translate(c.AppContext.T)`. The message ends up inside a **signed** URL, so an
    // approximation here is a different document rather than a cosmetic divergence: an id whose
    // sentence uses a construct `mm_app::i18n` skips is forwarded to Go instead.
    if !err.skip_translation {
        let bundle = mm_app::i18n::translations().await?;
        let config = state.app.config();
        let locale = bundle
            .request_locale(context.accept_language, &config.default_client_locale)
            .to_owned();
        if !bundle.can_render(&locale, &err.id) {
            return None;
        }
        bundle.translate_app_error(&locale, &mut err);
    }

    let config = state.app.config();
    if !config.enable_developer {
        err.detailed_error = String::new();
    }
    if config.experimental_enable_hardened_mode && err.status_code >= 500 {
        err.id = String::new();
        err.message = "Internal Server Error".to_owned();
        err.detailed_error = String::new();
        err.status_code = 500;
    }
    let status = u16::try_from(err.status_code).ok()?;
    let code = StatusCode::from_u16(status).ok()?;

    if context.mobile_app {
        set_header(&mut headers, "content-type", "application/json");
        let body = mm_model::utils::go_json_marshal(&err).ok()?;
        return Some(build_response(code, headers, Body::from(body)));
    }

    let key = state.app.asymmetric_signing_key().await?;
    let mut params = Values::new();
    params.set("message", &err.message);
    let page = render_web_error(&config.subpath(), status, &params, |digest| {
        use p256::ecdsa::signature::hazmat::PrehashSigner as _;
        let signature: p256::ecdsa::Signature = key.sign_prehash(digest).ok()?;
        Some(signature.to_der().as_bytes().to_vec())
    })?;
    Some(match page {
        WebErrorPage::Page { body, .. } => {
            set_header(&mut headers, "content-type", "text/html");
            build_response(code, headers, Body::from(body))
        }
        WebErrorPage::SignFailed => {
            // `http.Error`: its own content type and `nosniff`, no length carried over.
            headers.remove(axum::http::header::CONTENT_LENGTH);
            set_header(&mut headers, "content-type", "text/plain; charset=utf-8");
            set_header(&mut headers, "x-content-type-options", "nosniff");
            build_response(StatusCode::INTERNAL_SERVER_ERROR, headers, Body::from("\n"))
        }
    })
}

/// Port of `utils.RenderWebAppError` (utils/api.go:52) called **directly by a handler**, which
/// then returns with `c.Err` still set — so `ServeHTTP`'s [`handle_context_error`] renders the
/// error a second time into the same response. Go's body is therefore two pages back to back,
/// each with its own signature; the status and headers are the first write's.
///
/// The two copies can say different things. The first carries `err.Message` as
/// `model.NewAppError` left it — translated at construction by `i18n.T`, the **server** locale —
/// and the second the request's translation. `None` when either cannot be drawn here.
pub(crate) async fn render_web_app_error_twice(
    state: &crate::AppState,
    context: &ErrorContext<'_>,
    headers: HeaderMap,
    err: impl Fn() -> AppError,
) -> Option<Response> {
    let first = {
        let mut err = err();
        let bundle = mm_app::i18n::translations().await?;
        let config = state.app.config();
        let locale = bundle
            .server_locale(&config.default_server_locale)
            .to_owned();
        if !bundle.can_render(&locale, &err.id) {
            return None;
        }
        bundle.translate_app_error(&locale, &mut err);
        let key = state.app.asymmetric_signing_key().await?;
        let mut params = Values::new();
        params.set("message", &err.message);
        let status = u16::try_from(err.status_code).ok()?;
        match render_web_error(&config.subpath(), status, &params, |digest| {
            use p256::ecdsa::signature::hazmat::PrehashSigner as _;
            let signature: p256::ecdsa::Signature = key.sign_prehash(digest).ok()?;
            Some(signature.to_der().as_bytes().to_vec())
        })? {
            WebErrorPage::Page { body, .. } => body,
            // `http.Error` on the first write would fix the status at 500; not reached with a
            // loaded key, and not reproduced.
            WebErrorPage::SignFailed => return None,
        }
    };
    let second = handle_context_error(state, context, headers, err()).await?;
    let (mut parts, body) = second.into_parts();
    let second = axum::body::to_bytes(body, usize::MAX).await.ok()?;
    let mut whole = first.into_bytes();
    whole.extend_from_slice(&second);
    // The first write fixed the type: a mobile client's JSON second copy does not change it.
    set_header(&mut parts.headers, "content-type", "text/html");
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Some(Response::from_parts(parts, Body::from(whole)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake signer `reference/dump/behaviour_web_error.go` uses: `digest || digest || fb ff`.
    fn fixed_sign(digest: &[u8]) -> Option<Vec<u8>> {
        let mut out = digest.to_vec();
        out.extend_from_slice(digest);
        out.extend_from_slice(&[0xfb, 0xff]);
        Some(out)
    }

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_web_error.json"))
            .expect("behaviour_web_error.json is generated by reference/dump")
    }

    /// `RenderWebError` against Go's, byte for byte, over subpaths carrying every character either
    /// escaper treats specially and messages the query encoding has to handle. The fake signer
    /// makes the signed input part of the output, so a port that hashed anything but
    /// `<subpath>/error?<query>` fails here too.
    #[test]
    fn go_parity_render_web_error() {
        let fixture = fixture();
        let rows = fixture["render_web_error"].as_array().expect("an array");
        assert!(rows.len() > 100, "{}", rows.len());
        let mut failures = Vec::new();
        for row in rows {
            let subpath = row["subpath"].as_str().unwrap();
            let status = u16::try_from(row["status"].as_u64().unwrap()).unwrap();
            let message = row["message"].as_str().unwrap();
            let mut params = Values::new();
            params.set("message", message);
            let page = render_web_error(subpath, status, &params, fixed_sign);
            assert_eq!(row["content_type"], "text/html");
            let expected = WebErrorPage::Page {
                status: u16::try_from(row["out_status"].as_u64().unwrap()).unwrap(),
                body: row["body"].as_str().unwrap().to_owned(),
            };
            if page.as_ref() != Some(&expected) {
                failures.push(format!(
                    "subpath {subpath:?} status {status} message {message:?}:\n ours {page:?}\n Go   {expected:?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn the_fake_signer_is_the_oracles() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(b"/error?message=x");
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE.encode(fixed_sign(&digest).unwrap()),
            fixture()["fixed_signature_of_error_message_x"]
        );
    }

    #[test]
    fn a_redirect_status_is_not_rendered_and_a_failed_signature_is_an_empty_500() {
        let params = Values::new();
        assert_eq!(render_web_error("/", 302, &params, fixed_sign), None);
        assert_eq!(render_web_error("/", 300, &params, fixed_sign), None);
        assert_eq!(render_web_error("/", 399, &params, fixed_sign), None);
        assert!(matches!(
            render_web_error("/", 400, &params, fixed_sign),
            Some(WebErrorPage::Page { status: 400, .. })
        ));
        assert!(matches!(
            render_web_error("/", 299, &params, fixed_sign),
            Some(WebErrorPage::Page { status: 299, .. })
        ));
        assert_eq!(
            render_web_error("/", 400, &params, |_| None),
            Some(WebErrorPage::SignFailed)
        );
    }

    /// A real signature verifies with the key's public half over exactly the page's URL.
    #[test]
    fn a_real_signature_verifies_over_the_signed_url() {
        use base64::Engine as _;
        use p256::ecdsa::signature::hazmat::{PrehashSigner as _, PrehashVerifier as _};
        use sha2::Digest as _;
        let key = p256::ecdsa::SigningKey::from_slice(&[7u8; 32]).unwrap();
        let mut params = Values::new();
        params.set("message", "Invalid email.");
        let Some(WebErrorPage::Page { body, .. }) = render_web_error("/", 400, &params, |d| {
            let sig: p256::ecdsa::Signature = key.sign_prehash(d).ok()?;
            Some(sig.to_der().as_bytes().to_vec())
        }) else {
            panic!("a page");
        };
        let href = body
            .split("<a href=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .replace("&amp;", "&");
        let (signed, sig) = href.split_once("&s=").unwrap();
        assert_eq!(signed, "/error?message=Invalid+email.");
        let der = base64::engine::general_purpose::URL_SAFE
            .decode(sig)
            .unwrap();
        let sig = p256::ecdsa::Signature::from_der(&der).unwrap();
        let digest = sha2::Sha256::digest(signed.as_bytes());
        assert!(key.verifying_key().verify_prehash(&digest, &sig).is_ok());
    }
}
