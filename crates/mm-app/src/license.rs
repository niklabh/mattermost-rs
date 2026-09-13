//! Port of `PlatformService.LoadLicense` (channels/app/platform/license.go:49) and
//! `utils.LicenseValidator` (channels/utils/license.go), read per request instead of once.
//!
//! # What Go does, and what this does instead
//!
//! Go keeps the licence in memory. `LoadLicense` fills `licenseValue` **once, at startup**, from —
//! in order — the `MM_LICENSE` environment variable, the `Systems.ActiveLicenseId` row (through
//! `Licenses`), or a file on disk; `License()` then returns that value until a save replaces it.
//! There is no query to port, only a value we cannot see, so [`App::license`] rebuilds it on
//! demand from the same sources:
//!
//! | Go's source | Here |
//! |---|---|
//! | `MM_LICENSE` | the same variable, read by [`crate::config::Config::from_env`] and verified once at construction |
//! | `Systems.ActiveLicenseId` → `Licenses.Bytes` | the shared database, read here and verified here |
//! | a file at `LicenseFileLocation` | **not read** — Go's own loader calls `SaveLicense` on it, which writes the row above, so it arrives from the database |
//! | Mattermost Entry (`FeatureFlags.EnableMattermostEntry`) | not ported — `NewMattermostEntryLicense` is the enterprise licence manager |
//!
//! So a licensed installation is visible to us in every case except one: `MM_LICENSE` set on the
//! Go server's environment and not on ours. That is the same divergence [D-156] records for the
//! config settings, and it fails in the safe direction — we answer as unlicensed where Go would
//! forward nothing to us anyway.
//!
//! # The row has to verify
//!
//! A `Licenses` row is not proof of a licence: Go validates the bytes on every load
//! (`ValidateAndSetLicenseBytes`) and leaves the licence `nil` when they do not verify. Reading
//! the id alone — which this module did until 2026-09-13 — answered "licensed" for a planted row
//! Go would have refused, so the signature is checked here the way `LicenseValidatorImpl` checks
//! it: base64, a null-terminator strip, a 256-byte signature over SHA-512 of the rest, PKCS#1
//! v1.5 against the public key of the service environment, and the other environment's key as
//! the tiebreak that tells "wrong environment" from "forged".
//!
//! The two Mattermost public keys are compiled in from `license_keys/`, byte-identical copies of
//! `channels/utils/license-public-key*.txt`. `MMRS_LICENSE_PUBLIC_KEY_FILE` replaces them with a
//! key of the operator's choosing; it exists for the parity harness, whose licensed Go oracle
//! (`reference/licensed/main.go`) makes the same substitution on its side, and it is the only way
//! a licence not signed by Mattermost is ever honoured here.
//!
//! # Nothing is loaded once
//!
//! Go re-reads the licence only on a save; this reads `ActiveLicenseId` on every call, which is
//! what makes a licence installed through the Go server beside us visible without a restart. The
//! signature check is cached by licence id so the RSA work happens once per licence, not once
//! per request.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use mm_model::license::License;
use mm_model::utils::{AppError, AppResult};
use mm_store::{LicenseStore, SYSTEM_ACTIVE_LICENSE_ID, StoreError, SystemStore};
use rsa::pkcs8::DecodePublicKey;
use sha2::Digest;

use crate::App;

/// `channels/utils/license-public-key.txt` — the key a production licence verifies against.
pub const PRODUCTION_PUBLIC_KEY: &str = include_str!("license_keys/license-public-key.txt");
/// `channels/utils/license-public-key-test.txt` — the key for the test and dev environments.
pub const TEST_PUBLIC_KEY: &str = include_str!("license_keys/license-public-key-test.txt");

/// The length of the signature Mattermost appends — a 2048-bit RSA signature.
const SIGNATURE_LENGTH: usize = 256;

/// Whether this installation has a licence, as a two-valued summary of [`App::license`].
///
/// Kept beside the full licence because most gates ask only this, and a handler that forwards
/// on `Licensed` reads better than one that pattern-matches an `Option` it never looks inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseState {
    /// `License()` is `nil`: `ClientLicense()` is Go's fallback map and every licence gate refuses.
    Unlicensed,
    /// A licence verified and loaded. [`App::license`] has the body.
    Licensed,
}

impl LicenseState {
    /// Go's `ClientLicense()` fallback map (platform/license.go:303-308).
    ///
    /// One key, and its value is the **string** `"false"`, not the JSON literal: the whole
    /// structure is `map[string]string`, so every value a licensed server returns is quoted too.
    /// The same bytes as [`client_license`] on `None`, which is not a coincidence — Go's
    /// `GetClientLicense(nil)` produces this map too, and the fallback exists only because the
    /// atomic value it reads can be unset.
    pub fn unlicensed_client_license() -> BTreeMap<String, String> {
        client_license(None)
    }
}

/// Why a signed licence was refused. Port of the errors `ValidateLicense` (utils/license.go:71)
/// returns, with Go's messages — the parity corpus compares them.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LicenseValidationError {
    /// `base64.StdEncoding.Decode` failed. Go's inner message names the offending byte; ours
    /// names the decoder's reason, so only the prefix is comparable.
    #[error("encountered error decoding license: {0}")]
    Decode(String),
    /// Fewer than 257 bytes after the null strip — no room for a signature and one byte of
    /// licence.
    #[error("Signed license not long enough")]
    TooShort,
    /// Neither key verifies the signature.
    #[error("Invalid signature: crypto/rsa: verification error")]
    InvalidSignature,
    /// `utils.ErrLicenseProductionInTestEnvironment`.
    #[error(
        "license is a production license but the server is running in a test or development service environment"
    )]
    ProductionInTestEnvironment,
    /// `utils.ErrLicenseTestInProductionEnvironment`.
    #[error(
        "license is a test or development license but the server is running in a production service environment"
    )]
    TestInProductionEnvironment,
    /// The key material itself is unusable — Go's three `verifyLicenseSignature` failures that
    /// are not `rsa.ErrVerification`, and which it surfaces unchanged rather than trying the
    /// other key.
    #[error("{0}")]
    Key(String),
    /// The plaintext verified but is not a `model.License` — `json.Unmarshal` in
    /// `LicenseFromBytes`, `api.unmarshal_error`.
    #[error("Failed to decode license from JSON: {0}")]
    Json(String),
}

/// The public keys a licence may verify against: the service environment's own, and the other
/// environment's as the tiebreak. Port of `licenseKeysForEnvironment` (utils/license.go:134).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseKeys {
    primary: String,
    alternate: String,
    /// Which environment `primary` belongs to — decides which wrong-environment error a match
    /// on `alternate` produces.
    environment: String,
}

impl LicenseKeys {
    /// Mattermost's own keys, arranged for `environment` (`production`, `test` or `dev`).
    ///
    /// Go returns `nil, nil` for an unrecognised value, and every verification then fails on
    /// "failed to decode public key PEM block". `service_environment()` never produces one, so
    /// that arm is the dev arrangement here rather than a pair of empty keys.
    pub fn for_environment(environment: &str) -> Self {
        let (primary, alternate) = if environment == crate::config::SERVICE_ENVIRONMENT_PRODUCTION {
            (PRODUCTION_PUBLIC_KEY, TEST_PUBLIC_KEY)
        } else {
            (TEST_PUBLIC_KEY, PRODUCTION_PUBLIC_KEY)
        };
        Self {
            primary: primary.to_owned(),
            alternate: alternate.to_owned(),
            environment: environment.to_owned(),
        }
    }

    /// One operator-supplied key in both roles — the mirror of `reference/licensed/main.go`,
    /// whose validator has no second key to try, so a mismatch is always "Invalid signature" and
    /// never a wrong-environment error.
    pub fn single(pem: String, environment: &str) -> Self {
        Self {
            primary: pem.clone(), // both roles hold the same text; the struct owns each
            alternate: pem,
            environment: environment.to_owned(),
        }
    }

    /// The keys an [`App`] verifies with: the operator's override when the configuration names
    /// one, Mattermost's own otherwise.
    pub fn from_config(config: &crate::config::Config) -> Self {
        let environment = crate::config::service_environment();
        match &config.license_public_key {
            Some(pem) => {
                tracing::warn!(
                    "MMRS_LICENSE_PUBLIC_KEY_FILE is set: licences are verified against an \
                     operator-supplied key, not Mattermost's"
                );
                Self::single(pem.clone(), &environment) // the config keeps its copy
            }
            None => Self::for_environment(&environment),
        }
    }
}

/// Port of `LicenseValidatorImpl.ValidateLicense` (utils/license.go:71): the plaintext of a
/// signed licence, or why it was refused.
///
/// # The three things a reader would get wrong
///
/// 1. **Trailing zero bytes are stripped before the length check, whatever they are.** Go decodes
///    into a buffer sized by `DecodedLen`, ignores the count it gets back, and then strips
///    trailing NULs — which removes the padding slack *and any zero byte the signature itself
///    ends in*. A signature ending in `0x00` therefore fails to verify in Go, one time in 256,
///    and it fails here too.
/// 2. **Newlines are not an error.** Go's decoder skips `\r` and `\n` anywhere in the input,
///    which is how a licence file with a trailing newline loads. The `base64` crate does not, so
///    they are removed first.
/// 3. **The alternate key is consulted only on a genuine mismatch.** A malformed key is surfaced
///    as its own error without trying the other one, so a broken key can never be mistaken for
///    a licence from the wrong environment.
pub fn validate_license(
    signed: &[u8],
    keys: &LicenseKeys,
) -> Result<Vec<u8>, LicenseValidationError> {
    let stripped: Vec<u8> = signed
        .iter()
        .copied()
        .filter(|b| *b != b'\n' && *b != b'\r')
        .collect();
    let mut decoded = base64::engine::general_purpose::STANDARD
        .decode(&stripped)
        .map_err(|err| LicenseValidationError::Decode(err.to_string()))?;

    // remove null terminator
    while decoded.last() == Some(&0) {
        decoded.pop();
    }

    if decoded.len() <= SIGNATURE_LENGTH {
        return Err(LicenseValidationError::TooShort);
    }

    let (plaintext, signature) = decoded.split_at(decoded.len() - SIGNATURE_LENGTH);
    let digest = sha2::Sha512::digest(plaintext);

    match verify_license_signature(&keys.primary, &digest, signature) {
        Ok(()) => Ok(plaintext.to_vec()),
        Err(SignatureFailure::Key(message)) => Err(LicenseValidationError::Key(message)),
        Err(SignatureFailure::Verification) => {
            match verify_license_signature(&keys.alternate, &digest, signature) {
                Ok(()) => Err(wrong_environment(&keys.environment)),
                Err(SignatureFailure::Key(message)) => Err(LicenseValidationError::Key(message)),
                Err(SignatureFailure::Verification) => {
                    Err(LicenseValidationError::InvalidSignature)
                }
            }
        }
    }
}

/// Port of `wrongEnvironmentError` (utils/license.go:120).
fn wrong_environment(environment: &str) -> LicenseValidationError {
    if environment == crate::config::SERVICE_ENVIRONMENT_PRODUCTION {
        LicenseValidationError::TestInProductionEnvironment
    } else {
        LicenseValidationError::ProductionInTestEnvironment
    }
}

/// `verifyLicenseSignature`'s two kinds of failure: `rsa.ErrVerification`, which lets the caller
/// try the other key, and everything else, which does not.
enum SignatureFailure {
    Verification,
    Key(String),
}

/// Port of `verifyLicenseSignature` (utils/license.go:148): PKCS#1 v1.5 over a SHA-512 digest.
fn verify_license_signature(
    public_key_pem: &str,
    digest: &[u8],
    signature: &[u8],
) -> Result<(), SignatureFailure> {
    let key = rsa::RsaPublicKey::from_public_key_pem(public_key_pem).map_err(|err| {
        SignatureFailure::Key(format!("encountered error parsing public key: {err}"))
    })?;
    match key.verify(rsa::Pkcs1v15Sign::new::<sha2::Sha512>(), digest, signature) {
        Ok(()) => Ok(()),
        Err(rsa::Error::Verification) => Err(SignatureFailure::Verification),
        Err(err) => Err(SignatureFailure::Key(err.to_string())),
    }
}

/// Validate, parse, and default — what `ValidateAndSetLicenseBytes` (platform/license.go:263)
/// followed by `SetLicense` leaves in memory.
///
/// `Features.SetDefaults()` runs in `SetLicense`, so a stored licence with a sparse `features`
/// object has every flag set by the time anything reads it — and `GetClientLicense` dereferences
/// every one of them, so it must. A licence with **no** `features` object at all would make
/// that `SetDefaults` a method call on a nil pointer in Go (it dereferences `f.FutureFeatures`),
/// i.e. a crash at load; here it is refused as invalid, which is the closest a process that must
/// keep answering can come to a process that stops.
pub fn load_license(signed: &[u8], keys: &LicenseKeys) -> Result<License, LicenseValidationError> {
    let plaintext = validate_license(signed, keys)?;
    let mut license: License = serde_json::from_slice(&plaintext)
        .map_err(|err| LicenseValidationError::Json(err.to_string()))?;
    match license.features.as_mut() {
        Some(features) => features.set_defaults(),
        None => {
            return Err(LicenseValidationError::Json(
                "license.Features is nil".to_owned(),
            ));
        }
    }
    Ok(license)
}

/// Port of `utils.GetClientLicense` (utils/license.go:212) — the `map[string]string` every client
/// reads before it renders anything.
///
/// Every value is a **string**: `strconv.FormatBool`, `strconv.Itoa`, `strconv.FormatInt`. The
/// keys are Go's identifiers, not the JSON tags (`IDLoadedPushNotifications`, not `id_loaded`),
/// and `AutoTranslation`, `AdvancedLogging`, `EnterprisePlugins`, `ThemeManagement` and the
/// limits are **absent** — the map is a hand-written subset of `Features`, not `ToMap`.
///
/// Go dereferences `l.Customer` and every feature pointer. After `SetDefaults` the features are
/// never nil; `Customer` can be, and Go would panic. A missing customer is three empty strings
/// here, for the same reason `load_license` refuses a missing `features`: a process that must
/// keep answering does the least surprising thing where Go stops.
pub fn client_license(license: Option<&License>) -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();
    props.insert("IsLicensed".to_owned(), license.is_some().to_string());

    let Some(l) = license else {
        return props;
    };
    let features = l.features.clone().unwrap_or_default();
    let customer = l.customer.clone().unwrap_or_default();
    let flag = |value: Option<bool>| value.unwrap_or(false).to_string();

    let mut put = |key: &str, value: String| {
        props.insert(key.to_owned(), value);
    };
    put("Id", l.id.clone());
    put("SkuName", l.sku_name.clone());
    put("SkuShortName", l.sku_short_name.clone());
    put("Users", features.users.unwrap_or(0).to_string());
    put("LDAP", flag(features.ldap));
    put("LDAPGroups", flag(features.ldap_groups));
    put("MFA", flag(features.mfa));
    put("SAML", flag(features.saml));
    put("Cluster", flag(features.cluster));
    put("Metrics", flag(features.metrics));
    put("GoogleOAuth", flag(features.google_oauth));
    put("Office365OAuth", flag(features.office365_oauth));
    put("OpenId", flag(features.open_id));
    put("Compliance", flag(features.compliance));
    put("MHPNS", flag(features.mhpns));
    put("Announcement", flag(features.announcement));
    put("Elasticsearch", flag(features.elasticsearch));
    put("DataRetention", flag(features.data_retention));
    put(
        "IDLoadedPushNotifications",
        flag(features.id_loaded_push_notifications),
    );
    put("IssuedAt", l.issued_at.to_string());
    put("StartsAt", l.starts_at.to_string());
    put("ExpiresAt", l.expires_at.to_string());
    put("Name", customer.name);
    put("Email", customer.email);
    put("Company", customer.company);
    put(
        "EmailNotificationContents",
        flag(features.email_notification_contents),
    );
    put("MessageExport", flag(features.message_export));
    put(
        "CustomPermissionsSchemes",
        flag(features.custom_permissions_schemes),
    );
    put("GuestAccounts", flag(features.guest_accounts));
    put(
        "GuestAccountsPermissions",
        flag(features.guest_accounts_permissions),
    );
    put(
        "CustomTermsOfService",
        flag(features.custom_terms_of_service),
    );
    put(
        "LockTeammateNameDisplay",
        flag(features.lock_teammate_name_display),
    );
    put("Cloud", flag(features.cloud));
    put("SharedChannels", flag(features.shared_channels));
    put(
        "RemoteClusterService",
        flag(features.remote_cluster_service),
    );
    put(
        "OutgoingOAuthConnections",
        flag(features.outgoing_oauth_connections),
    );
    put("IsTrial", l.is_trial.to_string());
    put("IsGovSku", l.is_gov_sku.to_string());
    put("IsNonProduction", l.is_non_production.to_string());
    props
}

/// The seven keys `utils.GetSanitizedClientLicense` (utils/license.go:273) deletes for a caller
/// without `read_license_information`.
pub const SANITIZED_CLIENT_LICENSE_KEYS: [&str; 7] = [
    "Id",
    "Name",
    "Email",
    "IssuedAt",
    "StartsAt",
    "ExpiresAt",
    "SkuName",
];

/// Port of `utils.GetSanitizedClientLicense` — a copy with the seven keys removed. It only ever
/// deletes, so on the unlicensed map it is the identity.
pub fn sanitized_client_license(license: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut sanitized = license.clone(); // Go copies the map too
    for key in SANITIZED_CLIENT_LICENSE_KEYS {
        sanitized.remove(key);
    }
    sanitized
}

/// What `MM_LICENSE` resolved to, decided once when the [`App`] is built — the way Go decides it
/// once in `LoadLicense`.
///
/// Three states, not two: Go **returns** when the variable is set and invalid, without consulting
/// the database at all ("Failed to read license set in environment."). An invalid `MM_LICENSE`
/// therefore hides a perfectly good `ActiveLicenseId`, and so does this.
#[derive(Debug, Clone)]
pub enum EnvLicense {
    Unset,
    Loaded(Arc<License>),
    Invalid,
}

impl EnvLicense {
    /// Resolve `MM_LICENSE` against the keys, logging the failure Go logs.
    ///
    /// Not ported: the trial-eligibility question Go asks its licence manager for a trial
    /// licence (`CanStartTrial`). The manager is enterprise code absent from the open-source
    /// tree, where that call is a nil dereference — so a trial through `MM_LICENSE` is a crash
    /// on the reference server and simply a licence here.
    pub fn resolve(raw: &str, keys: &LicenseKeys) -> Self {
        if raw.is_empty() {
            return Self::Unset;
        }
        match load_license(raw.as_bytes(), keys) {
            Ok(license) => {
                tracing::info!("License key from ENV is valid, unlocking enterprise features.");
                Self::Loaded(Arc::new(license))
            }
            Err(err) => {
                tracing::error!(error = %err, "Failed to read license set in environment.");
                Self::Invalid
            }
        }
    }
}

/// The verified licence for one `Licenses.Id`, so the signature is checked once per licence.
pub(crate) type LicenseCache = std::sync::RwLock<Option<(String, Arc<License>)>>;

impl App {
    /// Port of `App.License()` — the licence this installation runs under, or `None`.
    ///
    /// The sources and their order are the module's. Two of Go's outcomes are folded into
    /// `None` because Go folds them too: an `ActiveLicenseId` that is not a valid id, and a row
    /// whose bytes do not verify ("License key is invalid." — the licence stays what it was,
    /// which at startup is nil). A store failure is a 500, not `None`; see
    /// [`license_state_error`].
    #[tracing::instrument(skip_all, fields(source, id))]
    pub async fn license(&self) -> AppResult<Option<Arc<License>>> {
        match &self.env_license {
            EnvLicense::Loaded(license) => {
                tracing::Span::current().record("source", "MM_LICENSE");
                return Ok(Some(Arc::clone(license)));
            }
            EnvLicense::Invalid => {
                tracing::Span::current().record("source", "MM_LICENSE (invalid)");
                return Ok(None);
            }
            EnvLicense::Unset => {}
        }

        let active = self
            .store()
            .system()
            .get_by_name(SYSTEM_ACTIVE_LICENSE_ID)
            .await
            .map_err(license_state_error)?;
        // `IsValidId` is the test Go applies to the stored id (platform/license.go:92) before it
        // will look a licence up — an empty or malformed value there means "no licence", which
        // is exactly what `RemoveLicense` leaves behind when it blanks the row rather than
        // deleting it.
        let Some(id) = active.filter(|id| mm_model::utils::is_valid_id(id)) else {
            tracing::Span::current().record("source", "none");
            return Ok(None);
        };
        tracing::Span::current().record("id", &id);

        if let Some(license) = self.cached_license(&id) {
            tracing::Span::current().record("source", "Licenses (cached)");
            return Ok(Some(license));
        }

        let Some(record) = self
            .store()
            .license()
            .get(&id)
            .await
            .map_err(license_state_error)?
        else {
            // Go: "License key from https://mattermost.com required to unlock enterprise
            // features." — the id names nothing, and the Mattermost Entry arm is not ported.
            tracing::Span::current().record("source", "Licenses (no row)");
            return Ok(None);
        };

        match load_license(record.bytes.as_bytes(), &self.license_keys) {
            Ok(license) => {
                tracing::Span::current().record("source", "Licenses");
                let license = Arc::new(license);
                self.remember_license(&id, &license);
                Ok(Some(license))
            }
            Err(err) => {
                tracing::warn!(error = %err, id = %id, "License key is invalid.");
                Ok(None)
            }
        }
    }

    /// [`App::license`] as a yes-or-no.
    #[tracing::instrument(skip_all, fields(state))]
    pub async fn license_state(&self) -> AppResult<LicenseState> {
        let state = match self.license().await? {
            Some(_) => LicenseState::Licensed,
            None => LicenseState::Unlicensed,
        };
        tracing::Span::current().record("state", format!("{state:?}"));
        Ok(state)
    }

    /// Port of `Server.ClientLicense()` (platform/license.go:303): the full client map.
    pub async fn client_license(&self) -> AppResult<BTreeMap<String, String>> {
        Ok(client_license(self.license().await?.as_deref()))
    }

    /// Port of `Server.GetSanitizedClientLicense()` (platform/license.go:335).
    pub async fn sanitized_client_license(&self) -> AppResult<BTreeMap<String, String>> {
        Ok(sanitized_client_license(&self.client_license().await?))
    }

    fn cached_license(&self, id: &str) -> Option<Arc<License>> {
        // A poisoned lock means a panic while holding it, which the cache's two-line critical
        // sections cannot produce; treating it as empty re-verifies, which is always correct.
        let guard = self.license_cache.read().ok()?;
        guard
            .as_ref()
            .filter(|(cached_id, _)| cached_id == id)
            .map(|(_, license)| Arc::clone(license))
    }

    fn remember_license(&self, id: &str, license: &Arc<License>) {
        if let Ok(mut guard) = self.license_cache.write() {
            *guard = Some((id.to_owned(), Arc::clone(license)));
        }
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
    tracing::error!(error = ?err, "reading the active licence failed");
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
        assert_eq!(
            sanitized_client_license(&map),
            map,
            "sanitising deletes nothing here"
        );
    }

    /// The two Mattermost keys are the reference's, byte for byte, and are usable RSA keys.
    #[test]
    fn the_compiled_in_keys_parse() {
        for pem in [PRODUCTION_PUBLIC_KEY, TEST_PUBLIC_KEY] {
            rsa::RsaPublicKey::from_public_key_pem(pem).expect("a PKIX RSA public key");
        }
        assert_ne!(PRODUCTION_PUBLIC_KEY, TEST_PUBLIC_KEY);
        let production = LicenseKeys::for_environment("production");
        let dev = LicenseKeys::for_environment("dev");
        assert_eq!(production.primary, dev.alternate);
        assert_eq!(production.alternate, dev.primary);
        assert_eq!(
            LicenseKeys::for_environment("test").primary,
            TEST_PUBLIC_KEY
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_license.json"))
            .expect("behaviour_license.json is generated by reference/dump")
    }

    /// `GetClientLicense` and `GetSanitizedClientLicense`, key for key and value for value,
    /// over the oracle's own licence, a sparse one whose values all come from `SetDefaults`, and
    /// `nil`. The sparse case is the one that catches a port defaulting a flag the wrong way,
    /// because `future_features:false` there makes "everything true" visibly wrong.
    #[test]
    fn the_client_maps_match_go() {
        let oracle = oracle();
        let cases = oracle["client_license"].as_array().unwrap();
        assert_eq!(cases.len(), 3);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let license: Option<License> = serde_json::from_value(case["license"].clone())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let license = license.map(|mut l| {
                // `SetLicense` runs `Features.SetDefaults()` before the map is built, and the
                // corpus did the same.
                if let Some(f) = l.features.as_mut() {
                    f.set_defaults();
                }
                l
            });
            let full = client_license(license.as_ref());
            let expected: BTreeMap<String, String> =
                serde_json::from_value(case["full"].clone()).unwrap();
            assert_eq!(full, expected, "{name}: full map");
            let expected_sanitized: BTreeMap<String, String> =
                serde_json::from_value(case["sanitized"].clone()).unwrap();
            assert_eq!(
                sanitized_client_license(&full),
                expected_sanitized,
                "{name}: sanitized map"
            );
        }
    }

    /// The pre-signature half of `ValidateLicense`: which refusal each malformed input gets.
    /// The decoder's inner message differs between the two implementations, so `decode` is
    /// compared by family; the other two are compared as text.
    #[test]
    fn the_validator_refuses_each_malformed_input_the_way_go_does() {
        let oracle = oracle();
        let keys = LicenseKeys::for_environment("dev");
        let cases = oracle["validate"].as_array().unwrap();
        assert!(cases.len() >= 11);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["input"].as_str().unwrap();
            let kind = case["kind"].as_str().unwrap();
            let err = validate_license(input.as_bytes(), &keys)
                .expect_err("nothing in the corpus is signed");
            match kind {
                "decode" => assert!(
                    matches!(err, LicenseValidationError::Decode(_)),
                    "{name}: expected a decode error, got {err}"
                ),
                _ => assert_eq!(err.to_string(), case["error"].as_str().unwrap(), "{name}"),
            }
        }
    }
}

/// Keys minted in-process and licences signed with them, for every test in this crate that needs
/// a licence to *load*. The reference's private keys are not in the tree, so this is the only way
/// the accepting branch can be exercised without the stack; the licensed Go oracle covers the same
/// branch over HTTP with the stack's key.
#[cfg(test)]
pub(crate) mod test_signing {
    use super::*;
    use rsa::pkcs8::EncodePublicKey;

    pub(crate) fn keypair() -> (rsa::RsaPrivateKey, String) {
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("keygen");
        let public = private
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .expect("pem");
        (private, public)
    }

    /// Mattermost's format, as `scripts/go-licensed.sh` produces it with openssl.
    pub(crate) fn sign(private: &rsa::RsaPrivateKey, plaintext: &[u8]) -> String {
        let digest = sha2::Sha512::digest(plaintext);
        let signature = private
            .sign(rsa::Pkcs1v15Sign::new::<sha2::Sha512>(), &digest)
            .expect("signs");
        assert_eq!(signature.len(), SIGNATURE_LENGTH);
        let mut signed = plaintext.to_vec();
        signed.extend_from_slice(&signature);
        base64::engine::general_purpose::STANDARD.encode(signed)
    }

    /// A `Config` whose `MM_LICENSE` is a licence of `sku` that verifies: the signed body and
    /// the PEM of the key it was signed with, in the two fields `App::with_config` reads.
    pub(crate) fn licensed_config(sku: &str) -> crate::config::Config {
        licensed_config_from(&format!(
            r#"{{"id":"mmrslicensedtestkey0000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{{"id":"c","name":"n","email":"e","company":"co"}},"features":{{"users":10}},"sku_name":"{sku}","sku_short_name":"{sku}"}}"#
        ))
    }

    /// [`licensed_config`] over an arbitrary licence body, for the tests whose subject is a
    /// field other than the SKU.
    pub(crate) fn licensed_config_from(body: &str) -> crate::config::Config {
        let (private, public) = keypair();
        crate::config::Config {
            license: sign(&private, body.as_bytes()),
            license_public_key: Some(public),
            ..crate::config::Config::default()
        }
    }
}

/// Signature verification end to end, with keys minted here.
#[cfg(test)]
mod signing {
    use super::test_signing::{keypair, sign};
    use super::*;

    const LICENSE: &str = r#"{"id":"mmrslicensedoracle00000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{"id":"c","name":"n","email":"e","company":"co"},"features":{"users":10},"sku_name":"Enterprise","sku_short_name":"enterprise"}"#;

    #[test]
    fn a_licence_signed_with_the_primary_key_verifies_and_loads_with_defaults() {
        let (private, public) = keypair();
        let keys = LicenseKeys::single(public, "dev");
        let signed = sign(&private, LICENSE.as_bytes());

        assert_eq!(
            validate_license(signed.as_bytes(), &keys).unwrap(),
            LICENSE.as_bytes()
        );
        let license = load_license(signed.as_bytes(), &keys).unwrap();
        assert_eq!(license.sku_short_name, "enterprise");
        // `SetDefaults` ran: a flag the JSON never mentioned is `true`, and `users` survived.
        let features = license.features.unwrap();
        assert_eq!(features.ldap, Some(true));
        assert_eq!(features.users, Some(10));
        assert_eq!(features.cloud, Some(false));
    }

    /// Go's decoder skips newlines, so a licence file with a trailing newline — the common
    /// case for one read from disk — still loads.
    #[test]
    fn newlines_in_the_base64_are_ignored() {
        let (private, public) = keypair();
        let keys = LicenseKeys::single(public, "dev");
        let signed = sign(&private, LICENSE.as_bytes());
        let mut wrapped = String::new();
        for (i, c) in signed.chars().enumerate() {
            if i > 0 && i % 64 == 0 {
                wrapped.push('\n');
            }
            wrapped.push(c);
        }
        wrapped.push_str("\r\n");
        assert!(validate_license(wrapped.as_bytes(), &keys).is_ok());
    }

    #[test]
    fn a_tampered_body_or_a_foreign_key_is_an_invalid_signature() {
        let (private, public) = keypair();
        let (_, other_public) = keypair();
        let signed = sign(&private, LICENSE.as_bytes());

        let foreign = LicenseKeys::single(other_public, "dev");
        assert_eq!(
            validate_license(signed.as_bytes(), &foreign),
            Err(LicenseValidationError::InvalidSignature)
        );

        let keys = LicenseKeys::single(public, "dev");
        let mut tampered = base64::engine::general_purpose::STANDARD
            .decode(&signed)
            .unwrap();
        tampered[0] ^= 0x01;
        let tampered = base64::engine::general_purpose::STANDARD.encode(tampered);
        assert_eq!(
            validate_license(tampered.as_bytes(), &keys),
            Err(LicenseValidationError::InvalidSignature)
        );
    }

    /// The alternate key matching is a *different* error, and which one depends on the
    /// environment the primary key belongs to.
    #[test]
    fn a_licence_from_the_other_environment_names_the_direction_of_the_mismatch() {
        let (private, public) = keypair();
        let (_, other_public) = keypair();
        let signed = sign(&private, LICENSE.as_bytes());

        let running_production = LicenseKeys {
            primary: other_public.clone(),
            alternate: public.clone(),
            environment: "production".to_owned(),
        };
        assert_eq!(
            validate_license(signed.as_bytes(), &running_production),
            Err(LicenseValidationError::TestInProductionEnvironment)
        );
        let running_dev = LicenseKeys {
            primary: other_public,
            alternate: public,
            environment: "dev".to_owned(),
        };
        assert_eq!(
            validate_license(signed.as_bytes(), &running_dev),
            Err(LicenseValidationError::ProductionInTestEnvironment)
        );
    }

    /// A malformed key is surfaced as itself, and the alternate is **not** tried — a broken
    /// primary key must never read as "a licence from the other environment".
    #[test]
    fn a_malformed_key_is_its_own_error_and_stops_the_search() {
        let (private, public) = keypair();
        let signed = sign(&private, LICENSE.as_bytes());
        let broken = LicenseKeys {
            primary: "not a pem".to_owned(),
            alternate: public,
            environment: "dev".to_owned(),
        };
        assert!(matches!(
            validate_license(signed.as_bytes(), &broken),
            Err(LicenseValidationError::Key(_))
        ));
    }

    /// A signature ending in a zero byte loses it to the null strip — Go's behaviour, kept.
    /// Built by hand rather than waited for: the strip is what makes 257 bytes into 256.
    #[test]
    fn trailing_zero_bytes_are_stripped_before_the_length_check() {
        let keys = LicenseKeys::for_environment("dev");
        let mut bytes = vec![0x41; 257];
        bytes[256] = 0;
        let signed = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(
            validate_license(signed.as_bytes(), &keys),
            Err(LicenseValidationError::TooShort)
        );
    }

    #[test]
    fn a_verified_licence_without_features_is_refused_rather_than_defaulted() {
        let (private, public) = keypair();
        let keys = LicenseKeys::single(public, "dev");
        let signed = sign(&private, br#"{"id":"x","sku_short_name":"enterprise"}"#);
        assert!(matches!(
            load_license(signed.as_bytes(), &keys),
            Err(LicenseValidationError::Json(_))
        ));
        let signed = sign(&private, b"not json");
        assert!(matches!(
            load_license(signed.as_bytes(), &keys),
            Err(LicenseValidationError::Json(_))
        ));
    }

    /// Go's `LoadLicense` **returns** when `MM_LICENSE` is set and invalid, so the database is
    /// never consulted: an unreachable store here is the proof that it was not.
    #[tokio::test]
    async fn an_invalid_env_licence_hides_the_database() {
        let (_, public) = keypair();
        let app = crate::App::with_config(
            mm_store::SqlStore::from_pool(
                sqlx::postgres::PgPoolOptions::new()
                    .acquire_timeout(std::time::Duration::from_millis(250))
                    .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
                    .expect("a lazy pool is built without connecting"),
            ),
            crate::config::Config {
                license: "garbage".to_owned(),
                license_public_key: Some(public),
                ..crate::config::Config::default()
            },
        );
        assert!(
            app.license()
                .await
                .expect("no store read happens")
                .is_none(),
            "invalid MM_LICENSE: unlicensed, and the store was not asked"
        );
    }

    #[test]
    fn an_env_licence_resolves_to_its_three_states() {
        let (private, public) = keypair();
        let keys = LicenseKeys::single(public, "dev");
        assert!(matches!(EnvLicense::resolve("", &keys), EnvLicense::Unset));
        assert!(matches!(
            EnvLicense::resolve("garbage", &keys),
            EnvLicense::Invalid
        ));
        let signed = sign(&private, LICENSE.as_bytes());
        assert!(matches!(
            EnvLicense::resolve(&signed, &keys),
            EnvLicense::Loaded(_)
        ));
    }
}
