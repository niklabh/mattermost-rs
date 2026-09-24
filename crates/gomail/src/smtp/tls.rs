//! The TLS client `crypto/tls` gives Mattermost — for implicit TLS (`tls.DialWithDialer`) and
//! for `STARTTLS` (`tls.Client`) — on rustls, with Go's certificate checks **in Go's order** and
//! Go's error texts.
//!
//! # Verification
//!
//! `InsecureSkipVerify` accepts any certificate (the handshake signature is still checked, as
//! Go checks it). Otherwise `GoVerifier` does what `crypto/x509.Certificate.Verify` does for a
//! server certificate, in its order:
//!
//! 1. the leaf's validity period — `x509: certificate has expired or is not yet valid: current
//!    time … is after …`;
//! 2. the host name, by Go's `VerifyHostname` rules (IP SANs for an IP, DNS SANs with a
//!    left-most wildcard otherwise, the legacy-Common-Name message when there are no SANs) —
//!    `x509: certificate is valid for …, not …`;
//! 3. the chain, to the **system** roots Go would load on Linux (`SSL_CERT_FILE`, the six
//!    bundle paths, then `SSL_CERT_DIR` or `/etc/ssl/certs` and `/etc/pki/tls/certs`) — through
//!    rustls-webpki. Its unknown-issuer verdict is Go's `x509: certificate signed by unknown
//!    authority`.
//!
//! Every verification failure reads `tls: failed to verify certificate: {x509 error}`.
//!
//! # Parity risks
//!
//! - Path building is webpki's, not Go's: a chain Go accepts through an unusual path (cross-signs,
//!   name constraints, policy) may be judged differently, and any chain failure other than an
//!   unknown issuer is rendered as `x509: ` plus rustls's description, not Go's text. The
//!   `UnknownAuthorityError` hint Go appends when a candidate issuer failed to verify is absent.
//! - Handshake failures other than the ones mapped in [`TlsError`] (a non-TLS peer, a remote
//!   alert, EOF, a deadline) are rendered as `tls: ` plus rustls's description.
//! - Protocol versions and cipher suites are rustls's safe defaults (TLS 1.2 and 1.3), which a
//!   Go 1.26 client also offers; a server that only speaks something Go still accepts and rustls
//!   does not would fail here and not in Go.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// The two `tls.Config` fields Mattermost sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    /// `ServerName`: the name verified against the certificate and sent as SNI (unless an IP).
    pub server_name: String,
    /// `InsecureSkipVerify`.
    pub insecure_skip_verify: bool,
}

/// A TLS failure with Go's text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TlsError {
    /// `makeClientHello` (handshake_client.go:46).
    #[error("tls: either ServerName or InsecureSkipVerify must be specified in the tls.Config")]
    NoServerName,
    /// `tls: failed to verify certificate: %w` (handshake_client.go).
    #[error("tls: failed to verify certificate: {0}")]
    Verify(X509Error),
    /// `RecordHeaderError` for a peer whose first record is not a handshake or alert.
    #[error("tls: first record does not look like a TLS handshake")]
    NotTls,
    /// `net.OpError{Op: "remote error", Err: alert}`.
    #[error("remote error: tls: {0}")]
    RemoteAlert(&'static str),
    /// `io.EOF` mid-handshake.
    #[error("EOF")]
    Eof,
    /// The dialer's context expired during the handshake.
    #[error("context deadline exceeded")]
    DeadlineExceeded,
    /// A transport error, already in Go's `OpError` form.
    #[error("{0}")]
    Io(String),
    /// Anything else: `tls: ` and rustls's description (not Go's text — see the module docs).
    #[error("tls: {0}")]
    Other(String),
}

/// The `crypto/x509` verification errors this port reproduces.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum X509Error {
    #[error("x509: certificate signed by unknown authority")]
    UnknownAuthority,
    /// `CertificateInvalidError{Reason: Expired}` with its detail.
    #[error("x509: certificate has expired or is not yet valid: {0}")]
    Expired(String),
    /// `HostnameError.Error()`, already formatted.
    #[error("{0}")]
    Hostname(String),
    /// `SystemRootsError`.
    #[error("x509: failed to load system roots and no roots provided")]
    NoSystemRoots,
    /// The leaf did not parse.
    #[error("x509: malformed certificate")]
    Malformed,
    /// A chain failure other than an unknown issuer, in rustls's words.
    #[error("x509: {0}")]
    Other(String),
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// `tls.Client(conn, config).Handshake()`. The deadline, when given, bounds the handshake and
/// reads [`TlsError::DeadlineExceeded`] when it passes — `tls.DialWithDialer`'s context.
pub async fn client_handshake(
    stream: TcpStream,
    config: &TlsConfig,
    deadline: Option<tokio::time::Instant>,
) -> Result<TlsStream<TcpStream>, TlsError> {
    if config.server_name.is_empty() && !config.insecure_skip_verify {
        return Err(TlsError::NoServerName);
    }
    let local = stream.local_addr().ok();
    let peer = stream.peer_addr().ok();
    let verifier = Arc::new(GoVerifier {
        host: config.server_name.clone(),
        insecure: config.insecure_skip_verify,
        provider: provider(),
    });
    let client_config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Other(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // Go sends SNI only for a non-IP name; rustls sends none for an IP address, so an empty or
    // non-DNS name maps to one.
    let server_name = match config.server_name.parse::<std::net::IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(config.server_name.trim_end_matches('.').to_owned())
            .unwrap_or(ServerName::IpAddress(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED).into(),
            )),
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let handshake = connector.connect(server_name, stream);
    let result = match deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline, handshake).await {
            Ok(r) => r,
            Err(_) => return Err(TlsError::DeadlineExceeded),
        },
        None => handshake.await,
    };
    result.map_err(|e| map_io_error(&e, local, peer))
}

/// A tokio-rustls error in Go's words.
pub(crate) fn map_io_error(
    e: &std::io::Error,
    local: Option<std::net::SocketAddr>,
    peer: Option<std::net::SocketAddr>,
) -> TlsError {
    if let Some(tls) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        return map_rustls_error(tls);
    }
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        return TlsError::Eof;
    }
    TlsError::Io(super::conn::op_error_text("read", local, peer, e))
}

fn map_rustls_error(e: &rustls::Error) -> TlsError {
    use rustls::{Error, InvalidMessage};
    match e {
        Error::InvalidCertificate(CertificateError::Other(other)) => {
            match other.0.downcast_ref::<X509Error>() {
                Some(x) => TlsError::Verify(x.clone()),
                None => TlsError::Verify(X509Error::Other(other.0.to_string())),
            }
        }
        Error::InvalidCertificate(CertificateError::UnknownIssuer) => {
            TlsError::Verify(X509Error::UnknownAuthority)
        }
        Error::InvalidCertificate(other) => {
            TlsError::Verify(X509Error::Other(format!("{other:?}")))
        }
        Error::InvalidMessage(
            InvalidMessage::InvalidContentType | InvalidMessage::UnknownProtocolVersion,
        ) => TlsError::NotTls,
        Error::AlertReceived(alert) => TlsError::RemoteAlert(alert_text(*alert)),
        other => TlsError::Other(other.to_string()),
    }
}

/// `alertText` (crypto/tls/alert.go:58).
fn alert_text(alert: rustls::AlertDescription) -> &'static str {
    use rustls::AlertDescription as A;
    match alert {
        A::CloseNotify => "close notify",
        A::UnexpectedMessage => "unexpected message",
        A::BadRecordMac => "bad record MAC",
        A::DecryptionFailed => "decryption failed",
        A::RecordOverflow => "record overflow",
        A::DecompressionFailure => "decompression failure",
        A::HandshakeFailure => "handshake failure",
        A::BadCertificate => "bad certificate",
        A::UnsupportedCertificate => "unsupported certificate",
        A::CertificateRevoked => "revoked certificate",
        A::CertificateExpired => "expired certificate",
        A::CertificateUnknown => "unknown certificate",
        A::IllegalParameter => "illegal parameter",
        A::UnknownCA => "unknown certificate authority",
        A::AccessDenied => "access denied",
        A::DecodeError => "error decoding message",
        A::DecryptError => "error decrypting message",
        A::ExportRestriction => "export restriction",
        A::ProtocolVersion => "protocol version not supported",
        A::InsufficientSecurity => "insufficient security level",
        A::InternalError => "internal error",
        A::InappropriateFallback => "inappropriate fallback",
        A::UserCanceled => "user canceled",
        A::NoRenegotiation => "no renegotiation",
        A::MissingExtension => "missing extension",
        A::UnsupportedExtension => "unsupported extension",
        A::CertificateUnobtainable => "certificate unobtainable",
        A::UnrecognisedName => "unrecognized name",
        A::BadCertificateStatusResponse => "bad certificate status response",
        A::BadCertificateHashValue => "bad certificate hash value",
        A::UnknownPSKIdentity => "unknown PSK identity",
        A::CertificateRequired => "certificate required",
        A::NoApplicationProtocol => "no application protocol",
        A::EncryptedClientHelloRequired => "encrypted client hello required",
        _ => "alert",
    }
}

/// The verifier described in the module docs.
#[derive(Debug)]
struct GoVerifier {
    host: String,
    insecure: bool,
    provider: Arc<CryptoProvider>,
}

impl GoVerifier {
    fn verify(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<(), X509Error> {
        let leaf = der::CertInfo::parse(end_entity.as_ref()).ok_or(X509Error::Malformed)?;
        let now_secs = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
        if now_secs < leaf.not_before.timestamp() {
            return Err(X509Error::Expired(format!(
                "current time {} is before {}",
                local_rfc3339(now_secs),
                utc_rfc3339(leaf.not_before)
            )));
        }
        if now_secs > leaf.not_after.timestamp() {
            return Err(X509Error::Expired(format!(
                "current time {} is after {}",
                local_rfc3339(now_secs),
                utc_rfc3339(leaf.not_after)
            )));
        }
        if !self.host.is_empty() {
            verify_hostname(&leaf, &self.host)?;
        }
        let roots = system_roots().ok_or(X509Error::NoSystemRoots)?;
        if roots
            .1
            .iter()
            .any(|der| der.as_slice() == end_entity.as_ref())
        {
            return Ok(());
        }
        let parsed = rustls::server::ParsedCertificate::try_from(end_entity)
            .map_err(|_| X509Error::Malformed)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &roots.0,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )
        .map_err(|e| match e {
            rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer) => {
                X509Error::UnknownAuthority
            }
            other => X509Error::Other(other.to_string()),
        })
    }
}

impl ServerCertVerifier for GoVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.insecure {
            return Ok(ServerCertVerified::assertion());
        }
        self.verify(end_entity, intermediates, now)
            .map(|()| ServerCertVerified::assertion())
            .map_err(|e| {
                rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(
                    Arc::new(e),
                )))
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `time.Now().Format(time.RFC3339)` in the local zone ("Z" when the offset is zero).
fn local_rfc3339(unix: i64) -> String {
    let t = DateTime::from_timestamp(unix, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local);
    if t.offset().local_minus_utc() == 0 {
        t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
    }
}

/// A certificate time formatted as Go formats `NotBefore`/`NotAfter` (UTC, RFC 3339).
fn utc_rfc3339(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `Certificate.VerifyHostname` (x509/verify.go) and `HostnameError.Error`.
fn verify_hostname(cert: &der::CertInfo, h: &str) -> Result<(), X509Error> {
    let candidate_ip = if h.len() >= 3 && h.starts_with('[') && h.ends_with(']') {
        &h[1..h.len() - 1]
    } else {
        h
    };
    if let Ok(ip) = candidate_ip.parse::<std::net::IpAddr>() {
        if cert
            .ip_addresses
            .iter()
            .any(|c| canonical_ip(*c) == canonical_ip(ip))
        {
            return Ok(());
        }
        return Err(hostname_error(cert, candidate_ip));
    }
    let candidate = to_lower_ascii(h);
    let valid_candidate = valid_hostname(&candidate, false);
    let host_parts = split_hostname(&candidate);
    for pattern in &cert.dns_names {
        let matched = if valid_candidate && valid_hostname(pattern, true) {
            match_hostnames(pattern, &host_parts)
        } else {
            match_exactly(pattern, &candidate)
        };
        if matched {
            return Ok(());
        }
    }
    Err(hostname_error(cert, h))
}

fn canonical_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        v4 => v4,
    }
}

fn hostname_error(cert: &der::CertInfo, host: &str) -> X509Error {
    const MAX_NAMES_INCLUDED: usize = 100;
    if !cert.has_san_extension && match_hostnames(&cert.common_name, &split_hostname(host)) {
        return X509Error::Hostname(
            "x509: certificate relies on legacy Common Name field, use SANs instead".to_owned(),
        );
    }
    let valid = if host.parse::<std::net::IpAddr>().is_ok() {
        if cert.ip_addresses.is_empty() {
            return X509Error::Hostname(format!(
                "x509: cannot validate certificate for {host} because it doesn't contain any IP SANs"
            ));
        }
        if cert.ip_addresses.len() >= MAX_NAMES_INCLUDED {
            return X509Error::Hostname(format!(
                "x509: certificate is valid for {} IP SANs, but none matched {host}",
                cert.ip_addresses.len()
            ));
        }
        cert.ip_addresses
            .iter()
            .map(|ip| canonical_ip(*ip).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        if cert.dns_names.len() >= MAX_NAMES_INCLUDED {
            return X509Error::Hostname(format!(
                "x509: certificate is valid for {} names, but none matched {host}",
                cert.dns_names.len()
            ));
        }
        cert.dns_names.join(", ")
    };
    if valid.is_empty() {
        return X509Error::Hostname(format!(
            "x509: certificate is not valid for any names, but wanted to match {host}"
        ));
    }
    X509Error::Hostname(format!(
        "x509: certificate is valid for {valid}, not {host}"
    ))
}

fn to_lower_ascii(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// `splitHostname`.
fn split_hostname(host: &str) -> Vec<String> {
    to_lower_ascii(host.strip_suffix('.').unwrap_or(host))
        .split('.')
        .map(str::to_owned)
        .collect()
}

/// `validHostname`.
fn valid_hostname(host: &str, is_pattern: bool) -> bool {
    let host = if is_pattern {
        host
    } else {
        host.strip_suffix('.').unwrap_or(host)
    };
    if host.is_empty() || host == "*" {
        return false;
    }
    for (i, part) in host.split('.').enumerate() {
        if part.is_empty() {
            return false;
        }
        if is_pattern && i == 0 && part == "*" {
            continue;
        }
        for (j, c) in part.char_indices() {
            if c.is_ascii_alphanumeric() || (c == '-' && j != 0) || c == '_' {
                continue;
            }
            return false;
        }
    }
    true
}

/// `matchExactly`.
fn match_exactly(a: &str, b: &str) -> bool {
    if a.is_empty() || a == "." || b.is_empty() || b == "." {
        return false;
    }
    to_lower_ascii(a) == to_lower_ascii(b)
}

/// `matchHostnames`.
fn match_hostnames(pattern: &str, host_parts: &[String]) -> bool {
    let pattern = to_lower_ascii(pattern);
    if pattern.is_empty() || host_parts.is_empty() {
        return false;
    }
    let pattern_parts: Vec<&str> = pattern.split('.').collect();
    if pattern_parts.len() != host_parts.len() {
        return false;
    }
    pattern_parts
        .iter()
        .zip(host_parts)
        .enumerate()
        .all(|(i, (p, h))| (i == 0 && *p == "*") || p == h)
}

/// The Linux system roots Go loads (`loadSystemRoots`, x509/root_unix.go), once. `None` when
/// none could be loaded. The second element keeps the DER of every root, for Go's
/// "the leaf itself is a root" shortcut.
#[allow(clippy::type_complexity)]
fn system_roots() -> Option<&'static (RootCertStore, Vec<Vec<u8>>)> {
    static ROOTS: OnceLock<Option<(RootCertStore, Vec<Vec<u8>>)>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            use rustls::pki_types::pem::PemObject as _;
            const CERT_FILES: &[&str] = &[
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/ca-bundle.pem",
                "/etc/pki/tls/cacert.pem",
                "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
                "/etc/ssl/cert.pem",
            ];
            const CERT_DIRS: &[&str] = &["/etc/ssl/certs", "/etc/pki/tls/certs"];
            let mut pem_blobs: Vec<Vec<u8>> = Vec::new();
            let files: Vec<String> = match std::env::var("SSL_CERT_FILE") {
                Ok(f) if !f.is_empty() => vec![f],
                _ => CERT_FILES.iter().map(|s| (*s).to_owned()).collect(),
            };
            for file in files {
                if let Ok(data) = std::fs::read(&file) {
                    pem_blobs.push(data);
                    break;
                }
            }
            let dirs: Vec<String> = match std::env::var("SSL_CERT_DIR") {
                Ok(d) if !d.is_empty() => d.split(':').map(str::to_owned).collect(),
                _ => CERT_DIRS.iter().map(|s| (*s).to_owned()).collect(),
            };
            for dir in dirs {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if let Ok(data) = std::fs::read(entry.path()) {
                        pem_blobs.push(data);
                    }
                }
            }
            let mut store = RootCertStore::empty();
            let mut ders = Vec::new();
            for blob in &pem_blobs {
                for cert in CertificateDer::pem_slice_iter(blob).flatten() {
                    ders.push(cert.as_ref().to_vec());
                    let _ = store.add(cert);
                }
            }
            (!store.is_empty()).then_some((store, ders))
        })
        .as_ref()
}

/// Just enough DER to read what Go's error messages name from a leaf certificate: its validity
/// period, its SAN DNS names and IP addresses, and its subject Common Name.
mod der {
    use chrono::{DateTime, NaiveDate, Utc};

    pub struct CertInfo {
        pub not_before: DateTime<Utc>,
        pub not_after: DateTime<Utc>,
        pub dns_names: Vec<String>,
        pub ip_addresses: Vec<std::net::IpAddr>,
        pub has_san_extension: bool,
        pub common_name: String,
    }

    /// One TLV: `(tag, contents, rest)`.
    fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = input.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (len, rest) = if first < 0x80 {
            (usize::from(first), rest)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return None;
            }
            let len = rest[..n]
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
            (len, &rest[n..])
        };
        if rest.len() < len {
            return None;
        }
        Some((tag, &rest[..len], &rest[len..]))
    }

    fn time(tag: u8, s: &[u8]) -> Option<DateTime<Utc>> {
        let s = std::str::from_utf8(s).ok()?.strip_suffix('Z')?;
        let (year, rest) = match tag {
            0x17 => {
                let yy: i32 = s.get(..2)?.parse().ok()?;
                (if yy < 50 { 2000 + yy } else { 1900 + yy }, s.get(2..)?)
            }
            0x18 => (s.get(..4)?.parse().ok()?, s.get(4..)?),
            _ => return None,
        };
        let num = |r: std::ops::Range<usize>| rest.get(r)?.parse::<u32>().ok();
        NaiveDate::from_ymd_opt(year, num(0..2)?, num(2..4)?)?
            .and_hms_opt(num(4..6)?, num(6..8)?, num(8..10)?)
            .map(|t| t.and_utc())
    }

    const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
    const OID_CN: &[u8] = &[0x55, 0x04, 0x03];

    impl CertInfo {
        pub fn parse(der: &[u8]) -> Option<Self> {
            let (_, cert, _) = tlv(der)?;
            let (_, tbs, _) = tlv(cert)?;
            let mut rest = tbs;
            let (tag, _, r) = tlv(rest)?;
            if tag == 0xa0 {
                rest = r;
            }
            let (_, _, r) = tlv(rest)?; // serialNumber
            let (_, _, r) = tlv(r)?; // signature
            let (_, _, r) = tlv(r)?; // issuer
            let (_, validity, r) = tlv(r)?;
            let (subject_tag, subject, r) = tlv(r)?;
            let (_, _, mut r) = tlv(r)?; // subjectPublicKeyInfo
            let (t1, nb, v) = tlv(validity)?;
            let (t2, na, _) = tlv(v)?;
            let mut info = CertInfo {
                not_before: time(t1, nb)?,
                not_after: time(t2, na)?,
                dns_names: Vec::new(),
                ip_addresses: Vec::new(),
                has_san_extension: false,
                common_name: String::new(),
            };
            if subject_tag == 0x30 {
                info.common_name = common_name(subject).unwrap_or_default();
            }
            while !r.is_empty() {
                let (tag, contents, next) = tlv(r)?;
                r = next;
                if tag != 0xa3 {
                    continue;
                }
                let (_, mut exts, _) = tlv(contents)?;
                while !exts.is_empty() {
                    let (_, ext, next) = tlv(exts)?;
                    exts = next;
                    let (_, oid, mut e) = tlv(ext)?;
                    if oid != OID_SAN {
                        continue;
                    }
                    info.has_san_extension = true;
                    let (tag, _, next) = tlv(e)?;
                    if tag == 0x01 {
                        e = next; // critical
                    }
                    let (_, octets, _) = tlv(e)?;
                    let (_, mut names, _) = tlv(octets)?;
                    while !names.is_empty() {
                        let (tag, value, next) = tlv(names)?;
                        names = next;
                        match tag {
                            0x82 => info
                                .dns_names
                                .push(String::from_utf8_lossy(value).into_owned()),
                            0x87 => match value.len() {
                                4 => info
                                    .ip_addresses
                                    .push(std::net::IpAddr::from(<[u8; 4]>::try_from(value).ok()?)),
                                16 => info.ip_addresses.push(std::net::IpAddr::from(
                                    <[u8; 16]>::try_from(value).ok()?,
                                )),
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
            Some(info)
        }
    }

    /// The last CN in the subject — Go's `pkix.Name.FillFromRDNSequence` keeps the last.
    fn common_name(mut rdns: &[u8]) -> Option<String> {
        let mut cn = None;
        while !rdns.is_empty() {
            let (_, set, next) = tlv(rdns)?;
            rdns = next;
            let mut atvs = set;
            while !atvs.is_empty() {
                let (_, atv, next) = tlv(atvs)?;
                atvs = next;
                let (_, oid, value) = tlv(atv)?;
                if oid == OID_CN {
                    let (_, s, _) = tlv(value)?;
                    cn = Some(String::from_utf8_lossy(s).into_owned());
                }
            }
        }
        cn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject as _;

    fn sink_cert() -> der::CertInfo {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../../fixtures/behaviour_mail.json")).unwrap();
        let pem = oracle["sink_cert_pem"].as_str().unwrap();
        let cert = CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        der::CertInfo::parse(cert.as_ref()).unwrap()
    }

    #[test]
    fn parses_the_sink_certificate() {
        let c = sink_cert();
        assert_eq!(c.dns_names, ["localhost"]);
        assert_eq!(c.ip_addresses, [std::net::IpAddr::from([127, 0, 0, 1])]);
        assert!(c.has_san_extension);
        assert_eq!(c.common_name, "mmrs-smtp-sink");
        assert_eq!(utc_rfc3339(c.not_before), "2020-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(c.not_after), "2120-01-01T00:00:00Z");
    }

    #[test]
    fn hostname_rules_match_go() {
        let c = sink_cert();
        assert!(verify_hostname(&c, "localhost").is_ok());
        assert!(verify_hostname(&c, "LOCALHOST.").is_ok());
        assert!(verify_hostname(&c, "127.0.0.1").is_ok());
        assert!(verify_hostname(&c, "[127.0.0.1]").is_ok());
        assert!(verify_hostname(&c, "::ffff:127.0.0.1").is_ok());
        let e = |h: &str| verify_hostname(&c, h).unwrap_err().to_string();
        assert_eq!(
            e("wrong.example"),
            "x509: certificate is valid for localhost, not wrong.example"
        );
        assert_eq!(
            e("10.1.2.3"),
            "x509: certificate is valid for 127.0.0.1, not 10.1.2.3"
        );
        assert!(match_hostnames(
            "*.example.com",
            &split_hostname("a.example.com")
        ));
        assert!(!match_hostnames(
            "*.example.com",
            &split_hostname("a.b.example.com")
        ));
        assert!(!match_hostnames(
            "a*.example.com",
            &split_hostname("ab.example.com")
        ));
        assert!(match_exactly("Weird_Name!", "weird_name!"));
        assert!(!valid_hostname("-a.com", false));
        assert!(valid_hostname("a.com.", false));
        assert!(!valid_hostname("*", true));
    }
}
