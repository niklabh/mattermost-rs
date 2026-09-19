#!/bin/bash
# Measure what share of a real browser session mm-api answers itself.
#
#   scripts/demo-traffic.sh            the scripted session on the current stack, then the report
#   scripts/demo-traffic.sh report     the report again, over the whole current mm-api log
#
# Needs the stack running (Postgres, `scripts/go-server.sh`, mm-api) and the snap geckodriver.
# mm-api must log `mm_api::traffic` lines (`MM_API_TRAFFIC_LOG=1`); if the running one does not,
# it is restarted with the flag, on the address it was already bound to. The demo accounts are
# seeded first (`scripts/demo-seed.py`). The report counts every request mm-api logs while the
# session runs, so run it on an otherwise idle stack.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
source "$ROOT/scripts/stack-env.sh"
LOG="/tmp/mmrs-mm-api$MMRS_STACK_SUFFIX.log"
BASE="http://127.0.0.1:$MMRS_API_PORT"

if [ "${1:-}" = "report" ]; then
  exec python3 "$ROOT/scripts/demo_traffic.py" report "$LOG"
fi

[ -x /snap/bin/geckodriver ] || { echo "needs /snap/bin/geckodriver (snap Firefox)"; exit 1; }
curl -sf -o /dev/null "$MMRS_GO_BASE/api/v4/system/ping" \
  || { echo "the Go server on $MMRS_GO_BASE is not answering: start the stack first"; exit 1; }

pid=$(mmrs_listener_pids "$MMRS_API_PORT" | head -1)
if [ -n "$pid" ] && [ "$(mmrs_listener_env "$MMRS_API_PORT" MM_API_TRAFFIC_LOG)" = 1 ]; then
  echo "mm-api (pid $pid) is already logging traffic"
else
  host=$(mmrs_listener_addr "$MMRS_API_PORT" | sed 's/:[0-9]*$//')
  echo "restarting mm-api with MM_API_TRAFFIC_LOG=1 on ${host:-127.0.0.1}"
  MMRS_API_HOST="${host:-127.0.0.1}" MM_API_TRAFFIC_LOG=1 zsh "$ROOT/scripts/mm-api.sh" start
fi

echo "seeding the demo accounts"
python3 "$ROOT/scripts/demo-seed.py" "$BASE"
exec python3 "$ROOT/scripts/demo_traffic.py" run "$LOG" "$BASE"
