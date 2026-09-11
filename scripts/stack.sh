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

login_token() {
  curl -si -X POST "$1/api/v4/users/login" -H 'Content-Type: application/json' \
    -d '{"login_id":"slice@example.com","password":"Slice-Test-1234"}' \
    | grep -i '^token:' | tr -d '\r' | awk '{print $2}'
}

# Idempotent throughout, so `up` can always call it: the user/team half is skipped when the login
# already works, and the deployment-shape half is re-checked every time — it is cheap and a stack
# that lost it is a stack whose suites quietly stop asserting anything.
seed_stack() {
  local base="$1" token user team
  if token=$(login_token "$base") && [ -n "$token" ]; then
    echo "  fixture user already present"
    seed_deployment_shape "$base" "$token"
    return 0
  fi

  echo "  seeding the fixture user, team and channel…"
  # The first user created becomes the system admin, which every suite's `go_minted_token`
  # depends on. Nothing hardcodes an id — the tests discover what they need.
  curl -sf -o /dev/null -X POST "$base/api/v4/users" -H 'Content-Type: application/json' \
    -d '{"email":"slice@example.com","username":"sliceuser","password":"Slice-Test-1234"}' \
    || { echo "  could not create the fixture user"; return 1; }

  token=$(login_token "$base")
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
  seed_deployment_shape "$base" "$token"
}

# The shapes several parity suites assert *about the deployment* rather than about a fixture they
# built themselves. Both were ambient accidents on the original stack and absent on a fresh one,
# which is how a suite that passes for months starts proving nothing the day someone recreates the
# volume:
#
# * a bot **with** a description, so `bots`'s `description,omitempty` claim has both sides —
#   `system-bot` supplies the side that omits it;
# * a `Jobs` row whose `data` column is SQL NULL, which in production is written by the
#   product-notices worker and here by nobody.
#
# Deliberately **not** planted by the tests: `bots.rs` has other tests that plant and unplant
# `mmrsbot%` rows, so a list test doing the same deletes theirs mid-run. This is stack furniture,
# created once, swept by no purge.
seed_deployment_shape() {
  local base="$1" token="$2"
  local owner
  owner=$(curl -s "$base/api/v4/users/me" -H "Authorization: Bearer $token" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

  # **Written straight to the tables, not through `POST /bots`.** Bot creation is refused on this
  # deployment — `ServiceSettings.EnableBotAccountCreation` defaults false and turning it on would
  # change what other routes answer — so the two rows go in the way `common::plant_bot` writes
  # them, with an id no purge prefix matches.
  docker exec -i "mmrs-postgres$MMRS_STACK_SUFFIX" psql -q -U mmuser -d mattermost >/dev/null <<SQL
INSERT INTO users
  (id, createat, updateat, deleteat, username, password, authdata, authservice, email,
   emailverified, nickname, firstname, lastname, position, roles, allowmarketing, props,
   notifyprops, lastpasswordupdate, lastpictureupdate, failedattempts, locale, timezone,
   mfaactive, mfasecret, remoteid, lastlogin, mfausedtimestamps)
VALUES ('seedbotdescribed0000000000', 1788600000000, 1788600000000, 0, 'seed-bot', '', NULL, '',
        'seed-bot@mmrs.invalid', false, '', 'Seed Bot', '', '', 'system_user', false,
        '{}'::jsonb, '{}'::jsonb, 1788600000000, 0, 0, 'en', '{}'::jsonb, false, '', NULL, 0,
        'null'::jsonb)
ON CONFLICT (id) DO NOTHING;

INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat, lasticonupdate)
VALUES ('seedbotdescribed0000000000',
        'stack furniture: the described side of description,omitempty', '$owner',
        1788600000000, 1788600000000, 0, 0)
ON CONFLICT (userid) DO UPDATE SET description = EXCLUDED.description, ownerid = EXCLUDED.ownerid;

-- Both null-ish shapes, because Go renders them differently and only one occurs naturally:
-- a SQL NULL comes back as an empty object and a literal JSON null as null. See
-- mm_store::job_store::JobRow::into_job. Every null-ish row a real deployment accumulates is the
-- second kind, written by the product-notices worker; the first is here so the divergence that
-- hid behind that fact stays tested. (No backticks: this heredoc expands $owner.)
INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, progress, data)
VALUES ('seedjobsqlnull000000000000', 'product_notices', 0, 1788600000000, 1788600000000,
        1788600000000, 'success', 0, NULL)
ON CONFLICT (id) DO UPDATE SET data = NULL;

INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, progress, data)
VALUES ('seedjobjsonnull00000000000', 'product_notices', 0, 1788600000000, 1788600000000,
        1788600000000, 'success', 0, 'null'::jsonb)
ON CONFLICT (id) DO UPDATE SET data = 'null'::jsonb;
SQL
  echo "  seeded: a described bot and both null-ish job shapes"
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
