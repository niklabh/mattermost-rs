#!/usr/bin/env bash
# Everything the four generic Go-interop crates must pass before `cargo publish`.
#
#   scripts/crates-preflight.sh            boundary, lint, docs, tests, package contents, dry run,
#                                          semver (once a version is on crates.io)
#   scripts/crates-preflight.sh --msrv     also build them with their declared rust-version
#   scripts/crates-preflight.sh --fuzz 60  also fuzz each gobwire target for 60 seconds (nightly)
#   scripts/crates-preflight.sh --no-go    skip the interop tests that build the Go oracles
#
# Publishing itself stays a manual step, in dependency order and in one command:
#
#   cargo publish -p gobwire-derive -p gobwire -p go-netrpc -p goplugin
set -euo pipefail

cd "$(dirname "$0")/.."

CRATES=(gobwire-derive gobwire go-netrpc goplugin)
PKG_ARGS=()
for c in "${CRATES[@]}"; do PKG_ARGS+=(-p "$c"); done

MSRV=0
FUZZ_SECS=0
GO=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --msrv) MSRV=1 ;;
        --fuzz) FUZZ_SECS="$2"; shift ;;
        --no-go) GO=0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step "boundary: no mm-* crate in any dependency tree"
for c in "${CRATES[@]}"; do
    if cargo tree -p "$c" -e normal,build --prefix none | grep -E '^mm-' ; then
        fail "$c depends on a Mattermost crate"
    fi
done

step "fmt and clippy"
cargo fmt --check "${PKG_ARGS[@]}"
cargo clippy "${PKG_ARGS[@]}" --all-targets --all-features -- -D warnings

step "docs, as docs.rs builds them"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features "${PKG_ARGS[@]}"

step "tests"
if [[ $GO == 1 ]]; then
    command -v go >/dev/null || fail "the interop tests build Go oracles; install Go or pass --no-go"
    cargo test "${PKG_ARGS[@]}" --all-features
else
    cargo test "${PKG_ARGS[@]}" --all-features --lib --examples
    cargo test "${PKG_ARGS[@]}" --all-features --doc
    cargo test -p gobwire --all-features --test derive
fi

step "package contents: sources, README, CHANGELOG, licences, examples; no tests"
for c in "${CRATES[@]}"; do
    files=$(cargo package -p "$c" --list --allow-dirty 2>/dev/null)
    for want in README.md CHANGELOG.md src/lib.rs; do
        grep -qx "$want" <<<"$files" || fail "$c: $want missing from the package"
    done
    grep -q '^LICENSE' <<<"$files" || fail "$c: no licence file in the package"
    if grep -E '^(tests|fuzz)/' <<<"$files"; then
        fail "$c: repository-only files would ship"
    fi
done

step "publish dry run (packages, then builds each from its tarball)"
cargo publish --dry-run --allow-dirty "${PKG_ARGS[@]}"

step "semver against the latest release"
for c in "${CRATES[@]}"; do
    if ! curl -sfo /dev/null -A "mattermost-rs crates-preflight" "https://crates.io/api/v1/crates/$c"; then
        echo "$c: not on crates.io yet; nothing to compare against"
    elif ! cargo semver-checks --version >/dev/null 2>&1; then
        fail "$c is published; install cargo-semver-checks to compare against it"
    else
        cargo semver-checks -p "$c" --all-features
    fi
done

if [[ $MSRV == 1 ]]; then
    msrv=$(cargo metadata --no-deps --format-version 1 |
        python3 -c 'import json,sys; print({p["name"]: p["rust_version"] for p in json.load(sys.stdin)["packages"]}["gobwire"])')
    step "MSRV: build with Rust $msrv"
    # A user on that toolchain resolves dependencies afresh, preferring versions that support it
    # (resolver 3). Reproduce that with a fresh lockfile, and put the workspace's back after.
    saved=$(mktemp)
    cp Cargo.lock "$saved"
    trap 'cp "$saved" Cargo.lock; rm -f "$saved"' EXIT
    CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo "+$msrv" generate-lockfile
    cargo "+$msrv" check "${PKG_ARGS[@]}" --all-features --lib --examples
fi

if [[ $FUZZ_SECS != 0 ]]; then
    step "fuzz gobwire, ${FUZZ_SECS}s per target, seeded with the Go-written corpus"
    cd crates/gobwire/fuzz
    # cargo-fuzz defaults to the triple it was built for. A prebuilt binary (CI installs one) is
    # musl, whose static libc the sanitizer refuses, so name the toolchain's own host.
    host=$(rustc +nightly -vV | sed -n 's/^host: //p')
    for t in decode_dynamic decode_typed reencode; do
        mkdir -p "corpus/$t"
        # A capped allocation is a finding, not an OOM for the whole machine.
        cargo +nightly fuzz run --target "$host" "$t" "corpus/$t" ../../../fixtures/gob -- \
            -max_total_time="$FUZZ_SECS" -rss_limit_mb=2048 -malloc_limit_mb=1024 -timeout=10
    done
fi

step "all checks passed"
