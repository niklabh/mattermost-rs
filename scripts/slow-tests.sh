#!/bin/zsh
# The tests `#[ignore]`d for wall clock, run on purpose.
#
#   scripts/slow-tests.sh
#
# # Why anything is ignored at all
#
# [D-217] measured the workspace suite at ~110s and found half of it in `mm-app`'s password
# module. The entry guessed the cause was "42 password tests" and proposed ignoring a
# cost-factor sweep. Timing them one at a time says something narrower: **the module's 55s wall
# clock is essentially one test.** libtest runs them in parallel, so the module costs as much as
# its slowest member, and the distribution is
#
#   55.3s  password::verify_go_parity::compare_matches_go
#   11.9s  password::verify_go_parity::hashes_go_wrote_verify_here
#   11.6s  password::pbkdf2_hasher::go_parity::recomputes_go_s_hashes_byte_for_byte
#    5.8s  password::verify_go_parity::a_hash_we_write_verifies_through_the_router
#    4.0s  password::pbkdf2_hasher::go_parity::an_embedded_nul_is_part_of_the_password
#    2.0s  password::pbkdf2_hasher::go_parity::a_different_password_does_not_reproduce_go_s_hash
#    1.9s  password::go_parity::the_latest_hasher_is_pbkdf2_not_bcrypt
#   <0.3s  the other 35, together
#
# There is no cost-factor sweep to delete. These are bcrypt and PBKDF2 verifications over a
# ≥30-row Go-generated corpus at Go's real 600,000 iterations, and a reduced work factor would
# prove nothing because the iteration count is part of what is being matched.
#
# # What is ignored, and what deliberately is not
#
# Only the **four** heaviest — the bulk-corpus sweeps — carry `#[ignore]`. The 4.0s and 2.0s
# PBKDF2 tests stay in the default run on purpose: `an_embedded_nul_is_part_of_the_password`
# asserts `format_hash(...)` equals a Go-generated hash byte-for-byte, so "PBKDF2 in this tree
# reproduces Go's bytes at Go's iteration count" is still a claim the default
# `cargo test --workspace` makes. What moved behind this script is the *volume* of that claim —
# thirty-odd corpus rows instead of two — not its kind. Every bcrypt assertion also still runs
# by default.
#
# So D-217's worry, that 110s becomes 55s "with the coverage quietly dropped", is answered by
# this file existing and being committed rather than by the coverage having survived untouched.
# Run it when you touch anything under `crates/mm-app/src/password/`, and in any session that
# has the slack to spare.
set -e
cd "$(dirname "$0")/.."
echo "running the #[ignore]d password parity sweeps (expect ~60s)…"
exec cargo test -p mm-app --lib -- --ignored password
