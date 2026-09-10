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
# `--no-fail-fast`, and a summary line you cannot misread.
#
# Two failure modes, both measured in the four-worktree session of 2026-09-10, both of which
# produce a **false green**:
#
#  1. Bare `cargo test --workspace` stops at the first failing binary. One failure in an early
#     target left 45 of 48 unrun, and the output still ends in a wall of `test result: ok` —
#     so a reader scrolling to the bottom sees green. `--no-fail-fast` runs them all.
#  2. `scripts/parity.sh | tail` reports **tail's** exit status, not cargo's. That is ordinary
#     shell pipeline behaviour rather than a bug here, but it is how an agent reads `exit=0`
#     off a run that failed, so the summary below states the verdict in words that survive a
#     `tail`.
#
# The summary counts binaries, not just tests: "52 targets" is the assertion that nothing was
# skipped, which is the half a passing test count cannot tell you.
LOG=$(mktemp "/tmp/mmrs-parity$MMRS_STACK_SUFFIX-XXXX.log")
# `RC` is captured **inside** each branch, immediately after the pipeline. Reading
# `$pipestatus` after the closing `fi` yields the compound statement's status — which is
# `tee`'s, always 0 — and the summary then printed GREEN over a run cargo had just called
# failed. Measured, once, on this line.
if [ $# -eq 0 ]; then
  cargo test --workspace --no-fail-fast 2>&1 | tee "$LOG"
  RC=${pipestatus[1]}
else
  cargo test "$@" --no-fail-fast 2>&1 | tee "$LOG"
  RC=${pipestatus[1]}
fi

targets=$(grep -c '^test result:' "$LOG" || true)
passed=$(grep -oE '^test result: (ok|FAILED)\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | paste -sd+ | bc)
failed=$(grep -oE '[0-9]+ failed' "$LOG" | grep -oE '^[0-9]+' | paste -sd+ | bc)
ignored=$(grep -oE '[0-9]+ ignored' "$LOG" | grep -oE '^[0-9]+' | paste -sd+ | bc)
echo ""
echo "================================================================"
if [ "$RC" -eq 0 ]; then
  echo "  PARITY SUITE GREEN   ${targets} targets, ${passed:-0} passed, ${ignored:-0} ignored"
else
  echo "  PARITY SUITE **FAILED**   ${targets} targets, ${passed:-0} passed, ${failed:-0} FAILED"
  echo ""
  # Re-run each failing test **alone**, and say which kind of failure this was.
  #
  # This suite has a standing class of tests that pass alone and fail in a concurrent run:
  # whole-database aggregates (`users_stats::the_count_matches_the_database_including_bots`,
  # `system_usage`'s rounded post count) read a global number while a sibling test on another
  # thread is creating or deleting fixture rows, and `user_get::each_servers_etag_round_trips_to_
  # its_own_304` is the same shape on an etag. They are real races in the *tests*, not in the
  # routes, and the project has documented them for several sessions.
  #
  # The reason to automate the check rather than trust a human to remember it: this is the
  # verdict a merge is decided on. Told "1 failed", a reader either dismisses a genuine
  # regression as "the usual flake" or reverts a good branch over a race. Re-running the
  # failures in isolation distinguishes the two in about two seconds and states it in words.
  failing=(${(f)"$(grep -oE '^    [a-z_]+::[a-z_:]+' "$LOG" | sed 's/^ *//' | sort -u)"})
  grep -E '^failures:' -A0 "$LOG" > /dev/null && printf '  failed: %s\n' "${failing[@]}"
  if [ ${#failing[@]} -gt 0 ] && [ ${#failing[@]} -le 12 ]; then
    echo ""
    echo "  re-running each alone to separate a real regression from a test-level race…"
    still=0
    for t in "${failing[@]}"; do
      if cargo test -p mm-api --test parity "$t" -- --exact --test-threads=1 \
           > "$LOG.solo" 2>&1; then
        echo "    PASSES ALONE  $t   (concurrency-sensitive test, not a route regression)"
      else
        echo "    FAILS ALONE   $t   <-- REAL"
        still=$((still + 1))
      fi
    done
    echo ""
    if [ "$still" -eq 0 ]; then
      echo "  VERDICT: every failure passed in isolation. No route regression indicated."
      echo "           Do not merge on this alone — but do not revert on it either."
    else
      echo "  VERDICT: $still failure(s) reproduce in isolation. Treat as REAL."
    fi
  fi
fi
echo "  full log: $LOG"
echo "================================================================"
exit "$RC"
