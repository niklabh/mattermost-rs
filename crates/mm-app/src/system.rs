//! Ports of the `app` functions behind the migrated `/api/v4/system/*` and `/api/v4/cluster/*`
//! reads: the onboarding flag, the applied-migration table and the cluster roster.
//!
//! Go scatters these across `app/onboarding.go`, `app/server.go` and `app/admin.go`; they are one
//! module here because they are one route family and none of them is more than a store call and
//! a decision.

use mm_model::cluster_info::ClusterInfo;
use mm_model::system::{AppliedMigration, SYSTEM_FIRST_ADMIN_SETUP_COMPLETE, System};
use mm_model::utils::{AppError, AppResult};
use mm_store::SystemStore;

use crate::App;

impl App {
    /// Port of `App.GetOnboarding` (app/onboarding.go:90).
    ///
    /// # A missing row is `"false"`, not an error and not an absent object
    ///
    /// Go asks the store for `FirstAdminSetupComplete` and, on `ErrNotFound`, **synthesises** a
    /// `model.System` with the same name and the string `"false"` (onboarding.go:94-98). So a
    /// server that has never completed onboarding answers `200 {"name":"FirstAdminSetupComplete",
    /// "value":"false"}` rather than 404 — the client cannot distinguish "not set" from "set to
    /// false", and is not meant to.
    ///
    /// # The value is a string, because the `Systems` table is `map[string]string`
    ///
    /// `"true"` / `"false"` are four and five bytes of text on the wire. A port reaching for a
    /// `bool` changes the JSON shape of every system row.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_onboarding(&self) -> AppResult<System> {
        let value = self
            .store()
            .system()
            .get_by_name(SYSTEM_FIRST_ADMIN_SETUP_COMPLETE)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "onboarding flag lookup failed");
                AppError::boxed(
                    "getFirstAdminCompleteSetup",
                    "api.error_get_first_admin_complete_setup",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", value.is_some());
        Ok(onboarding_row(value))
    }

    /// Port of `App.GetAppliedSchemaMigrations` (app/server.go:2034).
    ///
    /// One store call and an error wrap whose id is `api.file.read_file.app_error` — a
    /// translation key about **reading a file**, on a query against `db_migrations`. That is what
    /// Go emits; it is not a transcription slip here.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_applied_schema_migrations(&self) -> AppResult<Vec<AppliedMigration>> {
        let migrations = self.store().get_applied_migrations().await.map_err(|err| {
            tracing::error!(error = ?err, "db_migrations read failed");
            AppError::boxed(
                "GetDBSchemaTable",
                "api.file.read_file.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        tracing::Span::current().record("found", migrations.len());
        Ok(migrations)
    }

    /// Port of `App.GetClusterStatus` (app/admin.go:133).
    ///
    /// # Unlicensed, the answer is an empty array and there is nothing else it could be
    ///
    /// Go's whole body is `if a.Cluster() == nil { return make([]*model.ClusterInfo, 0), nil }`
    /// and a delegation to the cluster interface otherwise. `a.Cluster()` is
    /// `einterfaces.ClusterInterface`, registered only by the enterprise build
    /// (`platform/service.go:544`), so on Team Edition — and on any unlicensed server — the first
    /// branch is the only reachable one. That is the same boundary MIGRATION.md records for the
    /// cache-invalidation bus: there is no cluster to ask, in either process.
    ///
    /// **`make(..., 0)`, not `nil`** — so the JSON is `[]` and never `null`. The distinction is
    /// the whole wire format of this route.
    ///
    /// **The licence does not enter into it** (re-measured 2026-09-13 against the licensed Go
    /// oracle, which answers `[]` too). The interface is registered by the enterprise
    /// repository's `init`, which is not in this tree, so a licence changes nothing on any build
    /// from it. The gossip roster itself is the cluster bus, owed under [D-087]; until it exists
    /// here there is no branch to take.
    #[tracing::instrument(skip_all)]
    pub async fn get_cluster_status(&self) -> AppResult<Vec<ClusterInfo>> {
        Ok(Vec::new())
    }
}

/// Go's synthesised `model.System` for the onboarding flag (onboarding.go:94-98).
///
/// A pure function of what the store found, because that *is* the branch: a missing row and a row
/// holding `"false"` must produce identical JSON.
///
/// **It lives here rather than inside the test module, and that distinction cost a mutation.**
/// The first version of this port inlined the `unwrap_or_else` in `get_onboarding` and gave the
/// test module its own byte-identical copy to assert against — so a mutation changing the real
/// default to `"true"` left the test passing against its own private copy. A test that reimplements
/// the thing it is testing is a test of itself.
fn onboarding_row(stored: Option<String>) -> System {
    System {
        name: SYSTEM_FIRST_ADMIN_SETUP_COMPLETE.to_owned(),
        value: stored.unwrap_or_else(|| "false".to_owned()),
    }
}

/// `latestVersionCache` (app/admin.go): one entry, kept for twenty-four hours.
///
/// A process-global rather than a field on [`App`] for the same reason Go's is a package
/// variable — there is exactly one upstream and one answer, and a second `App` value (the
/// licensed test server) sharing it is what Go does too.
static LATEST_VERSION_CACHE: std::sync::LazyLock<
    std::sync::Mutex<
        Option<(
            std::time::Instant,
            mm_model::github_release::GithubReleaseInfo,
        )>,
    >,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// `SetWithExpiry("latest_version_cache", …, 24*time.Hour)`.
const LATEST_VERSION_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

fn latest_version_error(err: impl std::error::Error + Send + Sync + 'static) -> Box<AppError> {
    Box::new(
        AppError::new(
            "GetLatestVersion",
            mm_model::utils::NO_TRANSLATION,
            None,
            String::new(),
            500,
        )
        .wrap(err),
    )
}

impl App {
    /// Port of `App.GetLatestVersion` (app/admin.go:204).
    ///
    /// The cached release if there is one under a day old; otherwise **one plain `GET`** of
    /// `latest_version_url` — Go's `http.Get`, so the default client: redirects followed (ten,
    /// as reqwest's default), **no outbound-connection guard** and **no timeout**, both
    /// reproduced rather than improved on, because the guard would refuse nothing here (the URL
    /// is a constant on the public internet) and a timeout would be a behaviour Go does not
    /// have. GitHub refuses a request without a `User-Agent`, which Go's client always sends;
    /// this one names this server.
    ///
    /// Every failure — transport, body, JSON, `IsValid` — is the same 500 with the
    /// [`NO_TRANSLATION`](mm_model::utils::NO_TRANSLATION) id and the cause as the detail.
    /// `json.Unmarshal` ignores GitHub's hundred other fields and this type's `serde(default)`
    /// tolerates absent ones, so the only shape GitHub can answer with that fails is a release
    /// whose `id` is zero.
    #[tracing::instrument(skip_all, fields(cached))]
    pub async fn get_latest_version(
        &self,
        latest_version_url: &str,
    ) -> AppResult<mm_model::github_release::GithubReleaseInfo> {
        if let Ok(cache) = LATEST_VERSION_CACHE.lock()
            && let Some((stored, release)) = cache.as_ref()
            && stored.elapsed() < LATEST_VERSION_TTL
        {
            tracing::Span::current().record("cached", true);
            // The cache hands out a copy, as Go's does; the handler serialises it and drops it.
            return Ok(release.clone());
        }
        tracing::Span::current().record("cached", false);

        let response = reqwest::Client::builder()
            .user_agent("mm-api")
            .build()
            .map_err(latest_version_error)?
            .get(latest_version_url)
            .send()
            .await
            .map_err(latest_version_error)?;
        let body = response.bytes().await.map_err(latest_version_error)?;
        let release: mm_model::github_release::GithubReleaseInfo =
            serde_json::from_slice(&body).map_err(latest_version_error)?;
        release
            .is_valid()
            .map_err(|err| latest_version_error(*err))?;

        if let Ok(mut cache) = LATEST_VERSION_CACHE.lock() {
            // The cache keeps its own copy; the caller gets the other.
            *cache = Some((std::time::Instant::now(), release.clone()));
        }
        Ok(release)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_row_is_indistinguishable_from_a_stored_false() {
        assert_eq!(onboarding_row(None), onboarding_row(Some("false".into())));
        assert_eq!(onboarding_row(None).value, "false");
        assert_eq!(
            onboarding_row(None).name,
            "FirstAdminSetupComplete",
            "the constant's value is not its Go name"
        );
    }

    /// A stored value is passed through untouched — including one that is neither `"true"` nor
    /// `"false"`. Go does no validation here and neither does this.
    #[test]
    fn a_stored_value_is_not_interpreted() {
        assert_eq!(onboarding_row(Some("true".into())).value, "true");
        assert_eq!(onboarding_row(Some(String::new())).value, "");
        assert_eq!(onboarding_row(Some("yes".into())).value, "yes");
    }
}

/// Why `Server.Restart` (app/server.go:856) did not exec.
#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    /// `errors.Wrap(err, "unable to restart because the system has not been upgraded")` with a
    /// non-nil `err` — the upgrader's last error.
    #[error("unable to restart because the system has not been upgraded: {0}")]
    NotUpgraded(String),
    /// `exec.LookPath(os.Args[0])` found nothing executable by that name.
    #[error("exec: \"{0}\": executable file not found in $PATH")]
    NotFound(String),
    /// `os.Stat` or `syscall.Exec` itself.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Port of `exec.LookPath` (os/exec/lp_unix.go): a name with a slash is checked as given;
/// anything else is searched for along `PATH`. "Executable" is a regular file with any execute
/// bit set.
fn look_path(name: &str) -> Result<std::path::PathBuf, RestartError> {
    use std::os::unix::fs::PermissionsExt;
    let is_executable = |path: &std::path::Path| {
        std::fs::metadata(path)
            .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = std::path::PathBuf::from(name);
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(RestartError::NotFound(name.to_owned()))
        };
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let dir = if dir.as_os_str().is_empty() {
            std::path::PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(RestartError::NotFound(name.to_owned()))
}

impl App {
    /// Port of `App.RecycleDatabaseConnection` (app/admin.go:148): the two log lines and the
    /// store's recycle between them — see `SqlStore::recycle_db_connections` for how far sqlx
    /// lets that go.
    #[tracing::instrument(skip(self))]
    pub async fn recycle_database_connection(&self) {
        tracing::info!("Attempting to recycle database connections.");
        self.store().recycle_db_connections().await;
        tracing::info!("Finished recycling database connections.");
    }

    /// Port of `Server.InvalidateAllCaches` (app/admin.go:140) →
    /// `PlatformService.InvalidateAllCachesSkipSend` (platform/cluster_handlers.go:137), for a
    /// server with no cluster to forward the message to.
    ///
    /// # What there is to invalidate here
    ///
    /// Go purges the session cache, the status cache, six store-level caches and the
    /// link-metadata cache, and then reloads the licence. This port keeps two of those in memory —
    /// the status cache and the verified licence — and those two are what is dropped; sessions
    /// and every store read go to the table on each request ([D-087]).
    ///
    /// **The hub is not touched**, and until 2026-09-15 it was. `InvalidateAllCachesSkipSend`
    /// calls `ClearAllUsersSessionCacheLocal`, the cache purge alone — not
    /// `ClearSessionCacheForAllUsersSkipClusterSend`, which is the one that also reaches every
    /// connection (cluster_handlers.go:85). So a websocket connection's membership cache and
    /// session survive this call on Go, and now here.
    ///
    /// # It does not reach the Go server's caches
    ///
    /// While the proxy is on, the Go process beside this one holds its own copies of all of
    /// the above and this call cannot touch them; the handler
    /// (`mm_api::sysops::invalidate_caches`) forwards a second copy of the request there for
    /// exactly that reason, and says why.
    #[tracing::instrument(skip(self))]
    pub fn invalidate_all_caches(&self) -> AppResult {
        tracing::info!("Purging all caches");
        self.status_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        // `ps.LoadLicense()`: the next `license()` re-reads `ActiveLicenseId` and re-verifies
        // the row rather than answering from the cache.
        if let Ok(mut guard) = self.license_cache.write() {
            *guard = None;
        }
        Ok(())
    }

    /// Port of `App.CheckIntegrity` (app/app.go:148), drained: Go hands back the channel the
    /// store streams into and the handler collects it; the store here answers the collected
    /// list directly.
    pub async fn check_integrity(&self) -> Vec<mm_model::integrity::IntegrityCheckResult> {
        self.store().check_integrity().await
    }

    /// Port of `Server.Restart` (app/server.go:856).
    ///
    /// # On a server that has not been upgraded, this is a no-op that reports success
    ///
    /// `UpgradeToE0Status()` is `(0, nil)` unless an upgrade ran, and the guard is
    /// `errors.Wrap(err, "unable to restart …")` — which for a nil `err` is **nil**. So the
    /// common case returns no error, execs nothing, and the handler that called it logs nothing:
    /// `POST /api/v4/restart` on a stock server is a 200 after a one-second sleep and no
    /// restart at all. Only an upgrader error is reported, and only a completed upgrade
    /// (`percentage == 100`) execs — `exec.LookPath(os.Args[0])`, a stat, then `syscall.Exec`
    /// with the same arguments and environment, which replaces this process in place. No
    /// upgrade ever completes here (see `crate::upgrader`), so the exec is written down and
    /// unreachable.
    #[tracing::instrument(skip(self))]
    pub fn restart(&self) -> Result<(), RestartError> {
        let (percentage, error) = crate::upgrader::upgrade_to_e0_status();
        if error.is_some() || percentage != 100 {
            return match error {
                Some(err) => Err(RestartError::NotUpgraded(err)),
                None => Ok(()),
            };
        }

        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let arg0 = args
            .first()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_default();
        let argv0 = look_path(&arg0)?;
        std::fs::metadata(&argv0)?;
        tracing::info!("Restarting server");
        // `syscall.Exec` returns only on failure; so does this.
        let err = std::os::unix::process::CommandExt::exec(
            std::process::Command::new(&argv0).args(args.iter().skip(1)),
        );
        Err(RestartError::Io(err))
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    /// A name with a slash is not searched; a bare name is found on `PATH`; neither is found
    /// when nothing executable is there.
    #[test]
    fn look_path_matches_exec_look_path() {
        assert!(look_path("sh").is_ok());
        assert!(look_path("/bin/sh").is_ok() || look_path("/usr/bin/sh").is_ok());
        assert!(matches!(
            look_path("mmrs-no-such-executable-anywhere"),
            Err(RestartError::NotFound(_))
        ));
        assert!(matches!(
            look_path("./mmrs-no-such-executable-anywhere"),
            Err(RestartError::NotFound(_))
        ));
    }
}
