#!/bin/zsh
# Run the stack-backed suites against a freshly built mm-api from THIS checkout, under the stack
# lock (see stack-lock.sh).
#
#   scripts/parity.sh                                   cargo test --workspace (unit + store + api)
#   scripts/parity.sh -p mm-api --test parity_team_get  any `cargo test` args
#
# Builds mm-api from this tree, replaces whatever is bound to :8066 with it, runs the tests with
# MM_STORE_DB=1 MM_PARITY_STACK=1, and leaves the server running. Whoever runs next rebuilds and
# replaces it again — the binary on :8066 always belongs to the checkout that last tested.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
: ${DATABASE_URL:=postgres://mmuser:mmuser_password@localhost:5432/mattermost}
export DATABASE_URL MM_STORE_DB=1 MM_PARITY_STACK=1

if [ -z "$MMRS_STACK_LOCKED" ]; then
  export MMRS_STACK_LOCKED=1
  exec "$ROOT/scripts/stack-lock.sh" "$0" "$@"
fi

cargo build -p mm-api
pkill -f 'target/debug/mm-api' 2>/dev/null || true
sleep 1
(nohup "$ROOT/target/debug/mm-api" > /tmp/mmrs-mm-api.log 2>&1 &)
for _ in $(seq 30); do
  curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping && break
  sleep 0.5
done
curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping \
  || { echo "mm-api never came up — see /tmp/mmrs-mm-api.log"; exit 1; }
if [ $# -eq 0 ]; then exec cargo test --workspace; else exec cargo test "$@"; fi
