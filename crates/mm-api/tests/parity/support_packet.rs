//! Cross-server parity for `GET /api/v4/system/support_packet` past its licence gate.
//!
//! ```sh
//! scripts/parity.sh --test parity support_packet
//! ```
//!
//! # The pair
//!
//! The licensed Go oracle (`scripts/go-licensed.sh`) against an mm-api of this suite's own
//! (`common::licensed_rust_with`), because the shared licensed mm-api cannot serve the packet:
//! it does not know Go's plugin directory, so it cannot prove Go runs no plugin and forwards —
//! which [`the_shared_licensed_pair_forwards_while_go_may_run_a_plugin`] pins. This one is given
//! `MM_GO_PLUGIN_DIRECTORY` (the oracle's, empty) and Go's own `MM_CONFIG` and data source, so
//! `store_type` and `SqlSettings.DataSource` are the same strings on both sides.
//!
//! # What is compared, file by file
//!
//! `mm_app::support_packet`'s module docs say which bytes are Go's and which are the process's.
//! The database-derived files are compared **bracketed**: Go, Rust, Go again, and a file counts
//! only when Go's two answers agree — so a suite writing concurrently in a full run cannot fail
//! this one, and cannot make it pass by accident either. Each Go packet takes five seconds (its
//! CPU profile), so the bracket is at most three rounds.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::common;

use common::{client, go_minted_token, stack_enabled};

/// The suite's own licensed mm-api; see `second_server_ports`.
const SUPPORT_PACKET_RUST_PORT: u16 = 8122;

/// The three files only a Go runtime writes (`runtime/pprof`), absent from ours by design.
const GO_RUNTIME_FILES: [&str; 3] = ["heap.prof", "goroutines", "cpu.prof"];

/// The order this port writes the files in: Go's declaration order, which is one of the orders
/// Go's two map ranges can produce.
const RUST_ORDER: [&str; 10] = [
    "metadata.yaml",
    "stats.yaml",
    "jobs.yaml",
    "permissions.yaml",
    "plugins.json",
    "database_schema.yaml",
    "diagnostics.yaml",
    "sanitized_config.json",
    "mattermost.log",
    "warning.txt",
];

struct Packet {
    status: u16,
    headers: reqwest::header::HeaderMap,
    entries: Vec<(String, Vec<u8>)>,
    raw: Vec<u8>,
}

impl Packet {
    fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn file(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.as_slice())
    }

    fn text(&self, name: &str) -> String {
        String::from_utf8_lossy(self.file(name).unwrap_or_else(|| panic!("no {name}"))).into_owned()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

/// The entries of a zip, in central-directory order, each inflated and checked against its CRC.
/// Enough of the format for what both writers produce: Deflate entries, sizes in the central
/// directory, no zip64.
pub(crate) fn read_zip(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read as _;
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    let u32_at = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };
    assert!(bytes.len() >= 22, "not a zip: {} bytes", bytes.len());
    let end = bytes.len() - 22;
    assert_eq!(
        u32_at(end),
        0x0605_4b50,
        "no end-of-central-directory record"
    );
    let count = u16_at(end + 10);
    let mut at = u32_at(end + 16);
    let mut entries = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(at), 0x0201_4b50, "a central directory header");
        let method = u16_at(at + 10);
        let crc = u32_at(at + 16) as u32;
        let compressed = u32_at(at + 20);
        let name_len = u16_at(at + 28);
        let extra_len = u16_at(at + 30);
        let comment_len = u16_at(at + 32);
        let local = u32_at(at + 42);
        let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).expect("a name");
        assert_eq!(method, 8, "{name} is Deflate, as archive/zip writes it");
        let data_at = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let mut body = Vec::new();
        flate2::read::DeflateDecoder::new(&bytes[data_at..data_at + compressed])
            .read_to_end(&mut body)
            .expect("inflates");
        let mut check = flate2::Crc::new();
        check.update(&body);
        assert_eq!(check.sum(), crc, "{name}'s CRC");
        entries.push((name, body));
        at += 46 + name_len + extra_len + comment_len;
    }
    entries
}

async fn fetch(client: &reqwest::Client, base: &str, token: &str, query: &str) -> Packet {
    let response = client
        .get(format!("{base}/api/v4/system/support_packet{query}"))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} answers: {e}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let raw = response.bytes().await.expect("a body").to_vec();
    let entries = if status == 200 {
        read_zip(&raw)
    } else {
        Vec::new()
    };
    Packet {
        status,
        headers,
        entries,
        raw,
    }
}

/// The oracle's run directory's plugin directory, which it scans and which holds no bundle.
fn oracle_plugin_dir() -> String {
    let run = common::stack_run_dir();
    let name = run
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a run directory name")
        .replacen("mmroot", "mmlic", 1);
    run.with_file_name(name)
        .join("plugins")
        .to_string_lossy()
        .into_owned()
}

/// `scripts/go-licensed.sh`'s `DSN`: the database URL with Go's two parameters.
fn oracle_dsn() -> String {
    format!(
        "{}?sslmode=disable&connect_timeout=10",
        std::env::var("DATABASE_URL").expect("parity.sh sets DATABASE_URL")
    )
}

/// Held by every test that asks the oracle for a packet. Go's CPU profile is process-global, so
/// two packets at once on one Go server give the second a `cpu profiling already in use` warning
/// and no `cpu.prof` — a Go-side artefact of this suite's own concurrency.
static GO_PACKETS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static OWN_PAIR: tokio::sync::OnceCell<(common::LicensedPair, common::SecondServer)> =
    tokio::sync::OnceCell::const_new();

/// The suite's licensed pair, started once per test binary — every test here shares it, as the
/// shared licensed pair is shared, because two starts on one port kill each other.
async fn own_pair() -> &'static common::LicensedPair {
    let (pair, _server) = OWN_PAIR
        .get_or_init(|| async {
            let plugins = oracle_plugin_dir();
            let dsn = oracle_dsn();
            // The oracle's spelling of the shared file store — the same directory without the
            // `crates/mm-api/../..` the harness's own path carries.
            let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(std::path::Path::parent)
                .expect("the repository root");
            let run = common::stack_run_dir();
            let run_name = run.file_name().and_then(|n| n.to_str()).expect("a run dir");
            let data = format!(
                "{}/",
                repo.join("reference/.build")
                    .join(run_name)
                    .join("data")
                    .display()
            );
            common::licensed_rust_with(
                SUPPORT_PACKET_RUST_PORT,
                &[
                    ("MM_FILESETTINGS_DIRECTORY", data.as_str()),
                    ("MM_GO_PLUGIN_DIRECTORY", plugins.as_str()),
                    ("MM_CONFIG", dsn.as_str()),
                    ("MM_SQLSETTINGS_DATASOURCE", dsn.as_str()),
                ],
            )
            .await
        })
        .await;
    pair
}

/// The paths at which two JSON documents differ, for a failure message a reader can act on.
fn json_differences(go: &Json, rs: &Json, path: &str, out: &mut Vec<String>) {
    match (go, rs) {
        (Json::Object(a), Json::Object(b)) => {
            for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                let here = format!("{path}.{key}");
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => json_differences(x, y, &here, out),
                    (x, y) => out.push(format!("{here}: Go {x:?} / Rust {y:?}")),
                }
            }
        }
        (a, b) if a != b => out.push(format!("{path}: Go {a} / Rust {b}")),
        _ => {}
    }
}

/// A top-level YAML sequence of mappings, split into its items — `database_schema.yaml`'s tables,
/// which Go writes in map order.
fn table_blocks(yaml: &str) -> BTreeSet<String> {
    let Some((_, tables)) = yaml.split_once("\ntables:\n") else {
        return BTreeSet::new();
    };
    tables
        .split("\n- ")
        // The last table carries the document's final newline, and which table is last is Go's
        // map order — so every block is compared without it.
        .map(|block| {
            block
                .trim_start_matches("- ")
                .trim_end_matches('\n')
                .to_owned()
        })
        .collect()
}

/// `permissions.yaml` without the roles other suites plant (`mmrs_*`) — see the comparison.
fn without_fixture_roles(yaml: &str) -> String {
    let mut out = String::new();
    for (index, item) in yaml.split("\n- id: ").enumerate() {
        if index > 0 && item.contains("\n  name: mmrs_") {
            // An item runs to the next `- id:`; the last role's item also carries `schemes:`.
            if let Some(rest) = item.find("\nschemes:").map(|at| &item[at..]) {
                out.push_str(rest);
            }
            continue;
        }
        if index > 0 {
            out.push_str("\n- id: ");
        }
        out.push_str(item);
    }
    out
}

/// The keys of `diagnostics.yaml` whose values are the process's or the moment's, not the
/// server's: masked, while the key, its position and its comment are still compared.
const PROCESS_VALUES: [&str; 25] = [
    "process_id",
    "started_at",
    "host_started_at",
    "open_file_descriptors",
    "max_file_descriptors",
    "go_version",
    "master_connections",
    "master_connections_in_use",
    "master_connections_idle",
    "master_pool_wait_count",
    "master_pool_wait_duration_ms",
    "master_connections_closed_max_idle",
    "master_connections_closed_max_lifetime",
    "cache_hit_ratio",
    "deadlocks",
    "temp_files",
    "temp_bytes_mb",
    "rollbacks",
    "idle_in_transaction_count",
    "longest_query_duration_seconds",
    "waiting_for_lock_count",
    "posts_dead_tuples",
    "posts_last_autovacuum",
    "available_mb",
    "connections",
];

fn mask_diagnostics(yaml: &str) -> Vec<String> {
    yaml.lines()
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            let Some((key, rest)) = line.trim_start().split_once(": ") else {
                return line.to_owned();
            };
            if !PROCESS_VALUES.contains(&key) {
                return line.to_owned();
            }
            let comment = rest.split_once(" #").map(|(_, c)| c).unwrap_or("");
            format!("{}{key}: <process> #{comment}", " ".repeat(indent))
        })
        .collect()
}

/// `warning.txt`'s points, or `None` when the packet has none.
///
/// Whether a stack produces the log-path warning at all depends on **where its Go was launched**:
/// `config.ValidateLogFilePath` resolves symlinks on the file but not on the logging root, so a
/// server started through a worktree's symlinked `reference/.build` finds its own log "outside"
/// the root and one started from the main checkout does not. The two servers agree either way.
fn warnings(packet: &Packet) -> Option<BTreeSet<String>> {
    packet
        .file("warning.txt")
        .map(|_| warning_points(&packet.text("warning.txt")))
}

/// `warning.txt`'s points, with the log file's path — each server's own run directory —
/// replaced, so the two lists say the same thing in the same words.
pub(crate) fn warning_points(text: &str) -> BTreeSet<String> {
    text.split("\n\t* ")
        .skip(1)
        .map(|point| {
            let point = point.trim_end_matches('\n');
            if point.starts_with("log file path ") && point.contains("outside allowed logging") {
                "log file path <run>/logs/mattermost.log is outside allowed logging directory"
                    .to_owned()
            } else {
                point.to_owned()
            }
        })
        .collect()
}

fn top_level_keys(json: &str) -> Vec<String> {
    json.lines()
        .filter(|l| l.starts_with("    \""))
        .filter_map(|l| l.trim_start().split('"').nth(1).map(str::to_owned))
        .collect()
}

/// The whole packet, file by file, against Go's.
#[tokio::test]
async fn the_licensed_packet_matches_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _serial = GO_PACKETS.lock().await;
    let pair = own_pair().await;

    let mut compared: BTreeMap<&str, bool> = [
        "stats.yaml",
        "jobs.yaml",
        "permissions.yaml",
        "plugins.json",
        "database_schema.yaml",
        "sanitized_config.json",
    ]
    .into_iter()
    .map(|f| (f, false))
    .collect();

    let mut last: Option<(Packet, Packet)> = None;
    for _round in 0..3 {
        let g1 = fetch(&client, &pair.go, &admin, "").await;
        let r = fetch(&client, &pair.rust, &admin, "").await;
        let g2 = fetch(&client, &pair.go, &admin, "").await;
        assert_eq!(
            (g1.status, r.status),
            (200, 200),
            "Go {:?} / Rust {:?}",
            String::from_utf8_lossy(&g1.raw[..g1.raw.len().min(300)]),
            String::from_utf8_lossy(&r.raw[..r.raw.len().min(300)])
        );
        assert_eq!(
            r.header("x-mmrs-served-by"),
            "rust",
            "the packet is built here"
        );

        for (name, done) in compared.iter_mut() {
            if *done {
                continue;
            }
            let (a, b, ours) = (g1.text(name), g2.text(name), r.text(name));
            match *name {
                "database_schema.yaml" => {
                    if table_blocks(&a) == table_blocks(&b) {
                        assert_eq!(
                            a.split("\ntables:\n").next(),
                            ours.split("\ntables:\n").next(),
                            "the collation and encoding"
                        );
                        let (go, rs) = (table_blocks(&a), table_blocks(&ours));
                        assert!(
                            go.len() > 50,
                            "the stack's schema has its tables: {}",
                            go.len()
                        );
                        let only_go: Vec<_> = go.difference(&rs).take(3).collect();
                        let only_rs: Vec<_> = rs.difference(&go).take(3).collect();
                        assert!(
                            only_go.is_empty() && only_rs.is_empty(),
                            "tables differ:\nGo only {only_go:#?}\nRust only {only_rs:#?}"
                        );
                        *done = true;
                    }
                }
                // Counters over the whole database move under a full run's concurrent writers,
                // so each is bracketed on its own: ours must lie between Go's two readings.
                "stats.yaml" => {
                    let counts = |t: &str| -> BTreeMap<String, i64> {
                        t.lines()
                            .filter_map(|l| l.split_once(": "))
                            .map(|(k, v)| (k.to_owned(), v.parse().unwrap_or(i64::MIN)))
                            .collect()
                    };
                    let (ca, cb, co) = (counts(&a), counts(&b), counts(&ours));
                    assert_eq!(co.len(), 14, "fourteen counters: {ours}");
                    assert_eq!(
                        co.keys().collect::<Vec<_>>(),
                        ca.keys().collect::<Vec<_>>(),
                        "the counters"
                    );
                    let key_order = |t: &str| -> Vec<String> {
                        t.lines()
                            .filter_map(|l| l.split_once(": "))
                            .map(|(k, _)| k.to_owned())
                            .collect()
                    };
                    assert_eq!(key_order(&ours), key_order(&a), "the counters' order");
                    for (key, value) in &co {
                        let (x, y) = (ca[key], cb[key]);
                        assert!(
                            (x.min(y)..=x.max(y)).contains(value),
                            "{key}: ours {value}, Go {x} then {y}"
                        );
                    }
                    *done = true;
                }
                // Other suites' role fixtures (`mmrs_*`, left by `parity::roles`) are dropped:
                // the licensed oracle memoises `ChannelHigherScopedPermissions` per role-name set
                // (localcachelayer/role_layer.go:125), and after a full run it merges a scheme
                // that suite has since deleted — a stale Go cache `caches/invalidate` does not
                // reach, measured 2026-09-23. Every other role is compared byte for byte.
                "permissions.yaml" => {
                    let (a, b, ours) = (
                        without_fixture_roles(&a),
                        without_fixture_roles(&b),
                        without_fixture_roles(&ours),
                    );
                    if a == b {
                        assert!(a.matches("\n- id: ").count() > 10, "the roles are there");
                        assert_eq!(ours, a, "{name}");
                        *done = true;
                    }
                }
                "sanitized_config.json" => {
                    if a == b {
                        let go: Json = serde_json::from_str(&a).expect("Go's config");
                        let rs: Json = serde_json::from_str(&ours).expect("our config");
                        let mut differences = Vec::new();
                        json_differences(&go, &rs, "", &mut differences);
                        assert!(
                            differences.is_empty(),
                            "the sanitised configuration differs:\n{}",
                            differences.join("\n")
                        );
                        assert_eq!(top_level_keys(&ours), top_level_keys(&a), "section order");
                        assert_eq!(
                            top_level_keys(&a).last().map(String::as_str),
                            Some("FeatureFlags")
                        );
                        assert!(a.contains("postgres://****:****@"), "partially redacted");
                        *done = true;
                    }
                }
                _ => {
                    if a == b {
                        assert_eq!(ours, a, "{name}");
                        *done = true;
                    }
                }
            }
        }
        last = Some((g1, r));
        if compared.values().all(|d| *d) {
            break;
        }
    }
    let undecided: Vec<_> = compared
        .iter()
        .filter(|(_, d)| !**d)
        .map(|(n, _)| *n)
        .collect();
    assert!(
        undecided.is_empty(),
        "Go's answer never held still for {undecided:?} — a concurrent writer; rerun alone"
    );
    let (go, rs) = last.expect("a round ran");

    // The file list: Go's less its runtime profiles, in our fixed order.
    let go_names: BTreeSet<&str> = go
        .names()
        .into_iter()
        .filter(|n| !GO_RUNTIME_FILES.contains(n))
        .collect();
    let rs_names: BTreeSet<&str> = rs.names().into_iter().collect();
    assert_eq!(rs_names, go_names, "the same files, less Go's profiles");
    for profile in GO_RUNTIME_FILES {
        assert!(go.file(profile).is_some(), "Go writes {profile}");
    }
    let order: Vec<&str> = RUST_ORDER
        .iter()
        .copied()
        .filter(|n| rs_names.contains(n))
        .collect();
    assert_eq!(rs.names(), order);

    // metadata.yaml: every line but the moment it was generated.
    let strip = |t: &str| -> Vec<String> {
        t.lines()
            .filter(|l| !l.starts_with("generated_at: "))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        strip(&rs.text("metadata.yaml")),
        strip(&go.text("metadata.yaml"))
    );
    assert!(
        rs.text("metadata.yaml")
            .contains("license_id: mmrslicensedoracle")
    );

    // diagnostics.yaml: every key, comment and server-level value.
    let (gd, rd) = (
        mask_diagnostics(&go.text("diagnostics.yaml")),
        mask_diagnostics(&rs.text("diagnostics.yaml")),
    );
    assert_eq!(rd, gd, "diagnostics.yaml, process values masked");

    // warning.txt: the same points, the log path aside.
    assert_eq!(warnings(&rs), warnings(&go), "warning.txt");

    // The response around it.
    for header in [
        "content-type",
        "cache-control",
        "x-content-type-options",
        "x-frame-options",
        "content-security-policy",
    ] {
        assert_eq!(rs.header(header), go.header(header), "{header}");
    }
    let disposition = |p: &Packet| {
        let d = p.header("content-disposition").to_owned();
        let name = d
            .split("filename=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_default()
            .to_owned();
        (d, name)
    };
    let ((gd, gn), (rd, rn)) = (disposition(&go), disposition(&rs));
    let shape = |n: &str| {
        n.strip_prefix("mm_support_packet_mattermost-rs_")
            .and_then(|r| r.strip_suffix(".zip"))
            .is_some_and(|t| t.len() == 16 && t.as_bytes()[10] == b'T' && t.as_bytes()[13] == b'-')
    };
    assert!(shape(&gn) && shape(&rn), "Go {gd} / Rust {rd}");
    assert_eq!(
        rd,
        format!("attachment;filename=\"{rn}\"; filename*=UTF-8''{rn}")
    );
    assert_eq!(
        gd,
        format!("attachment;filename=\"{gn}\"; filename*=UTF-8''{gn}")
    );
}

/// `basic_server_logs=false` leaves the log out, and with it the log-path warning when the stack
/// produces one (see [`warnings`]).
#[tokio::test]
async fn basic_server_logs_false_leaves_the_log_out() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _serial = GO_PACKETS.lock().await;
    let pair = own_pair().await;
    for query in [
        "?basic_server_logs=false",
        "?basic_server_logs=false&plugin_packets=x&plugin_packets=y",
    ] {
        let go = fetch(&client, &pair.go, &admin, query).await;
        let rs = fetch(&client, &pair.rust, &admin, query).await;
        assert_eq!((go.status, rs.status), (200, 200), "{query}");
        for p in [&go, &rs] {
            assert!(p.file("mattermost.log").is_none(), "{query}");
        }
        assert_eq!(
            rs.file("warning.txt").is_some(),
            go.file("warning.txt").is_some(),
            "{query}: Go {:?}",
            go.file("warning.txt").map(String::from_utf8_lossy)
        );
    }
    // Anything but the exact word keeps the logs — `FormValue(…) == "false"`.
    let go = fetch(&client, &pair.go, &admin, "?basic_server_logs=False").await;
    let rs = fetch(&client, &pair.rust, &admin, "?basic_server_logs=False").await;
    assert_eq!(warnings(&rs), warnings(&go), "?basic_server_logs=False");
}

/// A user without `manage_system` is the permission error on both, before the licence is read.
#[tokio::test]
async fn a_non_admin_is_refused_as_go_refuses() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _serial = GO_PACKETS.lock().await;
    let pair = own_pair().await;
    let team = common::create_team(&client, &admin, "supportpkt").await;
    let user = common::create_plain_user(&client, &admin, &team, "supportpkt").await;
    let go = fetch(&client, &pair.go, &user.token, "").await;
    let rs = fetch(&client, &pair.rust, &user.token, "").await;
    assert_eq!((go.status, rs.status), (403, 403));
    let body =
        common::assert_error_bodies_match_except_known_gaps(&go.raw, &rs.raw, "support packet");
    assert_eq!(body["id"], "api.context.permissions.app_error");
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// The shared licensed mm-api does not know Go's plugin directory, so it cannot prove Go runs
/// no plugin, and the packet — whose plugin list, sanitised plugin settings and
/// `GenerateSupportData` files would all be Go's — goes to Go whole.
#[tokio::test]
async fn the_shared_licensed_pair_forwards_while_go_may_run_a_plugin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _serial = GO_PACKETS.lock().await;
    let pair = common::licensed().await;
    let rs = fetch(&client, &pair.rust, &admin, "?basic_server_logs=false").await;
    assert_eq!(rs.status, 200);
    assert_eq!(rs.header("x-mmrs-served-by"), "go");
    assert!(
        rs.file("heap.prof").is_some(),
        "Go's packet, profiles and all"
    );
}

/// Unlicensed, the plain pair refuses with Go's 403 — the gate this route always had.
#[tokio::test]
async fn the_unlicensed_refusal_is_unchanged() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let go = fetch(&client, common::GO, &admin, "").await;
    let rs = fetch(&client, common::RUST, &admin, "").await;
    assert_eq!((go.status, rs.status), (403, 403));
    let body = common::assert_error_bodies_match_except_known_gaps(&go.raw, &rs.raw, "unlicensed");
    assert_eq!(body["id"], "api.no_license");
}

/// The splitting and masking this suite compares through, without a stack.
#[test]
fn the_comparison_helpers_do_what_they_say() {
    let roles =
        "roles:\n- id: a\n  name: keep\n- id: b\n  name: mmrs_x\nschemes:\n- id: s\n  name: y\n";
    assert_eq!(
        without_fixture_roles(roles),
        "roles:\n- id: a\n  name: keep\nschemes:\n- id: s\n  name: y\n"
    );

    let yaml = "database_collation: C\ntables:\n- name: a\n  columns: []\n- name: b\n  columns:\n  - name: x\n    is_nullable: false\n";
    let blocks = table_blocks(yaml);
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(blocks.iter().all(|b| !b.ends_with('\n')), "{blocks:?}");
    assert!(blocks.iter().any(|b| b.starts_with("name: b")));
    let masked = mask_diagnostics("server:\n  process_id: 1 # x\n  os: linux\n");
    assert_eq!(
        masked,
        ["server:", "  process_id: <process> # x", "  os: linux"]
    );
    let points = warning_points(
        "2 errors occurred:\n\t* a\n\t* log file path /x/logs/mattermost.log is outside allowed logging directory: y\n\n",
    );
    assert_eq!(points.len(), 2);
    assert_eq!(
        top_level_keys("{\n    \"A\": {\n        \"B\": 1\n    },\n    \"C\": 2\n}"),
        ["A", "C"]
    );
}
