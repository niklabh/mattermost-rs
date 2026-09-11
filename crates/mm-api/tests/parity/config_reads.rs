//! Cross-server parity for the three `/api/v4/config` **reads**.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity config_reads
//! ```
//!
//! # Why this suite is the whole oracle for these routes
//!
//! There is no `fixtures/` file here and there cannot usefully be one. The input is the live
//! `Configurations` document — 27 KB carrying the database password and the public-link salt —
//! so a golden copy of it could not be committed, and a redacted copy would stop covering
//! precisely the fields `Sanitize` exists for. The parity oracle is therefore the running Go
//! server: same database, same row, same environment, byte-compared.
//!
//! That makes `scripts/mm-api-env.sh` part of the apparatus rather than a convenience. All three
//! routes answer *about* the environment overlay, so the two processes have to be launched with
//! the same `MM_*` overrides or they disagree for a reason that has nothing to do with the port.
//! One override cannot be reconciled — `MM_FILESETTINGS_DIRECTORY` is rooted at whichever
//! checkout launched each server — and it is exempted by name below, loudly, in one place.

use crate::common;

use common::{GO, RUST, assert_served_by_rust, client, fetch_both, go_minted_token, stack_enabled};

const CONFIG: &str = "/api/v4/config";
const CLIENT_CONFIG: &str = "/api/v4/config/client";
const ENVIRONMENT_CONFIG: &str = "/api/v4/config/environment";

/// `model.FakeSetting` (config.go:92).
const FAKE_SETTING: &str = "********************************";

/// The one key the two servers cannot agree on, and why.
///
/// `scripts/go-server.sh` and `scripts/mm-api-env.sh` each set `MM_FILESETTINGS_DIRECTORY` to a
/// path under **their own checkout**, because each server reads files from its own tree. The
/// setting is genuinely different between the processes, so this is a property of the test
/// apparatus rather than of the port — and it is the only one, which is the useful part: every
/// other one of the 47 sections is compared verbatim.
const PER_CHECKOUT_KEYS: &[(&str, &str)] = &[("FileSettings", "Directory")];

fn body_of(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("the body is JSON")
}

/// `GET /config/client` with **no** token — `APIHandler`, so Go answers rather than refusing, and
/// the map is the limited one every client reads before it can log in.
async fn anonymous_client_config(base: &str) -> (Vec<u8>, reqwest::header::HeaderMap) {
    let response = client()
        .get(format!("{base}{CLIENT_CONFIG}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{CLIENT_CONFIG} is unreachable: {e}"));
    assert_eq!(
        response.status(),
        200,
        "{base}{CLIENT_CONFIG} answers an anonymous caller"
    );
    let headers = response.headers().clone();
    (
        response.bytes().await.expect("body reads").to_vec(),
        headers,
    )
}

/// **The highest-value comparison in this file.** Every Mattermost client calls this before it
/// renders anything, and an anonymous caller gets the *limited* map — a different key set from
/// the authenticated one, built by a different Go function.
///
/// Byte-compared, not value-compared: `json.NewEncoder(w).Encode` sorts the map's keys and
/// appends a newline, and a client reading the body length would see both.
#[tokio::test]
async fn the_limited_client_config_matches_go_byte_for_byte() {
    if !stack_enabled() {
        return;
    }

    let (go, _) = anonymous_client_config(GO).await;
    let (rust, headers) = anonymous_client_config(RUST).await;
    assert_served_by_rust(&headers, CLIENT_CONFIG);

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rust),
        "the limited client config must match Go exactly"
    );
    // A guard against the comparison passing on two empty bodies, and against the limited map
    // silently becoming the full one.
    let map = body_of(&go);
    let map = map.as_object().expect("a flat object");
    assert!(map.len() > 100, "the limited map is a hundred-odd keys");
    assert!(
        !map.contains_key("MaxPostSize"),
        "MaxPostSize is computed only for an authenticated caller — this is the full map"
    );
}

/// The authenticated map: `GenerateClientConfig` on top of the limited one, plus the four
/// computed properties `ClientConfigWithComputed` adds from the database.
#[tokio::test]
async fn the_client_config_for_a_session_matches_go_byte_for_byte() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go, rust) = fetch_both(&client, &token, CLIENT_CONFIG).await;

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rust),
        "the client config must match Go exactly"
    );

    // The computed half, named rather than implied: each of these comes from a different table
    // and each has its own failure fallback, so a comparison that passed with all four missing
    // would prove nothing about any of them.
    let map = body_of(&go);
    for key in [
        "NoAccounts",
        "MaxPostSize",
        "UpgradedFromTE",
        "InstallationDate",
        "SchemaVersion",
        "AsymmetricSigningPublicKey",
        "DiagnosticId",
    ] {
        assert!(
            map.get(key).is_some(),
            "{key} is computed per request and must be present"
        );
    }
}

/// A session and no session are answered by **different Go functions**, so a port that built one
/// map and returned it twice would pass the two tests above and still be wrong. This is the test
/// that separates them.
#[tokio::test]
async fn the_two_client_configs_differ_and_the_full_one_contains_the_limited_one() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, rust_full) = fetch_both(&client, &token, CLIENT_CONFIG).await;
    let (rust_limited, _) = anonymous_client_config(RUST).await;

    let full = body_of(&rust_full);
    let limited = body_of(&rust_limited);
    let (full, limited) = (
        full.as_object().expect("object"),
        limited.as_object().expect("object"),
    );

    assert!(
        full.len() > limited.len(),
        "the authenticated map is strictly larger: {} vs {}",
        full.len(),
        limited.len()
    );
    for key in limited.keys() {
        assert!(full.contains_key(key), "{key} is missing from the full map");
    }
    // `CWSURL` is written twice: `""` as a licence default in the limited map, then from
    // `CloudSettings` in the full one. It is the one key whose *value* proves the two maps were
    // built by two different functions rather than filtered from one.
    assert_eq!(limited["CWSURL"], "", "the limited map's licence default");
    assert_ne!(
        full["CWSURL"], "",
        "the full map overwrites CWSURL from CloudSettings"
    );
}

/// `GET /config` — the whole document, sanitized. Compared section by section so the one
/// irreconcilable key can be named rather than the whole assertion weakened.
#[tokio::test]
async fn the_sanitized_config_matches_go_except_the_per_checkout_file_directory() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go, rust) = fetch_both(&client, &token, CONFIG).await;

    let mut go = body_of(&go);
    let mut rust = body_of(&rust);
    for (section, key) in PER_CHECKOUT_KEYS {
        for document in [&mut go, &mut rust] {
            document
                .get_mut(section)
                .and_then(|s| s.as_object_mut())
                .and_then(|s| s.remove(*key))
                .unwrap_or_else(|| panic!("{section}.{key} is in the document"));
        }
    }

    assert_eq!(
        go, rust,
        "the sanitized configuration must match Go on every section but the exempted keys"
    );
    assert!(
        go.as_object().expect("object").len() > 40,
        "the document is 47-odd sections; a smaller answer means something was dropped"
    );
    assert!(
        go.get("FeatureFlags").is_some(),
        "FeatureFlags is absent from the persisted row and supplied by SetDefaults — a response \
         without it means load_model_config stopped filling it"
    );
}

/// **The test that would catch a secret reaching a client.**
///
/// Asserted on *our* body rather than on the pair, because "Rust matches Go" would still pass if
/// both leaked. The data source is the one that matters: it carries the database password, and
/// `getConfig` passes nil `SanitizeOptions`, so it is fully masked rather than url-redacted.
#[tokio::test]
async fn no_secret_survives_into_the_config_response() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, rust) = fetch_both(&client, &token, CONFIG).await;
    let document = body_of(&rust);

    for (section, key) in [
        ("SqlSettings", "DataSource"),
        ("SqlSettings", "AtRestEncryptKey"),
        ("FileSettings", "PublicLinkSalt"),
        ("ElasticsearchSettings", "Password"),
        ("ServiceSettings", "SplitKey"),
        ("CacheSettings", "RedisPassword"),
    ] {
        let value = document
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{section}.{key} is a string in the response"));
        assert_eq!(value, FAKE_SETTING, "{section}.{key} reached the client");
    }

    let whole = serde_json::to_string(&document).expect("re-encodes");
    assert!(
        !whole.contains("mmuser_password"),
        "the stack's database password appears in the /config body"
    );
}

/// `GET /config/environment` — a sparse tree of `true`s naming the settings an environment
/// variable overrode. `w.Write` rather than an encoder, so **no trailing newline**.
#[tokio::test]
async fn the_environment_config_matches_go_byte_for_byte() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;
    let (go, rust) = fetch_both(&client, &token, ENVIRONMENT_CONFIG).await;

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rust),
        "the environment config must match Go exactly — if this fails on a variable name, \
         scripts/mm-api-env.sh and scripts/go-server.sh have drifted apart"
    );
    assert!(
        !go.ends_with(b"\n"),
        "getEnvironmentConfig uses w.Write, not an encoder"
    );

    // The asymmetry Go's own walk produces, asserted so a "tidier" port cannot quietly remove it:
    // `MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD` is set for both servers and **is** applied
    // to the running config, but `*FeatureFlags` is a pointer and therefore a leaf, so this route
    // never reports a per-flag variable.
    let map = body_of(&go);
    assert!(
        map.get("FeatureFlags").is_none(),
        "FeatureFlags is a pointer and never appears in the environment map"
    );
    assert_eq!(
        map.get("ServiceSettings").and_then(|s| s.get("SiteURL")),
        Some(&serde_json::Value::Bool(true)),
        "MM_SERVICESETTINGS_SITEURL is set for both servers"
    );
}

/// A caller with no system-console read permission gets **our** 403, not Go's — the permission
/// check runs before the decision to forward, so this is the port's own refusal and its body must
/// match byte for byte, `detail` and all.
#[tokio::test]
async fn a_plain_user_is_refused_identically() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;
    let user = common::create_plain_user(&client, &admin, &team, "cfgread").await;
    let token = common::login_plain_user(&client, "cfgread").await;

    let get = async |base: &str| -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
        let response = client
            .get(format!("{base}{CONFIG}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        (
            status,
            headers,
            response.bytes().await.expect("body").to_vec(),
        )
    };
    let (go_status, _, go_body) = get(GO).await;
    let (rust_status, rust_headers, rust_body) = get(RUST).await;
    assert_served_by_rust(&rust_headers, CONFIG);

    assert_eq!(go_status, 403);
    assert_eq!(rust_status, 403);
    // `message` is Go's translation of the id and `request_id` is per request; `id`,
    // `status_code` and `detailed_error` must agree. See [D-092] for the i18n gap.
    let go_json = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rust_body,
        "GET /config as a plain user",
    );
    assert_eq!(go_json["id"], "api.context.permissions.app_error");

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// `?remove_defaults=true` needs `SetDefaults()` to diff against and is forwarded — so the answer
/// is still Go's and still correct. Asserted rather than assumed: a handler that quietly served
/// its own unfiltered answer would look right to every other test here.
#[tokio::test]
async fn the_filter_query_parameters_are_forwarded_to_go() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    for query in ["remove_defaults=true", "remove_masked=1"] {
        let path = format!("{CONFIG}?{query}");
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path} must be forwarded: RemoveDefaults and RemoveMasked are not ported"
        );
    }

    // …and a value Go's `strconv.ParseBool` would reject is `false`, which is **not** a reason to
    // forward. `?remove_masked=yes` is an ordinary request and we answer it.
    let path = format!("{CONFIG}?remove_masked=yes");
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(response.status(), 200);
    assert_served_by_rust(response.headers(), &path);
}

/// The two routes that set `Cache-Control` and the one that does not.
#[tokio::test]
async fn the_cache_control_header_matches_go_on_each_route() {
    if !stack_enabled() {
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    for path in [CONFIG, CLIENT_CONFIG, ENVIRONMENT_CONFIG] {
        let header_of = async |base: &str| -> Option<String> {
            client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("reachable")
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            header_of(GO).await,
            header_of(RUST).await,
            "Cache-Control differs on {path}"
        );
    }
}
