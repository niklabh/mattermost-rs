#!/usr/bin/env bash
# Everything the publishable, mm-*-free crates must pass before `cargo publish`. Two sets:
#
#   plugin  the Go-interop crates: gobwire-derive, gobwire, go-netrpc, goplugin
#   mail    the ports behind Mattermost's e-mail: gohtml, gotemplate, gohtml2text, gomail,
#           gogoldmark
#
#   scripts/crates-preflight.sh              both sets: boundary, lint, docs, tests, package
#                                            contents, dry run, semver (once on crates.io)
#   scripts/crates-preflight.sh --set mail   one set only (plugin | mail | all)
#   scripts/crates-preflight.sh --msrv       also build each crate with its declared rust-version
#   scripts/crates-preflight.sh --fuzz 60    also fuzz each gobwire target for 60 seconds (nightly)
#   scripts/crates-preflight.sh --no-go      skip the plugin interop tests that build Go oracles
#                                            (the mail crates' oracles are committed fixtures)
#
# Publishing itself stays a manual step, in dependency order and in one command per set:
#
#   cargo publish -p gobwire-derive -p gobwire -p go-netrpc -p goplugin
#   cargo publish -p gohtml -p gotemplate -p gohtml2text -p gomail -p gogoldmark
set -euo pipefail

cd "$(dirname "$0")/.."

PLUGIN_CRATES=(gobwire-derive gobwire go-netrpc goplugin)
MAIL_CRATES=(gohtml gotemplate gohtml2text gomail gogoldmark)

SET=all
MSRV=0
FUZZ_SECS=0
GO=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --set) SET="$2"; shift ;;
        --msrv) MSRV=1 ;;
        --fuzz) FUZZ_SECS="$2"; shift ;;
        --no-go) GO=0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

case "$SET" in
    plugin) CRATES=("${PLUGIN_CRATES[@]}") ;;
    mail) CRATES=("${MAIL_CRATES[@]}") ;;
    all) CRATES=("${PLUGIN_CRATES[@]}" "${MAIL_CRATES[@]}") ;;
    *) echo "unknown set: $SET (plugin | mail | all)" >&2; exit 2 ;;
esac
PKG_ARGS=()
for c in "${CRATES[@]}"; do PKG_ARGS+=(-p "$c"); done
PLUGIN=0
[[ $SET != mail ]] && PLUGIN=1

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

# All features: a superset of what docs.rs builds (gobwire asks for all features; the others,
# gomail's `testing` apparatus among them, get their defaults).
step "docs, as docs.rs builds them"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features "${PKG_ARGS[@]}"

step "tests (README examples included, as doctests)"
if [[ $PLUGIN == 1 && $GO == 0 ]]; then
    PLUGIN_ARGS=()
    for c in "${PLUGIN_CRATES[@]}"; do PLUGIN_ARGS+=(-p "$c"); done
    cargo test "${PLUGIN_ARGS[@]}" --all-features --lib --examples
    cargo test "${PLUGIN_ARGS[@]}" --all-features --doc
    cargo test -p gobwire --all-features --test derive
    if [[ $SET == all ]]; then
        MAIL_ARGS=()
        for c in "${MAIL_CRATES[@]}"; do MAIL_ARGS+=(-p "$c"); done
        cargo test "${MAIL_ARGS[@]}" --all-features
    fi
else
    if [[ $PLUGIN == 1 ]]; then
        command -v go >/dev/null || fail "the interop tests build Go oracles; install Go or pass --no-go"
    fi
    cargo test "${PKG_ARGS[@]}" --all-features
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
# A proc-macro crate has no library API for cargo-semver-checks to compare, and it refuses one.
no_lib=$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if not any("lib" in t["kind"] for t in p["targets"]):
        print(p["name"])
')
for c in "${CRATES[@]}"; do
    if grep -qx "$c" <<<"$no_lib"; then
        echo "$c: no library target; nothing to semver-check"
    elif ! curl -sfo /dev/null -A "mattermost-rs crates-preflight" "https://crates.io/api/v1/crates/$c"; then
        echo "$c: not on crates.io yet; nothing to compare against"
    elif ! cargo semver-checks --version >/dev/null 2>&1; then
        fail "$c is published; install cargo-semver-checks to compare against it"
    else
        cargo semver-checks -p "$c" --all-features
    fi
done

if [[ $MSRV == 1 ]]; then
    # Each crate against its own declared rust-version, grouped by version: "1.88 gobwire ...".
    groups=$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
want = set(sys.argv[1:])
by = {}
for p in json.load(sys.stdin)["packages"]:
    if p["name"] in want:
        by.setdefault(p["rust_version"], []).append(p["name"])
for v, names in sorted(by.items()):
    print(v, *names)
' "${CRATES[@]}")
    # A user on that toolchain resolves dependencies afresh, preferring versions that support it
    # (resolver 3). Reproduce that with a fresh lockfile, and put the workspace's back after.
    saved=$(mktemp)
    cp Cargo.lock "$saved"
    trap 'cp "$saved" Cargo.lock; rm -f "$saved"' EXIT
    while read -r msrv names; do
        step "MSRV: build $names with Rust $msrv"
        args=()
        for c in $names; do args+=(-p "$c"); done
        CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo "+$msrv" generate-lockfile
        cargo "+$msrv" check "${args[@]}" --all-features --lib --examples
    done <<<"$groups"
fi

if [[ $FUZZ_SECS != 0 && $PLUGIN == 1 ]]; then
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
