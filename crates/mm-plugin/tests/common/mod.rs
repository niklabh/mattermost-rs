//! Shared by the mm-plugin suites: fixtures, the canonical rendering, and the Go oracle.

#![allow(dead_code)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{Map, Value as Json};

mod render;
pub use render::*;

/// `reference/dump/plugingen`, built once per test process.
pub fn plugingen() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("plugingen");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./plugingen")
            .current_dir(root().join("reference/dump"))
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/plugingen failed");
        out
    })
}

pub fn run_plugingen(args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let out = Command::new(plugingen())
        .args(args)
        // plugingen reads the Go tree relative to reference/dump.
        .current_dir(root().join("reference/dump"))
        .env("TZ", "Asia/Kolkata")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plugingen {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The gob oracle, generated once per test binary: two streams per wire struct and
/// `expected.json`, all written by `plugingen gob` from the pinned Go tree.
///
/// It is not committed. Every stream is a deterministic function of the IDL, which is committed
/// and checked against the Go tree, so the bytes would add nothing a reviewer could read — and
/// every suite that reads them builds the Go oracle anyway.
pub fn oracle_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // One directory per suite, reused across rebuilds: cargo's binary name ends in a hash that
        // changes with every build, which would leave a 6 MB copy behind each time.
        let binary = std::env::current_exe().unwrap();
        let stem = binary.file_stem().unwrap().to_string_lossy().into_owned();
        let suite = stem
            .rsplit_once('-')
            .map_or(stem.as_str(), |(suite, _)| suite);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("plugin-gob-{suite}"));
        let _ = std::fs::remove_dir_all(&dir);
        run_plugingen(&["gob".as_ref(), dir.as_os_str()]);
        dir
    })
    .clone()
}

pub fn expected() -> Map<String, Json> {
    let text = std::fs::read_to_string(oracle_dir().join("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

pub fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// The client half of the hijack scenario (plugingen/hijack.go, `HijackClient`): send the request
/// with a line after it, answer the `timeout:` line with `raw\n`, and read until the close.
pub async fn hijack_client(addr: std::net::SocketAddr) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {HIJACK_URL} HTTP/1.1\r\nHost: example.test\r\n\r\nping\n");
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut buf = [0; 4096];
    let mut answered = false;
    loop {
        let n = conn.read(&mut buf).await.unwrap();
        if n == 0 {
            return String::from_utf8(received).unwrap();
        }
        received.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&received);
        if !answered
            && text
                .find("timeout: ")
                .is_some_and(|i| text[i..].contains('\n'))
        {
            conn.write_all(b"raw\n").await.unwrap();
            answered = true;
        }
    }
}

/// A host's writer over a real connection, as Go's server hands one to a handler: its hijack
/// answers the stream and the bytes read past the request head.
pub struct TcpWriter {
    conn: Option<(tokio::net::TcpStream, Vec<u8>)>,
}

impl mm_plugin::http::ResponseWriter for TcpWriter {
    fn header(&mut self) -> mm_plugin::wire::http::Header {
        Default::default()
    }
    fn sync_header(&mut self, _: mm_plugin::wire::http::Header) {}
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "the hijack scenario writes nothing unhijacked",
        ))
    }
    fn write_header(&mut self, _: i64) {}
    fn hijack(&mut self) -> Option<std::io::Result<mm_plugin::hijack::Hijacked>> {
        let (conn, buffered) = self.conn.take()?;
        Some(Ok(mm_plugin::hijack::Hijacked {
            conn: Box::new(conn),
            buffered,
        }))
    }
}

/// The host half: serve the hijack request through a recorder, then through a real connection.
/// Answers what the recorder got and every byte the client received.
pub async fn serve_hijack(hooks: &mm_plugin::rpc::HooksClient) -> (Json, String) {
    use tokio::io::AsyncBufReadExt as _;

    let recorder = Recorder::default();
    hooks
        .serve_http(
            None,
            Some(Box::new(hijack_request())),
            None::<std::io::Cursor<Vec<u8>>>,
            recorder.clone(),
        )
        .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::spawn(hijack_client(listener.local_addr().unwrap()));
    let (conn, _) = listener.accept().await.unwrap();
    // Read the head as a server does, through a buffer that may hold the line after it.
    let mut reader = tokio::io::BufReader::new(conn);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let buffered = reader.buffer().to_vec();
    let writer = TcpWriter {
        conn: Some((reader.into_inner(), buffered)),
    };
    hooks
        .serve_http(
            None,
            Some(Box::new(hijack_request())),
            None::<std::io::Cursor<Vec<u8>>>,
            writer,
        )
        .await;
    (recorder.response(), client.await.unwrap())
}
