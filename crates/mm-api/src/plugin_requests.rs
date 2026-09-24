//! The plugin HTTP subrouter Go registers in `NewChannels` (app/channels.go:239-242):
//!
//! ```text
//! /plugins/{plugin_id:[A-Za-z0-9\_\-\.]+}                     ServePluginRequest
//! /plugins/{plugin_id:[A-Za-z0-9\_\-\.]+}/public/{file:.*}   ServePluginPublicRequest
//! /plugins/{plugin_id:[A-Za-z0-9\_\-\.]+}/{anything:.*}      ServePluginRequest
//! ```
//!
//! any method, under the subpath. **Served here only when this process hosts the plugins**
//! (`MMRS_PLUGIN_HOST=rust`); under the Go host the plugin is Go's and `mm_api::web_static`
//! forwards the prefix, as it did before. That was decided before this was written: nothing here
//! needs private code, and the Go process under the Rust host has no plugins to answer with.
//!
//! The request half, the session and the response framing are `mm_app::plugin_requests`; this
//! module matches the path, turns the axum request into what `net/http` would have handed Go's
//! handler, and the answer back into a response. A path gorilla would not match — an id outside
//! the character class, or a rest holding a newline (`.*` does not match one) — falls through to
//! the routes after it, and is forwarded.
//!
//! # Public files
//!
//! `ServePluginPublicRequest` is `http.ServeFile` over the running plugin's `public` directory
//! ([`web_static::serve_file`](crate::web_static)), with its three 404s first: a trailing slash,
//! plugins off or the plugin not running, and a cleaned path outside the prefix. A multi-range
//! request is the file server's one answer not ported, and is forwarded — to a Go that has no
//! such plugin, so it is Go's 404 ([D-1062]).

use std::net::SocketAddr;
use std::path::Path;

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Version, header};
use axum::response::Response;
use futures_util::TryStreamExt as _;
use mm_app::plugin_requests::{PluginHttpAnswer, PluginHttpRequest, canonical_header_key};
use mm_model::go_path;
use mm_model::go_url::GoUrl;

use crate::AppState;
use crate::serve_content::{build_response, serve_error};
use crate::web_static::{WebOwnHeaders, head_framing, serve_file};

/// Which of the subrouter's handlers a path reaches, with the plugin id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRoute {
    /// `ServePluginRequest`.
    Request(String),
    /// `ServePluginPublicRequest`.
    Public(String),
}

/// `[A-Za-z0-9\_\-\.]`.
fn is_plugin_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Gorilla's match of the three routes against `rel`, the decoded path below the subpath.
pub fn plugin_route(rel: &str) -> Option<PluginRoute> {
    let rest = rel.strip_prefix("/plugins/")?;
    let (id, tail) = match rest.find('/') {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    if id.is_empty() || !id.bytes().all(is_plugin_id_byte) {
        return None;
    }
    match tail {
        None => Some(PluginRoute::Request(id.to_owned())),
        // `.*` stops at a newline, and the route then does not match at all.
        Some(tail) if tail.contains('\n') => None,
        Some(tail) if tail.starts_with("public/") => Some(PluginRoute::Public(id.to_owned())),
        Some(_) => Some(PluginRoute::Request(id.to_owned())),
    }
}

/// Serve a request the subrouter matched.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    state: &AppState,
    subpath: &str,
    route: PluginRoute,
    parts: Parts,
    body: Body,
    url: GoUrl,
    raw_target: &str,
    path: &str,
) -> Response {
    let response = match route {
        PluginRoute::Request(id) => {
            let request = plugin_http_request(&parts, url, raw_target);
            let body = tokio_util::io::StreamReader::new(
                body.into_data_stream().map_err(std::io::Error::other),
            );
            let answer = state
                .app
                .serve_plugin_request(&id, request, Box::new(body), subpath)
                .await;
            into_response(answer)
        }
        PluginRoute::Public(id) => {
            match serve_public(state, subpath, &id, path, &url.raw_query, &parts).await {
                Some(response) => response,
                None => {
                    let request = axum::extract::Request::from_parts(parts, body);
                    return crate::proxy::forward_to_go(
                        axum::extract::State(state.clone()),
                        request,
                    )
                    .await;
                }
            }
        }
    };
    let mut response = head_framing(&parts.method, response);
    response.extensions_mut().insert(WebOwnHeaders);
    response
}

/// The request as `net/http` hands it to a handler: the header with canonical keys and without
/// `Host` or `Transfer-Encoding`, `Host` from the authority or the header, the peer as
/// `RemoteAddr`, and the raw target as `RequestURI`.
fn plugin_http_request(parts: &Parts, url: GoUrl, raw_target: &str) -> PluginHttpRequest {
    let mut header = std::collections::HashMap::new();
    for name in parts.headers.keys() {
        if name == header::HOST || name == header::TRANSFER_ENCODING {
            continue;
        }
        let values = parts
            .headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        header.insert(canonical_header_key(name.as_str()), values);
    }
    let host = parts
        .uri
        .authority()
        .map(|a| a.as_str().to_owned())
        .or_else(|| {
            parts
                .headers
                .get(header::HOST)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        })
        .unwrap_or_default();
    let (proto, major, minor) = match parts.version {
        Version::HTTP_09 => ("HTTP/0.9", 0, 9),
        Version::HTTP_10 => ("HTTP/1.0", 1, 0),
        Version::HTTP_2 => ("HTTP/2.0", 2, 0),
        Version::HTTP_3 => ("HTTP/3.0", 3, 0),
        _ => ("HTTP/1.1", 1, 1),
    };
    PluginHttpRequest {
        method: parts.method.as_str().to_owned(),
        url,
        proto: proto.to_owned(),
        proto_major: major,
        proto_minor: minor,
        header,
        host,
        remote_addr: parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.to_string())
            .unwrap_or_default(),
        request_uri: raw_target.to_owned(),
        context: crate::plugin_context::hook_context(parts, None),
    }
}

/// The answer as the client receives it. `Header.writeSubset`'s rules: a key that is not a valid
/// field name is dropped, and a value has CR and LF made spaces and its ends trimmed.
fn into_response(answer: PluginHttpAnswer) -> Response {
    let mut headers = HeaderMap::new();
    for (key, values) in &answer.header {
        let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else {
            continue;
        };
        for value in values {
            let value = value.replace(['\r', '\n'], " ");
            let value = value.trim_matches([' ', '\t']);
            match HeaderValue::from_bytes(value.as_bytes()) {
                Ok(value) => {
                    headers.append(name.clone(), value);
                }
                Err(_) => tracing::debug!(key, "dropping a header value hyper refuses"),
            }
        }
    }
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::OK);
    let body = if answer.done {
        Body::from(answer.first)
    } else {
        let first = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(Bytes::from(
            answer.first,
        ))]);
        let rest = futures_util::stream::unfold(answer.rest, |mut rest| async move {
            rest.recv()
                .await
                .map(|chunk| (Ok::<_, std::convert::Infallible>(Bytes::from(chunk)), rest))
        });
        Body::from_stream(futures_util::StreamExt::chain(first, rest))
    };
    build_response(status, headers, body)
}

/// Go's `http.NotFound`.
fn not_found() -> Response {
    serve_error(
        HeaderMap::new(),
        "404 page not found",
        StatusCode::NOT_FOUND,
    )
}

/// Port of `Channels.ServePluginPublicRequest` (app/plugin_requests.go:116) and `http.ServeFile`.
/// `path` is the decoded request path. `None` forwards (see the module docs).
async fn serve_public(
    state: &AppState,
    subpath: &str,
    plugin_id: &str,
    path: &str,
    raw_query: &str,
    parts: &Parts,
) -> Option<Response> {
    if path.ends_with('/') {
        return Some(not_found());
    }
    let Some(environment) = state.app.plugins_environment() else {
        return Some(not_found());
    };
    let Ok(public_files_path) = environment.public_files_path(plugin_id) else {
        return Some(not_found());
    };
    let public_file_path = go_path::clean(path);
    let prefix = go_path::join(&[subpath, "plugins", plugin_id, "public"]);
    let Some(rest) = public_file_path.strip_prefix(prefix.as_str()) else {
        return Some(not_found());
    };
    let public_file = go_path::join(&[&public_files_path.to_string_lossy(), rest]);

    // `http.ServeFile`.
    if contains_dot_dot(path) {
        return Some(serve_error(
            HeaderMap::new(),
            "invalid URL path",
            StatusCode::BAD_REQUEST,
        ));
    }
    let (dir, file) = match public_file.rfind('/') {
        Some(at) => (&public_file[..=at], &public_file[at + 1..]),
        None => ("", public_file.as_str()),
    };
    serve_file(
        Path::new(dir),
        file,
        path,
        raw_query,
        false,
        parts,
        HeaderMap::new(),
    )
    .await
}

/// `containsDotDot` (net/http/fs.go): a `..` path element.
fn contains_dot_dot(v: &str) -> bool {
    v.contains("..") && v.split(['/', '\\']).any(|element| element == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_routes_match_as_gorilla_matches_them() {
        let request = |id: &str| Some(PluginRoute::Request(id.to_owned()));
        assert_eq!(
            plugin_route("/plugins/com.ex-am_ple"),
            request("com.ex-am_ple")
        );
        assert_eq!(plugin_route("/plugins/p/"), request("p"));
        assert_eq!(plugin_route("/plugins/p/a/b?c"), request("p"));
        assert_eq!(plugin_route("/plugins/p/public"), request("p"));
        assert_eq!(
            plugin_route("/plugins/p/public/"),
            Some(PluginRoute::Public("p".to_owned()))
        );
        assert_eq!(
            plugin_route("/plugins/p/public/img/x.png"),
            Some(PluginRoute::Public("p".to_owned()))
        );
        // Not gorilla's: the next route (the web client) takes them.
        assert_eq!(plugin_route("/plugins/"), None);
        assert_eq!(plugin_route("/plugins"), None);
        assert_eq!(plugin_route("/plugins/a~b/x"), None);
        assert_eq!(plugin_route("/plugins/p/a\nb"), None);
        assert_eq!(plugin_route("/plugins/p/public/a\nb"), None);
        assert_eq!(plugin_route("/Plugins/p/x"), None);
    }

    #[test]
    fn dot_dot_is_an_element_not_a_substring() {
        assert!(contains_dot_dot("/a/../b"));
        assert!(contains_dot_dot(".."));
        assert!(contains_dot_dot("/a\\..\\b"));
        assert!(!contains_dot_dot("/a/..b/c"));
        assert!(!contains_dot_dot("/a/b.."));
    }

    fn parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().method("POST").uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).expect("a request").into_parts().0
    }

    #[test]
    fn the_handler_sees_canonical_keys_and_no_host() {
        let p = parts(
            "/plugins/p/x?a=1",
            &[
                ("host", "example.com:8065"),
                ("x-custom-thing", "1"),
                ("x-custom-thing", "2"),
                ("transfer-encoding", "chunked"),
                ("www-authenticate", "w"),
            ],
        );
        let url = mm_model::go_url::parse_request_uri("/plugins/p/x?a=1").expect("a url");
        let request = plugin_http_request(&p, url, "/plugins/p/x?a=1");
        assert_eq!(request.host, "example.com:8065");
        assert_eq!(request.header.get("Host"), None);
        assert_eq!(request.header.get("Transfer-Encoding"), None);
        assert_eq!(
            request.header.get("X-Custom-Thing"),
            Some(&vec!["1".to_owned(), "2".to_owned()])
        );
        assert!(request.header.contains_key("Www-Authenticate"));
        assert_eq!(request.request_uri, "/plugins/p/x?a=1");
        assert_eq!(
            (request.proto.as_str(), request.proto_major),
            ("HTTP/1.1", 1)
        );
        assert_eq!(request.remote_addr, "", "no peer, as over the local socket");
    }

    #[tokio::test]
    async fn a_value_loses_its_newlines_and_a_bad_key_is_dropped() {
        let (tx, rest) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let answer = PluginHttpAnswer {
            status: 202,
            header: [
                ("X-A".to_owned(), vec![" a\r\nb ".to_owned()]),
                ("Bad Key".to_owned(), vec!["x".to_owned()]),
            ]
            .into_iter()
            .collect(),
            first: b"body".to_vec(),
            done: true,
            rest,
        };
        let response = into_response(answer);
        assert_eq!(response.status(), 202);
        assert_eq!(response.headers()["x-a"], "a  b");
        assert_eq!(response.headers().len(), 2, "x-a and the served-by marker");
    }
}
