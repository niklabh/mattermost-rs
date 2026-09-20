//! The `plugin.Context` every write path hands its plugin hooks, built from the HTTP request.
//!
//! Go builds it once per request in `web.Handler.ServeHTTP` (`web/handlers.go:191-205`) and
//! carries it on `request.CTX`; `pluginContext(rctx)` (`app/context.go:41`) then copies six of
//! its fields into the value the hook sees. There is no request context in this tree, so
//! [`hook_context`] is that copy, made in the handler and passed down as an argument.
//!
//! # `request_id` is minted here, not taken from the client
//!
//! `requestID := model.NewId()` — the `X-Request-ID` a client sends is never read. It is echoed
//! to a plugin and to nothing else on these paths, so it is the one field of the six a
//! cross-server diff cannot compare.
//!
//! # `ip_address`
//!
//! Port of `utils.GetIPAddress` (channels/utils/utils.go:94), which walks
//! `ServiceSettings.TrustedProxyIPHeader` for the first header holding a parseable address and
//! otherwise takes the host of `RemoteAddr`. **That list is not modelled in this server's
//! configuration** and its Go default is empty (`config.go:679`), so only the `RemoteAddr` half
//! is ported and a stock server matches exactly; see `docs/TECH_DEBT.md` [D-930].
//!
//! The peer address comes from axum's `ConnectInfo`, which `mm_api::main` installs on the TCP
//! listener. A request over the local socket has none, and Go's `net.SplitHostPort` of a unix
//! `RemoteAddr` also fails and yields `""`, so both answer the empty string there.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use axum::http::request::Parts;

use mm_app::plugin_hooks::HookContext;
use mm_model::session::Session;

/// `model.ConnectionId` (model/client4.go:51).
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// Port of `pluginContext(rctx)` over the request that is being served.
pub fn hook_context(parts: &Parts, session: Option<&Session>) -> HookContext {
    build(&parts.headers, &parts.extensions, session)
}

/// [`hook_context`] for a handler that still holds the whole request.
pub fn hook_context_of<B>(
    request: &axum::http::Request<B>,
    session: Option<&Session>,
) -> HookContext {
    build(request.headers(), request.extensions(), session)
}

fn build(
    headers: &axum::http::HeaderMap,
    extensions: &axum::http::Extensions,
    session: Option<&Session>,
) -> HookContext {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    HookContext {
        request_id: mm_model::utils::new_id(),
        session_id: session.map(|s| s.id.clone()).unwrap_or_default(),
        // The `RemoteAddr` half of `utils.GetIPAddress`: `net.SplitHostPort(r.RemoteAddr)`,
        // whose error Go discards, so no peer address is the empty string.
        ip_address: extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip().to_string())
            .unwrap_or_default(),
        accept_language: header("Accept-Language"),
        user_agent: header("User-Agent"),
        connection_id: header(CONNECTION_ID_HEADER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_with(headers: &[(&str, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().uri("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("a request").into_parts().0
    }

    #[test]
    fn the_six_fields_come_from_the_request_and_the_session() {
        let parts = parts_with(&[
            ("Accept-Language", "fr-CA,fr;q=0.9"),
            ("User-Agent", "mmrs-test/1.0"),
            ("Connection-Id", "abc123"),
            // Never read: Go mints its own.
            ("X-Request-ID", "client-supplied"),
        ]);
        let session = Session {
            id: "sessionid123".to_owned(),
            ..Session::default()
        };
        let ctx = hook_context(&parts, Some(&session));
        assert_eq!(ctx.session_id, "sessionid123");
        assert_eq!(ctx.accept_language, "fr-CA,fr;q=0.9");
        assert_eq!(ctx.user_agent, "mmrs-test/1.0");
        assert_eq!(ctx.connection_id, "abc123");
        assert_eq!(ctx.request_id.len(), 26, "model.NewId()");
        assert_ne!(ctx.request_id, "client-supplied");
        // No `ConnectInfo`: the local socket, and Go's `SplitHostPort` failure.
        assert_eq!(ctx.ip_address, "");
    }

    #[test]
    fn an_absent_header_is_the_empty_string_not_a_missing_field() {
        let ctx = hook_context(&parts_with(&[]), None);
        assert_eq!(ctx.session_id, "");
        assert_eq!(ctx.accept_language, "");
        assert_eq!(ctx.user_agent, "");
        assert_eq!(ctx.connection_id, "");
    }

    #[test]
    fn the_peer_address_is_the_host_without_its_port() {
        let mut parts = parts_with(&[]);
        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54_321_u16))));
        assert_eq!(hook_context(&parts, None).ip_address, "127.0.0.1");
    }
}
