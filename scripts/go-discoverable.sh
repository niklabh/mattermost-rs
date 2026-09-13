#!/bin/zsh
# A **third** pinned Go server, on a spare port, with `MM_FEATUREFLAGS_DISCOVERABLECHANNELS=true`.
#
#   scripts/go-discoverable.sh start   start it, waiting until it answers /system/ping
#   scripts/go-discoverable.sh stop    stop it
#   scripts/go-discoverable.sh port    print the port it uses on this stack
#
# # Why a third server rather than a flag on the first
#
# `initChannelJoinRequestRoutes` (api4/channel_join_request.go:18) is one `if`: with
# `FeatureFlags.DiscoverableChannels` off it registers **none** of its seven routes, and the flag is
# `false` at the pinned SHA (feature_flags.go:208). So gorilla/mux has never heard of
# `/channels/{id}/join_request` and answers `api.context.404.app_error` — which means there is no
# Go oracle for the served shape of this family at all.
#
# Turning the flag on in `scripts/go-server.sh` would be wrong for the same reason `go-boards.sh`
# exists: the same flag changes `getChannel` (api4/channel.go:886, `serveDiscoverableNonMember`),
# `createChannel` and `patchChannel`, all of which this port already serves and the parity suite
# already asserts against. [D-153] is the ledger entry for that pin. Flipping it globally would
# move answers under tests that have nothing to do with join requests.
#
# It is a separate process from `go-boards.sh` rather than a second flag on it for the same
# reason: `parity_views` compares against a server whose only difference from the stack's is
# `IntegratedBoards`, and adding a second flag there would move `getChannel` under that suite too.
#
# # What it shares and what it does not
#
# Same Postgres and the same `MM_CONFIG` DSN, so the same configuration document and the same
# `Sessions` table — a token minted against the main server authenticates here unchanged, which is
# what makes a side-by-side comparison possible at all. Its own run directory, so the three do not
# fight over `logs/` or `plugins/`.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
source "$ROOT/scripts/stack-env.sh"
# +31, one above the boards oracle's +30, still below the next stack's block.
PORT=$((MMRS_GO_PORT + 31))
RUN="$BUILD/mmdisc$MMRS_RUN_SUFFIX"
LOG="$BUILD/discoverable$MMRS_STACK_SUFFIX.log"
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
    export MM_FEATUREFLAGS_DISCOVERABLECHANNELS=true
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the discoverable oracle never came up — see $LOG"; exit 1; }
    echo "the discoverable-on Go oracle is listening on :$PORT (log: $LOG)"
    ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
