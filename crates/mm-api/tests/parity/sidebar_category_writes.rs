//! Cross-server parity for the **five writes** of `BaseRoutes.ChannelCategories`:
//!
//! ```text
//! POST   /api/v4/users/{user_id}/teams/{team_id}/channels/categories
//! PUT    /api/v4/users/{user_id}/teams/{team_id}/channels/categories
//! PUT    /api/v4/users/{user_id}/teams/{team_id}/channels/categories/order
//! PUT    /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}
//! DELETE /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}
//! ```
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity sidebar_category_writes
//! ```
//!
//! # Two subjects on one team, and why not one
//!
//! A write mutates the database both servers share, so the same request cannot be sent to both and
//! compared: the second would see the first's rows. Each server gets its **own subject user**,
//! joined to the **same team and the same channels** — so channel ids appear identically in both
//! answers and only the user id and the minted category id differ, which
//! [`normalise`] substitutes out. Two teams would have made the channel ids differ too, and the
//! comparison would have had nothing left to compare.
//!
//! Neither subject has a DM, deliberately: a DM's display name is empty, two of them tie under the
//! orphan query's `ORDER BY DisplayName`, and the pair would have to be created separately per
//! subject anyway. The DM category's read-only `muted` is asserted here without one; the orphan
//! routing for `D`/`G` lives in `mm-store`'s `db_sidebar_category_writes`.
//!
//! # Everything is compared as raw bytes first
//!
//! Four of the five writes end in `w.Write(json.Marshal(...))` — no trailing newline — and the
//! `GET` on `/order` ends in `json.NewEncoder`, which does have one. So `PUT .../order` and
//! `GET .../order` differ in framing on the same path, and only a byte comparison sees it.
//!
//! # Every read-back goes through the server that wrote
//!
//! Go's in-process caches do not see our writes and ours do not see Go's ([D-190]).
//!
//! # Rows all begin `mmrssbwrite`
//!
//! Not the shared `mmrs-parity-` prefix: that purge runs once per test *binary* and binaries run
//! concurrently, so sharing it means another suite's start-up can delete this suite's team
//! mid-run. [`purge_write_fixtures`] clears these rows instead, and nothing else touches them.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, assert_error_bodies_match_except_known_gaps, client, go_minted_token,
    stack_enabled,
};

const PREFIX: &str = "mmrssbwrite";

/// A syntactically valid id that names nothing.
const NOWHERE: &str = "y9i4er48tt8bukijy7i3u5y9ar";

async fn purge_write_fixtures() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrssbwrite%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrssbwrite%'";
    let channels_of_teams = format!("SELECT id FROM channels WHERE teamid IN ({TEAMS})");

    for statement in [
        format!(
            "DELETE FROM sidebarchannels WHERE userid IN ({USERS}) OR categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS}))"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        format!("DELETE FROM preferences WHERE userid IN ({USERS})"),
        format!("DELETE FROM posts WHERE channelid IN ({channels_of_teams})"),
        format!("DELETE FROM channelmemberhistory WHERE channelid IN ({channels_of_teams})"),
        format!("DELETE FROM channelmembers WHERE channelid IN ({channels_of_teams})"),
        format!("DELETE FROM channelmembers WHERE userid IN ({USERS})"),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM channels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrssbwrite%'".to_owned(),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrssbwrite%'".to_owned(),
    ] {
        let _ = sqlx::query(&statement).execute(&pool).await;
    }
}

// ---------------------------------------------------------------------------
// One request, one server
// ---------------------------------------------------------------------------

/// `(status, raw body)` for one request, and — when the server was ours — the assertion that we
/// answered it rather than forwarding.
async fn send(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&serde_json::Value>,
    connection_id: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut request = http
        .request(method.clone(), format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(connection_id) = connection_id {
        request = request.header("Connection-Id", connection_id);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} {method} {path} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}

/// A body with malformed JSON — `reqwest::json` cannot produce one.
async fn send_raw_body(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>) {
    let response = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

struct Subject {
    id: String,
    token: String,
}

impl Subject {
    fn categories(&self, team_id: &str) -> String {
        format!(
            "/api/v4/users/{}/teams/{team_id}/channels/categories",
            self.id
        )
    }

    fn default_category(&self, kind: &str, team_id: &str) -> String {
        format!("{kind}_{}_{team_id}", self.id)
    }
}

struct Fixture {
    team_id: String,
    /// Display names chosen so display-name order is not id order; see the field comments.
    bravo: String,
    charlie: String,
    /// A channel on the team that neither subject is a member of — the one
    /// `validateSidebarCategoryChannels` must silently drop.
    outsider: String,
    go: Subject,
    rust: Subject,
    admin_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture).await
}

async fn go_post(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = http
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "POST {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the body decodes")
}

async fn build_fixture() -> Fixture {
    purge_write_fixtures().await;

    let http = client();
    let admin_token = go_minted_token(&http).await;

    let team = go_post(
        &http,
        &admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}team"),
            "display_name": "SBW Team",
            "type": "O",
        }),
    )
    .await;
    let team_id = team["id"].as_str().expect("an id").to_owned();

    // Display names in the reverse of the channel-name order, so any test that sees them sorted
    // knows which key was used.
    let mut created = Vec::new();
    for (name, display) in [
        ("bravo", "SBW Zulu"),
        ("charlie", "SBW Yankee"),
        ("outsider", "SBW Xray"),
    ] {
        let channel = go_post(
            &http,
            &admin_token,
            "/api/v4/channels",
            serde_json::json!({
                "team_id": team_id,
                "name": format!("{PREFIX}-{name}"),
                "display_name": display,
                "type": "O",
            }),
        )
        .await;
        created.push(channel["id"].as_str().expect("an id").to_owned());
    }
    let (bravo, charlie, outsider) = (created[0].clone(), created[1].clone(), created[2].clone());

    let mut subjects = Vec::new();
    for which in ["go", "rust"] {
        let username = format!("{PREFIX}{which}");
        let password = "Mmrs-Sidebar-Write-1234";
        let user = go_post(
            &http,
            &admin_token,
            "/api/v4/users",
            serde_json::json!({
                "email": format!("{username}@mmrs.invalid"),
                "username": username,
                "password": password,
            }),
        )
        .await;
        let id = user["id"].as_str().expect("an id").to_owned();

        // Joining the team is what creates the three default categories.
        go_post(
            &http,
            &admin_token,
            &format!("/api/v4/teams/{team_id}/members"),
            serde_json::json!({ "team_id": team_id, "user_id": id }),
        )
        .await;
        for channel in [&bravo, &charlie] {
            go_post(
                &http,
                &admin_token,
                &format!("/api/v4/channels/{channel}/members"),
                serde_json::json!({ "user_id": id }),
            )
            .await;
        }

        let login = http
            .post(format!("{GO}/api/v4/users/login"))
            .json(&serde_json::json!({ "login_id": username, "password": password }))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(login.status(), 200, "{username} cannot log in");
        let token = login
            .headers()
            .get("token")
            .expect("Go returns a token header")
            .to_str()
            .expect("ASCII")
            .to_owned();
        subjects.push(Subject { id, token });
    }
    let rust = subjects.pop().expect("two subjects");
    let go = subjects.pop().expect("two subjects");

    Fixture {
        team_id,
        bravo,
        charlie,
        outsider,
        go,
        rust,
        admin_token,
    }
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Substitute out everything that *must* differ between the two servers' answers: the subject's
/// user id — which also normalises the `{type}_{userId}_{teamId}` default-category ids — and the
/// freshly minted ids of any categories this test created.
///
/// Applied to every string in the graph, at any depth, so an id nested inside a websocket event's
/// stringified payload normalises too once that string has been re-parsed.
fn normalise(value: &serde_json::Value, subject_id: &str, minted: &[&str]) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let mut out = s.replace(subject_id, "<user>");
            for (index, id) in minted.iter().enumerate() {
                out = out.replace(id, &format!("<minted{index}>"));
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| normalise(item, subject_id, minted))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), normalise(item, subject_id, minted)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn parse(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    })
}

/// Remove every category this test created, so a rerun starts from the three defaults.
async fn reset(http: &reqwest::Client, f: &Fixture) {
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = subject.categories(&f.team_id);
        let (_, raw) = send(
            http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &path,
            None,
            None,
        )
        .await;
        let value = parse(&raw, "the category listing");
        let mut custom_ids = Vec::new();
        let mut order = Vec::new();
        for category in value["categories"].as_array().cloned().unwrap_or_default() {
            let id = category["id"].as_str().unwrap_or_default().to_owned();
            if category["type"] == "custom" {
                custom_ids.push(id);
            } else {
                order.push(id);
            }
        }

        // Empty every default category, so a previous test's channel placements do not leak.
        let empties: Vec<serde_json::Value> = value["categories"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|category| category["type"] != "custom")
            .map(|category| {
                serde_json::json!({
                    "id": category["id"],
                    "user_id": subject.id,
                    "team_id": f.team_id,
                    "type": category["type"],
                    "display_name": category["display_name"],
                    "sorting": "",
                    "muted": false,
                    "collapsed": false,
                    "channel_ids": [],
                })
            })
            .collect();
        send(
            http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            Some(&serde_json::Value::Array(empties)),
            None,
        )
        .await;

        for id in custom_ids {
            send(
                http,
                base,
                &subject.token,
                reqwest::Method::DELETE,
                &format!("{path}/{id}"),
                None,
                None,
            )
            .await;
        }
        // And the canonical order, so `/order` tests do not inherit a permutation.
        if !order.is_empty() {
            send(
                http,
                base,
                &subject.token,
                reqwest::Method::PUT,
                &format!("{path}/order"),
                Some(&serde_json::json!(order)),
                None,
            )
            .await;
        }
    }
}

/// Serialised: every test here mutates the two subjects' sidebars.
static SIDEBAR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Create one category on each server from the same request, and return
/// `((go_status, go_raw, go_id), (rust_status, rust_raw, rust_id))`.
async fn create_on_both(
    http: &reqwest::Client,
    f: &Fixture,
    body: &dyn Fn(&Subject) -> serde_json::Value,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = send(
        http,
        GO,
        &f.go.token,
        reqwest::Method::POST,
        &f.go.categories(&f.team_id),
        Some(&body(&f.go)),
        None,
    )
    .await;
    let rust = send(
        http,
        RUST,
        &f.rust.token,
        reqwest::Method::POST,
        &f.rust.categories(&f.team_id),
        Some(&body(&f.rust)),
        None,
    )
    .await;
    (go, rust)
}

// ---------------------------------------------------------------------------
// createCategoryForTeamForUser
// ---------------------------------------------------------------------------

/// The success body, and the four fields of the request that do not survive it.
#[tokio::test]
async fn creating_a_category_agrees_and_ignores_id_type_and_collapsed() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let body = |subject: &Subject| {
        serde_json::json!({
            // Every one of these three is ignored by the store.
            "id": NOWHERE,
            "type": "favorites",
            "collapsed": true,
            // And these four carry.
            "user_id": subject.id,
            "team_id": f.team_id,
            "display_name": "SBW Custom",
            "sorting": "alpha",
            "muted": true,
            // `charlie` twice, and one channel the subject is not a member of.
            "channel_ids": [f.charlie, f.bravo, f.charlie, f.outsider],
        })
    };
    let ((go_status, go_raw), (rust_status, rust_raw)) = create_on_both(&http, f, &body).await;

    assert_eq!(
        go_status,
        200,
        "Go answers OK: {}",
        String::from_utf8_lossy(&go_raw)
    );
    assert_eq!(rust_status, go_status);
    assert!(
        !go_raw.ends_with(b"\n"),
        "`w.Write` after `json.Marshal` leaves no newline: {:?}",
        String::from_utf8_lossy(&go_raw)
    );
    assert_eq!(
        rust_raw.ends_with(b"\n"),
        go_raw.ends_with(b"\n"),
        "the create body's framing differs"
    );

    let go = parse(&go_raw, "Go's created category");
    let rust = parse(&rust_raw, "our created category");
    let go_id = go["id"].as_str().expect("an id").to_owned();
    let rust_id = rust["id"].as_str().expect("an id").to_owned();
    assert_eq!(
        normalise(&go, &f.go.id, &[&go_id]),
        normalise(&rust, &f.rust.id, &[&rust_id]),
        "the created category differs:\n go: {go}\nrust: {rust}"
    );

    assert_eq!(rust["type"], "custom", "the request said favorites");
    assert_eq!(
        rust["collapsed"], false,
        "collapsed is not taken from the body"
    );
    assert_ne!(rust["id"], NOWHERE, "the id is minted");
    assert_eq!(rust_id.len(), 26, "a NewId, not a default-shaped id");
    assert_eq!(rust["muted"], true);
    assert_eq!(rust["sorting"], "alpha");
    assert_eq!(rust["display_name"], "SBW Custom");
    assert_eq!(
        rust["sort_order"], 10,
        "placed behind Favorites, and the answer is the patched value"
    );
    assert_eq!(
        rust["channel_ids"],
        serde_json::json!([f.charlie, f.bravo]),
        "the duplicate is removed and the non-member channel dropped, in request order"
    );

    // The row really is what was answered — read back through the server that wrote it.
    let (_, listing) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::GET,
        &f.rust.categories(&f.team_id),
        None,
        None,
    )
    .await;
    let listing = parse(&listing, "our listing");
    assert_eq!(
        listing["order"][1],
        rust_id.as_str(),
        "second in the order, behind Favorites: {listing}"
    );
    assert_eq!(listing["categories"][1]["channel_ids"], rust["channel_ids"]);
}

/// `display_name` is arbitrary user text and Go's `json.Marshal` escapes `<`, `>` and `&`.
/// `serde_json` does not, so a port using it differs by six bytes per character on a name a
/// client can trivially send.
#[tokio::test]
async fn a_display_name_with_html_characters_is_escaped_like_gos_marshal() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let body = |subject: &Subject| {
        serde_json::json!({
            "user_id": subject.id,
            "team_id": f.team_id,
            "display_name": "Q&A <b> \u{2028}",
            "channel_ids": [],
        })
    };
    let ((go_status, go_raw), (rust_status, rust_raw)) = create_on_both(&http, f, &body).await;
    assert_eq!(go_status, 200);
    assert_eq!(rust_status, 200);

    // The escaped forms, spelled out: `encoding/json` renders `&`, `<`, `>` and U+2028 as
    // `\u0026`, `\u003c`, `\u003e` and `\u2028`, in lower-case hex. `serde_json` passes the
    // first three through raw, so a port that used it fails here and nowhere else.
    const ESCAPED: &str = r"Q\u0026A \u003cb\u003e \u2028";
    let go_text = String::from_utf8_lossy(&go_raw).to_string();
    let rust_text = String::from_utf8_lossy(&rust_raw).to_string();
    assert!(
        go_text.contains(ESCAPED),
        "Go escapes these four characters: {go_text}"
    );
    assert!(
        rust_text.contains(ESCAPED),
        "and so must we, byte for byte: {rust_text}"
    );
    let go = parse(&go_raw, "Go");
    let rust = parse(&rust_raw, "ours");
    assert_eq!(
        normalise(&go, &f.go.id, &[go["id"].as_str().expect("an id")]),
        normalise(&rust, &f.rust.id, &[rust["id"].as_str().expect("an id")]),
        "and the decoded value agrees too"
    );

    // And the same name survives a read on both.
    let (_, go_listing) = send(
        &http,
        GO,
        &f.go.token,
        reqwest::Method::GET,
        &f.go.categories(&f.team_id),
        None,
        None,
    )
    .await;
    let (_, rust_listing) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::GET,
        &f.rust.categories(&f.team_id),
        None,
        None,
    )
    .await;
    assert!(String::from_utf8_lossy(&go_listing).contains(ESCAPED));
    assert!(
        String::from_utf8_lossy(&rust_listing).contains(ESCAPED),
        "the read route escapes too: {}",
        String::from_utf8_lossy(&rust_listing)
    );
}

/// A decode failure and a body whose ids disagree with the path are the **same** 400, because Go
/// tests them in one `if`.
#[tokio::test]
async fn a_bad_create_body_is_one_400_whatever_is_wrong_with_it() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;

    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "a foreign user_id",
            serde_json::json!({ "user_id": NOWHERE, "team_id": f.team_id }),
        ),
        (
            "a foreign team_id",
            serde_json::json!({ "user_id": "<subject>", "team_id": NOWHERE }),
        ),
        (
            "an absent user_id",
            serde_json::json!({ "team_id": f.team_id }),
        ),
    ];

    for (what, template) in cases {
        let body = |subject: &Subject| {
            let mut value = template.clone();
            if value["user_id"] == "<subject>" {
                value["user_id"] = serde_json::json!(subject.id);
            }
            value
        };
        let ((go_status, go_raw), (rust_status, rust_raw)) = create_on_both(&http, f, &body).await;
        assert_eq!(
            go_status,
            400,
            "{what}: {}",
            String::from_utf8_lossy(&go_raw)
        );
        assert_eq!(rust_status, go_status, "{what}");
        let body = assert_error_bodies_match_except_known_gaps(&go_raw, &rust_raw, what);
        assert_eq!(
            body["id"], "api.context.invalid_body_param.app_error",
            "{what}"
        );
    }

    // And a body that is not JSON at all takes the same branch.
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let (status, raw) = send_raw_body(
            &http,
            base,
            &subject.token,
            reqwest::Method::POST,
            &subject.categories(&f.team_id),
            "{not json",
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(
            parse(&raw, "the decode failure")["id"],
            "api.context.invalid_body_param.app_error"
        );
    }
}

// ---------------------------------------------------------------------------
// updateCategoriesForTeamForUser
// ---------------------------------------------------------------------------

/// Moving a channel from one category to another in one request, and the read-only fields.
#[tokio::test]
async fn updating_categories_moves_a_channel_out_of_its_old_category() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    // A custom category to move channels into.
    let ((_, go_raw), (_, rust_raw)) = create_on_both(&http, f, &|subject: &Subject| {
        serde_json::json!({
            "user_id": subject.id,
            "team_id": f.team_id,
            "display_name": "SBW Move",
            "channel_ids": [],
        })
    })
    .await;
    let go_custom = parse(&go_raw, "Go's category")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let rust_custom = parse(&rust_raw, "our category")["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let mut answers = Vec::new();
    for (base, subject, custom) in [(GO, &f.go, &go_custom), (RUST, &f.rust, &rust_custom)] {
        let channels = subject.default_category("channels", &f.team_id);
        let body = serde_json::json!([
            {
                "id": channels,
                "user_id": subject.id,
                "team_id": f.team_id,
                // Renaming a non-custom category is silently ignored.
                "display_name": "Renamed",
                "type": "channels",
                "sorting": "manual",
                "muted": true,
                "collapsed": true,
                "channel_ids": [f.charlie],
            },
            {
                "id": custom,
                "user_id": subject.id,
                "team_id": f.team_id,
                "display_name": "SBW Move",
                "type": "custom",
                "sorting": "",
                "muted": false,
                "collapsed": false,
                "channel_ids": [f.bravo],
            },
        ]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &subject.categories(&f.team_id),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert!(
            !raw.ends_with(b"\n"),
            "{base}: `w.Write` leaves no newline: {:?}",
            String::from_utf8_lossy(&raw)
        );
        answers.push((subject, custom.clone(), parse(&raw, "the update's answer")));
    }

    let (go_subject, go_custom_id, go_answer) = &answers[0];
    let (rust_subject, rust_custom_id, rust_answer) = &answers[1];
    assert_eq!(
        normalise(go_answer, &go_subject.id, &[go_custom_id.as_str()]),
        normalise(rust_answer, &rust_subject.id, &[rust_custom_id.as_str()]),
        "the update's answer differs:\n go: {go_answer}\nrust: {rust_answer}"
    );

    assert_eq!(
        rust_answer[0]["display_name"], "Channels",
        "DisplayName is read-only for a non-custom category"
    );
    assert_eq!(rust_answer[0]["sorting"], "manual", "sorting is writable");
    assert_eq!(rust_answer[0]["muted"], true, "and so is muted, here");
    assert_eq!(rust_answer[0]["collapsed"], true);
    // **The Channels category is not just what was named.** Joining a team auto-joins Town Square
    // and Off-Topic, and neither is filed in any category, so both arrive as orphans *after* the
    // explicit entry. Asserting the whole list here would be asserting the fixture, not the route;
    // what matters is that the explicit channel leads and that `bravo` has left.
    let channels_of = |value: &serde_json::Value| -> Vec<String> {
        value["channel_ids"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|id| id.as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let listed = channels_of(&rust_answer[0]);
    assert_eq!(
        listed.first().map(String::as_str),
        Some(f.charlie.as_str()),
        "the explicit channel comes first, before the orphans: {listed:?}"
    );
    assert!(
        !listed.contains(&f.bravo),
        "bravo moved to the custom category and must not still be here: {listed:?}"
    );
    assert!(
        !listed.contains(&f.outsider),
        "and a channel the subject is not in is never listed: {listed:?}"
    );
    assert_eq!(rust_answer[1]["channel_ids"], serde_json::json!([f.bravo]));

    // Now swap them, in one request. The channel must leave the category it was in.
    let channels = f.rust.default_category("channels", &f.team_id);
    let body = serde_json::json!([
        {
            "id": channels, "user_id": f.rust.id, "team_id": f.team_id, "type": "channels",
            "display_name": "Channels", "sorting": "", "muted": false, "collapsed": false,
            "channel_ids": [f.bravo],
        },
        {
            "id": rust_custom, "user_id": f.rust.id, "team_id": f.team_id, "type": "custom",
            "display_name": "SBW Move", "sorting": "", "muted": false, "collapsed": false,
            "channel_ids": [f.charlie],
        },
    ]);
    let (status, raw) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::PUT,
        &f.rust.categories(&f.team_id),
        Some(&body),
        None,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&raw));
    let swapped = parse(&raw, "the swap");
    let swapped_channels = channels_of(&swapped[0]);
    assert_eq!(
        swapped_channels.first().map(String::as_str),
        Some(f.bravo.as_str()),
        "{swapped_channels:?}"
    );
    assert!(
        !swapped_channels.contains(&f.charlie),
        "{swapped_channels:?}"
    );
    assert_eq!(swapped[1]["channel_ids"], serde_json::json!([f.charlie]));

    // And the swap really landed, read back through us.
    let (_, listing) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::GET,
        &f.rust.categories(&f.team_id),
        None,
        None,
    )
    .await;
    let listing = parse(&listing, "our listing");
    let by_id = |id: &str| {
        listing["categories"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|category| category["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} is missing from {listing}"))
    };
    let stored = channels_of(&by_id(&channels));
    assert_eq!(
        stored.first().map(String::as_str),
        Some(f.bravo.as_str()),
        "the explicit entry leads: {stored:?}"
    );
    assert!(
        !stored.contains(&f.charlie),
        "one channel each, so neither is in two categories: {stored:?}"
    );
    assert_eq!(
        by_id(&rust_custom)["channel_ids"],
        serde_json::json!([f.charlie])
    );
}

/// Moving a channel into and out of Favorites writes and deletes a `favorite_channel`
/// **preference** — the sidebar/preferences duality, observable through
/// `GET /api/v4/users/{id}/preferences`.
#[tokio::test]
async fn the_favorites_category_mirrors_itself_into_preferences() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let favourites_of = async |base: &str, subject: &Subject| -> Vec<String> {
        let (_, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &format!("/api/v4/users/{}/preferences", subject.id),
            None,
            None,
        )
        .await;
        let mut names: Vec<String> = parse(&raw, "the preferences")
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|preference| preference["category"] == "favorite_channel")
            .filter(|preference| preference["value"] == "true")
            .filter_map(|preference| preference["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    };

    let star = async |base: &str, subject: &Subject, channels: serde_json::Value| {
        let body = serde_json::json!([{
            "id": subject.default_category("favorites", &f.team_id),
            "user_id": subject.id,
            "team_id": f.team_id,
            "type": "favorites",
            "display_name": "Favorites",
            "sorting": "",
            "muted": false,
            "collapsed": false,
            "channel_ids": channels,
        }]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &subject.categories(&f.team_id),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        parse(&raw, "the favourites update")
    };

    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        assert!(
            favourites_of(base, subject).await.is_empty(),
            "{base}: the subject starts with nothing starred"
        );

        star(base, subject, serde_json::json!([f.bravo, f.charlie])).await;
        let mut expected = vec![f.bravo.clone(), f.charlie.clone()];
        expected.sort();
        assert_eq!(
            favourites_of(base, subject).await,
            expected,
            "{base}: filing a channel under Favorites stars it"
        );

        star(base, subject, serde_json::json!([f.bravo])).await;
        assert_eq!(
            favourites_of(base, subject).await,
            vec![f.bravo.clone()],
            "{base}: dropping one un-stars exactly that one"
        );

        star(base, subject, serde_json::json!([])).await;
        assert!(
            favourites_of(base, subject).await.is_empty(),
            "{base}: emptying Favorites un-stars everything"
        );

        // **The other branch, on its own.** Star a channel, then update *only* the Channels
        // category with it — Favorites is not in the request at all, so the un-starring can only
        // come from the non-Favorites branch, which deletes the **request's** channels from
        // `Preferences`. Every step above would pass with that branch removed.
        star(base, subject, serde_json::json!([f.charlie])).await;
        assert_eq!(favourites_of(base, subject).await, vec![f.charlie.clone()]);
        let body = serde_json::json!([{
            "id": subject.default_category("channels", &f.team_id),
            "user_id": subject.id,
            "team_id": f.team_id,
            "type": "channels",
            "display_name": "Channels",
            "sorting": "",
            "muted": false,
            "collapsed": false,
            "channel_ids": [f.charlie],
        }]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &subject.categories(&f.team_id),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert!(
            favourites_of(base, subject).await.is_empty(),
            "{base}: filing a starred channel under a non-Favorites category un-stars it, even \
             though Favorites was not named in the request"
        );
    }
}

/// The store's not-found — a user with no categories at all — is a **404** on the create route,
/// where every other refusal on it is a 400 or a 403.
///
/// Only reachable by deleting the rows, since a `GET` recreates them; a `POST` does not.
#[tokio::test]
async fn creating_a_category_for_a_user_with_no_categories_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connects to Postgres");

    let mut answers = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        sqlx::query(
            "DELETE FROM sidebarchannels WHERE categoryid IN
                (SELECT id FROM sidebarcategories WHERE userid = $1 AND teamid = $2)",
        )
        .bind(&subject.id)
        .bind(&f.team_id)
        .execute(&pool)
        .await
        .expect("clears the channels");
        sqlx::query("DELETE FROM sidebarcategories WHERE userid = $1 AND teamid = $2")
            .bind(&subject.id)
            .bind(&f.team_id)
            .execute(&pool)
            .await
            .expect("clears the categories");

        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::POST,
            &subject.categories(&f.team_id),
            Some(&serde_json::json!({
                "user_id": subject.id, "team_id": f.team_id,
                "display_name": "SBW Homeless", "channel_ids": [],
            })),
            None,
        )
        .await;
        assert_eq!(
            status,
            404,
            "{base}: the store's `categories not found`: {}",
            String::from_utf8_lossy(&raw)
        );
        answers.push(raw);
    }
    let body = assert_error_bodies_match_except_known_gaps(
        &answers[0],
        &answers[1],
        "creating with no categories",
    );
    assert_eq!(body["id"], "app.channel.sidebar_categories.app_error");
}

/// The per-category gate is a **400**, not the 403 every other permission refusal on these routes
/// produces — and it refuses the whole request, not the one entry.
#[tokio::test]
async fn a_category_belonging_to_someone_else_is_a_400_on_the_collection_route() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    // Each subject names *the other subject's* Favorites category alongside its own.
    let mut bodies = Vec::new();
    for (subject, other) in [(&f.go, &f.rust), (&f.rust, &f.go)] {
        bodies.push(serde_json::json!([
            {
                "id": subject.default_category("favorites", &f.team_id),
                "user_id": subject.id, "team_id": f.team_id, "type": "favorites",
                "display_name": "Favorites", "channel_ids": [f.bravo],
            },
            {
                "id": other.default_category("favorites", &f.team_id),
                "user_id": subject.id, "team_id": f.team_id, "type": "favorites",
                "display_name": "Favorites", "channel_ids": [],
            },
        ]));
    }

    let (go_status, go_raw) = send(
        &http,
        GO,
        &f.go.token,
        reqwest::Method::PUT,
        &f.go.categories(&f.team_id),
        Some(&bodies[0]),
        None,
    )
    .await;
    let (rust_status, rust_raw) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::PUT,
        &f.rust.categories(&f.team_id),
        Some(&bodies[1]),
        None,
    )
    .await;

    assert_eq!(
        go_status,
        400,
        "Go answers invalid-param, not forbidden: {}",
        String::from_utf8_lossy(&go_raw)
    );
    assert_eq!(rust_status, go_status);
    let body =
        assert_error_bodies_match_except_known_gaps(&go_raw, &rust_raw, "a foreign category");
    assert_eq!(body["id"], "api.context.invalid_body_param.app_error");

    // And the *first* entry — which was legitimate — was not applied.
    let (_, raw) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::GET,
        &f.rust.categories(&f.team_id),
        None,
        None,
    )
    .await;
    let listing = parse(&raw, "our listing");
    assert_eq!(
        listing["categories"][0]["channel_ids"],
        serde_json::json!([]),
        "the whole request is refused, not just the offending entry: {listing}"
    );

    // A category id that names nothing takes the same branch, because the gate fetches the row.
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let body =
            serde_json::json!([{ "id": NOWHERE, "user_id": subject.id, "team_id": f.team_id }]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &subject.categories(&f.team_id),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(
            parse(&raw, "the refusal")["id"],
            "api.context.invalid_body_param.app_error"
        );
    }
}

/// A body of `null` is accepted and answers `[]` — `json.Decode` into a `[]*T` leaves the slice
/// nil without erroring, so neither loop runs and the store's own empty literal is marshalled.
#[tokio::test]
async fn a_null_categories_body_answers_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;

    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let (status, raw) = send_raw_body(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &subject.categories(&f.team_id),
            "null",
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(
            String::from_utf8_lossy(&raw),
            "[]",
            "{base} should answer an empty array with no newline"
        );
    }
}

// ---------------------------------------------------------------------------
// updateCategoryForTeamForUser
// ---------------------------------------------------------------------------

/// The singular route answers one object, takes its id from the **path**, and its refusals are
/// 403s where the collection route's are 400s.
#[tokio::test]
async fn the_singular_update_takes_its_id_from_the_path_and_refuses_with_403() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let mut answers = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let favourites = subject.default_category("favorites", &f.team_id);
        let body = serde_json::json!({
            // Ignored: the path wins.
            "id": NOWHERE,
            "user_id": subject.id,
            "team_id": f.team_id,
            "type": "favorites",
            "display_name": "Renamed",
            "sorting": "recent",
            "muted": true,
            "collapsed": true,
            "channel_ids": [f.charlie],
        });
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &format!("{}/{favourites}", subject.categories(&f.team_id)),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert!(!raw.ends_with(b"\n"), "{base}: no trailing newline");
        let value = parse(&raw, "the singular update");
        assert!(value.is_object(), "{base} answers one object, not an array");
        assert_eq!(
            value["id"],
            favourites.as_str(),
            "{base}: the path's id wins"
        );
        answers.push((subject, value));
    }
    assert_eq!(
        normalise(&answers[0].1, &answers[0].0.id, &[]),
        normalise(&answers[1].1, &answers[1].0.id, &[]),
        "the singular update's answer differs"
    );
    assert_eq!(answers[1].1["display_name"], "Favorites");
    assert_eq!(answers[1].1["muted"], true);

    // The other subject's category, named in our own path: `SessionHasPermissionToCategory`
    // compares the row's `UserId` against **both** the session and the path, so this is a 403.
    let foreign = f.go.default_category("favorites", &f.team_id);
    let body = serde_json::json!({
        "user_id": f.rust.id, "team_id": f.team_id, "channel_ids": [],
    });
    let (go_status, go_raw) = send(
        &http,
        GO,
        &f.rust.token,
        reqwest::Method::PUT,
        &format!("{}/{foreign}", f.rust.categories(&f.team_id)),
        Some(&body),
        None,
    )
    .await;
    let (rust_status, rust_raw) = send(
        &http,
        RUST,
        &f.rust.token,
        reqwest::Method::PUT,
        &format!("{}/{foreign}", f.rust.categories(&f.team_id)),
        Some(&body),
        None,
    )
    .await;
    assert_eq!(
        go_status,
        403,
        "the singular route refuses with forbidden, unlike the collection route's 400: {}",
        String::from_utf8_lossy(&go_raw)
    );
    assert_eq!(rust_status, go_status);
    let body =
        assert_error_bodies_match_except_known_gaps(&go_raw, &rust_raw, "a foreign category");
    assert_eq!(body["id"], "api.context.permissions.app_error");
}

/// The Direct Messages category ignores `muted` and stores no channels; every other type takes
/// `muted` from the request.
#[tokio::test]
async fn the_dm_category_ignores_muted_where_the_others_do_not() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let mut answers = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let dms = subject.default_category("direct_messages", &f.team_id);
        let body = serde_json::json!({
            "user_id": subject.id, "team_id": f.team_id, "type": "direct_messages",
            "display_name": "Direct Messages", "sorting": "alpha",
            "muted": true, "collapsed": true, "channel_ids": [f.bravo],
        });
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &format!("{}/{dms}", subject.categories(&f.team_id)),
            Some(&body),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        answers.push((subject, parse(&raw, "the DM update")));
    }
    assert_eq!(
        normalise(&answers[0].1, &answers[0].0.id, &[]),
        normalise(&answers[1].1, &answers[1].0.id, &[]),
        "the DM category's answer differs"
    );

    let dms = &answers[1].1;
    assert_eq!(
        dms["muted"], false,
        "muted is read-only for this type alone"
    );
    assert_eq!(dms["collapsed"], true, "collapsed is still writable");
    assert_eq!(dms["sorting"], "alpha");
    assert_eq!(
        dms["channel_ids"],
        serde_json::json!([]),
        "the public channel named in the request is not filed here, and the subject has no DMs"
    );
}

// ---------------------------------------------------------------------------
// updateCategoryOrderForTeamForUser
// ---------------------------------------------------------------------------

/// The `PUT` and the `GET` on `/order` disagree about the trailing newline, and the `PUT` echoes
/// the de-duplicated list.
#[tokio::test]
async fn the_order_write_and_the_order_read_are_framed_differently() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = format!("{}/order", subject.categories(&f.team_id));
        let order = serde_json::json!([
            subject.default_category("direct_messages", &f.team_id),
            subject.default_category("channels", &f.team_id),
            subject.default_category("favorites", &f.team_id),
        ]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            Some(&order),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert!(
            !raw.ends_with(b"\n"),
            "{base}: `w.Write(ArrayToJSON(...))` leaves no newline: {:?}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            parse(&raw, "the echoed order"),
            order,
            "{base}: the write echoes the list it stored"
        );

        let (status, read) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &path,
            None,
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            read.ends_with(b"\n"),
            "{base}: the GET is encoder-framed and does have one: {:?}",
            String::from_utf8_lossy(&read)
        );
        assert_eq!(
            parse(&read, "the stored order"),
            order,
            "{base}: and the order really moved"
        );
        assert_eq!(
            String::from_utf8_lossy(&read)
                .trim_end_matches('\n')
                .as_bytes(),
            raw.as_slice(),
            "{base}: the two bodies differ only in the newline"
        );
    }
}

/// The wrong length is a **500** and a foreign id a **400**, and the length guard runs first.
#[tokio::test]
async fn a_short_order_is_a_500_and_a_foreign_id_is_a_400() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = format!("{}/order", subject.categories(&f.team_id));

        // Two of three: the length guard, which is a bare `errors.New`.
        let short = serde_json::json!([
            subject.default_category("favorites", &f.team_id),
            subject.default_category("channels", &f.team_id),
        ]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            Some(&short),
            None,
        )
        .await;
        assert_eq!(
            status,
            500,
            "{base}: a short list is a server error, not a 400: {}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            parse(&raw, "the length refusal")["id"],
            "app.channel.sidebar_categories.app_error"
        );

        // The right length, with the *other* subject's category in place of one of ours. The
        // handler's own per-id permission loop catches this before the store does, so it is a 400
        // with the invalid-param id rather than the store's.
        let other = if base == GO { &f.rust } else { &f.go };
        let foreign = serde_json::json!([
            subject.default_category("favorites", &f.team_id),
            subject.default_category("channels", &f.team_id),
            other.default_category("channels", &f.team_id),
        ]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            Some(&foreign),
            None,
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(
            parse(&raw, "the permission refusal")["id"],
            "api.context.invalid_body_param.app_error"
        );

        // **A duplicate is removed before anything else looks at the list**, which turns three
        // ids into two and therefore into the length refusal — a *500*. Without the dedup the
        // list would be the right length, the omitted category would fail the store's membership
        // guard, and the answer would be a 400. The two are distinguishable, which is the only
        // reason this case is worth sending.
        let duplicated = serde_json::json!([
            subject.default_category("favorites", &f.team_id),
            subject.default_category("channels", &f.team_id),
            subject.default_category("channels", &f.team_id),
        ]);
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            Some(&duplicated),
            None,
        )
        .await;
        assert_eq!(
            status,
            500,
            "{base}: the de-duplicated list is two long, not three: {}",
            String::from_utf8_lossy(&raw)
        );

        // And the stored order did not move.
        let (_, read) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &path,
            None,
            None,
        )
        .await;
        assert_eq!(
            parse(&read, "the stored order").as_array().map(Vec::len),
            Some(3),
            "{base}: none of the three refusals wrote anything"
        );
    }
}

/// `/order` is the one route in the family whose decode failure is `api.payload.parse.error`, and
/// a body of `null` is not a decode failure at all.
#[tokio::test]
async fn the_order_route_has_its_own_parse_error_and_accepts_null() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = format!("{}/order", subject.categories(&f.team_id));

        let (status, raw) = send_raw_body(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            "{}",
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&raw));
        let body = parse(&raw, "the parse failure");
        assert_eq!(
            body["id"], "api.payload.parse.error",
            "{base}: not the invalid_body_param its siblings answer"
        );
        assert!(
            body["params"].is_null(),
            "{base}: and no Name param, unlike SetInvalidParam: {body}"
        );

        // `null` decodes to a nil slice with **no error**, so it reaches the store, whose length
        // check then fails against the subject's three categories.
        let (status, raw) = send_raw_body(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &path,
            "null",
        )
        .await;
        assert_eq!(
            status,
            500,
            "{base}: null is not a parse error; it is a length mismatch: {}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            parse(&raw, "the length refusal")["id"],
            "app.channel.sidebar_categories.app_error"
        );
    }
}

// ---------------------------------------------------------------------------
// deleteCategoryForTeamForUser
// ---------------------------------------------------------------------------

/// A default category cannot be deleted; a custom one can, and its channels come back as orphans.
#[tokio::test]
async fn deleting_a_default_category_is_a_400_and_a_customs_channels_survive() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    for kind in ["favorites", "channels", "direct_messages"] {
        let (go_status, go_raw) = send(
            &http,
            GO,
            &f.go.token,
            reqwest::Method::DELETE,
            &format!(
                "{}/{}",
                f.go.categories(&f.team_id),
                f.go.default_category(kind, &f.team_id)
            ),
            None,
            None,
        )
        .await;
        let (rust_status, rust_raw) = send(
            &http,
            RUST,
            &f.rust.token,
            reqwest::Method::DELETE,
            &format!(
                "{}/{}",
                f.rust.categories(&f.team_id),
                f.rust.default_category(kind, &f.team_id)
            ),
            None,
            None,
        )
        .await;
        assert_eq!(
            go_status,
            400,
            "{kind}: {}",
            String::from_utf8_lossy(&go_raw)
        );
        assert_eq!(rust_status, go_status, "{kind}");
        let body = assert_error_bodies_match_except_known_gaps(&go_raw, &rust_raw, kind);
        assert_eq!(
            body["id"], "app.channel.sidebar_categories.app_error",
            "{kind}: the store's generic id, not a delete-specific one"
        );
    }

    // A custom category holding a channel, then deleted.
    let ((_, go_raw), (_, rust_raw)) = create_on_both(&http, f, &|subject: &Subject| {
        serde_json::json!({
            "user_id": subject.id, "team_id": f.team_id,
            "display_name": "SBW Doomed", "channel_ids": [f.charlie],
        })
    })
    .await;
    let go_id = parse(&go_raw, "Go's category")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let rust_id = parse(&rust_raw, "our category")["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    for (base, subject, id) in [(GO, &f.go, &go_id), (RUST, &f.rust, &rust_id)] {
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::DELETE,
            &format!("{}/{id}", subject.categories(&f.team_id)),
            None,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(
            String::from_utf8_lossy(&raw),
            r#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no trailing newline"
        );

        let (_, listing) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &subject.categories(&f.team_id),
            None,
            None,
        )
        .await;
        let listing = parse(&listing, "the listing");
        assert_eq!(
            listing["order"].as_array().map(Vec::len),
            Some(3),
            "{base}: back to the three defaults"
        );
        let channels = listing["categories"]
            .as_array()
            .expect("an array")
            .iter()
            .find(|category| category["type"] == "channels")
            .cloned()
            .expect("a Channels category");
        assert!(
            channels["channel_ids"]
                .as_array()
                .expect("an array")
                .contains(&serde_json::json!(f.charlie)),
            "{base}: the deleted category's channel is an orphan of Channels now: {channels}"
        );
    }
}

/// `me` in the path is resolved **before** the body's `user_id` is compared against it, so a body
/// carrying the caller's real id and a path saying `me` is accepted — and a body saying `me` is
/// not.
///
/// `RequireUserId` (web/context.go:301) substitutes the session's id into `c.Params.UserId` and
/// the handler then compares that, never the raw segment. A port that compared the segment would
/// reject every request the webapp's `me` shorthand produces.
#[tokio::test]
async fn the_me_alias_is_resolved_before_the_bodys_user_id_is_checked() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let mut created = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = format!("/api/v4/users/me/teams/{}/channels/categories", f.team_id);

        // The real id in the body, `me` in the path: accepted.
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::POST,
            &path,
            Some(&serde_json::json!({
                "user_id": subject.id, "team_id": f.team_id,
                "display_name": "SBW Me", "channel_ids": [],
            })),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        let value = parse(&raw, "the created category");
        assert_eq!(
            value["user_id"],
            subject.id.as_str(),
            "{base}: the answer carries the resolved id, not the literal `me`"
        );

        // `me` in the body: the comparison is against the resolved id, so this is a 400.
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::POST,
            &path,
            Some(&serde_json::json!({
                "user_id": "me", "team_id": f.team_id,
                "display_name": "SBW Not Me", "channel_ids": [],
            })),
            None,
        )
        .await;
        assert_eq!(
            status,
            400,
            "{base}: `me` is not substituted into the body: {}",
            String::from_utf8_lossy(&raw)
        );
        assert_eq!(
            parse(&raw, "the refusal")["id"],
            "api.context.invalid_body_param.app_error"
        );

        // And the delete route resolves it too.
        let id = value["id"].as_str().expect("an id");
        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::DELETE,
            &format!("{path}/{id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        assert_eq!(String::from_utf8_lossy(&raw), r#"{"status":"OK"}"#);
        created.push(id.to_owned());
    }

    assert_ne!(created[0], created[1], "each server minted its own id");
}

// ---------------------------------------------------------------------------
// The websocket events
// ---------------------------------------------------------------------------

/// All four sidebar events, their payload shapes, and the `omit_connection_id` that is **not**
/// there.
#[tokio::test]
async fn the_four_sidebar_events_agree_and_omit_no_connection() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let mut go_socket = SocketProbe::connect(GO, &f.go.token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &f.rust.token).await;

    // A `Connection-Id` on every request. Go's sidebar events pass `""` as `omitConnectionId`, so
    // unlike a draft or preference write the header must **not** reach the broadcast.
    let connection = "mmrssbwconnection123456789";

    let mut minted = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        let path = subject.categories(&f.team_id);
        let (_, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::POST,
            &path,
            Some(&serde_json::json!({
                "user_id": subject.id, "team_id": f.team_id,
                "display_name": "SBW Events", "channel_ids": [f.bravo],
            })),
            Some(connection),
        )
        .await;
        let id = parse(&raw, "the created category")["id"]
            .as_str()
            .expect("an id")
            .to_owned();

        send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &format!("{path}/{id}"),
            Some(&serde_json::json!({
                "user_id": subject.id, "team_id": f.team_id, "type": "custom",
                // An `&`, so the event payload's own escaping is observable: it is a
                // marshalled string, and `json.Marshal` escapes inside it too.
                "display_name": "SBW Events & Renamed", "channel_ids": [f.bravo],
            })),
            Some(connection),
        )
        .await;

        send(
            &http,
            base,
            &subject.token,
            reqwest::Method::PUT,
            &format!("{path}/order"),
            Some(&serde_json::json!([
                id,
                subject.default_category("favorites", &f.team_id),
                subject.default_category("channels", &f.team_id),
                subject.default_category("direct_messages", &f.team_id),
            ])),
            Some(connection),
        )
        .await;

        send(
            &http,
            base,
            &subject.token,
            reqwest::Method::DELETE,
            &format!("{path}/{id}"),
            None,
            Some(connection),
        )
        .await;

        minted.push(id);
    }

    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;

    let go_minted = minted[0].as_str();
    let rust_minted = minted[1].as_str();

    for event in [
        "sidebar_category_created",
        "sidebar_category_updated",
        "sidebar_category_order_updated",
        "sidebar_category_deleted",
    ] {
        let go_events = go_socket.events_named(event);
        let rust_events = rust_socket.events_named(event);
        assert_eq!(
            go_events.len(),
            1,
            "Go published one {event}: {:?}",
            go_socket.raw
        );
        assert_eq!(
            rust_events.len(),
            1,
            "we published one {event}: {:?}",
            rust_socket.raw
        );

        let go_event = normalise(&go_events[0], &f.go.id, &[go_minted]);
        let rust_event = normalise(&rust_events[0], &f.rust.id, &[rust_minted]);
        assert_eq!(
            go_event["broadcast"], rust_event["broadcast"],
            "{event}: the addressing differs"
        );
        assert_eq!(
            rust_event["broadcast"]["team_id"], f.team_id,
            "{event} is addressed to the team"
        );
        assert_eq!(
            rust_event["broadcast"]["user_id"], "<user>",
            "{event} is addressed to the user, which is what the hub reads first"
        );
        assert_eq!(
            rust_event["broadcast"]["channel_id"], "",
            "{event} carries no channel"
        );
        assert_eq!(
            rust_event["broadcast"]["omit_connection_id"], "",
            "{event} must NOT carry the Connection-Id header — Go passes \"\" here, unlike the \
             draft and preference writes"
        );

        // `sidebar_category_updated`'s payload is a marshalled *string*; the others are plain
        // values. Re-parse the string so the comparison is structural rather than byte-for-byte
        // on two different sets of ids.
        let payload = |event_value: &serde_json::Value| -> serde_json::Value {
            if let Some(raw) = event_value["data"]["updatedCategories"].as_str() {
                return serde_json::from_str(raw).expect("the payload is JSON inside a string");
            }
            event_value["data"].clone()
        };
        assert_eq!(
            payload(&go_event),
            payload(&rust_event),
            "{event}: the payload differs\n go: {go_event}\nrust: {rust_event}"
        );
    }

    // The three payload conventions, named.
    let created = &rust_socket.events_named("sidebar_category_created")[0];
    assert_eq!(
        created["data"]["category_id"], rust_minted,
        "the create event carries the id as a plain string"
    );
    let deleted = &rust_socket.events_named("sidebar_category_deleted")[0];
    assert_eq!(deleted["data"]["category_id"], rust_minted);
    let order = &rust_socket.events_named("sidebar_category_order_updated")[0];
    assert!(
        order["data"]["order"].is_array(),
        "the order event carries an array, not a string: {order}"
    );
    const ESCAPED_AMP: &str = r"SBW Events \u0026 Renamed";
    let updated = &rust_socket.events_named("sidebar_category_updated")[0];
    let payload = updated["data"]["updatedCategories"]
        .as_str()
        .unwrap_or_else(|| {
            panic!("the update event carries a JSON string, not an array: {updated}")
        });
    // Inside the string too: `json.Marshal` escapes `&` wherever it appears, and the payload was
    // marshalled by the same call. A port using `serde_json::to_string` here writes a raw `&` and
    // the two servers' events differ by five bytes.
    assert!(
        payload.contains(ESCAPED_AMP),
        "the payload is Go-marshalled, escaping included: {payload}"
    );
    let go_payload =
        go_socket.events_named("sidebar_category_updated")[0]["data"]["updatedCategories"]
            .as_str()
            .expect("Go's payload is a string")
            .to_owned();
    assert!(
        go_payload.contains(ESCAPED_AMP),
        "and Go really does escape it: {go_payload}"
    );
}

// ---------------------------------------------------------------------------
// Routing and the lazy migration
// ---------------------------------------------------------------------------

/// A subject whose `SidebarCategories` rows are missing gets them created **inside the `GET`** —
/// by whichever server answered. Constructed by deleting the rows, which no API call does.
#[tokio::test]
async fn a_get_on_a_user_with_no_categories_creates_the_three_defaults() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let http = client();
    let f = fixture().await;
    reset(&http, f).await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connects to Postgres");

    let mut listings = Vec::new();
    for (base, subject) in [(GO, &f.go), (RUST, &f.rust)] {
        sqlx::query(
            "DELETE FROM sidebarchannels WHERE categoryid IN
                (SELECT id FROM sidebarcategories WHERE userid = $1 AND teamid = $2)",
        )
        .bind(&subject.id)
        .bind(&f.team_id)
        .execute(&pool)
        .await
        .expect("clears the channels");
        sqlx::query("DELETE FROM sidebarcategories WHERE userid = $1 AND teamid = $2")
            .bind(&subject.id)
            .bind(&f.team_id)
            .execute(&pool)
            .await
            .expect("clears the categories");

        let (status, raw) = send(
            &http,
            base,
            &subject.token,
            reqwest::Method::GET,
            &subject.categories(&f.team_id),
            None,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&raw));
        let value = parse(&raw, "the migrated listing");
        assert_eq!(
            value["order"],
            serde_json::json!([
                subject.default_category("favorites", &f.team_id),
                subject.default_category("channels", &f.team_id),
                subject.default_category("direct_messages", &f.team_id),
            ]),
            "{base}: the three deterministic ids, in Favorites/Channels/DMs order"
        );
        listings.push((subject, value));
    }

    // `send` already asserted `x-mmrs-served-by: rust` for the Rust leg, which is the claim that
    // matters: this case used to be forwarded to Go.
    assert_eq!(
        normalise(&listings[0].1, &listings[0].0.id, &[]),
        normalise(&listings[1].1, &listings[1].0.id, &[]),
        "the migrated sidebar differs"
    );
}

/// All eight methods on the three paths are ours now. Registering a method takes the path out of
/// `Router::fallback`, so the guard against a *missing* registration is that nothing answers 405
/// and nothing is served by Go.
#[tokio::test]
async fn all_eight_category_routes_are_served_locally() {
    if !stack_enabled() {
        return;
    }
    let _lock = SIDEBAR.lock().await;
    let http = client();
    let f = fixture().await;
    let base = f.rust.categories(&f.team_id);

    // Bodies chosen so each request is refused rather than applied, except the two GETs.
    let cases: Vec<(reqwest::Method, String, Option<serde_json::Value>)> = vec![
        (reqwest::Method::GET, base.clone(), None),
        (reqwest::Method::GET, format!("{base}/order"), None),
        (reqwest::Method::GET, format!("{base}/{NOWHERE}"), None),
        (
            reqwest::Method::POST,
            base.clone(),
            Some(serde_json::json!({ "user_id": NOWHERE })),
        ),
        (
            reqwest::Method::PUT,
            base.clone(),
            Some(serde_json::json!([{ "id": NOWHERE }])),
        ),
        (
            reqwest::Method::PUT,
            format!("{base}/order"),
            Some(serde_json::json!([NOWHERE])),
        ),
        (
            reqwest::Method::PUT,
            format!("{base}/{NOWHERE}"),
            Some(serde_json::json!({ "user_id": NOWHERE })),
        ),
        (reqwest::Method::DELETE, format!("{base}/{NOWHERE}"), None),
    ];

    for (method, path, body) in cases {
        // `send` asserts `x-mmrs-served-by: rust` for us.
        let (status, raw) = send(
            &http,
            RUST,
            &f.rust.token,
            method.clone(),
            &path,
            body.as_ref(),
            None,
        )
        .await;
        assert_ne!(
            status,
            405,
            "{method} {path} answered method-not-allowed: {}",
            String::from_utf8_lossy(&raw)
        );
    }

    // A method gorilla never registered here still falls through to Go — the `partially_migrated`
    // fallback is load-bearing even now that every registered method is ours.
    let response = http
        .post(format!("{RUST}{base}/order"))
        .header("Authorization", format!("Bearer {}", f.admin_token))
        .json(&serde_json::json!([]))
        .send()
        .await
        .expect("the proxy answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST /order is not a Go route, so it must be forwarded for Go's own 405"
    );
}
