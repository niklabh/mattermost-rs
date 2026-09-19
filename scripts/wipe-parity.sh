#!/bin/zsh
# Run the destructive parity suite — `DELETE /api/v4/users` over the local socket, which erases
# every account — against a stack recreated for the purpose, and take that stack down after.
#
#   scripts/wipe-parity.sh            stack 6
#   scripts/wipe-parity.sh 7          another spare stack
#   MMRS_WIPE_KEEP=1 scripts/wipe-parity.sh   leave the stack up afterwards, to inspect it
#
# # Why a script and not a filter
#
# `parity::users_wipe` erases the fixture administrator every other suite logs in as, so it can
# never run on a worktree's own stack. It does nothing unless `MMRS_WIPE_PARITY` names the stack
# the binary targets, and refuses a database with more accounts than a fresh stack holds; this
# script is the one place that sets the variable, and it does so only after `stack.sh down` and
# `up` have given the stack a brand-new volume.
#
# The parity binary's base URLs are compile-time (`MMRS_GO_BASE`), so this rebuilds it for the
# target stack; the next ordinary `scripts/parity.sh` in the worktree rebuilds it back.
#
# **Not concurrently with a run on the worktree's own stack.** `parity.sh` frees its mm-api by
# binary path, and one checkout has one binary, so starting this stack's mm-api stops the other
# stack's — a mutation batch in flight there would lose its server.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)

K="${1:-6}"
case "$K" in
  ''|*[!0-9]*) echo "the stack must be a number, got '$K'" >&2; exit 2 ;;
esac
if [ "$K" = 0 ]; then
  echo "refusing stack 0: it is the historical shared stack" >&2
  exit 2
fi
if [ -f "$ROOT/.mmrs-stack" ] && [ "$K" = "$(tr -dc '0-9' < "$ROOT/.mmrs-stack")" ]; then
  echo "refusing stack $K: it is this worktree's own stack" >&2
  exit 2
fi

echo "recreating stack $K for a destructive run…"
"$ROOT/scripts/stack.sh" down "$K"
"$ROOT/scripts/stack.sh" up "$K"

set +e
MMRS_STACK="$K" MMRS_WIPE_PARITY="$K" "$ROOT/scripts/parity.sh" --test parity users_wipe
RC=$?
set -e

if [ -z "${MMRS_WIPE_KEEP:-}" ]; then
  "$ROOT/scripts/stack.sh" down "$K"
  # `parity.sh` started this stack's mm-api, and `stack.sh down` does not match it: free its port.
  (MMRS_STACK="$K" source "$ROOT/scripts/stack-env.sh" && mmrs_free_port "$MMRS_API_PORT")
  echo "stack $K is down (its volume is gone)"
else
  echo "stack $K left up, erased, for inspection; take it down with scripts/stack.sh down $K"
fi
exit $RC
