#!/bin/zsh
# Merge finished worktree branches into main, serially, re-running the suite after each.
#
#   scripts/merge-worktrees.sh --plan  wt/members wt/sidebar wt/chanlife wt/postwrite
#   scripts/merge-worktrees.sh         wt/members wt/sidebar wt/chanlife wt/postwrite
#
# `--plan` shows what each merge would touch and where the branches overlap, without merging.
#
# # Why serially, and why the suite after each
#
# Four agents on four stacks produce four branches that each passed the suite *in isolation*, on
# a database only they were using. That is not evidence that they pass together: the parity suites
# share fixture users, teams and channels, and two routes that each sort correctly can still tie
# on a sort key once both sets of fixtures exist. Merging all four and then running the suite once
# tells you something broke; merging one at a time tells you which one.
#
# Stack 0 is the merge stack for exactly this reason — it is the one nobody was developing on, so
# a failure after a merge is about the merge.
#
# # What conflicts, and how
#
# Expect keep-both conflicts, not semantic ones, in the files several agents append to:
#
#   crates/mm-api/src/lib.rs        router `.route(...)` calls and `pub mod` lines
#   crates/mm-store/src/*_store.rs  new trait methods and impls appended per agent
#   crates/mm-api/tests/parity.rs   the `mod` list
#   crates/mm-api/tests/common/mod.rs   shared helpers appended at the end
#   MIGRATION.md, docs/TECH_DEBT.md     appended rows and entries
#
# **A second conflict class that is NOT keep-both, and it does not announce itself**: when both
# agents append to the same file, git can place the `=======` at a point where *both* sides end
# mid-item and the lines that close them sit AFTER the `>>>>>>>` marker — so that shared tail
# closes only the second side. Stripping the three marker lines then produces a file with an
# unclosed delimiter thousands of lines from the actual edit. This happened on
# `channel_store.rs`, in three of its four regions, with three different tails:
#
#     region 1 (trait decls)    both sides end in `;`            nothing to duplicate
#     region 2 (impl methods)   shared tail `    }`              duplicate onto the first side
#     region 3 (free fns)       shared tail `source, })?; Ok(())` and its closing brace
#     region 4 (free fns)       shared tail `}`
#
# So: inspect the last lines of each side and the first lines after the marker before resolving.
# If both sides end mid-item, the first side needs its own copy of the shared tail. `cargo check`
# catches it, but it reports the error at the end of the file rather than at the merge.
#
# **The other conflict class that is NOT keep-both**: axum panics at startup on a duplicate route path,
# so two agents adding different methods to the same path must end up as ONE `.route()` call
# chaining them (`get(a).put(b).delete(c)`), never two calls for the same path. Taking both sides
# of that conflict compiles cleanly and dies at boot, so the suite catches it only because every
# parity test needs a live server. Read every resolved `.route(` hunk before continuing.
#
# This script stops at the first conflict and hands the tree to you. Resolve, `git add`, then
# `git commit` and re-run with the remaining branches.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)

PLAN=0
if [ "${1:-}" = "--plan" ]; then PLAN=1; shift; fi
[ $# -gt 0 ] || { sed -n '2,8p' "$0"; exit 2; }

[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] \
  || { echo "refusing: not on main (on $(git rev-parse --abbrev-ref HEAD))"; exit 1; }
[ -z "$(git status --porcelain)" ] \
  || { echo "refusing: working tree is dirty. Commit or stash first:"; git status --short; exit 1; }

if [ $PLAN -eq 1 ]; then
  echo "=== what each branch touches, and where they overlap ==="
  for b in "$@"; do
    git rev-parse --verify -q "$b" > /dev/null || { echo "  $b: NO SUCH BRANCH"; continue; }
    echo "\n--- $b  ($(git rev-list --count main.."$b") commits)"
    git diff --stat main..."$b" | tail -20
  done
  echo "\n=== files touched by more than one branch (these are your conflicts) ==="
  for b in "$@"; do
    git rev-parse --verify -q "$b" > /dev/null && git diff --name-only main..."$b"
  done | sort | uniq -d | sed 's/^/  /'
  exit 0
fi

for b in "$@"; do
  git rev-parse --verify -q "$b" > /dev/null || { echo "no such branch: $b"; exit 1; }
  echo "\n================ merging $b ================"
  if ! git merge --no-ff --no-edit "$b"; then
    echo "\nCONFLICT merging $b. Files:"
    git diff --name-only --diff-filter=U | sed 's/^/  /'
    echo "\nResolve, 'git add', 'git commit', then re-run with the remaining branches."
    echo "Remember: two .route() calls for one path compile and panic at boot — chain the methods."
    exit 1
  fi
  # Duplicate ledger numbers. Parallel worktrees cannot see each other's allocations, both
  # sides append to different parts of the file, and the merge is clean — so nothing catches this
  # but a check like this one. It happened three times across two entries in one round, and the
  # citations are load-bearing: a parity suite masks fields with the literal string `<D-235>`.
  dupes=$(grep -oE '^## D-[0-9]+ ' docs/TECH_DEBT.md | sort | uniq -d)
  if [ -n "$dupes" ]; then
    echo "\n  DUPLICATE TECH_DEBT numbers after merging $b:"
    echo "$dupes" | sed 's/^/    /'
    echo "  Renumber the newer entry and every citation of it before continuing:"
    echo "    grep -rn 'D-<n>' --include='*.rs' --include='*.md' . | grep -v ./target"
    exit 1
  fi

  echo "\n---- suite on stack 0 after $b ----"
  if ! MMRS_STACK=0 "$ROOT/scripts/parity.sh" > /tmp/mmrs-merge-$(echo "$b" | tr / -).log 2>&1; then
    echo "SUITE FAILED after merging $b — see /tmp/mmrs-merge-$(echo "$b" | tr / -).log"
    grep -E "^(test result: FAILED|failures:|---- )" "/tmp/mmrs-merge-$(echo "$b" | tr / -).log" | head -30
    echo "\nThe merge commit is in place. Fix forward or 'git reset --hard HEAD~1' to drop it."
    exit 1
  fi
  passed=$(grep -oP '^test result: ok\. \K[0-9]+(?= passed)' "/tmp/mmrs-merge-$(echo "$b" | tr / -).log" | paste -sd+ | bc)
  echo "  green: $passed passed"
done
echo "\nall merged, suite green."
