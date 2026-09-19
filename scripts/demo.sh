#!/bin/zsh
# Bring the whole stack up for a demo from another machine on the network, idempotently.
#
#   scripts/demo.sh up        check prerequisites, build what is stale, start what is down, seed
#   scripts/demo.sh down      stop mm-api, the Go server and this stack's Postgres (data is kept)
#   scripts/demo.sh status    what is running, and the URL to open
#
# Uses the current stack (`MMRS_STACK`, `.mmrs-stack`, else 0), like every other script here.
# `up` can be run any number of times: each step says what it found and does only what is missing.
#
# # What it changes compared with a development stack
#
# Two things, both only for servers this script starts:
#
# - mm-api binds `0.0.0.0` (`MMRS_API_HOST`), so the browser on another machine can reach it.
# - `SiteURL` is mm-api's **LAN** URL (`MMRS_SITE_URL`), for both servers — Go's own default is
#   `http://localhost:8065`, which makes permalinks and e-mail links point at a port the other
#   machine cannot reach. `go-server.sh` and `mm-api-env.sh` read the same variable, so the two
#   servers still agree; the parity harness never sets it.
#
# mm-api also logs a traffic line per request (`MM_API_TRAFFIC_LOG=1`), so
# `scripts/demo-traffic.sh report` can say afterwards what share of the demo Rust answered.
#
# The parity oracles (`go-boards.sh` and the rest) are not started: the demo does not need them.
# `MMRS_DEMO_HOST` overrides the detected LAN address.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
MMRS_ROOT="$ROOT"
source "$ROOT/scripts/stack-env.sh"
PINNED_SHA=9dfbaeca99f4096388fd1c048a9e6d1d0a86743e
DIST="$ROOT/webapp/channels/dist"

say() { print -r -- "==> $*"; }
fail() { print -r -- "!!  $*" >&2; exit 1; }

lan_host() {
  if [ -n "${MMRS_DEMO_HOST:-}" ]; then print -r -- "$MMRS_DEMO_HOST"; return; fi
  local ip
  ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}' | head -1)
  [ -n "$ip" ] || ip=$(hostname -I 2>/dev/null | awk '{print $1}')
  # macOS has neither: the address of the interface the default route leaves by.
  [ -n "$ip" ] || ip=$(ipconfig getifaddr \
    "$(route -n get default 2>/dev/null | awk '/interface:/ {print $2}')" 2>/dev/null)
  print -r -- "${ip:-127.0.0.1}"
}

SITE_URL="http://$(lan_host):$MMRS_API_PORT"

listener_env() { mmrs_listener_env "$@"; }

listener_addr() { mmrs_listener_addr "$@"; }

check_prerequisites() {
  say "checking prerequisites"
  local missing=0 sockets=ss
  # What `stack-env.sh` asks who holds a port: `ss` on Linux, `lsof` on macOS.
  [ "$(uname -s)" = Linux ] || sockets=lsof
  for tool in cargo go docker curl python3 $sockets; do
    if command -v "$tool" >/dev/null; then
      print "    $tool: $(command -v "$tool")"
    else
      print "    $tool: MISSING"; missing=1
    fi
  done
  docker info >/dev/null 2>&1 || { print "    docker: the daemon is not reachable"; missing=1; }
  docker compose version >/dev/null 2>&1 || { print "    docker compose: MISSING"; missing=1; }
  [ "$missing" = 0 ] || fail "install the missing tools above, then run this again"

  local head
  head=$(git -C "$ROOT/reference/mattermost" rev-parse HEAD 2>/dev/null || true)
  if [ "$head" != "$PINNED_SHA" ]; then
    print "    reference/mattermost is ${head:+at $head, not }missing${head:+ the pinned SHA}. To fetch it:"
    print "      git init reference/mattermost"
    print "      git -C reference/mattermost remote add origin https://github.com/mattermost/mattermost.git"
    print "      git -C reference/mattermost fetch --depth 1 origin $PINNED_SHA"
    print "      git -C reference/mattermost checkout FETCH_HEAD"
    fail "the Go server is built from the reference clone at the pinned SHA"
  fi
  print "    reference/mattermost: at the pinned SHA ${PINNED_SHA:0:8}"
}

# Node and npm are needed only to build the webapp, so their versions are checked only then.
check_node() {
  local node npm
  node=$(node --version 2>/dev/null | sed 's/^v//')
  npm=$(npm --version 2>/dev/null)
  [ "${node%%.*}" = 24 ] || fail "building the webapp needs Node 24 (found: ${node:-none})"
  [ "${npm%%.*}" = 11 ] || fail "building the webapp needs npm 11 (found: ${npm:-none})"
  print "    node $node, npm $npm"
}

build_webapp() {
  say "webapp bundle"
  local stale=""
  if [ ! -f "$DIST/root.html" ]; then
    stale="no build in webapp/channels/dist"
  else
    local newer
    newer=$(find "$ROOT/webapp" \( -path "$DIST" -o -name node_modules -o -name .git \) -prune \
      -o -type f -newer "$DIST/root.html" -print -quit)
    [ -z "$newer" ] || stale="${newer#$ROOT/} is newer than the build"
  fi
  if [ -z "$stale" ]; then
    print "    up to date"
    return
  fi
  print "    building: $stale (a few minutes cold)"
  check_node
  (cd "$ROOT/webapp" && npm ci && npm run build)
  # Bundle names are content-hashed and go-server.sh links them in at start, so a Go server
  # already running would serve the previous build's links.
  WEBAPP_REBUILT=1
}

start_postgres() {
  say "postgres on :$MMRS_PG_PORT"
  if docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1; then
    print "    already running"
    return
  fi
  mmrs_compose up -d postgres
  for _ in $(seq 60); do
    docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1 \
    || fail "postgres never became ready"
  print "    started"
}

start_go() {
  say "Go server on :$MMRS_GO_PORT, SiteURL $SITE_URL"
  local running
  running=$(listener_env "$MMRS_GO_PORT" MM_SERVICESETTINGS_SITEURL)
  if [ "$running" = "$SITE_URL" ] && [ -z "${WEBAPP_REBUILT:-}" ] \
    && curl -sf -o /dev/null "$MMRS_GO_BASE/api/v4/system/ping"; then
    print "    already running with this SiteURL"
    return
  fi
  if [ -n "$running" ]; then
    print "    restarting (it runs with SiteURL ${running}${WEBAPP_REBUILT:+, and the webapp was rebuilt})"
  else
    print "    starting (the first run builds the Go server and migrates the database)"
  fi
  MMRS_SITE_URL="$SITE_URL" zsh "$ROOT/scripts/go-server.sh" start | sed 's/^/    /'
}

start_mm_api() {
  say "mm-api on 0.0.0.0:$MMRS_API_PORT"
  local site addr traffic
  site=$(listener_env "$MMRS_API_PORT" MM_SERVICESETTINGS_SITEURL)
  traffic=$(listener_env "$MMRS_API_PORT" MM_API_TRAFFIC_LOG)
  addr=$(listener_addr "$MMRS_API_PORT")
  if [ "$site" = "$SITE_URL" ] && [ "$traffic" = 1 ] && [ "${addr%:*}" = "0.0.0.0" ] \
    && curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping"; then
    print "    already running on the LAN with this SiteURL"
    return
  fi
  print "    building and starting it"
  MMRS_API_HOST=0.0.0.0 MMRS_SITE_URL="$SITE_URL" MM_API_TRAFFIC_LOG=1 \
    zsh "$ROOT/scripts/mm-api.sh" start 2>&1 | grep -v '^ *\(Compiling\|Checking\)' | sed 's/^/    /'
}

seed() {
  say "demo accounts and teams"
  python3 "$ROOT/scripts/demo-seed.py" "$MMRS_RUST_BASE"
}

show() {
  print
  print "  Open  $SITE_URL   (from any machine on this network)"
  print
  print "  sliceuser  Slice-Test-1234     system admin"
  print "  tester     Tester-Pass-2026"
  print "  tester2    Tester2-Pass-2026"
  print
  print "  Teams: playground, lounge. The first page offers \"View in Browser\"."
  print "  Afterwards, scripts/demo-traffic.sh report  says what share of it Rust answered."
}

status() {
  say "stack $MMRS_STACK"
  if docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1; then
    print "    postgres :$MMRS_PG_PORT   up"
  else
    print "    postgres :$MMRS_PG_PORT   down"
  fi
  if curl -sf -o /dev/null "$MMRS_GO_BASE/api/v4/system/ping"; then
    print "    go       :$MMRS_GO_PORT   up, SiteURL $(listener_env "$MMRS_GO_PORT" MM_SERVICESETTINGS_SITEURL)"
  else
    print "    go       :$MMRS_GO_PORT   down"
  fi
  if curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping"; then
    print "    mm-api   :$MMRS_API_PORT   up on $(listener_addr "$MMRS_API_PORT"), SiteURL $(listener_env "$MMRS_API_PORT" MM_SERVICESETTINGS_SITEURL)"
  else
    print "    mm-api   :$MMRS_API_PORT   down"
  fi
  [ -f "$DIST/root.html" ] && print "    webapp   built" || print "    webapp   not built"
  print "    demo URL $SITE_URL"
}

case "${1:-}" in
  up)
    check_prerequisites
    build_webapp
    start_postgres
    start_go
    start_mm_api
    seed
    show
    ;;
  down)
    say "stopping mm-api";   zsh "$ROOT/scripts/mm-api.sh" stop | sed 's/^/    /'
    say "stopping the Go server"; zsh "$ROOT/scripts/go-server.sh" stop | sed 's/^/    /'
    say "stopping postgres (the volume, and the data in it, stays)"
    mmrs_compose stop postgres 2>&1 | sed 's/^/    /'
    ;;
  status)
    status
    ;;
  *)
    sed -n '2,6p' "$0"; exit 2
    ;;
esac
