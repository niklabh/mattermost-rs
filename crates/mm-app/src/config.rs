//! Port of the `model.Config` settings the authorization checks read — and nothing else.
//!
//! `config.go` is 5,795 lines and `MIGRATION.md` records the decision to translate it lazily,
//! section by section. This is the first section, pulled across because two checks in
//! `authorization.go` consult it and cannot be ported without it:
//!
//! | Setting | Read by | Go default |
//! |---|---|---|
//! | `ExperimentalSettings.RestrictSystemAdmin` | `SessionHasPermissionToAndNotRestrictedAdmin` (authorization.go:31) | `false` (config.go:1268) |
//! | `ComplianceSettings.Enable` | `HasPermissionToReadChannel` (authorization.go:475) | `false` (config.go:2874) |
//!
//! # Why this is not read from the database
//!
//! The Go server we run beside keeps its configuration in a **file**, not in the shared
//! Postgres: `docker-compose.yml` mounts the `mattermost-config` volume at
//! `/mattermost/config`, and `MM_CONFIG` is unset, so the store is `config.json` on a volume
//! this process cannot see. The strangler-fig deployment shares a database, not a filesystem —
//! so unlike every other value we serve, this one has no shared source of truth to read.
//!
//! What we do instead is read **the same environment variables the Go server reads**. Mattermost
//! overlays `MM_<SECTION>_<SETTING>` over whatever the file says, so an operator who configures
//! the Go server by environment — which is how `docker-compose.yml` configures it today, see
//! `MM_SQLSETTINGS_DATASOURCE` — gets the identical value on both servers for free. An operator
//! who edits `config.json` directly does not, and that is the divergence [D-156] records.
//!
//! # Which direction each default fails
//!
//! This matters more than the defaults themselves, because the two settings fail in *opposite*
//! directions when we are wrong about them:
//!
//! - `restrict_system_admin = false` makes `SessionHasPermissionToAndNotRestrictedAdmin`
//!   behave exactly like `SessionHasPermissionTo`. Being wrong here **over-grants**: we would
//!   admit a restricted system admin that Go denies.
//! - `compliance_enable = false` takes the public-channel fallback branch in
//!   `HasPermissionToReadChannel`, which is the *permissive* one. Being wrong here
//!   **over-grants** too: we would let a non-member read a public channel that Go, with
//!   compliance on, confines to members.
//!
//! Both are `false` in Go and both over-grant if that is wrong, so neither is a safe assumption
//! to bury. Note also that `authorization.go:475` reads `ComplianceSettings.Enable` **without**
//! consulting the licence, even though every compliance *feature* is licence-gated
//! (`app/compliance.go:18`). So "Team Edition cannot enable compliance" is not a proof that this
//! branch is unreachable — the setting alone moves it.

/// Port of `model.Config` (config.go), restricted to the fields a migrated code path reads.
///
/// Deliberately not a lazily-grown mirror of the whole struct: a field appears here when
/// something ported consults it, and its doc comment names the caller. A config field with no
/// reader is a guess about the wire format that nothing can falsify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `ExperimentalSettings.RestrictSystemAdmin` (config.go:1247).
    ///
    /// When true, Go's `SessionHasPermissionToAndNotRestrictedAdmin` denies **every** caller
    /// that is not unrestricted — it does not fall through to a role check. Read by
    /// [`crate::App::session_has_permission_to_and_not_restricted_admin`].
    pub restrict_system_admin: bool,

    /// `ComplianceSettings.Enable` (config.go:2874).
    ///
    /// When true, `HasPermissionToReadChannel` stops falling back to `read_public_channel` for
    /// open channels, confining reads to members so the compliance export sees every access.
    /// Read by [`crate::App::has_permission_to_read_channel`].
    pub compliance_enable: bool,
}

impl Default for Config {
    /// Go's `SetDefaults` for exactly these two fields. Both are `false`.
    fn default() -> Self {
        Self {
            restrict_system_admin: false,
            compliance_enable: false,
        }
    }
}

impl Config {
    /// Read the environment overlay the Go server reads, falling back to Go's defaults.
    ///
    /// The variable names are Mattermost's own `MM_<SECTION>_<SETTING>` convention, so this
    /// agrees with the neighbouring Go server whenever that server is configured by environment.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            restrict_system_admin: env_bool(
                "MM_EXPERIMENTALSETTINGS_RESTRICTSYSTEMADMIN",
                default.restrict_system_admin,
            ),
            compliance_enable: env_bool("MM_COMPLIANCESETTINGS_ENABLE", default.compliance_enable),
        }
    }
}

/// `strconv.ParseBool` (Go strconv/atob.go:10) — the exact set of accepted spellings, and
/// `None` for everything else.
///
/// Go's list is closed and case-sensitive apart from the six forms below: `TRUE`, `True` and
/// `true` parse, but `tRuE` and `yes` do not. Widening it to `eq_ignore_ascii_case` would accept
/// values the Go server rejects, which is how the two configurations drift apart.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// [`parse_bool`] over an environment variable, with an absent *or* unparseable value falling
/// back to the default.
///
/// The fallback direction is Go's: viper leaves the setting at its configured default when an
/// override does not parse, so `MM_COMPLIANCESETTINGS_ENABLE=yes` is *not* true on either
/// server. Treating an unparseable value as `true` would silently diverge on a typo.
fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .and_then(|raw| parse_bool(&raw))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_defaults_are_both_false() {
        let config = Config::default();
        assert!(!config.restrict_system_admin, "config.go:1269 — new(false)");
        assert!(!config.compliance_enable, "config.go:2875 — new(false)");
    }

    /// Exactly `strconv.ParseBool`'s twelve spellings, and nothing else.
    #[test]
    fn parse_bool_matches_go_strconv() {
        for raw in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(raw), Some(true), "{raw} should parse true");
        }
        for raw in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw} should parse false");
        }
    }

    /// The near-misses. Each of these is a value an operator plausibly writes, and Go rejects
    /// every one — so each must fall back rather than being read as true.
    #[test]
    fn parse_bool_rejects_everything_go_rejects() {
        for raw in [
            "yes", "no", "on", "off", "tRuE", "TrUe", "y", "n", "2", "-1", "", " true", "true ",
            "enabled",
        ] {
            assert_eq!(parse_bool(raw), None, "{raw:?} must not parse");
        }
    }

    /// An absent key takes the default in **both** directions — a fallback that always returned
    /// `false` would look correct against Go's defaults while ignoring its argument entirely.
    ///
    /// `env_bool` is exercised through a key nothing sets rather than by mutating the
    /// environment: the process environment is global, and writing it from one test races every
    /// other test in the binary.
    #[test]
    fn an_absent_key_takes_the_default_either_way() {
        assert!(env_bool("MMRS_CERTAINLY_ABSENT_KEY", true));
        assert!(!env_bool("MMRS_CERTAINLY_ABSENT_KEY", false));
    }
}
