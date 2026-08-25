//! Port of `model/service_environment.go` plus its two build-tagged defaults.
//!
//! The environment decides which public key validates enterprise licences, which telemetry keys
//! are live, and which Stripe keys are used. Go keeps it **out of `model.Config` on purpose** —
//! it must never be persisted to the config store or settable by any route, only by
//! `MM_SERVICEENVIRONMENT` in the process environment.
//!
//! # The default half is a build tag
//!
//! `getDefaultServiceEnvironment` has two definitions: `production` builds return
//! [`SERVICE_ENVIRONMENT_PRODUCTION`], everything else returns [`SERVICE_ENVIRONMENT_DEV`]. Rust's
//! equivalent would be a Cargo feature, and — as with `fips.rs` — declaring one that no build
//! profile sets would claim a guarantee this workspace does not make. So the dev default is what
//! [`default_service_environment`] returns, with the switch documented.

/// Port of `model.ServiceEnvironmentProduction` (service_environment.go:12).
pub const SERVICE_ENVIRONMENT_PRODUCTION: &str = "production";
/// Port of `model.ServiceEnvironmentTest` (service_environment.go:15).
pub const SERVICE_ENVIRONMENT_TEST: &str = "test";
/// Port of `model.ServiceEnvironmentDev` (service_environment.go:19).
pub const SERVICE_ENVIRONMENT_DEV: &str = "dev";

/// The env var that selects the environment. Read directly, never through the config store.
pub const SERVICE_ENVIRONMENT_ENV_VAR: &str = "MM_SERVICEENVIRONMENT";

/// Port of `getDefaultServiceEnvironment` (service_environment_dev_default.go:5) — the
/// non-`production` build. See the module docs.
pub fn default_service_environment() -> &'static str {
    SERVICE_ENVIRONMENT_DEV
}

/// Port of `model.GetServiceEnvironment` (service_environment.go:32).
///
/// The value is trimmed and lowercased before matching, and **anything unrecognised silently
/// falls back to the default** — there is no error path, so a typo in the variable is invisible.
pub fn get_service_environment() -> &'static str {
    let raw = std::env::var(SERVICE_ENVIRONMENT_ENV_VAR).unwrap_or_default();
    // Go: strings.TrimSpace(strings.ToLower(...)). `go_to_lower` rather than `to_lowercase`
    // because the two disagree on `İ` and final sigma — see `utils::go_to_lower`.
    let normalised = crate::utils::go_to_lower(&raw);
    match normalised.trim() {
        SERVICE_ENVIRONMENT_PRODUCTION => SERVICE_ENVIRONMENT_PRODUCTION,
        SERVICE_ENVIRONMENT_TEST => SERVICE_ENVIRONMENT_TEST,
        SERVICE_ENVIRONMENT_DEV => SERVICE_ENVIRONMENT_DEV,
        _ => default_service_environment(),
    }
}
