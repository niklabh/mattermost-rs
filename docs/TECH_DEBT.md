# Tech Debt Register

Deferred work and known divergences, carried across sessions. `MIGRATION.md` records **what was
translated and what Go actually does**; this file records **what we owe**.

Log an entry here whenever a session skips something, approximates something, or discovers a
divergence it does not close. An entry is cheap; a forgotten gap that surfaces in Phase 4 as a
client bug is not.

**Status:** `OPEN` (owed) · `ACCEPTED` (deliberate permanent divergence) · `CLOSED` (paid off).

**Severity:**
- `blocking` — something downstream cannot be built correctly until this is paid.
- `divergence` — Rust and Go behave differently on reachable input.
- `incomplete` — a function or type exists but does not cover everything Go does.
- `unverified` — we believe it matches Go but have not measured it.

---

## D-001 · `IsValidLocale` needs the IANA subtag registry

**Status** OPEN · **Severity** blocking · **Raised** 2026-08-13 (phase 1, after `user.go`)
**Blocks** `User::is_valid`, and any later `IsValid` that validates a locale.

`IsValidLocale` (user.go:1105) delegates to `golang.org/x/text/language.Parse`, which validates
against the **IANA subtag registry**, not merely BCP 47 syntax. Measured against Go:

| accepted | rejected |
|---|---|
| `en`, `eng`, `zh-CN`, `pt-br`, `en_US`, `root`, `und`, `qaa`, `mul`, `zxx` | `xx`, `xxx`, `engl`, `zh-Ha`, `en-1`, `i-en`, `a-b`, `C`, `POSIX` |

`xx` is syntactically perfect and still rejected — it is not a registered language. There is no
rule to write; the registry is the rule.

**Why it is tractable.** `UserLocaleMaxLength` is 5, so the reachable input space is finite and
enumerable *from Go itself*: roughly 180 two-letter codes, ~7k three-letter codes, ~250 regions,
plus `root`. Generating the exact accepted set is mechanical, not a guess.

**Options considered**
- **(a) Generate the table from Go** — full parity. Costs a ~30 KB generated Rust table plus a
  generator in `reference/dump/`. *Recommended.*
- **(b) Restrict to Mattermost's ~20 shipped locales** — small, but strictly narrower than Go, so
  it would reject with 400 an input the Go server accepts. Locale is user-settable, so this is a
  reachable behavioural difference, not a theoretical one.
- **(c) Leave unported** — current state.

**Decision** Deferred 2026-08-13 by the project owner: log and revisit. Chose (c) for now.
**To pay off** enumerate in `reference/dump/`, emit `crates/mm-model/src/locale_generated.rs`,
then wire `User::is_valid`.

---

## D-002 · `User::is_valid` and `User::pre_save` are not ported

**Status** OPEN · **Severity** blocking · **Raised** 2026-08-13 (phase 1, `user.go`)
**Depends on** [D-001], [D-004], [D-005]

`IsValid` (user.go:383) needs `IsValidEmail` (now done), `IsValidLocale` ([D-001]) and
`ValidateCustomStatus` ([D-004]). `PreSave` (user.go:486) additionally needs a
`UserPasswordHasher` and `timezones.DefaultUserTimezone()`.

`User::pre_save_partial` covers the rest and is named that way on purpose — **it does not hash
passwords**. Any caller mistaking it for Go's `PreSave` would store plaintext. Rename to
`pre_save` only when the hasher lands.

**To pay off** close D-001 and D-004, define the hasher trait, port the timezone defaults.

---

## D-003 · `IsValidHTTPURL` needs an RFC 3986 parser

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-13 (phase 1, `utils.go`)
**Closed** 2026-08-14 (phase 1, ahead of `message_attachment.go`)

Delegates to Go's `net/url.ParseRequestURI`. Same shape of problem as `IsValidEmail` was, and the
same solution applied: build a corpus, run it through Go, iterate until it matches. It stopped
being deferrable because `MessageAttachment.IsValid` calls it six times.

**How it was paid.** `utils::is_valid_http_url` reproduces `ParseRequestURI`'s grammar directly
rather than delegating to a URL crate — the `url` crate implements WHATWG, which normalises and
would disagree in both directions. Verified against Go over 136 hand-picked inputs, a 2,881-case
generated corpus, and four exhaustive 0..127 byte sweeps (host, path, query, userinfo) plus
targeted colon and bracket corpora.

**Superseded 2026-08-14** by the full `net/url` port ([D-047]). `is_valid_http_url` is now the two
lines Go is — a prefix test, then `go_url::parse_request_uri` succeeding with a non-empty scheme
and host — and the hand-written grammar underneath it is deleted. All 3,529 cases still pass
unchanged, which is what made the swap safe; the corpus this entry built now verifies the parser
rather than a predicate that shadowed it.

Four readings of the Go source were **wrong** and the oracle caught each one:

- The port is everything after the **first** colon, not the last. `a:1:2` fails as
  `invalid port ":1:2"`, not on a host character.
- A `[` anywhere in a non-bracketed host is `invalid IP-literal`, even though `[` is in
  `shouldEscape`'s allow list for hosts. A stray `]` is fine.
- A bracketed host must parse as a real **IPv6** address. `[abc]`, `[]` and `[1.2.3.4]` are all
  rejected; `[::ffff:1.2.3.4]` is accepted.
- `Host` includes the port, so `http://:1` and even `http://:` are valid — the hostname is empty
  but `Host` is not, and the emptiness test is on `Host`.

The diagnostics section of the fixture records Go's actual `parse_error` and `Host` for the
ambiguous inputs, which is what turned each of those from a guess into a measurement.

Two behaviours worth keeping in mind at call sites: `ParseRequestURI` does **not** strip a
`#fragment`, so `http://x#f` is invalid while `http://x/#f` is valid; and the query string is not
validated at all, so `?q=%zz` passes.

---

## D-004 · The `User` custom-status accessors are unported

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-13 (phase 1, `user.go`)
**Narrowed** 2026-08-14 (`custom_status.go`) · **Closed** 2026-08-14 (`user.go`)

Originally: "`custom_status.go` is not translated, so `GetCustomStatus`, `SetCustomStatus`,
`ClearCustomStatus` and `ValidateCustomStatus` are missing."

`custom_status.go` is now translated — `CustomStatus`, `RecentCustomStatuses` and the duration
table all live in `crates/mm-model/src/custom_status.rs`. What remains is **not** in that file:
all four accessors are methods on `*User` (user.go:781, 791, 809, 814) and belong to `user.rs`,
which is still PARTIAL.

Consequences still shipped: `User::pre_update` omits the trailing custom-status re-save
(user.go:588-594), and `User::is_valid` cannot run its props check (user.go:456).

**How it was paid.** All five are now in `user.rs` — there are *five*, not four: `GetCustomStatus`
(user.go:791) and `CustomStatus` (user.go:799) are byte-identical duplicates in the Go source, and
both are ported so call-site translation stays mechanical.

`User::pre_update`'s trailing custom-status re-save (user.go:588-594) is still absent, but that is
`pre_update`'s gap rather than a missing dependency; it is tracked with [D-002] now that nothing
blocks it. `User::is_valid` can run its props check as soon as [D-001] lands —
`validate_custom_status` is ported and exact against Go over all 21 oracle cases.

One divergence came out of this and is logged separately as [D-026].

---

## D-005 · Constants duplicated from six other Go files

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-13 (phase 1, `user.go`)

`crates/mm-model/src/user.rs::external` holds constants owned by `role.go`, `ldap.go`, `saml.go`,
`config.go` and `shared_channel.go`. `crates/mm-model/src/session.rs::external` and `team.rs` do
the same for `saml.go`, `push_notification.go` and `access_policy.go`. They are correct today and
will silently drift the moment upstream changes one.

Two came off the list on 2026-08-14 when their owning files were translated:
`USER_PROPS_KEY_CUSTOM_STATUS` (`custom_status.go`) and `STATUS_ONLINE` (`status.go`). Both now
have one definition in their own module, re-exported through `user.rs::external` so the old path
still resolves. `status.go` also removed a borrow in the *other* direction: `StatusCacheSize` is
defined as `SessionCacheSize` in Go, and `status::STATUS_CACHE_SIZE` aliases
`session::SESSION_CACHE_SIZE` rather than re-transcribing 35000, so the two cannot diverge.

`CURRENT_VERSION` used to be listed here as a borrow from `version.go`. It no longer is:
`version.go` was translated on 2026-08-14, the constant now lives in `version::CURRENT_VERSION`,
and `utils` re-exports it, so there is one definition rather than a copy. While it *was* a borrow
it carried a drift test that read the value out of the oracle (see D-010) — that is the pattern
the remaining borrows should adopt, one oracle line and one assertion each, rather than waiting
for their owning file to be translated.

**To pay off** move each into its own module as that file is translated, and delete `external`.

---

## D-006 · `is_valid_user_auth_service` was inferred, not read

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-13 (phase 1, `user.go`)

The accepted set was derived from the auth-service constants without opening the Go body
(user.go:942). It is the only function in `user.rs` not backed by either a fixture or the
behavioural oracle. Confirm when `ldap.go` / `saml.go` are translated — or sooner, by adding it
to `reference/dump/behaviour.go`, which is a five-line change.

---

## D-007 · `limit_bytes` truncates at a char boundary; Go does not

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-13 (phase 1, `utils.go`)

Go slices at an exact byte offset and can return invalid UTF-8 — measured:
`LimitBytes("aé", 2)` yields `a\xc3`. A Rust `String` cannot hold that, so `limit_bytes` stops at
the nearest char boundary below the limit. Identical for ASCII, which is every caller in the Go
tree today.

Accepted rather than open: closing it would mean returning `Vec<u8>` and pushing the problem to
every caller. Revisit only if a caller feeds it non-ASCII.

---

## D-008 · `GetTimeForMillis` returns server-local time

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-13 (phase 1, `utils.go`)

Not our divergence — Go's. `time.UnixMilli` attaches `time.Local`, so `GetStartOfDayMillis` and
`GetEndOfDayMillis` read the calendar date in the **server's** timezone. For `1700000000000` a
UTC+05:30 host reports the start of Nov 15; a UTC host reports Nov 14. Reproduced faithfully
(`DateTime<Local>`), and the oracle records the offset it ran under so tests stay portable.

Flagged because it will look like a Rust bug to whoever hits it in `mm-app`. It is not.

---

## D-009 · Fixture completeness is only checked at the top level

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-13 (phase 0, oracle)

`reference/dump/main.go` fails the run when a **top-level** key a struct declares is missing from
its JSON, which is the omitempty-dropped-a-zero-value trap. A zero value nested inside a struct
is not caught; the populator only emits a warning for fields it could not reach. No warnings fire
today, so nothing is currently unreached — but the guarantee is weaker for nested objects than
the top-level one, and `post.json` is deeply nested.

**To pay off** make `missingKeys` recurse.

---

## D-010 · `Team::Etag` needs `CurrentVersion`

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-13 (phase 1, `team.go`)
**Closed** 2026-08-14 (phase 1, `channel_list.go`)

`Etag` (utils.go:732) prefixes `CurrentVersion`, which is `versions[0]` in `version.go` — a
`var`, not a const, so it could not simply be transcribed. It blocked `Team::Etag`, `User::Etag`
and the whole of `channel_list.go`, which is nothing but `Etag`.

**How it was paid.** `CurrentVersion` is a `var` only because Go cannot compute `versions[0]` at
compile time; nothing in the model package reassigns it, and it is not injected by `-ldflags` the
way `BuildNumber` and friends are. So it *is* transcribable — the real risk was never mutability,
it was silent drift when the pinned SHA moves to a new release.

That risk is closed rather than accepted: `utils::CURRENT_VERSION` is `"11.11.0"`, the oracle
records `model.CurrentVersion`, and `channel_list::go_parity::current_version_matches_go` fails
the moment the two disagree. `Team::etag` and `User::etag` are now ported and pinned against Go.

---

## D-011 · `TeamMemberWithError` and the invite-error types are unported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-13 (phase 1, `team_member.go`)

`TeamMemberWithError`, `EmailInviteWithError` and their four helper functions embed `*AppError`
as a **wire** field (`json:"error"`), which the invite flow returns to clients. Skipped because
nothing consumes them yet, and because serialising `AppError` as a nested value needs the
`omitempty` behaviour of a pointer-to-struct checking, which no other type has needed so far.

---

## D-012 · `redact_device_id` truncates at a char boundary; Go does not

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-13 (phase 1, `session.go`)

Same class as [D-007]. Go slices the token at exactly 16 bytes and can split a multi-byte
character; this stops at the nearest boundary at or below 16. Device tokens are ASCII in
practice, and the output goes to logs rather than to a client.

---

## D-013 · `ChannelBannerInfo::Scan`/`Value` are unported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `channel.go`)

`Scan` (channel.go:58) and `Value` (channel.go:70) are `database/sql` plumbing and belong to
`mm-store`, alongside the other `Scan`/`Value` pairs deferred from `utils.go`. One semantic must
survive the move: **`Value()` returns SQL `NULL`, not `"{}"`, when the struct is entirely zero**
(`c == ChannelBannerInfo{}`). A store that always marshals would write `{"enabled":null,...}`
into a column Go leaves NULL, and every existing row would read back differently.

`Scan` also treats a `nil` value as success-with-no-change, leaving the receiver at whatever it
already held rather than zeroing it.

---

## D-014 · `ChannelsWithCount` and `DirectChannelForExport` are unported

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `channel.go`)
**Closed** 2026-08-14 (phase 1, `channel_list.go`)

Both embedded a type from a file that was not translated yet — `ChannelListWithTeamData` from
`channel_list.go` and `[]*ChannelMemberForExport` from `channel_member.go`.

`DirectChannelForExport` landed with `channel_member.go`; `ChannelsWithCount` landed with
`channel_list.go`. Both live in `channel.rs` with a generated fixture and a round-trip test.
`ChannelsWithCount.channels` is `Option<ChannelListWithTeamData>` because the field has no
`omitempty`, so a nil list is `null` on the wire and must not be flattened into `[]`.

---

## D-015 · `Channel::deep_copy` copies more than Go's `DeepCopy`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel.go`)

Go's `DeepCopy` (channel.go:302) is `cCopy := *o` plus a deep copy of `SchemeId` alone. The
result therefore **shares** the `Props` and `PolicyActions` maps and the `BannerInfo` pointer
with the original: mutating the copy's props mutates the original's. Rust's `Clone` copies all
of it.

Accepted rather than open: reproducing the aliasing would mean `Arc<Mutex<…>>` on two fields for
no benefit, and no Go call site relies on it. Flagged because a call site being ported that
mutates the copy and reads the original would change behaviour silently — check for that when
translating the app layer.

---

## D-016 · `ChannelPatch.ManagedCategoryName` is accepted and ignored

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel.go`)

`ChannelPatch` declares `ManagedCategoryName *string` with a `json:"managed_category_name"` tag,
so clients can send it, but `(*Channel).Patch` (channel.go:465) never applies it — the sibling
`DefaultCategoryName` is applied two lines away. Almost certainly an upstream oversight.

Reproduced faithfully and pinned by an oracle case (`managed_category_name_is_ignored`).
**Do not "fix" it**: making the patch work would make the Rust server accept a mutation the Go
server silently drops, and the two would then disagree on a shared database. If upstream fixes
it, the oracle case flips and the test fails, which is the intended signal.

---

## D-017 · Generator run policy

**Status** CLOSED · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `channel.go`)
**Closed** 2026-08-14 by the project owner.

`CLAUDE.md` used to say: add a type to `reference/dump/main.go` and *report* that the generator
needs re-running, rather than running it. The `channel.go` and `channel_member.go` sessions both
broke that rule (`go run .` in `reference/dump`; Go 1.26.2 is installed), because the alternative
was shipping ~20 provisional tests asserting values transcribed by hand — the exact failure mode
the oracle exists to prevent. Running it is also what caught the four counter-intuitive
`Channel::IsValid` results and corrected an assumption about Go's `\b`/`\f` escaping.

**Decision: relax the rule.** Run the generator, show the fixture diff. `CLAUDE.md` now says so.

Re-running was verified non-destructive both times: pre-existing fixtures came out byte-identical
apart from two deliberate `overrides` changes. That is the residual hazard and `CLAUDE.md` calls
it out separately — an `overrides` edit rewrites a committed fixture and can move a value the
Rust tests already assert against, so it is reported on its own rather than in the list of new
files.

---

## D-018 · `set_channel_muted` creates the notify-props map; Go panics

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel_member.go`)

`(*ChannelMember).SetChannelMuted` (channel_member.go:206) writes
`o.NotifyProps[MarkUnreadNotifyProp] = …` without a nil check, so Go **panics** when
`NotifyProps` is nil — and nil is reachable: the field has no `omitempty`, so a client can send
`"notify_props": null`, and `ChannelMember{}` from any code path has it nil.

The Rust port creates the map instead. Accepted rather than open because the alternatives are
worse: panicking is forbidden in library code by `CLAUDE.md`, and silently discarding the mute
would lose a user action. The divergence is only observable in a case where the Go server
returns a 500.

Note this is the *only* place the nil map is written. Reads (`IsChannelMuted`, the validators)
all handle nil correctly, because a Go map read on nil is defined.

---

## D-019 · `SetChannelMuted` ignores its argument — do not "fix" it

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel_member.go`)

```go
func (o *ChannelMember) SetChannelMuted(muted bool) {
	if o.IsChannelMuted() { o.NotifyProps[MarkUnreadNotifyProp] = ChannelMarkUnreadAll
	} else {               o.NotifyProps[MarkUnreadNotifyProp] = ChannelMarkUnreadMention }
}
```

`muted` is never read. The function toggles, so `SetChannelMuted(false)` on an unmuted channel
mutes it. Measured across every starting value (`all`, `mention`, `""`, garbage, absent) and
both arguments; pinned in `fixtures/behaviour_channel_member.json` under `set_channel_muted`.

Ported verbatim, with the parameter named `_muted` so the dead argument is visible at the
definition. **Do not repair it**: a Rust server that honoured the argument would disagree with
the Go server about a value both write to the same `ChannelMembers.NotifyProps` column. If
upstream fixes it, the oracle case flips and the test fails — which is the signal we want.

The Rust signature keeps the useless parameter so call-site ports stay mechanical. Revisit
when the app layer lands and the real call sites are visible.

---

## D-020 · The `Build*` values have no injection mechanism

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `version.go`)

`BuildNumber`, `BuildDate`, `BuildHash`, `BuildHashEnterprise` and `BuildEnterpriseReady`
(version.go:156-160) are empty `var`s that the Go build fills in with
`-ldflags "-X github.com/mattermost/mattermost/server/public/model.BuildNumber=…"`. Rust has no
link-time string injection, so `version.rs` reads `MM_BUILD_NUMBER` and friends at compile time
with `option_env!` and falls back to `""`, matching Go's zero value.

Two things are owed:

1. **Nothing sets those variables yet.** There is no build script wiring them to the same
   values the Go `Makefile` computes, so a Rust binary always reports an empty build. Harmless
   today because nothing reads them; it stops being harmless the moment the config or license
   endpoint is ported, since clients display the build hash.
2. **The variable names are invented.** `MM_BUILD_*` is ours. When the build wiring lands,
   reconcile it with whatever the Go `Makefile` already exports rather than adding a second
   source of truth.

`BuildEnterpriseReady` is a string compared against `"true"` at its Go call sites, not a bool.
Keep it a string — the comparison is `== "true"` exactly, so an injected `"1"` is false.

---

## D-021 · Fixture generation now depends on the Go source tree layout

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `version.go`)

`reference/dump/behaviour_version.go` is the first oracle that reads the **Go source file**
rather than only calling the package: it parses `../mattermost/server/public/model/version.go`
with `go/parser` to recover the unexported `versions` literal. Calling the package cannot
recover it — `versions` and `versionsWithoutHotFixes` are both unexported, and the Rust port
has to transcribe the release table, so without this the transcription would be unchecked.

The cost is a hard-coded relative path that assumes the generator runs from `reference/dump`,
which is already the convention (`-out ../../fixtures` defaults the same way). It fails loudly
with a wrapped parse error rather than silently emitting an empty list, and it cross-checks
`versions[0]` against `model.CurrentVersion` before writing, so a stale or wrong parse cannot
reach a fixture.

Accepted rather than open: the alternative is transcribing 137 strings with no oracle at all.
Revisit if the generator ever needs to run from somewhere else.

---

## D-022 · serde_json does not HTML-escape; Go's `encoding/json` does

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `custom_status.go`)
**Closed** 2026-08-14 (phase 1, `user.go`) — same day it was raised.

Go's `encoding/json` escapes `<`, `>`, `&`, U+2028 and U+2029 by default; serde_json escapes
none of them. This was already known and already solved *once*, narrowly:
`utils::go_json_marshal_string_map` exists because `ChannelMember`'s notify-props size cap
measures Go's encoding of a `map[string]string` (see MIGRATION.md, channel_member note 7).

`custom_status.go` is the second place it bites and the first where a **struct** is marshalled
for storage rather than measurement: `User.Props["customStatus"]` holds a marshalled
`CustomStatus` as a string, so a text containing `<` would be written as `<` by the Go
server and as `<` by ours, into the same column.

**What is *not* affected.** `Contains` and `Remove` compare marshalled bytes, and both sides of
every comparison go through the same encoder; escaping is injective, so the comparison result is
identical either way. The parity tests are likewise safe — they compare `serde_json::Value`
graphs, not bytes.

**What is affected** is any byte a client or the Go server reads back. Semantically the two
strings decode to the same value, so this is cosmetic until something compares the stored
strings for equality — which the recent-statuses list does, one level up, inside `Props`.

**How it was paid.** Porting `User::SetCustomStatus` in the very next session turned this from
cosmetic into concrete: the oracle recorded Go storing `{"emoji":"\u003cb\u003e", ...}` in
`Users.Props`, which is a column our server writes too. `utils::go_json_marshal` now marshals any
`Serialize` value with Go's escaping, by re-escaping serde_json's output rather than
reimplementing a serializer — serde_json and Go differ on exactly five characters (`<`, `>`, `&`,
U+2028, U+2029) and agree on every other escape, including the `\b`/`\f` shorthands.

`CustomStatus::marshal` uses it, so `SetCustomStatus` stores Go's bytes and `Contains`/`Remove`
compare Go's bytes. `user::custom_status_go_parity::set_custom_status_stores_gos_bytes` asserts
the stored string byte-for-byte against Go's, and
`utils::go_json_escape_tests::agrees_with_the_hand_written_string_map_marshaller` pins the new
general path against the older hand-built one.

**Still owed elsewhere:** any future type whose marshalled form is *stored* rather than sent must
use one of the Go-escaping marshallers too — `serde_json::to_string` is the wrong call for that
job and nothing enforces the choice. Tracked as [D-027], which also records which of the two
helpers applies where.

**One sharp edge, found by its own test.** `go_json_marshal` fixes *escaping*, not **key order**,
and the two are not the same problem. Struct fields serialize in declaration order in both
languages, so structs are safe. Go sorts **map** keys by byte value, while `StringMap` is a
`HashMap` and serde_json emits it in iteration order — neither sorted nor stable between runs. So
`StringMap` keeps its own `go_json_marshal_string_map`, which sorts, and `go_json_marshal` is
documented struct-only. A `BTreeMap` or `serde_json::Map` would be safe. Nothing in the type
system enforces this; a future caller passing a `HashMap` gets silently wrong bytes.

---

## D-023 · `null` into a `time.Time` field yields the zero time, not "unchanged"

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `custom_status.go`)

Go's `(*Time).UnmarshalJSON` returns early on `null` **without writing to the receiver**, so the
field keeps whatever it already held. Measured: unmarshalling `null` over a sentinel leaves the
sentinel intact.

A serde `Deserialize` cannot express that — it constructs the value, so there is no prior
value to preserve. `utils::go_time::deserialize` returns Go's zero time instead, which is what a
freshly allocated Go struct would have held and therefore matches for the only path that
matters: deserialising a whole `CustomStatus` from the wire.

It would diverge for a Go call site that unmarshals `null` **over an already-populated struct**,
which `json.Unmarshal` into a non-zero destination does. No such call site exists for
`CustomStatus` today; the API decodes into a fresh value.

Accepted rather than open: closing it would mean a custom `Deserialize` for every containing
struct that merges into an existing instance, which serde is not built for and no caller wants.
The `go_parity::time_unmarshal_matches_go` test asserts the divergence explicitly rather than
skipping the case, so it cannot rot silently.

---

## D-024 · `RecentCustomStatuses::add`/`remove` do not alias the caller's slice

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `custom_status.go`)

Go's `Add` and `Remove` both start with `newRCS := rcs[:0]` and filter **in place**, rewriting
the receiver's backing array as they go. The filter itself is the standard safe idiom (the write
index never overtakes the read index), but the side effect is real: after `rcs.Add(cs)` the
caller's original `rcs` still has its old length while its contents have been shuffled.

The Rust port takes `&self` and allocates a fresh `Vec`, so the input is untouched. Every Go
call site does `rcs = rcs.Add(cs)` and drops the old slice, so nothing observes the difference.

Accepted rather than open: reproducing the aliasing would mean `&mut self` plus a returned
value, i.e. exporting a footgun to make a discarded value match. Flagged because a call site
being ported that keeps the pre-`Add` slice and reads it afterwards would change behaviour
silently — check for that when the app layer lands. Same class of hazard as [D-015].

---

## D-025 · The populator warns spuriously on overrides for pointer fields

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `custom_status.go`)

Every generator run prints:

```
warning: channel: channel.bannerinfo.backgroundcolor left zero (override type string not convertible to *string)
```

The warning is wrong — `fixtures/channel.json` does contain `"background_color": "#1153ab"`.
`BackgroundColor` is a `*string`, so the walker first tries the override against the *pointer*,
fails, warns, then allocates the pointee and applies the same override successfully one level
down. Noticed while adding `customstatus.duration`; not caused by it, and not fixed here because
it is `channel.go`'s walker path rather than this session's file.

It matters because of [D-009]: the populator's warnings are the *only* signal that a nested
field was left unreached, and a permanent false positive is exactly what trains a reader to skim
past them. One real unreached field would now hide in the noise.

**To pay off** look up the override after dereferencing, or suppress the warning when the
pointee assignment later succeeds.

---

## D-026 · `get_custom_status` loses the fields Go salvages from a partial decode

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `user.go`)

`GetCustomStatus` (user.go:791) **discards the unmarshal error**:

```go
data := u.Props[UserPropsKeyCustomStatus]
_ = json.Unmarshal([]byte(data), &o)
```

Go's `encoding/json` is not all-or-nothing the way serde_json is, so what `o` holds afterwards
has four distinct shapes. Measured across 21 corpus cases:

| input | Go | ours |
|---|---|---|
| absent, `""`, `null`, syntax error, trailing data | nil | `None` ✓ |
| `{}`, `{"emoji":"a"}`, unknown keys | zero-filled status | same ✓ |
| `"a string"`, `0`, `true`, `[]` | **non-nil zero** status | same ✓ |
| `{"emoji":123,"text":"kept"}` | **partial** — `text` survives | zero status ✗ |

The last row is the divergence. A *type* error leaves the fields decoded so far in place; a
failing `Unmarshaler` (a malformed `expires_at`) aborts the object, keeping the keys before it
and dropping the ones after. Both depend on the **document order** of the keys.

Reproducing it exactly needs an order-sensitive, field-by-field decoder: serde_json's `Map` is a
`BTreeMap` without the `preserve_order` feature, so document order is not even recoverable from
a parsed `Value`. That is real machinery to make a corrupt status corrupt in the same way.

Accepted rather than open, for two reasons. The non-nil-ness matches in every case, which is the
only thing any caller branches on. And `ValidateCustomStatus` — the one consumer whose answer
reaches the wire, via `User::is_valid` — is written against the predicate Go's nil-ness actually
reduces to ("syntactically valid JSON, and not `null`") rather than against
`get_custom_status`, so it is **exact** on all 21 cases and cannot inherit this.

Reachability is low: the only writer is `SetCustomStatus`, which always marshals a well-formed
status. It takes hand-edited or legacy-corrupt `Users.Props` data to hit at all.
`get_custom_status_matches_go` asserts the divergence explicitly on those four cases rather than
skipping them, so it cannot rot silently.

---

## D-027 · `go_json_marshal` is the right call for two paths and nothing enforces it

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `preference.go`)
**Related** [D-022]

`preference.go` is the second file whose output must carry Go's HTML escaping, and it reaches it
by a different route than the first. `Preference::pre_update` re-marshals a `map[string]string`,
so it uses `utils::go_json_marshal_string_map` (which sorts keys, as Go does for maps);
`CustomStatus::marshal` marshals a struct, so it uses `utils::go_json_marshal` (which does not
reorder anything). Picking the wrong one of the two is silent:

- `go_json_marshal` on a `HashMap` emits iteration order where Go sorts — see D-022's note.
- `go_json_marshal_string_map` only accepts a `StringMap`, so that direction fails to compile.

Both are correct today and both are pinned byte-for-byte against Go
(`preference::go_parity::pre_update_matches_go`,
`user::custom_status_go_parity::set_custom_status_stores_gos_bytes`). The debt is that a third
caller has three plausible-looking options — these two and `serde_json::to_string` — and only the
tests distinguish them.

**To pay off** either make `go_json_marshal` sort map keys itself (it cannot, without parsing its
own output) or add a `#[deny]`-style lint / clippy.toml `disallowed-methods` entry pointing
`serde_json::to_string` at the right helper. The second is cheap and is the recommended option.

**Widened 2026-08-14** by [D-029]: `str::to_lowercase` is a third std method that looks right and
silently is not (Go's `strings.ToLower` is a different function). One `disallowed-methods` entry
should cover both, and the emoji session proved the failure mode is real rather than theoretical
— that one shipped in six call sites before it was measured.

**Half of it paid 2026-08-14** (`post.go` chunk 1). The *ordering* hazard is gone:
`utils::StringInterface` is now `serde_json::Map<String, Value>` rather than a `HashMap`, and
`serde_json::Map` is a `BTreeMap` absent the `preserve_order` feature — so it is sorted by byte
value exactly as Go sorts map keys when marshalling. That removes the documented sharp edge from
[D-022] ("`go_json_marshal` is struct-only") for every `StringInterface`: `Post::props` and
`Channel::props` now marshal byte-for-byte like Go's, and `go_json_marshal` is safe on any struct
containing one. It also removes a divergence nobody had logged — a `HashMap`'s iteration order is
not merely unsorted, it is **unstable between runs**, so two serialisations of the same post from
the same process could order props differently.

The *escaping* hazard is unchanged and remains the whole of this entry: `serde_json::to_string`
still does not HTML-escape, so it is still the wrong call whenever the bytes are stored or
compared rather than sent. `post::go_parity::plain_serde_differs_from_go_only_by_html_escaping`
pins that distinction at the `Post` level — plain serde differs from Go's bytes, `go_json_marshal`
matches them, and both decode to the same value. The `clippy.toml` `disallowed-methods` entry is
still the recommended fix and is still unwritten.

**The rest of the ordering half paid 2026-08-14** (`integration_action.go` chunk 2). `StringMap`
is now a `BTreeMap` rather than a `HashMap`, so both map aliases sort by byte value exactly as
Go's marshaller does, and the "two aliases with different guarantees" trap is gone. What forced
it was a wire probe: `DialogActionButton.Context` is a `map[string]string` on the wire, and the
byte-exact assertion against Go failed on key order — the first time the instability was
observable in a committed test rather than in reasoning.

The conversion cost one line elsewhere in the crate (`StringMap::with_capacity` has no `BTreeMap`
counterpart) and no test changed its expectations, which is the evidence that nothing depended on
hash iteration order.

`go_json_marshal_string_map` is kept: it is still the required call wherever Go's bytes are
**measured** rather than sent, because it applies the HTML escaping as well as the sorting. That
remains the whole of this entry — `serde_json::to_string` still does not escape `<`, `>`, `&`,
U+2028 or U+2029, and nothing enforces the choice. The `clippy.toml` `disallowed-methods` entry
is still the recommended fix and is still unwritten.

---

## D-028 · `Auditable` is unported on three types, and `Emoji`'s has an upstream bug

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `emoji.go`)
**Widened** 2026-08-14 (`file_info.go`)

`(*Emoji).Auditable` (emoji.go:29), `(*FileInfo).Auditable` (file_info.go:86) and
`ChannelMember`'s are all skipped for the same reason: audit projections are not wire types and
belong with the audit layer, which does not exist yet. `FileInfo`'s is a straight ten-key
projection with no surprises.

Recorded here because it carries a copy-paste bug that must survive the port:

```go
"delete_at":  emoji.CreateAt,   // emoji.go:34 — should be emoji.DeleteAt
```

Every other key reads its own field. Whoever ports the audit layer will read that line as a typo
and fix it — which would make the Rust audit log disagree with the Go one for any deleted emoji.
**Reproduce it, and pin it with an oracle case**, the same treatment D-016 and D-019 get. If
upstream fixes it, the test fails, which is the signal we want.

---

## D-029 · `str::to_lowercase` must never be used on Go-facing input

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `emoji.go`)
**Closed** 2026-08-14, same session — the six existing call sites were fixed.

Go's `strings.ToLower` applies Unicode's **simple** (1:1) lowercase mapping per rune. Rust's
`str::to_lowercase` applies the **full** (1:many) mapping *and* implements the Final_Sigma
context rule. Measured against Go over 30 inputs, they disagree on two:

| input | Go | `str::to_lowercase` |
|---|---|---|
| `İ` (U+0130) | `i` | `i` + U+0307 |
| `ΟΔΟΣ` | `οδοσ` | `οδος` |

This was found while porting `Emoji::PreSave` but was **already shipped** in six places:
`is_valid_email`'s `isLower` check, `normalize_username`, `normalize_email`, the mention-key
lowercasing in `User::pre_update`, `is_reserved_team_name` and `clean_team_name`. Usernames,
emails and team slugs are all stored and compared, so a Greek team name ending in sigma would
have produced a different slug in the two servers against one database.

**How it was paid.** `utils::go_to_lower` takes the first character of Rust's full mapping, which
is exactly the simple mapping; the character-level API has no context, so Final_Sigma cannot
apply. Pinned by `go_to_lower_parity::go_to_lower_matches_go` over the corpus, plus a second test
asserting the two inputs where `str::to_lowercase` disagrees. All six call sites converted; the
full suite passed unchanged, so no corpus depended on the old behaviour.

**Residual hazard** is the same one as [D-027]: nothing stops the *next* caller reaching for
`str::to_lowercase`. Both belong in the same `clippy.toml` `disallowed-methods` entry.

---

## D-030 · `NewInfo`'s mime lookup is not portable

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `file_info.go`)

`NewInfo` (file_info.go:213) calls `mime.TypeByExtension`, which consults a small builtin table
**and the host's `mime.types` files** (`/etc/mime.types`, `/etc/apache2/mime.types`, …). It is
therefore environment-dependent: two Mattermost servers on different base images can store
different `mime_type` values for the same upload, and no Rust port can match "Go" because there
is no single answer to match.

Measured on the machine that generated the fixture:

| extension | Go answered | in Go's builtin table? |
|---|---|---|
| `.txt` | `text/plain; charset=utf-8` | no |
| `.mp4` | `video/mp4` | no |
| `.gz` | `application/gzip` | no |
| `.png` | `image/png` | yes |

**What was ported.** The portable half: `file_extension` implements `filepath.Ext` plus
lowercasing plus the leading-period strip, and is asserted against the oracle. `new_info` takes
the mime type as a **parameter** rather than resolving it, so the decision moves to the app
layer where a mime database belongs. `new_info_name_and_extension_match_go` deliberately asserts
only `name` and `extension`; the fixture's `mime_type` column is evidence for this entry, not a
target.

**Options when this is paid off**
- **(a) Embed Go's builtin table** (~16 entries) and nothing else. Deterministic, and strictly
  narrower than any real Go server — so it would return `""` where Go returns `text/plain`.
- **(b) Use the `mime_guess` crate.** Broad coverage, but its table is not Go's, so it would
  disagree in a different direction.
- **(c) Load a `mime.types` file at startup and ship one** alongside the binary, making the
  answer a deployment artifact rather than a host accident. Matches Go when the same file is
  installed, and is the only option that can be made to agree on purpose.

**(c) is recommended**, and it needs a decision from the project owner because it changes
deployment. Until then the mime type is whatever the caller passes.

---

## D-031 · The project licence must become AGPL-3.0 before phase 2 lands

**Status** OPEN · **Severity** blocking · **Raised** 2026-08-14 (licensing)
**Blocks** every commit of code derived from `server/channels/` — i.e. all of phases 2 to 5.

Upstream Mattermost is licensed in two parts, and the boundary falls exactly where this port
currently sits. From the root `LICENSE.txt` of the pinned tree:

> You are licensed to use the source code in Admin Tools and Configuration Files
> (`server/templates/`, `server/i18n/`, **`server/public/`**, `webapp/` and all subdirectories
> thereof) under the **Apache License v2.0**.

The rest of the platform is **GNU AGPL v3.0**, or a commercial licence from Mattermost, Inc.

Everything translated to date comes from `server/public/model/`, so the repository is currently
licensed **Apache-2.0** and that is accurate. `server/public/LICENSE.txt` carries the Apache-2.0
text confirming it, and our `LICENSE` is a byte-identical copy.

**The moment phase 2 begins this becomes wrong.** `server/channels/store/`,
`server/channels/app/` and `server/channels/api4/` are AGPL v3.0. A Rust translation of them is a
derivative work, and a derivative of AGPL code cannot be redistributed under Apache-2.0.

**Decision required before the first `mm-store` commit** — chosen 2026-08-14 by the project owner
to defer, taking Apache-2.0 "for now" with this entry as the tripwire.

**To pay off**, one of:
- **(a) Relicense the repository to `AGPL-3.0-only`.** Apache-2.0 is one-way compatible with
  AGPL-3.0, so the existing `mm-model` code can be carried forward without permission. This is
  the default and the cheapest path. Note it is not retroactive: anything already published under
  Apache-2.0 stays available under Apache-2.0 to whoever received it.
- **(b) Split the licence the way upstream does** — keep `mm-model` Apache-2.0 with its own
  `LICENSE`, and put AGPL-3.0 at the root for the crates that need it. Mirrors Mattermost
  exactly, and preserves the more permissive terms for the wire types, which are the part
  another project is most likely to want to reuse.
- **(c) Obtain a commercial licence** from Mattermost, Inc.

Whichever is chosen, `Cargo.toml`'s `license` field, `LICENSE`, `NOTICE` and the README all have
to move together. `NOTICE` already states the current scope and the coming change.

---

## D-032 · The `file_info` oracle wrote random and clock-derived values into a committed fixture

**Status** CLOSED · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `post_embed.go`)
**Closed** 2026-08-14, same session.

`reference/dump/main.go`'s header states the rule plainly: *"Every generated value derives from a
hash of the field's path, so re-running produces byte-identical output… Do not introduce rand or
time.Now here."* The `file_info.go` session broke it twice:

- `fileInfoEtagAll` built its corpus with `model.NewId()`, which is a CSPRNG.
- `fileInfoPreSaveAll` recorded `out_update_at` for the `all_zero` case, where `PreSave` derives
  the value from `GetMillis()`.

Neither was caught when it was written, because `behaviour_file_info.json` was a **new** file
that session — there was nothing to diff it against. It surfaced one session later as an
unexplained `M fixtures/behaviour_file_info.json` after an unrelated generator run.

Both are fixed: the etag corpus takes ids from the fixed `idA`/`idB`/`idC` set (the id plays no
part in `GetEtagForFileInfos`, which reads `PostId` and `UpdateAt`), and `out_update_at` is
recorded as `0` when the input `CreateAt` was zero — which is exactly the case the Rust test
already skipped.

**Why this mattered more than the churn.** CLAUDE.md tells a reader that a clean generator run
touches only new files, and that anything else in `git status` is a signal worth reading. A
fixture that rewrites itself every run destroys that signal for *every* fixture, not just its
own. Verified fixed by running the generator twice and diffing all 47 fixtures: byte-identical.

**Residual risk:** nothing enforces this. A future oracle that calls `NewId`, `GetMillis` or
`time.Now` will reintroduce it, and will again go unnoticed for exactly one session. A cheap
guard would be a CI step that runs the generator twice and fails on any diff.

---

## D-033 · Go's `[]*T` accepts a nil element; our `Vec<T>` rejects the document

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_metadata.go`)

Go models every collection of model types as a slice of **pointers**, so `[null]` is a legal
value: `json.Unmarshal` stores a nil element and `json.Marshal` re-emits it as `null`. Rust's
`Vec<T>` cannot hold that, so `serde_json` fails the whole document with
`invalid type: null, expected struct PostEmbed`.

This is **not** new to `post_metadata.go` — it is a convention already shipped in two modules:

| Go | Rust | file |
|---|---|---|
| `Session.TeamMembers []*TeamMember` | `Option<Vec<TeamMember>>` | `session.rs` |
| `ChannelList []*Channel` | `Vec<Channel>` | `channel_list.rs` |
| `ChannelListWithTeamData []*ChannelWithTeamData` | `Vec<ChannelWithTeamData>` | `channel_list.rs` |
| `PostMetadata.{Embeds,Emojis,Files,Reactions,Acknowledgements}` | `Vec<T>` | `post_metadata.rs` |
| `PostMetadata.{Images,Translations}` `map[string]*T` | `HashMap<String, T>` | `post_metadata.rs` |
| `PostAction.Options []*PostActionOptions` | `Vec<PostActionOptions>` | `integration_action.rs` |
| `MmBlocksActionCookie.Actions map[string]map[string]any` | `Option<BTreeMap<String, StringInterface>>` | `integration_action.rs` — see [D-050] |
| `MessageAttachment.Fields []*MessageAttachmentField` | `Option<Vec<MessageAttachmentField>>` | `message_attachment.rs` |
| `MessageAttachment.Actions []*PostAction` | `Vec<PostAction>` | `message_attachment.rs` |
| `PostList.{Posts,BurnOnReadPosts}` `map[string]*Post` | `Option<BTreeMap<String, Post>>` | `post_list.rs` |
| `WranglerPostList.Posts []*Post` | `Option<Vec<Post>>` | `wrangler.rs` |

`post_metadata.go` is only where it stopped being hypothetical: the `embeds_nil_element` oracle
case is a **failing** decode, asserted explicitly in
`post_metadata::go_parity::the_wire_format_matches_go` so it cannot rot.

**Reachability is low but non-zero.** These collections are server-generated, and a nil element
would be a bug in the producer. The exposure is inbound: a client posting
`{"metadata":{"embeds":[null]}}` gets a 400 from us and a 200 from Go. That is the
stricter-than-Go failure mode the project rejected for [D-001] option (b), which is why this is
logged rather than shrugged off.

**Options**
- **(a) `Vec<Option<T>>` everywhere Go has `[]*T`.** Exactly faithful. Costs `.flatten()` at
  every call site across the whole app layer, for a state no correct producer emits.
- **(b) A tolerant deserialiser that drops nulls.** Cheap, but then re-marshalling loses the
  element where Go keeps it, trading a decode divergence for a *silent* wire divergence. Worse.
- **(c) Leave it.** Current state. Consistent across the crate, and the one measured case is
  pinned.

**Widened 2026-08-14** by `message_attachment.go`, where a nil element is not merely legal but
**produced by the Go code itself**: `ParseMessageAttachment` drops nil *attachments* while
leaving nil *fields* in place, so its output can contain `"fields":[null,…]` — which we cannot
decode. That moves the exposure from "a malformed client request" to "a document the Go server
writes", and it is the strongest argument yet for option (a). `StringifyMessageAttachmentFieldValue`
filters both, so the two functions disagree about whether a nil field survives.

**Widened again 2026-08-14** by `post.go` chunk 2, which makes the cost concrete rather than
theoretical. `(*Post).Attachments` re-decodes `props.attachments` element by element and **drops
the element when the decode fails**, so a nil `PostActionOptions` does not cost us one option —
it costs us the whole attachment. Measured: `{"actions":[{"options":[null]}]}` gives Go one
attachment holding `"options":[null]` and gives us none, so the post renders with an attachment
missing entirely rather than with an empty dropdown.

Go's own nil filter in the same function is the other half of the picture: it strips nil
**actions** and nil **fields** before returning, so those two are safe by construction and only
`options` is exposed. That asymmetry is why option (a) can be applied to `PostAction.Options`
alone at a fraction of the cost — it is the only `[]*T` in the tree whose nil element both
survives Go's filters and reaches a decode we perform. `a_nil_action_option_drops_the_attachment_
where_go_keeps_it` pins it.

**(c) for now**, revisit if the app layer ever sees a real nil element. Whatever is chosen must
be applied to all five types above at once — the value of the current state is that it is
uniform.

**Widened 2026-08-14** by `post_list.go`, where the exposure is a whole *response*: a
`{"posts":{"p1":null}}` document decodes in Go with `p1` present and nil, and fails our decode
outright. It is also the first place the nil element makes a **method** crash rather than merely
decode oddly — `Clone`, `StripActionIntegrations` and `MakeNonNil` all dereference it, which the
oracle records as `panicked: true` and the Rust tests assert as a decode failure instead. That
pairing (Go crashes, we refuse the document) is the least-bad shape this divergence has taken so
far, and it is another argument that `Vec<Option<T>>`/`BTreeMap<String, Option<T>>` would be
buying faithfulness to a state no correct producer emits.

---

## D-034 · `PostMetadata::Copy` drops `expire_at` and `recipients`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_metadata.go`)

`(*PostMetadata).Copy` (post_metadata.go:92) is documented "does a deep copy". It is neither
complete nor deep:

```go
return &PostMetadata{
    Embeds: …, Emojis: …, Files: …, Images: …, Reactions: …,
    Priority: …, Acknowledgements: …, Translations: …, RedactedFileCount: …,
}   // ExpireAt and Recipients are simply not here
```

Measured: copying a metadata with `expire_at: 1700000000000` and two `recipients` returns one
with `expire_at: 0` and no recipients. Almost certainly fields added to the struct without
updating `Copy`.

**Reproduced verbatim**, with the two fields written explicitly as `0`/`Vec::new()` rather than
omitted from the Rust literal, so the omission is visible at the site rather than looking like an
oversight of ours. Pinned by `copy_matches_go`, which asserts the output JSON byte-for-byte and
separately asserts Go's own `expire_at_survived`/`recipients_survived` flags — if upstream fixes
`Copy`, the test fails, which is the signal we want.

**Do not "fix" it.** A Rust copy that preserved the fields would carry data the Go server
discards, and the two would disagree about a value both write.

**Separately, `Copy` is shallow for every collection.** `copy`/`maps.Copy` duplicate the element
*pointers*, so Go's copy shares its embeds, emojis, files, reactions, acknowledgements, images
and translations with the original — mutating one mutates the other. Only `Priority` is rebuilt.
Rust owns its values, so ours is genuinely independent. Same class as [D-015] on
`Channel::deep_copy`, accepted for the same reason, and the oracle records the aliasing flags so
the divergence stays visible.

---

## D-035 · `Post::pre_commit` does not generate action ids

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `post.go` chunk 1)
**Closed** 2026-08-14 (phase 1, `integration_action.go` chunk 3) — same day it was raised.

`(*Post).PreCommit` (post.go:724) does four things. Three are ported — materialising `Props`,
`Filenames` and `FileIds`, and de-duplicating the file ids. The fourth, `o.GenerateActionIds()`,
is not: it walks `props.attachments`, mints an id for every interactive action that lacks one and
rewrites the props in place. It lives in `integration_action.go` and needs `MessageAttachment`.

`Post::pre_save` calls `pre_commit`, so **`pre_save` is incomplete for any post carrying
attachments** — the actions keep whatever ids the client sent, or none. For a post without
`props.attachments` the two are identical, which is every case in the oracle corpus today.

Not renamed the way `User::pre_save_partial` was ([D-002]): the failure mode there was storing a
plaintext password, which is a security incident. This one is a missing id on an interactive
button, and the whole interactive-message surface is unported anyway, so a mid-sized rename would
buy nothing. **Revisit when `integration_action.go` lands** — that is the same session that must
port `StripActionIntegrations`, and therefore `Post::ToJSON`/`EncodeJSON`, which are deferred for
exactly the same reason.

**How it was paid.** `Post::generate_action_ids` is ported in `integration_action.rs` and
`pre_commit` calls it in Go's position — before the file-id de-duplication. `pre_save` is
therefore complete for a post with attachments, and both are pinned over the same 34-case corpus.

Two things the port had to get right and neither is in the source:

- **The emptiness test is exact.** An id of `"  "` or `"x"` is kept, however unusable. Only `""`
  is minted over.
- **It rewrites the prop even when it mints nothing.** `GenerateActionIds` stores the *decoded*
  attachment list back into `props.attachments` whenever the prop is non-nil, so an ordinary
  `pre_save` normalises the client's payload: unknown keys vanish, a wrongly-typed element is
  dropped, and `{"attachments":[]}` comes back as `{"attachments":null}`. That last one is the
  trap — Go's `Attachments()` returns a *nil* slice, and a nil Go slice marshals as `null`.

The ids come from `NewId()`, so the oracle records the output with every id absent from the input
replaced by `<generated>` and counts them separately; the Rust test applies the same substitution
and additionally asserts each minted id passes `is_valid_id`. Recording the raw ids would have
broken the determinism rule [D-032] exists for.

One divergence came out of the rewrite and is logged separately as [D-048].

---

## D-036 · `Post::clone` copies more than Go's `ShallowCopy`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post.go` chunk 1)

`(*Post).ShallowCopy` (post.go:357) assigns all 25 fields and deep-copies exactly **one**:

```go
if o.IsFollowing != nil { dst.IsFollowing = new(*o.IsFollowing) }
```

Everything else that is a reference is aliased. Measured — mutating the clone writes through to
the original for `Props`, `FileIds`, `Participants` and `Metadata`; `RemoteId` is the same
pointer; only `IsFollowing` is independent. Rust's `Clone` owns its values, so ours is
independent throughout.

Third instance of this class after [D-015] (`Channel::deep_copy`) and [D-034]
(`PostMetadata::Copy`), accepted for the same reason: reproducing the aliasing means
`Arc<Mutex<…>>` on four fields to make a footgun faithful. `clone_diverges_from_gos_aliasing_by_
design` asserts Go's aliasing flags **and** our independence side by side, so the divergence is
pinned rather than assumed.

Flagged because the hazard is directional: an app-layer call site that clones a post, mutates the
clone's props and then reads the original would silently change behaviour. Check for that when
the app layer lands. `ShallowCopy`'s other observable, `error("dst cannot be nil")` on a nil
destination, is unreachable in Rust and is pinned in the oracle rather than ported.

**Widened 2026-08-14** by `post_list.go`. `(*PostList).Clone` deep-copies its posts — measured,
so this is not the same aliasing — but copies `HasNext` as a bare `*bool`, so writing through the
clone writes through to the original. Ours is an `Option<bool>` and is independent.
`clone_matches_go` asserts Go's aliasing flag **and** our independence side by side, the same
treatment this entry's own test gets. Fourth instance of the class, after `Channel::deep_copy`
([D-015]) and `PostMetadata::Copy` ([D-034]); `PostList::extend` and `PostList::add_post` are a
fifth and sixth, where Go files the caller's post *pointers* into the receiver and we copy.

---

## D-037 · `SlackCompatibleBool` matches Go's raw token; we match the decoded string

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `slack_compatibility.go`)

Go's `(*SlackCompatibleBool).UnmarshalJSON` (slack_compatibility.go:35) lowercases the **raw JSON
token** and compares it against four literals:

```go
value := strings.ToLower(string(data))
switch value {
case "true", `"true"`:  *b = true
case "false", `"false"`: *b = false
default: return fmt.Errorf("unmarshal: unable to convert %s to bool", data)
}
```

Because it sees the raw bytes, a string spelled with escapes does not match. Measured:
`"\u0074rue"`, `"tr\u0075e"`, `"\u0054RUE"` and `"fals\u0065"` are all **rejected** by Go,
though every one of them decodes to `true` or `false`.

Serde hands a visitor the **decoded** string, so this port accepts all four. Reproducing Go would
mean deserialising through `serde_json::value::RawValue` — which needs the `raw_value` feature,
ties the type to serde_json specifically, and complicates every containing struct — to make a
pathological input fail in the same way. No client library emits a boolean word spelled with
unicode escapes.

Accepted rather than open, and pinned rather than skipped:
`slack_compatibility::go_parity::unmarshal_matches_go` asserts the divergence explicitly on those
four cases, so if the decision is ever revisited the test says so.

**Not a divergence, and the more surprising half of this type:** the case-insensitivity applies
only to the *quoted* form. `TRUE` unquoted is rejected — not by `UnmarshalJSON`, which would
accept it, but by `encoding/json`'s scanner, which never calls the unmarshaler for an invalid
token. `"TRUE"` is accepted. Both languages agree here, for the same reason, and
`only_the_quoted_form_is_case_insensitive` pins it.

---

## D-038 · `PostAction::Equals` ignores three fields, and panics on a nil option

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `integration_action.go` chunk 1)

Two separate problems in `(*PostAction).Equals` (integration_action.go:272), both reproduced.

**It never compares `Tooltip`, `Disabled` or `Style`.** It walks Id, Type, Name, DataSource,
DefaultOption, Cookie, the Options list and the Integration — and stops. Measured: two actions
differing only in `Style` (`primary` vs `danger`) are equal; likewise `Disabled: false` vs
`Disabled: true`. Almost certainly fields added to the struct without updating `Equals`, the same
shape as [D-034] on `PostMetadata::Copy`.

**Do not "fix" it.** `Equals` gates whether an interactive-message update is treated as a change;
a Rust server that compared the three extra fields would diverge from the Go server on the same
data. `post_action_equals_matches_go` pins all three as *equal*, so if upstream repairs it the
test fails, which is the signal we want.

**Separately, Go panics on a nil option element.** After the length check it indexes
`p.Options[k].Text` with no nil guard, so `Options: []*PostActionOptions{nil}` crashes — measured
under `recover`, on the receiver, the input and both. `PostAction.IsValid` handles the same input
politely with `select action contains nil option`, so the two disagree about whether a nil option
is survivable.

Our `Vec<PostActionOptions>` cannot hold a nil, so the crash is unreachable and the `IsValid`
branch is dead. That is the standing [D-033] convention (`[]*T` → `Vec<T>`), and it is asserted
rather than skipped: `post_action_is_valid_matches_go` requires those two corpus cases to fail at
**decode** time, and `equals_panics_in_go_on_a_nil_option` records Go's panic. The exposure is the
usual D-033 one — a client posting `{"options":[null]}` gets a 400 from us and a 500 from Go.

---

## D-039 · `MessageAttachment`'s `any` fields validate a Go type JSON cannot express

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `message_attachment.go`)

`MessageAttachment.Timestamp` and `MessageAttachmentField.Value` are bare `any`s, and their
validators switch on the **Go dynamic type**:

```go
switch s.Timestamp.(type) { case string, int64: /* valid */ }   // message_attachment.go:95
switch s.Value.(type)     { case string, int:   /* valid */ }   // message_attachment.go:206
```

`encoding/json` decodes every JSON number into a `float64`, and **neither validator accepts
`float64`**. So `{"ts": 123}` and `{"fields":[{"value": 123}]}` are *invalid* when they arrive
over the wire, while the same structs built in Go code with an `int64`/`int` are valid. Measured
in both directions; the wire direction is the one a server takes, and it is the one this port
reproduces exactly.

**What we cannot reproduce** is the Go-native direction: a `serde_json::Value` has one number
type, so `is_valid` cannot distinguish an `int64` an app-layer caller built from the `float64` a
decode produced. Nothing in the ported tree constructs an attachment other than by decoding, so
this is currently unreachable. If the webhook path is ported and starts building attachments in
Rust, revisit — a Rust caller setting `Value::Number` gets a rejection where Go's `int64` would
pass.

Almost certainly an upstream bug rather than a design: no client can send a valid numeric `ts`.
Not "fixed" here, for the usual reason — a Rust server accepting `{"ts":123}` would accept a
payload the Go server rejects.

**Separately, `MessageAttachmentField.Equals` panics whenever either `Value` is nil.**

```go
if reflect.ValueOf(input.Value).Type().Comparable() && ...   // message_attachment.go:222
```

`reflect.ValueOf(nil)` is the zero `reflect.Value`, and calling `Type()` on it panics. A field
with no `value` key decodes to exactly that, so comparing two ordinary attachments crashes the Go
server. Measured under `recover` on the receiver, the input, and both. Ours compares
`Value::Null` normally — a divergence that replaces a crash, the same class as [D-018].

**Widened 2026-08-14** by `post.go` chunk 2: `(*Post).AttachmentsEqual` calls straight into that
panicking comparison, so the crash is reachable from a *post*, not only from two attachments an
app-layer caller happened to hold. Two of the twenty corpus pairs panic in Go — one where a field
carries no `value` key at all, which is the ordinary shape. Ours answers (`true` and `false`
respectively) and `equals_answers_where_go_panics` records both, so the divergence is measured
rather than skipped.

**One more, and it is the reason `json_values_equal_like_go` exists.** Go compares these fields
with `==` on two `any`s. Both sides of a real comparison came from JSON and are therefore both
`float64`, so `1` and `1.0` and `1e2` and `100` all compare **equal**. serde_json keeps integers
and floats apart, so a plain `Value == Value` would disagree with Go on any integral number
written with a decimal point or an exponent. `utils::json_values_equal_like_go` normalises
numbers through `f64` and is used by `MessageAttachment::equals`,
`MessageAttachmentField::equals` and `PostAction::equals`.

---

## D-040 · Go's `encoding/json` matches keys case-insensitively; serde does not

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post.go` chunk 2)

`encoding/json` falls back to a **case-insensitive** match when no field carries the exact JSON
name (`{"Title":"t"}` populates `Title`, and so does `{"TITLE":"t"}` and `{"tItLe":"t"}`). serde
matches the `rename` string byte-for-byte and treats anything else as an unknown field, which it
silently ignores.

Measured through `Post::attachments`, which is where client-supplied JSON gets decoded into a
model type: Go reads `{"Title":"t","TEXT":"x"}` as a populated attachment, we read it as an empty
one. `case_insensitive_keys_are_go_only` asserts both halves.

**This is crate-wide, not an attachment problem.** Every `Deserialize` in `mm-model` has it. The
exposure is a client — or a Go server writing into the shared database — that spells a key with
different casing: Go honours the value, we drop it, and the two disagree about a record neither
rejected.

**Options**
- **(a) `#[serde(alias = …)]` per field.** Only fixes the spellings enumerated, and the reachable
  set is every casing of every key. Not tractable by hand.
- **(b) A case-insensitive deserializer** (a `Visitor` lowercasing keys before matching, or
  `serde_json::Value` preprocessing at the crate boundary). Faithful, and it is one helper rather
  than a per-type change — but it must lowercase with [`utils::go_to_lower`], and Go's own rule is
  a *simple ASCII-ish fold* rather than full Unicode case folding, so the helper needs its own
  oracle before it can be trusted.
- **(c) Leave it.** Current state. Real clients emit the documented casing; the risk is bespoke
  integrations and hand-written webhook payloads.

**(c) for now.** Revisit at the API layer, where one boundary-level decoder could cover every
type at once — which is the argument for doing it there rather than in `mm-model`.

---

## D-041 · `AllStrings` covers everything except the interactive blocks

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `post.go` chunk 2)
**Closed** 2026-08-14 (phase 1, `post_interactive_blocks.go`) — same day it was raised.

`(*Post).AllStrings` (post.go:806) takes an `AllStringsOptions{OmitInteractiveBlocks bool}` and
ends with `appendHumanReadableInteractiveStrings`, which walks `props.mm_blocks`, `props.blocks`
(Block Kit) and `props.cards` (Adaptive Cards). That walker is all of
`post_interactive_blocks.go`, which is unported.

What shipped is `Post::all_strings_omitting_interactive_blocks`, exact against Go for
`OmitInteractiveBlocks: true` over all 45 corpus cases. It is named for the half it omits, the
same way `User::pre_save_partial` is — not because a caller could store a plaintext password, but
because the missing strings feed mention checks and search indexing, so a caller mistaking it for
`AllStrings` would silently under-index every post carrying an interactive payload.

`AllStringsOptions` itself is deliberately **not** ported: a struct whose only field is honoured
in one of its two positions is worse than no struct.

**How it was paid.** `post_interactive_blocks.go`'s three human-string walkers are ported in
`crates/mm-model/src/post_interactive_blocks.rs`, and the method is now
`Post::all_strings(AllStringsOptions)` — both option values, exact against Go over the 45 cases
in `behaviour_post_attachments.json` plus 51 new interactive-tree cases.

The gap assertion was inverted rather than deleted:
`the_interactive_half_is_the_only_difference_between_the_options` still requires that exactly the
four payload-carrying cases differ between the options, that the `omitting` answer is a **prefix**
of the `full` one, and that both match Go — so the walkers cannot regress silently and the
append-last ordering stays pinned. `the_interactive_half_of_all_strings_is_no_longer_a_gap` in the
new module re-runs the four cases that recorded the gap against the full answer.

What did **not** come with it is the id-collection half of the same Go file; that is [D-044].

---

## D-042 · `propsIsValid` and `ValidateProps` are still unported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `post.go` chunk 2)
**Depends on** [D-044] (the markdown parser) and `ValidateMmBlocksActions`
(integration_action.go:1103) — i.e. integration_action.go chunk 2.
**Narrowed** 2026-08-14 (`post_interactive_blocks.go`): the walkers are ported, so the only
missing pieces are the ones [D-044] describes.

`propsIsValid` (post.go:909) is ~150 lines of independent per-prop branches, all of whose
dependencies (`IsValidId`, `IsValidHTTPURL`, `MessageAttachment::IsValid`, `MultiError`) have
landed — **except two**, and both are load-bearing:

- `ValidateMmBlocksActions(o)` pulls in `CollectInteractiveActionIDsFromPost`,
  `mmBlocksEntryMapToSpec`, `validateIntegrationURL`, `validateOpenURL`, `ValidateActionQuery`
  and `validateMmBlocksActionsPairing`. The first of those is [D-044].
- `nonEmptyInteractivePayloadPropKeys` needs `interactivePropJSONArrayNonEmpty`, which is four
  lines and was left out of the `post_interactive_blocks.go` session for a reason worth
  recording: both functions are unexported, so **no exported Go function reaches them** and the
  oracle cannot measure either one. They are portable, but only against a reading of the source
  — so they land with `propsIsValid`, whose own oracle case will exercise them end to end.

Shipping the rest without them was considered and rejected. `CollectInteractiveActionIDsFromPost`
scans the post **Message** for `mmaction://` links, so a plain text post carrying one is invalid
in Go and would be valid for us — a divergence reachable by an ordinary message, not by a crafted
payload. `propsIsValid` accumulates a `*multierror.Error`, so a missing branch is a missing
message in a list whose count and order are the whole output.

**To pay off** port `post_interactive_blocks.go` and integration_action.go chunk 2 first, then
translate `propsIsValid` whole. `ValidateProps` is a one-line wrapper that logs the result — it
lands with the logging layer and reduces to `if let Err(e) = self.props_is_valid()`.

---

## D-043 · Absent JSON keys must zero-fill, and 14 of 75 types say so

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post.go` chunk 2)

Go's `encoding/json` leaves an absent field at its zero value; serde's derived `Deserialize`
**errors** with `missing field` unless the field or the container carries `#[serde(default)]`. So
any partial object a client sends is a 400 from us and a 200 from Go.

Found by feeding `Post` the oracle's corpus, which is written the way a client writes a post:
`{"channel_id":"c","message":"hi"}` failed with `missing field 'id'`. That is the **create-post
payload** — the single most common inbound document in the product.

`Post` is fixed (container-level `#[serde(default)]`, pinned by
`a_partial_post_decodes_the_way_go_zero_fills`). The audit is what is owed. Counted at the time
of writing: **75** structs in `mm-model` derive `Deserialize`, and **14** carry a container-level
default — `CustomStatus`, `Emoji`, `FileInfo` (both), `Post`, `PostAcknowledgement`, `PostEmbed`,
the four in `post_metadata.rs`, `Preference`, `Reaction` and `Status`.

Of the 61 that do not, three groups are already safe and should not be touched:
`#[serde(transparent)]` newtypes over a `Vec` or map (`ChannelList`, `ChannelListWithTeamData`,
`Preferences`, `RecentCustomStatuses`), types whose every field is an `Option` (`PostPatch`,
`ChannelPatch`, `UserPatch`, `TeamPatch`), and `MessageAttachment`/`MessageAttachmentField`,
which carry the attribute per field instead. The rest — `Channel` and its satellites,
`ChannelMember` and its, `User`, `Team`, `Session`, `TeamMember` and the ten in
`integration_action.rs` — reject a partial document that Go accepts.

**One field paid 2026-08-14** (`mm_blocks_actions.go`), and it is worth recording because it was
found the way the entry predicts: `MmBlocksActionCookie.actions` was the one field in that struct
without a per-field `default`, so a cookie written without an `actions` key — which
`ParseDecryptedActionCookiePayload` has to decode, and which Go zero-fills — failed. The other 60
containers are untouched.

**To pay off** add `#[serde(default)]` to each container that derives `Default`, and add a decode
test per file asserting the minimal realistic payload. Cheap and mechanical, but it touches every
already-shipped module, so it wants its own session rather than being smuggled into the next
translation. Nothing detects the gap today except a test that tries a partial document — the
round-trip fixtures are all **complete** objects, which is precisely why this survived nine files.

---

## D-044 · The `mmaction://` id scan needs `shared/markdown`

**Status** OPEN · **Severity** blocking · **Raised** 2026-08-14 (phase 1, `post_interactive_blocks.go`)
**Blocks** [D-042] (`propsIsValid`), and with it `ValidateMmBlocksActions`,
`RefreshInteractiveActionsOnPost` and the interactive-webhook path.

`appendMmactionIDsFromText` (post_interactive_blocks.go:385) is four lines and one of them is
`markdown.Inspect`:

```go
markdown.Inspect(text, func(blockOrInline any) bool {
    switch v := blockOrInline.(type) {
    case *markdown.InlineLink:    ids = appendMmactionIDFromURL(ids, v.Destination())
    case *markdown.ReferenceLink: … case *markdown.Autolink: …
    }
    return true
})
```

So finding the action ids a post references means **parsing the post's markdown** —
`server/public/shared/markdown` is 4,688 non-test lines across 20 files (CommonMark blocks,
inlines, links, reference definitions, autolinks, HTML entities). It is a package-sized
translation and it is the fourth "Go's stdlib-shaped dependency does the real work" case after
`net/mail`, `x/text/language` ([D-001]) and `net/url` ([D-003]).

**Everything downstream is deferred as a unit**, which is the point of this entry:
`CollectMmBlockActionIDs`, `CollectBlockKitActionIDs`, `CollectAdaptiveCardActionIDs`,
`CollectInteractiveActionIDs`, `CollectInteractiveActionIDsFromPost`, `CollectMmactionIDsFromText`,
`RefreshInteractiveActionsOnPost`, `ApplyMmBlocksWithActionsToProps`,
`validateMmBlocksActionsPairing`, `ValidateInteractiveActionsForWebhook` and
`ValidateMmBlocksActionsForWebhook`. Also `SubsetMmBlocksActions` and `interactiveControlDisabled`,
which are markdown-free but have no other caller.

**Porting the collectors without it was considered and rejected.** Every one of them walks text
nodes as well as controls — a `text` block, a Block Kit `section`'s text, an Adaptive Card
`TextBlock` — so a stubbed scanner returns a *subset* of the referenced ids. That subset flows
into `validateMmBlocksActionsPairing`, which then reports `mm_blocks_actions entry "x" is not
referenced by interactive content` for an entry that **is** referenced, rejecting a payload the Go
server accepts. Under-reporting is the dangerous direction here, and it is silent.

**Options**
- **(a) Port `shared/markdown`.** It is needed eventually regardless — the mention parser, the
  image-proxy rewriter (`RewriteImageURLs`) and the notification path all use it. Its own session,
  or several.
- **(b) Port only the link-destination scan** — a much smaller parser that finds `[x](dest)`,
  `<dest>` and reference definitions. Tempting, and wrong for the usual reason: "which text is a
  link" is exactly the question CommonMark's block/inline structure answers, and a scanner that
  disagrees inside code spans, fenced blocks or nested brackets reports different ids.
- **(c) Leave the whole family unported.** Current state.

**(c) until the markdown port is scheduled**, then (a). `appendMmactionIDFromURL` — the pure
string half, splitting the id off at the first `/`, `?` or `#` and matching
`^[A-Za-z0-9_-]+$` — is *not* ported either, because it cannot be tested in isolation: it is
unexported and reachable only through the parser, so any test of it would assert our reading of
the Go source rather than Go's answer.

---

## D-045 · The two `column_set` walkers disagree, and the image one is wrong

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_interactive_blocks.go`)

`appendHumanStringsFromMmBlockMap` (post_interactive_blocks.go:73) hands a column's whole `items`
array to the block walker. `appendMmBlockMapImageURLs` (:236) hands it **each element**, which the
same walker then re-tests as an array:

```go
for _, item := range colItems {
    out = appendMmBlocksArrayImageURLs(out, item)   // item, not colItems
}
```

Measured: for the ordinary shape `{"type":"column_set","columns":[{"items":[{"type":"image",
"url":"…"}]}]}` the text walker finds the column's contents and the image walker finds
**nothing**; an image only surfaces when `items` is an array *of arrays*, which no producer emits.
So images inside mm_blocks columns are invisible to link previews in the Go server today.

Reproduced verbatim and pinned by `the_two_column_set_walkers_disagree_the_way_go_does`, which
asserts the empty result for the flat shape, the found URL for the nested one, and the text
walker's answer for the same input side by side.

**Do not "fix" it.** A Rust server that found the image would fetch and attach a preview the Go
server never attaches, and the two would disagree about the metadata written for the same post.
If upstream repairs the loop, the oracle case flips and the test fails — the signal we want. Same
treatment as [D-016] and [D-019].

---

## D-046 · integration_action.go's crypto half is unported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `integration_action.go` chunk 2)
**Blocks** the interactive-message request path, not the model types.

Two clusters were left out of chunk 2 because neither is a translation problem — both are
cryptographic compatibility problems, and getting one subtly wrong fails open rather than loudly:

**Trigger ids** (`GenerateTriggerId` :636, `DecodeAndVerifyTriggerId` :664, plus the two method
wrappers). An ECDSA signature over `userId:timestamp`, base64-encoded, verified against a
timeout. Go signs with `crypto.Signer` and a SHA-256 digest; reproducing it means matching the
**signature encoding** (ASN.1 DER over the P-256 curve) and the exact digest input, since the
same key pair has to verify tokens minted by either server during the migration.
`signForGenerateTriggerId` also wraps the signing call in a `recover`, because an invalid signer
panics inside `crypto` — a Rust port has no panic to catch and should return an error instead.

**Post action cookies** (`AddPostActionCookies` :1261, `EncryptPostActionCookie` :1307,
`DecryptPostActionCookie` :1337). AES-GCM over a JSON `PostActionCookie`, keyed by the server's
`AtRestEncryptKey`, with the nonce prepended and the whole thing base64-encoded. Both servers
read the *same* posts, so a cookie written by Go must decrypt in Rust and vice versa: nonce
length, the associated data (there is none) and the base64 alphabet are all load-bearing.

Also deferred, and *not* crypto: `StripActionIntegrations` (:1044), `GetAction` (:1057) and
`GenerateActionIds` (:1246). All three walk `props.attachments` and rewrite it, which is now
possible — `Post::attachments` landed with chunk 2 of post.go — so they are the natural next
chunk. `GenerateActionIds` is what [D-035] is waiting on, and `Post::to_json`/`encode_json` wait
on `StripActionIntegrations`.

**Two of those three landed 2026-08-14** as chunk 3: `strip_action_integrations` closed the
`to_json`/`encode_json` deferral and `generate_action_ids` closed [D-035]. `GetAction` did not —
it needs `MergeQueryIntoURL`, which is a `net/url` port rather than a crypto one. See [D-047].

**To pay off** the crypto needs a decision on crates (`p256`/`ecdsa` and `aes-gcm`, or `ring`) and
an oracle that records Go's *ciphertext* for a fixed key and nonce — a round-trip test in Rust
alone would prove nothing about cross-server compatibility.

---

## D-047 · `Post::get_action` needs `MergeQueryIntoURL`, i.e. a `net/url` parser that re-emits

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `integration_action.go` chunk 3)
**Closed** 2026-08-14 (phase 1, `net/url` + `mm_blocks_actions.go`) — same day it was raised.
**Related** [D-003] (`IsValidHTTPURL`, which reproduces `ParseRequestURI` as a *validator*)

Chunk 3 shipped two of the three `Post` methods that walk `props.attachments`.
`(*Post).GetAction` (integration_action.go:1057) is the third and it did not, because its second
half is not a translation problem:

```go
if spec := o.GetMmBlocksActionSpec(id); spec != nil && spec.Type == MmBlocksActionTypeExternal && spec.URL != "" {
    url := spec.URL
    if len(spec.Query) > 0 {
        merged, err := MergeQueryIntoURL(spec.URL, spec.Query)   // mm_blocks_actions.go:148
        if err != nil { return nil }
        url = merged
    }
    ...
}
```

`MergeQueryIntoURL` is `url.Parse` → `u.Query()` → `values.Set` → `values.Encode()` → `u.String()`.
Four pieces of `net/url` and only the first overlaps with what [D-003] already built:

- **`url.Parse` is not `url.ParseRequestURI`.** It accepts a relative reference and it *does*
  split a `#fragment`, which `ParseRequestURI` does not — the one behaviour D-003's notes single
  out as a call-site trap. And `is_valid_http_url` answers a bool; this needs the **components**.
- **`Values.Encode` sorts by key and percent-encodes with `QueryEscape`**, which is not the same
  escape set as a path or a host — a space becomes `+`, not `%20`.
- **`URL.String()` re-assembles with its own escaping rules per component**, so a round trip is
  not the identity: it can normalise the input URL even when nothing was merged into it.

Shipping the attachment half alone and returning `None` for the mm_blocks half was considered and
rejected: that is the under-reporting direction, and it turns a working external action into a
404 rather than into a visible failure.

**Also deferred with it, and the same session's work:** the rest of `mm_blocks_actions.go` — the
`MmBlocksActionSpec` type, `GetMmBlocksActionSpec`, `mmBlocksEntryMapToSpec`,
`MmBlocksActionCookie::ActionSpec`, `ResolveMmBlocksAction`, `MmBlocksContextMap`,
`contextMapFromProp`, `stringMapFromPropValue` and `coerceToStringAnyMap`. All of those are pure
`map[string]any` coercion and could land today; they buy nothing without `GetAction`, so they wait
for it rather than being smuggled in one at a time.

**One method of that file did come across**, because `StripActionIntegrations` calls it and
shipping that without it would leak the `context` of every mm_blocks action to the client:
`(*Post).StripMmBlocksActionSecrets` (mm_blocks_actions.go:243) is ported in
`integration_action.rs` and pinned over all 34 corpus cases. `AddMmBlocksActionCookies` and
`ParseDecryptedActionCookiePayload` stay with the crypto in [D-046].

**To pay off** port `net/url`'s `Parse`/`String`/`Values` as a unit — it is the same shape of job
D-003 was, and an oracle recording Go's `String()` for a corpus of inputs is what makes it
checkable — then `GetAction` and the rest of `mm_blocks_actions.go` are mechanical.

**How it was paid.** `crates/mm-model/src/go_url.rs` is `net/url`'s `Parse`, `ParseRequestURI`,
`URL.String`, `EscapedPath`/`EscapedFragment`, `escape`/`unescape`, `ParseQuery` and
`Values.Encode`. `crates/mm-model/src/mm_blocks_actions.rs` is the rest of the Go file bar
`AddMmBlocksActionCookies`, which stays with the crypto in [D-046]. `Post::get_action` is ported
and pinned over all 44 corpus cases, asserting the **marshalled** synthesised action rather than
its fields.

**The strongest evidence is not the new corpus.** `utils::is_valid_http_url` was a hand-written
predicate reproducing `ParseRequestURI`'s grammar, verified over 3,529 inputs including four
exhaustive 0..127 byte sweeps. It is now two lines delegating to `go_url::parse_request_uri`, and
**every one of those 3,529 cases still passes, unchanged**. A corpus built to check a predicate
turned out to be a much better test of the parser underneath it. The 200-odd lines of duplicated
grammar in `utils.rs` are deleted; there is one implementation now.

Two things the new oracle caught that a reading would not have:

- **`escape` differs per position on ~30 bytes**, and the fixture runs all 256 byte values through
  all six reachable modes rather than sampling. `encodeFragment` leaves `!()*` alone and escapes
  `'`; `encodePath` escapes only `?` out of the reserved set; `encodeHost` allows `<>"`.
- **`URL.String()` is not the identity on its input.** `http://x/a%41b` comes back as
  `http://x/aAb` because the escaping is canonicalised, while `http://x/a%2fb` survives — the
  difference is `RawPath`, which `setPath` populates *only* when the default encoding differs.

One divergence came out of it and is logged separately as [D-049].

---

## D-048 · A rewritten `props.attachments` loses Go's struct field order

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `integration_action.go` chunk 3)
**Related** [D-027] (which made `StringInterface` sorted *because* Go sorts map keys)

`StripActionIntegrations` and `GenerateActionIds` both store the decoded attachment list back
into `props.attachments`. Go stores a native `[]*MessageAttachment`, so `json.Marshal` later emits
each element in **struct declaration order** — `id`, `fallback`, `color`, … , `ts`, `actions`.
Our props map holds `serde_json::Value`, and a `serde_json::Map` absent the `preserve_order`
feature is a `BTreeMap`, so the same element comes out **sorted** — `actions`, `author_icon`,
`author_link`, … .

The two documents are equal; the bytes are not.

This is the mirror image of [D-027]. There, `StringInterface` was changed from a `HashMap` to a
sorted map *because* Go sorts the keys of a `map[string]any`. Here Go is marshalling a **struct**
through an `any`-typed field, which is the one shape where Go does not sort — and it is not
reachable from JSON, so no round-trip fixture could have caught it. The `strip_action_integrations`
oracle section is what did.

**Options**
- **(a) Enable serde_json's `preserve_order`.** Makes `serde_json::Map` an `IndexMap` and would
  fix this case by insertion order — and would simultaneously *break* every ordinary prop, which
  Go sorts and we currently sort for free. Strictly worse.
- **(b) Hold an order-preserving representation for this one prop.** There is no such value type;
  it would mean not storing a `Value` at all, i.e. a parallel typed field on `Post` shadowing
  `props.attachments`, with every reader having to check both.
- **(c) Leave it.** Current state.

**(c).** JSON object key order carries no meaning, no Mattermost client depends on it, and the
one place the project *measures* Go's bytes rather than sending them — `Post::is_valid`'s
800,000-rune props cap — counts characters, which reordering does not change.

Pinned rather than shrugged off: `the_rewritten_attachments_differ_from_go_only_in_key_order`
asserts that the bytes differ, that Go's start with `"id":0,"fallback":"` and ours with
`"actions":[`, and that the parsed values are equal. If a future change closes the gap the test
fails, which is the signal we want. The corpus assertions fall back to a `serde_json::Value`
comparison for exactly the cases carrying a rewritten list, and stay byte-for-byte everywhere
else — including the HTML-escaping case, which is what proves `to_json` uses the right marshaller.

---

## D-049 · `go_url`'s error text is not Go's, and the query-parameter cap is not ported

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `net/url`)

Two deliberate gaps in `crates/mm-model/src/go_url.rs`, both recorded rather than reproduced.

**The error messages.** `UrlParseError` is a typed enum whose `Display` approximates Go's, but it
is not asserted against it and one variant is knowingly wrong: Go wraps `netip.ParseAddr`'s own
error for a bad IP-literal (`invalid host: ParseAddr("abc"): unable to parse IP`) and ours emits
that shape with a fixed reason. Reproducing `netip`'s wording means porting `netip`'s parser
error taxonomy, which is a package away from anything Mattermost calls.

Nothing in the ported tree reads one of these strings. `IsValidHTTPURL` discards the error;
`MergeQueryIntoURL` wraps it and `GetAction` turns the wrapped value into a `None`;
`ResolveMmBlocksAction` returns it to an app layer that does not exist yet. So the oracle records
Go's text as a **diagnostic** and every test asserts *whether* a parse failed, not what it said —
which is the same treatment [D-003]'s fixture gave its `parse_error` column, and for the same
reason.

Revisit if an error string ever reaches a client. It would then be wire surface, and the fixture
already holds Go's answer for 102 inputs to check against.

**The 10,000-parameter cap.** `parseQuery` (net/url/url.go:957) rejects a query with more than
`defaultMaxParams` settings, and the limit is a `godebug` knob (`urlmaxqueryparams`) rather than a
parse rule — a deployment can raise, lower or disable it at runtime. Ours has no limit. The
divergence needs a query with more than 10,000 `&`-separated settings to observe; Go returns an
error and keeps *no* pairs, we would return all of them.

Accepted rather than open because reproducing a runtime-tunable Go knob means inventing a Rust
equivalent and a way to configure it, which is a decision for the API layer rather than for
`mm-model`. Flagged because it is the one place `go_url` is knowingly more permissive than Go.

---

## D-050 · `MmBlocksActionCookie.actions` cannot hold a nil entry

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `mm_blocks_actions.go`)
**Related** [D-033], of which this is one more instance

Go's field is `map[string]map[string]any`, so `{"actions":{"a1":null}}` decodes with `a1` present
and nil — which is exactly what `ActionSpec`'s `entry == nil` guard is written for. Ours is
`Option<BTreeMap<String, StringInterface>>`, and `StringInterface` cannot be null, so the whole
cookie fails to decode.

This is the D-033 convention applied to a **map value** rather than a slice element, and the
exposure is the same shape: a document Go accepts is a decode failure for us. Reachability is
lower than D-033's, because the only writer is `AddMmBlocksActionCookies`, which builds the map
from `coerceToStringAnyMap` and therefore never stores a nil.

Listed rather than fixed for D-033's stated reason: the value of the current state is that it is
uniform across the crate, and whatever is chosen must be applied to every `[]*T` and
`map[string]*T` at once. `ActionSpec`'s nil-entry branch is consequently dead code in the Rust
port, and the doc comment says so at the site.

---

## D-051 · `SortByCreateAt` uses Go's **unstable** sort, and `order` is on the wire

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_list.go`)

`(*PostList).SortByCreateAt` (post_list.go:169) sorts `Order` with `sort.Slice`, which is
explicitly documented as "not guaranteed to be stable". `Order` is a wire field, so the
permutation Go picks among posts sharing a `create_at` is observable by every client.

Measured across five tie corpora:

| input | Go's `Order` out | a stable sort |
|---|---|---|
| 2, 3, 5 all-tied | input order | same ✓ |
| 4 elements, two tie groups | `b d a c` | same ✓ |
| 13 all-tied, 20 all-tied | input order | same ✓ |
| **20, two tie groups interleaved** | `s15 s1 s19 s3 s17 s5 s13 s7 s9 s11 s12 s8 s10 s6 s14 s0 s16 s4 s18 s2` | `s1 s3 … s19 s0 s2 … s18` ✗ |

Below thirteen elements `sort.Slice` runs insertion sort and is stable in practice; above it,
pdqsort's partitioning scrambles ties. The all-tied cases at 13 and 20 still agree because an
already-sorted input short-circuits — the divergence needs both a long list and interleaved keys,
which is what a real channel of posts looks like the moment two posts share a millisecond.

**The Rust port uses `sort_by_key` (stable) and diverges.** Reproducing Go's answer means
reimplementing `sort.Slice`'s pdqsort — pivot selection, `breakPatterns`, the partial-insertion
fallback and the depth limit — bit for bit, and then keeping it pinned to whatever the Go
runtime does next. That is a large, brittle amount of code to reproduce an ordering Go itself
calls arbitrary.

Accepted rather than open, with two things that bound the damage: both orderings are *correct*
sorts (the `create_at` sequence is identical, which the test asserts), and the only in-model
caller is `BuildWranglerPostList`, whose consumer is the move-thread feature rather than the
channel view. `an_unstable_go_sort_scrambles_ties_above_twelve` asserts the divergence explicitly
— including that Go's answer still differs from ours — so if upstream switches to `sort.SliceStable`
the test fails and this can be closed.

**Revisit** if a ported endpoint ever returns `Order` straight out of this function to a client
that compares it against the Go server's.

---

## D-052 · Three `PostList` methods return where Go panics

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_list.go`)
**Related** [D-018], which is the same call on `ChannelMember`

`CLAUDE.md` forbids `panic!` in library code, and three of `post_list.go`'s methods reach one on
input the public API can produce. All three are measured under `recover` in
`fixtures/behaviour_post_list.json`, so the Go answer is a crash rather than an inference.

| Go | why it panics | ours |
|---|---|---|
| `AddPost` (post_list.go:132) | assigns into `BurnOnReadPosts` without a nil check, and the field is `json:"-"` — so it is nil on **every decoded list** | creates the map |
| `SortByCreateAt` (post_list.go:169) | the comparator dereferences `o.Posts[id]` for an order id with no post | treats the missing post as `create_at: 0` |
| `BuildWranglerPostList` (post_list.go:207) | reads `p.UserId` off the nil element `ToSlice` returned | skips the element |

The first is the reachable one and it is not a corner case: `NewPostList` is the only constructor
that initialises `BurnOnReadPosts`, so any list that arrived over the wire and is then handed a
burn-on-read post crashes the Go server. The other two need an order id with no matching post,
which `AddOrder` produces without complaint.

Accepted for [D-018]'s reason: the divergence is only observable where the Go server returns a
500, and the alternatives are panicking (forbidden) or silently discarding a user's post. Each is
asserted in the parity tests rather than skipped — `add_post_matches_go` checks that the map was
nil *and* that we filed the post, so if upstream adds the nil check the test still passes and the
oracle row flips from `panicked: true` to a real answer.

---

## D-053 · `PostList::with_rewritten_image_urls` is unported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-14 (phase 1, `post_list.go`)
**Depends on** [D-044]

`(*PostList).WithRewrittenImageURLs` (post_list.go:79) is four lines over
`(*Post).WithRewrittenImageURLs` (post.go:1269), which calls `RewriteImageURLs` — a walk over
`shared/markdown`'s parsed document, the same 4,688-line dependency [D-044] is waiting on. It is
the only method in `post_list.go` left unported.

Its shape is worth recording now, because it is the **fourth** distinct copy semantic in the file
and the only one that is not `Clone`: it does `plCopy := *o`, so the copy shares `Order` and
`BurnOnReadPosts` with the original and gets a fresh `Posts` map — the same shallow-struct-copy
`ToJSON` does, rather than the nil-materialising `Clone` the other methods use. A port that
reached for `go_clone` here would materialise a nil `order` into `[]` and change the wire output.

**To pay off** close [D-044], port `RewriteImageURLs` and `Post::with_rewritten_image_urls`, then
this is four lines and one oracle section.
