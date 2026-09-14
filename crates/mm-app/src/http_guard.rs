//! Port of `shared/httpservice` — the guard every outbound request Go makes on a user's behalf
//! passes through (client.go, httpservice.go).
//!
//! # A request to an internal address is refused unless the operator allowed it
//!
//! `MakeClient(false)` wraps the dialer: the URL's host is looked up, and each resolved IP is
//! dialled only if it is neither in a **reserved range** (thirty CIDRs — private, loopback,
//! link-local, documentation, multicast, the IPv6 equivalents) nor one of the machine's **own**
//! interface addresses — unless `ServiceSettings.AllowedUntrustedInternalConnections` names the
//! host verbatim or a CIDR containing the IP. A refusal is a transport error, which every caller
//! turns into "the request failed" — `getRedirectLocation` caches an empty location for it.
//!
//! So on a stack whose allow-list is empty, `http://localhost:8065/…` from either server is a
//! refusal, and the parity suites can only see the refusals; the accept path is unit-tested.
//!
//! # `IsReservedIP` first takes `To4()`
//!
//! An IPv4-mapped IPv6 address (`::ffff:10.0.0.1`) is classified as its IPv4 half, which is why
//! the Go oracle in `fixtures/behaviour_httpservice.json` carries the mapped forms.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// `ConnectTimeout` (client.go:22).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// `RequestTimeout` (client.go:23), the whole request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The thirty reserved ranges of `client.go`'s `init`, in its order, as `(network, prefix)`.
const RESERVED_V4: &[(Ipv4Addr, u32)] = &[
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (Ipv4Addr::new(255, 255, 255, 255), 32),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
];
const RESERVED_V6: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 128),
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 128),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x2001, 0x10, 0, 0, 0, 0, 0, 0), 28),
    (Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28),
];

fn v4_in(net: Ipv4Addr, prefix: u32, ip: Ipv4Addr) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(ip) & mask == u32::from(net) & mask
}

fn v6_in(net: Ipv6Addr, prefix: u32, ip: Ipv6Addr) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    u128::from(ip) & mask == u128::from(net) & mask
}

/// Port of `httpservice.IsReservedIP` (client.go:30): `To4()` first, then the table.
pub fn is_reserved_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => RESERVED_V4
            .iter()
            .any(|&(net, prefix)| v4_in(net, prefix, v4)),
        IpAddr::V6(v6) => RESERVED_V6
            .iter()
            .any(|&(net, prefix)| v6_in(net, prefix, v6)),
    }
}

/// Port of `httpservice.IsOwnIP` (client.go:46): the machine's own interface addresses.
pub fn is_own_ip(ip: IpAddr) -> Result<bool, std::io::Error> {
    let interfaces = if_addrs::get_if_addrs()?;
    Ok(interfaces.iter().any(|interface| interface.ip() == ip))
}

/// `splitFields` — a comma or any whitespace separates entries.
fn allow_entries(allowed: &str) -> impl Iterator<Item = &str> {
    allowed
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
}

/// Port of `isAllowedInternalHost` (httpservice.go:76): the host, verbatim, among the entries.
pub fn is_allowed_internal_host(host: &str, allowed: &str) -> bool {
    allow_entries(allowed).any(|entry| entry == host)
}

/// A CIDR entry of the allow-list, parsed the way `net.ParseCIDR` accepts it.
fn parse_cidr(entry: &str) -> Option<(IpAddr, u32)> {
    let (addr, prefix) = entry.split_once('/')?;
    let addr: IpAddr = addr.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((addr, prefix))
}

fn cidr_contains(net: IpAddr, prefix: u32, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => v4_in(net, prefix, ip),
        (IpAddr::V6(net), IpAddr::V6(ip)) => v6_in(net, prefix, ip),
        // `ipRange.Contains` of an IPv4 range against an IPv6 address works through `To4()`
        // too; the reverse never matches.
        (IpAddr::V4(net), IpAddr::V6(ip)) => {
            ip.to_ipv4_mapped().is_some_and(|ip| v4_in(net, prefix, ip))
        }
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

/// Port of `checkInternalIP` (httpservice.go:83): `Ok` when the IP may be dialled, `Err` with
/// Go's sentence when it is reserved or the machine's own and no allow-list CIDR covers it.
pub fn check_internal_ip(ip: IpAddr, allowed: &str) -> Result<(), String> {
    let reserved = is_reserved_ip(ip);
    let own = is_own_ip(ip).map_err(|err| format!("unable to determine if IP is own IP: {err}"))?;
    if !reserved && !own {
        return Ok(());
    }
    if allow_entries(allowed)
        .filter_map(parse_cidr)
        .any(|(net, prefix)| cidr_contains(net, prefix, ip))
    {
        return Ok(());
    }
    if reserved {
        return Err(format!(
            "IP {ip} is in a reserved range and not in AllowedUntrustedInternalConnections"
        ));
    }
    Err(format!(
        "IP {ip} is a self-assigned IP and not in AllowedUntrustedInternalConnections"
    ))
}

/// Why a guarded request did not get a response.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// The URL would not parse, or names no host.
    #[error("invalid URL: {0}")]
    Url(String),
    /// `ErrAddressForbidden`, with the per-IP reasons Go appends.
    #[error(
        "address forbidden, you may need to set AllowedUntrustedInternalConnections to allow an integration access to your internal network: {0}"
    )]
    Forbidden(String),
    /// The lookup, the dial or the request itself failed.
    #[error("{0}")]
    Transport(String),
}

/// Port of the client `MakeClient(false)` builds: the allow-list, the insecure-TLS flag, and
/// the two timeouts. `head_without_redirects` is the shape `getRedirectLocation` needs —
/// `CheckRedirect` returning `ErrUseLastResponse`.
#[derive(Debug, Clone)]
pub struct GuardedClient {
    allowed_untrusted_internal_connections: String,
    insecure: bool,
}

impl GuardedClient {
    pub fn new(allowed_untrusted_internal_connections: &str, insecure: bool) -> Self {
        Self {
            allowed_untrusted_internal_connections: allowed_untrusted_internal_connections
                .to_owned(),
            insecure,
        }
    }

    /// The addresses the dialer may use for `host:port`, or the refusal: the host verbatim in
    /// the allow-list skips every IP check, as `allowHost` does before `LookupIP`.
    async fn resolve(&self, host: &str, port: u16) -> Result<Option<Vec<SocketAddr>>, GuardError> {
        if is_allowed_internal_host(host, &self.allowed_untrusted_internal_connections) {
            return Ok(None);
        }
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|err| GuardError::Transport(err.to_string()))?
            .collect();
        let mut permitted = Vec::new();
        let mut reasons = Vec::new();
        for addr in addrs {
            match check_internal_ip(addr.ip(), &self.allowed_untrusted_internal_connections) {
                Ok(()) => permitted.push(addr),
                Err(reason) => reasons.push(reason),
            }
        }
        if permitted.is_empty() {
            return Err(GuardError::Forbidden(reasons.join("; ")));
        }
        Ok(Some(permitted))
    }

    /// A `HEAD` that does not follow redirects, so the `Location` header is the answer.
    #[tracing::instrument(skip(self), fields(host))]
    pub async fn head_without_redirects(&self, url: &str) -> Result<reqwest::Response, GuardError> {
        let parsed = reqwest::Url::parse(url).map_err(|err| GuardError::Url(err.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| GuardError::Url("no host".to_owned()))?
            .to_owned();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| GuardError::Url("no port".to_owned()))?;
        tracing::Span::current().record("host", host.as_str());

        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .danger_accept_invalid_certs(self.insecure);
        if let Some(addrs) = self.resolve(&host, port).await? {
            builder = builder.resolve_to_addrs(&host, &addrs);
        }
        let client = builder
            .build()
            .map_err(|err| GuardError::Transport(err.to_string()))?;
        client
            .head(parsed)
            .send()
            .await
            .map_err(|err| GuardError::Transport(err.to_string()))
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    /// Every case of `fixtures/behaviour_httpservice.json` — Go's own `IsReservedIP` over the
    /// edges of all thirty ranges, the public addresses, and the IPv4-mapped forms.
    #[test]
    fn is_reserved_ip_matches_go() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_httpservice.json"))
                .expect("the fixture is JSON");
        let cases = fixture["is_reserved_ip"].as_array().expect("cases");
        assert!(cases.len() >= 90, "the corpus walks every range");
        for case in cases {
            let ip: IpAddr = case["ip"].as_str().expect("an ip").parse().expect("parses");
            assert_eq!(
                is_reserved_ip(ip),
                case["reserved"].as_bool().expect("a bool"),
                "{ip}"
            );
        }
    }

    /// `checkInternalIP`, transcribed: a public IP passes; a reserved one needs a CIDR that
    /// contains it, spelled in the space-or-comma list; a host entry is not a CIDR.
    #[test]
    fn check_internal_ip_follows_the_allow_list() {
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let private: IpAddr = "10.1.2.3".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(check_internal_ip(public, "").is_ok());
        assert!(check_internal_ip(private, "").is_err());
        assert!(check_internal_ip(private, "10.0.0.0/8").is_ok());
        assert!(check_internal_ip(private, "192.168.0.0/16, 10.0.0.0/8").is_ok());
        assert!(
            check_internal_ip(private, "10.1.2.3").is_err(),
            "not a CIDR"
        );
        assert!(check_internal_ip(loopback, "127.0.0.1/32 localhost").is_ok());
        assert!(check_internal_ip(loopback, "10.0.0.0/8").is_err());
        assert!(is_allowed_internal_host(
            "localhost",
            "127.0.0.1/32 localhost"
        ));
        assert!(is_allowed_internal_host("a", "a,b c"));
        assert!(!is_allowed_internal_host("localhos", "localhost"));
    }
}
