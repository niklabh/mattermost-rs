//! goplugin's host and plugin against HashiCorp go-plugin v1.8.0 (`reference/dump/goplugin`).
//!
//! The Go host's transcript against the Go plugin is the reference. Every other pairing — Rust
//! host with the Go plugin, Go host with the Rust plugin (`examples/kv_plugin.rs`), Rust with
//! Rust — must reproduce it: dispensing, calls and their errors, a server the host offers on the
//! broker, a server the plugin offers, the plugin's stdio streams, its hclog stderr, ping,
//! reattaching, killing, and the checksum, cookie and version refusals.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use go_netrpc::{Server, ServiceError};
use gobwire::Gob;
use goplugin::rpc::{Empty, RpcError};
use goplugin::{Client, ClientConfig, ClientError, HandshakeConfig, PluginCommand};
use serde_json::{Value as Json, json};

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("test timed out")
}

fn go_oracle() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("goplugin-oracle");
        let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/dump");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./goplugin")
            .current_dir(&dump)
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/goplugin failed");
        out
    })
}

/// `examples/kv_plugin`, which `cargo test` builds next to this test's own binary.
fn rust_plugin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
    let path = profile_dir.join("examples").join("kv_plugin");
    assert!(
        path.exists(),
        "{} missing: run through `cargo test`, which builds examples",
        path.display()
    );
    path
}

fn handshake() -> HandshakeConfig {
    HandshakeConfig {
        protocol_version: 1,
        magic_cookie_key: "GOPLUGIN_ORACLE".into(),
        magic_cookie_value: "hello".into(),
    }
}

/// An `AsyncWrite` into a shared buffer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for Captured {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[derive(Gob, Debug, Default)]
struct PutArgs {
    #[gob(name = "Key")]
    key: String,
    #[gob(name = "Value")]
    value: String,
}

#[derive(Gob, Debug, Default)]
struct AddArgs {
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

#[derive(Gob, Debug, Default)]
struct AddViaArgs {
    #[gob(name = "BrokerID")]
    broker_id: u32,
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

/// The Go oracle's `errText`.
fn err_text<T>(
    r: &Result<T, impl Into<Box<dyn std::error::Error>> + std::fmt::Display + ErrKind>,
) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => match e.server_message() {
            Some(m) => format!("server: {m}"),
            None => format!("error: {e}"),
        },
    }
}

trait ErrKind {
    fn server_message(&self) -> Option<String>;
}

impl ErrKind for go_netrpc::Error {
    fn server_message(&self) -> Option<String> {
        match self {
            go_netrpc::Error::Server(m) => Some(m.clone()),
            _ => None,
        }
    }
}

impl ErrKind for RpcError {
    fn server_message(&self) -> Option<String> {
        match self {
            RpcError::NetRpc(e) => e.server_message(),
            _ => None,
        }
    }
}

impl ErrKind for ClientError {
    fn server_message(&self) -> Option<String> {
        match self {
            ClientError::Rpc(e) => e.server_message(),
            _ => None,
        }
    }
}

fn launch_config(argv: &[PathBuf]) -> ClientConfig {
    let mut cmd = PluginCommand::new(&argv[0]);
    for a in &argv[1..] {
        cmd = cmd.arg(a);
    }
    let mut config = ClientConfig::new(handshake());
    config.cmd = Some(cmd);
    config
}

/// The Go oracle's `host`, step for step.
async fn rust_host(argv: &[PathBuf]) -> Json {
    let mut out = Vec::new();
    let mut add = |step: &str, result: String| out.push(json!({ "step": step, "result": result }));

    let (stdout, stderr, raw) = (
        Captured::default(),
        Captured::default(),
        Captured::default(),
    );
    let mut config = launch_config(argv);
    config.checksum = Some(goplugin::client::sha256_file(&argv[0]).unwrap());
    config.sync_stdout = Box::new(stdout.clone());
    config.sync_stderr = Box::new(stderr.clone());
    config.stderr = Box::new(raw.clone());
    let client = match Client::start(config).await {
        Ok(c) => c,
        Err(e) => {
            add("start", err_text::<()>(&Err(e)));
            return Json::Array(out);
        }
    };
    add(
        "start",
        format!(
            "protocol={} version={}",
            client.protocol(),
            client.negotiated_version()
        ),
    );

    let r = client.dispense("nope").await;
    add("dispense unknown", err_text(&r));

    let kv = match client.dispense("kv").await {
        Ok(kv) => kv,
        Err(e) => {
            add("dispense kv", err_text::<()>(&Err(e)));
            return Json::Array(out);
        }
    };
    add("dispense kv", "ok".into());

    let r = kv
        .client
        .call::<_, Empty>(
            "Plugin.Put",
            &PutArgs {
                key: "a".into(),
                value: "1".into(),
            },
        )
        .await;
    add("put a=1", err_text(&r));
    let r = kv.client.call::<_, String>("Plugin.Get", "a").await;
    let got = r.as_ref().cloned().unwrap_or_default();
    add("get a", format!("{} {:?}", err_text(&r), got));
    let r = kv.client.call::<_, String>("Plugin.Get", "zz").await;
    add("get zz", err_text(&r));

    let id = kv.broker.next_id();
    let mut adder = Server::new();
    adder.register("Plugin.Add", |a: AddArgs| async move {
        Ok::<_, ServiceError>(a.a + a.b)
    });
    tokio::spawn({
        let broker = kv.broker.clone();
        async move { broker.accept_and_serve(id, Arc::new(adder)).await }
    });
    let r = kv
        .client
        .call::<_, i64>(
            "Plugin.AddVia",
            &AddViaArgs {
                broker_id: id,
                a: 2,
                b: 40,
            },
        )
        .await;
    let sum = r.as_ref().copied().unwrap_or(0);
    add("add via host broker", format!("{} {sum}", err_text(&r)));

    let r = kv.client.call::<_, u32>("Plugin.Offer", &7i64).await;
    let result = match r {
        Err(e) => err_text::<()>(&Err(e)),
        Ok(offered) => match kv.broker.dial(offered).await {
            Err(e) => format!("error: {e}"),
            Ok(stream) => {
                let c = go_netrpc::Client::new(stream);
                let r = c.call::<_, i64>("Plugin.Mul", &6i64).await;
                let _ = c.close().await;
                let product = r.as_ref().copied().unwrap_or(0);
                format!("{} {product}", err_text(&r))
            }
        },
    };
    add("mul via plugin broker", result);

    let r = kv
        .client
        .call::<_, Empty>("Plugin.Print", "hello-stdio")
        .await;
    add("print", err_text(&r));
    let r = kv
        .client
        .call::<_, Empty>("Plugin.Log", "logged-line")
        .await;
    add("log", err_text(&r));
    tokio::time::sleep(Duration::from_millis(300)).await;
    add("stdout stream", stdout.text().trim().to_owned());
    add("stderr stream", stderr.text().trim().to_owned());
    let mut logged = String::new();
    for line in raw.text().lines() {
        if let Ok(Json::Object(entry)) = serde_json::from_str::<Json>(line)
            && entry.get("@message") == Some(&json!("logged-line"))
        {
            let s = |k: &str| {
                entry
                    .get(k)
                    .and_then(Json::as_str)
                    .unwrap_or("<nil>")
                    .to_owned()
            };
            logged = format!("{} {} {}", s("@level"), s("@module"), s("key"));
        }
    }
    add("stderr log line", logged);

    let r = client.ping().await;
    add("ping", err_text(&r));

    let mut again = ClientConfig::new(handshake());
    again.reattach = Some(client.reattach_config());
    // Kept open, as the Go host keeps its reattached client, until the kill.
    let mut _reattached_client = None;
    let reattached = match Client::start(again).await {
        Err(e) => err_text::<()>(&Err(e)),
        Ok(c2) => {
            let r = match c2.dispense("kv").await {
                Err(e) => err_text::<()>(&Err(e)),
                Ok(kv2) => err_text(&kv2.client.call::<_, String>("Plugin.Get", "a").await),
            };
            _reattached_client = Some(c2);
            r
        }
    };
    add("reattach", reattached);

    let socket = client.reattach_config().addr;
    let started = std::time::Instant::now();
    client.kill().await;
    add("kill", format!("exited={}", client.exited()));
    assert!(
        !client.force_killed(),
        "a plugin that honours Control.Quit is not killed"
    );
    // Not in the transcript (Go's timing is its own): a plugin that honours Control.Quit exits
    // well inside the two seconds after which the host kills it, and removes its socket.
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "graceful quit took {:?}",
        started.elapsed()
    );
    if let goplugin::PluginAddr::Unix(path) = &socket {
        assert!(
            !path.exists(),
            "the plugin left its socket behind: {}",
            path.display()
        );
    }
    // cmdrunner.ReattachFunc: a process that is gone cannot be reattached to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut dead = ClientConfig::new(handshake());
    dead.reattach = Some(client.reattach_config());
    assert!(matches!(
        Client::start(dead).await,
        Err(ClientError::ProcessNotFound)
    ));

    let mut bad = launch_config(argv);
    bad.checksum = Some(vec![0; 32]);
    add(
        "checksum mismatch",
        err_text(&Client::start(bad).await.map(|_| ())),
    );

    let mut wrong = launch_config(argv);
    wrong.handshake.magic_cookie_value = "wrong".into();
    wrong.start_timeout = Duration::from_secs(5);
    add(
        "wrong cookie",
        format!("failed={}", Client::start(wrong).await.is_err()),
    );

    let mut versioned = launch_config(argv);
    versioned.handshake.protocol_version = 2;
    versioned.protocol_versions = vec![2];
    add(
        "version mismatch",
        err_text(&Client::start(versioned).await.map(|_| ())),
    );

    Json::Array(out)
}

fn go_host(plugin: &[PathBuf]) -> Json {
    let out = Command::new(go_oracle())
        .arg("host")
        .args(plugin)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad transcript ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn reference() -> Json {
    let r = go_host(&[go_oracle().clone(), "plugin".into()]);
    // A transcript that stopped early would make every comparison trivially equal.
    let steps = r.as_array().map_or(0, Vec::len);
    assert_eq!(steps, 19, "the reference transcript is incomplete: {r:#}");
    assert_eq!(r[0]["result"], "protocol=netrpc version=1");
    assert_eq!(r[6]["result"], "ok 42");
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_host_runs_go_plugin() {
    within(async {
        let want = reference();
        let got = rust_host(&[go_oracle().clone(), "plugin".into()]).await;
        assert_eq!(got, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn go_host_runs_rust_plugin() {
    within(async {
        let want = reference();
        let plugin = rust_plugin();
        let got = tokio::task::spawn_blocking(move || go_host(&[plugin]))
            .await
            .unwrap();
        assert_eq!(got, want);
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_host_runs_rust_plugin() {
    within(async {
        let want = reference();
        let got = rust_host(&[rust_plugin()]).await;
        assert_eq!(got, want);
    })
    .await
}

/// A plugin binary run by hand explains itself and exits 1, as Go's does.
#[test]
fn a_plugin_run_directly_refuses() {
    for exe in [rust_plugin(), go_oracle().clone()] {
        let mut cmd = Command::new(&exe);
        if exe == *go_oracle() {
            cmd.arg("plugin");
        }
        let out = cmd.env_remove("GOPLUGIN_ORACLE").output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", exe.display());
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("This binary is a plugin."));
    }
}
