#!/bin/zsh
# Bring up, seed and tear down a numbered development stack.
#
#   scripts/stack.sh up 1            postgres + the pinned Go server, seeded, on stack 1
#   scripts/stack.sh up 1 2 3        three of them
#   scripts/stack.sh down 1          stop the Go server and remove the containers and volume
#   scripts/stack.sh status          what is running, per stack
#   scripts/stack.sh seed 1          (re)create the fixture user, team and channel
#   eval "$(scripts/stack.sh env 1)" export a shell onto stack 1
#
# # What a stack is
#
#   stack k   postgres  5432 + k     go  8065 + 100k     mm-api  8066 + 100k
#
# One Postgres, one Go server built from the pinned SHA, one mm-api — sharing nothing with any
# other stack. **Stack 0 is the historical layout**, container names and volume included, so an
# existing checkout keeps working untouched.
#
# # Why
#
# `stack-lock.sh` used to serialise every stack-backed run on the machine, which made a
# twenty-minute mutation batch block every other worktree. The lock is now per stack, so N
# worktrees on N stacks genuinely run at once. The Go **binary** is shared (built once into
# `reference/.build/mattermost`); only the run directory, database and ports are per stack.
#
# # The fixture state
#
# A fresh volume has no users and no teams, and every parity suite needs both. `seed` is the
# README's four calls, made idempotent: it does nothing when the login already works, so `up` can
# always call it.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)

usage() { sed -n '2,10p' "$0"; exit 2; }

seed_stack() {
  local base="$1"
  if curl -sf -o /dev/null -X POST "$base/api/v4/users/login" \
       -H 'Content-Type: application/json' \
       -d '{"login_id":"slice@example.com","password":"Slice-Test-1234"}'; then
    echo "  fixture user already present"
    return 0
  fi

  echo "  seeding the fixture user, team and channel…"
  # The first user created becomes the system admin, which every suite's `go_minted_token`
  # depends on. Nothing hardcodes an id — the tests discover what they need.
  curl -sf -o /dev/null -X POST "$base/api/v4/users" -H 'Content-Type: application/json' \
    -d '{"email":"slice@example.com","username":"sliceuser","password":"Slice-Test-1234"}' \
    || { echo "  could not create the fixture user"; return 1; }

  local token user team
  token=$(curl -si -X POST "$base/api/v4/users/login" -H 'Content-Type: application/json' \
    -d '{"login_id":"slice@example.com","password":"Slice-Test-1234"}' \
    | grep -i '^token:' | tr -d '\r' | awk '{print $2}')
  [ -n "$token" ] || { echo "  the fixture user cannot log in"; return 1; }

  user=$(curl -s "$base/api/v4/users/me" -H "Authorization: Bearer $token" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
  team=$(curl -s -X POST "$base/api/v4/teams" -H "Authorization: Bearer $token" \
    -H 'Content-Type: application/json' \
    -d '{"name":"slice-team","display_name":"Slice Team","type":"O"}' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
  curl -sf -o /dev/null -X POST "$base/api/v4/teams/$team/members" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
    -d "{\"team_id\":\"$team\",\"user_id\":\"$user\"}"
  echo "  seeded: user=$user team=$team"
}

up_stack() {
  local k="$1"
  export MMRS_STACK="$k"
  source "$ROOT/scripts/stack-env.sh"
  echo "stack $k  postgres :$MMRS_PG_PORT  go :$MMRS_GO_PORT  mm-api :$MMRS_API_PORT"

  mmrs_compose up -d postgres
  for _ in $(seq 60); do
    docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec "mmrs-postgres$MMRS_STACK_SUFFIX" pg_isready -U mmuser -d mattermost >/dev/null 2>&1 \
    || { echo "  postgres never became ready"; return 1; }
  echo "  postgres ready"

  # The Go server owns the schema: its first boot on a fresh volume runs every migration.
  MMRS_STACK="$k" "$ROOT/scripts/go-server.sh" start >/dev/null
  echo "  go server up"
  seed_stack "$MMRS_GO_BASE"
  echo "  eval \"\$(scripts/stack.sh env $k)\" to point a shell at it"
}

down_stack() {
  local k="$1"
  export MMRS_STACK="$k"
  source "$ROOT/scripts/stack-env.sh"
  MMRS_STACK="$k" "$ROOT/scripts/go-server.sh" stop >/dev/null 2>&1 || true
  pkill -f "MM_API_LISTEN=127.0.0.1:$MMRS_API_PORT" 2>/dev/null || true
  mmrs_compose down -v
  rm -rf "$ROOT/reference/.build/mmroot$MMRS_RUN_SUFFIX"
  echo "stack $k down"
}

status_all() {
  printf '%-6s %-10s %-24s %-24s\n' stack postgres go mm-api
  local k pg go api
  for k in $(seq 0 9); do
    MMRS_STACK=$k source "$ROOT/scripts/stack-env.sh"
    docker inspect -f '{{.State.Running}}' "mmrs-postgres$MMRS_STACK_SUFFIX" >/dev/null 2>&1 \
      && pg=up || pg=-
    curl -sf -o /dev/null "$MMRS_GO_BASE/api/v4/system/ping" && go="$MMRS_GO_BASE" || go=-
    curl -sf -o /dev/null "$MMRS_RUST_BASE/api/v4/system/ping" && api="$MMRS_RUST_BASE" || api=-
    if [ "$pg" = "-" ] && [ "$go" = "-" ] && [ "$api" = "-" ]; then continue; fi
    printf '%-6s %-10s %-24s %-24s\n' "$k" "$pg" "$go" "$api"
  done
}

case "${1:-}" in
  up)    shift; [ $# -gt 0 ] || usage; for k in "$@"; do up_stack "$k"; done ;;
  down)  shift; [ $# -gt 0 ] || usage; for k in "$@"; do down_stack "$k"; done ;;
  seed)  shift; export MMRS_STACK="${1:-0}"; source "$ROOT/scripts/stack-env.sh"; seed_stack "$MMRS_GO_BASE" ;;
  env)   shift; export MMRS_STACK="${1:-0}"; source "$ROOT/scripts/stack-env.sh"
         for v in MMRS_STACK MMRS_PG_PORT MMRS_GO_PORT MMRS_API_PORT MMRS_PORT_OFFSET \
                  MMRS_GO_BASE MMRS_RUST_BASE DATABASE_URL MMRS_STACK_SUFFIX \
                  MMRS_COMPOSE_PROJECT MMRS_RUN_SUFFIX MMRS_LOCK; do
           eval "printf 'export %s=%s\n' $v \"\${$v}\""
         done ;;
  status) status_all ;;
  *) usage ;;
esac
