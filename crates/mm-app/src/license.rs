//! Port of `Server.ClientLicense` (channels/app/license.go:129), restricted to the case this
//! server can answer: an installation with no licence.
//!
//! # Why this asks a question Go never asks
//!
//! Go keeps the licence in memory. `PlatformService.LoadLicense` (platform/license.go:49) fills
//! `licenseValue` **once, at startup**, from — in order — the `MM_LICENSE` environment variable,
//! the `Systems.ActiveLicenseId` row, or a file on disk; and `ClientLicense()` (platform/
//! license.go:303) then returns the derived map, or `{"IsLicensed": "false"}` when nothing was
//! loaded. There is no query to port, only a value we cannot see.
//!
//! Two of Go's three sources are reachable from here, and the third collapses into the second:
//!
//! | Go's source | Us |
//! |---|---|
//! | `MM_LICENSE` | the same variable, read by [`crate::config::Config::from_env`] — the [D-156] arrangement |
//! | `Systems.ActiveLicenseId` | the shared database, read here |
//! | a file at `LicenseFileLocation` | Go's own loader calls `SaveLicense` on it, which **writes** `ActiveLicenseId` — so it arrives in the row above |
//!
//! So a licensed installation is visible to us in every case except one: `MM_LICENSE` set on the
//! Go server's environment and not on ours. That is the same divergence [D-156] already records
//! for the config settings, and it fails in the safe direction — we do not answer, we forward.

use mm_model::utils::{AppError, AppResult};
use mm_store::{SYSTEM_ACTIVE_LICENSE_ID, StoreError, SystemStore};

use crate::App;

/// What `getClientLicense` should do with this installation.
///
/// Deliberately not `Option<map>`: "no licence" and "cannot answer" are different facts and a
/// handler that confuses them serves `IsLicensed: false` for a licensed server, which is the one
/// wrong answer this route can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseState {
    /// Nothing we can see says this server has a licence, so `ClientLicense()` is Go's `nil`
    /// branch and [`LicenseState::unlicensed_client_license`] is the whole answer.
    Unlicensed,
    /// A licence is installed. Its client map is derived from the signed licence body and the
    /// active feature set, none of which is ported — the caller forwards to Go.
    Licensed,
}

impl LicenseState {
    /// Go's `ClientLicense()` fallback map (platform/license.go:303-308).
    ///
    /// One key, and its value is the **string** `"false"`, not the JSON literal: the whole
    /// structure is `map[string]string`, so every value a licensed server returns is quoted too.
    pub fn unlicensed_client_license() -> std::collections::BTreeMap<String, String> {
        let mut map = std::collections::BTreeMap::new();
        map.insert("IsLicensed".to_owned(), "false".to_owned());
        map
    }
}

impl App {
    /// Whether this installation has a licence, by the two sources a second process can see.
    ///
    /// `IsValidId` is the test Go applies to the stored id (platform/license.go:92) before it will
    /// look a licence up — an empty or malformed value there means "no licence", which is exactly
    /// what `RemoveLicense` leaves behind when it blanks the row rather than deleting it.
    #[tracing::instrument(skip_all, fields(state))]
    pub async fn license_state(&self) -> AppResult<LicenseState> {
        if !self.config().license.is_empty() {
            tracing::Span::current().record("state", "licensed (MM_LICENSE)");
            return Ok(LicenseState::Licensed);
        }

        let active = self
            .store()
            .system()
            .get_by_name(SYSTEM_ACTIVE_LICENSE_ID)
            .await
            .map_err(license_state_error)?;

        let state = match active {
            Some(id) if mm_model::utils::is_valid_id(&id) => LicenseState::Licensed,
            _ => LicenseState::Unlicensed,
        };
        tracing::Span::current().record("state", format!("{state:?}"));
        Ok(state)
    }
}

/// A store failure here is a 500 and not "unlicensed".
///
/// Go's `LoadLicense` discards this error (`if nErr == nil`) and leaves the id empty, but it does
/// so **once at startup**, where the alternative is refusing to boot. Copying that per-request
/// would answer `IsLicensed: false` for a licensed server every time the database hiccuped —
/// turning a transient fault into a wrong answer instead of a visible one. Every other migrated
/// route answers 500 when its store fails; this one does too.
fn license_state_error(err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "reading the active licence id failed");
    AppError::boxed(
        "LoadLicense",
        "app.system.get_by_name.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one key, and the value quoted as a string. A bare `false` here would change the type
    /// of the field every client reads.
    #[test]
    fn the_unlicensed_map_is_one_string_valued_key() {
        let map = LicenseState::unlicensed_client_license();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("IsLicensed").map(String::as_str), Some("false"));
        assert_eq!(
            serde_json::to_string(&map).expect("a string map serialises"),
            r#"{"IsLicensed":"false"}"#
        );
    }
}
