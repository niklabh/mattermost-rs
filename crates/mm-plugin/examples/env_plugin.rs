//! The smallest plugin `tests/environment.rs` needs: it activates, deactivates, and appends one
//! line per call to the file named by `$ENV_PLUGIN_LOG`.
//!
//! The test installs this one binary under several names. A name containing `refuse` answers
//! `OnActivate` with an `*model.AppError`, which is how both environments see a plugin refuse to
//! start.

use std::io::Write;

use mm_plugin::rpc::{Hooks, HooksFileUpload, HooksHttp, NotImplemented, Plugin, client_main};
use mm_plugin::wire::model::AppError;
use mm_plugin::wire::plugin::{Z_OnActivateReturns, Z_OnDeactivateArgs, Z_OnDeactivateReturns};
use mm_plugin::wire::registered;

struct Env {
    name: String,
}

impl Env {
    fn log(&self, what: &str) {
        let Some(path) = std::env::var_os("ENV_PLUGIN_LOG") else {
            return;
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open the log");
        writeln!(f, "{}: {what}", self.name).expect("write the log");
    }
}

impl Hooks for Env {
    fn implemented(&self) -> Vec<String> {
        vec!["OnActivate".into(), "OnDeactivate".into()]
    }

    async fn on_deactivate(
        &self,
        _: Z_OnDeactivateArgs,
    ) -> Result<Z_OnDeactivateReturns, NotImplemented> {
        self.log("OnDeactivate");
        Ok(Z_OnDeactivateReturns::default())
    }
}

impl HooksHttp for Env {}
impl HooksFileUpload for Env {}

impl Plugin for Env {
    async fn on_activate(&self) -> Result<Z_OnActivateReturns, NotImplemented> {
        self.log("OnActivate");
        if !self.name.contains("refuse") {
            return Ok(Z_OnActivateReturns::default());
        }
        let refused = AppError {
            id: "env_plugin.refused".into(),
            message: "refused by env_plugin".into(),
            r#where: "OnActivate".into(),
            status_code: 500,
            ..Default::default()
        };
        Ok(Z_OnActivateReturns {
            a: Some(gobwire::Interface::new(registered::APP_ERROR, &refused).expect("encode")),
        })
    }
}

#[tokio::main]
async fn main() {
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if let Err(e) = client_main(Env { name }).await {
        eprintln!("env plugin: {e}");
        std::process::exit(1);
    }
}
