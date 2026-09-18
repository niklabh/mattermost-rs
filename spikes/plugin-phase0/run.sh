#!/usr/bin/env bash
# Phase 0 of docs/PLUGIN_PLAN.md: run all three spikes against real Go peers.
#
#   spikes/plugin-phase0/run.sh          full run (includes a 35 s yamux idle per direction)
#   QUICK=1 spikes/plugin-phase0/run.sh  skip the idle
#
# Work files go in target/p0 and sockets are addressed relatively: a unix socket path must fit
# in 108 bytes, which an absolute path under a deep checkout does not.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="$HERE/target/p0"
mkdir -p "$WORK/gob"
IDLE=35; [[ -n "${QUICK:-}" ]] && IDLE=0

echo "== build"
(cd "$ROOT/reference/dump" && for p in hello gobgen yamuxpeer; do go build -o "$WORK/$p" "./spike/$p"; done)
cargo build -q --release --manifest-path "$HERE/Cargo.toml"
BIN="$HERE/target/release"

echo "== spike 2: gob"
"$WORK/gobgen" "$WORK/gob"
"$BIN/gobdump" "$WORK/gob" | grep -v "^  type "

echo "== spike 1: yamux interop"
cd "$WORK"
rm -f a.sock b.sock
IDLE=$IDLE timeout 150 "$BIN/yamux_interop" server a.sock > a-server.log 2>&1 &
sleep 0.5
IDLE=$IDLE timeout 140 ./yamuxpeer client a.sock
wait
cat a-server.log
IDLE=$IDLE timeout 150 ./yamuxpeer server b.sock > b-server.log 2>&1 &
sleep 0.5
IDLE=$IDLE timeout 140 "$BIN/yamux_interop" client b.sock
wait
cat b-server.log

echo "== spike 3: launch a real plugin"
timeout 30 "$BIN/launch" "$WORK/hello"
echo "== PHASE 0 OK"
