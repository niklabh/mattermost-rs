#!/bin/zsh
# A pinned Go server that **accepts plugin uploads**, on a spare port, with its own file store.
#
#   scripts/go-plugins.sh start     start it, waiting until it answers /system/ping
#   scripts/go-plugins.sh stop      stop it
#   scripts/go-plugins.sh port      print the port it uses on this stack
#
# # Why a second server
#
# `uploadPlugin` refuses with a 501 unless `PluginSettings.EnableUploads` is on (api4/plugin.go:47),
# and `patchConfig` refuses to change that setting over the API (api4/config.go:306), so the main
# server — which runs on the stock `false` — can never answer an upload. This one sets it from the
# environment. It is the oracle `parity::plugin_upload` compares the Rust plugin host against.
#
# # What it shares and what it does not
#
# The database, the configuration document and the `Sessions` table, like `go-boards.sh`: a token
# minted on the main server works here. **Its own run directory and its own file store**, which is
# the point: an upload writes `plugins/<id>.tar.gz` to the file store and unpacks into `./plugins`,
# and neither may reach the main server, whose start-up sync would install whatever it found.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
source "$ROOT/scripts/stack-env.sh"
# +36: above the boards (+30), discoverable (+31), licensed (+32..+34) and edit-limit (+35) oracles.
PORT=$((MMRS_GO_PORT + 36))
RUN="$BUILD/mmplugins$MMRS_RUN_SUFFIX"
LOG="$BUILD/plugins$MMRS_STACK_SUFFIX.log"
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
    # Its own file store, deliberately: see the header.
    export MM_FILESETTINGS_DIRECTORY="$RUN/data/"
    export MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true
    # The one difference that matters.
    export MM_PLUGINSETTINGS_ENABLEUPLOADS=true
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the plugins oracle never came up — see $LOG"; exit 1; }
    echo "the uploads-on Go oracle is listening on :$PORT (log: $LOG)"
    ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
