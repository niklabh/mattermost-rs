#!/bin/zsh
# Keep the committed `.sqlx/` offline query cache honest.
#
#   scripts/sqlx-cache.sh check      exit 1 if any query! in the workspace is missing from .sqlx/,
#                                    or .sqlx/ holds an entry no query uses (D-332's guard)
#   scripts/sqlx-cache.sh prepare    regenerate .sqlx/ against this stack's database (D-805's fix)
#
# # Why a guard at all
#
# Every session runs with `DATABASE_URL` set and a live Postgres, so the `query!` macros check the
# schema directly and never read `.sqlx/`. The cache therefore rots silently — it was found 233
# entries short on 2026-09-12 and thirty queries stale again four days later — while README.md
# says the workspace builds without a database. `check` is what turns that sentence into something
# the tree asserts.
#
# # Its own target directory
#
# `cargo sqlx prepare` cleans and rebuilds every crate that uses the macros. Run in `target/` that
# throws away the dev build — and a mutation batch or parity run in progress with it, since both
# rebuild on the same directory. So it builds in `target/sqlx-cache` instead: slower the first
# time, and harmless to anything else running.
#
# `--all-targets` because the DB-backed suites under `tests/` use `query!` too, and an offline
# `cargo test` needs their entries as much as `cargo check` needs the library's.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
source "$ROOT/scripts/stack-env.sh"

export CARGO_TARGET_DIR="$ROOT/target/sqlx-cache"

case "$1" in
  check)
    cargo sqlx prepare --workspace --check -- --all-targets
    ;;
  prepare)
    cargo sqlx prepare --workspace -- --all-targets
    # Prove the result rather than trusting the tool. Not `SQLX_OFFLINE=true cargo check`: cargo
    # does not rebuild a crate because that variable changed, so on a warm target directory it
    # finishes in a tenth of a second having re-run no macro at all. `--check` cleans first.
    cargo sqlx prepare --workspace --check -- --all-targets
    ;;
  *)
    sed -n '2,6p' "$0"
    exit 2
    ;;
esac
