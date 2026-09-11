//! Cross-server parity for the *source* of configuration, rather than for a route.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity config_source
//! ```
//!
//! # What this file is for
//!
//! `mm_app::config::Config::load` reads the configuration in two layers — the `Configurations`
//! document the Go server persists, then the `MM_<SECTION>_<SETTING>` environment on top — and the
//! order is load-bearing rather than stylistic. That claim rests on one fact about Go:
//!
//! > `Store.Load` builds two configs and persists the one *without* the environment applied
//! > (`s.backingStore.Set(loadedCfgNoEnv)`, config/store.go:321).
//!
//! Everything else in this change is downstream of that. If it were false — if Go persisted the
//! environment-applied config — then re-applying the overlay would be harmless but pointless, and
//! a future reader would be entitled to delete [`Config::apply_env`] as redundant. If it is true
//! and we ever *stopped* applying the overlay, this server would disagree with the one beside it
//! on precisely the settings an operator bothered to change, and no route test would notice,
//! because both servers would still be internally consistent.
//!
//! So it is measured here, against the two servers, rather than trusted to a line number.
//!
//! # The oracle
//!
//! `GET /api/v4/config/client?format=old` is **unauthenticated** and returns the configuration the
//! Go server is actually *running* on. The `Configurations` row is what it *persisted*. A setting
//! that differs between the two is an environment override, and finding one proves the exclusion.
//!
//! The comparison here is Go-running against Go-persisted. `RUST` appears only in the last test,
//! which since 2026-09-11 confirms that the route is *served* rather than forwarded — it used to
//! confirm the opposite.

use crate::common;

use common::{GO, RUST, client, stack_enabled};

const CLIENT_CONFIG: &str = "/api/v4/config/client?format=old";

/// Read the active configuration document straight out of the shared database.
async fn persisted_document() -> serde_json::Value {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://mmuser:mmuser_password@localhost:5432/mattermost".into());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to the shared Postgres");

    let raw: String = sqlx::query_scalar("SELECT value FROM configurations WHERE active")
        .fetch_one(&pool)
        .await
        .expect(
            "an active row in Configurations — is MM_CONFIG pointed at this database? \
             See docker-compose.yml",
        );
    serde_json::from_str(&raw).expect("the row is a model.Config document")
}

async fn running_client_config(base: &str) -> serde_json::Value {
    let response = client()
        .get(format!("{base}{CLIENT_CONFIG}"))
        .send()
        .await
        .expect("the client config is reachable");
    assert_eq!(
        response.status(),
        200,
        "{base}{CLIENT_CONFIG} answers unauthenticated"
    );
    response.json().await.expect("the body is JSON")
}

/// **The fact the layering rests on.** Go persists the configuration *before* the environment
/// overlay, so a setting overridden by environment differs between the running server and the row.
///
/// `SiteURL` is the witness because `docker-compose.yml` sets `MM_SERVICESETTINGS_SITEURL` and
/// nothing else in the stack writes it: the running server reports `http://localhost:8065`, and
/// the persisted document still carries Go's default of `""`.
///
/// If this test ever fails because the two now agree, check *why* before deleting it. An operator
/// who moved `SiteURL` out of the environment and into the stored config would make it agree
/// innocently — but so would Go changing which config it persists, and that would silently make
/// `Config::apply_env` wrong rather than merely unnecessary.
#[tokio::test]
async fn the_persisted_document_excludes_the_environment_overlay() {
    if !stack_enabled() {
        return;
    }

    let running = running_client_config(GO).await;
    let persisted = persisted_document().await;

    let running_site_url = running
        .get("SiteURL")
        .and_then(|v| v.as_str())
        .expect("the client config carries SiteURL");
    let persisted_site_url = persisted
        .get("ServiceSettings")
        .and_then(|s| s.get("SiteURL"))
        .and_then(|v| v.as_str())
        .expect("the document carries ServiceSettings.SiteURL");

    // **`GO`, not a literal.** `scripts/go-server.sh` sets `MM_SERVICESETTINGS_SITEURL` to its own
    // stack's port, so a hardcoded `:8065` here passes on stack 0 and fails on every other stack
    // for a reason that has nothing to do with the route.
    assert_eq!(
        running_site_url, GO,
        "go-server.sh sets MM_SERVICESETTINGS_SITEURL; the running server should report it"
    );
    assert_eq!(
        persisted_site_url, "",
        "the persisted document must NOT carry the environment overlay (config/store.go:321). \
         If Go has started persisting the env-applied config, mm_app::config's layering is no \
         longer merely redundant — it is wrong, and Config::apply_env needs rethinking"
    );
    assert_ne!(
        running_site_url, persisted_site_url,
        "running and persisted must differ, or this test proves nothing"
    );
}

/// The settings `mm_app::config` models agree between the running Go server and the document we
/// read them from — i.e. none of them is being overridden by environment in this deployment.
///
/// This is the other half of the pair. The test above proves the overlay is *excluded* from the
/// document; this one proves that for the settings we actually read, excluding it currently costs
/// nothing — so any parity failure elsewhere in the suite is not secretly a configuration
/// mismatch. The moment someone adds an `MM_SERVICESETTINGS_*` override to the stack for a
/// modelled setting, this fails and says which one.
#[tokio::test]
async fn no_modelled_setting_is_overridden_by_environment_in_this_stack() {
    if !stack_enabled() {
        return;
    }

    let running = running_client_config(GO).await;
    let persisted = persisted_document().await;

    // The client config flattens sections and stringifies booleans (`format=old`), which is why
    // each pair is named rather than derived: these are the modelled settings that appear in it.
    let pairs: [(&str, &str, &str); 5] = [
        ("EnableCustomEmoji", "ServiceSettings", "EnableCustomEmoji"),
        ("PostPriority", "ServiceSettings", "PostPriority"),
        (
            "EnableIncomingWebhooks",
            "ServiceSettings",
            "EnableIncomingWebhooks",
        ),
        ("ShowFullName", "PrivacySettings", "ShowFullName"),
        ("ShowEmailAddress", "PrivacySettings", "ShowEmailAddress"),
    ];

    for (client_key, section, key) in pairs {
        let Some(running_value) = running.get(client_key).and_then(|v| v.as_str()) else {
            // The client config is a curated subset and it changes between versions. A setting
            // that is not in it cannot be cross-checked, which is a gap in the oracle rather than
            // a failure — say so instead of asserting on `None`.
            eprintln!("note: {client_key} is not in the client config; not cross-checked");
            continue;
        };
        let persisted_value = persisted
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| panic!("{section}.{key} is absent from the persisted document"));

        assert_eq!(
            running_value,
            persisted_value.to_string(),
            "{section}.{key} differs between the running Go server and the persisted document, \
             so it is set by environment. mm-api applies the MM_ overlay and will agree — but \
             this stack no longer proves that the document alone is sufficient, and any parity \
             failure touching this setting should be read in that light"
        );
    }
}

/// Both servers answer the client-config route identically, which is what lets the tests above
/// use Go's answer as an oracle without caring which server they asked.
///
/// **It used to be forwarded and is now served here** (`mm_api::config::get_client_config`), so
/// this no longer holds by construction and the header is checked: without that, a regression in
/// the handler would silently be measured against itself. The byte-level comparison lives in the
/// `config_reads` suite; this one keeps the oracle above honest.
#[tokio::test]
async fn the_client_config_route_agrees_whichever_server_answers() {
    if !stack_enabled() {
        return;
    }

    let response = client()
        .get(format!("{RUST}{CLIENT_CONFIG}"))
        .send()
        .await
        .expect("the client config is reachable");
    common::assert_served_by_rust(response.headers(), CLIENT_CONFIG);

    let (go, rust) = (
        running_client_config(GO).await,
        running_client_config(RUST).await,
    );
    assert_eq!(go, rust, "config/client must agree on both servers");
}
