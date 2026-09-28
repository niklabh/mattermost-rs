//! Cross-server parity for `GET /manualtest` under `ServiceSettings.EnableTesting` —
//! `mm_api::manualtest`, the on-branch of D-782.
//!
//! ```sh
//! MMRS_EDITLIMIT_VARIANT=testing scripts/go-edit-limit.sh start
//! scripts/parity.sh --test parity manualtest
//! ```
//!
//! # A pair of servers of its own
//!
//! Go reads `EnableTesting` once, in `api4.Init`, to decide whether the route exists at all, so
//! it cannot be toggled on the stack's servers. This suite talks to the testing oracle (the
//! stack's binary and database, `MM_SERVICESETTINGS_ENABLETESTING=true`, `GO`'s port + 70) and to
//! an mm-api `SecondServer` given the same variable, forwarding to it and started in the oracle's
//! run directory — so it finds the same `i18n/` Go translated with.
//!
//! # What can be compared, and what cannot
//!
//! Every answer the handler can give is compared: status, every header but the per-request ones,
//! and the body. Two parts of a body differ by construction and are compared structurally:
//!
//! - the error page's `s=`: an ECDSA signature, randomised on Go and deterministic here. Both
//!   servers' signatures are **verified** against the installation's public key over the exact
//!   URL each page carries, then replaced by a placeholder for the byte comparison;
//! - a JSON error's `request_id`.
//!
//! The handler's `uid` seed and random team name are not comparable and need not be: the seed is
//! a no-op at the pinned toolchain and the team is never saved (`mm_api::manualtest`'s module
//! doc). What the suite asserts instead is the **absence** of that write — no team with the
//! handler's `success+…simulator.amazonses.com` email exists after either server answers.
//!
//! # The branch only a planted row reaches
//!
//! `test=autolink` looks for `town-square` as user `""`, which exists only if a `ChannelMembers`
//! row with an empty user id does. The last case plants one on a fresh team's `town-square` and
//! takes it out again, so the post branch — Go's own REST client with no token — is compared too.

use crate::common::{self, GO, SecondServer, client, go_minted_token, stack_enabled};

/// `SecondServer` port for the testing mm-api. Unique in the binary —
/// `parity::second_server_ports` checks.
const TESTING_RUST_PORT: u16 = 8121;

/// `MMRS_GO_PORT + 70`, the port `MMRS_EDITLIMIT_VARIANT=testing scripts/go-edit-limit.sh` binds.
fn testing_go_base() -> String {
    let port: u16 = GO
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("GO carries a port");
    format!("http://localhost:{}", port + 70)
}

/// The oracle's run directory — where its `i18n` symlink is.
fn testing_run_dir() -> std::path::PathBuf {
    let stack: u16 = std::env::var("MMRS_STACK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let suffix = if stack == 0 {
        String::new()
    } else {
        format!("-{stack}")
    };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../reference/.build/mmtesting{suffix}"))
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set under parity.sh");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

#[derive(Debug)]
struct Answer {
    status: u16,
    /// Lower-cased, sorted, the per-request ones removed.
    headers: Vec<(String, String)>,
    served_by: Option<String>,
    body: String,
}

/// Per-request headers, and the length of a body whose signature length varies.
const VOLATILE: [&str; 5] = [
    "date",
    "x-request-id",
    "x-mmrs-served-by",
    "content-length",
    "connection",
];

async fn fetch(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let mut request = http.request(method, url);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut all: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(k, _)| !VOLATILE.contains(&k.as_str()))
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    all.sort();
    let body = response.text().await.expect("a body");
    Answer {
        status,
        headers: all,
        served_by,
        body,
    }
}

/// The installation's public key, from the `Systems` row both servers sign with.
async fn verifying_key(pool: &sqlx::PgPool) -> p256::ecdsa::VerifyingKey {
    let row: String =
        sqlx::query_scalar("SELECT value FROM systems WHERE name = 'AsymmetricSigningKey'")
            .fetch_one(pool)
            .await
            .expect("Go generated a signing key");
    // The coordinates are ~78-digit bare integers: read them as text.
    let coordinate = |name: &str| -> [u8; 32] {
        let at = row.find(&format!("\"{name}\":")).expect("a coordinate") + name.len() + 3;
        let digits: String = row[at..].chars().take_while(char::is_ascii_digit).collect();
        let mut out = [0u8; 32];
        for d in digits.bytes() {
            let mut carry = u32::from(d - b'0');
            for byte in out.iter_mut().rev() {
                let v = u32::from(*byte) * 10 + carry;
                *byte = (v & 0xff) as u8;
                carry = v >> 8;
            }
        }
        out
    };
    let point = p256::EncodedPoint::from_affine_coordinates(
        (&coordinate("x")).into(),
        (&coordinate("y")).into(),
        false,
    );
    p256::ecdsa::VerifyingKey::from_encoded_point(&point).expect("a point on P-256")
}

/// Check the page's signature over its own URL, then replace it with a placeholder. Every one of
/// the three places the URL appears must carry the same signature.
fn verify_and_blank_signature(who: &str, body: &str, key: &p256::ecdsa::VerifyingKey) -> String {
    use base64::Engine as _;
    use p256::ecdsa::signature::hazmat::PrehashVerifier as _;
    use sha2::Digest as _;
    let href = body
        .split("<a href=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("{who}: no link in {body}"))
        .replace("&amp;", "&");
    let (signed, sig) = href
        .split_once("&s=")
        .unwrap_or_else(|| panic!("{who}: no signature in {href}"));
    let der = base64::engine::general_purpose::URL_SAFE
        .decode(sig)
        .unwrap_or_else(|e| panic!("{who}: signature is not URL base64: {e}"));
    let signature = p256::ecdsa::Signature::from_der(&der)
        .unwrap_or_else(|e| panic!("{who}: signature is not DER: {e}"));
    let digest = sha2::Sha256::digest(signed.as_bytes());
    assert!(
        key.verify_prehash(&digest, &signature).is_ok(),
        "{who}: the page's signature does not verify over {signed:?}"
    );
    // The `onload` copy is JS-escaped, which turns base64's `=` padding into `\u003D`.
    let js = sig.replace('=', "\\u003D");
    let blanked = body.replace(&js, "SIG").replace(sig, "SIG");
    assert_eq!(
        blanked.matches("SIG").count(),
        3,
        "{who}: three copies of the URL"
    );
    blanked
}

fn blank_request_id(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut value) => {
            if let Some(obj) = value.as_object_mut() {
                if obj.contains_key("request_id") {
                    obj.insert("request_id".to_owned(), "RID".into());
                }
            }
            value.to_string()
        }
        Err(_) => body.to_owned(),
    }
}

struct Case {
    what: &'static str,
    target: String,
    headers: Vec<(&'static str, String)>,
    status: u16,
    /// What the body must hold, besides being equal: a translated message, or empty.
    needle: &'static str,
}

fn case(what: &'static str, target: &str, status: u16, needle: &'static str) -> Case {
    Case {
        what,
        target: target.to_owned(),
        headers: Vec::new(),
        status,
        needle,
    }
}

async fn compare(
    http: &reqwest::Client,
    go: &str,
    rust: &str,
    key: &p256::ecdsa::VerifyingKey,
    case: &Case,
) {
    let headers: Vec<(&str, &str)> = case.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let g = fetch(
        http,
        reqwest::Method::GET,
        &format!("{go}{}", case.target),
        &headers,
    )
    .await;
    let r = fetch(
        http,
        reqwest::Method::GET,
        &format!("{rust}{}", case.target),
        &headers,
    )
    .await;
    let what = case.what;
    assert_eq!(
        r.served_by.as_deref(),
        Some("rust"),
        "{what}: served, not forwarded"
    );
    assert_eq!(g.status, case.status, "{what}: Go's status");
    assert_eq!(r.status, g.status, "{what}: status");
    assert_eq!(r.headers, g.headers, "{what}: headers");
    let (gb, rb) = if g.body.contains("<!DOCTYPE html>") {
        (
            verify_and_blank_signature(&format!("{what} (Go)"), &g.body, key),
            verify_and_blank_signature(&format!("{what} (Rust)"), &r.body, key),
        )
    } else {
        (blank_request_id(&g.body), blank_request_id(&r.body))
    };
    assert_eq!(rb, gb, "{what}: body");
    if case.needle.is_empty() {
        assert!(g.body.is_empty(), "{what}: Go's body is empty");
    } else {
        assert!(
            g.body.contains(case.needle),
            "{what}: {:?} in {}",
            case.needle,
            g.body
        );
    }
}

/// Rows the handler's team would have written — its email has one shape.
async fn handler_teams(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM teams WHERE email LIKE 'success+%simulator.amazonses.com'",
    )
    .fetch_one(pool)
    .await
    .expect("the count")
}

#[tokio::test]
async fn every_manualtest_answer_matches_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let go = testing_go_base();
    if !http
        .get(format!("{go}/api/v4/system/ping"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        // Panic rather than skip: `stack_enabled` is true, so a skip would pass asserting nothing.
        panic!(
            "manualtest: no EnableTesting oracle at {go}. Run `MMRS_EDITLIMIT_VARIANT=testing \
             scripts/go-edit-limit.sh start`."
        );
    }
    // The oracle's own overrides (scripts/go-edit-limit.sh), so both hash the same client
    // configuration into `X-Version-Id`: `SiteURL` and `EnableTesting` are both in it.
    let go_port = go.rsplit(':').next().expect("a port").to_owned();
    let listen = format!(":{go_port}");
    let server = SecondServer::start_in(
        TESTING_RUST_PORT,
        &testing_run_dir(),
        &[
            ("MM_SERVICESETTINGS_ENABLETESTING", "true"),
            ("MM_SERVICESETTINGS_SITEURL", &go),
            ("MM_SERVICESETTINGS_LISTENADDRESS", &listen),
            ("MM_TEAMSETTINGS_ENABLEOPENSERVER", "true"),
            ("MM_SERVICESETTINGS_ENABLELOCALMODE", "false"),
            ("MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD", "true"),
            ("MM_GO_UPSTREAM", &go),
            // The oracle runs no jobs or schedulers (`scripts/go-edit-limit.sh`), and both
            // settings are in the configuration hash `X-Version-Id` carries.
            ("MM_JOBSETTINGS_RUNJOBS", "false"),
            ("MM_JOBSETTINGS_RUNSCHEDULER", "false"),
        ],
    )
    .await
    .expect("the testing mm-api starts");
    let rust = server.base.clone();
    let pool = pool().await;
    let key = verifying_key(&pool).await;
    // A row an interrupted run left would move `autolink` off its 500.
    sqlx::query("DELETE FROM channelmembers WHERE userid = ''")
        .execute(&pool)
        .await
        .unwrap();
    let teams_before = handler_teams(&pool).await;

    let token = go_minted_token(&http).await;
    // `MaximumURLLength` is 2048 on the stack: a request URI of exactly that many bytes passes,
    // one more is refused.
    let padded = |len: usize| {
        let head = "/manualtest?test=general&pad=";
        format!("{head}{}", "x".repeat(len - head.len()))
    };
    let at_limit = padded(2048);
    let long = padded(2049);

    let mut cases = vec![
        case(
            "any other test is an empty 200",
            "/manualtest?test=general",
            200,
            "",
        ),
        case("an unknown test too", "/manualtest?test=xyz", 200, ""),
        case("an empty test value", "/manualtest?test", 200, ""),
        case(
            "a test value that is not UTF-8",
            "/manualtest?test=%ff",
            200,
            "",
        ),
        case(
            "only the first test value is read",
            "/manualtest?test=general&test=autolink",
            200,
            "",
        ),
        case("no test", "/manualtest", 400, "Unable+to+parse+URL."),
        case(
            "a uid alone",
            "/manualtest?uid=abc",
            400,
            "Unable+to+parse+URL.",
        ),
        case(
            "a uid with a test",
            "/manualtest?uid=abc&test=general",
            200,
            "",
        ),
        case(
            "username without teamname",
            "/manualtest?username=a",
            400,
            "Unable+to+parse+URL.",
        ),
        case(
            "teamname without username",
            "/manualtest?teamname=b&test=general",
            200,
            "",
        ),
        case(
            "both: the team's email never validates",
            "/manualtest?username=a&teamname=b",
            400,
            "Invalid+email.",
        ),
        case(
            "both, empty",
            "/manualtest?username&teamname",
            400,
            "Invalid+email.",
        ),
        case(
            "both, before the test runs",
            "/manualtest?username=a&teamname=b&test=autolink",
            400,
            "Invalid+email.",
        ),
        case(
            "both, with a uid",
            "/manualtest?uid=u1&username=a&teamname=b",
            400,
            "Invalid+email.",
        ),
        case(
            "a bad escape",
            "/manualtest?a=%zz",
            400,
            "Unable+to+parse+URL.",
        ),
        case(
            "a semicolon",
            "/manualtest?a=1;b=2",
            400,
            "Unable+to+parse+URL.",
        ),
        case(
            "a bad escape after a good test",
            "/manualtest?test=general&x=%zz",
            400,
            "Unable+to+parse+URL.",
        ),
        case(
            "autolink with no member row",
            "/manualtest?test=autolink",
            500,
            "Unable+to+get+channels.",
        ),
        case("an over-long URL", &long, 414, "URL+is+too+long"),
        case("a URL exactly at the limit", &at_limit, 200, ""),
        case(
            "an invalid bearer token is ignored",
            "/manualtest?test=general",
            200,
            "",
        ),
        case(
            "a garbage query token is ignored",
            "/manualtest?test=general&access_token=nope",
            200,
            "",
        ),
        case(
            "a valid session in the query is refused",
            &format!("/manualtest?test=general&access_token={token}"),
            401,
            "Session+is+not+OAuth+but+token+was+provided",
        ),
        case(
            "a valid session cookie is fine",
            "/manualtest?test=general",
            200,
            "",
        ),
        case("Spanish", "/manualtest", 400, "No+se+pudo+analizar+el+URL."),
        case(
            "Brazilian Portuguese, on the 500",
            "/manualtest?test=autolink",
            500,
            "N%C3%A3o+%C3%A9+poss%C3%ADvel+obter+canais.",
        ),
        case(
            "German by its language part",
            "/manualtest?username=a&teamname=b",
            400,
            "Ung%C3%BCltige+E-Mail-Adresse.",
        ),
        case(
            "a mobile client gets JSON",
            "/manualtest?test=autolink",
            500,
            "\"id\":\"manaultesting.test_autolink.unable.app_error\"",
        ),
        case(
            "JSON for the email too",
            "/manualtest?username=a&teamname=b",
            400,
            "\"message\":\"Invalid email.\"",
        ),
        case(
            "JSON for the parse, translated",
            "/manualtest",
            400,
            "\"message\":\"No se pudo analizar el URL.\"",
        ),
    ];
    let with = |cases: &mut Vec<Case>, what: &str, headers: Vec<(&'static str, String)>| {
        let case = cases
            .iter_mut()
            .find(|c| c.what == what)
            .unwrap_or_else(|| panic!("no case {what}"));
        case.headers = headers;
    };
    with(
        &mut cases,
        "an invalid bearer token is ignored",
        vec![("Authorization", "Bearer nope".to_owned())],
    );
    with(
        &mut cases,
        "a valid session cookie is fine",
        vec![("Cookie", format!("MMAUTHTOKEN={token}"))],
    );
    with(
        &mut cases,
        "Spanish",
        vec![("Accept-Language", "es".to_owned())],
    );
    with(
        &mut cases,
        "Brazilian Portuguese, on the 500",
        vec![("Accept-Language", "pt-BR,pt;q=0.9".to_owned())],
    );
    with(
        &mut cases,
        "German by its language part",
        vec![("Accept-Language", "de-AT".to_owned())],
    );
    with(
        &mut cases,
        "a mobile client gets JSON",
        vec![("X-Mobile-App", "1".to_owned())],
    );
    with(
        &mut cases,
        "JSON for the email too",
        vec![("X-Mobile-App", "yes".to_owned())],
    );
    with(
        &mut cases,
        "JSON for the parse, translated",
        vec![
            ("X-Mobile-App", "1".to_owned()),
            ("Accept-Language", "es".to_owned()),
        ],
    );
    // gzip is accepted on every case where it is not the subject: none of these bodies is large
    // enough to compress, and `Vary` must still be there.
    for case in &mut cases {
        case.headers.push(("Accept-Encoding", "gzip".to_owned()));
    }

    for case in &cases {
        compare(&http, &go, &rust, &key, case).await;
    }

    // The over-long URL is refused before `ServeHTTP` sets a single header of its own.
    let refused = fetch(&http, reqwest::Method::GET, &format!("{rust}{long}"), &[]).await;
    assert!(
        !refused
            .headers
            .iter()
            .any(|(k, _)| k == "x-version-id" || k == "x-content-type-options"),
        "{:?}",
        refused.headers
    );

    // `HEAD` is not the route's method: both answer with the web client's page handler.
    let g = fetch(
        &http,
        reqwest::Method::HEAD,
        &format!("{go}/manualtest?test=general"),
        &[],
    )
    .await;
    let r = fetch(
        &http,
        reqwest::Method::HEAD,
        &format!("{rust}/manualtest?test=general"),
        &[],
    )
    .await;
    assert_eq!(r.served_by.as_deref(), Some("rust"));
    assert_eq!((r.status, &r.headers), (g.status, &g.headers), "HEAD");

    // The planted row: `town-square` of a fresh team, as user "".
    let admin = go_minted_token(&http).await;
    let team_id = common::create_team(&http, &admin, "manualtest").await;
    let town_square: serde_json::Value = http
        .get(format!(
            "{GO}/api/v4/teams/{team_id}/channels/name/town-square"
        ))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel_id = town_square["id"].as_str().expect("town-square").to_owned();
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount, \
         mentioncount, notifyprops, lastupdateat, schemeuser, schemeadmin, schemeguest, \
         mentioncountroot, msgcountroot, urgentmentioncount) \
         VALUES ($1, '', '', 0, 0, 0, '{}'::jsonb, 0, true, false, false, 0, 0, 0)",
    )
    .bind(&channel_id)
    .execute(&pool)
    .await
    .unwrap();
    let planted = [
        case(
            "autolink finds the channel and posts with no token",
            "/manualtest?test=autolink",
            401,
            "Invalid+or+expired+session",
        ),
        Case {
            headers: vec![("X-Mobile-App", "1".to_owned())],
            ..case(
                "the same, as JSON",
                "/manualtest?test=autolink",
                401,
                "\"id\":\"api.context.session_expired.app_error\"",
            )
        },
    ];
    for case in &planted {
        compare(&http, &go, &rust, &key, case).await;
    }
    sqlx::query("DELETE FROM channelmembers WHERE userid = ''")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        handler_teams(&pool).await,
        teams_before,
        "neither server saved the handler's team"
    );
}
