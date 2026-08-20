#!/bin/zsh
# Serialise stack-backed test runs across worktrees / sessions.
#
#   scripts/stack-lock.sh <command...>
#
# The parity suites talk to ONE mm-api on :8066 and ONE shared Postgres, and the DB fixture suites
# purge-and-seed shared rows. Two checkouts running them at once produce verdicts that belong to
# neither — a no-op control "failing" on scheduling luck was the whole finding of the
# getChannelUnread session. Anything that starts the server or touches the stack database goes
# through this lock. macOS has no flock(1), so this is an atomic-mkdir lock with stale-pid reaping.
set -e
LOCK=/tmp/mmrs-stack.lock
while ! mkdir "$LOCK" 2>/dev/null; do
  if [ -f "$LOCK/pid" ] && ! kill -0 "$(cat "$LOCK/pid")" 2>/dev/null; then
    rm -rf "$LOCK"; continue
  fi
  sleep 2
done
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK"' EXIT INT TERM
"$@"
