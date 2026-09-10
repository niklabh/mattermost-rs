#!/bin/zsh
# Run the stack-backed suites against a freshly built mm-api from THIS checkout, under the stack
# lock (see stack-lock.sh).
#
#   scripts/parity.sh                               cargo test --workspace (unit + store + api)
#   scripts/parity.sh -p mm-api --test parity      just the parity suite
#   scripts/parity.sh --test parity users_me       one module's tests
#
# Builds mm-api from this tree, replaces whatever is bound to :8066 with it, runs the tests with
# MM_STORE_DB=1 MM_PARITY_STACK=1, and leaves the server running. Whoever runs next rebuilds and
# replaces it again — the binary on :8066 always belongs to the checkout that last tested.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
# `MMRS_STACK` selects the stack; unset means 0, the historical :5432/:8065/:8066 layout.
source "$ROOT/scripts/stack-env.sh"
export MM_STORE_DB=1 MM_PARITY_STACK=1
# The test binary bakes `common::GO`/`common::RUST` from these at **compile** time (see
# `crates/mm-api/build.rs`), so a worktree pinned to a stack builds once and stays pinned;
# `MMRS_PORT_OFFSET` is read at runtime by `SecondServer::start`.
export MMRS_GO_BASE MMRS_RUST_BASE MMRS_PORT_OFFSET

if [ -z "$MMRS_STACK_LOCKED" ]; then
  export MMRS_STACK_LOCKED=1
  exec "$ROOT/scripts/stack-lock.sh" "$0" "$@"
fi

# The overrides the Go server runs with, which never reach the configuration document mm-api
# reads. One list, sourced by this script and by `mutate.sh`, because the two used to differ.
MMRS_ROOT="$ROOT"
source "$ROOT/scripts/mm-api-env.sh"

cargo build -p mm-api
# Scoped to **this checkout's** binary: two worktrees have different paths, so one stack's
# restart cannot kill another's server.
pkill -f "$ROOT/target/debug/mm-api" 2>/dev/null || true
sleep 1
mmrs_launch_mm_api "/tmp/mmrs-mm-api$MMRS_STACK_SUFFIX.log"
for _ in $(seq 30); do
  curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping" && break
  sleep 0.5
done
curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping" \
  || { echo "mm-api never came up — see /tmp/mmrs-mm-api$MMRS_STACK_SUFFIX.log"; exit 1; }
if [ $# -eq 0 ]; then exec cargo test --workspace; else exec cargo test "$@"; fi
