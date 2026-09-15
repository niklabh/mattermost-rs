#!/bin/zsh
# Run the stack-backed suites against a freshly built mm-api from THIS checkout, under the stack
# lock (see stack-lock.sh).
#
#   scripts/parity.sh                               cargo test --workspace (unit + store + api)
#                                                   — the parity binary in MMRS_PARITY_SHARDS (2) runs
#   MMRS_PARITY_PLAN_ONLY=1 scripts/parity.sh       print the shard plan and exit, touching no stack
#   scripts/parity.sh -p mm-api --test parity      just the parity suite
#   scripts/parity.sh --test parity users_me       one module's tests
#
# Builds mm-api from this tree, replaces whatever is bound to :8066 with it, runs the tests with
# MM_STORE_DB=1 MM_PARITY_STACK=1, and leaves the server running. Whoever runs next rebuilds and
# replaces it again — the binary on :8066 always belongs to the checkout that last tested.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)

# # The parity binary runs in shards (D-800)
#
# An unlicensed Go refuses a new user once 250 active non-bot users exist, and one run of the
# parity binary creates ~230 plain users that nothing retires before the binary exits — 62 once-cell
# fixtures alone hold ~150 for the whole run. Measured on stack 0, 2026-09-15: a peak of 252, and
# five create-user refusals failing tests that pass alone. Every new suite raises the peak.
#
# So the no-argument run executes the parity binary once per shard, sequentially. Each invocation
# is a new process, so `purge_api_fixtures` runs again at its start and deletes the previous
# shard's `mmrsplain%` users; the peak is roughly one shard's share. Shards are balanced by
# `create_plain_user(` call sites per module (plus one, so user-free modules spread too), and
# built from the `pub mod` list in `tests/parity.rs`, so a new module is always assigned. A shard
# is expressed as `--skip parity::<module>::` for every module outside it: a module missing from
# every other shard's list is never skipped, which duplicates a module rather than dropping one.
#
# What this changes besides the peak: modules in different shards no longer run concurrently, so
# a race between two suites shows up only when they share a shard.
parity_shard_plan() {
  local shards=${MMRS_PARITY_SHARDS:-2}
  local -a mods ordered
  local line m w k best
  mods=(${(f)"$(sed -nE 's/^[[:space:]]*pub mod ([a-z0-9_]+);.*/\1/p' "$ROOT/crates/mm-api/tests/parity.rs")"})
  ordered=(${(f)"$(for m in $mods; do
      w=$(grep -c 'create_plain_user(' "$ROOT/crates/mm-api/tests/parity/$m.rs" 2>/dev/null || true)
      echo "$(( ${w:-0} + 1 )) $m"
    done | sort -k1,1nr -k2,2)"})
  typeset -gA MMRS_SHARD_OF
  typeset -ga MMRS_SHARD_LOAD MMRS_SHARD_COUNT
  MMRS_SHARD_OF=()
  MMRS_SHARD_LOAD=()
  MMRS_SHARD_COUNT=()
  for k in {1..$shards}; do
    MMRS_SHARD_LOAD[$k]=0
    MMRS_SHARD_COUNT[$k]=0
  done
  for line in $ordered; do
    w=${line%% *}
    m=${line#* }
    best=1
    for k in {1..$shards}; do
      if (( MMRS_SHARD_LOAD[$k] < MMRS_SHARD_LOAD[$best] )); then best=$k; fi
    done
    MMRS_SHARD_OF[$m]=$best
    MMRS_SHARD_LOAD[$best]=$(( MMRS_SHARD_LOAD[$best] + w ))
    MMRS_SHARD_COUNT[$best]=$(( MMRS_SHARD_COUNT[$best] + 1 ))
  done
  typeset -g MMRS_SHARDS=$shards
  typeset -g MMRS_SHARD_MODULES=${#mods}
}

parity_shard_print() {
  local k
  echo "parity shards: $MMRS_SHARDS over $MMRS_SHARD_MODULES modules"
  for k in {1..$MMRS_SHARDS}; do
    echo "  shard $k: ${MMRS_SHARD_COUNT[$k]} modules, weight ${MMRS_SHARD_LOAD[$k]}"
  done
  # A duplicated `pub mod` line would collapse into one key and silently shrink the plan.
  if [ "${#MMRS_SHARD_OF}" -ne "$MMRS_SHARD_MODULES" ]; then
    echo "  the shard plan assigned ${#MMRS_SHARD_OF} of $MMRS_SHARD_MODULES modules" >&2
    return 1
  fi
}

# Sets MMRS_SKIPS to the `--skip` arguments that confine the parity binary to shard $1.
parity_shard_skips() {
  local m
  typeset -ga MMRS_SKIPS
  MMRS_SKIPS=()
  for m in ${(k)MMRS_SHARD_OF}; do
    if [ "${MMRS_SHARD_OF[$m]}" != "$1" ]; then
      MMRS_SKIPS+=(--skip "parity::${m}::")
    fi
  done
}

if [ -n "${MMRS_PARITY_PLAN_ONLY:-}" ]; then
  parity_shard_plan
  parity_shard_print
  total_skipped=0
  for k in {1..$MMRS_SHARDS}; do
    parity_shard_skips "$k"
    echo "  shard $k skips $(( ${#MMRS_SKIPS} / 2 )) modules, e.g. ${MMRS_SKIPS[2]}"
    total_skipped=$(( total_skipped + ${#MMRS_SKIPS} / 2 ))
  done
  # Across N shards every module is skipped N-1 times: that is "in exactly one shard".
  echo "  modules skipped in total: $total_skipped (expected $(( MMRS_SHARD_MODULES * (MMRS_SHARDS - 1) )))"
  exit 0
fi
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
# Free this stack's port, then make sure this checkout has no server left over.
#
# **The port is the owner key, not the path.** The path-scoped `pkill` below used to be the only
# step, on the reasoning that two worktrees have different paths so one stack's restart cannot
# kill another's server. That reasoning has a hole: a process's command line is fixed at `exec`
# time and does **not** follow a `git worktree move`. Renaming a worktree between rounds therefore
# leaves an mm-api whose cmdline still names the old directory, which the new pattern cannot
# match — so it keeps the port, the freshly built binary never binds, and the suite runs against
# **last round's server**.
#
# That is not a hypothetical and it does not announce itself. Measured on 2026-09-11: a server
# left from a worktree previously named `threads` held :8166, and 36 tests across `token_writes`,
# `command_writes`, `bot_writes`, `file_bytes` and `emoji_get` failed with "was forwarded to Go" —
# every one of them a route that branch predated. The same shape could as easily produce a false
# *pass*, since an older binary still serves the routes it did have.
#
# The kill-by-port itself is `mmrs_free_port` in `stack-env.sh`, shared with the three Go server
# scripts, which had the same hole for a second reason (a worktree's `reference/.build` is a
# symlink, so one run directory has as many path spellings as there are checkouts). The
# path-scoped kill stays as a second pass so a process that is running but not listening is
# still cleared.
mmrs_free_port "$MMRS_API_PORT"
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
  # Every binary but the parity tests, then the parity binary shard by shard — see the header.
  # All of it goes to one log, so the counts and the isolation re-runs below read it unchanged;
  # "targets" therefore counts the parity binary once per shard, plus once with every test skipped.
  parity_shard_plan
  parity_shard_print > "$LOG"
  cat "$LOG"
  cargo test --workspace --no-fail-fast -- --skip parity:: 2>&1 | tee -a "$LOG"
  RC=${pipestatus[1]}
  for k in {1..$MMRS_SHARDS}; do
    parity_shard_skips "$k"
    echo "---- parity shard $k of $MMRS_SHARDS ----" | tee -a "$LOG"
    cargo test -p mm-api --test parity --no-fail-fast -- "${MMRS_SKIPS[@]}" 2>&1 | tee -a "$LOG"
    shard_rc=${pipestatus[1]}
    if [ "$RC" -eq 0 ]; then RC=$shard_rc; fi
  done
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
  # `[A-Za-z0-9_:]`, not `[a-z_:]` — a digit in a test name (`a_304_is_not_activity…`) truncated
  # the match to `a_`, and the isolation re-run below then matched **no test**, ran zero tests,
  # exited 0 and was reported as "PASSES ALONE". A filter that matches nothing looks exactly like
  # a filter whose test passed; that is the same trap `mutate.sh`'s header documents, reproduced
  # here. Hence both this character class and the zero-tests guard below.
  failing=(${(f)"$(grep -oE '^    [A-Za-z0-9_]+::[A-Za-z0-9_:]+' "$LOG" | sed 's/^ *//' | sort -u)"})
  grep -E '^failures:' -A0 "$LOG" > /dev/null && printf '  failed: %s\n' "${failing[@]}"
  if [ ${#failing[@]} -gt 0 ] && [ ${#failing[@]} -le 12 ]; then
    echo ""
    echo "  re-running each alone to separate a real regression from a test-level race…"
    still=0
    for t in "${failing[@]}"; do
      # `set -e` would end the script on a failing rerun before "FAILS ALONE" could be printed —
      # which is what happened, three times on 2026-09-14: a real failure looked like a script
      # that stopped writing. The `if` keeps a non-zero exit from being fatal.
      if cargo test -p mm-api --test parity "$t" -- --exact --test-threads=1 \
        > "$LOG.solo" 2>&1; then
        solo_rc=0
      else
        solo_rc=$?
      fi
      # A run of zero tests exits 0. Treat it as inconclusive, never as a pass.
      ran=$(grep -oE 'test result: (ok|FAILED)\. [0-9]+ passed; [0-9]+ failed' "$LOG.solo" \
            | grep -oE '[0-9]+' | paste -sd+ | bc)
      if [ "${ran:-0}" -eq 0 ]; then
        echo "    NO TEST MATCHED  $t   <-- inconclusive, check the name by hand"
        still=$((still + 1))
      elif [ "$solo_rc" -eq 0 ]; then
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
# The licensed mm-api that `common::licensed` starts is a static in each test binary and is not
# dropped when the binary exits; it belongs to this run, so it ends with it. (The next run's
# `SecondServer::start` would free the port anyway — this keeps `ss -ltnp` honest in between.)
mmrs_free_port "$((MMRS_API_PORT + 24))" >/dev/null
exit "$RC"
