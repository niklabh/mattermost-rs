//! The hook recorder's configuration script: the plugin API's configuration and licence methods,
//! each written down with what the host answered, for `parity::plugin_hooks`' configuration
//! tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`CONFIG_SCRIPT`], from inside `MessageWillBePosted`, like
//! the KV script. The answers that carry a Go map — the two configurations, `GetPluginConfig` and
//! the licence — are decoded as [`gobwire::Dynamic`], not into the generated structs, so the
//! rendering shows what gob carried: an empty map that was sent renders as `{"$map": {}}`, a nil
//! one not at all. Decoded into a `HashMap`, the two would look the same.
//!
//! # What the suite plants first
//!
//! The manifest declares a settings schema with defaults and two secrets (one in a section), and
//! the shared configuration holds settings for a plugin no bundle installs and an empty entry for
//! another, but **nothing** for this one. The script reads that absence, then saves four times —
//! two maps, an empty map and a nil one, which Go stores as `{...}`, `{}` and `null` — and reads
//! back after each.

use std::collections::HashMap;
use std::sync::Arc;

use go_netrpc::Client;
use gobwire::{Dynamic, Interface, StructType, Type, Value};
use mm_plugin::wire::plugin::*;
use serde_json::Value as Json;

use crate::kv::call;

/// The message that runs the script.
pub const CONFIG_SCRIPT: &str = "!config-script";

async fn get_plugin_config(api: &Client) -> Json {
    call::<_, Dynamic>(api, "GetPluginConfig", Z_GetPluginConfigArgs {}).await
}

async fn load_plugin_configuration(api: &Client) -> Json {
    let mut entry = call::<_, Z_LoadPluginConfigurationArgsReturns>(
        api,
        "LoadPluginConfiguration",
        Z_LoadPluginConfigurationArgsArgs {},
    )
    .await;
    // The bytes are JSON; the text beside them makes a diff readable.
    if let Some(bytes) = entry["returns"]["A"]["$bytes"].as_str() {
        use base64::Engine as _;
        let text = base64::engine::general_purpose::STANDARD
            .decode(bytes)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        entry["text"] = Json::String(text);
    }
    entry
}

/// `SavePluginConfig` with the argument struct built by hand, so an **empty** map can be sent:
/// the generated struct would omit it, which is the nil case.
async fn save_empty_plugin_config(api: &Client) -> Json {
    let args = Dynamic {
        ty: Type::Struct(Arc::new(StructType {
            name: "Z_SavePluginConfigArgs".into(),
            fields: vec![(
                "A".into(),
                Type::Map(Arc::new(Type::String), Arc::new(Type::Interface)),
            )],
        })),
        value: Value::Struct(vec![Some(Value::Map(Vec::new()))]),
    };
    call::<_, Z_SavePluginConfigReturns>(api, "SavePluginConfig", args).await
}

/// The settings the script saves first: a secret under another spelling (`secretkey`), the
/// section's secret, every JSON kind, a float Go formats with an exponent and one it writes in
/// full where serde_json would not (`1e20`), and text Go escapes.
/// They cross as `json_to_interface` makes them — what a Go plugin's `map[string]any` of
/// decoded JSON would be.
fn first_settings() -> HashMap<String, Option<Interface>> {
    let settings = serde_json::json!({
        "Plain": "configured",
        "secretkey": "hunter2",
        "SectionSecret": "s3cret",
        "Nested": { "list": [1, "two", null, true], "deep": { "x": 1.25 } },
        "Big": 1e21,
        "Large": 1e20,
        "Html": "<b>&</b>",
        "Empty": {},
        "Nothing": null
    });
    settings
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), mm_plugin::rpc::json_to_interface(v)))
        .collect()
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client) -> Vec<Json> {
    let mut out = Vec::new();

    // No entry at all: `map[string]any{}`, sent, and the manifest's defaults alone.
    out.push(get_plugin_config(api).await);
    out.push(load_plugin_configuration(api).await);

    out.push(
        call::<_, Z_SavePluginConfigReturns>(
            api,
            "SavePluginConfig",
            Z_SavePluginConfigArgs {
                a: first_settings(),
            },
        )
        .await,
    );
    out.push(call::<_, Dynamic>(api, "GetConfig", Z_GetConfigArgs {}).await);
    out.push(call::<_, Dynamic>(api, "GetUnsanitizedConfig", Z_GetUnsanitizedConfigArgs {}).await);
    out.push(get_plugin_config(api).await);
    out.push(load_plugin_configuration(api).await);
    out.push(call::<_, Dynamic>(api, "GetLicense", Z_GetLicenseArgs {}).await);
    out.push(
        call::<_, Z_IsEnterpriseReadyReturns>(api, "IsEnterpriseReady", Z_IsEnterpriseReadyArgs {})
            .await,
    );
    out.push(call::<_, Z_GetPluginIDReturns>(api, "GetPluginID", Z_GetPluginIDArgs {}).await);
    out.push(
        call::<_, Z_GetTelemetryIdReturns>(api, "GetTelemetryId", Z_GetTelemetryIdArgs {}).await,
    );
    out.push(call::<_, Dynamic>(api, "GetCloudLimits", Z_GetCloudLimitsArgs {}).await);
    out.push(call::<_, Z_GetBundlePathReturns>(api, "GetBundlePath", Z_GetBundlePathArgs {}).await);

    // A second map over the first: the entry is replaced whole, not merged.
    let second = HashMap::from([
        ("Saved".to_owned(), Some(Interface::string("yes <&>"))),
        ("Number".to_owned(), Some(Interface::int(42))),
        ("Ratio".to_owned(), Some(Interface::float64(1.5))),
        ("Flag".to_owned(), Some(Interface::bool(false))),
        (
            "SecretKey".to_owned(),
            Some(Interface::string("new-secret")),
        ),
        ("Nothing".to_owned(), None),
    ]);
    out.push(
        call::<_, Z_SavePluginConfigReturns>(
            api,
            "SavePluginConfig",
            Z_SavePluginConfigArgs { a: second },
        )
        .await,
    );
    out.push(get_plugin_config(api).await);
    out.push(load_plugin_configuration(api).await);

    // An empty map, stored as `{}` and answered as a sent empty map.
    out.push(save_empty_plugin_config(api).await);
    out.push(get_plugin_config(api).await);
    out.push(load_plugin_configuration(api).await);

    // A nil map, stored as `null` and answered as nothing at all; the configuration then holds a
    // nil entry, which gob sends as an empty map because it is a map's element.
    out.push(
        call::<_, Z_SavePluginConfigReturns>(
            api,
            "SavePluginConfig",
            Z_SavePluginConfigArgs::default(),
        )
        .await,
    );
    out.push(get_plugin_config(api).await);
    out.push(load_plugin_configuration(api).await);
    out.push(call::<_, Dynamic>(api, "GetConfig", Z_GetConfigArgs {}).await);
    out
}
