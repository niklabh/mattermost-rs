# The port and path layout of ONE numbered stack, sourced by everything that touches it.
#
#   MMRS_STACK=2 source scripts/stack-env.sh
#
# # Why numbered stacks exist
#
# Every stack-backed suite talked to one Postgres, one Go server on :8065 and one mm-api on
# :8066, so `stack-lock.sh` had to serialise every checkout on the machine. That lock was the
# throughput ceiling: a twenty-minute mutation batch blocked every other worktree, and the
# parallel-worktree pattern could not actually run in parallel. A stack is now a *numbered*
# triple, and the lock is per-stack — two worktrees on two stacks never wait for each other.
#
# # The layout
#
#   stack k   postgres  5432 + k        go  8065 + 100k        mm-api  8066 + 100k
#
# Stack **0 is the historical layout, byte for byte**: 5432/8065/8066, container `mmrs-postgres`,
# the default compose project, `reference/.build/mmroot`. An existing checkout keeps working with
# no flags and no migration, which is the only reason the offsets are what they are.
#
# The 100-wide gap per stack leaves room for the `SecondServer` ports the parity suites start on
# 807x; see `MMRS_PORT_OFFSET` below.
# A worktree pins itself by writing its number into `.mmrs-stack` (see `scripts/worktree.sh`), so
# an agent working in one never has to remember to set the variable. An explicit `MMRS_STACK` in
# the environment still wins.
if [ -z "${MMRS_STACK:-}" ]; then
  _mmrs_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
  if [ -f "$_mmrs_root/.mmrs-stack" ]; then
    MMRS_STACK="$(tr -dc '0-9' < "$_mmrs_root/.mmrs-stack")"
  fi
  unset _mmrs_root
fi
: "${MMRS_STACK:=0}"

case "$MMRS_STACK" in
  ''|*[!0-9]*) echo "MMRS_STACK must be a number, got '$MMRS_STACK'" >&2; return 1 2>/dev/null || exit 1 ;;
esac

export MMRS_STACK
export MMRS_PG_PORT=$((5432 + MMRS_STACK))
export MMRS_GO_PORT=$((8065 + 100 * MMRS_STACK))
export MMRS_API_PORT=$((8066 + 100 * MMRS_STACK))
# Added to every hardcoded `SecondServer::start(80xx)` port in the parity suites, at **runtime**,
# so the call sites keep naming one number and two stacks still never collide.
export MMRS_PORT_OFFSET=$((100 * MMRS_STACK))

export MMRS_GO_BASE="http://localhost:$MMRS_GO_PORT"
export MMRS_RUST_BASE="http://127.0.0.1:$MMRS_API_PORT"
export DATABASE_URL="postgres://mmuser:mmuser_password@localhost:$MMRS_PG_PORT/mattermost"

if [ "$MMRS_STACK" = "0" ]; then
  # Stack 0 keeps the original names so the existing container and volume are reused rather than
  # orphaned: the default compose project name is the directory name, and passing `-p` would make
  # docker create a second, empty volume beside the one with the database in it.
  export MMRS_STACK_SUFFIX=""
  export MMRS_COMPOSE_PROJECT=""
  export MMRS_RUN_SUFFIX=""
else
  export MMRS_STACK_SUFFIX="-$MMRS_STACK"
  export MMRS_COMPOSE_PROJECT="mmrs-stack-$MMRS_STACK"
  export MMRS_RUN_SUFFIX="-$MMRS_STACK"
fi

export MMRS_LOCK="/tmp/mmrs-stack$MMRS_STACK_SUFFIX.lock"

# Who listens on a TCP port of this machine: pid, address and launch environment. `ss` and
# `/proc` on Linux; `lsof` and `ps -E` on macOS, which has neither. Both sets ship with their OS,
# so neither is a prerequisite to install. Each prints nothing when nobody listens, and always
# succeeds, so a caller under `set -e -o pipefail` can take an empty answer as "not running".
mmrs_listener_pids() {
  if [ "$(uname -s)" = Linux ]; then
    { ss -ltnpH "sport = :$1" 2>/dev/null | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u; } || true
  else
    { lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | sort -u; } || true
  fi
}

# `ADDRESS:PORT` as `ss` prints it. lsof writes a wildcard bind as `*:PORT`, which `ss` writes as
# `0.0.0.0:PORT` — `demo.sh` compares against the latter.
mmrs_listener_addr() {
  if [ "$(uname -s)" = Linux ]; then
    { ss -ltnH "sport = :$1" 2>/dev/null | awk '{print $4}' | head -1; } || true
  else
    { lsof -nP -iTCP:"$1" -sTCP:LISTEN -Fn 2>/dev/null | sed -n 's/^n//p' | head -1 \
      | sed 's/^\*:/0.0.0.0:/'; } || true
  fi
}

# The value of one environment variable of the process listening on a port, or nothing.
# macOS's `ps -E` appends the environment the process was *launched* with to its command line,
# space-separated, so a value containing a space would be cut short. The values read here (a URL,
# a `1`) never have one.
mmrs_listener_env() {
  local port="$1" name="$2" pid
  pid=$(mmrs_listener_pids "$port" | head -1)
  [ -n "$pid" ] || return 0
  if [ -r "/proc/$pid/environ" ]; then
    { tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | sed -n "s/^$name=//p" | head -1; } || true
  else
    { ps -E -ww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n' | sed -n "s/^$name=//p" | head -1; } || true
  fi
}

# `docker compose` for this stack, project flag included when there is one.
mmrs_compose() {
  if [ -n "$MMRS_COMPOSE_PROJECT" ]; then
    docker compose -p "$MMRS_COMPOSE_PROJECT" "$@"
  else
    docker compose "$@"
  fi
}

# Free a TCP port by killing whoever listens on it, and say who that was.
#
# **The port is the owner key, not the path.** `scripts/parity.sh` learned this for `mm-api` (see
# its comment): a process's command line is fixed at `exec` time, so a path-scoped `pkill` misses
# a server whose path string differs from the one you are about to launch. The Go servers have the
# same hole for a second reason — `reference/.build` is a **symlink** in every worktree, so a
# stack's run directory has as many spellings as there are checkouts. A server started as
# `/home/…/mattermost-rs/reference/.build/mmroot-3/bin/mattermost` is the same physical directory
# as `/home/…/mmrs-worktrees/config/reference/.build/mmroot-3/bin/mattermost`, and `pkill -f` on
# one does not match the other.
#
# The consequence is silent and it is not hypothetical. Measured 2026-09-12 on stacks 2 and 3:
# a Go server left from the main checkout held :8365, the worktree's `go-server.sh start` could
# not bind, and its **readiness probe was answered by the foreign process** — so the script
# reported success and the suite ran against a server with someone else's configuration. Two
# `config_reads` tests failed naming `LocalModeSocketLocation`, a setting the branch never
# touched. A false *pass* is just as available.
#
# Killing by port is safe because `scripts/worktree.sh` enforces one stack per worktree: the port
# belongs to the stack, and the stack belongs to exactly one checkout.
mmrs_free_port() {
  local pid
  for pid in $(mmrs_listener_pids "$1"); do
    echo "  freeing :$1 from pid $pid ($(ps -o command= -p "$pid" 2>/dev/null | cut -c1-70))"
    kill -9 "$pid" 2>/dev/null || true
  done
}

# The two unix domain sockets of the local-mode admin API, per stack.
#
# `MMRS_GO_LOCAL_SOCKET` is the one the Go server binds (`ServiceSettings.LocalModeSocketLocation`,
# app/server.go:1233). `MMRS_LOCAL_SOCKET` is mm-api's own, which serves the local routes that have
# been migrated and forwards the rest to the Go socket beside it — the Strangler Fig proxy, over a
# socket instead of a port.
#
# **Per-stack, and inside the run directory on purpose.** The config default is the *shared*
# `/var/tmp/mattermost_local.socket`; four stacks pointing at one path is one server stealing
# another's socket on restart, since `startLocalModeServer` begins with `os.RemoveAll(socket)`.
# `$RUN` is torn down with the stack, so the sockets go with it.
#
# Short names because `sockaddr_un.sun_path` is 108 bytes including the NUL — a worktree nested a
# few directories deeper than this one is closer to that limit than it looks.
export MMRS_GO_LOCAL_SOCKET="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)/reference/.build/mmroot$MMRS_RUN_SUFFIX/local.socket"
export MMRS_LOCAL_SOCKET="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)/reference/.build/mmroot$MMRS_RUN_SUFFIX/mmrs.socket"
