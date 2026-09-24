//! Cross-server parity for **rate limiting** (D-430): `RateLimitSettings.Enable` on both sides,
//! bursts fired at the three rate-limited routes and at a per-user route, and the status
//! sequences and `X-RateLimit-*` / `Retry-After` headers compared. Reproduced by
//! `mm_api::ratelimit`.
//!
//! ```sh
//! scripts/parity.sh --test parity ratelimit
//! ```
//!
//! # Its own pair of servers
//!
//! Go reads `Enable` **at start** — `RateLimitedHandler` when api4 registers its routes, the
//! global wrapper in `Server.Start` — so it cannot be turned on for the stack's servers for the
//! length of a test, and turned on for good it would refuse every suite's bursts. This suite starts
//! the stack's Go binary on Go's port + 75 and an mm-api `SecondServer`, both with the settings in
//! the **environment**, which Go never writes back to the shared document.
//!
//! `VaryByUser` is on, so the per-user limiter in `ServeHTTP` runs too; `PerSec` 1 and `MaxBurst`
//! 30 make the global and per-user budgets small enough to exhaust. Anonymous requests key on the
//! address on both limiters, so the login burst is unaffected by `VaryByUser`.
//!
//! # Timing
//!
//! A limiter's answers depend on elapsed time only through `floor` and `ceil` of multiples of its
//! period (200ms for login, 500ms for the other two, 1s for the global one). The requests are
//! **interleaved** — each one to Go, then the same to this server — so both limiters see the same
//! timeline to within one request, however loaded the machine is. The shared global budget's
//! `Remaining` and `Reset` on an anonymous answer are still compared within one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::common;
use common::{GO, SecondServer, client, go_minted_token, stack_enabled};

/// The mm-api under test; see `second_server_ports`.
const RUST_PORT: u16 = 8141;
/// This suite's Go server sits at Go's port plus this.
const GO_OFFSET: u16 = 75;

const ENV: [(&str, &str); 4] = [
    ("MM_RATELIMITSETTINGS_ENABLE", "true"),
    ("MM_RATELIMITSETTINGS_PERSEC", "1"),
    ("MM_RATELIMITSETTINGS_MAXBURST", "30"),
    ("MM_RATELIMITSETTINGS_VARYBYUSER", "true"),
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The Go server this suite starts: killed on drop.
struct GoServer {
    child: std::process::Child,
    base: String,
}

impl Drop for GoServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn go_port() -> u16 {
    GO.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("GO has a port")
}

/// Start the stack's Go binary in its own run directory, on the shared database and
/// configuration, with `env` on top — `parity::plugin_startup`'s launch, minus the plugins.
async fn start_go(env: &[(&str, &str)]) -> GoServer {
    let binary = repo().join("reference/.build/mattermost");
    assert!(
        binary.exists(),
        "no Go binary at {} — run scripts/go-server.sh",
        binary.display()
    );
    let stack = std::env::var("MMRS_STACK").unwrap_or_default();
    let run = repo().join(format!("reference/.build/mmratelimit-{stack}"));
    std::fs::create_dir_all(run.join("logs")).unwrap();
    std::fs::create_dir_all(run.join("data")).unwrap();
    let src = repo().join("reference/mattermost/server");
    for dir in ["i18n", "templates", "fonts"] {
        let _ = std::os::unix::fs::symlink(src.join(dir), run.join(dir));
    }
    let port = go_port() + GO_OFFSET;
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ss -ltnp 2>/dev/null | awk '$4 ~ /:{port}$/' \
             | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u | xargs -r kill -9"
        ))
        .status();
    let dsn = format!(
        "{}?sslmode=disable&connect_timeout=10",
        std::env::var("DATABASE_URL").expect("parity.sh sets DATABASE_URL")
    );
    let log = std::fs::File::create(run.join("go.log")).unwrap();
    let mut command = std::process::Command::new(&binary);
    command
        .arg("server")
        .current_dir(&run)
        .env("PWD", &run)
        .env("MM_CONFIG", &dsn)
        .env("MM_SQLSETTINGS_DRIVERNAME", "postgres")
        .env("MM_SQLSETTINGS_DATASOURCE", &dsn)
        .env(
            "MM_SERVICESETTINGS_SITEURL",
            format!("http://localhost:{port}"),
        )
        .env("MM_SERVICESETTINGS_LISTENADDRESS", format!(":{port}"))
        .env("MM_SERVICESETTINGS_ENABLELOCALMODE", "false")
        // See `parity::plugin_startup`: the database's connection budget is shared.
        .env("MM_SQLSETTINGS_MAXIDLECONNS", "2")
        .env("MM_SQLSETTINGS_MAXOPENCONNS", "5")
        .env("MM_JOBSETTINGS_RUNJOBS", "false")
        .env("MM_JOBSETTINGS_RUNSCHEDULER", "false")
        .env(
            "MM_FILESETTINGS_DIRECTORY",
            format!("{}/", run.join("data").display()),
        )
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command.spawn().expect("the Go server starts");
    let mut server = GoServer {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    let http = client();
    for _ in 0..450 {
        assert!(
            server.child.try_wait().ok().flatten().is_none(),
            "the Go server exited — see {}",
            run.join("go.log").display()
        );
        // Not `/system/ping`: every probe is a request the global limiter counts, and the count
        // is what this suite compares. A refused connection counts nothing.
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    drop(http);
    panic!(
        "the Go server never listened — see {}",
        run.join("go.log").display()
    );
}

/// One answer, reduced to what the limiters decide: the status, every rate-limit value in the
/// order written, and — for a refusal — the whole header set and the body.
#[derive(Debug, Clone, PartialEq)]
struct Answer {
    status: u16,
    limit: Vec<i64>,
    remaining: Vec<i64>,
    reset: Vec<i64>,
    retry_after: Vec<i64>,
    /// For a 429 only: the headers other than the per-process and rate-limit ones.
    refusal_headers: Vec<(String, String)>,
    refusal_body: String,
}

fn values(headers: &reqwest::header::HeaderMap, name: &str) -> Vec<i64> {
    headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap().parse().unwrap())
        .collect()
}

async fn fire(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Answer {
    let mut request = http.request(method.clone(), url);
    if method == reqwest::Method::POST {
        request = request
            .header("Content-Type", "application/json")
            .body("{}");
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    let mut refusal_headers = Vec::new();
    let mut refusal_body = String::new();
    if status == 429 {
        refusal_headers = headers
            .iter()
            .filter(|(k, _)| {
                ![
                    "date",
                    "x-request-id",
                    "x-version-id",
                    "x-mmrs-served-by",
                    "x-ratelimit-limit",
                    "x-ratelimit-remaining",
                    "x-ratelimit-reset",
                    "retry-after",
                ]
                .contains(&k.as_str())
            })
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
            .collect();
        refusal_headers.sort();
        refusal_body = body;
    }
    Answer {
        status,
        limit: values(&headers, "x-ratelimit-limit"),
        remaining: values(&headers, "x-ratelimit-remaining"),
        reset: values(&headers, "x-ratelimit-reset"),
        retry_after: values(&headers, "retry-after"),
        refusal_headers,
        refusal_body,
    }
}

/// `n` requests to each server, **interleaved** — Go's first, then ours, then Go's second — so both
/// limiters see the same timeline to within a request, whatever the load on the machine.
async fn burst(
    http: &reqwest::Client,
    method: reqwest::Method,
    bases: (&str, &str),
    path: &str,
    token: Option<&str>,
    n: usize,
) -> (Vec<Answer>, Vec<Answer>) {
    let mut out = (Vec::with_capacity(n), Vec::with_capacity(n));
    for _ in 0..n {
        out.0
            .push(fire(http, method.clone(), &format!("{}{path}", bases.0), token).await);
        out.1
            .push(fire(http, method.clone(), &format!("{}{path}", bases.1), token).await);
    }
    out
}

/// Compare two bursts: everything exactly, except the **first** value of `Remaining` and `Reset`
/// on an anonymous answer — the shared global budget's — within one. See the module's notes.
fn assert_same(context: &str, go: &[Answer], rust: &[Answer], global_tolerance: bool) {
    assert_eq!(go.len(), rust.len());
    for (i, (g, r)) in go.iter().zip(rust).enumerate() {
        let context = format!("{context} #{i}");
        assert_eq!(g.status, r.status, "{context}: status");
        assert_eq!(g.limit, r.limit, "{context}: X-RateLimit-Limit");
        assert_eq!(g.retry_after, r.retry_after, "{context}: Retry-After");
        assert_eq!(
            g.refusal_headers, r.refusal_headers,
            "{context}: refusal headers"
        );
        assert_eq!(g.refusal_body, r.refusal_body, "{context}: refusal body");
        for (name, gv, rv) in [
            ("Remaining", &g.remaining, &r.remaining),
            ("Reset", &g.reset, &r.reset),
        ] {
            assert_eq!(gv.len(), rv.len(), "{context}: {name} count");
            for (k, (a, b)) in gv.iter().zip(rv).enumerate() {
                if global_tolerance && k == 0 {
                    assert!((a - b).abs() <= 1, "{context}: {name}[0] {a} vs {b}");
                } else {
                    assert_eq!(a, b, "{context}: {name}[{k}]");
                }
            }
        }
    }
}

#[tokio::test]
async fn bursts_are_refused_as_go_refuses_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    // Two sessions of one user: the per-user budget is shared between them, the global one (keyed
    // on the token under `VaryByUser`) is not — which is what lets the per-user limiter refuse
    // first.
    let (team, _) = common::a_team_and_channel_the_user_is_in(&http, &admin).await;
    let user = common::create_plain_user(&http, &admin, &team, "ratelimit").await;
    let second = common::login_plain_user(&http, "ratelimit").await;

    let go = start_go(&ENV).await;
    let rust = SecondServer::start(RUST_PORT, &ENV)
        .await
        .expect("the rate-limited mm-api starts");
    // `SecondServer::start` knows the server is up by pinging it, which the global limiter
    // counts. One period (1s at `PerSec` 1) gives that token back before anything is compared.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Each server's limiters are its own, so interleaving the requests shares nothing.
    let bases = (go.base.as_str(), rust.base.as_str());
    let post = reqwest::Method::POST;
    let get = reqwest::Method::GET;
    let login = burst(&http, post.clone(), bases, "/api/v4/users/login", None, 13).await;
    let desktop = burst(
        &http,
        post.clone(),
        bases,
        "/api/v4/users/login/desktop_token",
        None,
        3,
    )
    .await;
    let register = burst(&http, post, bases, "/api/v4/oauth/apps/register", None, 3).await;
    // The global budget: 31 at 1/s, of which 19 are spent above; ping until it refuses.
    let ping = burst(&http, get.clone(), bases, "/api/v4/system/ping", None, 14).await;
    // Per user: 31 on the user id, 31 on each token.
    let first = burst(
        &http,
        get.clone(),
        bases,
        "/api/v4/users/me",
        Some(&user.token),
        20,
    )
    .await;
    let other = burst(&http, get, bases, "/api/v4/users/me", Some(&second), 13).await;
    common::delete_plain_user(&http, &admin, &user.id).await;

    assert_same("login", &login.0, &login.1, true);
    assert_same("desktop_token", &desktop.0, &desktop.1, true);
    assert_same("oauth register", &register.0, &register.1, true);
    assert_same("ping", &ping.0, &ping.1, true);
    assert_same("first session", &first.0, &first.1, false);
    assert_same("second session", &other.0, &other.1, false);
    let g = (
        &login.0,
        &desktop.0,
        &register.0,
        &ping.0,
        &first.0,
        &other.0,
    );

    // Every branch was reached, on Go's side, so the comparison above means something.
    let refused = |answers: &[Answer]| answers.iter().filter(|a| a.status == 429).count();
    assert_eq!(refused(g.0), 2, "login: 11 allowed, then refused");
    assert_eq!(g.0[0].limit, [31, 11], "the global set, then the route's");
    assert!(
        refused(g.1) >= 1 && refused(g.2) >= 1,
        "the 2/1 routes refuse"
    );
    assert!(refused(g.3) >= 1, "the global limiter refuses");
    assert_eq!(
        refused(g.4),
        0,
        "the first session stays inside both budgets"
    );
    assert!(
        refused(g.5) >= 1,
        "the per-user limiter refuses the second session"
    );
    let per_user = g.5.iter().find(|a| a.status == 429).expect("a refusal");
    assert!(
        per_user
            .refusal_headers
            .iter()
            .any(|(k, _)| k == "referrer-policy"),
        "the per-user refusal is written inside ServeHTTP, after its headers: {per_user:?}"
    );
    let bare = g.0.iter().find(|a| a.status == 429).expect("a refusal");
    assert!(
        !bare
            .refusal_headers
            .iter()
            .any(|(k, _)| k == "referrer-policy"),
        "the route refusal is written outside it: {bare:?}"
    );
    drop(go);
}
