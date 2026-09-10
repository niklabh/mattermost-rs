#!/bin/zsh
# Build and run the Go server **from the pinned reference SHA**, in place of the published image.
#
#   scripts/go-server.sh            build if needed, then run in the foreground
#   scripts/go-server.sh start      … in the background, waiting until it answers /system/ping
#   scripts/go-server.sh stop       stop it
#   scripts/go-server.sh rebuild    force a rebuild, then start
#
# # Why this exists
#
# `docker-compose.yml` used to run `mattermost/mattermost-team-edition:11.11.0-rc1` as the forward
# target. That image and `reference/mattermost/` are **both** 11.11.0 and they are not the same
# code: rc1 is an earlier cut, so `GET /api/v4/bots` carries a `system_owned` field the pinned
# source has never heard of, and `api4/properties.go`'s routes answer a mux 404 there while being
# registered here. A route whose live shape disagrees with the reference cannot be ported
# honestly — matching the source produces a body the proxy's own target does not serve, and
# matching the server means reverse-engineering a field with no source to read. That was [D-167],
# and building the reference is what closes it.
#
# It is also **native**. The published image has no arm64 manifest, so on this host it ran under
# qemu at roughly a third of native speed; this binary is built for the host.
#
# # What it is not
#
# Not the webapp. `client/` is empty, so the browser UI 404s — the API, the websocket and every
# parity suite work, and nothing in this repository needs the bundle. The two `error` lines about
# `root.html` and the SMTP server on boot are expected and harmless.
#
# # The migrations are one-way
#
# The pinned SHA is *later* than rc1, so its first boot migrates the shared database forward and
# the rc1 image can no longer read it. Going back means recreating the volume; see [D-130].
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
# Everything below is per-stack, so several Go servers can run at once — one per worktree. Stack
# 0 is the historical layout (:8065, `mmroot`, `server.log`) exactly as before; see stack-env.sh.
source "$ROOT/scripts/stack-env.sh"
RUN="$BUILD/mmroot$MMRS_RUN_SUFFIX"
LOG="$BUILD/server$MMRS_STACK_SUFFIX.log"
PORT=$MMRS_GO_PORT
DSN="postgres://mmuser:mmuser_password@localhost:$MMRS_PG_PORT/mattermost?sslmode=disable&connect_timeout=10"

[ -d "$SRC" ] || { echo "reference/mattermost is not cloned — see MIGRATION.md for the pinned SHA"; exit 2; }

build() {
  mkdir -p "$BUILD"
  # `server/go.mod` requires the *published* `server/public v0.4.0`, not the copy sitting next to
  # it, so a plain `go build` fails with a screen of undefined `model.` symbols that look like a
  # broken checkout and are not. Upstream's `make setup-go-work` writes a `go.work` into the
  # source tree; this writes an equivalent one **outside** it, with absolute paths, so the
  # reference stays untouched.
  cat > "$BUILD/go.work" <<EOF
go 1.26.4

use $SRC
use $SRC/public
EOF
  echo "building the pinned Go server (a few minutes on a cold cache)…"
  (cd "$SRC" && GOWORK="$BUILD/go.work" GOFLAGS=-buildvcs=false go build -o "$BUILD/mattermost" ./cmd/mattermost)
}

layout() {
  mkdir -p "$RUN/bin" "$RUN/data" "$RUN/plugins" "$RUN/client/plugins" "$RUN/config" "$RUN/logs"
  cp -f "$BUILD/mattermost" "$RUN/bin/mattermost"
  # `i18n`, `templates` and `fonts` are read from the working directory at runtime and are
  # unmodified reference files — symlinked rather than copied so they cannot drift from the SHA.
  for dir in i18n templates fonts; do
    [ -e "$RUN/$dir" ] || ln -s "$SRC/$dir" "$RUN/$dir"
  done
}

env_for_server() {
  # The same environment `docker-compose.yml` gave the container, with `postgres` swapped for
  # `localhost` — the database is still the one in Docker.
  #
  # **`MM_CONFIG` is a DSN on purpose.** It selects `config.DatabaseStore` over the file store, so
  # the configuration lives in the shared database and the Rust server reads the very document
  # this server runs on. Do not "simplify" it to a path. See [D-156].
  export MM_CONFIG="$DSN"
  export MM_SQLSETTINGS_DRIVERNAME=postgres
  export MM_SQLSETTINGS_DATASOURCE="$DSN"
  export MM_SERVICESETTINGS_SITEURL="http://localhost:$PORT"
  # **Not optional once there is more than one stack.** The listen address lives in the
  # configuration document, which `SetDefaults` fills with `:8065` on a fresh database — so
  # without this every stack's server binds 8065 and all but the first die with
  # "address already in use", having already migrated their own database.
  export MM_SERVICESETTINGS_LISTENADDRESS=":$PORT"
  export MM_TEAMSETTINGS_ENABLEOPENSERVER=true
  export MM_SERVICESETTINGS_ENABLELOCALMODE=false
  export MM_FILESETTINGS_DIRECTORY="$RUN/data/"
  # Feature flags stay at their compiled defaults **unless a session turns one on deliberately**.
  # `IntegratedBoards` and `DiscoverableChannels` gate whole route families (`api4/view.go`,
  # `api4/channel_join_request.go`) and also change behaviour on routes already ported —
  # `getChannel`'s discoverable-non-member branch, [D-153]. Those two are still off.
  #
  # `EnableShiftEscapeToMarkAllRead` is on, turned on 2026-09-10 with the parity run its own
  # comment asked for. It is read in exactly two places (api4/channel.go:702, :2100) and gates
  # nothing else, so unlike the other two it cannot change an answer on a route already served.
  # Off, both of its routes are a 501 and neither has a comparable 200.
  #
  # **Whatever is set here must also be set in `scripts/mm-api-env.sh`**: an environment override
  # never reaches the configuration document, which is what `mm-api` reads.
  export MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true
}

case "${1:-run}" in
  stop)
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    echo "stopped"
    ;;
  rebuild)
    build; exec "$0" start
    ;;
  start|run)
    [ -x "$BUILD/mattermost" ] || build
    layout
    env_for_server
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    if [ "${1:-run}" = "run" ]; then
      cd "$RUN" && exec "$RUN/bin/mattermost" server
    fi
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 60); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the Go server never came up — see $LOG"; exit 1; }
    echo "the pinned Go server is listening on :$PORT (log: $LOG)"
    ;;
  *)
    sed -n '2,8p' "$0"; exit 2
    ;;
esac
