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

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-13 (phase 1, `utils.go`)

Delegates to Go's `net/url.ParseRequestURI`. Same shape of problem as `IsValidEmail` was, and the
same solution applies: build a corpus, run it through Go, iterate until it matches. Not yet
needed by anything translated, which is why it was skipped.

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

**(c) for now**, revisit if the app layer ever sees a real nil element. Whatever is chosen must
be applied to all five types above at once — the value of the current state is that it is
uniform.

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
