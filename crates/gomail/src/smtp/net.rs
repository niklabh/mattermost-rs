//! The TCP dial `net.Dialer.Dial("tcp", addr)` performs, with Go's error texts — which reach
//! the wire inside `app.admin.test_email.failure` as `dial tcp 127.0.0.1:1: connect: connection
//! refused` and friends.
//!
//! What is ported: `SplitHostPort`'s refusals, `LookupPort` (Go's builtin service table and
//! `/etc/services`, with `parsePort`'s range rules), IP literals (zones included), an empty host
//! (the local system), `isDomainName`'s pre-check, and the `OpError` / `AddrError` / `DNSError`
//! formatting. Addresses are tried in resolver order and the **first** failure is reported, as
//! `dialSerial` does.
//!
//! What is approximated, and why:
//!
//! - **Name resolution** goes through the C library (`getaddrinfo`, via tokio) where Go uses its
//!   own DNS client. For a name that does not exist Go says
//!   `lookup {host} on {server}: no such host`, naming the name server it asked; this port names
//!   the first `nameserver` of `/etc/resolv.conf` (Go's default `127.0.0.1:53` when there is
//!   none), which is the server Go asks first. A resolution that outlives the timeout reads
//!   `lookup {host} on {server}: i/o timeout`, as Go's does; other resolver failures (SERVFAIL
//!   and the like) are reported as `lookup {host} on {server}: server misbehaving`, Go's text
//!   for SERVFAIL, whatever the C library's actual reason.
//! - **Address order**: Go sorts DNS answers by RFC 6724 and races IPv4 against IPv6 (Happy
//!   Eyeballs); glibc applies the same RFC 6724 sort and this port dials serially, so the order
//!   agrees on ordinary hosts and the reported error is the first address's in both.
//! - **Errno texts** are the C library's `strerror` with the first letter lower-cased, which is
//!   what Go's table holds for every errno a connect can return.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;

/// `net.OpError{Op: "dial", Net: "tcp", …}` for the failures this dialer can produce.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DialError {
    /// `dial tcp: address {addr}: {why}` — `SplitHostPort` or `LookupPort` refused.
    #[error("dial tcp: address {addr}: {why}")]
    Address { addr: String, why: &'static str },
    /// `dial tcp: lookup {name}[ on {server}]: {why}`.
    #[error("dial tcp: lookup {name}{}: {why}", .server.as_ref().map(|s| format!(" on {s}")).unwrap_or_default())]
    Lookup {
        name: String,
        server: Option<String>,
        why: &'static str,
    },
    /// `dial tcp {addr}: connect: {errno}`.
    #[error("dial tcp {addr}: connect: {errno}")]
    Connect { addr: String, errno: String },
    /// `dial tcp {addr}: i/o timeout`.
    #[error("dial tcp {addr}: i/o timeout")]
    Timeout { addr: String },
}

/// Dial `addr` (`host:port`) over TCP with Go's `Dialer{Timeout: timeout}` semantics: `None`
/// is no timeout; the timeout covers resolution and every connect attempt together.
pub async fn dial_tcp(addr: &str, timeout: Option<Duration>) -> Result<TcpStream, DialError> {
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let (host, port) = split_host_port(addr)?;
    let port = lookup_port(port)?;
    let targets = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, resolve(host, port))
            .await
            .map_err(|_| DialError::Lookup {
                name: host.to_owned(),
                server: Some(first_nameserver()),
                why: "i/o timeout",
            })??,
        None => resolve(host, port).await?,
    };
    let mut first_err = None;
    for (target, shown) in targets {
        // `dialSerial` checks the deadline before each attempt: one already past (a negative
        // timeout) is an i/o timeout without a connect.
        if deadline.is_some_and(|d| d <= tokio::time::Instant::now()) {
            first_err.get_or_insert(DialError::Timeout { addr: shown });
            break;
        }
        let attempt = TcpStream::connect(target);
        let result = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, attempt).await {
                Ok(r) => r,
                Err(_) => {
                    first_err.get_or_insert(DialError::Timeout { addr: shown });
                    break;
                }
            },
            None => attempt.await,
        };
        match result {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                first_err.get_or_insert(DialError::Connect {
                    addr: shown,
                    errno: errno_text(&e),
                });
            }
        }
    }
    Err(first_err.unwrap_or(DialError::Lookup {
        name: host.to_owned(),
        server: None,
        why: "no such host",
    }))
}

/// `net.SplitHostPort` (ipsock.go:165).
pub fn split_host_port(hostport: &str) -> Result<(&str, &str), DialError> {
    const MISSING_PORT: &str = "missing port in address";
    const TOO_MANY_COLONS: &str = "too many colons in address";
    let err = |why| DialError::Address {
        addr: hostport.to_owned(),
        why,
    };
    let Some(i) = hostport.rfind(':') else {
        return Err(err(MISSING_PORT));
    };
    let (host, j, k);
    if hostport.starts_with('[') {
        let Some(end) = hostport.find(']') else {
            return Err(err("missing ']' in address"));
        };
        if end + 1 == hostport.len() {
            return Err(err(MISSING_PORT));
        } else if end + 1 != i {
            if hostport.as_bytes()[end + 1] == b':' {
                return Err(err(TOO_MANY_COLONS));
            }
            return Err(err(MISSING_PORT));
        }
        host = &hostport[1..end];
        (j, k) = (1, end + 1);
    } else {
        host = &hostport[..i];
        if host.contains(':') {
            return Err(err(TOO_MANY_COLONS));
        }
        (j, k) = (0, 0);
    }
    if hostport[j..].contains('[') {
        return Err(err("unexpected '[' in address"));
    }
    if hostport[k..].contains(']') {
        return Err(err("unexpected ']' in address"));
    }
    Ok((host, &hostport[i + 1..]))
}

/// `Resolver.LookupPort("tcp", service)` (lookup.go:415).
fn lookup_port(service: &str) -> Result<u16, DialError> {
    let (port, needs_lookup) = parse_port(service);
    let port = if needs_lookup {
        lookup_service(service).ok_or_else(|| DialError::Lookup {
            name: format!("tcp/{service}"),
            server: None,
            why: "unknown port",
        })?
    } else {
        port
    };
    u16::try_from(port).map_err(|_| DialError::Address {
        addr: service.to_owned(),
        why: "invalid port",
    })
}

/// `parsePort` (port.go:15): `(port, needsLookup)`, saturating out-of-range values so they fail
/// the 0..=65535 check.
fn parse_port(service: &str) -> (i64, bool) {
    if service.is_empty() {
        return (0, false);
    }
    const MAX: u64 = (1 << 32) - 1;
    const CUTOFF: u64 = 1 << 30;
    let (neg, digits) = match service.as_bytes()[0] {
        b'+' => (false, &service[1..]),
        b'-' => (true, &service[1..]),
        _ => (false, service),
    };
    let mut n: u64 = 0;
    for d in digits.chars() {
        let Some(d) = d.to_digit(10) else {
            return (0, true);
        };
        if n >= CUTOFF {
            n = MAX;
            break;
        }
        n *= 10;
        let nn = n + u64::from(d);
        if nn > MAX {
            n = MAX;
            break;
        }
        n = nn;
    }
    let port = if !neg && n >= CUTOFF {
        (CUTOFF - 1) as i64
    } else if neg && n > CUTOFF {
        CUTOFF as i64
    } else {
        n as i64
    };
    (if neg { -port } else { port }, false)
}

/// `goLookupPort("tcp", service)`: Go's builtin table, then `/etc/services`; the service name is
/// lower-cased (ASCII) and must fit Go's 32-byte buffer.
fn lookup_service(service: &str) -> Option<i64> {
    const BUILTIN: &[(&str, i64)] = &[
        ("ftp", 21),
        ("ftps", 990),
        ("gopher", 70),
        ("http", 80),
        ("https", 443),
        ("imap2", 143),
        ("imap3", 220),
        ("imaps", 993),
        ("pop3", 110),
        ("pop3s", 995),
        ("smtp", 25),
        ("submissions", 465),
        ("ssh", 22),
        ("telnet", 23),
    ];
    if service.len() > 32 {
        return None;
    }
    let lower = service.to_ascii_lowercase();
    // `/etc/services` entries are stored as written and override the builtins.
    let mut found = BUILTIN.iter().find(|(k, _)| *k == lower).map(|(_, p)| *p);
    if let Ok(text) = std::fs::read_to_string("/etc/services") {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 2 {
                continue;
            }
            let Some((port, proto)) = fields[1].split_once('/') else {
                continue;
            };
            let Ok(port) = port.parse::<i64>() else {
                continue;
            };
            if port <= 0 || proto != "tcp" {
                continue;
            }
            if fields
                .iter()
                .enumerate()
                .any(|(i, f)| i != 1 && *f == lower)
            {
                found = Some(port);
            }
        }
    }
    found
}

/// The addresses to try, each with the text Go's `OpError` would print for it.
async fn resolve(host: &str, port: u16) -> Result<Vec<(SocketAddr, String)>, DialError> {
    if host.is_empty() {
        // Go dials the local system and prints the address as ":port".
        return Ok(vec![(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            format!(":{port}"),
        )]);
    }
    let (bare, zone) = match host.rfind('%') {
        Some(i) if i > 0 => (&host[..i], Some(&host[i + 1..])),
        _ => (host, None),
    };
    if let Ok(ip) = bare.parse::<IpAddr>() {
        let shown = match (ip, zone) {
            (IpAddr::V6(v6), Some(zone)) => format!("[{v6}%{zone}]:{port}"),
            _ => SocketAddr::new(ip, port).to_string(),
        };
        let target = match (ip, zone) {
            (IpAddr::V6(_), Some(_)) => tokio::net::lookup_host(format!("[{host}]:{port}"))
                .await
                .ok()
                .and_then(|mut it| it.next())
                .unwrap_or_else(|| SocketAddr::new(ip, port)),
            _ => SocketAddr::new(ip, port),
        };
        return Ok(vec![(target, shown)]);
    }
    if !is_domain_name(host) {
        return Err(DialError::Lookup {
            name: host.to_owned(),
            server: None,
            why: "no such host",
        });
    }
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.map(|a| (a, a.to_string())).collect();
            if addrs.is_empty() {
                return Err(no_such_host(host));
            }
            Ok(addrs)
        }
        Err(e) => {
            // getaddrinfo's EAI_NONAME / EAI_NODATA are "no such host"; everything else is a
            // resolver failure.
            let text = e.to_string();
            if text.contains("not known") || text.contains("No address associated") {
                Err(no_such_host(host))
            } else {
                Err(DialError::Lookup {
                    name: host.to_owned(),
                    server: Some(first_nameserver()),
                    why: "server misbehaving",
                })
            }
        }
    }
}

fn no_such_host(host: &str) -> DialError {
    DialError::Lookup {
        name: host.to_owned(),
        server: Some(first_nameserver()),
        why: "no such host",
    }
}

/// The first `nameserver` of `/etc/resolv.conf` as Go's resolver names it (`ip:53`, IPv6 in
/// brackets), or Go's default `127.0.0.1:53`.
fn first_nameserver() -> String {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next() == Some("nameserver"))
                    .then(|| fields.next())
                    .flatten()
                    .and_then(|ip| ip.parse::<IpAddr>().ok())
                    .map(|ip| SocketAddr::new(ip, 53).to_string())
            })
        })
        .unwrap_or_else(|| "127.0.0.1:53".to_owned())
}

/// `isDomainName` (dnsclient.go:89).
pub fn is_domain_name(s: &str) -> bool {
    if s == "." {
        return true;
    }
    let b = s.as_bytes();
    let l = b.len();
    if l == 0 || l > 254 || (l == 254 && b[l - 1] != b'.') {
        return false;
    }
    let mut last = b'.';
    let mut non_numeric = false;
    let mut partlen = 0;
    for &c in b {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                non_numeric = true;
                partlen += 1;
            }
            b'0'..=b'9' => partlen += 1,
            b'-' => {
                if last == b'.' {
                    return false;
                }
                partlen += 1;
                non_numeric = true;
            }
            b'.' => {
                if last == b'.' || last == b'-' {
                    return false;
                }
                if partlen > 63 || partlen == 0 {
                    return false;
                }
                partlen = 0;
            }
            _ => return false,
        }
        last = c;
    }
    if last == b'-' || partlen > 63 {
        return false;
    }
    non_numeric
}

/// Go's errno text: the C library's `strerror` with the first letter lower-cased.
pub fn errno_text(e: &io::Error) -> String {
    let base = match e.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code).to_string(),
        None => e.to_string(),
    };
    let base = match base.find(" (os error") {
        Some(i) => &base[..i],
        None => &base,
    };
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_host_port_matches_go() {
        let e = |a: &str| split_host_port(a).unwrap_err().to_string();
        assert_eq!(split_host_port("a:1").unwrap(), ("a", "1"));
        assert_eq!(split_host_port("[::1]:25").unwrap(), ("::1", "25"));
        assert_eq!(split_host_port(":25").unwrap(), ("", "25"));
        assert_eq!(split_host_port("a:").unwrap(), ("a", ""));
        assert_eq!(e("a"), "dial tcp: address a: missing port in address");
        assert_eq!(
            e("::1:1"),
            "dial tcp: address ::1:1: too many colons in address"
        );
        assert_eq!(
            e("[::1:1"),
            "dial tcp: address [::1:1: missing ']' in address"
        );
        assert_eq!(
            e("[::1]"),
            "dial tcp: address [::1]: missing port in address"
        );
        assert_eq!(
            e("[::1]x:1"),
            "dial tcp: address [::1]x:1: missing port in address"
        );
        assert_eq!(
            e("[::1]::1"),
            "dial tcp: address [::1]::1: too many colons in address"
        );
        assert_eq!(
            e("a[:1"),
            "dial tcp: address a[:1: unexpected '[' in address"
        );
        assert_eq!(
            e("a]:1"),
            "dial tcp: address a]:1: unexpected ']' in address"
        );
    }

    #[test]
    fn ports_match_go() {
        assert_eq!(lookup_port("25").unwrap(), 25);
        assert_eq!(lookup_port("+25").unwrap(), 25);
        assert_eq!(lookup_port("").unwrap(), 0);
        assert_eq!(lookup_port("65535").unwrap(), 65535);
        assert_eq!(lookup_port("SMTP").unwrap(), 25);
        let e = |p: &str| lookup_port(p).unwrap_err().to_string();
        assert_eq!(e("65536"), "dial tcp: address 65536: invalid port");
        assert_eq!(e("-1"), "dial tcp: address -1: invalid port");
        assert_eq!(
            e("99999999999999"),
            "dial tcp: address 99999999999999: invalid port"
        );
        assert_eq!(
            e("nosuchservice"),
            "dial tcp: lookup tcp/nosuchservice: unknown port"
        );
        assert_eq!(e("2x"), "dial tcp: lookup tcp/2x: unknown port");
    }

    #[test]
    fn domain_names_match_go() {
        for (name, ok) in [
            ("example.com", true),
            ("256.1.1.1", false),
            ("1.2.3", false),
            ("exa mple.com", false),
            ("a-.com", false),
            ("-a.com", false),
            ("a..b", false),
            ("_srv.example", true),
            ("a.", true),
            (".", true),
            ("", false),
        ] {
            assert_eq!(is_domain_name(name), ok, "{name}");
        }
    }

    #[test]
    fn errno_texts_are_lower_cased() {
        assert_eq!(
            errno_text(&io::Error::from_raw_os_error(111)),
            "connection refused"
        );
        assert_eq!(
            errno_text(&io::Error::from_raw_os_error(101)),
            "network is unreachable"
        );
    }
}
