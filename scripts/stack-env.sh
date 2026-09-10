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

# `docker compose` for this stack, project flag included when there is one.
mmrs_compose() {
  if [ -n "$MMRS_COMPOSE_PROJECT" ]; then
    docker compose -p "$MMRS_COMPOSE_PROJECT" "$@"
  else
    docker compose "$@"
  fi
}
