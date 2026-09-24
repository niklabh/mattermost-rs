//! An SMTP server the parity suite listens with, so it can compare what Go and mm-api each send.
//!
//! Both servers of a stack send to `localhost:$MMRS_SMTP_PORT` (`scripts/stack-env.sh` sets the
//! port on each as `MM_EMAILSETTINGS_SMTPPORT`). Nothing listens there between runs, so outside
//! this suite a send fails exactly as it did before the port existed. [`smtp_sink`] binds it for
//! the life of the test binary and records every session: the client's commands verbatim and the
//! un-dot-stuffed `DATA`.
//!
//! # Why threads and not a tokio task
//!
//! Every `#[tokio::test]` builds and drops its own runtime, and a task spawned on one dies with
//! it — the sink would stop listening the moment the first test that started it returned. The
//! listener runs on plain OS threads instead, and a test waits for a message by polling.
//!
//! # Both loopback families
//!
//! `SMTPServer` is the document's `localhost`, which both servers resolve to `::1` and
//! `127.0.0.1`. Binding both means neither client's address order decides whether it connects.
//!
//! # Telling the two servers' mail apart
//!
//! By recipient. A test sends through one server, [`Sink::take`]s the message addressed to its
//! own fixture user, then does the same through the other. Recipients are unique per test, so
//! concurrent tests never take each other's mail.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// One SMTP session as the client drove it.
#[derive(Debug, Clone, Default)]
pub struct Captured {
    /// Every command line the client sent, without its CRLF, in order — `EHLO localhost`,
    /// `MAIL FROM:<…> BODY=8BITMIME`, `RCPT TO:<…>`, `DATA`, `QUIT`.
    pub commands: Vec<String>,
    /// The `RCPT TO` addresses, angle brackets stripped.
    pub recipients: Vec<String>,
    /// The message as transmitted, dot-stuffing undone, without the terminating `.` line.
    pub data: Vec<u8>,
}

impl Captured {
    pub fn data_str(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

pub struct Sink {
    messages: Mutex<Vec<Captured>>,
}

const GREETING: &[u8] = b"220 mmrs-parity-sink ESMTP\r\n";
const EHLO_REPLY: &[u8] = b"250-mmrs-parity-sink\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 HELP\r\n";

static SINK: OnceLock<Option<&'static Sink>> = OnceLock::new();

/// The stack's SMTP port: `MMRS_SMTP_PORT`, or Mattermost's default shifted like every other
/// stack port when only `MMRS_PORT_OFFSET` is set.
pub fn smtp_port() -> u16 {
    if let Some(port) = std::env::var("MMRS_SMTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        return port;
    }
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    10025 + offset
}

/// The process's sink, started on first use. `None` when the port could not be bound — another
/// process holds it — which a test on a live stack should treat as a harness failure.
pub fn smtp_sink() -> Option<&'static Sink> {
    *SINK.get_or_init(|| {
        let port = smtp_port();
        let v4 = TcpListener::bind(("127.0.0.1", port));
        let v6 = TcpListener::bind(("::1", port));
        if v4.is_err() && v6.is_err() {
            eprintln!("smtp sink: could not bind port {port}: {v4:?}");
            return None;
        }
        let sink: &'static Sink = Box::leak(Box::new(Sink {
            messages: Mutex::new(Vec::new()),
        }));
        for listener in [v4, v6].into_iter().flatten() {
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || {
                        let _ = serve(sink, stream);
                    });
                }
            });
        }
        Some(sink)
    })
}

fn serve(sink: &'static Sink, stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    writer.write_all(GREETING)?;
    let mut session = Captured::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let command = String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        session.commands.push(command.clone());
        let verb = command
            .split([' ', ':'])
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match verb.as_str() {
            "EHLO" => writer.write_all(EHLO_REPLY)?,
            "HELO" | "MAIL" | "RSET" | "NOOP" => writer.write_all(b"250 OK\r\n")?,
            "RCPT" => {
                let address = command
                    .split_once(':')
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or_default();
                let address = address
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                session.recipients.push(address.to_owned());
                writer.write_all(b"250 OK\r\n")?;
            }
            "DATA" => {
                writer.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")?;
                let mut data = Vec::new();
                loop {
                    line.clear();
                    if reader.read_until(b'\n', &mut line)? == 0 {
                        return Ok(());
                    }
                    if line == b".\r\n" || line == b".\n" {
                        break;
                    }
                    let unstuffed = if line.first() == Some(&b'.') {
                        &line[1..]
                    } else {
                        &line[..]
                    };
                    data.extend_from_slice(unstuffed);
                }
                session.data = data;
                writer.write_all(b"250 OK: queued\r\n")?;
                // One message per session is all either server sends; record it now rather than at
                // QUIT, so a client that drops the connection without quitting is still heard.
                if let Ok(mut messages) = sink.messages.lock() {
                    messages.push(session.clone());
                }
            }
            "QUIT" => {
                writer.write_all(b"221 Bye\r\n")?;
                break;
            }
            _ => writer.write_all(b"502 Command not implemented\r\n")?,
        }
    }
    Ok(())
}

impl Sink {
    /// Remove and return the first message addressed to `recipient`, waiting up to `timeout`.
    pub async fn take(&self, recipient: &str, timeout: Duration) -> Option<Captured> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(mut messages) = self.messages.lock() {
                if let Some(index) = messages
                    .iter()
                    .position(|m| m.recipients.iter().any(|r| r == recipient))
                {
                    return Some(messages.remove(index));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drop every message addressed to `recipient` — for a test that must start clean.
    pub fn discard(&self, recipient: &str) {
        if let Ok(mut messages) = self.messages.lock() {
            messages.retain(|m| !m.recipients.iter().any(|r| r == recipient));
        }
    }
}

/// Replace the parts of a message that are random or clock-dependent on **both** servers: the
/// `Date` and `Message-ID` header values and every multipart boundary. Everything else must match
/// byte for byte.
pub fn normalize_message(data: &str) -> String {
    let mut out = data.to_owned();
    let mut boundaries: Vec<String> = Vec::new();
    for line in data.split("\r\n") {
        if let Some(rest) = line.trim_start().strip_prefix("boundary=") {
            boundaries.push(rest.trim_matches('"').to_owned());
        }
    }
    for (i, boundary) in boundaries.iter().enumerate() {
        out = out.replace(boundary.as_str(), &format!("BOUNDARY-{i}"));
    }
    let mut lines: Vec<String> = Vec::new();
    for line in out.split("\r\n") {
        if line.starts_with("Date: ") {
            lines.push("Date: <date>".to_owned());
        } else if line.starts_with("Message-ID: ") || line.starts_with("Message-Id: ") {
            lines.push("Message-ID: <message-id>".to_owned());
        } else {
            lines.push(line.to_owned());
        }
    }
    lines.join("\r\n")
}
