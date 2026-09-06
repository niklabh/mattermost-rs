#!/bin/zsh
# Check every committed mutation plan against the current tree, without running anything.
#
#   scripts/preflight-plans.sh                       every plan
#   scripts/preflight-plans.sh scripts/mutations/x.plan …   just these
#
# `mutate-batch.sh` already pre-flights the plan it is about to run. This runs the same check over
# **all** of them in a second, which is what catches the failure mode that check cannot: a plan
# that was fine when written and has since gone stale because the code it anchors on moved.
#
# Two ways an anchor rots, and the second is the dangerous one:
#
#   0 matches — `mutate.sh` exits 3, `set -e` aborts the batch, and every mutation after it is
#               thrown away. Loud, and expensive only in wall-clock.
#   2+ matches — `mutate.sh` replaces the **first** occurrence, so the mutation lands on whichever
#               copy comes first in the file and the CAUGHT/SURVIVED verdict belongs to a function
#               nobody meant to test. Silent, and it makes the tally a lie.
#
# The unescaping is `printf %b`, deliberately: it is what `mutate.sh` itself uses, and a validator
# that decodes differently from the runner is not a validator. A Python-side `.replace("\\n", …)`
# was tried first and disagreed with the runner on every pattern containing `\\n` — reporting 77
# problems where there were 42, and hiding which was which.
set -e
cd "$(dirname "$0")/.."

PLANS=("$@")
if [ ${#PLANS[@]} -eq 0 ]; then
  PLANS=(scripts/mutations/*.plan)
fi

BAD=0
LINES=0
for PLAN in "${PLANS[@]}"; do
  while IFS=$'\t' read -r NAME FILE FROM TO SUITE FILTER; do
    case "$NAME" in ''|'#'*) continue ;; esac
    LINES=$((LINES + 1))
    if [ ! -f "$FILE" ]; then
      echo "$PLAN: $NAME: names a file that does not exist: $FILE"
      BAD=$((BAD + 1))
      continue
    fi
    PATTERN=$(printf '%b' "$FROM")
    COUNT=$(FILE="$FILE" PATTERN="$PATTERN" python3 -c '
import os
print(open(os.environ["FILE"]).read().count(os.environ["PATTERN"]))
')
    if [ "$COUNT" != "1" ]; then
      echo "$PLAN: $NAME: anchor matches $COUNT times in $FILE"
      BAD=$((BAD + 1))
    fi
  done < "$PLAN"
done

echo "plan lines checked: $LINES, stale anchors: $BAD"
[ "$BAD" -eq 0 ]
