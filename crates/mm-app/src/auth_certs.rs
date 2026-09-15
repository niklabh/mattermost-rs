//! Port of the halves of `app/saml.go`, `app/ldap.go` and `app/audit.go` that the routes of
//! `api4/saml.go`, `api4/ldap.go` and `api4/audit_logging.go` reach on this build.
//!
//! # What is nil here, and what that decides
//!
//! `App.Saml()`, `App.Ldap()` and `App.LdapDiagnostic()` (app/app.go:84-104) are interfaces the
//! enterprise package registers. The open-source tree registers nothing, so they are nil on
//! **both** oracles — measured, not inferred: the licensed server answers `POST /ldap/test` with
//! `ent.ldap.disabled.app_error` and `POST /saml/reset_auth_data` with
//! `api.admin.saml.not_available.app_error`, exactly as the Team Edition one does once its
//! licence gate is passed. Every function below that Go routes through one of those interfaces
//! is therefore its nil branch, verbatim, and nothing else: `UserStore.ResetAuthDataToEmailForUsers`
//! is not ported because no build of this tree reaches it, and `SyncLdap` is the log line its
//! goroutine ends on.
//!
//! # The certificate writers are not here
//!
//! `AddSamlPublicCertificate` and its five siblings (app/saml.go:60-171, app/ldap.go:235-309,
//! app/audit.go:174-238) are `platform.SetConfigFile`/`RemoveConfigFile` followed by
//! `UpdateConfig`, which persists a new `Configurations` row. This server's [`crate::config::Config`]
//! is fixed at construction and the configuration-document write belongs to the config family,
//! so a certificate written here would be invisible to the process that wrote it. The API layer
//! (`mm_api::auth_certs`) serves the gate and the request parse of every add and remove and
//! forwards the write itself to Go — see [D-660]. What *is* served is
//! [`App::get_saml_certificate_status`], and it reads the live document so it agrees with Go
//! after such a write.

use mm_model::config::LdapSettings;
use mm_model::ldap::{LdapDiagnosticResult, LdapDiagnosticTestType};
use mm_model::license::License;
use mm_model::saml::{SamlCertificateStatus, SamlMetadataResponse};
use mm_model::utils::{AppError, AppResult};
use mm_store::ConfigStore;

use crate::App;
use crate::config::ConfigError;

/// `license != nil && *license.Features.LDAP` — the gate on every `/ldap` route
/// (api4/ldap.go:49, 68, 87, 106, 348). Go dereferences the pointer, and `SafeDereference` on
/// `testLdapConnection` is the same answer for a licence that went through `SetDefaults`.
pub fn license_has_ldap(license: &License) -> bool {
    license
        .features
        .as_ref()
        .and_then(|features| features.ldap)
        .unwrap_or(false)
}

/// `ent.ldap.disabled.app_error`, 501 — the answer of `TestLdap`, `TestLdapConnection`,
/// `TestLdapDiagnostics` and `MigrateIdLDAP` with no LDAP interface (app/ldap.go:44, 58, 72,
/// 212). The `where` differs per caller and is the one thing this takes.
fn ldap_disabled(where_: &str) -> Box<AppError> {
    Box::new(AppError::new(
        where_,
        "ent.ldap.disabled.app_error",
        None,
        String::new(),
        501,
    ))
}

/// `api.admin.saml.not_available.app_error`, 501 — `a.Saml() == nil` (app/saml.go:29, 184,
/// 290).
fn saml_not_available(where_: &str) -> Box<AppError> {
    Box::new(AppError::new(
        where_,
        "api.admin.saml.not_available.app_error",
        None,
        String::new(),
        501,
    ))
}

impl App {
    /// Port of `App.GetSamlCertificateStatus` (app/saml.go:173).
    ///
    /// Go asks `HasConfigFile` for the three filenames in its in-memory `SamlSettings`, which
    /// `UpdateConfig` refreshes on every certificate write. This server's [`crate::config::Config`]
    /// is fixed at construction, so the filenames are read from the **live** configuration
    /// document — the row Go's `SaveConfig` wrote — and a certificate added through Go a moment
    /// ago is reported here as it is there. The three `HasFile` errors are dropped
    /// (`status.X, _ = …`), so a store failure is `false`, as in Go; the document read has no Go
    /// counterpart and is the one failure this returns.
    #[tracing::instrument(skip_all)]
    pub async fn get_saml_certificate_status(&self) -> Result<SamlCertificateStatus, ConfigError> {
        let config = crate::config::load_model_config(self.store.config()).await?;
        let saml = &config.saml_settings;
        Ok(SamlCertificateStatus {
            idp_certificate_file: self
                .has_config_file(saml.idp_certificate_file.as_deref())
                .await,
            private_key_file: self.has_config_file(saml.private_key_file.as_deref()).await,
            public_certificate_file: self
                .has_config_file(saml.public_certificate_file.as_deref())
                .await,
        })
    }

    /// `platform.HasConfigFile` with the error dropped the way `GetSamlCertificateStatus` drops
    /// it. A `None` name is a document that never went through `SetDefaults`; Go would
    /// dereference nil there, and this asks for the empty name, which no file has.
    async fn has_config_file(&self, name: Option<&str>) -> bool {
        match self.store.config().has_file(name.unwrap_or_default()).await {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(error = %err, "HasConfigFile failed; reporting the file as absent, as Go does");
                false
            }
        }
    }

    /// Port of `App.SyncLdap` (app/ldap.go:18).
    ///
    /// Go spawns a goroutine and the handler answers `{"status":"OK"}` before it runs, so nothing
    /// in it reaches the wire. With an LDAP-featured licence the goroutine finds either
    /// `EnableSync` off or `Ldap()` nil and logs an error; without one it does nothing. This tree
    /// registers no LDAP interface, so the outcome is the `Ldap()`-nil log line whichever setting
    /// holds — the `EnableSync` branch is a different message on the same nothing and is not
    /// distinguished. A licence read that fails is logged and treated as no licence.
    #[tracing::instrument(skip_all)]
    pub async fn sync_ldap(&self) {
        let licensed = match self.license().await {
            Ok(license) => license.as_deref().is_some_and(license_has_ldap),
            Err(err) => {
                tracing::error!(error = %err, "could not read the licence for the LDAP sync");
                false
            }
        };
        if licensed {
            tracing::error!("Not executing ldap sync because ldap is not available");
        }
    }

    /// Port of `App.TestLdap` (app/ldap.go:38): `LdapDiagnostic()` is nil, so the licence and
    /// the two settings are never consulted and the answer is the 501.
    pub fn test_ldap(&self) -> AppResult {
        Err(ldap_disabled("TestLdap"))
    }

    /// Port of `App.TestLdapConnection` (app/ldap.go:48) — the same nil branch. The settings are
    /// decoded by the handler because a body that does not decode is a different answer.
    pub fn test_ldap_connection(&self, _settings: &LdapSettings) -> AppResult {
        Err(ldap_disabled("TestLdapConnection"))
    }

    /// Port of `App.TestLdapDiagnostics` (app/ldap.go:62) — the same nil branch.
    pub fn test_ldap_diagnostics(
        &self,
        _test_type: &LdapDiagnosticTestType,
        _settings: &LdapSettings,
    ) -> AppResult<Vec<LdapDiagnosticResult>> {
        Err(ldap_disabled("TestLdapDiagnostics"))
    }

    /// Port of `App.MigrateIdLDAP` (app/ldap.go:200): `Ldap()` is nil, so the attribute is never
    /// used. Note the `where` — `IdMigrateLDAP`, not the function's own name.
    pub fn migrate_id_ldap(&self, _to_attribute: &str) -> AppResult {
        Err(ldap_disabled("IdMigrateLDAP"))
    }

    /// Port of `App.ResetSamlAuthDataToEmail` (app/saml.go:288): `Saml()` is nil, so the store
    /// call after it is never made and the three parameters are never read.
    pub fn reset_saml_auth_data_to_email(
        &self,
        _include_deleted: bool,
        _dry_run: bool,
        _user_ids: &[String],
    ) -> AppResult<i64> {
        Err(saml_not_available("ResetAuthDataToEmail"))
    }

    /// Port of `App.GetSamlMetadataFromIdp` (app/saml.go:183): the nil check is the first
    /// statement, ahead of the `https://` prefixing and the fetch. The handler wraps this in its
    /// own 400 (api4/saml.go:256), so the 501 here never reaches a client.
    pub fn get_saml_metadata_from_idp(
        &self,
        _idp_metadata_url: &str,
    ) -> AppResult<SamlMetadataResponse> {
        Err(saml_not_available("GetSamlMetadataFromIdp"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::license::Features;

    #[test]
    fn the_ldap_feature_needs_a_features_block_and_a_true_flag() {
        let mut license = License::default();
        assert!(!license_has_ldap(&license), "no features block");
        license.features = Some(Features::default());
        assert!(!license_has_ldap(&license), "flag unset");
        if let Some(features) = license.features.as_mut() {
            features.ldap = Some(false);
        }
        assert!(!license_has_ldap(&license), "flag off");
        if let Some(features) = license.features.as_mut() {
            features.ldap = Some(true);
        }
        assert!(license_has_ldap(&license));
    }

    #[test]
    fn the_nil_branches_carry_gos_where_and_status() {
        let err = ldap_disabled("IdMigrateLDAP");
        assert_eq!(err.where_, "IdMigrateLDAP");
        assert_eq!(err.id, "ent.ldap.disabled.app_error");
        assert_eq!(err.status_code, 501);
        let err = saml_not_available("ResetAuthDataToEmail");
        assert_eq!(err.id, "api.admin.saml.not_available.app_error");
        assert_eq!(err.status_code, 501);
    }
}
