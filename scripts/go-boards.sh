#!/bin/zsh
# A **second** pinned Go server, on a spare port, with `MM_FEATUREFLAGS_INTEGRATEDBOARDS=true`.
#
#   scripts/go-boards.sh start     start it, waiting until it answers /system/ping
#   scripts/go-boards.sh stop      stop it
#   scripts/go-boards.sh port      print the port it uses on this stack
#
# # Why a second server rather than a flag on the first
#
# `api4/view.go` registers its seven routes only when `FeatureFlags.IntegratedBoards` is on
# (view.go:15), and the flag is **false** at the pinned SHA (feature_flags.go:194). With it off the
# gorilla mux has never heard of `/channels/{id}/views` and answers `api.context.404.app_error` —
# measured, not assumed. So there is no Go oracle for the served shape of these routes at all.
#
# Turning the flag on in `scripts/go-server.sh` is not the fix, and its own comment says why: the
# same flag changes `api4/post.go` at four sites (:719, :784, :1149, :1268) and registers
# `api4/properties.go`, both of which are routes this port already serves and the parity suite
# already asserts. Flipping it globally would move the answers under tests that have nothing to do
# with views.
#
# A second process, same database, same configuration document, flag on, on its own port, changes
# nothing for anybody else and gives `parity_views.rs` something to compare against.
#
# # What it shares and what it does not
#
# Same Postgres and the same `MM_CONFIG` DSN, so the same configuration document and the same
# `Sessions` table — a token minted against the main server authenticates here unchanged, which is
# what makes a side-by-side comparison possible at all. Its own run directory, so the two do not
# fight over `logs/` or `plugins/`. Both will run the jobs scheduler against one database; that is
# tolerable for an oracle that is up for the length of a test run and is the reason this is not
# started by `stack.sh`.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
source "$ROOT/scripts/stack-env.sh"
# +30 sits above the `SecondServer` block (807x + offset) and below the next stack's Postgres.
PORT=$((MMRS_GO_PORT + 30))
RUN="$BUILD/mmboards$MMRS_RUN_SUFFIX"
LOG="$BUILD/boards$MMRS_STACK_SUFFIX.log"
DSN="postgres://mmuser:mmuser_password@localhost:$MMRS_PG_PORT/mattermost?sslmode=disable&connect_timeout=10"

case "${1:-start}" in
  port) echo "$PORT" ;;
  stop)
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    echo "stopped"
    ;;
  start)
    [ -x "$BUILD/mattermost" ] || { echo "no binary at $BUILD/mattermost — run scripts/go-server.sh first"; exit 2; }
    mkdir -p "$RUN/bin" "$RUN/data" "$RUN/plugins" "$RUN/client/plugins" "$RUN/config" "$RUN/logs"
    cp -f "$BUILD/mattermost" "$RUN/bin/mattermost"
    for dir in i18n templates fonts; do
      [ -e "$RUN/$dir" ] || ln -s "$SRC/$dir" "$RUN/$dir"
    done
    export MM_CONFIG="$DSN"
    export MM_SQLSETTINGS_DRIVERNAME=postgres
    export MM_SQLSETTINGS_DATASOURCE="$DSN"
    export MM_SERVICESETTINGS_SITEURL="http://localhost:$PORT"
    export MM_SERVICESETTINGS_LISTENADDRESS=":$PORT"
    export MM_TEAMSETTINGS_ENABLEOPENSERVER=true
    export MM_SERVICESETTINGS_ENABLELOCALMODE=false
    # The main server's data directory, deliberately: a file id created there must resolve here.
    export MM_FILESETTINGS_DIRECTORY="$BUILD/mmroot$MMRS_RUN_SUFFIX/data/"
    export MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true
    # The one difference from `go-server.sh`, and the entire point of this script.
    export MM_FEATUREFLAGS_INTEGRATEDBOARDS=true
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the boards oracle never came up — see $LOG"; exit 1; }
    echo "the boards-on Go oracle is listening on :$PORT (log: $LOG)"
    ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
