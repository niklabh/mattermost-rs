//! Port of the file-backend half of `server/channels/app/export.go` — `ListExports`,
//! `DeleteExport` and `GeneratePresignURLForExport`.
//!
//! The bulk-export machinery those names suggest is not here; these three only walk a directory.
//!
//! # `ExportSettings.Directory` is not `FileSettings.ExportDirectory`
//!
//! The first is a path **inside** the export backend and defaults to `./export`. The second is
//! the export backend's **root** and defaults to `./data/`. On a stock server an export therefore
//! lives at `./data/export/<name>`, and reading either setting for the other's job puts every one
//! of these functions in the wrong directory while still finding a plausible-looking one.

use mm_model::go_path;
use mm_model::utils::AppError;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.ListExports` (app/export.go:1248).
    ///
    /// Reduces each listing entry with `filepath.Base`, because `ListDirectory` returns entries
    /// prefixed with the directory it was asked about. Unlike [`App::list_imports`] it filters
    /// **nothing** — a half-written `.tmp` export is listed.
    pub async fn list_exports(&self) -> Result<Vec<String>, PrepareError> {
        let exports = self
            .list_export_directory(&self.config().export_directory)
            .await?;
        Ok(exports.iter().map(|path| go_path::base(path)).collect())
    }

    /// Port of `app.App.DeleteExport` (app/export.go:1297).
    ///
    /// **A missing export is success, not a 404.** The handler then answers `{"status":"OK"}`,
    /// so `DELETE` of a name that was never there is indistinguishable from deleting a real one.
    pub async fn delete_export(&self, name: &str) -> Result<(), PrepareError> {
        let path = go_path::join(&[&self.config().export_directory, name]);

        if !self.export_file_exists(&path).await? {
            return Ok(());
        }

        self.remove_export_file(&path).await
    }

    /// Port of `app.App.GeneratePresignURLForExport` (app/export.go:1262), up to the backend.
    ///
    /// Three refusals, in Go's order, each a **500**: `FeatureFlags.EnableExportDirectDownload`
    /// (default `false`), then `FileSettings.DedicatedExportStore`, then an export backend that is
    /// not a `FileBackendWithLinkGenerator` — which, as in Go, is the backend built at boot
    /// ([`crate::filestore::FileBackend::generates_links`]). Past them the backend is S3 or Azure,
    /// whose `FileExists` and `GeneratePublicLink` only the Go server implements, so the rest is
    /// [`PrepareError::Unreproducible`] and the handler forwards.
    ///
    /// All three ids carry Go's typo, `eport`. Reproduced: a client matching on the string sees
    /// the same one from either server.
    pub async fn generate_presign_url_for_export(
        &self,
        _name: &str,
    ) -> Result<serde_json::Value, PrepareError> {
        let refuse = |id: &'static str| {
            PrepareError::App(AppError::boxed(
                "GeneratePresignURLForExport",
                id,
                None,
                String::new(),
                500,
            ))
        };
        let config = self.config();
        if !config.feature_flags.enable_export_direct_download {
            return Err(refuse(
                "app.eport.generate_presigned_url.featureflag.app_error",
            ));
        }
        if !config.dedicated_export_store {
            return Err(refuse("app.eport.generate_presigned_url.config.app_error"));
        }
        if !self.export_file_backend().generates_links() {
            return Err(refuse("app.eport.generate_presigned_url.driver.app_error"));
        }
        Err(PrepareError::Unreproducible(
            "presigning an export needs the S3 or Azure file backend, which only Go implements",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(flag: bool, dedicated: bool, export_driver: &str) -> App {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let mut config = crate::config::Config {
            dedicated_export_store: dedicated,
            file_export_driver_name: export_driver.to_owned(),
            ..crate::config::Config::default()
        };
        config.feature_flags.enable_export_direct_download = flag;
        App::with_config(mm_store::SqlStore::from_pool(pool), config)
    }

    async fn refusal(app: App) -> String {
        match app.generate_presign_url_for_export("x.zip").await {
            Err(PrepareError::App(err)) => {
                assert_eq!(err.status_code, 500);
                err.id
            }
            Err(PrepareError::Unreproducible(_)) => "forward".to_owned(),
            Ok(value) => panic!("answered {value}"),
        }
    }

    /// `GeneratePresignURLForExport`'s three gates in Go's order (export.go:1263-1275): each
    /// refuses on its own even when a later one would too, and only past all three does the
    /// request need the S3/Azure backend, which forwards.
    #[tokio::test]
    async fn the_presign_gates_refuse_in_gos_order() {
        assert_eq!(
            refusal(app(false, true, "amazons3")).await,
            "app.eport.generate_presigned_url.featureflag.app_error"
        );
        assert_eq!(
            refusal(app(false, false, "local")).await,
            "app.eport.generate_presigned_url.featureflag.app_error"
        );
        assert_eq!(
            refusal(app(true, false, "amazons3")).await,
            "app.eport.generate_presigned_url.config.app_error"
        );
        assert_eq!(
            refusal(app(true, true, "local")).await,
            "app.eport.generate_presigned_url.driver.app_error"
        );
        assert_eq!(refusal(app(true, true, "amazons3")).await, "forward");
        assert_eq!(refusal(app(true, true, "azureblob")).await, "forward");
    }
}
