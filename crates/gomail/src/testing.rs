//! Test apparatus shared with `mm_app::mail`'s tests (feature `testing`): the SMTP sink that
//! replays `reference/dump/behaviour_mail.go`'s scripts, and the transcript normalisation rule.
//! Keep both in step with the Go side — the semantics are documented on `sinkScript` there.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// `sinkScript` (behaviour_mail.go).
#[derive(Debug, Clone, Default)]
pub struct SinkScript {
    pub tls: bool,
    pub greeting: String,
    pub replies: HashMap<String, String>,
    pub auth_replies: Vec<String>,
    pub close_on: String,
}

impl SinkScript {
    fn reply(&self, verb: &str) -> String {
        if let Some(r) = self.replies.get(verb) {
            return r.clone();
        }
        if verb == "DATA_END" {
            return "250 2.0.0 queued\r\n".to_owned();
        }
        "502 5.5.2 not implemented\r\n".to_owned()
    }
}

/// A TLS server config for the sink from the fixture's pinned PEM pair.
pub fn server_tls_config(cert_pem: &str, key_pem: &str) -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes()).expect("fixture cert");
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("fixture key");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("fixture pair"),
    )
}

/// Bind a sink on 127.0.0.1, serve exactly one connection with `script`, and return the port
/// and a handle yielding everything the client sent.
pub async fn spawn_sink(
    script: SinkScript,
    tls: Arc<rustls::ServerConfig>,
) -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        let mut transcript = Vec::new();
        let Ok((tcp, _)) = listener.accept().await else {
            return String::new();
        };
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        if script.tls {
            match acceptor.accept(tcp).await {
                Ok(stream) => {
                    serve(Box::new(stream), &script, &acceptor, &mut transcript).await;
                }
                Err(_) => transcript.extend_from_slice(b"[TLS HANDSHAKE FAILED]"),
            }
        } else {
            serve(Box::new(tcp), &script, &acceptor, &mut transcript).await;
        }
        String::from_utf8_lossy(&transcript).into_owned()
    });
    (port, handle)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Read one line (through `\n`) into `line`; false at end of input.
async fn read_line(io: &mut (dyn Io + '_), buf: &mut Vec<u8>, line: &mut Vec<u8>) -> bool {
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            *line = buf.drain(..=pos).collect();
            return true;
        }
        let mut chunk = [0u8; 4096];
        match io.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                *line = std::mem::take(buf);
                return false;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn serve(
    mut io: Box<dyn Io + '_>,
    script: &SinkScript,
    acceptor: &tokio_rustls::TlsAcceptor,
    transcript: &mut Vec<u8>,
) {
    let mut buf = Vec::new();
    if script.greeting.is_empty() {
        let mut sink = Vec::new();
        let _ = io.read_to_end(&mut sink).await;
        return;
    }
    if io.write_all(script.greeting.as_bytes()).await.is_err() {
        return;
    }
    if script.close_on == "GREETING" {
        return;
    }
    let mut auth_queue: std::collections::VecDeque<String> =
        script.auth_replies.iter().cloned().collect();
    let mut in_auth = false;
    loop {
        let mut line = Vec::new();
        let more = read_line(io.as_mut(), &mut buf, &mut line).await;
        transcript.extend_from_slice(&line);
        if !more {
            return;
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        let mut verb = trimmed.split(' ').next().unwrap_or("").to_uppercase();
        if trimmed == "*" {
            verb = "*".to_owned();
        }
        if !script.close_on.is_empty() && verb == script.close_on {
            return;
        }
        let reply = if verb == "*" {
            in_auth = false;
            script.reply("*")
        } else if in_auth || verb == "AUTH" {
            let r = auth_queue
                .pop_front()
                .unwrap_or_else(|| "535 5.7.8 no more auth replies\r\n".to_owned());
            in_auth = r.starts_with("334");
            r
        } else {
            script.reply(&verb)
        };
        if io.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if verb == "DATA" && reply.starts_with("354") {
            let mut body = Vec::new();
            while !body.ends_with(b"\r\n.\r\n") {
                let mut l = Vec::new();
                let more = read_line(io.as_mut(), &mut buf, &mut l).await;
                body.extend_from_slice(&l);
                if !more {
                    transcript.extend_from_slice(&body);
                    return;
                }
            }
            transcript.extend_from_slice(&body);
            if io
                .write_all(script.reply("DATA_END").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        if verb == "STARTTLS" && reply.starts_with("220") {
            let _ = io.flush().await;
            let stream = PlainOrTls(io);
            match acceptor.accept(stream).await {
                Ok(tls) => {
                    transcript.extend_from_slice(b"[STARTTLS]");
                    io = Box::new(tls);
                    buf.clear();
                }
                Err(_) => {
                    transcript.extend_from_slice(b"[STARTTLS HANDSHAKE FAILED]");
                    return;
                }
            }
        }
    }
}

/// A boxed stream handed to the TLS acceptor as-is.
struct PlainOrTls<'a>(Box<dyn Io + 'a>);

impl AsyncRead for PlainOrTls<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for PlainOrTls<'_> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

/// `normaliseTranscript` (behaviour_mail.go): boundaries → `BOUNDARY<n>` by first appearance,
/// `Date: …` lines → `Date: DATE`, generated message ids → `<RANDOM-UNIX@`.
pub fn normalise_transcript(s: &str) -> String {
    // 1. Leftmost-first runs of exactly 60 lower-case hex digits, as Go's regexp finds them.
    let bytes = s.as_bytes();
    let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let mut seen: Vec<String> = Vec::new();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 60 <= bytes.len() && bytes[i..i + 60].iter().all(|&b| is_hex(b)) {
            let b = &s[i..i + 60];
            let n = match seen.iter().position(|x| x == b) {
                Some(n) => n + 1,
                None => {
                    seen.push(b.to_owned());
                    seen.len()
                }
            };
            out.push_str(&format!("BOUNDARY{n}"));
            i += 60;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    // 2. `(?m)^Date: [^\r\n]*\r$`.
    let mut dated = String::with_capacity(out.len());
    for piece in out.split_inclusive('\n') {
        let line = piece.strip_suffix('\n').unwrap_or(piece);
        if line.starts_with("Date: ")
            && line.ends_with('\r')
            && !line[..line.len() - 1].contains('\r')
        {
            dated.push_str("Date: DATE\r");
            if piece.ends_with('\n') {
                dated.push('\n');
            }
        } else {
            dated.push_str(piece);
        }
    }
    // 3. `<[ybndrfg8ejkmcpqxot1uwisza345h769]{16}-[0-9]+@`.
    const ZBASE32: &str = "ybndrfg8ejkmcpqxot1uwisza345h769";
    let b = dated.as_bytes();
    let mut result = String::with_capacity(dated.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<'
            && i + 17 < b.len()
            && b[i + 1..i + 17]
                .iter()
                .all(|&c| ZBASE32.as_bytes().contains(&c))
            && b[i + 17] == b'-'
        {
            let mut j = i + 18;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 18 && j < b.len() && b[j] == b'@' {
                result.push_str("<RANDOM-UNIX@");
                i = j + 1;
                continue;
            }
        }
        let ch = dated[i..].chars().next().unwrap_or('\u{fffd}');
        result.push(ch);
        i += ch.len_utf8().max(1);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_matches_the_go_rule() {
        let b1 = "a".repeat(60);
        let b2 = "0123456789".repeat(6);
        let input = format!(
            "x {b1}\r\nDate: Mon, 01 Jan 2024 00:00:00 +0000\r\nMessage-ID: <ybndrfg8ejkmcpqx-1700000000@h>\r\n{b2} {b1} {}\r\n",
            "f".repeat(61)
        );
        assert_eq!(
            normalise_transcript(&input),
            "x BOUNDARY1\r\nDate: DATE\r\nMessage-ID: <RANDOM-UNIX@h>\r\nBOUNDARY2 BOUNDARY1 BOUNDARY3f\r\n"
        );
    }
}
