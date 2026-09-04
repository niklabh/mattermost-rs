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

# Pre-flight: every `from` pattern must occur **exactly once** in its file before anything runs.
#
# `mutate.sh` exits 3 when a pattern is missing, and `set -e` then aborts the whole batch — so a
# single stale anchor throws away every mutation after it. That cost a twenty-minute run to a
# `\'` that `printf %b` does not unescape (it passes `\n` and `\\` through, and leaves an
# unknown escape like `\'` alone), which a Python-side check had already read as fine. Checking
# with the *same* `printf %b` the loop uses is the point: a validator that decodes differently
# from the runner is not a validator.
#
# A pattern occurring **twice** is just as bad and silent: `mutate.sh` replaces the first
# occurrence, so an ambiguous anchor mutates whichever copy comes first in the file and the
# verdict belongs to a function nobody meant to test.
PREFLIGHT=0
while IFS=$'\t' read -r NAME FILE FROM TO SUITE FILTER; do
  case "$NAME" in ''|'#'*) continue ;; esac
  if [ ! -f "$FILE" ]; then
    echo "plan: $NAME names a file that does not exist: $FILE"; PREFLIGHT=1; continue
  fi
  # An escape `printf %b` does not know is passed through as backslash-plus-character, which in
  # a Rust or SQL pattern is almost always a typo. `\'` is the one that keeps happening: a quote
  # needs no escaping in a tab-separated field, and `b\'\n\'` reaches rustc as an unterminated
  # character literal. That cost two full runs — once undiagnosed, once diagnosed — before this
  # check existed. Checked on `to` as well as `from`, because a broken `to` is the expensive
  # half: the pattern applies, the crate does not compile, and the verdict is lost.
  for FIELD in "$FROM" "$TO"; do
    BADESC=$(FIELD="$FIELD" python3 -c '
import os, re, sys
bad = sorted(set(re.findall(r"\\(.)", os.environ["FIELD"])) - set("abefnrtv\\0x"))
print(" ".join("\\" + c for c in bad))
')
    [ -z "$BADESC" ] || { echo "plan: $NAME has escapes printf %b will not expand: $BADESC"; PREFLIGHT=1; }
  done

  HITS=$(FROM=$(printf '%b' "$FROM") python3 -c '
import io, os, sys
print(io.open(sys.argv[1], encoding="utf-8").read().count(os.environ["FROM"]))
' "$FILE")
  [ "$HITS" = "1" ] || { echo "plan: $NAME matches $HITS times in $FILE (want exactly 1)"; PREFLIGHT=1; }
done < "$PLAN"
[ "$PREFLIGHT" -eq 0 ] || { echo "plan does not apply to this tree — nothing was run."; exit 2; }

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
