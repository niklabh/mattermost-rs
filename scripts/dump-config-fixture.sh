#!/usr/bin/env bash
#
# Regenerate fixtures/config_active.json from the running Go server's own configuration.
#
# The oracle for `mm_app::config::Config::from_document` is the document the **Go server** writes
# into `Configurations.Value` — not `reference/dump`, which is where every other fixture in this
# repo comes from. That is deliberate: the config document is produced by `SetDefaults()` running
# inside a real server boot, which is the thing we are trying to agree with, and no amount of
# marshalling a hand-built `model.Config` would prove the same fact.
#
# WHY THIS IS A PROJECTION AND NOT THE WHOLE ROW
#
# The row is 27 KB across 47 sections and carries real secrets: `FileSettings.PublicLinkSalt` is
# generated at first boot, and `SqlSettings.DataSource` holds the database password. So this dumps
# only the keys `mm_app::config` actually models. The projection is mechanical — the key list
# below is the same list the Rust `Document` struct declares — which keeps the fixture honest: it
# is Go's own bytes for exactly the fields under test, with nothing transcribed by hand.
#
# KEEP THE KEY LIST IN SYNC. Adding a field to `mm_app::config::Config` means adding its key here
# and re-running this, or the fixture silently stops covering the new field while still passing.
#
# **It had drifted, and the test that was supposed to catch that did not.** The list held six
# sections and seventeen keys while the Rust `Document` had grown to fourteen sections and
# thirty-eight; the count assertion in `the_fixture_covers_every_document_sourced_setting` was
# comparing the fixture against a hardcoded 17 rather than against the struct, so it agreed with
# the stale list. The list below is now the struct's own keys, extracted from it rather than
# maintained beside it, and the count in that test is the number this script writes.
#
#   ./scripts/dump-config-fixture.sh
#
set -euo pipefail

cd "$(dirname "$0")/.."

: "${PGCONTAINER:=mmrs-postgres}"
: "${PGUSER:=mmuser}"
: "${PGDATABASE:=mattermost}"

if ! docker exec "$PGCONTAINER" true 2>/dev/null; then
	echo "error: container '$PGCONTAINER' is not running — try 'docker compose up -d'" >&2
	exit 1
fi

raw=$(docker exec "$PGCONTAINER" psql -U "$PGUSER" -d "$PGDATABASE" -tAc \
	"SELECT value FROM configurations WHERE active")

if [ -z "$raw" ]; then
	echo "error: no active row in Configurations." >&2
	echo "       Is MM_CONFIG pointed at this database? See docker-compose.yml." >&2
	exit 1
fi

printf '%s' "$raw" | python3 -c '
import json, sys

# Exactly the keys mm_app::config::Config models, section by section.
MODELLED = {
    "ServiceSettings": [
        "EnablePostIconOverride", "EnableCustomEmoji", "PostPriority", "AllowSyncedDrafts",
        "EnableBurnOnRead", "EnableIncomingWebhooks", "EnableOutgoingWebhooks",
        "EnableOAuthServiceProvider", "SessionIdleTimeoutInMinutes",
        "ExtendSessionLengthWithActivity", "GoroutineHealthThreshold", "EnableTesting",
        "ScheduledPosts", "EnableUserStatuses", "EnableDynamicClientRegistration",
        "EnableOutgoingOAuthConnections", "EnablePostUsernameOverride",
        "AllowPersistentNotifications", "UniqueEmojiReactionLimitPerPost",
        # Not a setting Config carries — the `isUpdate` discriminator. `Config.isUpdate` is
        # `ServiceSettings.SiteURL != nil` (config.go:4289) and two defaults are `!isUpdate`, so
        # the fixture has to record that a real document *has* the key. The value is "" here and
        # is never read; it is projected so the presence is measured rather than asserted.
        "SiteURL",
    ],
    "ComplianceSettings": ["Enable"],
    "ExperimentalSettings": ["RestrictSystemAdmin"],
    "ImageProxySettings": ["Enable"],
    "FileSettings": ["DriverName"],
    "PrivacySettings": ["ShowFullName", "ShowEmailAddress"],
    "ClientRequirements": [
        "AndroidLatestVersion", "AndroidMinVersion", "IosLatestVersion", "IosMinVersion",
    ],
    "SqlSettings": ["DisableDatabaseSearch"],
    "ElasticsearchSettings": ["EnableSearching"],
    "AIRecapSettings": ["Enable"],
    "TeamSettings": [
        "RestrictDirectMessage", "RestrictCreationToDomains", "UserStatusAwayTimeout",
        "EnableCustomUserStatuses",
    ],
    "EmailSettings": ["RequireEmailVerification"],
    "GuestAccountsSettings": ["RestrictCreationToDomains"],
    "MessageExportSettings": ["DownloadExportResults"],
}

full = json.load(sys.stdin)

missing = []
out = {}
for section, keys in MODELLED.items():
    if section not in full:
        missing.append(section)
        continue
    out[section] = {}
    for key in keys:
        if key not in full[section]:
            missing.append(f"{section}.{key}")
            continue
        out[section][key] = full[section][key]

if missing:
    sys.stderr.write("error: absent from the live document: " + ", ".join(missing) + "\n")
    sys.stderr.write("       A modelled key the server does not write is a wrong key name.\n")
    raise SystemExit(1)

# FeatureFlags is asserted ABSENT rather than projected: Go clears the section before persisting
# when readOnlyFF is set, which is the default (config/store.go:306-310). If it ever shows up the
# fixture should start covering it, and the test that reads this file should start failing first.
if "FeatureFlags" in full:
    sys.stderr.write("error: the live document now carries FeatureFlags.\n")
    sys.stderr.write("       config.rs documents that section as unpersistable. Recheck store.go.\n")
    raise SystemExit(1)

json.dump(out, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
' >fixtures/config_active.json

echo "wrote fixtures/config_active.json ($(wc -c <fixtures/config_active.json) bytes)"
