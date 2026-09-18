//! A Mattermost plugin written in Rust with the `mm_plugin` SDK: the plugin half of
//! `tests/sdk_conformance.rs`, run under a Go host (`reference/dump/plugingen host`).
//!
//! Every generated hook answers with its `Z_<Hook>Returns` fixture. `OnActivate` calls every
//! generated API method with its `Z_<Method>Args` fixture. Everything the plugin sees goes to the
//! JSON-lines file named by `$CONFORMANCE_TRANSCRIPT`, in order:
//!
//! ```text
//! {"set_api": true}
//! {"hook": "<Name>", "args": <render>}
//! {"api": "<Name>", "returns": <render>}
//! {"activated": true}
//! ```
//!
//! With `$CONFORMANCE_REFUSE_ACTIVATION` set, `OnActivate` returns an `*model.AppError` instead
//! of touring the API, and records `{"refused": true}`.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

use mm_plugin::rpc::{ApiClient, Hooks, NotImplemented, Plugin, client_main};
use mm_plugin::wire::model::AppError;
use mm_plugin::wire::plugin::Z_OnActivateReturns;
use mm_plugin::wire::registered;
use serde_json::{Value as Json, json};

#[path = "../tests/common/render.rs"]
mod render;
use render::{fixture, render_typed};

struct Conformance {
    api: OnceLock<ApiClient>,
    transcript: Mutex<std::fs::File>,
}

impl Conformance {
    fn record(&self, entry: Json) {
        let mut f = self.transcript.lock().unwrap();
        writeln!(f, "{entry}").unwrap();
    }

    fn answer<A: gobwire::Encode, R: gobwire::Decode + Default + Send + 'static>(
        &self,
        name: &str,
        returns: &str,
        args: &A,
    ) -> R {
        self.record(json!({ "hook": name, "args": render_typed(args) }));
        fixture(returns)
    }
}

macro_rules! names {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        &[$($name),*]
    };
}
const HOOKS: &[&str] = mm_plugin::for_each_hook!(names);

/// What the log methods send. Go's plugin sends `%+v` of ("key", 42, true), which is what these
/// already are: Go stringifies them client-side (stringifier.go).
const LOG_MESSAGE: &str = "a logged line";
const LOG_PAIRS: [&str; 3] = ["key", "42", "true"];

macro_rules! hooks {
    ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
        impl Hooks for Conformance {
            fn implemented(&self) -> Vec<String> {
                let mut names: Vec<String> = HOOKS.iter().map(|s| (*s).to_owned()).collect();
                names.push("OnActivate".into());
                names
            }
            $(
                async fn $method(&self, args: $args) -> Result<$returns, NotImplemented> {
                    Ok(self.answer($name, $returns_name, &args))
                }
            )*
        }
    };
}
mm_plugin::for_each_hook!(hooks);

impl Plugin for Conformance {
    fn set_api(&self, api: ApiClient, _driver: go_netrpc::Client) {
        self.record(json!({ "set_api": true }));
        let _ = self.api.set(api);
    }

    async fn on_activate(&self) -> Result<Z_OnActivateReturns, NotImplemented> {
        let Some(api) = self.api.get() else {
            panic!("OnActivate before SetAPI");
        };
        if std::env::var_os("CONFORMANCE_REFUSE_ACTIVATION").is_some() {
            let err = AppError {
                id: "conformance.refused".into(),
                message: "activation refused".into(),
                r#where: "conformance.OnActivate".into(),
                status_code: 500,
                ..AppError::default()
            };
            self.record(json!({ "refused": true }));
            return Ok(Z_OnActivateReturns {
                a: Some(gobwire::Interface::new(registered::APP_ERROR, &err).unwrap()),
            });
        }
        macro_rules! tour {
            ($(($method:ident, $name:literal, $args_name:literal, $args:ty, $returns_name:literal, $returns:ty),)*) => {
                $(
                    let returns = api.$method(fixture::<$args>($args_name)).await;
                    self.record(json!({ "api": $name, "returns": render_typed(&returns) }));
                )*
            };
        }
        mm_plugin::for_each_api_call!(tour);

        // The methods whose clients are hand-written, with the values the test expects.
        let pairs = LOG_PAIRS.map(str::to_owned);
        api.log_debug(LOG_MESSAGE, &pairs).await;
        api.log_info(LOG_MESSAGE, &pairs).await;
        api.log_warn(LOG_MESSAGE, &pairs).await;
        api.log_error(LOG_MESSAGE, &pairs).await;
        // The audit record goes through the gob-safe JSON round trip inside the client.
        // Each method sends its own fixture's record, which is what the test expects of it.
        let logged: mm_plugin::wire::plugin::Z_LogAuditRecArgs = fixture("Z_LogAuditRecArgs");
        api.log_audit_rec(*logged.a.expect("the fixture has a record"))
            .await;
        let with_level: mm_plugin::wire::plugin::Z_LogAuditRecWithLevelArgs =
            fixture("Z_LogAuditRecWithLevelArgs");
        api.log_audit_rec_with_level(
            *with_level.a.expect("the fixture has a record"),
            with_level.b,
        )
        .await;

        let config = api.load_plugin_configuration().await;
        self.record(json!({
            "api": "LoadPluginConfiguration",
            "config": serde_json::from_slice::<Json>(&config).unwrap_or(Json::Null),
        }));

        self.record(json!({ "activated": true }));
        Ok(Z_OnActivateReturns::default())
    }
}

#[tokio::main]
async fn main() {
    let path =
        std::env::var_os("CONFORMANCE_TRANSCRIPT").expect("CONFORMANCE_TRANSCRIPT is not set");
    let transcript = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open the transcript");
    let plugin = Conformance {
        api: OnceLock::new(),
        transcript: Mutex::new(transcript),
    };
    if let Err(e) = client_main(plugin).await {
        eprintln!("conformance plugin: {e}");
        std::process::exit(1);
    }
}
