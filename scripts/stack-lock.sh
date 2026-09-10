#!/bin/zsh
# Serialise stack-backed test runs across worktrees / sessions.
#
#   scripts/stack-lock.sh <command...>
#
# The parity suites talk to ONE mm-api and ONE Postgres, and the DB fixture suites purge-and-seed
# shared rows. Two checkouts running them against the **same stack** at once produce verdicts that
# belong to neither — a no-op control "failing" on scheduling luck was the whole finding of the
# getChannelUnread session. Anything that starts a server or touches a stack database goes through
# this lock. macOS has no flock(1), so this is an atomic-mkdir lock with stale-pid reaping.
#
# **The lock is per stack** (`/tmp/mmrs-stack-<n>.lock`, see stack-env.sh). It used to be one lock
# for the machine, which made it the throughput ceiling: a twenty-minute mutation batch blocked
# every other worktree, so the parallel-worktree pattern could not actually run in parallel. Two
# worktrees on two stacks now share nothing — not a port, not a database, not this lock.
set -e
source "$(dirname "$0")/stack-env.sh"
LOCK="$MMRS_LOCK"
while ! mkdir "$LOCK" 2>/dev/null; do
  if [ -f "$LOCK/pid" ] && ! kill -0 "$(cat "$LOCK/pid")" 2>/dev/null; then
    rm -rf "$LOCK"; continue
  fi
  sleep 2
done
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK"' EXIT INT TERM
"$@"
