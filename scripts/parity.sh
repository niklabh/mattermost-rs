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
: ${DATABASE_URL:=postgres://mmuser:mmuser_password@localhost:5432/mattermost}
export DATABASE_URL MM_STORE_DB=1 MM_PARITY_STACK=1

if [ -z "$MMRS_STACK_LOCKED" ]; then
  export MMRS_STACK_LOCKED=1
  exec "$ROOT/scripts/stack-lock.sh" "$0" "$@"
fi

# The file backend is a **directory on this host**, and the two servers must agree on which one.
# `scripts/go-server.sh` runs the Go server from `reference/.build/mmroot` with
# `MM_FILESETTINGS_DIRECTORY` pointing at its own `data/`; that is an environment override, so it
# never reaches the configuration document mm-api reads, and without it mm-api would resolve
# `FileSettings.Directory`'s default `./data/` against *its* working directory and answer 404 for
# every file that exists. Same variable, same value, same convention Mattermost uses.
#
# Scoped to the server launch below and **not exported**, because `cargo test` runs in this shell
# too: `config::go_parity::the_env_overlay_preserves_document_values_it_does_not_name` asserts
# that no `MM_` variable is set, and an export here makes that unit test fail for a reason that
# has nothing to do with the code it covers.
MMRS_FILE_DIRECTORY="$ROOT/reference/.build/mmroot/data/"

cargo build -p mm-api
pkill -f 'target/debug/mm-api' 2>/dev/null || true
sleep 1
(MM_FILESETTINGS_DIRECTORY="$MMRS_FILE_DIRECTORY" nohup "$ROOT/target/debug/mm-api" > /tmp/mmrs-mm-api.log 2>&1 &)
for _ in $(seq 30); do
  curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping && break
  sleep 0.5
done
curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping \
  || { echo "mm-api never came up — see /tmp/mmrs-mm-api.log"; exit 1; }
if [ $# -eq 0 ]; then exec cargo test --workspace; else exec cargo test "$@"; fi
