//! The connection under `smtp.Client`: Go's `textproto.Conn` (a `bufio` reader and writer over
//! a `net.Conn` that may be a `tls.Conn`), with the transport errors in Go's `net.OpError` form.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::Error;
use super::net::{DialError, dial_tcp, errno_text};
use super::textproto::{LineSource, take_line};
use super::tls::{self, TlsConfig, TlsError};

/// `net.OpError.Error()` for a failed read or write on an established TCP connection:
/// `read tcp 127.0.0.1:5000->127.0.0.1:25: read: connection reset by peer`.
pub(crate) fn op_error_text(
    op: &str,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    e: &std::io::Error,
) -> String {
    let mut s = format!("{op} tcp");
    if let Some(local) = local {
        s.push(' ');
        s.push_str(&local.to_string());
    }
    if let Some(peer) = peer {
        s.push_str(if local.is_some() { "->" } else { " " });
        s.push_str(&peer.to_string());
    }
    s.push_str(&format!(": {op}: {}", errno_text(e)));
    s
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    /// A `tls.Conn` whose handshake failed: Go returns the handshake error from every later
    /// read and write.
    Failed(TlsError),
    Closed,
}

/// Port of `textproto.Conn` over a TCP or TLS stream.
pub struct Conn {
    stream: Stream,
    rbuf: Vec<u8>,
    eof: bool,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
}

/// Why [`dial_tls`] failed: the TCP dial or the handshake.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DialTlsError {
    #[error(transparent)]
    Dial(#[from] DialError),
    #[error(transparent)]
    Tls(#[from] TlsError),
}

/// `net.Dialer{Timeout: timeout}.Dial("tcp", addr)`.
pub async fn dial(addr: &str, timeout: Option<Duration>) -> Result<Conn, DialError> {
    dial_tcp(addr, timeout).await.map(Conn::plain)
}

/// `tls.DialWithDialer(&net.Dialer{Timeout: timeout}, "tcp", addr, config)`: the timeout covers
/// the dial and the handshake together, and an empty `ServerName` is filled from `addr` (all of
/// it before the last colon, brackets included, as Go does).
pub async fn dial_tls(
    addr: &str,
    timeout: Option<Duration>,
    config: &TlsConfig,
) -> Result<Conn, DialTlsError> {
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let stream = dial_tcp(addr, timeout).await?;
    let mut config = config.clone();
    if config.server_name.is_empty() {
        config.server_name = addr[..addr.rfind(':').unwrap_or(addr.len())].to_owned();
    }
    let local = stream.local_addr().ok();
    let peer = stream.peer_addr().ok();
    let tls = tls::client_handshake(stream, &config, deadline).await?;
    Ok(Conn {
        stream: Stream::Tls(Box::new(tls)),
        rbuf: Vec::new(),
        eof: false,
        local,
        peer,
    })
}

impl Conn {
    /// A connection over an established TCP stream.
    pub fn plain(stream: TcpStream) -> Self {
        let local = stream.local_addr().ok();
        let peer = stream.peer_addr().ok();
        Self {
            stream: Stream::Plain(stream),
            rbuf: Vec::new(),
            eof: false,
            local,
            peer,
        }
    }

    /// Whether the transport is TLS — `conn.(*tls.Conn)`.
    pub fn is_tls(&self) -> bool {
        matches!(self.stream, Stream::Tls(_) | Stream::Failed(_))
    }

    /// `textproto.Conn.Close`.
    pub fn close(&mut self) {
        self.stream = Stream::Closed;
    }

    /// `tls.Client(conn, config)` plus a fresh `textproto.Conn`: bytes the old reader had
    /// buffered are dropped. The handshake runs now rather than on first use; if it fails the
    /// connection is left failed and every later read or write returns the failure, which is
    /// what Go's lazy handshake amounts to.
    pub async fn start_tls(&mut self, config: &TlsConfig) {
        self.rbuf.clear();
        self.eof = false;
        match std::mem::replace(&mut self.stream, Stream::Closed) {
            Stream::Plain(tcp) => {
                self.stream = match tls::client_handshake(tcp, config, None).await {
                    Ok(tls) => Stream::Tls(Box::new(tls)),
                    Err(e) => Stream::Failed(e),
                };
            }
            other => self.stream = other,
        }
    }

    fn io_error(&self, op: &str, e: &std::io::Error, tls: bool) -> Error {
        if tls {
            return match tls::map_io_error(e, self.local, self.peer) {
                TlsError::Eof => Error::Eof,
                other => Error::Tls(other),
            };
        }
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Error::Eof;
        }
        Error::Io(op_error_text(op, self.local, self.peer, e))
    }

    /// `bufio.Writer.Write` + `Flush`.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        let result = match &mut self.stream {
            Stream::Plain(s) => s.write_all(data).await.map(|()| false),
            Stream::Tls(s) => match s.write_all(data).await {
                Ok(()) => s.flush().await.map(|()| true),
                Err(e) => Err(e),
            },
            Stream::Failed(e) => return Err(Error::Tls(e.clone())),
            Stream::Closed => return Err(Error::Closed),
        };
        let tls = self.is_tls();
        result
            .map(|_| ())
            .map_err(|e| self.io_error("write", &e, tls))
    }

    /// `textproto.Reader.ReadLine`.
    pub async fn read_line(&mut self) -> Result<String, Error> {
        loop {
            if let Some(line) = take_line(&mut self.rbuf, self.eof) {
                // A server reply that is not UTF-8 is replaced lossily; Go keeps the bytes (and
                // its `%q` would print them as `\xNN`).
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            if self.eof {
                return Err(Error::Eof);
            }
            let mut chunk = [0u8; 4096];
            let tls = self.is_tls();
            let n = match &mut self.stream {
                Stream::Plain(s) => s.read(&mut chunk).await,
                Stream::Tls(s) => s.read(&mut chunk).await,
                Stream::Failed(e) => return Err(Error::Tls(e.clone())),
                Stream::Closed => return Err(Error::Closed),
            };
            match n {
                Ok(0) => self.eof = true,
                Ok(n) => self.rbuf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    let err = self.io_error("read", &e, tls);
                    if matches!(err, Error::Eof) {
                        self.eof = true;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }
}

impl LineSource for Conn {
    type Error = Error;

    async fn read_line(&mut self) -> Result<String, Error> {
        Conn::read_line(self).await
    }
}
