#!/bin/zsh
# A **fourth** pinned Go server, on a spare port, that is **licensed** — Enterprise SKU, every
# feature flag on — so the licensed half of a route has a Go answer to be compared against.
#
#   scripts/go-licensed.sh start     build if needed, then start it, waiting for /system/ping
#   scripts/go-licensed.sh stop      stop it
#   scripts/go-licensed.sh rebuild   force a rebuild of the binary, then start
#   scripts/go-licensed.sh port      print the port it uses on this stack
#   scripts/go-licensed.sh files     print the directory holding the key pair and the licence
#
# # Why the stack's own Go server cannot be licensed
#
# `scripts/go-server.sh` builds `cmd/mattermost` with no `-ldflags`, so `model.BuildEnterpriseReady`
# is empty and `LoadLicense` is never called (platform/service.go:371). Planting
# `Systems.ActiveLicenseId`, dropping a file into `config/`, setting `MM_LICENSE` — none of them
# moves that server, which is what every "no oracle beside it" note in `docs/TECH_DEBT.md` was
# describing. And a licence it *would* load must verify against a Mattermost public key whose
# private half is not in the tree.
#
# So this runs `reference/licensed/main.go`: the same server, built with `BuildEnterpriseReady=true`,
# whose validator trusts the public key named by `MMRS_LICENSE_PUBLIC_KEY_FILE`. The key pair is
# generated once per machine into `reference/.build/license/`, the licence below is signed with it
# the way Mattermost signs theirs (SHA-512, PKCS#1 v1.5, signature appended, base64), and the
# result goes to this process as `MM_LICENSE`. `mm-api` reads the same two variables and trusts
# the same key, so a licensed mm-api (`common::licensed_rust` in the parity harness) verifies the
# very bytes this server loaded.
#
# # `MM_LICENSE`, never the database
#
# `LoadLicense` takes the environment variable **without writing anything** — the file-on-disk
# path is the one that calls `SaveLicense`, and `SaveLicense` is what writes `Licenses` and
# `Systems.ActiveLicenseId`. Those tables are shared with the stack's ordinary server and its
# mm-api, and a row there would make *that* mm-api believe the installation is licensed while
# its Go stays Team Edition. The environment keeps the licence private to this pair.
#
# # One licence, and which one
#
# A process loads one licence, so this is one oracle, not one per SKU. `enterprise` (tier 20)
# opens `requireLicense`, `MinimumProfessionalLicense` and `MinimumEnterpriseLicense`, and leaves
# `MinimumEnterpriseAdvancedLicense` closed — which is also the line past which the open-source
# tree has no code to run (the access-control service, ABAC). The tier ladder itself is a pure
# function with its own corpus in `fixtures/behaviour_license.json`; this process is for what
# sits behind the gates, not for the gates' arithmetic.
#
# `cloud` is the one feature left off: `IsCloud()` reroutes dozens of handlers to a billing
# service that does not exist here. Everything else is on, so a route gated on a feature flag
# reaches its licensed branch.
set -e
cd "$(dirname "$0")/.."
ROOT=$(pwd)
SRC="$ROOT/reference/mattermost/server"
BUILD="$ROOT/reference/.build"
LIC="$BUILD/license"
source "$ROOT/scripts/stack-env.sh"
# +32, one above the discoverable oracle's +31, still below the next stack's block.
PORT=$((MMRS_GO_PORT + 32))
RUN="$BUILD/mmlic$MMRS_RUN_SUFFIX"
LOG="$BUILD/licensed$MMRS_STACK_SUFFIX.log"
DSN="postgres://mmuser:mmuser_password@localhost:$MMRS_PG_PORT/mattermost?sslmode=disable&connect_timeout=10"
BIN="$BUILD/mattermost-licensed"

build() {
  mkdir -p "$BUILD"
  # The same workspace trick as `go-server.sh`, with the wrapper module added, so the server's
  # own go.mod and go.sum resolve every dependency and the wrapper needs none of its own.
  cat > "$BUILD/go-licensed.work" <<WORK
go 1.26.4

use $SRC
use $SRC/public
use $ROOT/reference/licensed
WORK
  echo "building the licensed Go server (a few minutes on a warm cache)…"
  (cd "$ROOT/reference/licensed" && GOWORK="$BUILD/go-licensed.work" GOFLAGS=-buildvcs=false \
    go build -ldflags "-X github.com/mattermost/mattermost/server/public/model.BuildEnterpriseReady=true" \
    -o "$BIN" .)
}

# The key pair and the signed licence. Generated once per machine; `license.json` is rewritten on
# every start so an edit here reaches the oracle, and re-signed because the bytes changed.
license_files() {
  mkdir -p "$LIC"
  if [ ! -f "$LIC/private.pem" ]; then
    openssl genrsa -out "$LIC/private.pem" 2048 2>/dev/null
    openssl rsa -in "$LIC/private.pem" -pubout -out "$LIC/public.pem" 2>/dev/null
  fi
  # Timestamps: issued and started 2026-01-01T00:00:00Z, expiring 2100-01-01T00:00:00Z. The span
  # must not equal either trial duration to the millisecond (`IsTrialLicense`, license.go), and
  # must be far enough out that no expiry check trips while the stack lives. `is_trial` false
  # for the same reason: a trial through `MM_LICENSE` asks the licence manager, which the
  # open-source tree does not have, whether a trial may start — and dereferences nil to ask.
  cat > "$LIC/license.json" <<'JSON'
{"id":"mmrslicensedoracle00000001","issued_at":1767225600000,"starts_at":1767225600000,"expires_at":4102444800000,"customer":{"id":"mmrslicensecustomer0000001","name":"mattermost-rs parity oracle","email":"oracle@mmrs.invalid","company":"mattermost-rs"},"features":{"users":100000,"ldap":true,"ldap_groups":true,"mfa":true,"google_oauth":true,"office365_oauth":true,"openid":true,"compliance":true,"cluster":true,"metrics":true,"mhpns":true,"saml":true,"elastic_search":true,"announcement":true,"theme_management":true,"email_notification_contents":true,"data_retention":true,"message_export":true,"custom_permissions_schemes":true,"custom_terms_of_service":true,"guest_accounts":true,"guest_accounts_permissions":true,"id_loaded":true,"lock_teammate_name_display":true,"enterprise_plugins":true,"advanced_logging":true,"cloud":false,"shared_channels":true,"remote_cluster_service":true,"outgoing_oauth_connections":true,"auto_translation":true,"future_features":true},"sku_name":"Enterprise","sku_short_name":"enterprise","is_trial":false,"is_gov_sku":false,"is_non_production":false,"is_seat_count_enforced":false}
JSON
  # Mattermost's format: plaintext, then a 256-byte PKCS#1 v1.5 signature over its SHA-512, the
  # whole thing base64 (utils/license.go:71). A 2048-bit key is what makes the signature 256 bytes.
  openssl dgst -sha512 -sign "$LIC/private.pem" -out "$LIC/license.sig" "$LIC/license.json"
  cat "$LIC/license.json" "$LIC/license.sig" | base64 -w0 > "$LIC/license.signed"
}

case "${1:-start}" in
  port) echo "$PORT" ;;
  files) echo "$LIC" ;;
  stop)
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    # The licensed mm-api the parity harness starts beside it (`common::licensed`, :8090 + the
    # stack offset) is a static in the test binary and outlives the run; it is the pair's other
    # half, so it goes with this.
    mmrs_free_port "$((MMRS_API_PORT + 24))"
    echo "stopped"
    ;;
  rebuild)
    build; exec "$0" start
    ;;
  start)
    [ -d "$SRC" ] || { echo "reference/mattermost is not cloned — see MIGRATION.md for the pinned SHA"; exit 2; }
    [ -x "$BIN" ] || build
    license_files
    mkdir -p "$RUN/bin" "$RUN/data" "$RUN/plugins" "$RUN/client/plugins" "$RUN/config" "$RUN/logs"
    cp -f "$BIN" "$RUN/bin/mattermost"
    for dir in i18n templates fonts; do
      [ -e "$RUN/$dir" ] || ln -s "$SRC/$dir" "$RUN/$dir"
    done
    export MM_CONFIG="$DSN"
    export MM_SQLSETTINGS_DRIVERNAME=postgres
    export MM_SQLSETTINGS_DATASOURCE="$DSN"
    export MM_SERVICESETTINGS_SITEURL="http://localhost:$PORT"
    export MM_SERVICESETTINGS_LISTENADDRESS=":$PORT"
    export MM_TEAMSETTINGS_ENABLEOPENSERVER=true
    export MM_SERVICESETTINGS_ENABLELOCALMODE=false
    # The main server's data directory, deliberately: a file id created there must resolve here.
    export MM_FILESETTINGS_DIRECTORY="$BUILD/mmroot$MMRS_RUN_SUFFIX/data/"
    export MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD=true
    # The two differences from `go-server.sh`, and the entire point of this script.
    export MM_LICENSE="$(cat "$LIC/license.signed")"
    export MMRS_LICENSE_PUBLIC_KEY_FILE="$LIC/public.pem"
    mmrs_free_port "$PORT"
    pkill -f "$RUN/bin/mattermost" 2>/dev/null || true
    sleep 1
    (cd "$RUN" && nohup "$RUN/bin/mattermost" server > "$LOG" 2>&1 &)
    for _ in $(seq 90); do
      curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" && break
      sleep 1
    done
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/api/v4/system/ping" \
      || { echo "the licensed oracle never came up — see $LOG"; exit 1; }
    # Say so in words: a licensed oracle that loaded no licence is the silent failure this
    # script exists to remove.
    if curl -sf "http://127.0.0.1:$PORT/api/v4/license/client?format=old" | grep -q '"IsLicensed":"true"'; then
      echo "the licensed Go oracle is listening on :$PORT (log: $LOG)"
    else
      echo "the licensed oracle is up on :$PORT but reports IsLicensed=false — see $LOG"; exit 1
    fi
    ;;
  *)
    sed -n '2,9p' "$0"; exit 2
    ;;
esac
