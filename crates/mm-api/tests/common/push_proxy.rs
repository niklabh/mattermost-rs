//! A push proxy the parity suite listens as, so it can compare what Go and mm-api each send.
//!
//! Both servers of a stack post to `http://localhost:$MMRS_PUSH_PORT` (`scripts/stack-env.sh`
//! turns push on for both and points them here). [`push_proxy`] binds that port for the life of
//! the test binary, records every request — method, path, body — and answers the way Mattermost's
//! push proxy does: `{"status":"OK"}`, or, for a device id containing `remove` or `fail`,
//! `{"status":"REMOVE"}` and `{"status":"FAIL","error":"…"}`. Threads rather than a tokio task,
//! for the reason `smtp_sink` gives.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// One request a server made of the proxy.
#[derive(Debug, Clone)]
pub struct PushRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl PushRequest {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub struct PushProxy {
    requests: Mutex<Vec<PushRequest>>,
}

static PROXY: OnceLock<Option<&'static PushProxy>> = OnceLock::new();

/// The stack's push proxy port, derived like [`super::smtp_sink::smtp_port`].
pub fn push_port() -> u16 {
    if let Some(port) = std::env::var("MMRS_PUSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        return port;
    }
    let offset: u16 = std::env::var("MMRS_PORT_OFFSET")
        .ok()
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    10050 + offset
}

/// The process's proxy, started on first use; `None` when the port is held by someone else.
pub fn push_proxy() -> Option<&'static PushProxy> {
    *PROXY.get_or_init(|| {
        let port = push_port();
        let v4 = TcpListener::bind(("127.0.0.1", port));
        let v6 = TcpListener::bind(("::1", port));
        if v4.is_err() && v6.is_err() {
            eprintln!("push proxy: could not bind port {port}: {v4:?}");
            return None;
        }
        let proxy: &'static PushProxy = Box::leak(Box::new(PushProxy {
            requests: Mutex::new(Vec::new()),
        }));
        for listener in [v4, v6].into_iter().flatten() {
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || {
                        let _ = serve(proxy, stream);
                    });
                }
            });
        }
        Some(proxy)
    })
}

fn serve(proxy: &'static PushProxy, stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut headers = Vec::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim().to_owned();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().unwrap_or(0);
                }
                headers.push((name.to_owned(), value));
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let device = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["device_id"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let answer = if device.contains("remove") {
            r#"{"status":"REMOVE"}"#
        } else if device.contains("fail") {
            r#"{"status":"FAIL","error":"the proxy refused it"}"#
        } else {
            r#"{"status":"OK"}"#
        };
        if let Ok(mut requests) = proxy.requests.lock() {
            requests.push(PushRequest {
                method,
                path,
                headers,
                body,
            });
        }
        write!(
            writer,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{answer}",
            answer.len()
        )?;
    }
}

impl PushProxy {
    /// Remove and return the first request `matches` accepts, waiting up to `timeout`.
    pub async fn take(
        &self,
        timeout: Duration,
        matches: impl Fn(&PushRequest) -> bool,
    ) -> Option<PushRequest> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(mut requests) = self.requests.lock() {
                if let Some(index) = requests.iter().position(&matches) {
                    return Some(requests.remove(index));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drop every request `matches` accepts.
    pub fn discard(&self, matches: impl Fn(&PushRequest) -> bool) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.retain(|r| !matches(r));
        }
    }
}

/// A push body with the per-send random parts — `ack_id` and the ES256 `signature` over it —
/// replaced, after checking the signature is a three-part JWT whose claims name that ack id and
/// device.
pub fn normalize_push(mut body: serde_json::Value) -> serde_json::Value {
    use base64::Engine as _;
    let ack = body["ack_id"].as_str().unwrap_or_default().to_owned();
    let device = body["device_id"].as_str().unwrap_or_default().to_owned();
    if let Some(signature) = body["signature"].as_str().filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = signature.split('.').collect();
        assert_eq!(parts.len(), 3, "the signature is a JWT");
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.decode(parts[0]).expect("the header is base64url");
        assert_eq!(header, br#"{"alg":"ES256","typ":"JWT"}"#);
        let claims: serde_json::Value =
            serde_json::from_slice(&engine.decode(parts[1]).expect("base64url")).expect("JSON");
        assert_eq!(claims["ack_id"], ack.as_str());
        assert_eq!(claims["device_id"], device.as_str());
        assert_eq!(
            engine.decode(parts[2]).expect("base64url").len(),
            64,
            "an ES256 signature is r || s"
        );
        body["signature"] = serde_json::Value::from("<jwt>");
    }
    if !ack.is_empty() {
        body["ack_id"] = serde_json::Value::from("<ack>");
    }
    body
}
