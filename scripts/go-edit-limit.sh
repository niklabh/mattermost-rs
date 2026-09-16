#!/bin/zsh
# Another pinned Go server, on a spare port, with `MM_SERVICESETTINGS_POSTEDITTIMELIMIT=0`.
#
#   scripts/go-edit-limit.sh start   start it, waiting until it answers /system/ping
#   scripts/go-edit-limit.sh stop    stop it
#   scripts/go-edit-limit.sh port    print the port it uses on this stack
#
# # Why a separate server
#
# `postEditTimeLimitExpired` (api4/post.go:1052) returns false on the stock `-1` before it looks
# at anything else, so the 400 `api.post.update_post.permissions_time_limit.app_error` that
# `updatePost`, `patchPost` and the pin routes raise has no Go oracle on the stack server [D-222].
# `0` is the value that reaches it for every post: "expired" is `now > CreateAt + limit * 1000`.
#
# It cannot be set on the stack server, even through `PUT /config/patch` for the length of one
# test: two dozen suites edit or pin posts, and each would 400 while the setting was on. An
# environment override on a separate process moves only the answers of the one suite that talks to
# it — `parity::post_edit_time_limit` — whose mm-api `SecondServer` carries the same variable.
#
# # What it shares and what it does not
#
# Same Postgres and the same `MM_CONFIG` DSN, so the same `Sessions` and `Posts` tables — a post
# created through the main server is edited here. The override is an environment variable, which
# Go does not write back into the shared configuration document. Its own run directory.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
source "$ROOT/scripts/stack-env.sh"
# +35, above the licensed oracles' +32..+34, still below the next stack's block.
PORT=$((MMRS_GO_PORT + 35))
RUN="$BUILD/mmeditlimit$MMRS_RUN_SUFFIX"
LOG="$BUILD/editlimit$MMRS_STACK_SUFFIX.log"
DSN="postgres://mmuser:mmuser_password@localhost:$MMRS_PG_PORT/mattermost?sslmode=disable&connect_timeout=10"

case "${1:-start}" in
  port) echo "$PORT" ;;
  stop)
    mmrs_free_port "$PORT"
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
    export MM_SERVICESETTINGS_POSTEDITTIMELIMIT=0
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the edit-limit oracle never came up — see $LOG"; exit 1; }
    echo "the PostEditTimeLimit=0 Go oracle is listening on :$PORT (log: $LOG)"
    ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
