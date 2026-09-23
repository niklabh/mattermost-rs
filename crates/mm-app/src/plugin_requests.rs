//! Port of `Channels.ServePluginRequest`, `servePluginRequest` and
//! `validateCSRFForPluginRequest` (app/plugin_requests.go:23, :156, :273) — a client's HTTP
//! request to a plugin's `ServeHTTP` — with the part of `net/http`'s server that decides what the
//! client receives from what the plugin wrote ([`GoResponseWriter`]).
//!
//! # Only under the Rust plugin host
//!
//! Decided before any of this was written: `mm_api` routes `/plugins/{plugin_id}` here only when
//! this process hosts the plugins (`MMRS_PLUGIN_HOST=rust`). Under the Go host the plugin runs in
//! Go, so the request is forwarded, as it always was. Nothing here depends on private code.
//!
//! # The request the plugin sees
//!
//! In Go's order: the token from `Authorization: Bearer`, then `Authorization: token`, then the
//! `MMAUTHTOKEN` cookie (which alone makes it a *cookie* request), then `?access_token=` — the
//! header wins over the cookie here, the reverse of the REST API's `ParseAuthTokenFromRequest`.
//! Then, whatever the token turns out to be: `Mattermost-Plugin-Id` and `Mattermost-User-Id` go,
//! the `Cookie` header is rebuilt from every cookie but `MMAUTHTOKEN` (as one line, the values
//! re-quoted by `AddCookie`'s rules), `Referer` goes, the query loses `access_token` and is
//! re-encoded sorted by key, and the path loses `<subpath>/plugins/<id>`. `RequestURI` is **not**
//! scrubbed: Go hands the plugin the raw request target, `access_token` and all.
//!
//! A token that resolves drops the `Authorization` header before MFA and CSRF are judged, so a
//! session that then fails either reaches the plugin unauthenticated *and* without its header.
//! Only a session that passes both sets `Mattermost-User-Id` and the context's `SessionId`.
//!
//! # The CSRF check reads the body
//!
//! A cookie request other than `GET` with no `X-CSRF-Token` header has its body read whole to look
//! for a `csrf` form field (`ParseForm`: a url-encoded body of a `POST`, `PUT` or `PATCH` up to
//! 10 MB, then the query), and the plugin is handed the bytes that were read. Every other request
//! streams its body to the plugin as the plugin reads it. `X-Requested-With: XMLHttpRequest`
//! passes a failed check unless `ExperimentalStrictCSRFEnforcement` is on.
//!
//! # What this does not do
//!
//! A personal access token is not a session here (`App::get_session` does not mint one, [D-1061]),
//! so such a request reaches the plugin unauthenticated where Go's would carry the user. A plugin
//! cannot hijack the connection (a websocket served by a plugin), [D-1060]. Trailers are dropped.

use std::collections::HashMap;

use mm_model::go_path;
use mm_model::go_url::{self, GoUrl};
use mm_model::session::Session;
use mm_plugin::http::ResponseWriter;
use mm_plugin::wire::http::Header;
use mm_plugin::wire::plugin::HTTPRequestSubset;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, oneshot};

use crate::App;
use crate::plugin_hooks::HookContext;

/// `model.SessionCookieToken`.
pub const SESSION_COOKIE_TOKEN: &str = "MMAUTHTOKEN";
/// `bufferBeforeChunkingSize` (net/http/server.go): what `net/http` holds before it must decide
/// the head, and so whether it can still send a `Content-Length`.
const BUFFER_BEFORE_CHUNKING: usize = 2048;
/// `defaultCookieMaxNum` (net/http/cookie.go): more cookies than this and `readCookies` reads none.
const COOKIE_MAX_NUM: usize = 3000;
/// `parsePostForm`'s cap on a url-encoded body.
const MAX_FORM_SIZE: usize = 10 << 20;

/// A request body the plugin reads from.
pub type RequestBody = Box<dyn AsyncRead + Send + Unpin>;

/// The request as `net/http` hands it to Go's handler, before `servePluginRequest` touches it.
#[derive(Debug, Clone, Default)]
pub struct PluginHttpRequest {
    pub method: String,
    /// `r.URL`: the parsed request target, its path decoded.
    pub url: GoUrl,
    pub proto: String,
    pub proto_major: i64,
    pub proto_minor: i64,
    /// `r.Header`: canonical keys, and neither `Host` nor `Transfer-Encoding`, which `net/http`
    /// takes out of the map.
    pub header: Header,
    pub host: String,
    pub remote_addr: String,
    pub request_uri: String,
    /// The `plugin.Context` fields that come from the request; `session_id` is set here.
    pub context: HookContext,
}

/// The head of what the client receives, taken when `net/http` would have written it.
#[derive(Debug)]
pub struct PluginHttpAnswer {
    pub status: u16,
    /// The plugin's header as it stood at the status, with `net/http`'s `Content-Length` and
    /// sniffed `Content-Type` added and a no-body status's suppressed headers removed.
    pub header: Header,
    /// The bytes written before the head was taken.
    pub first: Vec<u8>,
    /// The handler had returned: `first` is the whole body.
    pub done: bool,
    /// The rest of the body, as the plugin writes it.
    pub rest: mpsc::UnboundedReceiver<Vec<u8>>,
}

// ---------------------------------------------------------------------------------------------
// Headers and cookies
// ---------------------------------------------------------------------------------------------

/// `httpguts.IsTokenRune` over a byte: RFC 7230's `tchar`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Port of `textproto.CanonicalMIMEHeaderKey`: a key holding anything but token bytes is left as
/// it is; otherwise each dash-separated word is capitalised.
pub fn canonical_header_key(key: &str) -> String {
    if !key.bytes().all(is_token_byte) {
        return key.to_owned();
    }
    let mut upper = true;
    key.bytes()
        .map(|b| {
            let c = if upper {
                b.to_ascii_uppercase()
            } else {
                b.to_ascii_lowercase()
            };
            upper = b == b'-';
            c as char
        })
        .collect()
}

/// `Header.Get`: the first value, or empty.
fn header_get<'a>(header: &'a Header, key: &str) -> &'a str {
    header
        .get(&canonical_header_key(key))
        .and_then(|v| v.first())
        .map_or("", String::as_str)
}

/// `Header.Del`.
fn header_del(header: &mut Header, key: &str) {
    header.remove(&canonical_header_key(key));
}

/// `Header.Set`.
fn header_set(header: &mut Header, key: &str, value: &str) {
    header.insert(canonical_header_key(key), vec![value.to_owned()]);
}

/// `textproto.TrimString`: ASCII space, tab, CR and LF off both ends.
fn trim_string(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

/// A cookie as `readCookies` parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub quoted: bool,
}

/// `validCookieValueByte`.
fn valid_cookie_value_byte(b: u8) -> bool {
    (0x20..0x7f).contains(&b) && b != b'"' && b != b';' && b != b'\\'
}

/// Port of `parseCookieValue(raw, true)`: one pair of surrounding quotes comes off, and any byte
/// outside the cookie-octet set (space and comma allowed) refuses the cookie.
fn parse_cookie_value(raw: &str) -> Option<(String, bool)> {
    let (raw, quoted) = if raw.len() > 1 && raw.starts_with('"') && raw.ends_with('"') {
        (&raw[1..raw.len() - 1], true)
    } else {
        (raw, false)
    };
    raw.bytes()
        .all(valid_cookie_value_byte)
        .then(|| (raw.to_owned(), quoted))
}

/// Port of `readCookies` (net/http/cookie.go:371): every `Cookie` line, split on `;`, each part
/// a token name and a valid value, optionally only those named `filter`. More than
/// [`COOKIE_MAX_NUM`] parts across the lines and there are none at all.
pub fn read_cookies(header: &Header, filter: &str) -> Vec<Cookie> {
    let Some(lines) = header.get("Cookie") else {
        return Vec::new();
    };
    let count: usize = lines.iter().map(|l| l.matches(';').count() + 1).sum();
    if count > COOKIE_MAX_NUM {
        return Vec::new();
    }
    let mut cookies = Vec::new();
    for line in lines {
        for part in trim_string(line).split(';') {
            let part = trim_string(part);
            if part.is_empty() {
                continue;
            }
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            let name = trim_string(name);
            if name.is_empty() || !name.bytes().all(is_token_byte) {
                continue;
            }
            if !filter.is_empty() && filter != name {
                continue;
            }
            if let Some((value, quoted)) = parse_cookie_value(value) {
                cookies.push(Cookie {
                    name: name.to_owned(),
                    value,
                    quoted,
                });
            }
        }
    }
    cookies
}

/// Port of `sanitizeCookieValue`: invalid bytes dropped, and quotes around a value that holds a
/// space or a comma or arrived quoted.
fn sanitize_cookie_value(value: &str, quoted: bool) -> String {
    let value: String = if value.bytes().all(valid_cookie_value_byte) {
        value.to_owned()
    } else {
        value
            .chars()
            .filter(|c| c.is_ascii() && valid_cookie_value_byte(*c as u8))
            .collect()
    };
    if quoted || value.contains([' ', ',']) {
        format!("\"{value}\"")
    } else {
        value
    }
}

/// Port of `Request.AddCookie`: onto the one `Cookie` line, after a `; `.
fn add_cookie(header: &mut Header, cookie: &Cookie) {
    let name = cookie.name.replace(['\n', '\r'], "-");
    let pair = format!(
        "{name}={}",
        sanitize_cookie_value(&cookie.value, cookie.quoted)
    );
    let existing = header_get(header, "Cookie");
    let line = if existing.is_empty() {
        pair
    } else {
        format!("{existing}; {pair}")
    };
    header_set(header, "Cookie", &line);
}

// ---------------------------------------------------------------------------------------------
// servePluginRequest's request half
// ---------------------------------------------------------------------------------------------

/// The token and whether it came from the cookie (plugin_requests.go:171-185): `Bearer ` and
/// `token ` compared case-insensitively on the whole header, then the cookie, then the query. The
/// value arrives trimmed, so a bare `Bearer` matches neither prefix and falls to the cookie.
pub fn plugin_request_token(header: &Header, url: &GoUrl) -> (String, bool) {
    const BEARER: &str = "BEARER ";
    const TOKEN: &str = "token ";
    let auth = header_get(header, "Authorization");
    // Go upper- and lower-cases the whole header and slices the original by the prefix's byte
    // length: a prefix reached through a non-ASCII fold (the Kelvin sign lower-cases to `k`)
    // slices mid-character, which no session token can match either way.
    let slice =
        |at: usize| String::from_utf8_lossy(&auth.as_bytes()[at.min(auth.len())..]).into_owned();
    if auth.to_uppercase().starts_with(BEARER) {
        return (slice(BEARER.len()), false);
    }
    if auth.to_lowercase().starts_with(TOKEN) {
        return (slice(TOKEN.len()), false);
    }
    if let Some(cookie) = read_cookies(header, SESSION_COOKIE_TOKEN)
        .into_iter()
        .next()
    {
        return (cookie.value, true);
    }
    let token = url
        .query()
        .get("access_token")
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default();
    (token, false)
}

/// The scrub every request gets, authenticated or not (plugin_requests.go:187-213): the two
/// server-set headers, the session cookie, `Referer`, `access_token`, and the path's prefix.
pub fn scrub_plugin_request(request: &mut PluginHttpRequest, plugin_id: &str, subpath: &str) {
    let header = &mut request.header;
    header_del(header, "Mattermost-Plugin-ID");
    header_del(header, "Mattermost-User-Id");
    let cookies = read_cookies(header, "");
    header_del(header, "Cookie");
    for cookie in cookies.iter().filter(|c| c.name != SESSION_COOKIE_TOKEN) {
        add_cookie(header, cookie);
    }
    header_del(header, "Referer");

    let mut query = request.url.query();
    query.del("access_token");
    request.url.raw_query = query.encode();

    let prefix = go_path::join(&[subpath, "plugins", plugin_id]);
    let path = String::from_utf8_lossy(&request.url.path).into_owned();
    if let Some(rest) = path.strip_prefix(prefix.as_str()) {
        request.url.path = rest.as_bytes().to_vec();
    }
}

/// The body as the plugin will read it: still on the wire, or already read by the CSRF check.
pub enum Body {
    Stream(RequestBody),
    Read(Vec<u8>),
}

impl Body {
    fn into_reader(self) -> RequestBody {
        match self {
            Body::Stream(stream) => stream,
            Body::Read(bytes) => Box::new(std::io::Cursor::new(bytes)),
        }
    }
}

/// `mime.ParseMediaType`'s answer for the media type alone: the lower-cased, trimmed type before
/// the first `;`, or empty when that is not `type/subtype` (or a bare token) or when a parameter
/// is repeated with a different value. A parameter Go cannot parse still returns the type.
fn media_type(content_type: &str) -> String {
    let (base, mut rest) =
        content_type.split_at(content_type.find(';').unwrap_or(content_type.len()));
    let media = base.to_lowercase().trim().to_owned();
    let (typ, after) = media.split_at(
        media
            .bytes()
            .position(|b| !is_token_byte(b))
            .unwrap_or(media.len()),
    );
    let well_formed = !typ.is_empty()
        && (after.is_empty()
            || after
                .strip_prefix('/')
                .is_some_and(|sub| !sub.is_empty() && sub.bytes().all(is_token_byte)));
    if !well_formed {
        return String::new();
    }
    // `consumeMediaParam` for each parameter, to find a conflicting duplicate; a parse error
    // ends the scan and keeps the type, as Go's `ErrInvalidMediaParameter` does.
    let mut seen: HashMap<String, String> = HashMap::new();
    loop {
        rest = rest.trim_start();
        let Some(param) = rest.strip_prefix(';') else {
            break;
        };
        let param = param.trim_start();
        let end = param
            .bytes()
            .position(|b| !is_token_byte(b))
            .unwrap_or(param.len());
        let key = param[..end].to_lowercase();
        let Some(after_eq) = param[end..].trim_start().strip_prefix('=') else {
            break;
        };
        let after_eq = after_eq.trim_start();
        let (value, remainder) = if let Some(quoted) = after_eq.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut close = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        close = Some(i);
                        break;
                    }
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    '\r' | '\n' => break,
                    other => value.push(other),
                }
            }
            match close {
                Some(i) => (value, &quoted[i + 1..]),
                None => break,
            }
        } else {
            let end = after_eq
                .bytes()
                .position(|b| !is_token_byte(b))
                .unwrap_or(after_eq.len());
            if end == 0 {
                break;
            }
            (after_eq[..end].to_owned(), &after_eq[end..])
        };
        if key.is_empty() {
            break;
        }
        if seen.get(&key).is_some_and(|existing| *existing != value) {
            return String::new();
        }
        seen.insert(key, value);
        rest = remainder;
    }
    media
}

/// `r.FormValue("csrf")` after `ParseForm` (net/http/request.go): a url-encoded body of a `POST`,
/// `PUT` or `PATCH` (none past 10 MB) first, then the query.
pub fn form_csrf(method: &str, header: &Header, body: &[u8], raw_query: &str) -> String {
    let mut form = go_url::Values::new();
    if matches!(method, "POST" | "PUT" | "PATCH") {
        let content_type = match header_get(header, "Content-Type") {
            "" => "application/octet-stream",
            other => other,
        };
        if media_type(content_type) == "application/x-www-form-urlencoded"
            && body.len() <= MAX_FORM_SIZE
        {
            form = go_url::parse_query(&String::from_utf8_lossy(body)).0;
        }
    }
    let from_body = form.get("csrf").map(<[u8]>::to_vec);
    let value = from_body.or_else(|| {
        go_url::parse_query(raw_query)
            .0
            .get("csrf")
            .map(<[u8]>::to_vec)
    });
    value
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default()
}

/// Port of `validateCSRFForPluginRequest` (plugin_requests.go:273). The body is read only when
/// the header is missing; the bytes are handed back for the plugin.
pub async fn validate_csrf(
    request: &PluginHttpRequest,
    body: Body,
    session: &Session,
    cookie_auth: bool,
    strict: bool,
) -> (bool, Body) {
    if !cookie_auth || request.method == "GET" {
        return (true, body);
    }
    let mut body = body;
    let mut from_client = header_get(&request.header, "X-CSRF-Token").to_owned();
    if from_client.is_empty() {
        let bytes = match body {
            Body::Read(bytes) => bytes,
            Body::Stream(mut stream) => {
                let mut bytes = Vec::new();
                if let Err(err) = stream.read_to_end(&mut bytes).await {
                    tracing::warn!(error = %err, "Failed to read request body for plugin request");
                }
                bytes
            }
        };
        from_client = form_csrf(
            &request.method,
            &request.header,
            &bytes,
            &request.url.raw_query,
        );
        body = Body::Read(bytes);
    }
    if from_client == session.get_csrf() {
        return (true, body);
    }
    if header_get(&request.header, "X-Requested-With") == "XMLHttpRequest" {
        const MESSAGE: &str = "CSRF Check failed for request - Please migrate your plugin to either send a CSRF Header or Form Field, XMLHttpRequest is deprecated";
        if strict {
            tracing::warn!(session_id = %session.id, "{MESSAGE}");
            return (false, body);
        }
        tracing::debug!(session_id = %session.id, "{MESSAGE}");
        return (true, body);
    }
    (false, body)
}

// ---------------------------------------------------------------------------------------------
// net/http's response
// ---------------------------------------------------------------------------------------------

/// `bodyAllowedForStatus`.
fn body_allowed(status: u16) -> bool {
    !((100..=199).contains(&status) || status == 204 || status == 304)
}

/// The `http.ResponseWriter` `net/http` gives a handler, as far as a client can tell: the status
/// and header are fixed at the first `WriteHeader` or `Write` (a later status is superfluous),
/// and up to 2 KiB is held before the head goes out. If the handler returns with everything
/// still held, the head gains a `Content-Length`; otherwise the body is chunked. With no
/// `Content-Type` the first bytes are sniffed (`DetectContentType`). A flush sends the head at
/// once. Held bytes are unbounded here where Go's writer blocks, so only memory differs.
pub struct GoResponseWriter {
    is_head: bool,
    handler_header: Header,
    /// `w.wroteHeader`, with `w.status`.
    wrote: Option<u16>,
    /// `cw.header`: the header as it was at the status, until the head takes it.
    snapshot: Option<Header>,
    held: Vec<u8>,
    head: Option<oneshot::Sender<Head>>,
    rest: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

/// The status, the header, the bytes held, and whether the handler had returned.
type Head = (u16, Header, Vec<u8>, bool);

/// The receiving half of a [`GoResponseWriter`].
pub struct PendingAnswer {
    head: oneshot::Receiver<Head>,
    rest: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl PendingAnswer {
    /// Wait for the head. A writer dropped without one cannot happen (its `Drop` sends it); it
    /// would be an empty 200.
    pub async fn answer(self) -> PluginHttpAnswer {
        let (status, header, first, done) =
            self.head
                .await
                .unwrap_or((200, Header::default(), Vec::new(), true));
        PluginHttpAnswer {
            status,
            header,
            first,
            done,
            rest: self.rest,
        }
    }
}

impl GoResponseWriter {
    pub fn new(is_head: bool) -> (Self, PendingAnswer) {
        let (head_tx, head) = oneshot::channel();
        let (rest_tx, rest) = mpsc::unbounded_channel();
        (
            Self {
                is_head,
                handler_header: Header::default(),
                wrote: None,
                snapshot: None,
                held: Vec::new(),
                head: Some(head_tx),
                rest: Some(rest_tx),
            },
            PendingAnswer { head, rest },
        )
    }

    /// `chunkWriter.writeHeader(p)`: the head, from the header as it was at the status and the
    /// bytes held. `done` is `w.handlerDone`.
    fn commit(&mut self, done: bool) {
        let Some(head) = self.head.take() else {
            return;
        };
        let status = self.wrote.unwrap_or(200);
        let mut header = self.snapshot.take().unwrap_or_default();
        let p = std::mem::take(&mut self.held);
        // `Trailer:` keys are the fake trailers; neither they nor `Trailer` itself are sent.
        let trailers =
            header.contains_key("Trailer") || header.keys().any(|k| k.starts_with("Trailer:"));
        header.retain(|k, _| !k.starts_with("Trailer:"));
        header.remove("Trailer");
        let has_te = !header_get(&header, "Transfer-Encoding").is_empty();
        if done
            && !trailers
            && !has_te
            && body_allowed(status)
            && !header.contains_key("Content-Length")
            && (!self.is_head || !p.is_empty())
        {
            header_set(&mut header, "Content-Length", &p.len().to_string());
        }
        if body_allowed(status) {
            let has_ce = !header_get(&header, "Content-Encoding").is_empty();
            if !has_ce && !header.contains_key("Content-Type") && !has_te && !p.is_empty() {
                header_set(
                    &mut header,
                    "Content-Type",
                    crate::link_image::detect_content_type(&p),
                );
            }
        } else {
            // `suppressedHeaders`.
            if status == 304 {
                header.remove("Content-Type");
            }
            header.remove("Content-Length");
            header.remove("Transfer-Encoding");
        }
        let _ = head.send((status, header, p, done));
    }

    /// `response.WriteHeader` without a head yet: the implicit 200 of a first write.
    fn ensure_status(&mut self) {
        if self.wrote.is_none() {
            self.write_header(200);
        }
    }
}

impl ResponseWriter for GoResponseWriter {
    fn header(&mut self) -> Header {
        self.handler_header.clone()
    }

    fn sync_header(&mut self, header: Header) {
        self.handler_header = header;
    }

    fn write(&mut self, body: &[u8]) -> std::io::Result<()> {
        self.ensure_status();
        if !body_allowed(self.wrote.unwrap_or(200)) {
            return Err(std::io::Error::other(
                "http: request method or response status code does not allow body",
            ));
        }
        if self.head.is_none() {
            return match &self.rest {
                Some(rest) if rest.send(body.to_vec()).is_ok() => Ok(()),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "the client has gone",
                )),
            };
        }
        self.held.extend_from_slice(body);
        // `bufio.Writer.Write`: past the buffer's room, the head goes out with what is held.
        if self.held.len() > BUFFER_BEFORE_CHUNKING {
            self.commit(false);
        }
        Ok(())
    }

    /// Only the first status counts. An informational status other than 101 is written ahead of
    /// the response by Go and does not fix the status; it is not sent here, and a client never
    /// sees one either way.
    fn write_header(&mut self, status: i64) {
        let Ok(status) = u16::try_from(status) else {
            return;
        };
        if (100..=199).contains(&status) && status != 101 {
            return;
        }
        if self.wrote.is_some() {
            tracing::debug!(status, "http: superfluous response.WriteHeader call");
            return;
        }
        self.wrote = Some(status);
        // The copy Go's `Header()` takes between the status and the head: every writer this
        // serves has read the header by now (the RPC client syncs it before each call).
        self.snapshot = Some(self.handler_header.clone());
    }

    fn flush(&mut self) {
        self.ensure_status();
        self.commit(false);
    }
}

/// `finishRequest`: the implicit 200, then the head if nothing has sent it, then the body's end.
impl Drop for GoResponseWriter {
    fn drop(&mut self) {
        self.ensure_status();
        self.commit(true);
        self.rest.take();
    }
}

/// `doPluginRequest`'s URL (app/integration_action.go:225-262): the plugin id from a relative
/// `plugins/<id>/…` (one leading slash forgiven), the rest of the path, and `values` with the URL's
/// own query added after them — as `http.NewRequest` parses the result.
pub fn plugin_request_target(
    raw_url: &str,
    values: go_url::Values,
) -> Result<(String, GoUrl), Box<mm_model::utils::AppError>> {
    let refused = |detail: &str| {
        mm_model::utils::AppError::boxed(
            "doPluginRequest",
            "api.post.do_action.action_integration.app_error",
            None,
            detail,
            400,
        )
    };
    let raw_url = raw_url.strip_prefix('/').unwrap_or(raw_url);
    let in_url = go_url::go_parse(raw_url).map_err(|err| refused(&err.to_string()))?;
    let in_path = String::from_utf8_lossy(&in_url.path).into_owned();
    let cleaned = go_path::clean(&in_path);
    let result: Vec<&str> = cleaned.split('/').collect();
    if result.len() < 2 {
        return Err(refused("err=Unable to find pluginId"));
    }
    if result[0] != "plugins" {
        return Err(refused("err=plugins not in path"));
    }
    let prefix = format!("plugins/{}", result[1]);
    let plugin_id = result[1].to_owned();
    let rest = in_path.strip_prefix(prefix.as_str()).unwrap_or(&in_path);
    let mut base = go_url::go_parse(rest).map_err(|err| refused(&err.to_string()))?;
    let mut values = values;
    // Go ranges over a map, but `Encode` sorts by key, and within a key the URL's values
    // follow the caller's either way.
    for (key, list) in in_url.query().iter() {
        for value in list {
            values.add(key, value);
        }
    }
    base.raw_query = values.encode();
    // `http.NewRequest` parses the string it is given.
    let url = go_url::go_parse(&base.to_go_string()).map_err(|err| refused(&err.to_string()))?;

    Ok((plugin_id, url))
}

/// Port of `LocalResponseWriter` (app/integration_action.go:197): a header, the last status and
/// the **last** write — each `Write` replaces the data — handed over when the plugin is done. It
/// is neither a flusher nor a hijacker.
pub struct LocalResponseWriter {
    header: Header,
    status: i64,
    data: Vec<u8>,
    done: Option<oneshot::Sender<LocalAnswer>>,
}

/// The status, the header and the last write.
pub type LocalAnswer = (i64, Header, Vec<u8>);

impl LocalResponseWriter {
    pub fn new() -> (Self, oneshot::Receiver<LocalAnswer>) {
        let (done, answer) = oneshot::channel();
        (
            Self {
                header: Header::default(),
                status: 0,
                data: Vec::new(),
                done: Some(done),
            },
            answer,
        )
    }
}

impl ResponseWriter for LocalResponseWriter {
    fn header(&mut self) -> Header {
        self.header.clone()
    }

    fn sync_header(&mut self, header: Header) {
        self.header = header;
    }

    fn write(&mut self, body: &[u8]) -> std::io::Result<()> {
        body.clone_into(&mut self.data);
        Ok(())
    }

    fn write_header(&mut self, status: i64) {
        self.status = status;
    }
}

impl Drop for LocalResponseWriter {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send((
                self.status,
                std::mem::take(&mut self.header),
                std::mem::take(&mut self.data),
            ));
        }
    }
}

/// `http.Error` into a writer.
fn http_error<W: ResponseWriter>(writer: &mut W, text: &str, code: i64) {
    let mut header = writer.header();
    header.remove("Content-Length");
    header_set(&mut header, "Content-Type", "text/plain; charset=utf-8");
    header_set(&mut header, "X-Content-Type-Options", "nosniff");
    writer.sync_header(header);
    writer.write_header(code);
    let _ = writer.write(format!("{text}\n").as_bytes());
}

// ---------------------------------------------------------------------------------------------
// ServePluginRequest
// ---------------------------------------------------------------------------------------------

impl App {
    /// Port of `Channels.ServePluginRequest` (plugin_requests.go:23) and `servePluginRequest`;
    /// see the module docs. `subpath` is `utils.GetSubpathFromConfig`'s.
    #[tracing::instrument(skip_all, fields(plugin_id, method = %request.method))]
    pub async fn serve_plugin_request(
        &self,
        plugin_id: &str,
        request: PluginHttpRequest,
        body: RequestBody,
        subpath: &str,
    ) -> PluginHttpAnswer {
        let (writer, pending) = GoResponseWriter::new(request.method == "HEAD");
        self.serve_plugin_request_into(plugin_id, request, body, subpath, writer)
            .await;
        pending.answer().await
    }

    /// `ServePluginRequest(w, r)` into any writer: `net/http`'s for a client, a
    /// [`LocalResponseWriter`] for the server's own request. The writer is dropped when the
    /// answer is complete — here for Go's own answers, by the plugin's task otherwise.
    async fn serve_plugin_request_into<W: ResponseWriter>(
        &self,
        plugin_id: &str,
        mut request: PluginHttpRequest,
        body: RequestBody,
        subpath: &str,
        mut writer: W,
    ) {
        let Some(environment) = self.plugins_environment() else {
            // Go writes the status before it sets the type, so the type is never sent and the
            // JSON is sniffed as text.
            let body = self.plugins_disabled_body();
            writer.write_header(501);
            let _ = writer.write(body.as_bytes());
            return;
        };
        let hooks = match environment.hooks_for_plugin(plugin_id) {
            Ok(hooks) => hooks,
            Err(err) => {
                tracing::debug!(missing_plugin_id = plugin_id, error = %err, "Access to route for non-existent plugin");
                http_error(&mut writer, "404 page not found", 404);
                return;
            }
        };

        let (token, cookie_auth) = plugin_request_token(&request.header, &request.url);
        scrub_plugin_request(&mut request, plugin_id, subpath);
        let mut body = Body::Stream(body);

        if !token.is_empty() {
            match self.get_session(&token).await {
                Err(err) if err.status_code == 500 => {
                    tracing::error!(
                        error = %err.to_string().replace(&token, "<redacted>"),
                        "Internal server error while loading session"
                    );
                    http_error(&mut writer, "Internal Server Error", 500);
                    return;
                }
                Err(err) => {
                    tracing::debug!(
                        error = %err.to_string().replace(&token, "<redacted>"),
                        "Token in plugin request is invalid. Treating request as unauthenticated"
                    );
                }
                Ok(session) => {
                    header_del(&mut request.header, "Authorization");
                    match self.mfa_required(Some(&session), false).await {
                        Err(err) if err.status_code == 500 => {
                            tracing::error!(error = %err, "Internal server error during MFA validation");
                            http_error(&mut writer, "Internal Server Error", 500);
                            return;
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "Treating session as unauthenticated since MFA required");
                        }
                        Ok(()) => {
                            let strict = self.config().experimental_strict_csrf_enforcement;
                            let (passed, read) =
                                validate_csrf(&request, body, &session, cookie_auth, strict).await;
                            body = read;
                            if passed {
                                header_set(
                                    &mut request.header,
                                    "Mattermost-User-Id",
                                    &session.user_id,
                                );
                                request.context.session_id = session.id.clone();
                            } else {
                                tracing::debug!(
                                    "CSRF request failed. Treating the request as unauthenticated."
                                );
                            }
                        }
                    }
                }
            }
        }

        let context = request.context.to_wire();
        let subset = HTTPRequestSubset {
            method: request.method,
            url: Some(gobwire::BinaryBytes(
                request.url.to_go_string().into_bytes(),
            )),
            proto: request.proto,
            proto_major: request.proto_major,
            proto_minor: request.proto_minor,
            header: request.header,
            host: request.host,
            remote_addr: request.remote_addr,
            request_uri: request.request_uri,
            body: None,
        };
        let reader = body.into_reader();
        tokio::spawn(async move {
            hooks
                .serve_http(
                    Some(Box::new(context)),
                    Some(Box::new(subset)),
                    Some(reader),
                    writer,
                )
                .await;
        });
    }

    /// Port of `Channels.doPluginRequest` (app/integration_action.go:224): the server's own
    /// request to a plugin, on behalf of `session` — a relative `plugins/<id>/…` URL, the given
    /// values merged with its query, the user id and the session's token as a bearer header, and
    /// `ServePluginRequest` into a [`LocalResponseWriter`]. The answer is the status (200 when
    /// none was written), the header, and the **last** write, which is all Go's writer keeps.
    pub async fn do_plugin_request(
        &self,
        session: &Session,
        method: &str,
        raw_url: &str,
        values: go_url::Values,
        body: Vec<u8>,
    ) -> Result<(i64, Header, Vec<u8>), Box<mm_model::utils::AppError>> {
        let (plugin_id, url) = plugin_request_target(raw_url, values)?;

        let mut header = Header::default();
        header_set(&mut header, "Mattermost-User-Id", &session.user_id);
        header_set(
            &mut header,
            "Authorization",
            &format!("Bearer {}", session.token),
        );
        let request = PluginHttpRequest {
            method: method.to_owned(),
            host: String::from_utf8_lossy(&url.host).into_owned(),
            url,
            proto: "HTTP/1.1".to_owned(),
            proto_major: 1,
            proto_minor: 1,
            header,
            remote_addr: String::new(),
            request_uri: String::new(),
            // `GetIPAddress` of an empty `RemoteAddr`, and no browser headers.
            context: HookContext {
                request_id: mm_model::utils::new_id(),
                ..HookContext::default()
            },
        };
        let (writer, answer) = LocalResponseWriter::new();
        let subpath = self.config().subpath();
        self.serve_plugin_request_into(
            &plugin_id,
            request,
            Box::new(std::io::Cursor::new(body)),
            &subpath,
            writer,
        )
        .await;
        let (status, header, data) = answer.await.unwrap_or_default();
        Ok((if status == 0 { 200 } else { status }, header, data))
    }

    /// `app.plugin.disabled.app_error` as `appErr.ToJSON()` writes it.
    fn plugins_disabled_body(&self) -> String {
        let mut err = mm_model::utils::AppError::new(
            "ServePluginRequest",
            "app.plugin.disabled.app_error",
            None,
            "Enable plugins to serve plugin requests",
            501,
        );
        if let Some(bundle) = crate::i18n::loaded() {
            bundle.translate_app_error(
                bundle.server_locale(&self.config().default_server_locale),
                &mut err,
            );
        }
        serde_json::to_string(&err).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(lines: &[&str]) -> Header {
        let mut header = Header::default();
        for line in lines {
            let (name, value) = line.split_once(':').expect("a header line");
            header
                .entry(canonical_header_key(&name.to_ascii_lowercase()))
                .or_default()
                .push(trim_string(value).to_owned());
        }
        header
    }

    fn url(target: &str) -> GoUrl {
        go_url::parse_request_uri(target).expect("a target")
    }

    #[test]
    fn canonical_keys_are_gos() {
        assert_eq!(canonical_header_key("x-csrf-token"), "X-Csrf-Token");
        assert_eq!(
            canonical_header_key("mattermost-plugin-id"),
            "Mattermost-Plugin-Id"
        );
        assert_eq!(canonical_header_key("WWW-AUTHENTICATE"), "Www-Authenticate");
        assert_eq!(
            canonical_header_key("bad key"),
            "bad key",
            "not a token: left alone"
        );
    }

    /// Each branch of the order, and what each one leaves `cookie_auth` as.
    #[test]
    fn the_token_comes_from_the_header_then_the_cookie_then_the_query() {
        let q = url("/plugins/p/x?access_token=fromquery");
        let token = |lines: &[&str]| plugin_request_token(&header(lines), &q);
        assert_eq!(
            token(&["Authorization: Bearer b", "Cookie: MMAUTHTOKEN=c"]),
            ("b".into(), false)
        );
        assert_eq!(token(&["Authorization: token t"]), ("t".into(), false));
        assert_eq!(
            token(&["Authorization: Bearer ", "Cookie: MMAUTHTOKEN=c"]),
            ("c".into(), true),
            "net/http trims the value, so a bare `Bearer` has no space to match"
        );
        assert_eq!(
            token(&["Authorization: Basic x", "Cookie: MMAUTHTOKEN=c"]),
            ("c".into(), true)
        );
        assert_eq!(token(&["Cookie: MMAUTHTOKEN="]), (String::new(), true));
        assert_eq!(token(&["Cookie: other=1"]), ("fromquery".into(), false));
        assert_eq!(
            plugin_request_token(&Header::default(), &url("/x")),
            (String::new(), false)
        );
    }

    #[test]
    fn the_scrub_rebuilds_the_cookie_line_and_trims_the_path() {
        let mut request = PluginHttpRequest {
            method: "POST".into(),
            url: url("/sub/plugins/p/a/b?access_token=t&z=1&a=2"),
            header: header(&[
                "Cookie: a=1; MMAUTHTOKEN=s; b=x y",
                "Mattermost-User-Id: spoof",
                "Mattermost-Plugin-ID: spoof",
                "Referer: r",
                "X-Kept: k",
            ]),
            ..PluginHttpRequest::default()
        };
        scrub_plugin_request(&mut request, "p", "/sub");
        assert_eq!(request.url.to_go_string(), "/a/b?a=2&z=1");
        assert_eq!(
            request.header.get("Cookie"),
            Some(&vec!["a=1; b=\"x y\"".to_owned()])
        );
        for gone in ["Mattermost-User-Id", "Mattermost-Plugin-Id", "Referer"] {
            assert!(!request.header.contains_key(gone), "{gone}");
        }
        assert!(request.header.contains_key("X-Kept"));

        // Only the session cookie: no line at all.
        let mut request = PluginHttpRequest {
            url: url("/plugins/other/x"),
            header: header(&["Cookie: MMAUTHTOKEN=s"]),
            ..PluginHttpRequest::default()
        };
        scrub_plugin_request(&mut request, "p", "/");
        assert!(!request.header.contains_key("Cookie"));
        assert_eq!(
            request.url.to_go_string(),
            "/plugins/other/x",
            "another prefix stays"
        );
    }

    fn session(csrf: &str) -> Session {
        let mut props = mm_model::utils::StringMap::new();
        if !csrf.is_empty() {
            props.insert("csrf".into(), csrf.into());
        }
        Session {
            props: Some(props),
            ..Session::default()
        }
    }

    async fn csrf(
        method: &str,
        target: &str,
        lines: &[&str],
        body: &str,
        strict: bool,
    ) -> (bool, Vec<u8>) {
        let request = PluginHttpRequest {
            method: method.into(),
            url: url(target),
            header: header(lines),
            ..PluginHttpRequest::default()
        };
        let stream: RequestBody = Box::new(std::io::Cursor::new(body.as_bytes().to_vec()));
        let (passed, body) = validate_csrf(
            &request,
            Body::Stream(stream),
            &session("right"),
            true,
            strict,
        )
        .await;
        let mut read = Vec::new();
        let _ = body.into_reader().read_to_end(&mut read).await;
        (passed, read)
    }

    #[tokio::test]
    async fn the_csrf_check_takes_the_header_then_the_form_then_the_legacy_header() {
        const FORM: &str = "Content-Type: application/x-www-form-urlencoded";
        assert!(
            csrf("POST", "/x", &["X-CSRF-Token: right"], "", false)
                .await
                .0
        );
        assert!(
            !csrf(
                "POST",
                "/x",
                &["X-CSRF-Token: wrong", FORM],
                "csrf=right",
                false
            )
            .await
            .0
        );
        let (passed, handed) = csrf("POST", "/x", &[FORM], "csrf=right", false).await;
        assert!(passed);
        assert_eq!(
            handed, b"csrf=right",
            "the plugin still reads what was read"
        );
        assert!(csrf("POST", "/x?csrf=right", &[], "", false).await.0);
        assert!(
            !csrf("POST", "/x?csrf=right", &[FORM], "csrf=wrong", false)
                .await
                .0
        );
        assert!(
            !csrf("HEAD", "/x", &[], "", false).await.0,
            "only GET is exempt"
        );
        let legacy = ["X-Requested-With: XMLHttpRequest"];
        assert!(csrf("POST", "/x", &legacy, "", false).await.0);
        assert!(!csrf("POST", "/x", &legacy, "", true).await.0);

        // Not a cookie, or a GET: nothing is read at all.
        let request = PluginHttpRequest {
            method: "GET".into(),
            ..PluginHttpRequest::default()
        };
        let stream: RequestBody = Box::new(std::io::Cursor::new(b"x".to_vec()));
        let (passed, body) =
            validate_csrf(&request, Body::Stream(stream), &session("r"), true, true).await;
        assert!(passed);
        assert!(matches!(body, Body::Stream(_)));
    }

    #[test]
    fn the_media_type_is_gos() {
        assert_eq!(
            media_type(" Application/JSON ; charset=x"),
            "application/json"
        );
        assert_eq!(media_type("a/b; x=1; X=\"1\""), "a/b");
        assert_eq!(media_type("a/b; x=1; x=2"), "");
        assert_eq!(media_type("a/b/c"), "");
        assert_eq!(media_type("token"), "token");
        assert_eq!(media_type("a/b; =x"), "a/b");
    }

    async fn drain(answer: PluginHttpAnswer) -> (u16, Header, Vec<u8>, bool) {
        let mut body = answer.first;
        let mut rest = answer.rest;
        while let Some(chunk) = rest.recv().await {
            body.extend(chunk);
        }
        (answer.status, answer.header, body, answer.done)
    }

    #[tokio::test]
    async fn a_short_body_is_sized_and_sniffed_and_a_long_one_streams() {
        let (mut w, pending) = GoResponseWriter::new(false);
        w.write(b"<html>").expect("written");
        drop(w);
        let (status, header, body, done) = drain(pending.answer().await).await;
        assert_eq!((status, done, body.as_slice()), (200, true, &b"<html>"[..]));
        assert_eq!(header_get(&header, "Content-Length"), "6");
        assert_eq!(
            header_get(&header, "Content-Type"),
            "text/html; charset=utf-8"
        );

        let (mut w, pending) = GoResponseWriter::new(false);
        w.write(&[b'a'; 2049]).expect("written");
        let answer = pending.answer().await;
        assert!(!answer.done, "the head is out before the handler returns");
        drop(w);
        let (_, header, body, _) = drain(answer).await;
        assert_eq!(body.len(), 2049);
        assert!(!header.contains_key("Content-Length"));
    }

    #[test]
    fn the_servers_own_request_names_the_plugin_and_merges_the_query() {
        let mut values = go_url::Values::new();
        values.add(b"b", b"from-values");
        values.add(b"a", b"x y");
        let (id, url) =
            plugin_request_target("/plugins/com.p/suggest/fetch?b=from-url&c=1", values)
                .expect("a target");
        assert_eq!(id, "com.p");
        assert_eq!(
            url.to_go_string(),
            "/suggest/fetch?a=x+y&b=from-values&b=from-url&c=1"
        );
        let (id, url) = plugin_request_target("plugins/p", go_url::Values::new()).expect("bare");
        assert_eq!((id.as_str(), url.to_go_string().as_str()), ("p", ""));

        let refused = |raw: &str| {
            plugin_request_target(raw, go_url::Values::new())
                .err()
                .map(|e| e.detailed_error)
        };
        assert_eq!(
            refused("plugins").as_deref(),
            Some("err=Unable to find pluginId")
        );
        assert_eq!(
            refused("other/p/x").as_deref(),
            Some("err=plugins not in path")
        );
        assert_eq!(
            refused("//plugins/p/x").as_deref(),
            Some("err=plugins not in path"),
            "only one slash is forgiven"
        );
        assert_eq!(
            refused("http://host/plugins/p/x").as_deref(),
            Some("err=plugins not in path")
        );
    }

    #[tokio::test]
    async fn the_local_writer_keeps_the_last_write_and_the_last_status() {
        let (mut w, answer) = LocalResponseWriter::new();
        w.write_header(201);
        w.write_header(202);
        w.write(b"first").expect("written");
        w.write(b"second").expect("written");
        w.flush();
        drop(w);
        let (status, _, data) = answer.await.expect("handed over");
        assert_eq!((status, data.as_slice()), (202, &b"second"[..]));
    }

    #[test]
    fn a_write_to_a_no_body_status_is_refused() {
        let (mut w, _pending) = GoResponseWriter::new(false);
        w.write_header(204);
        assert!(w.write(b"x").is_err());
    }
}

/// `fixtures/behaviour_plugin_requests.json`, from `reference/dump/behaviour_plugin_requests.go`:
/// the token order and the scrub, the CSRF check, and `net/http`'s framing of a handler's writes.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value as Json;

    fn oracle() -> Json {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_plugin_requests.json"
        ))
        .expect("behaviour_plugin_requests.json is generated by reference/dump")
    }

    fn lines(case: &Json, key: &str) -> Vec<String> {
        case[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_owned))
            .collect()
    }

    /// `http.ReadRequest`'s header: canonical keys, values trimmed, `Host` taken out.
    fn read_header(lines: &[String]) -> Header {
        let mut header = Header::default();
        for line in lines {
            let (name, value) = line.split_once(':').expect("a header line");
            header
                .entry(canonical_header_key(name))
                .or_default()
                .push(trim_string(value).to_owned());
        }
        header
    }

    fn as_header(value: &Json) -> Header {
        serde_json::from_value(value.clone()).expect("a header map")
    }

    #[test]
    fn the_token_and_the_scrub_match_go() {
        let oracle = oracle();
        let cases = oracle["requests"].as_array().expect("requests");
        assert!(cases.len() >= 20);
        for expected in cases {
            let case = &expected["case"];
            let name = case["name"].as_str().unwrap_or_default();
            let target = case["target"].as_str().expect("a target");
            let mut request = PluginHttpRequest {
                method: "POST".into(),
                url: go_url::parse_request_uri(target).expect("a target"),
                header: read_header(&lines(case, "headers")),
                ..PluginHttpRequest::default()
            };
            let (token, cookie_auth) = plugin_request_token(&request.header, &request.url);
            assert_eq!(
                token,
                expected["token"].as_str().unwrap_or_default(),
                "{name}"
            );
            assert_eq!(cookie_auth, expected["cookie_auth"], "{name}");
            scrub_plugin_request(&mut request, "p", case["subpath"].as_str().unwrap_or("/"));
            assert_eq!(request.url.to_go_string(), expected["url"], "{name}");
            assert_eq!(request.header, as_header(&expected["header"]), "{name}");
        }
    }

    #[tokio::test]
    async fn the_csrf_check_matches_go() {
        let oracle = oracle();
        let cases = oracle["csrf"].as_array().expect("csrf");
        assert!(cases.len() >= 25);
        for expected in cases {
            let case = &expected["case"];
            let name = case["name"].as_str().unwrap_or_default();
            let mut body = case["body"]
                .as_str()
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            let pad = case["body_pad"].as_u64().unwrap_or(0) as usize;
            body.extend(std::iter::repeat_n(b'a', pad));
            let mut props = mm_model::utils::StringMap::new();
            let csrf = case["session_csrf"].as_str().unwrap_or_default();
            if !csrf.is_empty() {
                props.insert("csrf".into(), csrf.into());
            }
            let session = Session {
                props: Some(props),
                ..Session::default()
            };
            let request = PluginHttpRequest {
                method: case["method"].as_str().expect("a method").into(),
                url: go_url::parse_request_uri(case["target"].as_str().expect("a target"))
                    .expect("a target"),
                header: read_header(&lines(case, "headers")),
                ..PluginHttpRequest::default()
            };
            let stream: RequestBody = Box::new(std::io::Cursor::new(body));
            let (passed, handed) = validate_csrf(
                &request,
                Body::Stream(stream),
                &session,
                case["cookie_auth"].as_bool().unwrap_or(false),
                case["strict"].as_bool().unwrap_or(false),
            )
            .await;
            assert_eq!(passed, expected["passed"], "{name}");
            let mut read = Vec::new();
            let _ = handed.into_reader().read_to_end(&mut read).await;
            assert_eq!(read.len() as u64, expected["body_len"], "{name}");
            let prefix = String::from_utf8_lossy(&read[..read.len().min(64)]).into_owned();
            assert_eq!(prefix, expected["body_prefix"], "{name}");
        }
    }

    #[tokio::test]
    async fn the_framing_matches_net_http() {
        let oracle = oracle();
        let cases = oracle["framing"].as_array().expect("framing");
        assert!(cases.len() >= 20);
        for expected in cases {
            let case = &expected["case"];
            let name = case["name"].as_str().unwrap_or_default();
            let is_head = case["method"] == "HEAD";
            let (mut w, pending) = GoResponseWriter::new(is_head);
            let mut errors: Vec<String> = Vec::new();
            for op in case["ops"].as_array().into_iter().flatten() {
                match op["op"].as_str() {
                    Some("set") => {
                        let mut header = w.header();
                        header_set(
                            &mut header,
                            op["key"].as_str().unwrap_or_default(),
                            op["value"].as_str().unwrap_or_default(),
                        );
                        w.sync_header(header);
                    }
                    Some("status") => w.write_header(op["code"].as_i64().unwrap_or(0)),
                    Some("write") => {
                        let repeat = op["repeat"].as_u64().unwrap_or(0) as usize;
                        let text = if repeat > 0 {
                            vec![b'a'; repeat]
                        } else {
                            op["text"].as_str().unwrap_or_default().as_bytes().to_vec()
                        };
                        if let Err(err) = w.write(&text) {
                            errors.push(err.to_string());
                        }
                    }
                    Some("flush") => w.flush(),
                    other => panic!("{name}: unknown op {other:?}"),
                }
            }
            drop(w);
            let answer = pending.answer().await;
            let done = answer.done;
            let status = answer.status;
            let header = answer.header.clone();
            let mut body = answer.first;
            let mut rest = answer.rest;
            while let Some(chunk) = rest.recv().await {
                body.extend(chunk);
            }
            assert_eq!(i64::from(status), expected["status"], "{name}");
            assert_eq!(header, as_header(&expected["header"]), "{name}: the header");
            // hyper chunks a streamed body exactly when no length was declared.
            let chunked =
                !done && body_allowed(status) && !header.contains_key("Content-Length") && !is_head;
            assert_eq!(chunked, expected["chunked"], "{name}: chunked");
            let sent = if is_head { 0 } else { body.len() };
            assert_eq!(sent as u64, expected["body_len"], "{name}: the body");
            let expected_errors: Vec<String> = expected["op_errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect();
            assert_eq!(errors, expected_errors, "{name}: the write errors");
        }
    }
}
