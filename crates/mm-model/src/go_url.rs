//! Port of Go's `net/url` — not a Mattermost source file, the same category as
//! [`crate::utils::go_time`] and [`crate::utils::go_json_marshal`].
//!
//! Two call sites need it and they need different halves. `model.IsValidHTTPURL` (utils.go:790)
//! needs `ParseRequestURI` to succeed with a non-empty scheme and host — a **predicate**, which
//! is all [D-003] built. `model.MergeQueryIntoURL` (mm_blocks_actions.go:148) needs `Parse`, then
//! `URL.Query()`, `Values.Set`, `Values.Encode()` and `URL.String()` — it takes a URL apart, edits
//! it and puts it back together, so a predicate is not enough.
//!
//! **The `url` crate is not substitutable.** It implements the WHATWG URL Standard, which
//! normalises (lowercasing hosts, resolving dot segments, adding a default port) and disagrees
//! with RFC 3986 in both directions. Everything here reproduces Go's grammar directly.
//!
//! # Why the byte-level types
//!
//! Go strings are byte strings. `unescape("%80", encodePath)` yields one byte, `0x80`, which no
//! Rust `String` can hold — and `https://example.com/%80` is an ordinary URL that Go parses
//! without complaint. So [`GoUrl`]'s path, host, fragment and userinfo are `Vec<u8>`. Only the
//! parts that are slices of the input (`scheme`, `opaque`, `raw_query`) are `String`.
//!
//! [`GoUrl::to_go_string`] is nonetheless total: every component it does not copy verbatim from
//! the input is re-escaped, and `escape` emits only ASCII.
//!
//! # What is not ported
//!
//! `ResolveReference`/`resolvePath`, `JoinPath`, `RequestURI`, `Redacted`, `MarshalBinary` and
//! `Hostname`/`Port`. None has a call site in the ported tree; each is self-contained enough to
//! add when one appears.
//!
//! # What is not asserted
//!
//! **The error *messages*.** [`UrlParseError`] is typed and renders approximately like Go's, but
//! `netip.ParseAddr`'s wording in particular is not reproduced. Nothing in the ported tree reads
//! an error string here — `IsValidHTTPURL` discards it and `GetAction` turns it into a `None` —
//! so the oracle records Go's text as a diagnostic and the tests assert *whether* a parse failed,
//! not what it said. See [D-049].

use std::collections::BTreeMap;
use std::net::Ipv6Addr;

use crate::utils::go_quote;

// --- encoding modes -----------------------------------------------------------------------

/// Go's unexported `encoding` (net/url/encoding_table.go:9). Which characters must be
/// percent-escaped depends on where in the URL they sit, and the seven positions disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Path,
    PathSegment,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

/// Port of `shouldEscape` (net/url/gen_encoding_table.go:152). The shipped `net/url` reads a
/// generated lookup table; this is the reference implementation that generates it, which is the
/// readable form of the same answer.
fn should_escape(c: u8, mode: Encoding) -> bool {
    // §2.3 unreserved characters (alphanum)
    if c.is_ascii_alphanumeric() {
        return false;
    }

    if matches!(mode, Encoding::Host | Encoding::Zone) {
        // §3.2.2 sub-delims, plus `:` because we carry `:port` in Host, plus `[` `]` because we
        // carry `[ipv6]:port`, plus `<` `>` `"` because they are the only ones left that could
        // be allowed — `parse` rejects them escaped, since a host may not `%`-encode ASCII.
        if matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        ) {
            return false;
        }
    }

    match c {
        // §2.3 unreserved characters (mark)
        b'-' | b'_' | b'.' | b'~' => return false,

        // §2.2 reserved characters. Different sections allow different subsets unescaped.
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            // §3.3 — this package only manipulates the path as a whole, so `/`, `;` and `,`
            // are allowed through as well. Only `?` is escaped.
            Encoding::Path => return c == b'?',
            Encoding::PathSegment => {
                return c == b'/' || c == b';' || c == b',' || c == b'?';
            }
            // §3.2.1 — `:` is escaped too because parsing treats it as the password separator.
            Encoding::UserPassword => {
                return c == b'@' || c == b'/' || c == b'?' || c == b':';
            }
            // §3.4 — the RFC reserves everything, so escape everything.
            Encoding::QueryComponent => return true,
            // §4.1 — the grammar allows everything, so escape nothing.
            Encoding::Fragment => return false,
            Encoding::Host | Encoding::Zone => {}
        },
        _ => {}
    }

    if mode == Encoding::Fragment {
        // A subset of sub-delims need not be escaped in a fragment. Single quote stays escaped
        // deliberately — Go issue #19917.
        if matches!(c, b'!' | b'(' | b')' | b'*') {
            return false;
        }
    }

    true
}

const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";

fn unhex(c: u8) -> u8 {
    9 * (c >> 6) + (c & 15)
}

/// Port of `escape` (net/url/url.go:195).
fn escape(s: &[u8], mode: Encoding) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if should_escape(c, mode) {
            if c == b' ' && mode == Encoding::QueryComponent {
                out.push(b'+');
            } else {
                out.push(b'%');
                out.push(UPPERHEX[(c >> 4) as usize]);
                out.push(UPPERHEX[(c & 15) as usize]);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Port of `unescape` (net/url/url.go:105).
///
/// Two rules beyond "every `%` needs two hex digits", both easy to miss and both load-bearing:
///
/// - in a **host**, a `%` escape may only encode a byte >= 0x80, the single exception being
///   `%25` — so `%80` is legal and `%41` is not, the reverse of the usual intuition;
/// - in a **host or zone**, an unescaped ASCII byte outside the host class is an error rather
///   than something to be escaped later.
fn unescape(s: &[u8], mode: Encoding) -> Result<Vec<u8>, UrlParseError> {
    // Go's first pass validates and counts; the second rewrites. Kept as two passes because the
    // validation order is observable — an early bad escape wins over a later bad host byte.
    let mut n = 0usize;
    let mut has_plus = false;
    let mut i = 0usize;
    while i < s.len() {
        match s[i] {
            b'%' => {
                n += 1;
                if i + 2 >= s.len()
                    || !s[i + 1].is_ascii_hexdigit()
                    || !s[i + 2].is_ascii_hexdigit()
                {
                    let tail = &s[i..];
                    let tail = &tail[..tail.len().min(3)];
                    return Err(UrlParseError::Escape(lossy(tail)));
                }
                if mode == Encoding::Host && unhex(s[i + 1]) < 8 && &s[i..i + 3] != b"%25" {
                    return Err(UrlParseError::Escape(lossy(&s[i..i + 3])));
                }
                if mode == Encoding::Zone {
                    // RFC 6874 says anything goes in a zone id, but Go restricts the escaped
                    // bytes to ones that could have been written directly — plus a space,
                    // because Windows puts them there.
                    let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                    if &s[i..i + 3] != b"%25" && v != b' ' && should_escape(v, Encoding::Host) {
                        return Err(UrlParseError::Escape(lossy(&s[i..i + 3])));
                    }
                }
                i += 3;
            }
            b'+' => {
                has_plus = mode == Encoding::QueryComponent;
                i += 1;
            }
            c => {
                if matches!(mode, Encoding::Host | Encoding::Zone)
                    && c < 0x80
                    && should_escape(c, mode)
                {
                    return Err(UrlParseError::InvalidHostCharacter(lossy(&s[i..i + 1])));
                }
                i += 1;
            }
        }
    }

    if n == 0 && !has_plus {
        return Ok(s.to_vec());
    }

    let unescaped_plus = if mode == Encoding::QueryComponent {
        b' '
    } else {
        b'+'
    };
    let mut out = Vec::with_capacity(s.len() - 2 * n);
    let mut i = 0usize;
    while i < s.len() {
        match s[i] {
            b'%' => {
                out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
                i += 3;
            }
            b'+' => {
                out.push(unescaped_plus);
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Renders a byte slice for an error message. Go interpolates the raw bytes with `%q`, which is
/// defined for invalid UTF-8; this is the closest total equivalent.
fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Port of `QueryEscape` (net/url/url.go:185). Note a space becomes `+`, not `%20`.
pub fn query_escape(s: &str) -> String {
    // `escape` in query mode emits only ASCII, so the result is always valid UTF-8.
    String::from_utf8(escape(s.as_bytes(), Encoding::QueryComponent)).unwrap_or_default()
}

// --- errors -------------------------------------------------------------------------------

/// The failures Go's `net/url` reports. See the module docs: the rendering approximates Go's and
/// is deliberately not asserted against it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlParseError {
    #[error("net/url: invalid control character in URL")]
    ControlCharacter,
    #[error("empty url")]
    EmptyUrl,
    #[error("missing protocol scheme")]
    MissingProtocolScheme,
    #[error("invalid URI for request")]
    InvalidUriForRequest,
    #[error("first path segment in URL cannot contain colon")]
    FirstSegmentColon,
    #[error("net/url: invalid userinfo")]
    InvalidUserinfo,
    #[error("invalid IP-literal")]
    InvalidIpLiteral,
    #[error("missing ']' in host")]
    MissingCloseBracket,
    #[error("invalid port {} after host", go_quote(.0))]
    InvalidPort(String),
    /// Go wraps `netip.ParseAddr`'s own error here; its wording is not reproduced.
    #[error("invalid host: ParseAddr({}): unable to parse IP", go_quote(.0))]
    InvalidHost(String),
    #[error("invalid URL escape {}", go_quote(.0))]
    Escape(String),
    #[error("invalid character {} in host name", go_quote(.0))]
    InvalidHostCharacter(String),
    #[error("invalid semicolon separator in query")]
    InvalidSemicolon,
}

/// Port of `url.Error` (net/url/url.go:32) — what `Parse` and `ParseRequestURI` actually return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{op} {}: {err}", go_quote(.url))]
pub struct UrlError {
    pub op: &'static str,
    pub url: String,
    pub err: UrlParseError,
}

// --- Userinfo -----------------------------------------------------------------------------

/// Port of `url.Userinfo` (net/url/url.go:330). Immutable in Go, and the empty-username case is
/// distinct from "no userinfo at all" — `http://@x` has a `Userinfo` whose username is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Userinfo {
    username: Vec<u8>,
    password: Vec<u8>,
    password_set: bool,
}

impl Userinfo {
    pub fn user(username: Vec<u8>) -> Self {
        Self {
            username,
            password: Vec::new(),
            password_set: false,
        }
    }

    pub fn user_password(username: Vec<u8>, password: Vec<u8>) -> Self {
        Self {
            username,
            password,
            password_set: true,
        }
    }

    pub fn username(&self) -> &[u8] {
        &self.username
    }

    pub fn password(&self) -> Option<&[u8]> {
        self.password_set.then_some(&self.password[..])
    }

    /// Port of `(*Userinfo).String` (net/url/url.go:354).
    fn encode(&self) -> Vec<u8> {
        let mut out = escape(&self.username, Encoding::UserPassword);
        if self.password_set {
            out.push(b':');
            out.extend_from_slice(&escape(&self.password, Encoding::UserPassword));
        }
        out
    }
}

// --- URL ----------------------------------------------------------------------------------

/// Port of `url.URL` (net/url/url.go:275).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoUrl {
    pub scheme: String,
    /// Encoded opaque data: everything after `scheme:` when the rest is not rootless-path shaped.
    pub opaque: String,
    pub user: Option<Userinfo>,
    /// `host` or `host:port` — the port is part of it, which is why an emptiness test on `host`
    /// passes for `http://:1`.
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    /// A hint at `path`'s original encoding, set only when it differs from the default escaping.
    pub raw_path: Vec<u8>,
    /// The encoded query, without the leading `?`.
    pub raw_query: String,
    pub fragment: Vec<u8>,
    pub raw_fragment: Vec<u8>,
    /// The raw URL ended in `?` with an empty query, so `to_go_string` must re-emit the `?`.
    pub force_query: bool,
    pub omit_host: bool,
}

/// Port of `getScheme` (net/url/url.go:368).
///
/// A scheme is `[a-zA-Z][a-zA-Z0-9+.-]*`. Anything else means there is no scheme and the whole
/// input is the path — the *only* error is a leading `:`.
fn get_scheme(raw_url: &str) -> Result<(&str, &str), UrlParseError> {
    for (i, c) in raw_url.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Ok(("", raw_url));
                }
            }
            b':' => {
                if i == 0 {
                    return Err(UrlParseError::MissingProtocolScheme);
                }
                return Ok((&raw_url[..i], &raw_url[i + 1..]));
            }
            // An invalid character means there is no valid scheme.
            _ => return Ok(("", raw_url)),
        }
    }
    Ok(("", raw_url))
}

/// Port of `stringContainsCTLByte` (net/url/url.go:1306).
fn contains_ctl_byte(s: &str) -> bool {
    s.bytes().any(|b| b < b' ' || b == 0x7f)
}

/// Port of `validOptionalPort` (net/url/url.go:761): empty, or `:` followed by ASCII digits.
///
/// A bare `":"` is valid and the number is never range-checked, so `:0` and
/// `:99999999999999999999` both pass.
fn valid_optional_port(port: &[u8]) -> bool {
    match port.split_first() {
        None => true,
        Some((b':', digits)) => digits.iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

/// Port of `validUserinfo` (net/url/url.go:1272). Note `@` is allowed, so
/// `http://user:p@ssword@host` parses with the last `@` as the delimiter.
fn valid_userinfo(s: &str) -> bool {
    s.chars().all(|r| {
        r.is_ascii_alphanumeric()
            || matches!(
                r,
                '-' | '.'
                    | '_'
                    | ':'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
                    | '@'
            )
    })
}

/// Port of `parseHost` (net/url/url.go:548).
fn parse_host(scheme: &str, host: &str) -> Result<Vec<u8>, UrlParseError> {
    if let Some(open) = host.rfind('[') {
        if open > 0 {
            // A `[` anywhere but the front is fatal, whatever surrounds it.
            return Err(UrlParseError::InvalidIpLiteral);
        }

        let Some(close) = host.rfind(']') else {
            return Err(UrlParseError::MissingCloseBracket);
        };

        let colon_port = &host[close + 1..];
        if !valid_optional_port(colon_port.as_bytes()) {
            return Err(UrlParseError::InvalidPort(colon_port.to_string()));
        }
        let unescaped_colon_port = unescape(colon_port.as_bytes(), Encoding::Host)?;

        let hostname = &host[1..close];
        // RFC 6874: `%25` introduces the zone identifier, which may use any escaping it likes —
        // unlike the host, which may only escape non-ASCII bytes.
        let unescaped_hostname = match hostname.find("%25") {
            Some(zone_idx) => {
                let mut out = unescape(&hostname.as_bytes()[..zone_idx], Encoding::Host)?;
                out.extend_from_slice(&unescape(&hostname.as_bytes()[zone_idx..], Encoding::Zone)?);
                out
            }
            None => unescape(hostname.as_bytes(), Encoding::Host)?,
        };

        // Only a valid IPv6 address may be bracketed. That excludes IPv4 — but notably not
        // IPv4-mapped addresses, so `[::ffff:1.2.3.4]` is accepted and `[1.2.3.4]` is not.
        if !parses_as_ipv6(&unescaped_hostname) {
            return Err(UrlParseError::InvalidHost(lossy(&unescaped_hostname)));
        }

        let mut out = Vec::with_capacity(unescaped_hostname.len() + 2);
        out.push(b'[');
        out.extend_from_slice(&unescaped_hostname);
        out.push(b']');
        out.extend_from_slice(&unescaped_colon_port);
        return Ok(out);
    }

    if let Some(i) = host.find(':') {
        let last_colon = host.rfind(':').unwrap_or(i);
        // RFC 3986 does not allow a colon in the host, but some databases spell a host list that
        // way, so Go enforces strict colons for http/https only. Under the default
        // `urlstrictcolons` setting the port is everything after the **first** colon for those
        // two schemes, which is why `http://a:1:2` fails as `invalid port ":1:2"`.
        let i = if last_colon != i && scheme != "http" && scheme != "https" {
            last_colon
        } else {
            i
        };
        let colon_port = &host[i..];
        if !valid_optional_port(colon_port.as_bytes()) {
            return Err(UrlParseError::InvalidPort(colon_port.to_string()));
        }
    }

    unescape(host.as_bytes(), Encoding::Host)
}

/// Go calls `netip.ParseAddr` and then rejects `addr.Is4()`. Rust's `Ipv6Addr` parser is the same
/// grammar for the v6 half, and it rejects every v4 form, so the two tests collapse into one —
/// except for the zone, which `netip` accepts and Rust's parser does not, so it is split off
/// first. An empty zone is rejected by both.
fn parses_as_ipv6(host: &[u8]) -> bool {
    let Ok(host) = std::str::from_utf8(host) else {
        return false;
    };
    let (addr, zone) = match host.find('%') {
        Some(i) => (&host[..i], Some(&host[i + 1..])),
        None => (host, None),
    };
    if zone.is_some_and(str::is_empty) {
        return false;
    }
    addr.parse::<Ipv6Addr>().is_ok()
}

/// Port of `parseAuthority` (net/url/url.go:511).
fn parse_authority(
    scheme: &str,
    authority: &str,
) -> Result<(Option<Userinfo>, Vec<u8>), UrlParseError> {
    // The userinfo/host split is at the LAST `@`, so an earlier one belongs to the userinfo.
    let at = authority.rfind('@');
    let host = parse_host(scheme, &authority[at.map_or(0, |i| i + 1)..])?;

    let Some(at) = at else {
        return Ok((None, host));
    };

    let userinfo = &authority[..at];
    if !valid_userinfo(userinfo) {
        return Err(UrlParseError::InvalidUserinfo);
    }
    let user = match userinfo.split_once(':') {
        None => Userinfo::user(unescape(userinfo.as_bytes(), Encoding::UserPassword)?),
        Some((username, password)) => Userinfo::user_password(
            unescape(username.as_bytes(), Encoding::UserPassword)?,
            unescape(password.as_bytes(), Encoding::UserPassword)?,
        ),
    };
    Ok((Some(user), host))
}

impl GoUrl {
    /// Port of `(*URL).setPath` (net/url/url.go:659). `raw_path` is kept only when it differs
    /// from the default escaping of `path`, so callers cannot come to rely on it.
    fn set_path(&mut self, p: &[u8]) -> Result<(), UrlParseError> {
        let path = unescape(p, Encoding::Path)?;
        self.raw_path = if escape(&path, Encoding::Path) == p {
            Vec::new()
        } else {
            p.to_vec()
        };
        self.path = path;
        Ok(())
    }

    /// Port of `(*URL).setFragment` (net/url/url.go:726).
    fn set_fragment(&mut self, f: &[u8]) -> Result<(), UrlParseError> {
        let frag = unescape(f, Encoding::Fragment)?;
        self.raw_fragment = if escape(&frag, Encoding::Fragment) == f {
            Vec::new()
        } else {
            f.to_vec()
        };
        self.fragment = frag;
        Ok(())
    }

    /// Port of `(*URL).EscapedPath` (net/url/url.go:686).
    pub fn escaped_path(&self) -> Vec<u8> {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Encoding::Path)
            && unescape(&self.raw_path, Encoding::Path).is_ok_and(|p| p == self.path)
        {
            return self.raw_path.clone();
        }
        if self.path == b"*" {
            // Go issue 11202 — the asterisk-form request target is not escaped.
            return b"*".to_vec();
        }
        escape(&self.path, Encoding::Path)
    }

    /// Port of `(*URL).EscapedFragment` (net/url/url.go:749).
    pub fn escaped_fragment(&self) -> Vec<u8> {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Encoding::Fragment)
            && unescape(&self.raw_fragment, Encoding::Fragment).is_ok_and(|f| f == self.fragment)
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Encoding::Fragment)
    }

    /// Port of `(*URL).Query` (net/url/url.go:1153).
    ///
    /// The decode error is **discarded**, and `ParseQuery` keeps every pair it could decode — so
    /// a query with one bad escape still yields the rest.
    pub fn query(&self) -> Values {
        parse_query(&self.raw_query).0
    }

    /// Port of `(*URL).String` (net/url/url.go:797).
    ///
    /// This is not the identity on the input. `Parse` decodes each component and this re-encodes
    /// it with the *canonical* escaping for its position, so `http://x/a%41b` comes back as
    /// `http://x/aAb`. `raw_path` and `raw_fragment` are what keep an unusual-but-valid encoding
    /// through a round trip; everything else is normalised.
    pub fn to_go_string(&self) -> String {
        let mut buf: Vec<u8> = Vec::new();

        if !self.scheme.is_empty() {
            buf.extend_from_slice(self.scheme.as_bytes());
            buf.push(b':');
        }

        if !self.opaque.is_empty() {
            buf.extend_from_slice(self.opaque.as_bytes());
        } else {
            if !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some() {
                let omit = self.omit_host && self.host.is_empty() && self.user.is_none();
                if !omit {
                    if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                        buf.extend_from_slice(b"//");
                    }
                    if let Some(user) = &self.user {
                        buf.extend_from_slice(&user.encode());
                        buf.push(b'@');
                    }
                    if !self.host.is_empty() {
                        buf.extend_from_slice(&escape(&self.host, Encoding::Host));
                    }
                }
            }

            let path = self.escaped_path();
            if !path.is_empty() && path[0] != b'/' && !self.host.is_empty() {
                buf.push(b'/');
            }
            if buf.is_empty() {
                // RFC 3986 §4.2 — a relative-path reference whose first segment contains a colon
                // would be misread as a scheme, so it is prefixed with `./`.
                let segment = match path.iter().position(|&c| c == b'/') {
                    Some(i) => &path[..i],
                    None => &path[..],
                };
                if segment.contains(&b':') {
                    buf.extend_from_slice(b"./");
                }
            }
            buf.extend_from_slice(&path);
        }

        if self.force_query || !self.raw_query.is_empty() {
            buf.push(b'?');
            buf.extend_from_slice(self.raw_query.as_bytes());
        }
        if !self.fragment.is_empty() {
            buf.push(b'#');
            buf.extend_from_slice(&self.escaped_fragment());
        }

        // Every component that is not a verbatim slice of the (UTF-8) input is re-escaped, and
        // `escape` emits only ASCII, so this is always valid UTF-8. The fallback is unreachable
        // rather than lossy; it is written as a fallback because library code here does not panic.
        String::from_utf8(buf).unwrap_or_default()
    }
}

/// Port of `validEncoded` (net/url/url.go:702).
fn valid_encoded(s: &[u8], mode: Encoding) -> bool {
    s.iter().all(|&c| match c {
        // pchar sub-delims, `:` and `@` — shouldEscape is not quite RFC-compliant, so Go checks
        // these itself. `[` and `]` are not in the RFC and are left alone by modern browsers.
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':'
        | b'@' | b'[' | b']' | b'%' => true,
        _ => !should_escape(c, mode),
    })
}

/// Port of `parse` (net/url/url.go:431) — the shared body of `Parse` and `ParseRequestURI`.
fn parse(raw_url: &str, via_request: bool) -> Result<GoUrl, UrlParseError> {
    if contains_ctl_byte(raw_url) {
        return Err(UrlParseError::ControlCharacter);
    }
    if raw_url.is_empty() && via_request {
        return Err(UrlParseError::EmptyUrl);
    }

    let mut url = GoUrl::default();
    if raw_url == "*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }

    let (scheme, mut rest) = get_scheme(raw_url)?;
    url.scheme = scheme.to_ascii_lowercase();

    if rest.ends_with('?') && rest.matches('?').count() == 1 {
        url.force_query = true;
        rest = &rest[..rest.len() - 1];
    } else if let Some((before, query)) = rest.split_once('?') {
        url.raw_query = query.to_string();
        rest = before;
    }

    if !rest.starts_with('/') {
        if !url.scheme.is_empty() {
            // A rootless path per RFC 3986 is opaque.
            url.opaque = rest.to_string();
            return Ok(url);
        }
        if via_request {
            return Err(UrlParseError::InvalidUriForRequest);
        }
        // RFC 3986 §3.3: the first segment of a relative-path reference cannot contain `:`, or
        // it would be mistaken for a scheme. Go issue 16822 — `cache_object:foo/bar`.
        let segment = rest.split('/').next().unwrap_or(rest);
        if segment.contains(':') {
            return Err(UrlParseError::FirstSegmentColon);
        }
    }

    if (!url.scheme.is_empty() || (!via_request && !rest.starts_with("///")))
        && rest.starts_with("//")
    {
        let mut authority = &rest[2..];
        rest = "";
        if let Some(i) = authority.find('/') {
            rest = &authority[i..];
            authority = &authority[..i];
        }
        let (user, host) = parse_authority(&url.scheme, authority)?;
        url.user = user;
        url.host = host;
    } else if !url.scheme.is_empty() && rest.starts_with('/') {
        // Go issue 46059 — an empty authority is not the same as no authority.
        url.omit_host = true;
    }

    url.set_path(rest.as_bytes())?;
    Ok(url)
}

/// Port of `Parse` (net/url/url.go:398).
///
/// Unlike [`parse_request_uri`] it **splits off a `#fragment`** first, which is the single most
/// consequential difference between the two: `http://x#f` is valid here and invalid there,
/// because there the `#` lands in the host.
pub fn go_parse(raw_url: &str) -> Result<GoUrl, UrlError> {
    let (before, frag) = match raw_url.split_once('#') {
        Some((before, frag)) => (before, frag),
        None => (raw_url, ""),
    };

    let mut url = parse(before, false).map_err(|err| UrlError {
        op: "parse",
        url: before.to_string(),
        err,
    })?;

    if frag.is_empty() {
        return Ok(url);
    }
    // Note the error carries the **whole** raw URL here and only the pre-`#` part above.
    url.set_fragment(frag.as_bytes()).map_err(|err| UrlError {
        op: "parse",
        url: raw_url.to_string(),
        err,
    })?;
    Ok(url)
}

/// Port of `ParseRequestURI` (net/url/url.go:419). Assumes the URL arrived in an HTTP request, so
/// only an absolute URI or an absolute path is allowed and a `#fragment` is *not* stripped.
pub fn parse_request_uri(raw_url: &str) -> Result<GoUrl, UrlError> {
    parse(raw_url, true).map_err(|err| UrlError {
        op: "parse",
        url: raw_url.to_string(),
        err,
    })
}

// --- Values -------------------------------------------------------------------------------

/// Port of `url.Values` (net/url/url.go:884).
///
/// Go's is a `map[string][]string` whose keys `Encode` sorts by byte value; a `BTreeMap<Vec<u8>>`
/// is already in that order, so the sort is free and cannot drift. The values are bytes because
/// `QueryUnescape` can produce any byte — `?a=%80` is a legal query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values(BTreeMap<Vec<u8>, Vec<Vec<u8>>>);

impl Values {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Port of `Values.Get` — the **first** value for the key, or empty.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key.as_bytes())?.first().map(Vec::as_slice)
    }

    /// Port of `Values.Set` — replaces every existing value for the key.
    pub fn set(&mut self, key: &str, value: &str) {
        self.0
            .insert(key.as_bytes().to_vec(), vec![value.as_bytes().to_vec()]);
    }

    /// Port of `Values.Add` — appends.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        self.0.entry(key.to_vec()).or_default().push(value.to_vec());
    }

    /// Port of `Values.Encode` (net/url/url.go:993). Sorted by key, `key=value` pairs joined by
    /// `&`, both halves through `QueryEscape` — so a space becomes `+`.
    pub fn encode(&self) -> String {
        let mut buf = String::new();
        for (key, values) in &self.0 {
            let key_escaped =
                String::from_utf8(escape(key, Encoding::QueryComponent)).unwrap_or_default();
            for value in values {
                if !buf.is_empty() {
                    buf.push('&');
                }
                buf.push_str(&key_escaped);
                buf.push('=');
                buf.push_str(
                    &String::from_utf8(escape(value, Encoding::QueryComponent)).unwrap_or_default(),
                );
            }
        }
        buf
    }
}

/// Port of `ParseQuery` (net/url/url.go:931) and `parseQuery` (:957).
///
/// Returns every pair it could decode **and** the first error, which is what lets
/// [`GoUrl::query`] discard the error and keep the rest. Four rules are worth stating:
///
/// - a setting with no `=` is a key set to the empty value;
/// - an empty setting (`a=1&&b=2`) is skipped without error;
/// - a setting containing a `;` is an error *and* is dropped — semicolons stopped being a
///   separator and are not silently tolerated either;
/// - a bad `%` escape drops that setting only.
///
/// Go's 10,000-parameter limit is a `godebug` knob rather than a parse rule and is not ported;
/// see [D-049].
pub fn parse_query(query: &str) -> (Values, Option<UrlParseError>) {
    let mut values = Values::new();
    let mut err: Option<UrlParseError> = None;
    let mut rest = query;

    while !rest.is_empty() {
        let (mut key, tail) = match rest.split_once('&') {
            Some((key, tail)) => (key, tail),
            None => (rest, ""),
        };
        rest = tail;

        if key.contains(';') {
            // Go reports this and keeps going, so a later good pair still lands.
            if err.is_none() {
                err = Some(UrlParseError::InvalidSemicolon);
            }
            continue;
        }
        if key.is_empty() {
            continue;
        }

        let mut value = "";
        if let Some((k, v)) = key.split_once('=') {
            key = k;
            value = v;
        }

        let decoded_key = match unescape(key.as_bytes(), Encoding::QueryComponent) {
            Ok(k) => k,
            Err(e) => {
                err = err.or(Some(e));
                continue;
            }
        };
        let decoded_value = match unescape(value.as_bytes(), Encoding::QueryComponent) {
            Ok(v) => v,
            Err(e) => {
                err = err.or(Some(e));
                continue;
            }
        };
        values.add(&decoded_key, &decoded_value);
    }

    (values, err)
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_url.json")).unwrap()
    }

    fn section(o: &Value, key: &str) -> Vec<Value> {
        o.get(key).unwrap().as_array().unwrap().clone()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    /// Go marshals a `[]byte` as base64, and a nil one as `null` — which is what an empty
    /// component records as when it was never set.
    fn bytes(v: &Value, key: &str) -> Vec<u8> {
        match v.get(key).unwrap() {
            Value::Null => Vec::new(),
            Value::String(s) => B64.decode(s).unwrap(),
            other => panic!("not a []byte: {other}"),
        }
    }

    fn assert_url_matches_go(name: &str, ours: &Result<GoUrl, UrlError>, case: &Value) {
        let want_err = s(case, "err");
        if !want_err.is_empty() {
            assert!(
                ours.is_err(),
                "{name}: Go failed with {want_err:?} and we did not"
            );
            return;
        }
        let ours = match ours {
            Ok(url) => url,
            Err(e) => panic!("{name}: Go parsed this and we failed with {e}"),
        };

        assert_eq!(ours.scheme, s(case, "scheme"), "{name}: scheme");
        assert_eq!(ours.opaque, s(case, "opaque"), "{name}: opaque");
        assert_eq!(ours.host, bytes(case, "host"), "{name}: host");
        assert_eq!(ours.path, bytes(case, "path"), "{name}: path");
        assert_eq!(ours.raw_path, bytes(case, "raw_path"), "{name}: raw_path");
        assert_eq!(ours.raw_query, s(case, "raw_query"), "{name}: raw_query");
        assert_eq!(ours.fragment, bytes(case, "fragment"), "{name}: fragment");
        assert_eq!(
            ours.raw_fragment,
            bytes(case, "raw_fragment"),
            "{name}: raw_fragment"
        );
        assert_eq!(
            ours.force_query,
            b(case, "force_query"),
            "{name}: force_query"
        );
        assert_eq!(ours.omit_host, b(case, "omit_host"), "{name}: omit_host");

        assert_eq!(ours.user.is_some(), b(case, "has_user"), "{name}: has_user");
        if let Some(user) = &ours.user {
            assert_eq!(user.username(), bytes(case, "username"), "{name}: username");
            assert_eq!(
                user.password().unwrap_or_default(),
                bytes(case, "password"),
                "{name}: password"
            );
            assert_eq!(
                user.password().is_some(),
                b(case, "password_set"),
                "{name}: password_set"
            );
        }

        assert_eq!(
            ours.escaped_path(),
            bytes(case, "escaped_path"),
            "{name}: escaped_path"
        );
        assert_eq!(
            ours.escaped_fragment(),
            bytes(case, "escaped_fragment"),
            "{name}: escaped_fragment"
        );
        assert_eq!(ours.to_go_string(), s(case, "string"), "{name}: String()");
    }

    #[test]
    fn parse_matches_go() {
        for case in section(&oracle(), "parse") {
            let name = s(&case, "name");
            assert_url_matches_go(&name, &go_parse(&name), &case);
        }
    }

    #[test]
    fn parse_request_uri_matches_go() {
        for case in section(&oracle(), "parse_request_uri") {
            let name = s(&case, "name");
            assert_url_matches_go(&name, &parse_request_uri(&name), &case);
        }
    }

    /// Runs all 256 byte values through every escaping mode, which pins the whole `shouldEscape`
    /// table rather than the characters a hand-picked corpus happens to contain. The modes
    /// disagree on 30-odd bytes, and `encodeFragment` disagrees with `encodePath` on the ones a
    /// reading is least likely to check.
    #[test]
    fn escape_matches_go_for_every_byte_in_every_mode() {
        let all: Vec<u8> = (0..=255).collect();
        for case in section(&oracle(), "escape") {
            let mode = match s(&case, "mode").as_str() {
                "query_component" => Encoding::QueryComponent,
                "path_segment" => Encoding::PathSegment,
                "path" => Encoding::Path,
                "host" => Encoding::Host,
                "user_password" => Encoding::UserPassword,
                "fragment" => Encoding::Fragment,
                other => panic!("unknown mode {other}"),
            };
            assert_eq!(escape(&all, mode), bytes(&case, "out"), "{mode:?}");
        }
    }

    #[test]
    fn unescape_matches_go() {
        for case in section(&oracle(), "unescape") {
            let name = format!("{}/{}", s(&case, "name"), s(&case, "mode"));
            let mode = match s(&case, "mode").as_str() {
                "query_component" => Encoding::QueryComponent,
                "path_segment" => Encoding::PathSegment,
                other => panic!("unknown mode {other}"),
            };
            let ours = unescape(s(&case, "in").as_bytes(), mode);
            assert_eq!(ours.is_ok(), b(&case, "ok"), "{name}: ok");
            if let Ok(out) = ours {
                assert_eq!(out, bytes(&case, "out"), "{name}: out");
            }
        }
    }

    #[test]
    fn parse_query_matches_go() {
        for case in section(&oracle(), "parse_query") {
            let name = s(&case, "name");
            let (values, err) = parse_query(&s(&case, "query"));

            assert_eq!(err.is_none(), b(&case, "ok"), "{name}: ok");

            let want: Vec<(Vec<u8>, Vec<Vec<u8>>)> = case
                .get("pairs")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    let vs = p
                        .get("values")
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| match v {
                            Value::Null => Vec::new(),
                            Value::String(s) => B64.decode(s).unwrap(),
                            other => panic!("not a []byte: {other}"),
                        })
                        .collect();
                    (bytes(p, "key"), vs)
                })
                .collect();

            let got: Vec<(Vec<u8>, Vec<Vec<u8>>)> = values
                .0
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(got, want, "{name}: pairs");
            assert_eq!(values.encode(), s(&case, "encode"), "{name}: encode");
        }
    }

    #[test]
    fn values_encode_matches_go() {
        for case in section(&oracle(), "encode") {
            let name = s(&case, "name");
            let mut values = Values::new();
            for (k, v) in case.get("input").unwrap().as_object().unwrap() {
                values.set(k, v.as_str().unwrap());
            }
            assert_eq!(values.encode(), s(&case, "out"), "{name}");
        }
    }

    /// The 3,529-case corpus that [D-003] built to verify a hand-written predicate now verifies
    /// the parser underneath it. This is the smaller shared corpus; the big one still runs in
    /// [`crate::utils`], unchanged, and it is what makes the delegation safe.
    #[test]
    fn is_valid_http_url_still_matches_go() {
        for case in section(&oracle(), "is_valid_http_url") {
            let name = s(&case, "name");
            assert_eq!(
                crate::utils::is_valid_http_url(&name),
                b(&case, "valid"),
                "{name}"
            );
        }
    }
}
