#!/bin/zsh
# Run a batch of mutations under **one** acquisition of the stack lock.
#
#   scripts/mutate-batch.sh <plan-file>
#
# The plan is one mutation per line, **tab**-separated:
#
#   name<TAB>file<TAB>from<TAB>to<TAB>suite[<TAB>filter]
#
# The optional sixth field is the `MUTATE_FILTER` for that one line, overriding the environment.
# It exists because a plan covering several routes has no single filter that fits: the `api`
# suites are named after their routes, libtest takes one filter, and leaving it unset lets an
# unrelated suite decide every verdict — the failure mode `mutate.sh` warns about. Before this
# field, a four-route plan had to be split into four files, each with its own control.
#
# Tab rather than a printable separator because the patterns are Rust and SQL, which contain `|`,
# `&`, `%` and `,` freely but never a literal tab — rustfmt emits spaces. `\n` in `from`/`to`
# becomes a newline, so a multi-line pattern still fits on one plan line.
#
# Why a batch rather than a loop of `scripts/mutate.sh`: each stack-backed mutation rebuilds and
# restarts the server on :8066, and taking the lock per mutation lets a sibling worktree replace
# that binary between the build and the assertion. A fifteen-mutation run then produces verdicts
# that belong to whichever checkout happened to win each race — the failure mode `stack-lock.sh`
# exists to prevent, reintroduced by holding the lock too briefly.
#
# Lines beginning with `#` and blank lines are skipped, so a plan can carry its own commentary.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
PLAN="$1"
[ -n "$PLAN" ] && [ -f "$PLAN" ] || { sed -n '2,16p' "$0"; exit 2; }

if [ -z "$MMRS_STACK_LOCKED" ]; then
  export MMRS_STACK_LOCKED=1
  exec "$ROOT/scripts/stack-lock.sh" "$0" "$@"
fi

RUN=0
CAUGHT=0
SURVIVED=0
FAULTS=0
while IFS=$'\t' read -r NAME FILE FROM TO SUITE FILTER; do
  case "$NAME" in ''|'#'*) continue ;; esac
  RUN=$((RUN + 1))
  FROM=$(printf '%b' "$FROM")
  TO=$(printf '%b' "$TO")
  OUT=$(MUTATE_FILTER="${FILTER:-$MUTATE_FILTER}" \
    "$ROOT/scripts/mutate.sh" "$NAME" "$FILE" "$FROM" "$TO" "${SUITE:-unit}")
  echo "$OUT"
  case "$OUT" in
    *SURVIVED*)       SURVIVED=$((SURVIVED + 1)) ;;
    *"HARNESS FAULT"*) FAULTS=$((FAULTS + 1)) ;;
    *CAUGHT*)         CAUGHT=$((CAUGHT + 1)) ;;
    *)                FAULTS=$((FAULTS + 1)); echo "  (neither caught nor survived)" ;;
  esac
done < "$PLAN"

echo
echo "tally: $RUN run, $CAUGHT caught, $SURVIVED survived, $FAULTS harness faults"
[ "$FAULTS" -eq 0 ] || echo "A harness fault voids the whole run — fix it and re-run, do not report the tally."
