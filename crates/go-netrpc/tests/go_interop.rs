//! go-netrpc against Go's net/rpc (`reference/dump/netrpc`), both directions.
//!
//! The Go client's transcript against the Go server is the reference. The Rust client must
//! produce the same transcript against the Go server, and the Go client the same transcript
//! against the Rust server, which implements the same services as the Go oracle.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use go_netrpc::{Client, Error, Server, ServiceError};
use gobwire::{Gob, Interface, Type, Value};
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;

/// Every test runs under a deadline: a lost reply or a missed shutdown shows up as a hang, and a
/// hung test would stall a mutation batch instead of failing it.
async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(20), f)
        .await
        .expect("test timed out: a call never completed")
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Args {
    #[gob(name = "A")]
    a: i64,
    #[gob(name = "B")]
    b: i64,
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Quotient {
    #[gob(name = "Quo")]
    quo: i64,
    #[gob(name = "Rem")]
    rem: i64,
}

#[derive(Gob, Debug, Default, Clone, PartialEq)]
struct Empty {}

fn oracle() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("netrpc-oracle");
        let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/dump");
        let status = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./netrpc")
            .current_dir(&dump)
            .status()
            .unwrap_or_else(|e| panic!("the Go oracle needs a Go toolchain on PATH: {e}"));
        assert!(status.success(), "building reference/dump/netrpc failed");
        out
    })
}

fn transcript(stdout: &[u8]) -> Vec<Json> {
    serde_json::from_slice::<Vec<Json>>(stdout)
        .unwrap_or_else(|e| panic!("bad transcript ({e}): {}", String::from_utf8_lossy(stdout)))
}

fn go_reference() -> Vec<Json> {
    let out = Command::new(oracle()).arg("selftest").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    transcript(&out.stdout)
}

/// A Go server, killed when dropped (its stdin closes).
struct GoServer {
    child: Child,
    addr: String,
}

impl GoServer {
    fn start() -> Self {
        let mut child = Command::new(oracle())
            .arg("server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Self {
            child,
            addr: line.trim().to_owned(),
        }
    }
}

impl Drop for GoServer {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Go's `json.Marshal` of what an `any` holds.
fn iface_json(i: &Option<Interface>) -> Json {
    fn value(ty: &Type, v: &Value) -> Json {
        match (ty, v) {
            (_, Value::Interface(i)) => iface_json(&i.as_deref().cloned()),
            (_, Value::Float(f)) => json!(f),
            (_, Value::Int(n)) => json!(n),
            (_, Value::String(s)) => json!(s),
            (_, Value::Bool(b)) => json!(b),
            (Type::Slice(t), Value::Slice(items)) => {
                Json::Array(items.iter().map(|x| value(t, x)).collect())
            }
            (Type::Map(_, vt), Value::Map(pairs)) => Json::Object(
                pairs
                    .iter()
                    .map(|(k, v)| match k {
                        Value::String(k) => (k.clone(), value(vt, v)),
                        other => panic!("non-string key {other:?}"),
                    })
                    .collect(),
            ),
            other => panic!("unexpected {other:?}"),
        }
    }
    match i {
        None => Json::Null,
        Some(i) => value(&i.ty, &i.value),
    }
}

fn record<T>(out: &mut Vec<Json>, call: &str, r: Result<T, Error>, render: impl FnOnce(T) -> Json) {
    out.push(match r {
        Ok(v) => json!({ "call": call, "reply": render(v) }),
        Err(Error::Server(msg)) => json!({ "call": call, "error": format!("server: {msg}") }),
        Err(e) => json!({ "call": call, "error": format!("client: {e}") }),
    });
}

/// The Go oracle's `scenario`, call for call.
async fn rust_scenario(c: &Client) -> Vec<Json> {
    let mut out = Vec::new();
    let r = c
        .call::<_, i64>("Arith.Multiply", &Args { a: 7, b: 8 })
        .await;
    record(&mut out, "Arith.Multiply 7x8", r, |n| json!(n));
    let quotient = |q: Quotient| json!({ "Quo": q.quo, "Rem": q.rem });
    let r = c
        .call::<_, Quotient>("Arith.Divide", &Args { a: 17, b: 5 })
        .await;
    record(&mut out, "Arith.Divide 17/5", r, quotient);
    let r = c
        .call::<_, Quotient>("Arith.Divide", &Args { a: 1, b: 0 })
        .await;
    record(&mut out, "Arith.Divide 1/0", r, quotient);
    let r = c
        .call::<_, Quotient>("Arith.Fail", &Args { a: 3, b: 4 })
        .await;
    record(&mut out, "Arith.Fail", r, quotient);
    let r = c.call::<_, i64>("Arith.Nope", &Args::default()).await;
    record(&mut out, "Arith.Nope", r, |n| json!(n));
    let r = c.call::<_, i64>("Nope.Method", &Args::default()).await;
    record(&mut out, "Nope.Method", r, |n| json!(n));
    let r = c.call::<_, i64>("NoDot", &Args::default()).await;
    record(&mut out, "NoDot", r, |n| json!(n));
    let r = c
        .call::<_, i64>("Arith.Multiply", &Args { a: -3, b: 9 })
        .await;
    record(&mut out, "Arith.Multiply after errors", r, |n| json!(n));

    let map = Interface::new(
        gobwire::names::STRING_ANY_MAP,
        &HashMap::from([
            (
                "k".to_string(),
                Some(
                    Interface::new(
                        gobwire::names::ANY_SLICE,
                        &vec![
                            Some(Interface::float64(1.5)),
                            Some(Interface::string("x")),
                            None,
                        ],
                    )
                    .unwrap(),
                ),
            ),
            ("n".to_string(), Some(Interface::string("s"))),
        ]),
    )
    .unwrap();
    let r = c
        .call::<_, Option<Interface>>("Echo.Iface", &Some(map))
        .await;
    record(&mut out, "Echo.Iface", r, |i| iface_json(&i));
    let r = c
        .call::<_, Vec<String>>("Echo.Strings", &vec!["a".to_string(), String::new()])
        .await;
    record(&mut out, "Echo.Strings", r, |v| json!(v));
    let r = c
        .call::<_, HashMap<String, i64>>(
            "Echo.Map",
            &HashMap::from([("one".to_string(), 1i64), ("two".to_string(), 2)]),
        )
        .await;
    record(&mut out, "Echo.Map", r, |m| json!(m));
    let r = c.call::<_, Empty>("Echo.Empty", &Empty {}).await;
    record(&mut out, "Echo.Empty", r, |_| json!({}));
    let big: Vec<u8> = (0..3usize << 20).map(|i| (i * 7) as u8).collect();
    let r = c.call::<_, [i64; 2]>("Echo.Sum", &big).await;
    record(&mut out, "Echo.Sum 3MiB", r, |s| json!(s));

    let calls = (0..50i64).map(|i| {
        let c = c.clone();
        async move {
            (
                200 - i * 3,
                c.call::<_, i64>("Echo.Slow", &(200 - i * 3)).await,
            )
        }
    });
    let results = futures_join_all(calls).await;
    let answered = results.iter().filter(|(_, r)| r.is_ok()).count();
    let mismatched = results
        .iter()
        .filter(|(k, r)| matches!(r, Ok(v) if v != k))
        .count();
    out.push(json!({ "call": "Echo.Slow x50", "reply": { "answered": answered, "mismatched": mismatched } }));

    let r = c.call::<_, i64>("Arith.Multiply", "not args").await;
    let r = r.map_err(|e| match e {
        Error::Server(_) => Error::Server("<decode error>".into()),
        other => other,
    });
    record(&mut out, "Arith.Multiply with a string", r, |n| json!(n));
    let r = c
        .call::<_, i64>("Arith.Multiply", &Args { a: 2, b: 21 })
        .await;
    record(&mut out, "Arith.Multiply after a bad body", r, |n| json!(n));
    out
}

async fn futures_join_all<F: std::future::Future + Send + 'static>(
    futs: impl Iterator<Item = F>,
) -> Vec<F::Output>
where
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

/// The Go oracle's services.
fn rust_server() -> Arc<Server> {
    let mut s = Server::new();
    s.register("Arith.Multiply", |a: Args| async move {
        Ok::<_, ServiceError>(a.a * a.b)
    });
    s.register("Arith.Divide", |a: Args| async move {
        if a.b == 0 {
            return Err(ServiceError("divide by zero".into()));
        }
        Ok(Quotient {
            quo: a.a / a.b,
            rem: a.a % a.b,
        })
    });
    s.register("Arith.Fail", |a: Args| async move {
        let _partial = Quotient { quo: 99, rem: 0 };
        Err::<Quotient, _>(ServiceError(format!("failed with {}", a.a)))
    });
    s.register("Echo.Iface", |i: Option<Interface>| async move {
        Ok::<_, ServiceError>(i)
    });
    s.register("Echo.Strings", |mut v: Vec<String>| async move {
        v.push("!".into());
        Ok::<_, ServiceError>(v)
    });
    s.register("Echo.Map", |m: HashMap<String, i64>| async move {
        Ok::<_, ServiceError>(
            m.into_iter()
                .map(|(k, v)| (k, v * 2))
                .collect::<HashMap<_, _>>(),
        )
    });
    s.register("Echo.Empty", |_: Empty| async move {
        Ok::<_, ServiceError>(Empty {})
    });
    s.register("Echo.Sum", |b: Vec<u8>| async move {
        Ok::<_, ServiceError>([b.len() as i64, b.iter().map(|&x| i64::from(x)).sum::<i64>()])
    });
    s.register("Echo.Slow", |ms: i64| async move {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        Ok::<_, ServiceError>(ms)
    });
    Arc::new(s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_client_against_go_server() {
    within(async move {
        let reference = go_reference();
        let server = GoServer::start();
        let stream = tokio::net::TcpStream::connect(&server.addr).await.unwrap();
        let client = Client::new(stream);
        let got = rust_scenario(&client).await;
        client.close().await.unwrap();
        assert_eq!(Json::Array(got), Json::Array(reference));
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn go_client_against_rust_server() {
    within(async move {
        let reference = go_reference();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = rust_server();
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            server.serve(stream).await
        });
        let out = tokio::task::spawn_blocking(move || {
            Command::new(oracle())
                .args(["client", &addr])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(Json::Array(transcript(&out.stdout)), Json::Array(reference));
        // The Go client closed its connection: the server returns cleanly.
        served.await.unwrap().unwrap();
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_client_against_rust_server() {
    within(async move {
        let reference = go_reference();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = rust_server();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            server.serve(stream).await
        });
        let client = Client::new(tokio::net::TcpStream::connect(addr).await.unwrap());
        let got = rust_scenario(&client).await;
        assert_eq!(Json::Array(got), Json::Array(reference));
    })
    .await
}
