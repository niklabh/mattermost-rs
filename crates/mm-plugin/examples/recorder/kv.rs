//! The hook recorder's API script: a fixed sequence of plugin API calls, each written down with
//! what the host answered, for `parity::plugin_hooks`' KV tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`KV_SCRIPT`], from inside `MessageWillBePosted`, so the
//! trigger is an ordinary client request both hosts serve. Each call goes through the raw net/rpc
//! client rather than `ApiClient`'s wrappers, which swallow a transport error into zero values:
//! here a method the host lacks shows up as `API <Name> called but not implemented.`, not as an
//! answer indistinguishable from an empty one.
//!
//! # What the suite plants first
//!
//! The script reads rows the suite writes straight into `PluginKeyValueStore` before each side
//! runs, so that the pre-5.6 hashed spelling, an expired value,
//! a NULL expiry and another plugin's key are all reachable, none of which the API can write:
//! `legacy` and `hashed-only` under their hashed spellings only, `expired` and `expired-old`
//! already expired, `null-expiry` with a NULL `ExpireAt`, `future-old` expiring in the far future,
//! and `shared-key` for **another** plugin id.

use go_netrpc::Client;
use gobwire::{Decode, Encode, Interface};
use mm_plugin::wire::model::PluginKVSetOptions;
use mm_plugin::wire::plugin::*;
use serde_json::{Value as Json, json};

use crate::render::render_typed;

/// The message that runs the script.
pub const KV_SCRIPT: &str = "!kv-script";

/// One call through the raw client, written down with its arguments and what came back — or the
/// transport error, such as a method the host lacks.
pub async fn call<A, R>(api: &Client, name: &str, args: A) -> Json
where
    A: Encode,
    R: Decode + Default + Encode + Send + 'static,
{
    match api.call::<A, R>(&format!("Plugin.{name}"), &args).await {
        Ok(returns) => json!({
            "call": name,
            "args": render_typed(&args),
            "returns": render_typed(&returns),
        }),
        Err(e) => json!({ "call": name, "args": render_typed(&args), "error": e.to_string() }),
    }
}

fn b(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

async fn get(api: &Client, key: &str) -> Json {
    call::<_, Z_KVGetReturns>(api, "KVGet", Z_KVGetArgs { a: key.into() }).await
}

async fn set(api: &Client, key: &str, value: &str) -> Json {
    call::<_, Z_KVSetReturns>(
        api,
        "KVSet",
        Z_KVSetArgs {
            a: key.into(),
            b: b(value),
        },
    )
    .await
}

async fn set_with_expiry(api: &Client, key: &str, value: &str, seconds: i64) -> Json {
    call::<_, Z_KVSetWithExpiryReturns>(
        api,
        "KVSetWithExpiry",
        Z_KVSetWithExpiryArgs {
            a: key.into(),
            b: b(value),
            c: seconds,
        },
    )
    .await
}

async fn set_with_options(
    api: &Client,
    key: &str,
    value: &str,
    atomic: bool,
    old: &str,
    seconds: i64,
) -> Json {
    call::<_, Z_KVSetWithOptionsReturns>(
        api,
        "KVSetWithOptions",
        Z_KVSetWithOptionsArgs {
            a: key.into(),
            b: b(value),
            c: PluginKVSetOptions {
                atomic,
                old_value: b(old),
                expire_in_seconds: seconds,
            },
        },
    )
    .await
}

/// An empty `old` or `new` is Go's nil: gob sends nothing for an empty slice.
async fn compare_and_set(api: &Client, key: &str, old: &str, new: &str) -> Json {
    call::<_, Z_KVCompareAndSetReturns>(
        api,
        "KVCompareAndSet",
        Z_KVCompareAndSetArgs {
            a: key.into(),
            b: b(old),
            c: b(new),
        },
    )
    .await
}

async fn compare_and_delete(api: &Client, key: &str, old: &str) -> Json {
    call::<_, Z_KVCompareAndDeleteReturns>(
        api,
        "KVCompareAndDelete",
        Z_KVCompareAndDeleteArgs {
            a: key.into(),
            b: b(old),
        },
    )
    .await
}

async fn delete(api: &Client, key: &str) -> Json {
    call::<_, Z_KVDeleteReturns>(api, "KVDelete", Z_KVDeleteArgs { a: key.into() }).await
}

async fn list(api: &Client, page: i64, per_page: i64) -> Json {
    call::<_, Z_KVListReturns>(
        api,
        "KVList",
        Z_KVListArgs {
            a: page,
            b: per_page,
        },
    )
    .await
}

/// The four log methods, each with a well-formed pair and a dangling last argument.
async fn logs(api: &Client, out: &mut Vec<Json>) {
    let pairs = || {
        vec![
            Some(Interface::string("script")),
            Some(Interface::string("kv")),
            Some(Interface::string("dangling")),
        ]
    };
    out.push(
        call::<_, Z_LogDebugReturns>(
            api,
            "LogDebug",
            Z_LogDebugArgs {
                a: "hook recorder debug".into(),
                b: pairs(),
            },
        )
        .await,
    );
    out.push(
        call::<_, Z_LogInfoReturns>(
            api,
            "LogInfo",
            Z_LogInfoArgs {
                a: "hook recorder info".into(),
                b: pairs(),
            },
        )
        .await,
    );
    out.push(
        call::<_, Z_LogWarnReturns>(
            api,
            "LogWarn",
            Z_LogWarnArgs {
                a: "hook recorder warn".into(),
                b: pairs(),
            },
        )
        .await,
    );
    out.push(
        call::<_, Z_LogErrorReturns>(
            api,
            "LogError",
            Z_LogErrorArgs {
                a: "hook recorder error".into(),
                b: pairs(),
            },
        )
        .await,
    );
}

/// Run the script, in order, and answer every call with what came back.
pub async fn run(api: &Client) -> Vec<Json> {
    let wide = "é".repeat(150);
    let too_long = "k".repeat(151);
    let mut out = Vec::new();

    // Reads of what the suite planted.
    for key in [
        "legacy",
        "missing",
        "expired",
        "null-expiry",
        "shared-key",
        "future-old",
    ] {
        out.push(get(api, key).await);
    }

    // Plain writes, the hashed spelling's clean-up, and the key rules.
    out.push(set(api, "alpha", "1").await);
    out.push(set(api, "legacy", "rewritten").await);
    out.push(get(api, "legacy").await);
    out.push(set(api, "", "x").await);
    out.push(set(api, &too_long, "x").await);
    out.push(set(api, &wide, "wide").await);
    out.push(get(api, &wide).await);
    out.push(set_with_options(api, "beta", "2", false, "x", 0).await);

    // Compare-and-set, both paths.
    out.push(compare_and_set(api, "alpha", "1", "2").await);
    out.push(compare_and_set(api, "alpha", "1", "3").await);
    out.push(compare_and_set(api, "alpha", "", "4").await);
    out.push(compare_and_set(api, "gamma", "", "g").await);
    out.push(compare_and_set(api, "expired", "", "fresh").await);
    out.push(compare_and_set(api, "null-expiry", "", "x").await);
    out.push(compare_and_set(api, "expired-old", "stale", "new").await);
    out.push(compare_and_set(api, "future-old", "stale", "new").await);
    out.push(compare_and_set(api, "alpha", "2", "").await);
    out.push(get(api, "alpha").await);
    out.push(compare_and_set(api, "", "", "x").await);

    // Compare-and-delete.
    out.push(compare_and_delete(api, "gamma", "wrong").await);
    out.push(compare_and_delete(api, "gamma", "").await);
    out.push(compare_and_delete(api, "gamma", "g").await);
    out.push(compare_and_delete(api, "", "x").await);
    out.push(compare_and_delete(api, "expired-old", "stale").await);

    // Expiry, and the options.
    out.push(set_with_expiry(api, "ttl-past", "p", -1).await);
    out.push(get(api, "ttl-past").await);
    out.push(set_with_expiry(api, "ttl-future", "f", 3600).await);
    out.push(get(api, "ttl-future").await);
    out.push(set_with_options(api, "opt-atomic", "v", true, "", 60).await);
    out.push(set_with_options(api, "opt-atomic", "w", true, "", 0).await);
    out.push(set_with_options(api, "opt-plain", "v", false, "", 0).await);
    out.push(set_with_options(api, "opt-plain", "", false, "", 0).await);
    out.push(get(api, "opt-plain").await);
    out.push(set(api, "alpha2", "a").await);
    out.push(set(api, "alpha2", "").await);
    out.push(get(api, "alpha2").await);

    // Paging, over more live keys than the default page of ten, so that page is observable.
    for n in 1..=5 {
        out.push(set(api, &format!("page-{n}"), "p").await);
    }
    out.push(list(api, 0, 100).await);
    out.push(list(api, 0, 3).await);
    out.push(list(api, 1, 3).await);
    out.push(list(api, 0, 0).await);
    out.push(list(api, -1, 3).await);
    out.push(list(api, 100, 3).await);

    // Deletes, including a key that exists only under its hash.
    out.push(delete(api, "legacy").await);
    out.push(delete(api, "hashed-only").await);
    out.push(delete(api, "nope").await);
    out.push(get(api, "legacy").await);
    out.push(list(api, 0, 100).await);

    logs(api, &mut out).await;

    out.push(
        call::<_, Z_GetServerVersionReturns>(api, "GetServerVersion", Z_GetServerVersionArgs {})
            .await,
    );
    out.push(
        call::<_, Z_GetDiagnosticIdReturns>(api, "GetDiagnosticId", Z_GetDiagnosticIdArgs {}).await,
    );
    out.push(
        call::<_, Z_GetSystemInstallDateReturns>(
            api,
            "GetSystemInstallDate",
            Z_GetSystemInstallDateArgs {},
        )
        .await,
    );

    // Delete-all, then the rows the suite compares.
    out.push(call::<_, Z_KVDeleteAllReturns>(api, "KVDeleteAll", Z_KVDeleteAllArgs {}).await);
    out.push(list(api, 0, 10).await);
    out.push(get(api, "future-old").await);
    out.push(set(api, "final-a", "A").await);
    out.push(set_with_expiry(api, "final-b", "B", 3600).await);
    out.push(set_with_expiry(api, "final-c", "C", -5).await);
    out
}
