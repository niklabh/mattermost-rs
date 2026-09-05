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
# **It filters test NAMES, not test targets.** `MUTATE_FILTER=db_webhook_owner_filter` — the name
# of a file in `tests/` — matches no test function, so cargo runs **zero** tests, exits 0, and the
# mutation is reported SURVIVED. Two mutations were reported that way before this line existed.
# Filter on something in the test function's own name (`a_named_owner`), and sanity-check a new
# plan line by confirming the CAUGHT message names a test.
#
# For `api`, narrow to the suite(s) under test — leaving this unset lets an unrelated suite
# decide the verdict, which has twice produced a whole run of false CAUGHTs:
#
#   MUTATE_FILTER='sidebar_categories' scripts/mutate.sh ...     # one module's tests
#   MUTATE_FILTER='sidebar' scripts/mutate.sh ...                # multiple modules
#
# Previously separate --test binaries (MUTATE_API_SUITE=parity_foo, MUTATE_API_TARGETS) are
# now converted automatically: `parity_foo` → `foo` in MUTATE_FILTER.
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

# Rebuild mm-api from the mutated source and put it back on :8066.
#
# Two failures used to look identical and be reported as one **HARNESS FAULT** whose message,
# "does not compile, or the server never came up", could not say which. That is the wrong
# question to leave open: a compile error means the mutation was malformed and the plan needs
# fixing, while a slow start means the verdict was lost to load and a re-run would have been
# fine. Two runs in this session hit the second and were investigated as the first. So the build
# failure now prints the compiler's own last lines, and the start is retried once with a longer
# budget — 30 seconds against the old 10, because this machine also carries Postgres, the Go
# server and a concurrent cargo.
restart_server() {
  if ! cargo build -p mm-api > "$WORK/build.log" 2>&1; then
    echo "  the mutated source does not compile:"
    grep -E '^(error|error\[)' "$WORK/build.log" | head -3 | sed 's/^/    /'
    return 1
  fi
  for attempt in 1 2; do
    pkill -f 'target/debug/mm-api' 2>/dev/null || true
    sleep 1
    (nohup "$ROOT/target/debug/mm-api" > "$WORK/mm-api.log" 2>&1 &)
    for _ in $(seq 60); do
      curl -sf -o /dev/null http://127.0.0.1:8066/api/v4/system/ping && return 0
      sleep 0.5
    done
    echo "  mm-api did not answer within 30s (attempt $attempt); last log lines:"
    tail -3 "$WORK/mm-api.log" 2>/dev/null | sed 's/^/    /'
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
  # `--tests` builds all twenty mm-store test binaries, which costs minutes per mutation and
  # lets a suite that never saw the change decide the verdict — the same two problems
  # MUTATE_API_TARGETS exists for. Narrow it the same way:
  #
  #   MUTATE_STORE_TARGETS='--test db_post_channel_page'
  store) cargo test -p mm-store ${=MUTATE_STORE_TARGETS:---tests} ${=MUTATE_FILTER} > "$LOG" 2>&1 || RC=$? ;;
  app)   cargo test -p mm-app --tests ${MUTATE_FILTER:+$MUTATE_FILTER} > "$LOG" 2>&1 || RC=$? ;;
  api)   if restart_server; then
           # The parity tests are now in a single `--test parity` binary. Filter by test name
           # to narrow the suite under test — otherwise an unrelated failure decides the verdict:
           #
           #   MUTATE_FILTER='sidebar_categories' scripts/mutate.sh ...  # one module's tests
           #   MUTATE_FILTER='sidebar' scripts/mutate.sh ...             # multiple modules
           #
           # Legacy MUTATE_API_SUITE and MUTATE_API_TARGETS (from the 35-binary era) are
           # converted for compatibility: `parity_foo` in either variable maps to `foo` in
           # MUTATE_FILTER.
           if [ -z "$MUTATE_FILTER" ]; then
             if [ -n "$MUTATE_API_SUITE" ]; then
               MUTATE_FILTER="${MUTATE_API_SUITE#parity_}"
             elif [ -n "$MUTATE_API_TARGETS" ]; then
               # Convert --test parity_X --test parity_Y to "X|Y" filter pattern
               MUTATE_FILTER=$(echo "$MUTATE_API_TARGETS" | sed 's/--test parity_//g' | tr ' ' '|')
             fi
           fi
           # `--` before the filters: cargo takes at most one positional TESTNAME, so a plan
           # naming two suites (`'post_reactions emoji_get'`) fails with `unexpected argument`
           # unless the words go to libtest, which accepts any number and matches on any.
           if [ -n "$MUTATE_FILTER" ]; then
             cargo test -p mm-api --test parity -- ${=MUTATE_FILTER} > "$LOG" 2>&1 || RC=$?
           else
             cargo test -p mm-api --test parity > "$LOG" 2>&1 || RC=$?
           fi
         else
           RC=1; echo "the mutated server never became testable — see the lines above" > "$LOG"
         fi ;;
  all)   if restart_server; then
           cargo test --workspace ${=MUTATE_FILTER} > "$LOG" 2>&1 || RC=$?
         else
           RC=1; echo "the mutated server never became testable — see the lines above" > "$LOG"
         fi ;;
  *) echo "unknown suite: $SUITE"; exit 2 ;;
esac

restore_source
case "$SUITE" in api|all) restart_server || true ;; esac

if [ $RC -eq 0 ]; then
  echo "$NAME: **SURVIVED** — the suite cannot see this change. Fix the fixture, not the tally."
else
  NAMED=$(grep -h '^test .* FAILED' "$LOG" | head -3 | sed 's/ \.\.\. FAILED//;s/^test //' | paste -sd'; ' -)
  if [ -z "$NAMED" ]; then
    # The suite exited non-zero without failing a named test: a compile error, a server that
    # never came up, or the wrong targets being run. That is a harness fault, not a caught
    # mutation, and reporting it as CAUGHT is how a whole run comes back green on nothing.
    echo "$NAME: **HARNESS FAULT** — non-zero exit but no test failed. Last lines:"
    tail -5 "$LOG" | sed 's/^/    /'
  else
    echo "$NAME: CAUGHT ($NAMED)"
  fi
fi
