//! The client's IP address as Go derives it: `utils.GetIPAddress` over
//! `ServiceSettings.TrustedProxyIPHeader`, computed once per request.
//!
//! Go computes it in `web.Handler.ServeHTTP` (web/handlers.go:195) and carries it on the
//! request context, where `pluginContext`, `MakeAuditRecord`, `LogAudit` and the websocket
//! upgrade read it. [`stamp_client_ip`] is that step: a middleware on both routers that stores
//! the answer as a [`ClientIp`] request extension, read back by [`client_ip`]. Of Go's readers,
//! this server has the plugin hook context ([`crate::plugin_context`]) and the `Audits` rows its
//! handlers write through [`crate::audit_log`]; session attributes are not written here at all. The rate limiter
//! ([`crate::ratelimit`]) keys with [`get_ip_address`] directly, over the trusted headers it was
//! built with, as Go's does.
//!
//! # The peer half
//!
//! Go's fallback is the host of `r.RemoteAddr`, which `net/http` sets to the peer
//! `TCPAddr.String()`: `IP.String()` prints a v4-mapped address as dotted IPv4, so a dual-stack
//! listener's `::ffff:10.1.2.3` is `10.1.2.3`, which [`std::net::IpAddr::to_canonical`]
//! reproduces. A request over the local socket has no `ConnectInfo`, and Go's `SplitHostPort` of a
//! unix `RemoteAddr` fails and yields `""` — so both answer `""` there, but the header walk still
//! runs first, as Go's does.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Extensions, HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;

/// The client address [`stamp_client_ip`] derived for this request. Port of what
/// `request.CTX.IPAddress()` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIp(pub String);

/// Port of `utils.GetIPAddress` (channels/utils/utils.go:94).
///
/// For each configured header, in configuration order: `r.Header.Get` (the **first** line of that
/// name, matched case-insensitively — a configured name that is not a valid field name, such as
/// `""`, finds nothing on either side), the part before the first `,`, `strings.TrimSpace`d. The
/// first such value `net.ParseIP` accepts is returned **as written**, not re-formatted. Only then
/// the peer's host. `address` survives across iterations exactly as Go's does; it cannot change
/// the answer, because a value that failed to parse fails again.
///
/// Parseability is [`IpAddr`]'s `FromStr`, which agrees with `net.ParseIP` (`netip.ParseAddr`
/// minus zones) on every row of `fixtures/behaviour_ip_address.json`: no port, no brackets, no
/// zone, no leading-zero octet, IPv4-embedded IPv6 accepted. `TrimSpace` is Unicode whitespace,
/// as `str::trim` is; the value's bytes are decoded lossily first, since `net/http` hands a
/// handler obs-text bytes verbatim and an invalid sequence can never parse as an address anyway.
pub fn get_ip_address(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_ip_header: &[String],
) -> String {
    let mut address = String::new();

    for proxy_header in trusted_proxy_ip_header {
        let header = headers
            .get(proxy_header.as_str())
            .map(|value| value.as_bytes())
            .unwrap_or_default();
        if !header.is_empty() {
            let first = header.split(|b| *b == b',').next().unwrap_or_default();
            address = String::from_utf8_lossy(first).trim().to_owned();
        }

        if !address.is_empty() && address.parse::<IpAddr>().is_ok() {
            return address;
        }
    }

    peer.map(|addr| addr.ip().to_canonical().to_string())
        .unwrap_or_default()
}

/// The header the forward leg must set so that the Go process behind this one derives the same
/// client address this one did — or `None` when forwarding the client's headers verbatim already
/// does. [D-1150].
///
/// Go walks `TrustedProxyIPHeader` and falls back to the peer, and behind this server the peer is
/// **this server**. So:
///
/// - no trusted header configured: nothing Go would read can carry the client, and nothing is
///   set — Go keys every forwarded request on this server's address, exactly as it would behind
///   any other proxy it was not told to trust. The operator's remedy is Go's own: configure the
///   header (`ratelimit` logs it at start when rate limiting is on);
/// - a configured header held a parseable address: the client's headers are forwarded verbatim,
///   and Go's walk finds the same one;
/// - none did, so the address was the peer's: the **first** configured name that is a valid
///   header name is set to it, replacing whatever unparseable value it held. Go's walk reaches it
///   before any other that could parse — names before it are ones no request can carry — and
///   returns it as written, which is what this side computed.
///
/// The one thing Go sees differently is that header's raw value where the client sent none: with
/// `X-Forwarded-For` first, `request.CTX`'s `XForwardedFor` is the peer rather than `""`.
pub fn forwarded_address_header(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_ip_header: &[String],
) -> Option<(HeaderName, HeaderValue)> {
    let peer = peer?;
    let from_header = get_ip_address(headers, None, trusted_proxy_ip_header);
    if !from_header.is_empty() {
        return None;
    }
    let name = trusted_proxy_ip_header
        .iter()
        .find_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())?;
    let value = HeaderValue::from_str(&peer.ip().to_canonical().to_string()).ok()?;
    Some((name, value))
}

/// The peer as `net/http` formats `r.RemoteAddr` (`TCPAddr.String()`): a v4-mapped peer is
/// written as IPv4. `""` with no peer, as for the local socket.
pub fn go_remote_addr(extensions: &Extensions) -> String {
    peer(extensions)
        .map(|addr| SocketAddr::new(addr.ip().to_canonical(), addr.port()).to_string())
        .unwrap_or_default()
}

fn peer(extensions: &Extensions) -> Option<SocketAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
}

/// The request's client address: the [`ClientIp`] [`stamp_client_ip`] stored, or — for a
/// request that never passed through it, such as a unit test's — the same walk over no trusted
/// headers, which is Go's default configuration.
pub fn client_ip(headers: &HeaderMap, extensions: &Extensions) -> String {
    match extensions.get::<ClientIp>() {
        // Cloned because the caller owns the result (`HookContext::ip_address`) while the request
        // keeps its extension.
        Some(ClientIp(ip)) => ip.clone(),
        None => get_ip_address(headers, peer(extensions), &[]),
    }
}

/// The `request.NewContext(…, utils.GetIPAddress(r, TrustedProxyIPHeader), …)` step of
/// `web.Handler.ServeHTTP` (web/handlers.go:192), for every request on the router it wraps.
pub(crate) async fn stamp_client_ip(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = get_ip_address(
        request.headers(),
        peer(request.extensions()),
        &state.app.config().trusted_proxy_ip_header,
    );
    request.extensions_mut().insert(ClientIp(ip));
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer_v4() -> Option<SocketAddr> {
        Some(SocketAddr::from(([10, 0, 0, 1], 1_u16)))
    }

    fn trusted(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_extension_wins_and_its_absence_is_the_peer() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let mut extensions = Extensions::new();
        extensions.insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9_u16))));
        assert_eq!(client_ip(&headers, &extensions), "127.0.0.1");
        extensions.insert(ClientIp("5.6.7.8".to_owned()));
        assert_eq!(client_ip(&headers, &extensions), "5.6.7.8");
        assert_eq!(client_ip(&headers, &Extensions::new()), "");
    }

    #[test]
    fn go_remote_addr_writes_a_mapped_peer_as_ipv4() {
        let mut extensions = Extensions::new();
        let mapped: SocketAddr = "[::ffff:10.1.2.3]:80".parse().unwrap();
        extensions.insert(ConnectInfo(mapped));
        assert_eq!(go_remote_addr(&extensions), "10.1.2.3:80");
        let v6: SocketAddr = "[2001:db8::7]:443".parse().unwrap();
        extensions.insert(ConnectInfo(v6));
        assert_eq!(go_remote_addr(&extensions), "[2001:db8::7]:443");
        assert_eq!(go_remote_addr(&Extensions::new()), "");
    }

    /// The forward leg's header, each branch: nothing configured, a configured header that parsed,
    /// none that did (the first valid name gets the peer, replacing a bad value), no peer.
    #[test]
    fn the_forwarded_header_carries_the_peer_only_when_go_would_fall_back_to_it() {
        let peer: Option<SocketAddr> = Some("[::ffff:10.1.2.3]:9".parse().unwrap());
        let mut headers = HeaderMap::new();
        assert_eq!(forwarded_address_header(&headers, peer, &[]), None);
        let xff = trusted(&["X-Forwarded-For"]);
        let (name, value) = forwarded_address_header(&headers, peer, &xff).expect("set");
        assert_eq!(
            (name.as_str(), value.to_str().unwrap()),
            ("x-forwarded-for", "10.1.2.3")
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("junk"));
        assert!(
            forwarded_address_header(&headers, peer, &xff).is_some(),
            "a bad value"
        );
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        assert_eq!(
            forwarded_address_header(&headers, peer, &xff),
            None,
            "Go finds it too"
        );
        assert_eq!(
            forwarded_address_header(&HeaderMap::new(), None, &xff),
            None
        );
        // A name no request can carry is skipped for the next.
        let (name, _) = forwarded_address_header(
            &HeaderMap::new(),
            peer,
            &trusted(&["", "X-Real-IP", "X-Forwarded-For"]),
        )
        .expect("set");
        assert_eq!(name.as_str(), "x-real-ip");
        // A later header that parses wins in Go too: nothing to set.
        let mut real = HeaderMap::new();
        real.insert("x-forwarded-for", HeaderValue::from_static("9.9.9.9"));
        assert_eq!(
            forwarded_address_header(&real, peer, &trusted(&["X-Real-IP", "X-Forwarded-For"])),
            None
        );
    }

    /// A non-UTF-8 byte after the first comma does not spoil the first element.
    #[test]
    fn obs_text_after_the_first_element_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"1.2.3.4, \xff").unwrap(),
        );
        assert_eq!(
            get_ip_address(&headers, peer_v4(), &trusted(&["X-Forwarded-For"])),
            "1.2.3.4"
        );
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"\xff1.2.3.4").unwrap(),
        );
        assert_eq!(
            get_ip_address(&headers, peer_v4(), &trusted(&["X-Forwarded-For"])),
            "10.0.0.1"
        );
    }

    /// Every row of `fixtures/behaviour_ip_address.json`, produced by Go's own
    /// `utils.GetIPAddress` over requests read by `http.ReadRequest`.
    mod go_parity {
        use super::*;

        #[test]
        fn every_row_matches_go() {
            let oracle: serde_json::Value =
                serde_json::from_str(include_str!("../../../fixtures/behaviour_ip_address.json"))
                    .expect("behaviour_ip_address.json is generated by reference/dump");
            let rows = oracle["get_ip_address"].as_array().expect("rows");
            assert!(rows.len() >= 70, "the corpus is all there");
            for row in rows {
                let name = row["name"].as_str().unwrap();
                let mut headers = HeaderMap::new();
                for pair in row["headers"].as_array().unwrap() {
                    let key = pair[0].as_str().unwrap();
                    // What hyper hands a handler: the value with its surrounding SP/HTAB gone,
                    // exactly what `textproto` trims on Go's side.
                    let value = pair[1].as_str().unwrap().trim_matches([' ', '\t']);
                    headers.append(
                        axum::http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                        HeaderValue::from_bytes(value.as_bytes()).unwrap(),
                    );
                }
                let peer_ip = row["peer_ip"].as_str().unwrap();
                let peer = (!peer_ip.is_empty()).then(|| {
                    let port = u16::try_from(row["peer_port"].as_u64().unwrap()).unwrap();
                    SocketAddr::new(peer_ip.parse().unwrap(), port)
                });
                let trusted: Vec<String> = row["trusted"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();

                assert_eq!(
                    get_ip_address(&headers, peer, &trusted),
                    row["want"].as_str().unwrap(),
                    "{name}: {row}"
                );

                let mut extensions = Extensions::new();
                if let Some(addr) = peer {
                    extensions.insert(ConnectInfo(addr));
                    assert_eq!(
                        go_remote_addr(&extensions),
                        row["remote_addr"].as_str().unwrap(),
                        "{name}: RemoteAddr"
                    );
                }
            }
        }
    }
}
