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

**Narrowed again 2026-08-16** (`scheduled_post_recurrence.go`). The two `ScheduledPostRepeatType*`
constants were borrowed into `scheduled_post.rs` because `BaseIsValid`'s switch needs both and
their owning file was unported. They now live in `scheduled_post_recurrence::` and
`scheduled_post.rs` re-exports them, so both paths resolve and there is one definition. Same shape
as `CURRENT_VERSION` above; a third borrow paid off by translating its owner rather than by
adding a drift test.

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

**Third hazard behind the same fix, 2026-08-17.** [D-073] adds float rendering: three renderings
are live, they disagree on 10–12 of 29 measured values, and `serde_json::to_string` on an `f64` is
the wrong one. The `disallowed-methods` entry now covers `serde_json::to_string`,
`str::to_lowercase` and a bare `f64` serialization. It has been the recommended fix for three days
and is still unwritten; each new hazard makes it cheaper relative to the alternative.

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
**Widened** 2026-08-14 (`file_info.go`, then `post_search_results.go`)

`(*Emoji).Auditable` (emoji.go:29), `(*FileInfo).Auditable` (file_info.go:86),
`(*PostSearchResults).Auditable` (post_search_results.go:43) and `ChannelMember`'s are all
skipped for the same reason: audit projections are not wire types and belong with the audit
layer, which does not exist yet. `FileInfo`'s is a straight ten-key projection with no surprises.
`PostSearchResults`' is two keys and is the one place in that file the nil embed is handled
rather than dereferenced — port it with the guard intact, or it joins [D-054].

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

**Status** CLOSED · **Severity** blocking · **Raised** 2026-08-14 (licensing)
**Closed** 2026-08-17 (licensing) — chose **(b)**, the split, by the project owner.
**Blocked** every commit of code derived from `server/channels/` — i.e. all of phases 2 to 5.

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
to defer, taking Apache-2.0 "for now" with this entry as the tripwire. Revisited and settled
2026-08-17, ahead of any phase-2 work rather than at the moment of the first `mm-store` commit,
so the tripwire never had to fire.

**Options that were on the table:**
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

**Resolution — (b), the split.** (a) was cheaper and (b) preserves more: `mm-model` is the part
of this repository another project is most likely to reuse, it owes nothing to the AGPL half, and
collapsing it into an AGPL root would have given up the permissive terms for no gain. What landed:

| Change | Detail |
|---|---|
| Root `LICENSE` | Now the **verbatim** GNU AGPL v3.0 (extracted from `reference/mattermost/LICENSE.txt:237-897`, which carries the FSF text unmodified). Verbatim rather than upstream's preamble-plus-text arrangement, so licence detectors identify it. |
| `crates/mm-model/LICENSE` | The previous root `LICENSE`, moved with `git mv` — still byte-identical to upstream's `server/public/LICENSE.txt`. |
| `[workspace.package]` | `license = "AGPL-3.0-only"` — the default, inherited by `mm-store`, `mm-app`, `mm-api`, `mm-ws`. |
| `crates/mm-model/Cargo.toml` | **Overrides** back to `license = "Apache-2.0"`; no longer `license.workspace = true`. |
| `NOTICE`, `README.md` | Both restated for the split, including the one-way-compatibility rule below. |

**The rule this creates, and it is the part that can be got wrong later:** Apache-2.0 is one-way
compatible with AGPL-3.0, so the AGPL crates may depend on `mm-model` and the reverse must never
happen. `mm-model` cannot take code or a dependency from an AGPL crate or from
`server/channels/`. The existing architectural rule that `mm-model` has zero internal
dependencies already enforces it, but it is now a **licensing** constraint too, and a future
session that "just needs one type from `mm-store`" in `mm-model` would breach the licence rather
than merely the layering. A crate that starts consuming `server/channels/` drops its Apache
override; it never adds one.

**Deliberate:** the four AGPL crates carry `AGPL-3.0-only` while still holding zero AGPL-derived
lines. The label is a precondition for the first such commit, not a consequence of it — the whole
point of closing this entry ahead of phase 2 rather than during it.

**Not addressed here**, because neither is a licensing question: upstream's compiled-binary MIT
grant (we distribute source, not Mattermost, Inc.'s binaries) and the trademark position (already
stated in `NOTICE`, unchanged).

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
| `FileInfoList.FileInfos` `map[string]*FileInfo` | `Option<BTreeMap<String, FileInfo>>` | `file_info_list.rs` |
| `FileUploadResponse.FileInfos []*FileInfo` | `Option<Vec<FileInfo>>` | `file.rs` |
| `UserAutocompleteInChannel.{InChannel,OutOfChannel}` `[]*User` | `Option<Vec<User>>` | `user_autocomplete.rs` |
| `UserAutocompleteInTeam.InTeam []*User` | `Option<Vec<User>>` | `user_autocomplete.rs` |
| `UserAutocomplete.Users []*User` | `Option<Vec<User>>` | `user_autocomplete.rs` |
| `UserAutocomplete.{OutOfChannel,Agents}` `[]*User` **+ omitempty** | `Vec<User>` | `user_autocomplete.rs` — see below |
| `AnalyticsRows []*AnalyticsRow` | `Vec<AnalyticsRow>` | `analytics_row.rs` |

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
be applied to all the types above at once — the value of the current state is that it is uniform.

**One row in that table is a `Vec` rather than an `Option<Vec>` and it is not an inconsistency.**
`UserAutocomplete.{OutOfChannel,Agents}` carry `omitempty`, so Go drops a nil slice *and* an empty
one and the two are indistinguishable on the wire — an `Option` there would invent a distinction
Go cannot express. That is the general rule the crate follows and it is worth stating here because
this table makes the shapes look uniform when the *tags* are what decide: no `omitempty` →
`Option<Vec<T>>`, `omitempty` → `Vec<T>` with a length predicate. `user_autocomplete.go` is the
clearest case, because `out_of_channel` appears in two structs in the same file under different
rules. Option (a) would replace the element type in both, not the container.

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

**Widest instance measured 2026-08-17** (`channel_member_history.go`), and it also bounds the
rule. That file and `channel_member_history_result.go` carry **no `json:` tags at all**, so every
wire key is a Go field name in PascalCase — and Go's fallback means `channelid`, `CHANNELID` and
`cHaNnElId` all populate `ChannelId` there while none does here. Where the earlier instances cost
one field or one embed, this costs *every key of the type*, and the same is true of
`wrangler.go`, the other tagless type.

The bound is the useful half: the fallback folds **case only, not punctuation**. `channel_id` and
`channel-id` are unknown keys in Go too, so the divergent set for a key is exactly its
case-variants and not "any plausible spelling" — which is what makes option (b) a finite,
well-defined transformation rather than a guess. `only_the_declared_key_casing_decodes_here`
drives all six spellings and asserts which three Go accepts.

**Stated precisely 2026-08-17** (`team_stats.go`), because the bound above reads as a
counterexample and is not. `{"Total_Member_Count":5}` **does** populate `TotalMemberCount`, even
though `channel_id` failed to populate `ChannelId` one file earlier. Both are the same rule: Go
folds case against the field's **effective name**, which is the `json:` tag when there is one and
the Go field name when there is not. `total_member_count` is the tag, so it already contains the
underscores and `Total_Member_Count` folds onto it; `ChannelId` is a field name, and no
underscored spelling folds onto that.

So the divergent set for a key is the case-variants **of its effective name** — still finite,
still mechanical, but a boundary decoder has to fold against the tag rather than against the Rust
field identifier. `the_case_fold_is_against_the_tag_not_the_field_name` pins it, and it is worth
reading before implementing option (b).

**And the set's size depends on the tag's own casing, measured 2026-08-17** (`limits.go`). That
file tags everything camelCase — `maxUsersLimit` — which is a **third** naming convention after
snake_case and tagless PascalCase. It widens the exposure, because the Go *field name*
`MaxUsersLimit` is itself a case-variant of its tag, so Go accepts both spellings where a
snake_case tag admits no PascalCase spelling at all. Four of seven probed spellings diverge there
against three of six for a tagless type.

The reassuring half: `max_users_limit` — the spelling a Rust port invents by habit after sixty
snake_case files — populates the field on **neither** side, because the fold still does not cross
punctuation. So a mis-tagged field is a silent no-op rather than a silent mis-read, and a
comparison against Go's key list catches it. `the_key_casing_matches_go` drives all seven.

**A second entry now points at the same fix.** [D-071] (a repeated key takes the last value in Go
and fails the decode here) is the other crate-wide `encoding/json`-versus-serde decode
difference, and option (b) closes both: a boundary decoder that parses into a
`serde_json::Value` resolves duplicates for free, because `serde_json::Map` keeps the last value.
Neither entry justifies that machinery alone; together they do.

**Second measured instance 2026-08-16** (`file_info_search_results.go`), and it is worse than the
first because the casing decides a **structural** question rather than one field's value.
`{"ORDER":[]}` makes Go allocate the embedded `*FileInfoList` and set `order` on it, so the
response carries five keys; here it is an unknown key and the embed stays nil, so the response
carries one. Same for `PostSearchResults`, where the divergence is five keys too. A field-value
disagreement is a wrong value; this is a differently-shaped document.

Pinned by `uppercase_key_only` in both types' oracles, asserted as a divergence rather than
skipped. It strengthens option (b): a boundary decoder that folds keys once would fix the
structural case and the field case together, and there is no per-field alias that could.

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

**Confirmed again 2026-08-17** (`channel_data.go`), and the encounter is worth recording because
of how it was avoided rather than how it was hit. The `ChannelData` oracle's first draft wrote its
corpus as hand-written JSON literals — `{"channel":{}}`, `{"channel":{"id":"c1","update_at":1}}` —
which are perfectly good probes of Go and which the Rust port cannot decode at all, because
`Channel` and `ChannelMember` are both on the unfixed list above. Three parity tests failed with
`missing field`.

The fix was **not** to add the attribute to those two containers, which would be one file of a
61-file audit and would leave the crate more inconsistent than it is now. It was to build the
corpus from Go **values** and marshal them, so every document is complete. That is better oracle
design independently: what the wire format has to agree on is the document the Go *server* emits,
and a partial document tests D-043 rather than the file under translation. Worth copying — a
behaviour oracle for a type with nested model structs should marshal from values, not hand-write
JSON, until this entry is paid.

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

---

## D-054 · Three `PostSearchResults` methods panic on a nil embed

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_search_results.go`)
**Related** [D-052], which is the same call on `PostList`, and [D-018] on `ChannelMember`

`PostSearchResults` embeds a **pointer**, `*PostList`. `ToJSON`, `EncodeJSON` and `ForPlugin` all
dereference it without a nil check, so each crashes the Go server on a value the type's own
constructor produces — `MakePostSearchResults(nil, matches)` is a legal call, and `Auditable` is
written with an explicit `if o.PostList != nil` guard, so the nil state is not theoretical.

| Go | reached through | ours |
|---|---|---|
| `ToJSON` (post_search_results.go:25) | `psCopy.PostList.StripActionIntegrations()` → `o.Posts` | marshals what is there |
| `EncodeJSON` (post_search_results.go:32) | same call, on the receiver | same |
| `ForPlugin` (post_search_results.go:37) | `plCopy.PostList.ForPlugin()` → `Clone()` → `len(o.Order)` | keeps the embed `None` |

What makes this worse than [D-052] is **which** documents reach it. The embed is nil for every
document carrying none of `PostList::WIRE_KEYS` — measured, not read — and that includes the
ordinary `{"matches":{"<post-id>":["term"]}}`. So a search response that carried matches and no
posts is a 500 from `ToJSON`, not an empty result. Nine of the nineteen corpus documents crash,
in all three methods — 27 of the oracle's 76 recorded answers.

Accepted for [D-052]'s reason: `CLAUDE.md` forbids `panic!` in library code, the divergence is
only observable where the Go server returns a 500, and the answer we give in its place is exactly
what Go's own marshaller emits for a nil embed (`{"matches":…}` with the six promoted keys
dropped). Each is asserted in the parity tests rather than skipped —
`to_json_matches_go_and_strips_the_receiver` requires that the panicking cases are precisely the
nil-embed ones, so if upstream adds a nil check the oracle row flips and the test still holds.

---

## D-055 · `PostSearchResults::for_plugin` does not alias the caller's `Matches`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post_search_results.go`)
**Related** [D-024], the same hazard on `RecentCustomStatuses`

`(*PostSearchResults).ForPlugin` opens with `plCopy := *o`, which copies the `Matches` **map
header**. It then replaces the `PostList` pointer, so the two values end up with independent post
lists and a *shared* matches map: writing a key through the returned copy is visible on the
receiver. Measured — `matches_aliased: true` on every corpus case with a non-nil map — rather
than inferred from the assignment.

The Rust port clones the map, so the two are independent.

Accepted for [D-024]'s reason: reproducing the sharing means `Rc<RefCell<…>>` or an `&mut`
signature, i.e. exporting a footgun to make a side effect that no Go call site relies on. Every
`ForPlugin` caller in the Go tree hands the result straight to a plugin API and drops the
original.

Flagged because it is the second aliasing divergence in the crate and the two are opposite in
shape: [D-024] is a receiver mutated by a method that looks pure, this is a *result* that shares
state with a receiver left visibly untouched. A ported call site that writes to
`results.matches` after taking a `for_plugin` copy would silently change behaviour — check for
that when the search endpoint lands.

---

## D-056 · `go vet` is not clean on `reference/dump/behaviour_post.go`

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `post_search_results.go`)

Thirteen findings, all the same one:

```
behaviour_post.go:238:9: range var c copies lock: struct{name string; p model.Post}
    contains model.Post contains sync.RWMutex
```

`model.Post` carries an unexported `propsMu sync.RWMutex` guarding `Props` (post.go:156), so the
corpus slices of `struct{name string; p model.Post}` copy a mutex every time they are ranged over
or assigned. Found while checking the tooling for this session; it predates it, and no other
`behaviour_*.go` file trips it because they hold their corpora as JSON strings rather than as
built `Post` values.

**Why it is probably harmless.** The generator is single-threaded and never contends the lock, so
copying an unlocked mutex produces an unlocked mutex. The failure mode `vet` is warning about —
copying a *held* lock, so two values share a corrupted state — needs concurrency the generator
does not have.

**Why it is logged rather than shrugged off.** It is unverified, not proven-safe: `Post.Props`
accessors take the lock, and a corpus case that copies a `Post` *while* an accessor holds it would
produce a fixture value that depends on lock state. More practically, a non-clean `vet` is a
signal that stops being read once it is routine, and the "definition of done" in `CLAUDE.md`
implies a clean one.

**To pay off** hold the corpus as `[]struct{name, doc string}` and decode per case — the shape
every other behaviour file already uses — or take the corpus by pointer. Mechanical either way;
it is a change to ~7 loops in one file, none of which affects a recorded value.

---

## D-057 · `null` into a scalar field is accepted by Go and rejected crate-wide

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `search_params.go`)
**Related** [D-043] (absent keys, which is the *other* half of the same contract) and [D-033]

Go's `encoding/json` documents that unmarshalling `null` into anything other than an interface,
map, pointer or slice **has no effect and produces no error** — the destination keeps its zero
value. So `{"terms":null,"modifier":null}` is a legal `SearchParams`, and Go re-emits it as
`{"modifier":""}`.

serde has no such rule. `String`, `bool` and `i64` all reject `null` outright, so the whole
document fails to decode. Measured, not read off the Go docs: the `wire` section of
`fixtures/behaviour_search_params.json` records Go's answer for two such documents, and a probe
across the crate confirmed the same rejection on `Post.message`, `Channel.display_name` and
`Session.create_at`.

**Reachability is the same as [D-043]'s and the fix is not.** D-043 was closed with a
container-level `#[serde(default)]`, which handles an *absent* key. An explicitly null one still
fails, and there is no container-level switch for it — closing this means a `deserialize_with` on
every scalar field of every wire type, or a custom `Deserializer` wrapper that maps null to
`Default` before the derive sees it.

**The slice half is already closed for `SearchParams`** and deliberately so, not as an
inconsistency: every other nullable slice in the crate is an `Option<Vec<T>>`, which decodes
`null` fine. `SearchParams`' six lists are bare `Vec`s — Go's `omitempty` drops nil and empty
alike, so no `Option` is warranted — and `null_as_empty` restores the decode behaviour the
`Option` would have given. The scalars are left alone precisely because fixing one type out of
seventy-five would be the inconsistency.

**The rule reaches slice elements too, measured 2026-08-17** (`channel_search.go`): `[null]` into
a `[]string` gives Go `[""]` and gives us a failed decode. Logged separately as [D-075] because
the *fix* is shared but the shape is not — it is the same `null`-to-zero-value rule one level
down, and no earlier corpus had put a `null` inside an array of scalars. A boundary decoder that
folds `null` to the default closes both at once; a per-field `deserialize_with` helper would close
only this entry.

**To pay off** decide the convention once, then apply it everywhere at the same time — the same
instruction [D-033] carries. A `#[serde(deserialize_with = …)]` helper per scalar type
(`null_as_default::<String>` and friends) is the cheap version; a wrapping `Deserializer` that
turns `null` into "use the default" for every field is the version that cannot be forgotten on a
new type.

Pinned rather than shrugged off: `a_null_scalar_is_accepted_by_go_and_rejected_here` asserts both
sides — that Go accepts the two documents and that we do not — so closing this fails the test and
the exemption gets deleted rather than lingering.

**Third measured instance 2026-08-16** (`file.go`), and the first where the surrounding corpus
makes the *scope* of the divergence precise rather than merely noting it. `PresignURLResponse
.Expiration` was driven with all 17 shapes a client could put in a numeric field — integers at
both `int64` bounds, an out-of-range integer, `1.0`, `1e9`, two quoted numbers, a bool, an object
and an array. Go and `serde_json` return the **same verdict on sixteen of the seventeen**; `null`
is the only one they disagree about.

That is worth recording because it bounds the work. This entry could read as "serde's scalar
decoding differs from Go's", which would imply a per-type audit; what is actually true is that
the two agree everywhere except `null`, so a single `null`-to-default mechanism closes the whole
entry and nothing else needs checking. `duration_unmarshal_matches_go` drives all seventeen and
exempts exactly one by name.

---

## D-058 · Three `FileInfoList` paths panic in Go and answer here

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `file_info_list.go`)
**Related** [D-052] (the same three shapes on `PostList`), [D-018], [D-033]

`CLAUDE.md` forbids `panic!` in library code, and three of this type's paths reach one in Go. All
three are measured under `recover` in `fixtures/behaviour_file_info_list.json`.

| Go | why it panics | ours |
|---|---|---|
| `AddFileInfo` (file_info_list.go:54) | nil-checks the **map**, then dereferences the **argument** for its key | takes `FileInfo` by move — no nil to pass |
| `SortByCreateAt` (file_info_list.go:87) | the comparator dereferences `o.FileInfos[o.Order[i]]` for an order id with no file | treats the missing file as `create_at: 0` |
| `Etag` (file_info_list.go:93) | ranges the map and reads `v.UpdateAt` off a nil `*FileInfo` | unreachable — [D-033] means the map cannot hold one |

Two of the three are made **unrepresentable** rather than merely handled, which is a stronger
position than [D-052]'s and worth stating: `AddFileInfo`'s nil argument has no Rust spelling, and
`Etag`'s nil map value cannot survive a decode. Only `SortByCreateAt`'s is a live divergence, and
it needs an order id with no matching file — which `AddOrder` produces without complaint, so it is
reachable through the public API rather than only through a malformed document.

Accepted for [D-052]'s reason: the divergence is only observable where the Go server returns a
500, and the alternatives are panicking (forbidden) or silently dropping the file. Asserted rather
than skipped — `sort_by_create_at_answers_where_go_panics` requires that exactly one corpus case
crashes Go and checks where the missing id lands for us, and `add_file_info_matches_go` requires
that Go crashed on **every** nil argument. If upstream adds a nil check, both tests fail and this
can be revisited.

**Note for the store layer.** `AddFileInfo`'s crash is the one a caller can trip without a
malformed document: any code path holding a `*FileInfo` that a lookup might have left nil. Ported
call sites get the compiler's help here; Go's do not.

---

## D-059 · `Post::is_valid` takes an unsigned size limit where Go's is signed

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `draft.go`)

`(*Post).IsValid(maxPostSize int)` and `(*Draft).IsValid(maxDraftSize int)` take Go's `int` —
**signed**. Both compare `utf8.RuneCountInString(o.Message) > max`, so a negative limit rejects
every message including the empty one, and a zero limit rejects every non-empty one.

`crates/mm-model/src/post.rs::Post::is_valid` takes `usize`, which cannot represent the negative
case at all. `draft.rs::Draft::is_valid` takes `i64`, which can, and the oracle pins it
(`message_max_negative`: an empty message with `max = -1` is a `message_length` error in Go).

The two signatures now disagree with each other, which is the actual debt — a call site being
ported from Go passes the same config-derived `int` to both and has to think about it once per
type. Reachability of the negative case itself is low: the value comes from
`ServiceSettings.MaxPostSize`, which the config validator constrains, so it takes a hand-edited
config or an unvalidated plugin call to go negative.

**To pay off** change `Post::is_valid`'s parameter to `i64` and add the two corpus cases
(`message_max_zero`, `message_max_negative`) to `reference/dump/behaviour_post.go`, which
currently drives only the zero case. One-line change on each side; deferred here only because
`post.rs` is not this session's file and re-running its oracle rewrites a 1.7 MB committed
fixture.

---

## D-060 · `behaviour_post.json` embeds 1.7 MB of pure padding

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `draft.go`)
**Related** [D-032]

`PostPropsMaxRunes` is 800,000, so any corpus case that probes the props cap embeds an
800,000-character string. `fixtures/behaviour_post.json` has two such cases and is 1.7 MB;
`behaviour_draft.json` would have been 4 MB with five.

The draft oracle solved it with a `pad` descriptor — the marshalled draft holds `""` at the padded
key and the fixture records `{field, key, prefix, fill, count}`, which the Rust side expands
before decoding. The result is 80 KB and the assertions are unchanged. `behaviour_post.json` still
embeds its padding.

This is a readability debt rather than a correctness one, and it is the same concern [D-032]
raises from the other direction: a fixture is an oracle only if a human can open it and check what
it claims. A 1.7 MB line nobody scrolls through is not being checked.

**To pay off** apply `draftPad`'s shape to `postIsValidAll`'s `props_at_limit` and
`props_over_limit`. It rewrites a committed fixture, so it wants its own session and a diff that
shows only those two cases changing.

---

## D-061 · A nil result and an empty one are the same `Vec` here

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel_mentions.go`)
**Related** [D-033] (the same nil/empty question on the *input* side)

All three functions in channel_mentions.go declare `var names []string` and return it, so
"nothing matched" is a **nil** slice, not an empty one. `json.Marshal` renders that as `null`
where an empty slice renders as `[]`. Ours returns `Vec::new()` for both, and Rust has no
spelling for the difference.

Measured across the corpus: 6 of the 44 `ChannelMentionsFromStrings` cases and 6 of the 23
attachment cases return nil, and **none of the three functions can return an empty non-nil
slice** — the only way to get a zero-length result is the nil path. So the two states are not
merely indistinguishable to us, they are indistinguishable in Go as well for these functions.

Reachability of an observable difference is therefore limited to a caller that marshals the
result directly. Go has one candidate: `FillInPostProps` writes the answer into
`props.channel_mentions`. It is unported, so this entry exists to be read when it lands — if it
stores the raw slice, a post with no channel mentions gets `"channel_mentions":null` from Go and
`"channel_mentions":[]` from us, into the same `Posts.Props` column.

**To pay off**, if that call site turns out to store the raw value: return
`Option<Vec<String>>` from the three functions, or have the *caller* map an empty result to
`Value::Null`. The second is cheaper and keeps the mention API honest.

---

## D-062 · Go's `\b`/`\B` are ASCII and the `regex` crate's are Unicode

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `channel_mentions.go`)
**Closed** 2026-08-14, same session — the only affected pattern in the crate was this file's.
**Related** [D-027] (the same shape of hazard: a std/crate API that looks right and is not)

Go's RE2 defines `\b` and `\B` over the **ASCII** word class `[0-9A-Za-z_]`. The `regex` crate
defines them over Unicode. A pattern string copied from Go compiles in Rust and silently means
something else:

| input | Go `\B~[a-zA-Z0-9\-_]+` | Rust, bare `\B` | Rust, `(?-u:\B)` |
|---|---|---|---|
| `a~chan` | no match | no match | no match |
| `é~chan` | `chan` | **no match** | `chan` |
| `日~chan` | `chan` | **no match** | `chan` |
| `٣~chan` | `chan` | **no match** | `chan` |

This is the third member of a family already in this register: `\d`/`\s` are ASCII in Go and
Unicode in the crate (`search_params.go`, spelled out as `0-9` and the five whitespace bytes),
and `unicode.IsLetter` is general-category `L` where `char::is_alphabetic` is the Alphabetic
*property* (`utils.go` note 3). All three have the same failure mode — the naive port compiles,
passes an ASCII test corpus, and diverges on real user text.

**How it was paid.** `channel_mentions::CHANNEL_MENTION_REGEX` uses `(?-u:\B)`, and a
164-codepoint sweep drives every ASCII byte plus 36 curated non-ASCII characters through four
positions in the pattern. Six tests fail if the `(?-u:…)` is dropped, which was verified by
dropping it. The sweep also pins the character class as ASCII-only in all three of its positions.

**Residual hazard**, and the reason this is worth reading rather than filing: nothing stops the
next transcribed Go pattern from carrying a bare `\b`. There is no lint for it — `clippy.toml`'s
`disallowed-methods` cannot see inside a string literal. The only defence is the habit: **every
Go regex ported into this crate gets a codepoint sweep before it is trusted.** `\b`, `\B`, `\d`,
`\D`, `\s`, `\S` and `\w` are all ASCII in Go and all Unicode in the crate.

---

## D-063 · `ToURLValues` emits one ordering where Go emits any

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `mention_map.go`)
**Related** [D-051] (Go's unstable sort), [D-027] (map iteration order, the other direction)

`mentionsToURLValues` (mention_map.go:71) ranges a Go map, and Go randomises map iteration. It
adds the mention and the id to two *parallel* slices under two different keys, so the pairing
survives — but the order does not. `Values.Encode` sorts by key and there are only two keys, so
the slice under each keeps insertion order: a two-entry mention map encodes two ways from one
input, a three-entry one six ways, and the same Go process produces different bytes on successive
calls.

`StringMap` is a `BTreeMap`, so ours always emits the sorted-by-mention ordering — one of the
orderings Go can produce, never the others.

**Why this is accepted rather than owed.** The consumer is
`mentionsFromURLValues`, which pairs by index; permuting the two slices together is exactly the
transformation it is invariant under. The oracle records `round_trips: true` for all twelve corpus
maps, and `a_reversed_go_ordering_decodes_to_the_same_map` builds an ordering we never emit and
decodes it to the same map. So a Rust client and a Go server agree about content and disagree only
about query-string bytes.

**Where it could still bite**, and why it is logged rather than shrugged off:

- **A signed or hashed URL.** Anything that MACs the query string would see two different
  messages for one map. Nothing in the tree does this today; `AddMmBlocksActionCookies` ([D-046])
  is the nearest thing and does not touch these keys.
- **A test or a cache key built from the encoded string.** Ours is stable, Go's is not, so a Go
  test asserting an exact encoding can only have one entry — which is a hint that upstream knows.

**Not to be "fixed" by randomising ours.** Deterministic output is strictly better here; the entry
exists so that whoever compares a Rust-generated URL against a Go-generated one knows why the byte
strings differ and that it is not a bug.

---

## D-064 · A query parameter that is not UTF-8 is an error here and a map key in Go

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `mention_map.go`)
**Related** [D-033] (the same shape: Go accepts, we refuse), [D-057]

Go's `url.Values` is `map[string][]string`, and a Go `string` is an arbitrary byte sequence. So
`?user_mentions=%80&user_mentions_ids=abc` gives `UserMentionMapFromURLValues` a map whose key is
the single byte `0x80` — no error, no replacement character. `crate::go_url::Values` already
models this correctly (its keys and values are `Vec<u8>`, which is why the URL corpus can record
`?a=%80` at all), but `UserMentionMap` is a `StringMap` and cannot hold it.

`MentionMapError::NotUtf8` is the result: a fifth error variant with no Go counterpart, returned
where Go returns a map.

**Reachability is low but it is client-controlled**, which is the part worth noting — it takes one
percent-escape in a query parameter, not a malformed internal state. The consequence is a 400
where Go would have built a map that then matched nothing, so the *outcome* for the user is
similar; the difference is which side reports it.

**Options**
- **(a) Type the maps as `BTreeMap<Vec<u8>, Vec<u8>>`.** Exactly faithful, and it would infect
  every call site with byte handling for a state no correct client produces.
- **(b) `String::from_utf8_lossy`.** Silently rewrites the key to `U+FFFD`, so the map is
  non-empty and wrong. Worse than erroring.
- **(c) The typed error.** Current state. Visible, testable, and it cannot corrupt a key.

**(c) for now.** Revisit if the API layer turns out to need Go's exact status code for this input
— which is the one thing the corpus cannot tell us, because Go has no code path for it.

---

## D-065 · `time.LoadLocation` is a filesystem lookup, so Go has no single answer

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `scheduled_post.go`)
**Related** [D-030] (`mime.TypeByExtension`, the same shape), [D-008]

`ScheduledPost.BaseIsValid` validates `repeat_timezone` with `time.LoadLocation`, which consults
`$ZONEINFO` and then the **host's** zoneinfo directory. The accepted set is therefore a property
of the machine, not of Go — two Mattermost servers on different base images can disagree about
whether the same scheduled post is valid.

The 50-name corpus in `fixtures/behaviour_scheduled_post.json` was generated on macOS and shows it
plainly:

| name | Go here | why |
|---|---|---|
| `america/new_york`, `AMERICA/NEW_YORK`, `utc` | **accepted** | APFS is case-insensitive |
| `America//New_York` | **accepted** | the OS collapses the doubled separator |
| `America/New_York/` | rejected, `not a directory` | an **OS error**, not Go's |
| `../etc/passwd`, `/UTC` | rejected, `time: invalid location name` | Go's own path guard |
| `US/Pacific-New` | rejected | dropped from recent tzdata |

The first four would all behave differently on a Linux server with a case-sensitive filesystem —
which is what production runs.

**What was ported.** `chrono_tz::Tz::from_str`, an embedded case-sensitive IANA table: what a
Linux Go server effectively answers. It agrees with the corpus on **44 of 50** names. The six that
differ are the four filesystem artifacts above plus `""` and `"Local"`, which Go special-cases to
UTC and to the server's own zone — and which `base_is_valid` rejects *before* the lookup runs, so
`chrono_tz` not knowing them is unobservable.
`the_timezone_table_agrees_with_go_except_on_host_artifacts` lists all six by name with the reason
each differs, so a **new** disagreement fails the test rather than widening a skip predicate.

**The error text is not reproduced exactly.** Go appends `LoadLocation`'s own message to the
`detailed_error`, which is `unknown time zone <name>` for a missing zone and an OS error string
for a path-shaped one. Ours always emits the first form. A client parsing that suffix would see a
difference on `America/New_York/`-shaped input; nothing does, and the error id and status are
identical.

**Adding `chrono-tz` is the other half of this entry.** It is a new workspace dependency, chosen
over (a) embedding a name list — which would go stale silently — and (b) taking the validator as a
parameter the way [D-030] moved the mime lookup to the caller. (b) was rejected here because,
unlike a mime type, the timezone is *validated* rather than *resolved*, so pushing it out would
put a wire-visible 400 in the app layer. `scheduled_post_recurrence.go`'s next-occurrence
arithmetic will need the real tz data regardless.

**Widened 2026-08-16** by `scheduled_post_recurrence.go`, which reaches `LoadLocation` a second
time — `ComputeNextScheduledAt` loads the zone itself rather than taking a location. Two things
changed and one did not:

- **`""` is no longer a divergence.** Go *documents* `LoadLocation("")` as UTC, which is portable
  in a way the filesystem lookup is not, so `scheduled_post_recurrence::load_location`
  special-cases it. `base_is_valid` still rejects an empty `repeat_timezone` for a weekly post,
  so the two are consistent: the name is invalid, and it is not a lookup *failure*.
- **`"Local"` is still a divergence**, and now a reachable one: `ComputeNextScheduledAt` is a
  public method with no validation in front of it, where before the only caller was `BaseIsValid`
  itself. Go resolves it against the host and we return the load error.
  `local_is_rejected_where_go_accepts_it` asserts the divergence rather than skipping it.
- **The error text is still not reproduced exactly**, for the reason above. Go's
  `failed to load repeat timezone %q: %w` wraps `LoadLocation`'s message, which is
  `time: invalid location name` for `../etc/passwd` and an OS error for
  `America/New_York/`-shaped input; ours always emits `unknown time zone <name>`.
  `compute_next_scheduled_at_matches_go` compares the full string for the repeat-type arm and
  only the prefix for this one, so the divergence is bounded by a test rather than by a comment.

The corpus also records `america/new_york` as **accepted** by the generating macOS host and
rejected by us, which is the same case-insensitive-filesystem artifact the 50-name sweep found.
It is listed by name in `HOST_DEPENDENT` alongside `Local`.

---

## D-066 · `ToPost` aliases the scheduled post's files and metadata in Go

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `scheduled_post.go`)
**Related** [D-015] (`Channel::deep_copy`), [D-024], [D-034]

`(*ScheduledPost).ToPost` (scheduled_post.go:116) writes `FileIds: s.FileIds` and
`Metadata: s.Metadata` into the new `Post`. Both are references: the slice shares its backing
array and the metadata shares its pointer, so the post and the scheduled post are the **same**
metadata afterwards. Ours clones.

That matters more here than in the earlier members of this family, because `ToPost` then
**mutates** the metadata it just aliased: when the priority map is complete it does
`post.Metadata.Priority = &PostPriority{…}`. In Go that writes through to
`s.Metadata.Priority` — converting a scheduled post to a post silently gives the *scheduled post*
a typed priority it did not have. Ours leaves the receiver untouched.

Reachability is real but the consequence is small: the Go call site sends the post and then
deletes the scheduled-post row, so nobody reads the mutated receiver. Logged because the app-layer
port will have the same code in front of it, and "convert, then inspect the original" is a natural
thing to write.

Accepted for [D-015]'s reason: reproducing the aliasing means `Arc<Mutex<…>>` on two fields to
make a discarded value match.

---

## D-067 · `ScheduledPost`'s `Serialize` restates `Draft`'s field list

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-14 (phase 1, `scheduled_post.go`)

Go's anonymous field inlines `Draft`'s nine keys into `ScheduledPost`'s object **before** its own
six. `#[serde(flatten)]` compiles and puts them **last**, so `Serialize` is hand-written in
`scheduled_post.rs` and repeats Draft's field names, order and skip predicates.

The hazard is a field added to `Draft` upstream and not to that impl: it would vanish from the
scheduled-post wire form while the draft's own tests stayed green.

Two things stand in the way today, and neither is a real guarantee:

- `the_embedded_half_comes_first` asserts a scheduled post's JSON *starts with* its draft's JSON
  minus the closing brace. That catches an omission, a reorder and a renamed key — it is the
  strong one, and it is why this entry is `unverified` rather than `divergence`.
- `the_wire_format_matches_go` is byte-exact against the oracle, which would also catch it — but
  only after the fixture is regenerated against a newer Go tree.

**To pay off**, if a second embedding shows up (Go's model package has several): factor the
draft's field emission into a helper on `Draft` that both impls call, e.g. a
`fn serialize_fields<S: SerializeStruct>(&self, s: &mut S)`. One definition, and the compiler
enforces it. Not done for a single call site.

---

## D-068 · `compute_next_scheduled_at` gives up where Go loops on

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-16 (phase 1, `scheduled_post_recurrence.go`)

`ComputeNextScheduledAt` advances a week at a time until the candidate is after `now`, with no
bound on the number of steps:

```go
for !next.After(now) { next = next.AddDate(0, 0, 7) }
```

Go's `time.Time` reaches year 292277026596, so a `now` near `math.MaxInt64` milliseconds makes
that loop run about fifteen billion times — it does not terminate in any useful sense, but it
never errors either. `chrono`'s range stops at year 262143, so ours returns
`ComputeNextScheduledAtError::OutOfRange` after roughly 13.7 million steps instead.

Not worth closing. `BaseIsValid` rejects any `scheduled_at` more than five seconds in the past
([D-059]'s sibling check), so a schedule that could reach the boundary cannot be stored, and
`now_millis` comes from the clock. Reaching it takes a hand-built `ScheduledPost` and a
`now_millis` around 8e15. The alternative — reproducing Go's non-termination — is not a behaviour
worth having.

Two smaller pieces of the same divergence are in the same enum arm: a `scheduled_at` outside
`chrono`'s range fails immediately, where Go's `time.UnixMilli` would accept it.

---

## D-069 · A generator run rewrites `behaviour_utils.json` when the host's timezone differs

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-16 (phase 1, `scheduled_post_recurrence.go`)
**Related** [D-032] (the rule this weakens), [D-008] (the Go behaviour underneath it)

`behaviourDayBounds` records `GetStartOfDayMillis`/`GetEndOfDayMillis`, which read the calendar
date in the **server's** zone ([D-008]). The corpus therefore depends on `TZ` at generation time,
and this session's first `go run .` rewrote all twenty rows of `fixtures/behaviour_utils.json`
purely because this machine sits at UTC+01:00 and the committed fixture was generated at
UTC+05:30.

Nothing is wrong with either fixture: the row carries `local_offset` and the Rust test rebuilds
the instant in the *recorded* zone rather than the host's, so `day_bounds_match_go` passes against
both. Verified this session — re-running under `TZ=Asia/Kolkata` reproduced all 73 fixtures
byte-identically, which is what isolates the cause to `TZ` and nothing else.

**Why it is logged anyway.** [D-032] closed on the principle that a clean generator run touches
only new files, so anything else in `git status` is a signal worth reading. A fixture that rewrites
itself on a differently-configured machine destroys that signal for every fixture, exactly as a
`time.Now` call would — it is the same failure mode arriving through the environment instead of
through the code. Whoever generates next on a third machine will see the same diff and have to
rediscover that it is benign.

**To pay off**, one of:
- **(a) Pin the zone in the generator** — `os.Setenv("TZ", "Asia/Kolkata")` before the day-bounds
  corpus, or run the whole binary under a fixed `TZ`. Cheapest, and it makes the recorded
  `local_offset` a constant rather than an accident. *Recommended.*
- **(b) Record the corpus under several zones at once**, which would also widen [D-008]'s
  evidence. More useful, more work.
- **(c) Leave it**, and rely on this entry.

---

## D-070 · The CJK script tables carry the Go toolchain's Unicode version, not the pinned tree's

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-16 (phase 1, `unicode.go`)
**Related** [D-021] (the generator reads the Go source tree), [D-030], [D-065] (both
environment-dependent answers)

`ContainsCJK` delegates to `unicode.Han` and three sibling `RangeTable`s, which live in the **Go
standard library** rather than in Mattermost. `crates/mm-model/src/unicode_generated.rs` is
emitted from them, so its content is a property of whichever `go` compiled the generator —
currently Go 1.26.2, Unicode **15.0.0** — and not of the SHA `reference/mattermost` is pinned to.

Three consequences:

1. **Re-running the generator under a newer Go rewrites a committed source file.** Same hazard
   class as [D-069], arriving through the toolchain instead of through `TZ`. Unicode assigns new
   codepoints every year and the CJK extension blocks are where most of them land, so this will
   move — Unicode 16.0 added extension I at `U+2EBF0..U+2EE5D`.
2. **Two Mattermost servers built against different Go releases already disagree**, so there is
   no single answer to match. This is [D-030]'s shape exactly: the "correct" behaviour is a
   property of a deployment.
3. **The disagreement is narrow and one-directional.** A newer table is a superset for these four
   scripts — Unicode does not un-assign codepoints — so the only reachable difference is a
   recently assigned character that a newer server calls CJK and an older one does not.

**Why accepted rather than open.** The alternatives are worse: pinning our own copy of the
Unicode data makes us disagree with *every* Go server rather than with some of them, and taking a
third-party script crate substitutes its vendored version for the toolchain's without making the
coupling any weaker.

**What guards it.** `unicode::CJK_UNICODE_VERSION` is emitted alongside the tables and
`the_unicode_version_matches_the_generator` asserts it against the fixture, so a Go upgrade fails
one test with an obvious cause rather than a scatter of codepoint failures. The version is also
`pub`, because "which Unicode do these tables speak" is a deployment question an operator may
need to answer.

**To revisit** if a caller's answer ever reaches the wire. Nothing in `server/public/` calls
`ContainsCJK` today — only its own test does — so the blast radius is currently zero, and that is
worth re-checking when the app layer lands.

---

## D-071 · A repeated JSON key takes the last value in Go and fails the decode here

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-16 (phase 1, `channel_view.go`)
**Related** [D-040] (the other crate-wide `encoding/json`-versus-serde decode difference)

`encoding/json` has no duplicate-key rule: it walks the object and assigns each field as it comes,
so the **last** occurrence wins. `serde_derive`'s generated `Deserialize` tracks which fields it
has seen and returns `duplicate field \`status\`` on the second one, failing the whole document.

Measured: `{"status":"first","status":"second"}` gives Go a `ChannelViewResponse` with
`Status == "second"`; we return a 400.

**The crate is currently inconsistent about this**, which is the part worth fixing even if the
divergence itself is left. The two hand-written `Deserialize` impls —
`post_search_results.rs` and `file_info_search_results.rs` — take the last value, matching Go,
and both say so in a comment. Every *derived* impl in the crate rejects. So the behaviour depends
on whether the type happened to need a hand-written decoder, which is not a distinction anyone
chose.

**It does not apply to map keys.** A repeated key inside a `map[string]T` overwrites in Go and
overwrites in a `BTreeMap` too, so `{"a":1,"a":2}` gives `{"a":2}` on both sides.
`the_response_wire_format_matches_go` covers both cases and only the struct one is exempted.

**Reachability** is [D-040]'s: real clients emit each key once, and the exposure is hand-written
integrations, webhook payloads, and anything that concatenates JSON fragments. Unlike D-040 the
failure is loud — a 400 rather than a silently dropped value — which makes it the less dangerous
of the two.

**Options**
- **(a) `#[serde(deny_unknown_fields)]`-style container attribute.** There is none for this;
  serde has no "last one wins" switch.
- **(b) A boundary decoder.** The same one [D-040] option (b) proposes: preprocess into a
  `serde_json::Value` at the API edge, where a duplicate key is resolved by the parser before the
  derive sees it. `serde_json::Map` keeps the last value, so this falls out for free — one
  mechanism closes both entries.
- **(c) Leave it.** Current state.

**(c) for now, and (b) is the same recommendation D-040 already carries** — which is the useful
result here. Two independent crate-wide decode divergences now point at the same fix, and neither
is worth solving alone.

---

## D-072 · `ChannelData::etag` answers where Go panics on a nil channel

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `channel_data.go`)
**Related** [D-052], [D-054], [D-058], [D-018] — the same family, now five files deep

`(*ChannelData).Etag` (channel_data.go:11) guards one pointer and dereferences the other three
lines later:

```go
var mt int64
if o.Member != nil { mt = o.Member.LastUpdateAt }
return Etag(o.Channel.Id, o.Channel.UpdateAt, o.Channel.LastPostAt, mt)
```

A nil `Member` yields `0`; a nil `Channel` crashes. Measured under `recover` — three of the
eleven corpus cases are a panic, and the oracle records *which* pointer was nil for each, so the
attribution is not an inference.

**Reachability is high, which is what separates this from the rest of the family.** Neither field
has `omitempty`, so `{}`, `{"channel":null}` and any document carrying only a member all decode
to a nil channel, and `ChannelData{}` from any code path has both nil. The other entries in this
family need a specific malformed collection; this one needs an empty struct.

**What ours does.** Returns the etag Go itself produces for a zero-valued channel:
`11.11.0..0.0.<member_time>`. The value is measured rather than chosen — `&Channel{}` is in the
corpus and takes the same path in Go — so the divergence is narrowed to "a nil channel and a
zero-valued channel are indistinguishable here", rather than us inventing a sentinel.

Accepted for [D-052]'s reason: `panic!` is forbidden in library code, the alternative is an
`Option<String>` return that every call site would have to unwrap for a state the Go server 500s
on, and the divergence is only observable where Go crashes.
`etag_matches_go` asserts the panic cases explicitly — including that Go panicked *because* the
channel was nil — so if upstream adds the guard the oracle row flips and the test says so.

---

## D-073 · Three float renderings are live in the crate and nothing enforces the choice

**Status** OPEN · **Severity** unverified · **Raised** 2026-08-17 (phase 1, `analytics_row.go`)
**Related** [D-027], which is the same shape of hazard for string escaping and map ordering

`analytics_row.go` put the first `float64` on the wire, and it turns out there are **three**
renderings of a `float64` in play, all reachable, all plausible at a call site:

| helper | Go equivalent | `1234567.0` | `1e-6` | `9.999999999999999e20` |
|---|---|---|---|---|
| `utils::go_json_format_float` | `encoding/json`'s encoder | `1234567` | `0.000001` | `999999999999999900000` |
| `utils::go_format_float` | `fmt`'s `%v`, i.e. `%g` | `1.234567e+06` | `1e-06` | `9.999999999999999e+20` |
| `serde_json::to_string` | — | `1234567.0` | `1e-6` | `9.999999999999999e+20` |

Measured over 29 values: `go_format_float` disagrees with the JSON rendering on **10** of them
and serde_json on **12**. The disagreements are not on exotic values — every integral float is in
both sets, and an analytics count is an integer.

The two Go helpers are both correct and both needed: `%v` is what `Etag`, the multierror layout
and every `Sprintf` call site produce, and the JSON encoder is what any wire float must use. The
debt is that a third caller has three plausible-looking options and only a test distinguishes
them — exactly [D-027]'s complaint about `serde_json::to_string` versus the two Go marshallers.

**What guards it today.** `analytics_row::go_parity::the_float_rendering_matches_go` asserts the
JSON rendering, asserts `%v`'s answer separately, and **counts** the disagreements — so if a
future change made the two agree, or made the corpus stop straddling the thresholds, the count
moves and the test fails rather than quietly proving nothing.

**To pay off** the same `clippy.toml` `disallowed-methods` entry [D-027] has been recommending
since 2026-08-14, extended to point a bare `f64` serialization at `go_json_format_float`. It is
now three hazards behind one unwritten config file — `serde_json::to_string`,
`str::to_lowercase`, and this.

---

## D-074 · Go's `int` is platform-width and `ClusterStats` uses it

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-17 (phase 1, `cluster_stats.go`)
**Related** [D-070], [D-030], [D-065], [D-008] — the family of answers that depend on the machine

`ClusterStats` declares its three counts as bare `int` where `TeamStats` and `UsersStats` use
`int64`:

```go
TotalWebsocketConnections int   // cluster_stats.go:8
```

Go's `int` is 64-bit on `amd64`/`arm64` and 32-bit on a 32-bit build, so the accepted wire range
for those three fields is a property of the **builder's target**, not of the type.

**Measured rather than assumed.** `fixtures/behaviour_stats.json` records `strconv.IntSize` (64 on
the generating host) and drives eleven numeric bounds through an `int` field and an `int64` field
side by side. They agree on all eleven — `2147483648`, both `int64` extremes, and the two values
just past them, which both reject. That agreement is what licenses mapping `int` to `i64` here;
without it the mapping would have been a habit.

**What would differ on a 32-bit build.** Go would reject `2147483648` into
`total_websocket_connections` and we would accept it — a websocket count that large is not
reachable, so the exposure is theoretical rather than merely unlikely.

Accepted rather than open: Mattermost publishes no 32-bit server, and closing it would mean a
platform-conditional wire type — `#[cfg(target_pointer_width)]` on a struct field — which is a
real cost against an unreachable state.

**What guards it.** `go_int_and_go_int64_agree_on_this_host` asserts `int_size == 64` with a
message naming this entry, so regenerating the fixture on a 32-bit builder fails one test that
says exactly what changed rather than producing a quietly weaker corpus. It also asserts the
per-case `agree` flag, so a future Go release changing either type's decode rules fails too.

**Other `int` fields will appear.** This is the first in the tree; the same measurement should be
cited rather than repeated when the next one lands.

---

## D-075 · `null` inside a `[]string` is the empty string in Go and a decode failure here

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `channel_search.go`)
**Related** [D-057] (the same rule at struct-field position), [D-033] (a nil element in a `[]*T`)

`{"team_ids":[null]}` decodes in Go to a one-element slice holding `""`, and re-marshals as
`{"team_ids":[""]}`. `serde_json` rejects the document: `invalid type: null, expected a string`.

This is **[D-057]'s rule one level down**. That entry covers `null` into a struct field, where
Go's decoder writes the zero value and moves on; the same decoder does the same thing to a slice
element, and the crate has never measured it there before because no earlier corpus put a `null`
inside an array of scalars.

**It is not [D-033].** That entry is about `[]*T` — a slice of *pointers*, where Go's nil element
is a nil pointer and re-marshals as `null`. Here the element type is a plain `string`, the nil
becomes `""`, and the round trip is lossy in Go itself: `[null]` in, `[""]` out. So the two
entries have different fixes and should not be merged.

| | Go decodes to | Go re-emits | we do |
|---|---|---|---|
| `[]*T` with `[null]` ([D-033]) | a nil element | `[null]` | reject |
| `[]string` with `[null]` (this) | `""` | `[""]` | reject |

**Reachability** is a client sending a partial list — a search request built by concatenating ids
where one is missing is the plausible shape. Go silently searches for the empty-string team id;
we return a 400. Arguably ours is the better behaviour, which is exactly why it needs recording:
it is still a disagreement between two servers on one database.

**Options** are [D-057]'s, and the same boundary decoder closes both — a `null`-to-default
transform applied before the derive sees the document handles a slice element as readily as a
struct field. Every other nullable slice in the crate is an `Option<Vec<T>>`, which handles a
`null` *slice* fine; this is only about a `null` **element**.

**Widened 2026-08-17** (`audits.go`) from `[]string` to **any non-pointer element type**.
`Audits` is `[]Audit` — the first value-element slice in the tree — and `[null]` gives Go a
one-element slice holding a **zero-valued `Audit`**, seven keys and all, rather than an error. So
the rule is not about strings: `encoding/json` writes the element type's zero value whatever it
is, and only a *pointer* element gets to stay nil ([D-033]).

That completes the picture for slices, and the three cases need keeping apart because their Go
answers differ:

| element type | `[null]` in Go | Go re-emits | we do |
|---|---|---|---|
| `[]*T` ([D-033]) | a nil element | `[null]` | reject |
| `[]string` (this) | `""` | `[""]` | reject |
| `[]T` for a struct `T` (this) | a zero-valued `T` | the full zero object | reject |

**(c) leave it for now**, pinned by `the_decode_matches_go` in `channel_search.rs` and
`a_null_element_becomes_a_zero_audit_in_go` in `audit.rs`, both of which assert Go's actual result
rather than skipping the case.

---

## D-076 · `Audits::etag` is only correct if the caller sorted the list

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `audits.go`)
**Related** [D-010] and MIGRATION.md's channel_list notes, which describe how every *other* list
etag is computed

```go
func (o Audits) Etag() string {
    if len(o) > 0 { return Etag(o[0].CreateAt) }   // the first in the list is always the most current
    return ""
}
```

Two properties, both reproduced verbatim and both worth flagging before the audit store is ported:

1. **An empty list etags to `""`.** Every other list etag in the crate returns a versioned string
   for an empty list — `ChannelList` gives `11.11.0.0.0.0.0`. This gives the empty string, which
   is not an etag. A handler that writes it into an `ETag:` header emits an empty header, and a
   conditional request against it will not behave as a caller expects.

2. **It reads element `[0]` rather than scanning for the maximum.** The comment asserts the
   ordering instead of the code establishing it. Measured: an ascending list yields the etag of
   its **oldest** row, and an unsorted list yields neither the newest nor the oldest — so the
   etag can stay constant while newer audits arrive, and the client never refetches.

**This is not our divergence — it is Go's, faithfully reproduced.** It is logged because the
correctness of the value is a property of the **query that produced the list**, not of the
function, and that dependency is invisible at the call site. Whoever ports the audit store must
preserve the `ORDER BY CreateAt DESC`; a port that changed the ordering for any other reason would
break cache invalidation with nothing failing.

`the_etag_matches_go` pins both properties against Go's own answers, including the ascending and
unsorted cases, and asserts the etag does **not** track the maximum — so if upstream ever changes
the function to scan, the test fails rather than silently agreeing.

---

## D-077 · `Session.TeamMembers` is not populated

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `session_store.go`)
**Closed** 2026-08-17 by porting `TeamStore::GetTeamsForUser` and its scheme-roles machinery.
**Blocked** anything that reads `Session.TeamMembers` — team-scoped permission checks above all.

Go's `SqlSessionStore.Get` (session_store.go:111) does two queries, not one: after loading the
session it calls `Team().GetTeamsForUser(...)` and keeps the members whose `DeleteAt == 0`.

Ours does the first query only and leaves `team_members` at `None`. The second needs the
scheme-roles join, which is a store method in its own right and would have doubled the slice.

**Why it is safe for the vertical slice and not in general.** The slice uses the session to
authenticate — `user_id` and expiry — and `/users/me` never reads team membership. The first
team-scoped route ported will read it, and an empty list is indistinguishable from "member of no
teams", so the failure is a **silent** permission denial rather than an error.

**Paid off** in `mm-store/src/team_store.rs`. The wrapper was three lines; the content was
`getTeamRoles` (team_store.go:100), which computes a member's **effective** roles from three
booleans on `TeamMembers`, three nullable role names on the team's scheme, and whatever is
already sitting in the `Roles` column.

**The branch that would have been got wrong by reading:** a scheme role id found in the `Roles`
column sets its flag *even when the column says false*, and is then excluded from
`ExplicitRoles`. That is the un-migrated case, and it is invisible in the common data — every row
in a fresh install has an empty `Roles` column, so a port that ignored the rule would pass every
casual test and silently mis-grant team admin on any pre-migration row.

**Verified against the running Go server, not reasoned.** `getTeamRoles` is unexported, so the
usual `reference/dump` oracle cannot call it. Instead the row was mutated in the shared database
and both servers asked the same question — Go through `GET /api/v4/users/me/teams/members`, ours
through `SessionStore::get`. Six shapes, all matching:

| `Roles` column | guest / user / admin | both servers answer |
|---|---|---|
| `` | f / t / t | `team_user team_admin`, explicit `` |
| `team_admin custom_role` | t / t / **f** | `custom_role team_guest team_user team_admin`, explicit `custom_role` |
| `custom_a custom_b` | f / f / f | `custom_a custom_b`, explicit both |
| `team_guest` | f / f / f | `team_guest`, explicit `` |
| `team_user team_admin team_guest` | f / f / f | all three implied, explicit `` |
| `zzz_role team_user` | t / f / t | `zzz_role team_guest team_user team_admin` |

The second row is the un-migrated case: `scheme_admin` comes back **true** from both servers
although the column said false. `crates/mm-api/tests/parity_session_team_members.rs` keeps the
comparison as a standing test.

**One branch remains unverified against Go, deliberately.** The scheme-*derived* role names —
where `Teams.SchemeId` is set and the implied role is the scheme's `DefaultTeamUserRole` rather
than the constant — cannot be exercised here: `Schemes` is an enterprise feature, the table is
empty on Team Edition, and creating a scheme needs a licence. Those branches are covered by unit
tests transcribed from the Go source and are **provisional** in exactly the sense `CLAUDE.md`
describes. The join is a `LEFT JOIN` precisely so the unset case still returns the member.

---

## D-078 · Nullable session columns default here and would fail a scan in Go

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `session_store.go`)

`Sessions` declares `NOT NULL` on `Id` and `VoipDeviceId` only. Go scans the rest into
non-pointer struct fields, so an actual `NULL` in, say, `Roles` is a scan error and the request
fails. We take the `Option` sqlx infers and `unwrap_or_default()`, so the same row yields an
empty string and the request succeeds.

**Accepted** because the divergence is strictly more permissive and cannot invent a wrong
non-empty value: `NULL` becomes `""`, which is what the column means in practice. Mattermost's
own writes never produce these NULLs, so the divergence is only reachable via a row some other
tool wrote. Reproducing Go's failure would mean rejecting a request over a column the handler
does not read.

Revisit if a column is ever added where `NULL` and `""` mean different things.

---

## D-079 · The session token is redacted from errors, where Go interpolates it

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `session_store.go`)

Two places where Go puts a live credential into a string that reaches logs:

- `store.NewErrNotFound("Session", fmt.Sprintf("sessionIdOrToken=%s", ...))` (session_store.go:107)
- `model.NewAppError(..., map[string]any{"Token": token, ...})` (app/session.go:96, :115)

Both are reproduced with the token replaced by `<redacted>` / omitted. The error **id**, status
code and detail string are unchanged, so nothing a client sees differs — `AppError.params` is
`json:"-"` and never serialised.

**Accepted deliberately, and it is the one place this port is intentionally not bug-compatible.**
The miss path runs on every request with a bad token, which is exactly the path most likely to be
high-volume in a log aggregator. A test in `session_store.rs` asserts the token does not appear.

---

## D-080 · The etag's version component tracks the pinned SHA, not the peer server

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `/users/me`)

`User.Etag` prefixes `model.CurrentVersion`. Ours is the pinned tree's `11.11.0`; the development
container runs the `latest` image, `11.10.0`. So the two servers issue different etags for a
byte-identical user, measured:

```
go   11.10.0.y9i4er48tt8bukijy7i3u5y9ar.1786973424207..0.true.true.0
rust 11.11.0.y9i4er48tt8bukijy7i3u5y9ar.1786973424207..0.true.true.0
```

**Accepted** because it is an environment mismatch, not a port bug: a Go server built from the
pinned SHA agrees. The consequence during a mixed deployment is a cache miss — a client holding
Go's etag revalidates against us and gets a 200 instead of a 304 — never a wrong body.

The parity test strips the version (**three** dot-separated components, not one) and compares the
rest, and separately asserts our prefix is `CURRENT_VERSION` so the exemption cannot widen.

---

## D-081 · Two token locations are not parsed

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `authentication.go`)

`ParseAuthTokenFromRequest` reads six locations. Four are ported — cookie, `Bearer`, `token`,
`?access_token=`. Two are not: `X-Cloud-Token` (`TokenLocationCloudHeader`) and the
remote-cluster token header.

Neither is reachable by a normal client, and both authenticate a *different kind* of principal
than a session — mishandling them is worse than not handling them. A request carrying only one
of these gets 401 here and would be served by Go.

**To pay off** port them with the principal types they imply, not as extra token strings.

---

## D-082 · `/users/me` skips the permission check because its target is always self

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `api4/user.go`)
**Blocks** — read this before adding `GET /users/{user_id}`.

Go's `getUser` calls `UserCanSeeOtherUser(session.UserId, params.UserId)` before anything else.
The migrated route resolves `me` only, so the target is the session's own user and the check is
`true` by construction.

**Accepted for this route and dangerous to generalise.** `getUser` is one handler serving both
`/users/me` and `/users/{id}`; wiring the second path to this function without adding the check
would let any authenticated user read any other user's profile. The handler's doc comment says
so at the call site, which is where someone adding the route will be looking.

---

## D-083 · The terms-of-service fields are always zero

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `api4/user.go`)

`getUser` fetches `GetUserTermsOfService(user.Id)` when the viewer is the user or an admin and
copies `TermsOfServiceId` / `TermsOfServiceCreateAt` onto the response (user.go:329-337). The
`UserTermsOfService` store is not ported, so both stay zero.

Invisible on a server with no ToS policy configured — which is why the parity test passes — and
wrong on one that has: the webapp uses these fields to decide whether to show the acceptance
gate, so a user who has accepted would be asked again. They also feed `User.Etag`, so the etag
is wrong too.

**To pay off** port `UserTermsOfServiceStore.GetByUser` and the 404-is-not-an-error branch.

---

## D-084 · `UpdateLastActivityAtIfNeeded` is not called on the read path

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `api4/user.go`)

Go's `getUser` ends with `UpdateLastActivityAtIfNeeded(session)` — a **write** on a GET, which is
how session idle timeouts stay accurate. Ours does not.

Consequence while both servers run: a user whose traffic is served by the migrated route stops
refreshing `Sessions.LastActivityAt`, so a Go server enforcing `SessionIdleTimeoutInMinutes` may
revoke a session belonging to an active user. Goes together with [D-088]'s idle-timeout check —
one writes the value, the other reads it, and porting either alone is worse than neither.

**To pay off** port it with the session cache, since Go's "if needed" is a cache-backed
throttle rather than an unconditional write.

---

## D-085 · Privacy settings are hardcoded to Go's defaults

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `mm-api`)
**Depends on** config being ported (`model/config.go`, out of scope for hand-translation).

`getUser` reads `PrivacySettings.ShowFullName` and `ShowEmailAddress` and passes both to
`User.Etag`. `AppState` carries `true`/`true` — Go's defaults — as named constants.

An admin who turns either off gets a wrong etag from us and the correct one from Go. The
response **body** is unaffected on this route, because the self case calls `Sanitize` with an
empty map and that strips nothing (see the note in `users.rs`); a route serving *other* users
would have a wrong body too.

**To pay off** load config. The fields are two booleans, so this is a config-plumbing task rather
than a translation one.

---

## D-086 · `json.NewEncoder.Encode` appends a newline and `json.Marshal` does not

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `/users/me`)
**Closed** 2026-08-17, same session.

The first cross-server byte comparison differed by exactly one byte in 721: Go's body ended
`...false}\n` and ours ended `...false}`. Go's api4 handlers write with
`json.NewEncoder(w).Encode(v)` (user.go:353), which appends `\n`; `json.Marshal` does not.

Everything else matched on the first attempt — every field, every value and the **key order**,
which serde reproduces from the struct's field order.

**Closed** by pushing `b'\n'` in the handler. Recorded rather than just fixed because it is a
property of the *call site*, not the type: every handler ported from an `Encode` call owes the
newline, and every one ported from a `Marshal` call must not add it. `post.rs::encode_json`
already had this right for the same reason — this is the second instance, so it is a pattern.

---

## D-087 · The Go server serves `/users/me` from a stale user cache

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, cross-server parity)
**Decided** 2026-08-17 — see **Decision** below. **Constrains every write route from here on.**

Measured, and the most consequential thing this slice turned up.

A login bumps `Users.UpdateAt`. The Go server answers `/users/me` from an in-memory user cache
that the login does not invalidate, so it keeps serving the pre-login row. We read through to the
database and return the current one. At the same instant:

```
psql   updateat = 1786974497630
go     update_at = 1786974491337     <- 6.3 s stale
rust   update_at = 1786974497630     <- the row
```

It does not converge: fifteen seconds of polling did not change it.

**We are the correct one.** That is the uncomfortable part — the divergence cannot be fixed by
making our answer match Go's, because Go's answer does not match its own database.

**Why it matters beyond this field.** The Strangler Fig assumes two servers over one database.
It does *not* automatically give them one cache. Every read Go caches is a read where the two
servers can disagree, and every write we make is a write Go's cache will not hear about. Go
invalidates its caches through the cluster message bus; we publish nothing to it.

**Options as first written**
- **(a) Join the cluster bus** and publish invalidations. Correct and the largest.
- **(b) Read-through only, never cache** on the Rust side, and accept that Go serves stale data
  for keys we write. Cheapest; leaves the divergence in place in one direction.
- **(c) Migrate a cached entity's reads and writes together**, so no entity is half-owned.
  Constrains route ordering rather than requiring new machinery.

---

### Decision (2026-08-17): **(b), extended — read in Rust, write through Go**

Two of the three options turned out to be unavailable, and the third turned out to be
insufficient as written. All three findings are measured.

**(a) is licensed away.** Invalidation is published through `ps.clusterIFace`
(`app/platform/web_hub.go:238`), an `einterfaces.ClusterInterface`. `einterfaces/` is the
enterprise interface surface and the only implementations live in `enterprise/`, which
`MIGRATION.md` already lists as permanently out of scope. Implementing it would mean
reimplementing a licensed component and speaking an internal, unstable message format.

**The elegant alternative is licensed away too, and this is the part worth recording.** Setting
`CacheSettings.CacheType = redis` moves the cache out of process, keys it `{cacheName}:{key}`
(`cache/redis.go:83`), and — because the client uses rueidis client-side tracking with a
five-minute TTL — deleting a key would invalidate the Go server's local copy as well. Better
still, *invalidating* needs only the key name, never the value encoding, so it would have
sidestepped the msgpack codecs entirely. It does not work:

```
{"msg":"Successfully connected to cache backend","backend":"redis","result":"PONG"}
Error: failed to initialize platform: Redis cannot be used in an instance without a
license or a license without clustering
```

**It is the Go server that refuses to boot, not Redis.** Redis starts fine, passes its own
healthcheck and answers the `PONG` in the line above; Mattermost then rejects its own
configuration at `channels/app/platform/service.go:380` and exits. Their comment there calls the
check "a hack" — the licence cannot be loaded before the store, and the store cannot be loaded
before the cache, so the Redis client is already connected by the time anything can veto it.

**There is an escape hatch, and it is not reachable from configuration.** The same condition ends
`&& !ps.forceEnableRedis`, set by the `ForceEnableRedis()` functional option
(`platform/options.go:139`). Its only caller in the tree is the test harness
(`api4/apitestlib.go:133`). So it is a **build-time** switch, not an env var or a config key: a
stock `mattermost/mattermost-team-edition` image cannot be talked into it, but a server built
from the pinned source with that option wired in can.

So the honest statement is narrower than "no channel exists": **no channel exists on a stock
binary.** Building the Go server from source unlocks Redis cache mode, and with it external
invalidation by `DEL` on `{cacheName}:{key}` — which needs only key names, never the msgpack
value encoding. That is a real option for a project that already keeps the Go source pinned; it
costs a source build of the Go server in the development stack, and it is not needed until
stale-on-write actually bites.

**(c) was insufficient.** Migrating an entity's *routes* does not give us the entity: the Go
server reads users internally for its own permission checks and webhook paths, straight from its
own cache, regardless of which server owns the HTTP route. Route-level ownership is not
read-level ownership.

**What was chosen.** Three standing rules:

1. **The Rust side never caches.** Every read goes through to Postgres. This removes one
   direction of the problem completely and costs nothing at migration-era traffic — and it is
   why we were the *correct* server in the measurement above, not merely a different one.
2. **Read routes migrate freely.** We are always at least as fresh as Go, never staler.
3. **Write routes migrate freely too — with a known consequence, not a gate.** A write we make
   to an entity Go caches is invisible to Go until its cache entry expires. That is *staleness,
   not corruption*: the row is correct, Postgres is consistent, and Go catches up on TTL. Port
   the write when you want the write ported, and expect a stale read from the Go side in the
   meantime.

**Calibration, corrected 2026-08-17.** This entry first stated rule 3 as "a write route stays
proxied to Go", making cache coherence a precondition for migrating any write. That was
over-engineering: it converted a bounded staleness window into a hard block on development, for
a project with no users and no uptime commitment. The blocking version would have made writes
the *last* thing to migrate; the corrected version makes them schedulable like anything else.
Tighten it again only when there are real users, and then per-entity — the entities where a
stale Go read actually matters (sessions, permissions) rather than all of them.

**If a clean answer is wanted later**, there are two levers, and the cheap one needs no licence:
build the Go server from the pinned source with `ForceEnableRedis()`, or run a licence with
clustering. Either permits Redis cache mode, at which point the Rust side can `DEL` the key after
writing and the staleness window closes. Worth knowing both exist; neither is worth blocking on.

**Bonus finding — this closes an open question.** The cache values are msgpack, encoded by the
generated `user_serial_gen.go` (1,343 lines) and `session_serial_gen.go` (937). Since we never
populate Go's cache under this decision, those 2,280 lines are confirmed **out of scope** rather
than merely deprioritised. Under the Redis option we would have needed only key names, and under
the chosen option we do not touch the cache at all — so there is no path on which they are
required.

**In the meantime** the parity test normalises `update_at` out of the byte comparison, asserts
everything else matches exactly, and then checks our value against the row — so the exemption
proves us right rather than hiding a difference.

---

## D-088 · The session idle timeout is not enforced

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `app/session.go`)

`GetSession` revokes a session when `ServiceSettings.SessionIdleTimeoutInMinutes > 0` and the
session is not OAuth, not a mobile app, not a user access token, and
`ExtendSessionLengthWithActivity` is off (session.go:118-137). Ours checks expiry only.

So a session idle past the configured timeout authenticates against the migrated route and is
revoked by Go. Needs config ([D-085]) and the revoke path; pairs with [D-084], which is the
write that keeps `LastActivityAt` accurate in the first place.

**To pay off** with config, and with [D-084] in the same change — porting the check without the
write would revoke sessions that are in fact active.

---

## D-089 · A write served here publishes no WebSocket event

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-17 (phase 2, first write route)
**Affects** every write route from here on.

Go's write paths end with `a.Publish(message)` — `UpdatePreferences` publishes
`sidebar_category_updated` and `preferences_changed` (app/preference.go:66-76). `Publish` writes
to the **in-process** hub and, when `clusterIFace` is set, to the cluster bus. We are a separate
process with no cluster, so a write served by Rust reaches the database and reaches no connected
client.

The user-visible effect: a browser tab open against the Go server does not learn that its
preferences changed, and shows stale state until something else forces a refetch. Unlike
[D-087], which is a bounded staleness window on a cached read, this one does not self-heal —
there is no TTL on "an event that was never sent".

**Not measured.** No WebSocket client was available in the environment to observe it, so this
entry is reasoned from `Publish`'s implementation rather than demonstrated, and is **provisional**
in the sense `CLAUDE.md` describes. The reasoning is strong — the hub is in-process and the
cluster bus is the enterprise component [D-087] already established we cannot reach — but it has
not been watched happening.

**To pay off**, one of:
- **(a) Build `mm-ws`** and have clients connect to *it* rather than to Go. Correct, and it is
  phase 5 of the plan anyway. Large.
- **(b) Have Rust writes go through Go's API** rather than the database, so Go publishes. Costs
  the latency of a second hop and makes the migrated route a proxy with extra steps.
- **(c) Accept it.** For a project with no users, a missed live update is invisible; a developer
  reloads the page. This is the current position, consistent with [D-087]'s calibration.

---

## D-090 · `PreferenceStore::save` clones each preference where Go mutates the caller's

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `preference_store.go`)

Go's `saveTx` calls `preference.PreUpdate()` on a value it received by pointer, so the caller's
`model.Preferences` is normalised in place as a side effect of saving. Ours takes `&Preferences`
and clones each entry before `pre_update`.

**Accepted** because no caller in the ported tree reads the normalised form back — `Save` returns
only an error, and the handler discards the batch afterwards. The clone is per preference in a
batch capped at 100, so the cost is bounded and small.

Revisit if a caller ever needs the post-`PreUpdate` values, which would make the difference
observable rather than merely present.

---

## D-091 · Sidebar categories are not updated when preferences change

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `app/preference.go`)
**Closed** 2026-08-17, same session — by forwarding rather than by porting.

`UpdatePreferences` calls `Store().Channel().UpdateSidebarChannelsByPreferences(preferences)`
(preference.go:62) — Go keeps sidebar categories in step with the `direct_channel_show` and
`group_channel_show` preferences, which is how showing or hiding a DM moves it in the sidebar.
The channel store is not ported, so we skip it.

Consequence: a client that changes DM or GM visibility **through the migrated route** gets a
preference row that is right and a sidebar that does not follow. Unlike the missing WebSocket
event ([D-089]), this one is a persisted inconsistency rather than a missed notification — a
reload does not fix it.

Narrow but sharp: only those two preference categories are affected, and only through our route.

**Closed** by taking exactly that option: `direct_channel_show` and `group_channel_show` joined
`flagged_post` in `FORWARDED_CATEGORIES`, so a batch touching either goes to the Go server, which
runs the sidebar sync itself. Nothing was ported and the inconsistency is gone.

Worth noting the shape of the fix, because it generalises: **forwarding is a correctness tool, not
only a stopgap.** A handler that cannot do part of its job correctly can decline that part rather
than approximate it, and the client sees no difference. Porting
`ChannelStore::UpdateSidebarChannelsByPreferences` later would let these categories be served
here, but nothing is broken until then.

---

## D-092 · Error messages are untranslated ids where Go sends prose

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-17 (phase 2, first error compared)
**Affects** every error body this server produces.

Go turns an `AppError` into a response in `web.Handler.ServeHTTP` (handlers.go:424-455), and
three steps happen *there* rather than in the handler. Measured side by side on a 403:

| field | Go | ours |
|---|---|---|
| `id` | `api.preference.update_preferences.set.app_error` | **same** |
| `status_code` | 403 | **same** |
| `detailed_error` | `""` | `""` — now **same**, see below |
| `request_id` | populated | populated — now **same** |
| `message` | `Unable to set user preferences.` | `api.preference.update_preferences.set.app_error` |

Two of the three were closed the moment they were measured, in `ApiError::into_response`:

- **`WipeDetailed`.** Go blanks `detailed_error` unless `ServiceSettings.EnableDeveloper` is on,
  and it defaults to **off** — so the default is to wipe. Skipping it leaked internal detail Go
  withholds; ours had been sending `userId=..., preference.UserId=...` to the client.
- **`RequestId`.** Set on every error. Ours omitted the key entirely (`omitempty`), so the shapes
  differed as well as the values.

What remains is `Translate`, which needs the i18n bundle — the same dependency
`post_deletion_report.go` is blocked on. Until then our `message` equals our `id`, which is
exactly what an unconfigured Go server emits before `AppErrorInit` runs, so it is the same
degradation rather than a novel one.

**To pay off** port the i18n bundle loader and `AppError::Translate`. Worth noting the webapp
branches on `id`, not `message`, so the practical impact is on humans reading errors.

---

## D-093 · A migrated method silently breaks the other methods on its path

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, first write route)
**Closed** 2026-08-17, same session, by `partially_migrated` in `mm-api/src/lib.rs`.

axum matches the **path** before the method. Registering
`PUT /api/v4/users/me/preferences` therefore made `GET` on that same path return **405 from our
own router**, instead of falling through to `Router::fallback` and reaching the Go server. A route
that had been working, and that we had not touched, broke because a *different* method beside it
was migrated.

This is the Strangler Fig's sharpest edge so far, because the failure is silent and does not
resemble its cause: the symptom was an empty response body in a parity test, not a routing error.
It will recur on every path where methods are migrated one at a time — which is most of them,
since `/users/{id}` alone carries GET, PUT, POST and DELETE across different handlers.

**Closed** by routing every migrated path through `partially_migrated`, which attaches
`MethodRouter::fallback(forward_to_go)` so unmigrated methods are proxied rather than rejected.
Registering a route directly is now the thing to avoid, and
`an_unmigrated_method_on_a_migrated_path_still_reaches_go` fails if it happens.

---

## D-094 · The permission system now gates almost every remaining route

**Status** OPEN · **Severity** blocking · **Raised** 2026-08-17 (phase 2, after four routes)
**Blocks** most of api4.

The self-scoped routes are nearly exhausted, and what is left runs into one wall. Measured across
`channels/api4/`:

| | |
|---|---|
| handlers (`func x(c *Context, ...)`) | 687 |
| `SessionHasPermission*` call sites | **674** |
| files containing at least one | 59 |

The four migrated routes are the exception rather than a sample: each is `me`-scoped, and Go's
checks short-circuit for self — `SessionHasPermissionToUser` returns true when
`session.UserId == userID` (authorization.go:258), `UserCanSeeOtherUser` when
`userID == otherUserId` (user.go:2711). That is why they were portable, and it does not extend.

**Two shapes of blocker, and the difference matters.**

*Escapable* — the check guards something that cannot act on this route.
`GET /users/me/teams/members` gates `SanitizeRoleData` behind
`SessionHasPermissionToTeam(..., PermissionManageTeamRoles)`, but that sanitiser is a no-op when
`o.UserId == currentUserId` (team_member.go:147) and the route returns the caller's own
memberships. The guard cannot change the output, so the route is portable and the sanitiser is
simply called unconditionally. Migrated on that basis, and verified byte-identical against Go.

*Not escapable* — the check decides what is in the response. `GET /users/me/teams` gates
`SanitizeTeam`, which strips `email` and `invite_id` unless the caller holds `PermissionManageTeam`
and `PermissionInviteUser` respectively (app/team.go:2303). There is no self-shortcut: a user can
be in a team without either permission. Serving it without the check would **leak an invite id**,
which is enough to join the team. Not migrated; forwarding, with a test asserting it stays
forwarded.

**So the next substantial step is the permission system itself, not another route.** What it needs:
- `model/permission.go` (2,789 lines) — already out of scope for hand-translation; **generate** it.
- `model/role.go` (1,311 lines) — the role definitions and their permission sets.
- The scheme-roles resolution already ported in `mm-store/src/team_store.rs` is the same shape one
  layer down, so the groundwork is not zero.

**Until then**, the honest options are: forward anything permission-gated (correct, and free), or
keep porting `mm-model` files, of which 141 remain. Neither is blocked.

---

## D-095 · `Bot::patch` cannot reproduce Go's nil-patch panic

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `bot.go`)

`(*Bot).Patch` takes `*BotPatch` and dereferences each field without a nil check (bot.go:143), so
`bot.Patch(nil)` panics. The oracle probes it and records `panics: true`.

Ours takes `&BotPatch`, which makes the state unrepresentable — there is no value to pass that
would panic. `WouldPatch` is the opposite case and *is* faithful: Go guards nil there explicitly
and answers `false`, so the port takes `Option<&BotPatch>` and reproduces that.

**Accepted** because the difference is a consequence of the type system rather than a choice: the
only way to reproduce the panic would be to take an `Option` and then `unwrap` it, which
`CLAUDE.md` forbids in library code and which would be worse code for an unreachable state. The
asymmetry between the two methods is Go's, and it is preserved in the signatures.

Same shape as the panics accepted in [D-052] and [D-058].

---

## D-096 · Two upstream bugs in `bot.go` are reproduced deliberately

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `bot.go`)

Both confirmed against Go by `fixtures/behaviour_bot.json`, not read out of the source.

**`IsValidCreate` reports the wrong error id for a long display name.** The branch checking
`DisplayName` against `BotDisplayNameMaxRunes` returns `model.bot.is_valid.user_id.app_error`
(bot.go:93) — a copy-paste of the line above it. There is no
`model.bot.is_valid.display_name.app_error` anywhere in the tree. Measured:

```
display_name_too_long  -> model.bot.is_valid.user_id.app_error
description_too_long   -> model.bot.is_valid.description.app_error
```

Reachable from any bot-creation form, and a client branching on the id would get a different
answer from the two servers if we "fixed" it. The parity test asserts the wrong id explicitly, so
removing the bug from the port fails loudly rather than silently diverging.

**`BotList.Etag`'s third component is always zero.** `var delta int64` is declared, never
assigned, and passed to `Etag` (bot.go:200), so every bot-list etag carries a literal `0` there.
It reads as a leftover from a version that computed something. Kept, with the variable and its
name, so a future reader who deletes the "unused" binding fails a test.

Related: `id` starts as the **string** `"0"` rather than empty, so an empty list etags as
`11.11.0.0.0.0.0` and a list whose every `UpdateAt` is zero keeps `"0"` as its id — the same trap
`Audits::etag` carries ([D-076]).

---

## D-097 · `AuditRecord::add_meta` records where Go panics

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `audit_record.go`)

Every `AddEventParameter*` function lazily creates its map:

```go
if rec.EventData.Parameters == nil {
    rec.EventData.Parameters = make(map[string]any)
}
```

`AddMeta` does not — its whole body is `rec.Meta[name] = val` (audit_record.go:130). `Meta` has
no constructor anywhere in the file, so calling it on a zero-valued record assigns to a nil map
and **panics**. Measured side by side:

| call on a zero record | Go panics |
|---|---|
| `AddEventParameterToAuditRec` | no |
| `AddEventParameterAuditableToAuditRec` | no |
| `AddEventParameterAuditableArrayToAuditRec` | no |
| `AddEventPriorState` | no |
| **`AddMeta`** | **yes** |

Ours creates the map, matching what the siblings do.

**Accepted**, for three reasons. `CLAUDE.md` forbids a panic in library code. The divergence is in
the safe direction — Go's panic surfaces as a 500 and *loses the audit record it was building*,
where ours records the entry. And the asymmetry reads as an oversight rather than a decision: the
four functions around it all guard, and nothing in the file explains why this one does not.

The parity test asserts Go's answer for all six probes, so if upstream adds the nil check this
stops being a divergence and the test says so.

---

## D-098 · `add_event_parameter` accepts a wider set than Go's generic constraint

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `audit_record.go`)

Go constrains the parameter helper to six types:

```go
func AddEventParameterToAuditRec[T string | bool | int | int64 | []string | map[string]string](...)
```

Ours takes `impl Into<serde_json::Value>`, which is strictly wider — a float or a nested object
would compile here and not there.

**Accepted** because it cannot produce a different result for any value Go accepts: each of the
six lands in a `map[string]any` and marshals as its own JSON type on both sides, and the parity
test drives all six. Reproducing the constraint exactly would mean a six-variant enum at every
call site, which buys a compile error for a case no caller in the tree writes.

Revisit if an audit consumer ever depends on the parameter map's value types being drawn from
that closed set.
