#!/bin/zsh
# Create a git worktree pinned to its own stack, ready for an agent to work in.
#
#   scripts/worktree.sh add route-members 1     worktree on stack 1, branch wt/route-members
#   scripts/worktree.sh list
#   scripts/worktree.sh rm route-members
#
# # What "pinned" means
#
# The worktree gets a `.mmrs-stack` file, which `scripts/stack-env.sh` reads when `MMRS_STACK` is
# unset — so `scripts/parity.sh` and `scripts/mutate.sh` inside it talk to that stack's Postgres,
# Go server and mm-api without anyone remembering a variable. Two worktrees on two stacks share
# no port, no database and no lock, which is the whole point.
#
# # The three things a worktree does not get from git, and needs
#
# * **`reference/mattermost`** is gitignored as a directory, so a fresh worktree has no Go source
#   to read. Symlinked to the main checkout's clone. It also means the symlink shows up untracked
#   inside the worktree; `git rm --cached reference/mattermost` before committing if it ever gets
#   added.
# * **`reference/.build`** holds the compiled Go server, shared by every stack — building it per
#   worktree would cost minutes each. Symlinked too. The per-stack run directories
#   (`mmroot-<n>`) live inside it and stay separate.
# * **`.env`** carries `DATABASE_URL` for the sqlx compile-time macros, which read it directly
#   rather than through `stack-env.sh`. Written pointing at this stack's Postgres.
#
# # Pre-build, always
#
# A cold `target/` plus the full suite exceeds a ten-minute command timeout, so `add` kicks off
# `cargo test --no-run` in the background and tells you where the log is. Wait for it before
# asking an agent to run anything.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
TREES="${MMRS_WORKTREES:-$(dirname "$ROOT")/mmrs-worktrees}"

usage() { sed -n '2,8p' "$0"; exit 2; }

case "${1:-}" in
  add)
    NAME="${2:?worktree name}"; STACK="${3:?stack number}"
    DEST="$TREES/$NAME"
    mkdir -p "$TREES"

    # **One stack, one worktree.** Two worktrees pinned to the same stack share a Postgres, a Go
    # server and — the part that actually bites — one `purge_api_fixtures`, so each run deletes
    # the other's fixtures mid-test. That configuration existed unnoticed for a whole round
    # (`probe` and `threads` both on stack 1) and is the leading suspect for [D-284], a websocket
    # flake three worktrees reported and the merge stack could never reproduce.
    # Written without zsh glob qualifiers on purpose: the shebang says zsh but this script gets
    # invoked with `bash scripts/worktree.sh` often enough that a zsh-only `(N)` is a syntax error
    # half the time it matters.
    for existing in "$TREES"/*/.mmrs-stack; do
      [ -f "$existing" ] || continue
      if [ "$(cat "$existing")" = "$STACK" ]; then
        echo "stack $STACK is already claimed by $(basename "$(dirname "$existing")")" >&2
        echo "pick another, or 'scripts/worktree.sh rm' the one that has it." >&2
        exit 1
      fi
    done
    git worktree add -b "wt/$NAME" "$DEST" HEAD
    ln -sfn "$ROOT/reference/mattermost" "$DEST/reference/mattermost"
    ln -sfn "$ROOT/reference/.build"     "$DEST/reference/.build"
    echo "$STACK" > "$DEST/.mmrs-stack"
    echo "DATABASE_URL=postgres://mmuser:mmuser_password@localhost:$((5432 + STACK))/mattermost" > "$DEST/.env"
    echo "$DEST  ->  stack $STACK"
    (cd "$DEST" && nohup cargo test --workspace --no-run > /tmp/mmrs-prebuild-$NAME.log 2>&1 &)
    echo "pre-building in the background: /tmp/mmrs-prebuild-$NAME.log"
    ;;
  list) git worktree list ;;
  rm)
    NAME="${2:?worktree name}"
    git worktree remove --force "$TREES/$NAME"
    git branch -D "wt/$NAME" 2>/dev/null || true
    echo "removed $NAME"
    ;;
  *) usage ;;
esac
