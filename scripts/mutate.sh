#!/bin/zsh
# Mutation testing harness. See CLAUDE.md § "Mutation testing".
#
#   scripts/mutate.sh <name> <file> <from> <to> [suite]
#
# Applies a one-shot literal replacement, runs a suite, restores, and reports CAUGHT or SURVIVED.
# A survivor is a finding about the tests, not a shrug.
#
#   suite = unit    cargo test --workspace --lib          (no stack; fast, the default)
#           store   mm-store DB-backed tests               (needs Postgres)
#           app     mm-app DB-backed tests                 (needs Postgres)
#           api     mm-api parity suites                   (needs Postgres + Go + a live mm-api)
#           all     everything
#
# Pick the *narrowest* suite that should catch the mutation. The whole point of a mutation is that
# you can predict which test dies; running everything turns a 5-second check into a 90-second one.
#
# Narrow it further with MUTATE_FILTER, a cargo-test name filter:
#
#   MUTATE_FILTER=authorization:: scripts/mutate.sh ...
#
# `unit` without a filter runs the 47-second PBKDF2 suite on every single mutation, which is the
# whole cost of a fifteen-mutation run. Filtering to the module under test cuts each one to
# seconds. Set it to the narrowest name that still contains every test able to catch the change —
# too narrow and a SURVIVED verdict means nothing.
#
# Prerequisites for `store`, `api` and `all`:
#   docker compose up -d
#   export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
#
# The `api` suite needs a running mm-api built from the *mutated* source, so the harness rebuilds
# and restarts it — and rebuilds and restarts it again on the way out. Restoring the source is not
# restoring the system when the thing under test is a server: a mutated binary left bound to :8066
# once produced a genuine-looking 500 that belonged to the previous mutation.

set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
: ${DATABASE_URL:=postgres://mmuser:mmuser_password@localhost:5432/mattermost}
export DATABASE_URL MM_STORE_DB=1 MM_PARITY_STACK=1

NAME="$1"; FILE="$2"; FROM="$3"; TO="$4"; SUITE="${5:-unit}"
[ -n "$FILE" ] || { sed -n '2,28p' "$0"; exit 2; }

# Stack-backed suites share :8066 and the database with every other checkout; serialise them.
case "$SUITE" in
  store|app|api|all)
    if [ -z "$MMRS_STACK_LOCKED" ]; then
      export MMRS_STACK_LOCKED=1
      exec "$ROOT/scripts/stack-lock.sh" "$0" "$@"
    fi ;;
esac


WORK=$(mktemp -d)
# Restore the source on **any** exit, including SIGINT/SIGTERM. A bare `EXIT` trap does not run
# when the shell is killed, and a timeout that lands mid-run then leaves the mutation applied in
# the working tree — measured, and it survived into a later `cargo test` before being noticed.
trap 'rm -rf "$WORK"' EXIT
trap 'restore_source 2>/dev/null; rm -rf "$WORK"; exit 143' TERM INT HUP
BACKUP="$WORK/backup"; LOG="$WORK/test.log"
cp "$FILE" "$BACKUP"

restore_source() { cp "$BACKUP" "$FILE"; }

restart_server() {
  cargo build -p mm-api > "$WORK/build.log" 2>&1 || return 1
  pkill -f 'target/debug/mm-api' 2>/dev/null || true
  sleep 1
  (nohup "$ROOT/target/debug/mm-api" > "$WORK/mm-api.log" 2>&1 &)
  for _ in $(seq 20); do
    curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping && return 0
    sleep 0.5
  done
  return 1
}

python3 - "$FILE" "$FROM" "$TO" <<'PY' || { echo "$NAME: SKIPPED (pattern not found — has the file been reformatted?)"; exit 3; }
import sys, io
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
body = io.open(path, encoding="utf-8").read()
if old not in body:
    sys.exit(1)
io.open(path, "w", encoding="utf-8").write(body.replace(old, new, 1))
PY

RC=0
case "$SUITE" in
  unit)  cargo test --workspace --lib ${MUTATE_FILTER:+$MUTATE_FILTER} > "$LOG" 2>&1 || RC=$? ;;
  store) cargo test -p mm-store --tests ${MUTATE_FILTER:+$MUTATE_FILTER} > "$LOG" 2>&1 || RC=$? ;;
  app)   cargo test -p mm-app --tests ${MUTATE_FILTER:+$MUTATE_FILTER} > "$LOG" 2>&1 || RC=$? ;;
  api)   if restart_server; then
           cargo test -p mm-api --tests > "$LOG" 2>&1 || RC=$?
         else
           RC=1; echo "does not compile, or the server never came up" > "$LOG"
         fi ;;
  all)   if restart_server; then
           cargo test --workspace > "$LOG" 2>&1 || RC=$?
         else
           RC=1; echo "does not compile, or the server never came up" > "$LOG"
         fi ;;
  *) echo "unknown suite: $SUITE"; exit 2 ;;
esac

restore_source
case "$SUITE" in api|all) restart_server || true ;; esac

if [ $RC -eq 0 ]; then
  echo "$NAME: **SURVIVED** — the suite cannot see this change. Fix the fixture, not the tally."
else
  echo "$NAME: CAUGHT ($(grep -h '^test .* FAILED' "$LOG" | head -3 | sed 's/ \.\.\. FAILED//;s/^test //' | paste -sd'; ' -))"
fi
