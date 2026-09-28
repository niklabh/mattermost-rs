#!/bin/zsh
# A pinned Go server that **may fetch link previews from this machine**, on a spare port.
#
#   scripts/go-links.sh start     start it, waiting until it answers /system/ping
#   scripts/go-links.sh stop      stop it
#   scripts/go-links.sh port      print the port it uses on this stack
#
# # Why a second server
#
# `POST /api/v4/posts` fetches the first link of a message through `MakeClient(false)`, whose
# dialer refuses every reserved address — loopback included — unless
# `ServiceSettings.AllowedUntrustedInternalConnections` names it. `parity::post_create_links`
# serves its pages and images from a mock on 127.0.0.1, so the main server, whose allow-list is
# empty and must stay empty for `parity::redirect_location`'s refusals, would only ever show the
# refusal. This one allows `127.0.0.1` from the environment, which is never persisted, so the
# shared configuration document is untouched.
#
# `SiteURL` is this server's own address, and the Rust server the suite compares it with is given
# the same string: a permalink is recognised by its site-URL prefix, so the two must agree on it.
#
# # What it shares and what it does not
#
# The database, the configuration document and the `Sessions` table, like `go-plugins.sh`: a token
# minted on the main server works here, and the `LinkMetadata` rows both servers write are one
# table. Its own run directory, so nothing it does on disk reaches the main server. Configuration
# **writes** must not go through it: it would save its start-time copy of the document over
# everyone else's changes (see `oracle-config-saves-are-stale`).
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
source "$ROOT/scripts/stack-env.sh"
# +50: above the oracles at +30..+38, the mock Marketplace at +39, and the stack-0 second servers
# at 8105..8108 (= +40..+43) that `second_server_ports` would otherwise let collide with this.
PORT=$((MMRS_GO_PORT + 50))
RUN="$BUILD/mmlinks$MMRS_RUN_SUFFIX"
LOG="$BUILD/links$MMRS_STACK_SUFFIX.log"
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
    # Only the stack's main Go server runs jobs. An oracle on the same database claims pending
    # jobs too, and a job it runs publishes to its own hub and pushes with its own settings, so
    # a job parity test could not tell what Go did (parity::persistent_notifications). Its
    # schedulers are off too: a licensed oracle would queue licensed-only job types itself.
    export MM_JOBSETTINGS_RUNJOBS=false
    export MM_JOBSETTINGS_RUNSCHEDULER=false
    export MM_SERVICESETTINGS_SITEURL="http://localhost:$PORT"
    export MM_SERVICESETTINGS_LISTENADDRESS=":$PORT"
    export MM_TEAMSETTINGS_ENABLEOPENSERVER=true
    export MM_SERVICESETTINGS_ENABLELOCALMODE=false
    export MM_FILESETTINGS_DIRECTORY="$RUN/data/"
    export MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true
    # The one difference that matters: the mock the suite serves is on 127.0.0.1.
    export MM_SERVICESETTINGS_ALLOWEDUNTRUSTEDINTERNALCONNECTIONS=127.0.0.1
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the links oracle never came up — see $LOG"; exit 1; }
    echo "the link-preview Go oracle is listening on :$PORT (log: $LOG)"
    ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
