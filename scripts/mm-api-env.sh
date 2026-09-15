# The environment `mm-api` must run with to agree with the Go server beside it.
#
#   source "$ROOT/scripts/mm-api-env.sh"
#   mmrs_launch_mm_api /path/to/logfile
#
# `scripts/go-server.sh` sets several settings as **environment overrides**, and an override
# never reaches the configuration document in the database — which is the document `mm-api`
# reads. So without these the two servers disagree about configuration they are both supposed to
# share, and a route gated on one of them answers differently on each side for a reason that has
# nothing to do with the port. `MM_FILESETTINGS_DIRECTORY` was the first (a 404 for every file
# that exists); `MM_TEAMSETTINGS_ENABLEOPENSERVER` is the second, and it is the difference
# between a 400 and a page of users on `GET /api/v4/users/invalid_emails`.
# `MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD` is the third, and it is a **feature flag**
# rather than a setting: `FeatureFlags` is stripped before the configuration document is
# persisted (config/store.go:306), so the environment is the only place either server can read it
# from. It opens `PUT /channels/members/{user_id}/direct/read` and
# `PUT /users/{user_id}/teams/{team_id}/read`, which are a 501 without it.
#
# The three ports, the database and the file directory all come from `stack-env.sh`, so this
# launches the mm-api **of the current stack** — which is what lets several worktrees test at once.
#
# **Scoped to the launch and never exported.** `cargo test` runs in the same shell, and
# `config::go_parity::the_env_overlay_preserves_document_values_it_does_not_name` asserts that no
# `MM_` variable is set — an export here fails a unit test that has nothing to do with the code
# it covers.
#
# **The last four arrived with the `/api/v4/config` reads.** `GET /config/client` answers with
# `SiteURL`, and `GET /config` and `GET /config/environment` answer with the whole overlay — so
# for those three the environment *is* the subject of the response and not merely a setting that
# colours one. `LISTENADDRESS` is deliberately the **Go** server's port, not this process's: the
# route reports the configuration the pair shares, and mm-api's own port comes from
# `MM_API_LISTEN`, which no Mattermost setting is named after.
#
# `MM_SQLSETTINGS_DATASOURCE` is **Go's DSN string, verbatim** — `DATABASE_URL` plus the
# `?sslmode=disable&connect_timeout=10` that `go-server.sh` appends — and not merely present.
# `GET /config` masks it to `FakeSetting` on both servers, and `GET /config/environment` only
# reports that it is set; but `localGetConfig` over the unix socket is **unsanitized** (see
# `mm_api::local_misc`), so the socket parity suite compares the value itself. This process
# still connects with `DATABASE_URL`; the variable is only what the config routes project.
#
# One override still differs and cannot be reconciled by this file: `MM_FILESETTINGS_DIRECTORY` is
# rooted at whichever checkout launched each server, so `FileSettings.Directory` is exempted by
# name in the `config_reads` parity suite.
#
# Both launch sites source this. They used to differ: `parity.sh` set the file directory and
# `mutate.sh` did not, so every `api`-suite mutation ran against a server configured unlike the
# one the tests were written against.
#
# **Launched from the Go server's run directory, since 2026-09-15.** `GET /api/v4/logs` and its
# two siblings read `LogSettings.FileLocation`, which is `""` on a stock server and then means
# "the `logs` directory found beside the working directory or the binary" (`fileutils.FindDir`),
# validated against a logging root found the same way. Two servers resolve that to the same file
# only when they run from the same place, so `mm-api` starts with the Go server's `$RUN` as its
# working directory — the layout `go-server.sh` builds, whose `logs/mattermost.log` is the file
# Go writes. Nothing else this process reads is relative to the working directory: the file
# directory is absolute above, and the config document's `./plugins`-style defaults now resolve
# to the same directories Go resolves them to.
#
# The three local-mode variables opened the unix-socket admin API, 2026-09-11. They are the same
# names Go reads, and the socket paths are the per-stack ones from `stack-env.sh` —
# `MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION` names the **Go** server's socket, which is mm-api's
# forward target, and `MM_API_LOCAL_SOCKET` is mm-api's own. Pointing both at one path is refused
# at startup. `scripts/go-server.sh` sets the matching pair on the other side.
#
# `MM_GO_PLUGIN_DIRECTORY` (2026-09-15) is, like `MM_GO_UPSTREAM`, a fact about the **Go peer**
# rather than a Mattermost setting: the directory the stack Go server scans for plugin bundles
# (its `PluginSettings.Directory`, `./plugins`, resolved against its own run directory). The
# slash-command routes answer only while it holds no plugin, since a plugin may register
# commands this server cannot see. It is not `MM_PLUGINSETTINGS_DIRECTORY`, which `GET /config`
# would report. Unset, those routes forward.
mmrs_launch_mm_api() {
  local root="${MMRS_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)}"
  source "$root/scripts/stack-env.sh"
  local log="${1:-/tmp/mmrs-mm-api$MMRS_STACK_SUFFIX.log}"
  (
    cd "$root/reference/.build/mmroot$MMRS_RUN_SUFFIX" || exit 1
    DATABASE_URL="$DATABASE_URL" \
    MM_API_LISTEN="127.0.0.1:$MMRS_API_PORT" \
    MM_GO_UPSTREAM="$MMRS_GO_BASE" \
    MM_FILESETTINGS_DIRECTORY="$root/reference/.build/mmroot$MMRS_RUN_SUFFIX/data/" \
    MM_TEAMSETTINGS_ENABLEOPENSERVER=true \
    MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true \
    MM_SERVICESETTINGS_SITEURL="$MMRS_GO_BASE" \
    MM_SERVICESETTINGS_LISTENADDRESS=":$MMRS_GO_PORT" \
    MM_SQLSETTINGS_DRIVERNAME=postgres \
    MM_SQLSETTINGS_DATASOURCE="$DATABASE_URL?sslmode=disable&connect_timeout=10" \
    MM_SERVICESETTINGS_ENABLELOCALMODE=true \
    MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION="$MMRS_GO_LOCAL_SOCKET" \
    MM_API_LOCAL_SOCKET="$MMRS_LOCAL_SOCKET" \
    MM_GO_PLUGIN_DIRECTORY="$root/reference/.build/mmroot$MMRS_RUN_SUFFIX/plugins" \
    nohup "$root/target/debug/mm-api" > "$log" 2>&1 &
  )
}
