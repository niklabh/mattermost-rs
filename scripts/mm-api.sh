#!/bin/zsh
# Build and run `mm-api` for the current stack, the way the parity harness runs it.
#
#   scripts/mm-api.sh start     build, then run in the background, waiting until it answers /system/ping
#   scripts/mm-api.sh stop      stop it
#
#   MMRS_API_HOST=0.0.0.0 scripts/mm-api.sh start    reachable from other machines, not just localhost
#
# `cargo run -p mm-api` with only `DATABASE_URL` set starts a server that disagrees with the Go
# server beside it about half a dozen settings, because Go's are environment overrides that never
# reach the shared configuration document. `mm-api-env.sh` is the one list of them; this is the
# launch `parity.sh` does, without the tests. `MMRS_STACK` picks the stack, as everywhere else.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
MMRS_ROOT="$ROOT"
source "$ROOT/scripts/stack-env.sh"
source "$ROOT/scripts/mm-api-env.sh"
LOG="/tmp/mmrs-mm-api$MMRS_STACK_SUFFIX.log"

case "${1:-}" in
  stop)
    mmrs_free_port "$MMRS_API_PORT"
    echo "stopped"
    ;;
  start)
    cargo build -p mm-api
    # By port, not by command line: see the long note in `parity.sh`.
    mmrs_free_port "$MMRS_API_PORT"
    sleep 1
    mmrs_launch_mm_api "$LOG"
    for _ in $(seq 30); do
      curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping" && break
      sleep 0.5
    done
    curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping" \
      || { echo "mm-api never came up — see $LOG"; exit 1; }
    echo "mm-api is listening on ${MMRS_API_HOST:-127.0.0.1}:$MMRS_API_PORT, forwarding to $MMRS_GO_BASE (log: $LOG)"
    ;;
  *)
    sed -n '2,5p' "$0"; exit 2
    ;;
esac
