//! goplugin's yamux against hashicorp/yamux v0.1.2 (`reference/dump/yamux`), both roles and
//! both sides of the connection.
//!
//! The Go driver's transcript against the Go acceptor is the reference: 300 concurrent echo
//! streams, 8 and 16 MiB transfers, half-closes in both orders, pings, streams opened by the
//! acceptor, an idle period across the keepalive, 300 streams opened while the acceptor is not
//! accepting (more than its backlog), and GoAway. Every pairing below reproduces it.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use goplugin::yamux::{Config, Session, Stream};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(90), f)
        .await
        .expect("test timed out")
}

fn oracle() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("yamux-oracle");
        let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/dump");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./yamux")
            .current_dir(&dump)
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/yamux failed");
        out
    })
}

fn config() -> Config {
    Config {
        keepalive_interval: Duration::from_millis(250),
        ..Config::default()
    }
}

fn pattern(i: usize) -> u8 {
    ((i * 31 + 7) % 251) as u8
}

/// A Go process; it exits when its stdin closes.
struct Go {
    child: Child,
}

impl Go {
    fn spawn(args: &[&str]) -> Self {
        let child = Command::new(oracle())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child }
    }

    fn first_line(&mut self) -> String {
        let mut line = String::new();
        let stdout = self.child.stdout.as_mut().unwrap();
        let mut r = BufReader::new(stdout);
        r.read_line(&mut line).unwrap();
        line.trim().to_owned()
    }

    fn transcript(mut self) -> Json {
        let mut out = String::new();
        std::io::Read::read_to_string(self.child.stdout.as_mut().unwrap(), &mut out).unwrap();
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("bad transcript ({e}): {out}"))
    }
}

impl Drop for Go {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reference() -> Json {
    Go::spawn(&["selftest"]).transcript()
}

// ─── the acceptor ──────────────────────────────────────────────────────────────────────────

async fn read_line(s: &mut Stream) -> Option<String> {
    let mut line = Vec::new();
    let mut one = [0u8; 1];
    loop {
        if s.read(&mut one).await.ok()? == 0 {
            return None;
        }
        if one[0] == b'\n' {
            return String::from_utf8(line).ok();
        }
        line.push(one[0]);
    }
}

async fn acceptor(sess: Session) {
    let last_count = Arc::new(AtomicI64::new(0));
    loop {
        let Ok(mut s) = sess.accept().await else {
            return;
        };
        let Some(line) = read_line(&mut s).await else {
            continue;
        };
        let fields: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        if fields.first().map(String::as_str) == Some("pause") {
            let ms: u64 = fields[1].parse().unwrap();
            let _ = s.write_all(b"paused\n").await;
            let _ = s.shutdown().await;
            drop(s);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            continue;
        }
        tokio::spawn(handle(sess.clone(), s, fields, last_count.clone()));
    }
}

async fn handle(sess: Session, mut s: Stream, cmd: Vec<String>, last_count: Arc<AtomicI64>) {
    let arg: usize = cmd.get(1).and_then(|a| a.parse().ok()).unwrap_or(0);
    match cmd.first().map(String::as_str) {
        Some("echo") => {
            let (mut r, mut w) = tokio::io::split(s);
            let _ = tokio::io::copy(&mut r, &mut w).await;
            let _ = w.shutdown().await;
            return;
        }
        Some("sink") => {
            let mut buf = vec![0u8; 32 * 1024];
            let (mut count, mut sum) = (0usize, 0usize);
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        count += n;
                        sum += buf[..n].iter().map(|&b| usize::from(b)).sum::<usize>();
                    }
                }
            }
            let _ = s.write_all(format!("{count} {sum}\n").as_bytes()).await;
        }
        Some("send") => {
            let out: Vec<u8> = (0..arg).map(pattern).collect();
            let _ = s.write_all(&out).await;
        }
        Some("closefirst") => {
            let _ = s.shutdown().await;
            let mut sink = Vec::new();
            let n = s.read_to_end(&mut sink).await.unwrap_or(0);
            last_count.store(n as i64, Ordering::SeqCst);
        }
        Some("lastcount") => {
            let _ = s
                .write_all(format!("{}\n", last_count.load(Ordering::SeqCst)).as_bytes())
                .await;
        }
        Some("ping") => {
            let line = match sess.ping().await {
                Ok(_) => "pong\n".to_owned(),
                Err(e) => format!("err {e}\n"),
            };
            let _ = s.write_all(line.as_bytes()).await;
        }
        Some("open") => {
            let tasks: Vec<_> = (0..arg)
                .map(|i| {
                    let sess = sess.clone();
                    tokio::spawn(async move {
                        let mut o = sess.open().await?;
                        o.write_all(format!("hello {i}\n").as_bytes())
                            .await
                            .map_err(|_| goplugin::yamux::Error::StreamClosed)?;
                        o.shutdown()
                            .await
                            .map_err(|_| goplugin::yamux::Error::StreamClosed)?;
                        Ok::<_, goplugin::yamux::Error>(())
                    })
                })
                .collect();
            let (mut ok, mut errs) = (0, 0);
            for t in tasks {
                match t.await.unwrap() {
                    Ok(()) => ok += 1,
                    Err(_) => errs += 1,
                }
            }
            let _ = s
                .write_all(format!("opened {ok} {errs}\n").as_bytes())
                .await;
        }
        Some("goaway") => {
            let _ = sess.go_away();
            let _ = s.write_all(b"ok\n").await;
        }
        Some(other) => {
            let _ = s.write_all(format!("unknown {other}\n").as_bytes()).await;
        }
        None => {}
    }
    let _ = s.shutdown().await;
}

// ─── the driver ────────────────────────────────────────────────────────────────────────────

async fn command(sess: &Session, line: &str) -> Result<Stream, String> {
    let mut s = sess.open().await.map_err(|e| e.to_string())?;
    s.write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    Ok(s)
}

async fn echo_many(sess: &Session, streams: usize, size: usize) -> String {
    let tasks: Vec<_> = (0..streams)
        .map(|i| {
            let sess = sess.clone();
            tokio::spawn(async move {
                let Ok(s) = command(&sess, "echo").await else {
                    return false;
                };
                let want: Vec<u8> = (0..size).map(|j| ((j + i) % 256) as u8).collect();
                // Read and write from different tasks: the halves must not starve each other.
                let (mut r, mut w) = tokio::io::split(s);
                let to_write = want.clone();
                let writer = tokio::spawn(async move {
                    let _ = w.write_all(&to_write).await;
                    let _ = w.shutdown().await;
                });
                let mut got = Vec::new();
                let ok = r.read_to_end(&mut got).await.is_ok() && got == want;
                let _ = writer.await;
                ok
            })
        })
        .collect();
    let mut bad = 0;
    for t in tasks {
        if !t.await.unwrap() {
            bad += 1;
        }
    }
    format!("{streams} streams, {bad} failed")
}

async fn reply(sess: &Session, line: &str) -> String {
    let mut s = match command(sess, line).await {
        Ok(s) => s,
        Err(e) => return format!("open: {e}"),
    };
    let mut got = Vec::new();
    if let Err(e) = s.read_to_end(&mut got).await {
        return format!("read: {e}");
    }
    let _ = s.shutdown().await;
    String::from_utf8_lossy(&got).trim().to_owned()
}

async fn drive(sess: Session) -> Json {
    let mut out = Vec::new();
    let mut add = |step: &str, result: String| out.push(json!({ "step": step, "result": result }));

    add("echo 300x64KiB", echo_many(&sess, 300, 64 << 10).await);

    let r = match command(&sess, "send 8388608").await {
        Err(e) => format!("open: {e}"),
        Ok(mut s) => {
            let mut got = Vec::new();
            let err = s.read_to_end(&mut got).await.err();
            let bad = got
                .iter()
                .enumerate()
                .filter(|(i, b)| **b != pattern(*i))
                .count();
            let err = err.map_or("<nil>".to_owned(), |e| e.to_string());
            format!("{} bytes, {bad} wrong, err={err}", got.len())
        }
    };
    add("send 8MiB", r);

    let r = match command(&sess, "sink").await {
        Err(e) => format!("open: {e}"),
        Ok(mut s) => {
            let data: Vec<u8> = (0..16usize << 20).map(pattern).collect();
            let want: usize = data.iter().map(|&b| usize::from(b)).sum();
            let _ = s.write_all(&data).await;
            let _ = s.shutdown().await;
            let mut got = Vec::new();
            let _ = s.read_to_end(&mut got).await;
            format!(
                "{} (want {} {want})",
                String::from_utf8_lossy(&got).trim(),
                data.len()
            )
        }
    };
    add("sink 16MiB", r);

    let r = match command(&sess, "closefirst").await {
        Err(e) => format!("open: {e}"),
        Ok(mut s) => {
            let mut sink = Vec::new();
            let n = s.read_to_end(&mut sink).await.unwrap_or(0);
            let werr = s
                .write_all(b"abc")
                .await
                .err()
                .map_or("<nil>".to_owned(), |e| e.to_string());
            let _ = s.shutdown().await;
            drop(s);
            tokio::time::sleep(Duration::from_millis(100)).await;
            format!(
                "read {n}, write err={werr}, peer read {}",
                reply(&sess, "lastcount").await
            )
        }
    };
    add("closefirst", r);

    add("ping", reply(&sess, "ping").await);

    let accepting = {
        let sess = sess.clone();
        tokio::spawn(async move {
            let mut hellos = 0;
            for _ in 0..20 {
                let Ok(mut s) = sess.accept().await else {
                    break;
                };
                let mut line = Vec::new();
                let _ = s.read_to_end(&mut line).await;
                if line.starts_with(b"hello ") {
                    hellos += 1;
                }
            }
            hellos
        })
    };
    let r = reply(&sess, "open 20").await;
    let hellos = accepting.await.unwrap();
    add("open 20", format!("{r}, {hellos} hellos"));

    tokio::time::sleep(Duration::from_secs(1)).await;
    add("after 1s idle", echo_many(&sess, 1, 10).await);

    add("pause", reply(&sess, "pause 500").await);
    add("echo 300 while paused", echo_many(&sess, 300, 16).await);

    add("goaway", reply(&sess, "goaway").await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = match sess.open().await {
        Ok(_) => "opened".to_owned(),
        Err(e) => e.to_string(),
    };
    add("open after goaway", r);
    Json::Array(out)
}

// ─── the pairings ──────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rust_client_drives_go_server() {
    within(async {
        let want = reference();
        let mut go = Go::spawn(&["listen"]);
        let addr = go.first_line();
        let sess = Session::client(TcpStream::connect(&addr).await.unwrap(), config()).unwrap();
        assert_eq!(drive(sess).await, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rust_server_drives_go_client() {
    within(async {
        let want = reference();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let _go = Go::spawn(&["dial", &addr]);
        let (conn, _) = listener.accept().await.unwrap();
        let sess = Session::server(conn, config()).unwrap();
        assert_eq!(drive(sess).await, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn go_server_drives_rust_client() {
    within(async {
        let want = reference();
        let mut go = Go::spawn(&["listen-drive"]);
        let addr = go.first_line();
        let sess = Session::client(TcpStream::connect(&addr).await.unwrap(), config()).unwrap();
        tokio::spawn(acceptor(sess));
        let got = tokio::task::spawn_blocking(move || go.transcript())
            .await
            .unwrap();
        assert_eq!(got, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn go_client_drives_rust_server() {
    within(async {
        let want = reference();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let go = Go::spawn(&["dial-drive", &addr]);
        let (conn, _) = listener.accept().await.unwrap();
        let sess = Session::server(conn, config()).unwrap();
        tokio::spawn(acceptor(sess));
        let got = tokio::task::spawn_blocking(move || go.transcript())
            .await
            .unwrap();
        assert_eq!(got, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rust_drives_rust() {
    within(async {
        let want = reference();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            acceptor(Session::server(conn, config()).unwrap()).await;
        });
        let sess = Session::client(TcpStream::connect(addr).await.unwrap(), config()).unwrap();
        assert_eq!(drive(sess).await, want);
        server.abort();
    })
    .await
}
