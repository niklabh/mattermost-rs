# Tech Debt Register

Deferred work and known divergences, carried across sessions. `MIGRATION.md` records **what was
translated and what Go actually does**; this file records **what we owe**.

Log an entry here whenever a session skips something, approximates something, or discovers a
divergence it does not close. An entry is cheap; a forgotten gap that surfaces in Phase 4 as a
client bug is not.

**Status:** `OPEN` (owed) · `ACCEPTED` (deliberate permanent divergence) · `CLOSED` (paid off).

## Standing decision: reproduce what we can measure, forward what we cannot

Settled 2026-08-17, after four separate entries turned out to be the same question wearing
different clothes — [D-044] (`shared/markdown`, 4,688 lines), [D-046] (ECDSA and AES-GCM),
[D-105] (a third-party OpenGraph package) and [D-001] (the IANA subtag registry).

Each is "Go leans on a package we would have to reproduce". The answer depends on **whether the
dependency's behaviour is measurable from outside**:

- **Measurable and mechanical** — the input space can be enumerated from Go itself and turned
  into a table or a corpus. Port it. [D-001] is this: `UserLocaleMaxLength` is 5, so the accepted
  set is finite and generable.
- **Measurable only by reimplementing the package** — a parser, a renderer, a protocol. **Do not
  port it; forward the routes that need it to the Go server.** That is what the Strangler Fig is
  for, and [D-091] already demonstrated that forwarding is a *correctness* tool rather than a
  stopgap: a handler that cannot do part of its job correctly can decline that part, and the
  client sees no difference.
- **Cryptographic** — a third case, because "close enough" fails open rather than loudly. Port
  it, but only with an oracle recording Go's actual ciphertext, and only with a crate that
  exposes the raw encoding Go emits.

This is a hobby project with no users ([[licensing-must-not-gate-development]] applies): none of
these blocks anything, and forwarding costs nothing because unmigrated is already the default.

**Severity:**
- `blocking` — something downstream cannot be built correctly until this is paid.
- `divergence` — Rust and Go behave differently on reachable input.
- `incomplete` — a function or type exists but does not cover everything Go does.
- `unverified` — we believe it matches Go but have not measured it.

---

## D-001 · `IsValidLocale` needs the IANA subtag registry

**Status** CLOSED · **Severity** blocking · **Raised** 2026-08-13 (phase 1, after `user.go`)
**Closed** 2026-08-17 — the table is generated and verified; see below.
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
Revisited 2026-08-17 in the batch resolution above and settled on **(a)**; done the same day.

**Paid off.** `reference/dump/locale_gen.go` emits
`crates/mm-model/src/locale_generated.rs`, and `is_valid_locale` is ported.

**The enumeration is exhaustive, not sampled.** `UserLocaleMaxLength` is 5, so the reachable input
space is every string of at most five bytes; over the characters a tag can contain that is
**81,376,658** strings, and `language.Parse` answers all of them in about eight seconds. 234,421
are accepted.

**The emitted tables are not that set.** 234,421 strings is too many to ship as a list, so they are
decomposed into components — 190 two-letter languages, 8,794 three-letter, 327 regions — plus a
structural rule for `ll<sep>RR`, `root` and private-use `x-…`. The generator then **re-derives all
81 million answers from the tables** and fails the build if one disagrees. The enumeration is the
proof, not the payload.

**That step caught a real error immediately.** The first rule missed the registry's
**grandfathered** tags — `i-ami`, `i-bnn`, `i-hak`, `i-lux`, `i-pwn`, `i-tao`, `i-tay`, `i-tsu`,
each in both separator spellings — and the verification named all sixteen rather than letting them
ship. They are now a 16-entry exception table the generator derives from its **own residual**:
whatever its rule fails to cover becomes the list, so nobody has to know in advance which tags are
irregular. Note `i-en`, which looks identical in shape, is **not** registered and is correctly
rejected — which is exactly why guessing the pattern would not have worked.

**Cost:** the generator run grows by about 18 seconds, and `locale_generated.rs` is 1,198 lines.

---

## D-002 · `User::is_valid` and `User::pre_save` are not ported

**Status** CLOSED · **Severity** blocking · **Raised** 2026-08-13 (phase 1, `user.go`)
**Closed** 2026-08-17 — `IsValid` landed; `PreSave` split out into [D-108].
**Depends on** [D-001], [D-004], [D-005]

`IsValid` (user.go:383) needs `IsValidEmail` (now done), `IsValidLocale` ([D-001]) and
`ValidateCustomStatus` ([D-004]). `PreSave` (user.go:486) additionally needs a
`UserPasswordHasher` and `timezones.DefaultUserTimezone()`.

`User::pre_save_partial` covers the rest and is named that way on purpose — **it does not hash
passwords**. Any caller mistaking it for Go's `PreSave` would store plaintext. Rename to
`pre_save` only when the hasher lands.

**Half-unblocked 2026-08-17.** [D-001] is closed and [D-004] was already, so `IsValid`'s three
dependencies — `IsValidEmail`, `IsValidLocale`, `ValidateCustomStatus` — are all ported and
`User::is_valid` is now writable. It is 81 lines and 18 error branches, so it wants its own oracle
and its own session.

`PreSave` is **not** unblocked: it additionally needs a `UserPasswordHasher` and
`timezones.DefaultUserTimezone()`. Until it lands, `pre_save_partial` keeps its name and the
warning that goes with it — a caller mistaking it for Go's `PreSave` stores plaintext.

**Paid off, in half.** `User::is_valid` landed 2026-08-17 — 18 branches, all measured, including
three a reading gets wrong: a **remote user may hold an invalid email** (only the format check is
skipped, not emptiness or length); the timezone cap counts **runes of Go's marshalled JSON**, so a
`<` costs six; and `Props` gates the custom-status check by **nil-ness**, so an empty-but-present
map still validates.

`PreSave` is **not** done and is now [D-108], because its remaining dependencies — a bcrypt hasher
and the timezone defaults — have nothing to do with `IsValid`'s and deserve their own entry rather
than keeping a closed one open.

**Fully paid 2026-08-17**, later the same day: [D-108] closed, `User::pre_save` landed and
`pre_save_partial` was **deleted** rather than renamed. Note the dependency named above was wrong —
Go writes **PBKDF2**, not bcrypt; see [D-108] for what that changed.

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

**Measured 2026-08-17**, after `product_notices.go` shipped a wire test that used
`serde_json::to_string`, passed every probe in the file, and failed only on the realistic one —
`Conditions` holds semver ranges like `">=1.2.3"`, which is exactly the shape [D-022] escapes.

The current spread across the crate:

| | |
|---|---|
| modules calling `go_json_marshal` | 29 |
| modules with a byte-exact wire test | 16 |
| fixtures containing `<`, `>` or `&` | 23 of 132 |

So the habit is largely established, and the exposure is narrower than the "nothing enforces it"
framing suggests — but it is real, and the failure mode is now demonstrated rather than
hypothetical: **a wire test whose fixture happens to contain no escapable character passes with
either marshaller**, and only starts failing when the type's real data grows one.

That matters beyond byte-comparison because `Post::is_valid` measures its length caps against
Go's JSON, where each escaped character is six bytes instead of one.

**Swept 2026-08-17.** Eleven modules had a byte-exact wire test and did not reference
`go_json_marshal` at all — `audit_record`, `bot`, `channel`, `file_info`, `oauth`, `oauth_dcr`,
`post_acknowledgement`, `post_embed`, `post_metadata`, `team_member`, `wrangler`. **37 call sites**
were switched to the correct marshaller and every test still passed, which is the point: they were
passing because their probes happened to contain no escapable character, and each was one data
change away from being wrong.

**The enforcement idea does not survive contact.** A source lint of the form "a file comparing
against a fixture's `json` field must not use `serde_json::to_string`" produces false positives
that are all legitimate: `analytics_row` asserts serde's float rendering precisely *because* it
differs from Go's, `post_list` checks a default's shape with `contains`, `custom_status` mentions
the function only in a doc comment. Nine files trip that rule and none of them is wrong.

Nor does the stronger version work: walking every fixture and re-marshalling it would need a
fixture-name-to-Rust-type registry, i.e. a second copy of the mapping the `#[test]`s already
encode, which would rot separately.

**Status stays OPEN, with the scope narrowed to what is actually left:** the *known* instances are
fixed, and no mechanical guard is available. The practical defence is the habit plus this entry —
when adding a byte-exact wire test, use `go_json_marshal`, and treat a fixture with no `<`, `>` or
`&` in it as evidence of nothing.

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

**Decided 2026-09-06: (c).** Ship a `mime.types` file alongside the binary and load it at startup, making
the answer a deployment artifact rather than a host accident. It is the only option that can be
made to agree with Go *on purpose* rather than by coincidence of table contents, and the
deployment cost it was weighed against is now a smaller objection than it was: [D-156] already
established that this stack is configured deliberately rather than by whatever the host provides.
Not yet implemented — it lands with the first route that returns a `FileInfo` this server
constructs, rather than one it reads back from a row Go wrote.

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
plaintext password, which is a security incident. (That function no longer exists — [D-108] closed
2026-08-17 and it became `User::pre_save`. It is cited here as the precedent, not as live code.) This one is a missing id on an interactive
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

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-14 (phase 1, `post.go` chunk 2)
**Closed** 2026-09-06 at the exposure it was measured through — `Post::attachments`.
`mm_model::go_json::remap_object_keys` rewrites folded keys to their exact spellings before serde
sees them, driven by a per-type `GoFields` schema; `case_insensitive_keys` came off the corpus's
`DIVERGENT` list and five nested cases were added beside it.

**Option (b) was the right one, and cheaper than the entry assumed.** It says a case-insensitive
deserializer "must lowercase with `utils::go_to_lower`, and Go's own rule is a *simple ASCII-ish
fold* rather than full Unicode case folding, so the helper needs its own oracle before it can be
trusted". The oracle exists now, and it says something better: Go folds ASCII **up**, not down, and
a sweep of every scalar value finds exactly **two** non-ASCII runes whose fold lands in ASCII —
U+017F and U+212A. Since every `json:` name in the tree is ASCII, those two are the entire Unicode
surface, and the port needs no fold table beyond them. `go_to_lower` was the wrong tool.

**Not crate-wide, and that is deliberate.** The remap is opt-in per type, and the types that opted
in are the ones reachable from client-supplied JSON: `MessageAttachment` and everything under it.
Option (a)'s objection — "the reachable set is every casing of every key" — applied to `alias`
attributes, not to this; what remains is that 66 other modules still match strictly, and none of
them decodes anything a client wrote. Adding one is two consts and a call.

`encoding/json` falls back to a **case-insensitive** match when no field carries the exact JSON
name (`{"Title":"t"}` populates `Title`, and so does `{"TITLE":"t"}` and `{"tItLe":"t"}`). serde
matches the `rename` string byte-for-byte and treats anything else as an unknown field, which it
silently ignores.

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
same way `User::pre_save_partial` was — not because a caller could store a plaintext password, but
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

**Status** ACCEPTED · **Severity** blocking · **Raised** 2026-08-14 (phase 1, `post_interactive_blocks.go`)
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

**To pay off** the crypto needs an oracle that records Go's *ciphertext* for a fixed key and
nonce — a round-trip test in Rust alone would prove nothing about cross-server compatibility.

**Crate choice decided 2026-09-06: RustCrypto** — `p256` with `ecdsa` for the trigger-id signature, and
`aes-gcm` for the post-action cookies, rather than `ring`. Two reasons, both about being able to
match Go rather than about ergonomics. `ecdsa::Signature` exposes the ASN.1 DER encoding directly
(`to_der`/`from_der`), which is the encoding Go's `crypto.Signer` produces and the thing that has
to agree byte-for-byte; `ring` steers callers towards its own fixed-width form. And `aes-gcm` lets
the nonce be supplied and read back explicitly, which matters because Go prepends it to the
ciphertext — a library that manages nonces internally cannot decrypt a cookie Go wrote. The
implementation still waits for a route that needs it.

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

**Status** CLOSED · **Severity** unverified · **Raised** 2026-08-16 (phase 1, `scheduled_post_recurrence.go`)
**Closed** 2026-08-17 — the generator now pins the zone itself; see below.
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

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `api4/user.go`)
**Closed** 2026-08-20 — `GET /users/{user_id}` landed with the check: self or user-based
`view_members` serve locally, everything else (the restrictions machinery) forwards to Go.

Go's `getUser` calls `UserCanSeeOtherUser(session.UserId, params.UserId)` before anything else.
The migrated route resolves `me` only, so the target is the session's own user and the check is
`true` by construction.

**Accepted for this route and dangerous to generalise.** `getUser` is one handler serving both
`/users/me` and `/users/{id}`; wiring the second path to this function without adding the check
would let any authenticated user read any other user's profile. The handler's doc comment says
so at the call site, which is where someone adding the route will be looking.

---

## D-083 · The terms-of-service fields are always zero

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `api4/user.go`)
**Closed** 2026-08-20 — `UserTermsOfServiceStore.GetByUser` and the 404-is-not-an-error branch
landed with `GET /users/{user_id}`; `/users/me` shares the tail, so its fields and etag are
right too. The found case is parity-tested with a directly-planted row (Team Edition cannot
author a ToS over REST).

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

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `api4/user.go`)
**Closed** 2026-09-06 — ported with [D-088] in one change, as the entry asked. `getUser` (both
variants) and `getUsers` refresh `Sessions.LastActivityAt`; `getUserByUsername`, which shares the
whole rest of its tail, deliberately does not, because Go does not. The five-minute throttle is
`model.SessionActivityTimeout`, not a cache lookup — the entry's "to pay off" note guessed
otherwise, and the cache turns out to be irrelevant to the *write*: Go's `session.LastActivityAt =
now` afterwards mutates a by-value copy on its way into a cache we do not have.

Go's `getUser` ends with `UpdateLastActivityAtIfNeeded(session)` — a **write** on a GET, which is
how session idle timeouts stay accurate. Ours did not.

Consequence while both servers ran: a user whose traffic was served by the migrated route stopped
refreshing `Sessions.LastActivityAt`, so a Go server enforcing `SessionIdleTimeoutInMinutes` could
revoke a session belonging to an active user. Goes together with [D-088]'s idle-timeout check —
one writes the value, the other reads it, and porting either alone is worse than neither.

---

## D-085 · Privacy settings are hardcoded to Go's defaults

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `mm-api`)
**Closed** 2026-09-06 — `PrivacySettings.ShowFullName` and `ShowEmailAddress` are read from the
configuration document alongside every other setting ([D-156]). The `DEFAULT_SHOW_*` constants and
the two `AppState` fields are deleted; `AppState::show_full_name` and `show_email_address` are now
accessors onto `App::config`, so there is exactly one place either value lives and a reload cannot
leave a copy stale. The dependency line this entry carried — "out of scope for hand-translation" —
turned out to be the wrong frame: the document did not need translating, only reading.

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

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, `app/session.go`)
**Closed** 2026-09-06 — ported with [D-084], as the entry required. All four exemptions are
reproduced and each is asserted separately; the revoke is a synchronous `Sessions` delete where Go
uses a goroutine it does not wait for, which is unobservable in the response and removes a race
from the tests. Both settings now come from the configuration document ([D-156]).

The one thing the entry did not anticipate: `ExtendSessionLengthWithActivity` has **no constant
default**. Go writes `new(!isUpdate)` where `isUpdate` is `ServiceSettings.SiteURL != nil`, so a
fresh config defaults it `true` — which would have disarmed this check on every real server, since
every persisted document carries a `SiteURL`.

`GetSession` revokes a session when `ServiceSettings.SessionIdleTimeoutInMinutes > 0` and the
session is not OAuth, not a mobile app, not a user access token, and
`ExtendSessionLengthWithActivity` is off (session.go:118-137). Ours checked expiry only.

So a session idle past the configured timeout authenticated against the migrated route and was
revoked by Go.

---

## D-089 · A write served here publishes no WebSocket event

**Status** CLOSED 2026-09-08 · **Severity** divergence · **Raised** 2026-08-17 (phase 2, first
write route)

**Closed by option (a).** `mm-ws` is a real hub (`crates/mm-app/src/hub.rs`), every write route
ported since publishes its events, and the parity suites compare the two servers' event streams
frame by frame. What remains is not this entry: a client connected to *Go* still does not see a
Rust write, and vice versa, because the two hubs are separate processes with no cluster bus
between them. That is [D-182]/[D-190], it is a property of running two servers, and it ends when
Go does.

The last route still carrying the original gap was `UpdatePreferences`, whose two publishes landed
with the custom-status group — its sibling `DeletePreferences` had had them since it was written,
and the asymmetry inside one route was the thing worth fixing.

The entry as originally written, for the record:

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
  reloads the page. ~~This is the current position~~ — superseded; (a) was built.

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

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 2, first error compared)
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

**Status** CLOSED · **Severity** blocking · **Raised** 2026-08-17 (phase 2, after four routes)
**Closed** 2026-08-21 (phase 2, authorization.go)

**Closed because the claim in the title is no longer true.** The wall this entry described was
that 674 `SessionHasPermission*` call sites across 59 api4 files reached checks we had not
ported, so a route needing one had to be forwarded. `authorization.go` is now ported to **20 of
its 36 functions** — every system-, team-, channel-, user- and post-scoped check, in both the
session-scoped and `askingUserId` forms. That is the entire surface an ordinary read or write of a
team, channel, user or post reaches.

What remains unported is not a wall but four specific stores, each blocking a named and
self-contained group: group, sidebar-category, bot and property-field. Those are tracked in
[D-134], which stays OPEN and now lists them precisely. `HasPermissionToFileAction` is enterprise
ABAC and permanently out of scope.

The two `Config` reads this entry did not anticipate are [D-156].

Everything below is the original entry, kept because its analysis of *escapable* versus
*not escapable* checks is still the right way to judge a route.

---

**Original entry (2026-08-17), superseded above.**

**2026-08-20:** the wall is down for team- and channel-scoped reads. `SessionHasPermissionTo`,
`SessionHasPermissionToTeam`, `SessionHasPermissionToChannel` and `SessionHasPermissionToUser`
are all ported, and the entry's own "not escapable" example — `GET /users/{user_id}/teams`, kept
forwarded because `SanitizeTeam` needs two team-scoped permission reads — now serves from Rust
with the sanitisation byte-compared against Go (`parity_teams_for_user.rs`). Held OPEN because
the *system-console* and ancillary checks (`SessionHasPermissionToChannelByPost`,
`SessionHasPermissionToCategory`, …) are still unported and still gate real routes.

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
  **Done 2026-08-19**: `reference/dump/permission_gen.go` emits all 311 permissions and the seven
  tables, cross-checked between the AST and the linked package.
- `model/role.go` (1,311 lines) — the role definitions and their permission sets. **This is now the
  next file**, and the only remaining model-layer prerequisite.
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

---

## D-099 · `oauth.go`'s two client-registration functions are unported

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 1, `oauth.go`)
**Closed** 2026-08-17, same day, by porting `oauth_dcr.go`.
**Depended on** `model/oauth_dcr.go` (254 lines), now ported.

`NewOAuthAppFromClientRegistration` (oauth.go:224) and `(*OAuthApp).ToClientRegistrationResponse`
(oauth.go:252) are the OAuth 2.0 Dynamic Client Registration surface. Both take or return types
that live in `oauth_dcr.go` — `ClientRegistrationRequest`, `ClientRegistrationResponse` — plus
`GetDefaultGrantTypes()` and `GetDefaultResponseTypes()` from the same file.

Everything else in `oauth.go` is ported, including the five constants those functions consume,
which are borrowed from `access.go` and `oauth_metadata.go` and pinned by the oracle.

**Worth noting before porting them:** `NewOAuthAppFromClientRegistration` mints a client secret
with `NewId()` when the requested auth method is not `none`, which is the *only* place in the
file that creates a secret — `PreSave` explicitly does not. So the public/confidential decision is
made at registration and nowhere else.

**Paid off** exactly that way. Two findings came with them, both measured:

**`NewOAuthAppFromClientRegistration` does not re-validate.** The secret is minted when the
requested auth method is `!= none`, not when it is `== client_secret_post` — so a
`client_secret_basic` request, which `ClientRegistrationRequest.IsValid` **rejects**, still gets a
confidential client if it reaches this function. Callers must validate first, and the port says so
at the call site. A nil auth method defaults to confidential.

**`ToClientRegistrationResponse` takes a `siteURL` it never reads.** Confirmed by calling with two
different values and comparing the marshalled results, which are identical. The parameter is kept
so the signature matches Go's, with a doc comment explaining that it does nothing.

---

## D-100 · Two upstream oddities in `oauth.go` are reproduced deliberately

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `oauth.go`)

Both measured against Go, not read.

**The callback-URL cap measures Go's slice formatting.**

```go
if len(a.CallbackUrls) == 0 || len(fmt.Sprintf("%s", a.CallbackUrls)) > 1024 {
```

`%s` on a `[]string` renders `[first second third]`, so the 1024 limit applies to that string —
the sum of the entries **plus one separator between each plus two brackets**. Measured: one
28-byte URL renders to 30; two 24-byte URLs render to 51, not 48. A port summing the entries
accepts payloads Go rejects at the boundary. Reproduced by `go_format_string_slice`, which is
tested against Go's rendering for seven corpora — including that `["a b"]` and `["a", "b"]` are
indistinguishable, so the format is not a parseable encoding.

**`Name` is capped in bytes and `Description` in runes, in the same function.** `len(a.Name) > 64`
against `utf8.RuneCountInString(a.Description) > 512`; every other cap in the function — client
secret 128, homepage 256, icon URL 512, app id 32 — is `len`. Measured: a 33-character name of
two-byte runes (66 bytes) is **rejected**, a 512-character description of two-byte runes (1024
bytes) is **accepted**. A test asserts that pair on its own so it cannot be lost in the corpus.

**And a third, smaller one:** `Auditable` emits `"callback_urls:"` — with a trailing colon
(oauth.go:68). Audit consumers read that key, so correcting it would make the two servers write
different audit records for the same event. The parity test asserts the typo'd key is present and
the clean one absent.

Also reproduced, and not an oddity but a security property worth naming: the confidential-client
secret comparison uses `crypto/subtle.ConstantTimeCompare`, so the port takes the `subtle` crate
rather than `==`. A short-circuiting comparison leaks the secret's length and matching prefix
through timing, and hand-rolling the loop is liable to be optimised back into a short-circuit.

---

## D-101 · The DCR redirect-URI matcher is security-critical and entirely oracle-driven

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-17 (phase 1, `oauth_dcr.go`)

Not a divergence — a note about *why* this file's tests look disproportionate, and what would make
them insufficient.

`RedirectURIMatchesGlob` decides whether an OAuth redirect target is allowed. One case too
permissive is an open redirect and hands an attacker a token; one case too strict breaks a working
client. Neither shows up in a happy-path test, and the matcher is a hand-rolled recursive globber
with three interacting layers — pattern validation, component splitting, byte-wise matching.

So it is pinned by 44 glob probes, 25 pattern-validity probes and 11 allowlist probes, all
answered by Go. Four properties are additionally asserted on their own so a regression names
itself rather than pointing at a case index:

- a host wildcard must not satisfy a path requirement (`https://example.com/evil` must **not**
  match `https://*/cb`);
- `*` stops at `/` and `**` does not;
- a pattern with no query requires a candidate with none;
- an invalid pattern never matches.

**The generated sweep landed 2026-08-17, same day.** The concern above said "a property-based
sweep would be stronger, and [D-003] showed what that buys". So it was built rather than left on
a pile: `dcrGlobGeneratedAll` crosses 45 URIs against 72 patterns — **3,240 pairs** — and records
Go's answer for each. Every one passes.

The alphabets are chosen so each interaction is reachable rather than merely plausible: a
subdomain host (does `*` cross a dot?), a multi-segment path (does `*` cross a slash?), a
percent-encoded path (is `EscapedPath` matched or the decoded form?), a multi-byte path (is
matching byte-wise?), a port (does a wildcard cover it?), and every present/absent query
combination on both sides.

Generation is **systematic, not random** — no rand, no seed — so the fixture is byte-identical on
every run and a diff means behaviour changed. The Rust test additionally asserts the corpus is not
degenerate: 312 of the 3,240 match, so a matcher hard-coded to `false` would fail 312 cases rather
than passing 2,928.

**Also accepted:** `redirect_uri_matches_glob_recur` is recursive with no depth limit, exactly as
Go's is, so a pathological pattern can exhaust the stack on both servers equally. Reproducing the
recursion rather than rewriting it iteratively keeps the matching semantics identical, which
matters more here than the shared exposure — and the patterns are operator-configured, not
attacker-supplied.

**What remains accepted:** the sweep is systematic over a chosen alphabet, not exhaustive over
all strings — a pattern shape outside those alphabets is still unmeasured. Widening the alphabet
is cheap when a specific shape becomes a concern; enumerating everything is not possible.

And the recursion note above stands unchanged: no depth limit, exactly as Go has none.

---

## D-102 · `product_notices.go`'s four matchers disagree about unknown values

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `product_notices.go`)

Not a divergence from Go — a divergence *within* Go, reproduced faithfully, and the reason this
file could not be tidied.

| method | an unrecognised value |
|---|---|
| `NoticeAudience.Matches` | **`false`** — a `switch` with no default, falling to `return false` |
| `NoticeInstanceType.Matches` | **`true`** — three `if`s, then `return true` |
| `NoticeClientType.Matches` | exact equality only |
| `NoticeSKU.Matches` | exact equality only |

So an audience nobody recognises **hides** a notice and an instance type nobody recognises
**shows** it. Measured both ways, including for the zero value.

The obvious tidy — one shared enum with a uniform fallback, or four `match` expressions written
the same way — silently changes who sees a notice, in opposite directions depending on the field.
`NoticeInstanceType::matches` is therefore written as Go writes it, three `if`s and a trailing
`true`, so the permissive fallthrough stays visible rather than becoming a `_ =>` arm that reads
like a decision.

**Two smaller traps in the same file**, both pinned:

* `NoticeSKU.Matches` treats `e0` and `team` as "unlicensed", so they match the **empty string**
  and not their own names — `NoticeSKUE0.Matches("e0")` is `false`.
* `NoticeClientType`'s `mobile` alias is **one-directional**: `mobile` matches `mobile-ios`, and
  `mobile-ios` does not match `mobile`.

---

## D-103 · `NoticeClientTypeFromString` rejects two of its own constants

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `product_notices.go`)

The function accepts `web`, `mobile-ios`, `mobile-android` and `desktop`. It does **not** accept
`mobile` or `all` — both declared `NoticeClientType` constants, and `all` is the type's documented
default. Measured; every other input, those two included, is an error.

On failure Go returns `NoticeClientTypeAll` **alongside** the error, so a caller that ignores the
error gets the *permissive* value rather than a zero one. That is easy to lose in translation: a
Rust `Result<NoticeClientType, SomeError>` would discard it.

**Reproduced** by returning `Result<NoticeClientType, NoticeClientType>` — the error arm carries
the fallback Go returns. Ugly, and deliberately so: the alternative is an error type that silently
drops a value real callers may be reading.

---

## D-104 · `NoticeMessage`'s embed forces a hand-written `Serialize`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `product_notices.go`)

`NoticeMessage` embeds `NoticeMessageInternal` anonymously, so Go inlines its keys and emits them
**first**. Measured:

```
action actionParam actionText description image title id sysAdminOnly teamAdminOnly
```

serde's `#[serde(flatten)]` emits flattened keys **last**. This is [D-067] a second time — the
same problem `ScheduledPost` has — so it is solved the same way: `Serialize` is hand-written and
`Deserialize` still derives with `flatten`, since input order does not matter.

The cost is the one [D-067] already records: the hand-written impl restates the embedded type's
field list, so a field added to `NoticeMessageInternal` must be added here too or it silently
vanishes from `NoticeMessage`'s output. The wire test covers it only if the fixture is
regenerated.

---

## D-105 · `link_metadata.go`'s OpenGraph half needs a third-party package

**Status** ACCEPTED · **Severity** incomplete · **Raised** 2026-08-17 (phase 1, `link_metadata.go`)
**Blocks** `TruncateOpenGraph`, `FilterSVGImages`, `firstNImages`, `truncateText`, and the
OpenGraph branches of `IsValid` and `DeserializeDataToConcreteType`.

`link_metadata.go` imports `github.com/dyatlov/go-opengraph/opengraph` and its `types/image`
package. `LinkMetadata.Data` holds a `*opengraph.OpenGraph` for HTML links, and five functions
manipulate its fields — truncating `Title`/`Description`/`SiteName`, blanking `Article`, `Book`,
`Profile`, `Determiner`, `Locale`, `LocalesAlternate`, `Audios` and `Videos`, and filtering
`Images`.

This is the fourth "a package does the real work" case after `net/mail`, `x/text/language`
([D-001]), `net/url` ([D-003]) and `shared/markdown` ([D-044]) — but unlike those it is a
**third-party** dependency rather than the Go standard library or Mattermost's own tree, which
makes the decision different:

- **(a) Port the OpenGraph types.** They are data structs plus a parser; only the structs are
  needed here, since parsing happens elsewhere. Probably the smallest real option.
- **(b) Find a Rust OpenGraph crate.** Risky in the way the `url` crate was for [D-003]: the wire
  shape has to match `dyatlov`'s struct tags exactly, and a crate written to the OpenGraph spec
  rather than to that library will differ.
- **(c) Leave the OpenGraph link-preview path proxied to Go**, as [D-044] does for interactive
  webhooks. Costs nothing and is reversible.

**Decided 2026-09-06: (c)** — leave the OpenGraph link-preview path proxied to Go, as [D-044] does for
interactive webhooks. It costs nothing, it is reversible, and the alternative is the failure this
project exists to avoid: (b) would have us match `dyatlov`'s struct tags by reading a Rust crate
written to a different specification, and (a) buys a port of types nothing currently reads. The
portable half — the hash, the hour rounding, `IsSVGImageURL`, the wire type and `PreSave` — is
ported and pinned; nothing depends on the rest.

**Worth knowing before choosing:** `truncateText` uses `fmt.Sprintf("%.300s[...]", …)`, where Go's
precision for `%s` is measured in **runes**, not bytes. It is unexported and only reachable
through `TruncateOpenGraph`, so it is untested here and lands with whichever option is taken.

---

## D-106 · `LinkMetadata.Data` is a `Value`, so a struct inside it loses field order

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `link_metadata.go`)

`Data` is `any` in Go, holding `*PostImage`, `*opengraph.OpenGraph` or nil according to `Type`.
It is `Option<serde_json::Value>` here, and `serde_json::Value::Object` is a **BTreeMap** — so it
sorts keys. Measured:

```
go   "Data":{"width":100,"height":200,"format":"png","frame_count":1}
ours "Data":{"format":"png","frame_count":1,"height":200,"width":100}
```

The values are identical and every JSON reader sees the same document, so this is a byte-level
difference only — but this project's bar is byte-level, and the wire test says so explicitly
rather than comparing parsed values and moving on.

**It also blocks `IsValid`.** Go's validation is a *type assertion* — `o.Data.(*PostImage)` — on
the concrete Go type. A `Value` cannot answer that question: an arbitrary object and a serialised
`PostImage` are indistinguishable. So `IsValid` and `DeserializeDataToConcreteType` are deferred
with the OpenGraph half rather than approximated, since approximating a validation is the
dangerous direction.

**To pay off**, type `Data` as an enum over the concrete variants:

```rust
enum LinkMetadataData { Image(PostImage), OpenGraph(/* D-105 */), Raw(serde_json::Value) }
```

which restores both field order and the type test in one move. It needs [D-105] resolved first for
the middle variant, and needs care on the deserialise side: `#[serde(untagged)]` would try
`PostImage` against every object, and `PostImage`'s fields all have defaults.

---

## Batch resolutions, 2026-08-17

Six entries settled together rather than one per encounter, under the standing decision at the
top of this file.

### [D-001] — generate the locale table. **Decided: option (a).**

The entry already recommended it and the project owner deferred once. It is the "measurable and
mechanical" case: `UserLocaleMaxLength` is 5, so the accepted set is finite and enumerable *from
Go*, and the output is a generated table beside `emoji_generated.rs` and `unicode_generated.rs`.

Deciding it matters more than the table does, because it unblocks **[D-002]** — `User::is_valid`
and `User::pre_save` — which is the highest-severity open pair and the reason `pre_save_partial`
still carries a name warning that it does not hash passwords. (Both landed on 2026-08-17; the
warning-named function is gone. See [D-108].)

Still **OPEN** as work; no longer open as a question.

### [D-044] — do not port `shared/markdown`. **Decided: forward.**

4,688 lines of CommonMark to serve one route family. The interactive-webhook routes stay proxied
to the Go server indefinitely, alongside `enterprise/` and the plugin host. Reversible at any
time, costs nothing today, and the entry's own analysis already showed the tempting shortcut —
a stubbed scanner — **under-reports** action ids and silently rejects payloads Go accepts.

Status moves to **ACCEPTED**: this is now a deliberate permanent divergence, not owed work.

### [D-105] — do not port the OpenGraph package. **Decided: forward.**

Same shape as [D-044] and, being third-party, worse: option (b), a Rust OpenGraph crate, would
have to match `dyatlov`'s struct tags rather than the OpenGraph spec, which is the trap the `url`
crate would have been for [D-003]. The link-preview path stays proxied.

Status moves to **ACCEPTED**. [D-106] stays open, since it is about our own modelling rather than
the package — but its enum needs this decision, and "forward" means the `OpenGraph` variant can
simply hold raw JSON.

### [D-046] — use RustCrypto. **Decided.**

`p256`/`ecdsa` and `aes-gcm` rather than `ring`. Two reasons: Go emits an ASN.1 DER signature over
P-256 and reads it back, which needs the raw encoding `ring` deliberately hides; and `subtle` is
already a workspace dependency from the same family ([D-100]).

The entry's other requirement stands and is the harder half: **an oracle recording Go's actual
ciphertext** for a fixed key and nonce. A Rust-only round-trip proves nothing about cross-server
compatibility, and this is the one area where a near-miss fails open.

Still **OPEN** as work.

### [D-089] — accept the missing WebSocket event.

Consistent with [D-087]'s calibration: a missed live update is invisible on a project with no
users, and a developer reloads the page. Revisit when `mm-ws` lands, which is phase 5 and the
proper fix.

Status moves to **ACCEPTED**, with the caveat the entry already carries: it is **reasoned, not
measured** — no WebSocket client was available to watch it happen.

### [D-092] — accept untranslated error ids.

The remaining third of the entry needs the i18n bundle. The webapp branches on `id`, not
`message`, so the practical cost is to humans reading errors rather than to clients. Port the
bundle when a human-facing surface needs it — the same trigger `post_deletion_report.go` is
waiting on.

Status moves to **ACCEPTED** for the `message` field specifically; the two fixed thirds stay
closed.

---

## D-107 · `User.IsValid`'s `auth_data` branch formats a pointer, so Go's own detail is unstable

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-17 (phase 1, `user.go`)

```go
if u.AuthData != nil && len(*u.AuthData) > UserAuthDataMaxLength {
    return InvalidUserError("auth_data", u.Id, u.AuthData)   // u.AuthData is *string
}
```

`InvalidUserError` renders its value with `%v`, and `%v` on a `*string` prints the **address**.
The two neighbouring auth-data branches dereference first — `*u.AuthData` — so this is the only
one that does it, which reads like an oversight rather than a choice.

Measured, twice in one process:

```
value_is_an_address: true
stable_across_calls: false
```

So Go's own `detailed_error` for this branch is **not reproducible even by Go**. There is nothing
to match.

**Our detail is `auth_data=<pointer>`.** The alternative — emitting the dereferenced value, which
is what the sibling branches do — was rejected on privacy grounds: `AuthData` is an SSO identifier,
`detailed_error` reaches logs, and Go's accident happens to keep it out of them. A marker preserves
that property and signals the difference.

The parity test asserts Go's recorded detail still contains `0x`, so if upstream ever dereferences
the pointer this entry stops being a divergence and the test says so.

---

## D-108 · `User::pre_save` is still unported, and `pre_save_partial` is still a loaded gun

**Status** CLOSED · **Severity** blocking · **Raised** 2026-08-17 (phase 1, `user.go`)
**Closed** 2026-08-17 — `pre_save` landed, `pre_save_partial` is gone. With it, [D-002] is fully paid.
**Split from** [D-002], whose `IsValid` half closed first.

`User::is_valid` landed 2026-08-17 once [D-001] closed. `PreSave` did not, and its two remaining
dependencies are unrelated to anything ported so far:

- **A password hasher.** Go uses `golang.org/x/crypto/bcrypt`. Both servers read the same
  `Users.Password` column, so the cost factor and hash format are load-bearing — this is a
  cross-server compatibility problem like [D-046], not a free choice.
- **`timezones.DefaultUserTimezone()`**, which reads a timezone list Mattermost ships.

Until then `pre_save_partial` keeps its name **and the reason for it**: a caller mistaking it for
Go's `PreSave` stores a plaintext password. It is the one genuinely dangerous thing in the crate,
and the name is the only guard.

**To pay off** decide the bcrypt crate and pin it against Go's output for a known password and
cost, then port the timezone defaults, then rename.

---

### Paid off — and the premise above was wrong

**bcrypt is not what a Mattermost server writes.** The entry says "Go uses
`golang.org/x/crypto/bcrypt`", which was true of the codebase for years and is **not** true at the
pinned SHA. `channels/app/password/hashers/hashers.go`:

```go
latestHasher PasswordHasher = DefaultPBKDF2()
```

and the *only* caller of `User.PreSave` in the entire tree,
`channels/store/sqlstore/user_store.go:180`:

```go
if err := user.PreSave(hashers.GetLatestHasher()); err != nil {
```

So every password the Go server writes into the shared `Users.Password` column today is a
**PBKDF2 PHC string**, `$pbkdf2$f=SHA256,w=600000,l=32$<salt>$<hash>`. bcrypt survives only as the
fallback `GetHasherFromPHCString` returns for a stored value that does not parse as PHC — i.e.
rows written before Mattermost's own migration.

Doing exactly what this entry asked — pick a bcrypt crate, pin the cost — would have produced a
Rust server writing the **superseded** format into a column the Go server is actively migrating
away from. Not a break (Go routes non-PHC rows back to bcrypt and they still authenticate) but a
new divergence, introduced while closing an entry about avoiding one.

**What landed.** `crates/mm-app/src/password/`, in `mm-app` rather than `mm-model` because the
implementations derive from `server/channels/` and `mm-model` is Apache-2.0 ([D-031]). Go draws
the line in the same place, which is *why* the hasher is a parameter to `PreSave` at all.

| | |
|---|---|
| `mm-model::user::UserPasswordHasher` | the trait (user.go:78), plus `PasswordHashError` for `ErrPasswordTooLong` and Go's opaque half |
| `mm-app::password::Pbkdf2` | `pbkdf2` + `hmac` + `sha2`. **The default**, matching `latestHasher` |
| `mm-app::password::BCrypt` | the `bcrypt` crate, cost 10. Ported anyway — old rows still have to verify |
| `mm-model::timezones` | `shared/timezones/`, whole; 592 zones **generated** into `timezones_generated.rs` |

**The pinning is exact, not structural.** Both algorithms are deterministic *given the salt*, and
both stored formats carry their salt. So `behaviour_password.json` holds hashes the Go package
actually produced, and the Rust tests decode the salt back out, recompute, and assert **the whole
Go string byte-for-byte** — six passwords per hasher, including an embedded NUL and both sides of
the 72-byte cap. That is "Rust emits Go's bytes", not merely "Rust can read Go's", and it needed no
cross-process handshake.

The fixture's hashes are literals, which is normally the guessing the oracle exists to prevent —
so **the generator re-verifies every one against Go before writing**: `CompareHashAndPassword` must
accept it for its password and reject it for another, `bcrypt.Cost` must report 10, and the PBKDF2
PHC must parse and satisfy `IsPHCValid`. A mistyped character fails the generator, not a
downstream test. They are literals rather than fresh calls because both hashers salt randomly and
a fixture that called them would be rewritten on every run — [D-032]'s defect.

**Three findings the corpus produced rather than confirmed:**

1. **bcrypt truncates at 72 bytes, and the two Go layers disagree about it.**
   `x/crypto/bcrypt.CompareHashAndPassword` **accepts** a 73-byte password against a hash of its
   first 72 — the classic truncation — while `GenerateFromPassword` refuses to create one. The
   `hashers` package puts the length check back on the compare path, so *through the package* it is
   rejected. A port reproducing the crate rather than the package would authenticate a login the Go
   server denies. Found because the generator's negative control (append a character) failed on the
   72-byte case; pinned as `bcrypt_truncation` for whoever ports verification ([D-109]).
2. **The 72-byte cap is bcrypt's, and PBKDF2 inherited it for no algorithmic reason.** PBKDF2
   accepts any length; `hashers` applies one rule to every hasher. A port reasoning from the
   algorithm would leave it off the PBKDF2 path and accept a password Go rejects.
3. **The timezone guard is `== nil`, not `len() == 0`** — so an empty-but-present map is left
   empty, while `NotifyProps` three lines above *does* use `len() == 0`. Smoothing the two into a
   consistency Go does not have is the obvious mistake.

**Cost:** four new dependencies (`pbkdf2`, `hmac`, `sha2`, `bcrypt`), and `[profile.dev.package.*]`
entries building them at `opt-level = 3` — the parity tests must run at Go's real 600,000
iterations, because the iteration count is part of what they match.

---

## D-109 · The password **verification** half is unported

**Status** CLOSED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `hashers/`)
**Closed** 2026-08-18 — `phcparser`, both `CompareHashAndPassword`s, both `IsPHCValid`s and
`GetHasherFromPHCString` all landed. Only `App.migratePassword` remains, and it belongs with the
login route rather than here.
**Blocked** the login route, and any password-change flow.

[D-108] ported the write half — `Hash` on both hashers — because that is all `User::PreSave`
needs. The read half is not ported:

| Go | why it was left |
|---|---|
| `phcparser` (`channels/app/password/phcparser/parser.go`) | a hand-written state-machine parser; needs its own corpus |
| `PasswordHasher.CompareHashAndPassword` | needs the parser to know which hasher a stored value belongs to |
| `PasswordHasher.IsPHCValid` | ditto, and it is what decides whether a row needs migrating |
| `GetHasherFromPHCString` | the router: PHC → PBKDF2, anything else → bcrypt |
| `App.migratePassword` | re-hashes an old row on successful login |

Nothing is blocked by this that is not already blocked by the login route being forwarded, and
forwarding is correct — the Go server verifies against the same column and reaches the same answer.

**Two facts the port will need, both already measured and pinned** in
`fixtures/behaviour_password.json` rather than left to be rediscovered:

- **The 72-byte length check must be on the compare path too.** `x/crypto` accepts a 73-byte
  password against a 72-byte hash; the `hashers` package does not. Reproducing the crate's
  behaviour instead of the package's would accept a login Go rejects. Pinned under
  `bcrypt_truncation`, asserted by
  `password::bcrypt_hasher::go_parity::the_truncation_boundary_is_pinned_for_whoever_ports_verification`.
- **`GetHasherFromPHCString` defaults to bcrypt on a parse failure**, so an unparseable stored
  value is not an error — it is a legacy row. Pinned under
  `which_hasher_writes.bcrypt_is_still_the_fallback`.

The comparison must be constant-time. `subtle` is already a workspace dependency for
`OAuthApp::validate_confidential_client_grant`, and Go uses `crypto/subtle.ConstantTimeCompare`
here for the same reason.

---

### Paid off

`crates/mm-app/src/password/phcparser.rs` plus the verification methods on both hashers and the
router in `mod.rs`. `mm-app` is now at 51 tests, and
`verify_go_parity::hashes_go_wrote_verify_here` is the one that matters: every hash the **Go**
package produced, through both hashers, verifies here.

**The parser is where the content was.** 434 lines of hand-written state machine, and the corpus
overturned five readings of it:

1. **A bcrypt hash does not parse — and that is the mechanism, not a bug.** `$2a$10$…` gives
   `id="2a"`, then `10` as a salt, then a digest containing `.`, which is not base64. The failure
   is how `GetHasherFromPHCString` recognises a legacy row, and the whole stored string then
   becomes the PHC's `Hash`. This entry's own framing ("PHC → PBKDF2, anything else → bcrypt") was
   right by accident: the "anything else" includes strings that parse perfectly.
2. **An unknown function id routes to bcrypt even when the string parses.** A valid
   `$argon2id$v=19$…` PHC is well-formed, is not PBKDF2, and falls to the `default` arm — so it is
   handed to bcrypt with the parsed PHC **discarded**.
3. **`$pbkdf2` with bad or missing parameters is a hard error**, not a fallback, because it matched
   the id and `NewPBKDF2FromPHC` then runs. A bare `$pbkdf2` fails with
   `invalid work factor parameter 'w='`.
4. **The first parameter name is validated against a wider class than every later one.** It is
   scanned as `B64ENCODED` before the parser knows whether it is a name or a salt, then used as a
   name unchecked — so `$x$A=1` and `$x$a+b/c=1` are accepted while `$x$a=1,B=2` is not.
5. **`v` means three different things in three positions**: the version key in first position, an
   error in second position after a version block, and an ordinary parameter name inside the comma
   loop. The check that reads as though it guards first position is unreachable from there.

Plus a NUL byte being **swallowed** rather than rejected or treated as a terminator (`read` returns
the `eof` sentinel, which *is* `rune(0)`, and `scanIdent` breaks without unreading), and
`parseToken` **discarding the literal** on failure so most error messages say `found ""` instead of
naming the offending character.

**`MaxRunes` counts bytes, and getting that pinned took three attempts** — recorded because the
process is the lesson, not the conclusion:

- Draft 1 asserted a hand-picked "decisive" input. It was not decisive: every character in all four
  classes is single-byte, so within a legal prefix the byte index equals the rune index and both
  limiters cut in the same place. The test passed and proved nothing.
- A **mutation** — switching the port to count runes — passed the entire suite. That is what
  exposed it.
- Draft 2 concluded the two are therefore indistinguishable. Also wrong: when the 256-byte cut
  lands *inside* a multi-byte character, Go decodes the orphaned lead byte as U+FFFD where a rune
  limiter would deliver the whole character, and the error text differs. Four of 24 boundary inputs
  do this; the same mutation now fails.

The rule this suggests: **when a test passes on the first run, mutate the thing it claims to
measure.** Both earlier drafts were green.

**One correction to what [D-108] shipped.** `hashers.ErrPasswordTooLong` is
`fmt.Errorf("hashers: %w", model.ErrPasswordTooLong)` — the hasher hands `PreSave` the *wrapped*
error, so the `AppError`'s `detailed_error` reads `hashers: password too long; …` and
`errors.Is` still finds the sentinel. [D-108]'s `PasswordHashError` had a flat `TooLong` variant
that could reproduce one of those and not the other, so the 400's wire text was nine bytes short.
Fixed: the enum gained a `Wrapped` variant and an `is_too_long()` that walks the chain, and
`pre_save` branches on the predicate rather than the variant. Had `pre_save` kept matching the bare
variant while the real hasher returned a wrapped one, **every genuinely too-long password would
have taken the 500 branch instead of the 400.**

**Not closed with it:** `App.migratePassword`, which re-hashes a stale row on successful login.
`is_latest_hasher` is the predicate it branches on and is ported; the function itself needs the
login route and the store, so it lands with those. FIPS remains [D-110].

**Cost:** `cargo test -p mm-app` now takes ~54s, almost all of it PBKDF2 at Go's real 600,000
iterations. A cheap work factor would make the compare corpus meaningless — the iteration count is
part of what is being matched — so the time is the price of the assertion, not waste.

---

## D-110 · FIPS mode is not modelled

**Status** ACCEPTED · **Severity** incomplete · **Raised** 2026-08-17 (phase 2, `hashers/`)
**Accepted** 2026-09-06 — a permanent decision, not deferred work. A FIPS Mattermost is a different
binary built with a build tag; the Rust equivalent is a cargo feature, and this repo has no FIPS
deployment to shape one against. Choosing a feature layout with no caller is the speculative
porting CLAUDE.md forbids. The behaviour stays recorded below because it is counter-intuitive and
someone will eventually meet it — but nothing is owed, so it does not belong on a backlog.

`hashers/fips.go` and `fips_default.go` are build-tagged: under `requirefips`, `fipsMinKeyLength`
becomes `model.PasswordFIPSMinimumLength` and `PBKDF2.CompareHashAndPassword` short-circuits to
`ErrMismatchedHashAndPassword` for any password shorter than it, because the OpenSSL 3.x FIPS
provider refuses such a key outright (NIST SP 800-132).

Not ported: it only affects the compare path ([D-109]), and it is a **deployment** property rather
than a code one — a FIPS Mattermost is a different binary. Rust has no build tags; the equivalent
is a cargo feature, and choosing one before there is a caller would be guessing at the shape.

Worth recording because the behaviour is counter-intuitive: under FIPS a short password does not
fail to hash, it fails to *verify*, so an account created on a non-FIPS build can become
unloggable after a FIPS upgrade. That is upstream's behaviour, not a divergence we would be
introducing.

---

## D-111 · The `user_is_valid` oracle wrote a heap address into a committed fixture

**Status** CLOSED · **Severity** unverified · **Raised** 2026-08-17 (phase 1, `user.go`)
**Closed** 2026-08-17, same day it was found. **Related** [D-032], [D-107]

Same defect as [D-032] and found the same way: an unexplained `M fixtures/behaviour_user_is_valid.json`
after a generator run that should have touched only new files.

[D-107]'s note claimed the fixture "records whether the detail is stable rather than the detail
itself for that branch". That was true of the dedicated `auth_data_ptr` probe and **false of the
`cases` section**, which recorded `detailed_error` verbatim — including
`auth_data=0x452662b10900`, a heap address that differs on every run.

Two things followed from it:

- Every generator run rewrote the file, destroying the "a clean run touches only new files" signal
  that [D-069] had just restored.
- The only thing pinning [D-107] was `case["detailed_error"].contains("0x")` in `user.rs`. The
  `auth_data_ptr` probe — the section written *specifically* to record this properly — had **no
  Rust test reading it at all**.

**Fixed:** `uvRedactAddresses` replaces the hex with `<address>` before writing, and
`user::go_parity::auth_data_detail_is_an_unstable_address` now asserts the probe directly —
`value_is_an_address` and, more usefully, `stable_across_calls == false`, which is the fact that
makes the value unrecordable in the first place. The case-level assertion now checks the redaction
fired, which is the same signal in a deterministic form.

The general lesson is [D-032]'s and is now worth stating once: **a fixture created in the same
session as its writer has nothing to diff against**, so a nondeterminism in it cannot be seen until
someone runs the generator for an unrelated reason. Both instances surfaced exactly that way.

---

## D-112 · The oracle module now builds against the AGPL half of the Go tree

**Status** ACCEPTED · **Severity** unverified · **Raised** 2026-08-17 (phase 2, `hashers/`)
**Related** [D-021], [D-031]

`reference/dump` previously required only `server/public`. Generating `behaviour_password.json`
needs `channels/app/password/hashers`, so `go.mod` now also requires
`github.com/mattermost/mattermost/server/v8`, replaced to the local clone the same way.

**This is not a licence problem**, and the reasoning is worth recording so it is not re-litigated:
`reference/dump` sits at the repository root, which is AGPL-3.0-only ([D-031]). The constraint is
that **`mm-model` may not derive from `server/channels/`** — and it does not: `behaviour_password.json`
is consumed by `mm-app`'s tests alone. A future oracle feeding an `mm-model` test must stay within
`server/public`.

Two real costs:

1. **`go mod tidy` bumped `golang.org/x/text` from v0.37.0 to v0.40.0**, which is the package
   `IsValidLocale` delegates to and therefore the source of the entire 9,327-entry locale table
   ([D-001]). Checked rather than assumed: the regenerated `locale_generated.rs` is byte-identical.
   And v0.40.0 is the *correct* version — `server/public/go.mod` pins v0.37.0, but the server
   binary builds from `server/go.mod`, which pins **v0.40.0**, and MVS selects it for the `public`
   packages too. The oracle now matches the shipping binary rather than the sub-module.
2. **A much larger dependency graph is reachable from the generator.** Nothing else imports from
   it today. If a future oracle pulls in something that touches the filesystem or the network at
   init, the determinism guarantee weakens; the existing "no rand, no `time.Now`" rule in
   `main.go`'s header is the place to extend if that happens.

Accepted rather than open: the alternative was reproducing the PHC format and the cost constants
by transcription, i.e. exactly the guessing the oracle exists to eliminate — and it is what would
have hidden the PBKDF2 finding in [D-108].

---

## D-113 · `verify_pkce` compares in constant time; Go does not

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `authorize.go`)

`(*AuthData).VerifyPKCE` (authorize.go:235) finishes with a plain string comparison:

```go
return calculatedChallenge == ad.CodeChallenge
```

The Rust port uses `subtle::ConstantTimeEq` instead. **Every input produces the same answer** — the
divergence is in timing alone, and no test can observe it.

Accepted rather than reproduced-exactly for two reasons. It is free: `subtle` is already a
workspace dependency for `OAuthApp::validate_confidential_client_grant`, and the comparison is 43
bytes. And the alternative is a short-circuiting `==` on a security path, which is the shape of
thing that gets flagged in review forever afterwards.

**Worth being honest about how much this buys: very little.** The stored `code_challenge` is not a
secret — the client sends it in the authorization request, in a URL. An attacker who could learn it
through timing would still need to invert SHA-256 to produce a matching verifier. So this is
defence in depth rather than a fix for a live leak, and it is recorded here so nobody later
"restores parity" by reverting it without knowing which direction is which.

Note the same is **not** true of `ValidatePKCEForClientType`'s branches, which short-circuit on
emptiness and are reproduced exactly: those leak only whether a field was empty, which the response
already says.

---

## D-114 · `AuthorizeRequest.IsValid` reports `AuthData.IsValid` — do not "fix" it

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `authorize.go`)

All five branches `(*AuthorizeRequest).IsValid` owns (authorize.go:112, 116, 120, 124, 128) build
their error with `NewAppError("AuthData.IsValid", …)` — the name of the function above it. A
copy-paste.

What makes it recognisable rather than debatable is that the **two branches it delegates get it
right**: the PKCE one reports `AuthorizeRequest.validatePKCE` and the resource one
`AuthorizeRequest.IsValid`. So the `Where` is inconsistent inside a single function, and both
spellings of the correct value appear a few lines from the wrong one.

`Where` is on the wire — `AppError` serialises it, and it is what a client or an operator reads to
locate a 400. Repairing it would make the Rust server describe the same rejection differently from
the Go server it sits in front of.

Reproduced verbatim, with `const W: &str = "AuthData.IsValid";` carrying a comment saying it is
upstream's. Pinned by `authorize::go_parity::authorize_request_reports_auth_datas_where`, which
asserts **both** halves — the wrong value on the owned branches and the right one on the delegated
ones. If upstream fixes it, that test fails, which is the signal we want. Same treatment as
[D-016], [D-019] and [D-096].

---

## D-115 · `AuthData.IsExpired` overflows in int32 — reproduced

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `authorize.go`)

```go
func (ad *AuthData) IsExpired() bool {
	return GetMillis() > ad.CreateAt+int64(ad.ExpiresIn*1000)
}
```

`ExpiresIn` is an **`int32`**, so `ExpiresIn*1000` is evaluated at int32 width and widened only
afterwards. Go does not panic on non-constant integer overflow, so it wraps silently. Measured:

| `expires_in` | Go's product | what the line reads like | threshold |
|---|---|---|---|
| 600 | 600,000 | 600,000 | ten minutes out |
| 2,147,483 | 2,147,483,000 | same | ~24 days out |
| 2,147,484 | **−2,147,483,296** | 2,147,484,000 | ~24 days **before** `CreateAt` |
| 2,147,483,647 | **−1,000** | 2,147,483,647,000 | one second before `CreateAt` |
| −2,147,483,648 | **0** | −2,147,483,648,000 | exactly `CreateAt` |

So an authorization code with the largest expressible expiry is **already expired**, and one with
`i32::MIN` expires at the instant it was created. Nothing in the tree can produce those values
today — `PreSave` writes 600 and `IsValid` only rejects zero — but `expires_in` is a wire field, so
a client-supplied value reaches it.

**Fails closed**, which is why this is a divergence to reproduce rather than a vulnerability to
report: the overflow makes codes expire sooner, never later. A port that "fixed" it with
`i64::from(expires_in) * 1000` would make the Rust server accept a code the Go server rejects,
against the same `OAuthAuthData` row — which is the failure that matters.

Ported as `self.create_at + i64::from(self.expires_in.wrapping_mul(1000))`. `expiry_threshold_millis`
is split out from `is_expired` so the arithmetic is assertable without a clock, and
`the_expiry_threshold_matches_go` drives ten values against Go's own products — including an
explicit `assert_ne!` against the intuitive translation, so the wrong port cannot pass.

Note also that `IsValid` guards `ExpiresIn == 0` and **not** `<= 0`, so a negative expiry validates
and then makes `IsExpired` true forever. Same class, same reasoning, same treatment.

---

## D-116 · `View::clone` deep-copies props; Go's `Clone` shares nested maps

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `view.go`)
**Related** [D-015], [D-024]

`(*View).Clone` (view.go:137) copies the struct, then the props map one level deep:

```go
v.Props = make(StringInterface, len(o.Props))
maps.Copy(v.Props, o.Props)
```

`maps.Copy` is **shallow**, so a nested object inside `Props` is the same `map[string]any` in both
the original and the clone. Measured: mutating `Props["nested"]["k"]` through the original is
visible through the clone. Only the top level is independent.

Rust's `Clone` on `serde_json::Map` copies the whole tree, so ours is fully independent.

Accepted rather than open, for the same reason as [D-015]: reproducing the aliasing would mean
`Arc<Mutex<…>>` inside a wire type, and no Go call site relies on it — `Clone`'s callers all
discard the original. Flagged because a call site being ported that mutates the clone and then
reads the original would change behaviour silently.

Both directions are asserted rather than assumed: the oracle records Go's sharing as
`clone.nested_map_is_shared = true`, and
`view::go_parity::clone_shares_nested_maps_in_go_and_not_here` asserts that value **and** the
opposite behaviour on our side. If upstream switches to a deep copy the test fails, which is the
signal we want.

---

## D-117 · `View`'s nil-versus-empty props distinction cannot survive the wire

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `view.go`)

`validateKanbanProps` (view.go:197) branches on nil-ness:

| `Props` | error id |
|---|---|
| nil | `model.view.is_valid.props.kanban_required.app_error` |
| `{}` | `model.view.is_valid.props.kanban_field_id.app_error` |

But `View.Props` carries `omitempty`, and Go's `omitempty` on a map drops **nil and empty alike**.
So the two states serialise to the identical document, and decoding that document yields nil.

**This is Go's divergence, not ours** — the Go server has exactly the same hole. It is recorded
because the shape is a trap in both directions:

- A port that modelled `Props` as a non-`Option` map would collapse the distinction *inbound* too,
  changing which error a client sees for `"props":{}`.
- A port that dropped the `is_empty` half of the skip predicate would emit `"props":{}` where Go
  emits nothing, which is a wire difference.

Ported as `Option<StringInterface>` with a predicate that skips `None` **and** `Some(empty)`, so
the distinction is preserved in memory and inbound, and lost outbound exactly as Go loses it.
`view::tests::nil_and_empty_props_differ_in_validation_and_not_on_the_wire` asserts all three
halves: the two error ids differ, the two documents are byte-identical, and a round trip collapses
the second into the first.

No action is owed unless upstream removes the `omitempty`.

---

## D-118 · `ParseFormatedMillis`'s error *text* is not reproduced

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `job.go`)

Go's `time.Parse` failures describe its own layout machinery:

```text
parsing time "2023-11-14T16:43:20" as "2006-01-02T15:04:05.999Z07:00": cannot parse "" as "Z07:00"
parsing time "2023-11-14" as "2006-01-02T15:04:05.999Z07:00": cannot parse "" as "T"
parsing time "not a timestamp" as "2006-01-02T15:04:05.999Z07:00": cannot parse "not a timestamp" as "2006"
```

The trailing clause names the **layout element** that failed and the remaining input at that point,
so reproducing it means reproducing `time.Parse`'s element-by-element walk. `timeutils::TimeParseError`
carries the first half only.

Accepted rather than open. The only caller is `Job.UnmarshalYAML` ([D-119], unported), the error
reaches a CLI import operator rather than a client, and no `AppError` id depends on it.

**What *is* exact** and asserted over all eleven corpus inputs: the accept/reject verdict and the
parsed value — including the two that a reading gets wrong. An empty string returns **zero with no
error**, by an early return that predates the layout; and extra precision is **truncated**, so
`.9999` is 999 milliseconds rather than a rejection or a rounding.

---

## D-119 · `Job`'s YAML codec is unported

**Status** ACCEPTED · **Accepted** 2026-09-06 — nothing on the REST path reads YAML. These codecs exist
for Mattermost's own config-as-code tooling, not for any route this server answers or forwards, so
no wire format depends on them. If a route ever needs one, reopen with the route named.

**Superseded status line:** OPEN · **Severity** incomplete · **Raised** 2026-08-18 (phase 1, `job.go`)

`(*Job).MarshalYAML` (job.go:118) and `UnmarshalYAML` (job.go:142) render the three timestamps as
**formatted strings** rather than integers, via `timeutils`. They exist for the CLI export/import
path.

Not ported because the workspace has no YAML codec and nothing needs one. Taking a dependency to
serve a code path with no caller is the wrong order.

**The hard half is already done.** The interesting behaviour is not the YAML — it is
`timeutils::format_millis`, which is timezone-dependent and elides trailing zeros, and
`parse_formated_millis`, which tolerates an empty string and truncates precision. Both are ported
and pinned in `fixtures/behaviour_job.json`. What remains is field-name glue plus a crate choice.

Note the YAML struct's field names are `create_at`/`start_at`/`last_activity_at`, matching the JSON
tags, but its local variables inside `UnmarshalYAML` are named `createAt`, `updateAt` and
`deleteAt` — leftovers from whatever type it was copied from. They are assigned to the right
fields; the names are noise. Do not let them suggest the mapping is anything other than positional.

---

## D-120 · `AllJobTypes` omits eighteen declared job types

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-18 (phase 1, `job.go`)

job.go declares **42** `JobType*` constants. `AllJobTypes` lists **24**. `IsValidJobType` is a
linear scan of that array, so eighteen declared types fail the model's own validator:

`cli_message_export`, `resend_invitation_email`, `upgrade_notify_admin`, `trial_notify_admin`,
`post_persistent_notifications`, `install_plugin_notify_admin`, `hosted_purchase_screening`,
`s3_path_migration`, `delete_empty_drafts_migration`, `delete_orphan_drafts_migration`,
`export_users_to_csv`, `delete_dms_preferences_migration`, `access_control_sync`,
`access_control_team_sync`, `push_proxy_auth`, `recap`, `delete_expired_posts`,
`autotranslation_recovery`.

**Not our divergence — Go's**, and reproduced exactly. It is recorded because the shape invites a
"fix": a port that derived `AllJobTypes` from the constant list, or that added the missing ones,
would accept job types the Go server rejects, on a shared `Jobs` table.

**Why it is survivable, and the second half of the finding:** `Job.IsValid` **never calls
`IsValidJobType`**. So a job row carrying any of the eighteen validates and stores fine; only
whatever calls `IsValidJobType` directly — the scheduler, in `server/channels/`, unported — turns
it away. The two halves have to be ported together or the gap changes shape.

Pinned by `job::go_parity::eighteen_declared_job_types_are_rejected`, which asserts the count on
both sides (42 declared, 24 accepted, 18 rejected) and every individual verdict, and by
`all_job_types_matches_go`, which asserts the array in **Go's order**. If upstream adds a missing
type, both fail.

---

## D-121 · No response body applies Go's JSON escaping

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-19 (phase 1, permission.go)

`mm-model` has had `go_json_escape` and `go_json_marshal` since utils.go was ported: serde_json and
Go's `encoding/json` differ on exactly five characters — `<`, `>`, `&`, U+2028 and U+2029 — which Go
escapes as `\u003c`, `\u003e`, `\u0026`, `\u2028`, `\u2029` by default, in both `json.Marshal` and
`json.Encoder`.

**Nothing in `mm-api` uses them.** Every route serialises with plain `serde_json::to_vec`:
`users.rs:77`, `teams.rs:64`, `sessions.rs:49`, and the error path at `error.rs:83`. So any payload
carrying one of the five characters — a nickname or channel purpose containing `&`, a message with
`<`, a URL query in an error detail — comes off our server with different bytes than Go's, on a
route a client may already be hitting.

The vertical slice's byte-identical `/users/me` is not evidence against this: that payload contains
none of the five. Found while porting permission.go, when tightening `AppError::to_json`'s parity
assertion to compare bytes exposed the same gap one layer down ([D-122]).

**Not fixed here**, because the fix needs its own oracle before it can be verified rather than
assumed: a Go-marshalled payload for each ported wire type containing all five characters, which is
a `reference/dump` change plus a per-route assertion, not a `to_vec` → `go_json_marshal` sweep. The
sweep without the oracle would be the "confident, wrong translation" the fixture discipline exists
to prevent — `go_json_marshal`'s own doc warns it must not be handed a `HashMap`, so applying it
blindly to every response type is not obviously safe either.

**Related** [D-122] (the same gap in `to_json`, closed), [D-027] (map key ordering).

---

## D-122 · `AppError::to_json` sorted its keys, and a value-graph assertion hid it

**Status** CLOSED · **Severity** divergence · **Raised and closed** 2026-08-19 (phase 1, permission.go)

`to_json` built a `serde_json::Value` so it could substitute the folded `detailed_error`, and
`serde_json::Map` is a `BTreeMap`. Every error body therefore came out as
`detailed_error, id, message, status_code`; Go marshals a struct in declaration order,
`id, message, detailed_error, status_code`.

**The interesting half is why it survived a parity test.** `utils::go_parity::app_error_rendering_matches_go`
compared `to_json`'s output with Go's after parsing both into `serde_json::Value`, under the comment
*"key order is not part of the contract"* — the one comparison that cannot see a key-order defect.
Measured rather than argued: restoring that assertion and deliberately swapping two fields in the
wire struct leaves all 76 `utils::` tests passing.

Fixed by routing both `Serialize for AppError` and `to_json` through one `AppErrorWire<'a>`
projection, so the field order is defined once, and by marshalling through the existing
`go_json_marshal` — which caught a **second** divergence in the same function: `to_json` was not
applying Go's five-character escaping either. The parity assertion now compares bytes.

`mm-api`'s response path was never affected by the ordering half — it serialises `AppError` through
the derive, which was already in declaration order — but it *is* affected by the escaping half,
which is [D-121].

**The transferable rule:** where the goal is byte-identical output, assert bytes. A parity test that
parses both sides before comparing is testing the data, not the encoding.

---

## D-123 · `unicode-general-category` disagreed with Go on 5,812 code points

**Status** CLOSED · **Severity** divergence · **Raised and closed** 2026-08-19 (phase 1, role.go)

`go_quote`, `is_go_letter` and `is_go_number` evaluated their Unicode category through the
`unicode-general-category` crate, which answers from **that crate's** Unicode version. Go answers
from the toolchain's. Measured over the whole code-point space before the swap: **5,812**
disagreements for `IsPrint` alone, 23 of them landing on the boundary corpus. U+0897 is the shape of
all of them — ARABIC PEPET, assigned in Unicode 16.0, a nonspacing mark to the crate and unassigned
to Go — so `go_quote("\u{897}")` wrote the character literally where Go writes `\u0897`.

Same class as [D-070], which recorded the identical hazard for the CJK script ranges and solved it
the same way. Now `reference/dump/go_unicode_gen.go` emits `IsPrint` (711 ranges), `IsLetter` (659)
and `IsNumber` (137) from the linked Go toolchain, and the corpus probes **both sides of every one
of the 1,507 boundaries** plus every ASCII byte — 3,703 rune probes.

The dependency is gone from the workspace; nothing else used it.

**Reachable how:** `IsPrint` through role.go's two `%q` error messages, which quote a role name
taken off the wire. `IsLetter`/`IsNumber` through `IsValidId`. No behaviour changed for any input
the existing corpora cover — every test passed before and after — which is the point: the
divergence lived exactly where nothing was looking.

---

## D-124 · `FakeSetting` is defined in role.rs, but it belongs to config.go

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 1, role.go)

`Role.Sanitize` writes `model.FakeSetting` (config.go:92), thirty-two asterisks, into both display
fields. config.go is 5,795 lines and `MIGRATION.md` translates it lazily, section by section, so
the constant has no home yet and currently sits in `role.rs` with a comment saying so.

Cheap to pay, and it should be paid by whichever session lands the config section that owns it —
several other `Sanitize` methods in the tree use the same constant, and two definitions of it would
be worse than one in the wrong file. The value is pinned against Go by
`role::go_parity::constants_match_go`, so a drifted copy fails rather than diverging quietly.

**Moved to `utils.rs` 2026-08-19**, when `scheme.go` landed and gave it a second user. Keeping it in
`role.rs` would have had `scheme.rs` importing a config constant from the role module, which is a
worse lie about where it belongs than `utils` is. Still owed, still config.go's.

---

## D-125 · Three role.go functions have no defined output order, and it is Go's map iteration

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 1, role.go)

`ChannelModeratedPermissionsChangedByPatch` and `RolePatchFromChannelModerationsPatch` build their
returned slice by ranging a `map`, and Go randomises map iteration per range. So Go has no order to
reproduce: **the same call returns a differently-ordered slice on consecutive invocations.**

Measured rather than inferred. The oracle calls each fifty times per case and records whether the
order ever varied; it varies for every case with two or more results and is stable (trivially) for
the rest. Fifty runs make a false "stable" a 2^-49 event.

Ours return the results **sorted**, and the parity tests compare sorted sets. The test asserts that
at least one corpus case observed Go varying, so if upstream ever makes the order deterministic the
set comparison stops silently over-accepting.

`GetChannelModeratedPermissions` also ranges the map but returns a **map**, and each iteration
writes at most one key, so no answer depends on the order — that one is faithful, not accepted.

**Why accepted rather than owed:** there is no Go behaviour to match. A caller that depended on the
order would already be broken against Go. Sorting is the only stable choice, and it is strictly more
useful than reproducing one arbitrary run.

---

## D-126 · `RolePatch` cannot express a pointer to a nil slice

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 1, role.go)

Go's `RolePatch.Permissions` is `*[]string`, which has three states: nil pointer, pointer to a nil
slice, and pointer to a slice. `Role.Patch` writes through the pointer, so the middle state sets
`Permissions` to **nil**, while `Option<Vec<String>>` collapses it into `Some(vec![])`, which sets
an empty slice. Those two serialise differently — `null` versus `[]`.

**Unreachable from the wire**, which is why this is accepted rather than owed: `encoding/json`
unmarshals a JSON `null` into a nil *pointer*, never a pointer-to-nil-slice, and there is no JSON
that produces the middle state. Only Go code constructing the patch by hand can reach it, and no
such construction exists in the pinned tree.

Same shape as [D-095]: the difference is a consequence of the type system, and reproducing it would
mean modelling a state the wire cannot carry.

---

## D-127 · `RolePatchFromChannelModerationsPatch` panics on a partial patch; ours cannot

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 1, role.go)

Go dereferences `*channelModerationPatch.Name` (role.go:762) and `channelModerationPatch.Roles.Members`
(role.go:765) without a nil check. A `ChannelModerationPatch` missing either field therefore panics,
which the oracle records under `recover`: `nil_name_panics: true`, `nil_roles_panics: true`.

Both fields are `Option` on our side, and the port treats a missing `name` as matching no control and
a missing `roles` as disabling nothing. That is the conservative direction: the alternative reading
would *remove* permissions the Go server would not have removed. It cannot grant one either — the
enable branch requires `Some(true)`.

Reachable from the API: `ChannelModerationPatch` is a wire type, so a client sending
`{"name": "create_post"}` with no `roles` crashes the Go handler. Not our bug to fix, and not one we
can reproduce without an `unwrap` that `CLAUDE.md` forbids in library code. Same reasoning as
[D-095] and [D-052].

---

## D-128 · The `Role` and `Scheme` YAML codecs are unported

**Status** ACCEPTED · **Accepted** 2026-09-06 — same reasoning as [D-119]: no served or forwarded route
reads YAML, so there is no wire format to get wrong. Reopen with a route if that changes.

**Superseded status line:** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 1, role.go)

`(*Role).MarshalYAML` and `UnmarshalYAML` (role.go:471, 499) are not ported, the same call as
[D-119] made for `Job`. They round-trip the three timestamps through
`timeutils.FormatMillis`/`ParseFormatedMillis` — both of which **are** ported — so the missing piece
is only the YAML codec itself and a decision about which crate provides it.

Where it matters: the config export/import path, which is `server/channels/` and unported. Nothing
in `mm-model` needs it today.

Two things to carry into that session: the YAML struct is a *different shape* from the JSON one —
the timestamps are **strings** in YAML and numbers in JSON — and `ParseFormatedMillis`'s error text
is [D-118], still unreproduced.

**Extended 2026-08-19 to `Scheme`**, which has the identical pair (scheme.go:73, 116) with the same
three string timestamps and the same shape difference. Both should land together; they are one
decision, not two.

---

## D-129 · `behaviour_role.json` was regenerating itself on every run

**Status** CLOSED · **Severity** unverified · **Raised and closed** 2026-08-19 (phase 1, scheme.go)

`behaviour_role.go` seeded its `IsValid` corpus with `model.NewId()`, so every `go run .` rewrote
twenty lines of a committed fixture with a fresh random id. That is [D-032]'s defect exactly, and
the rule it breaks is the one that makes the generator useful: *"Output is deterministic, so a clean
run touches only the new files; anything else in `git status` is a signal."* With the fixture
churning on every run, that signal reads as noise and the next real drift hides inside it.

Found the way the rule intends — a clean regeneration during the scheme.go session showed
`M fixtures/behaviour_role.json` with nothing else to explain it.

Both corpora now pin a **fixed** id (the one the registry filler already produces for
`fixtures/role.json` and `fixtures/scheme.json`) and the generator calls `model.IsValidId` on the
literal, so a mistyped constant panics in the generator rather than silently producing a corpus
whose "valid id" case tests the invalid branch. Verified: two consecutive runs now leave the file
byte-identical.

**The committed fixture changes as a result** — twenty `id` values move from a random 26-character
string to the pinned one. No Rust test asserts the id's *value*; they assert the verdicts it
produces, which are unchanged.

**Note for whoever reviews PR #5:** the defective generator is in that PR. The fix rides on the
scheme.go branch, so #5 carries a fixture that rewrites itself until this lands.

**Related** [D-032] (a fixture whose hashes were regenerated per run), [D-069] (the `TZ` pinning
that protects the same signal).

---

## D-130 · The Go server we compare against is a different version from the source we port

**Status** OPEN (reopened 2026-09-06) · **Severity** divergence · **Raised** 2026-08-19 (phase 2)
**Affected** every cross-server parity claim in `mm-api`.

`docker-compose.yml:59` pins `mattermost/mattermost-team-edition:**latest**`. The reference tree is
pinned by SHA to **11.11.0** (`versions[0]` in version.go). The container currently reports
`X-Version-Id: 11.10.0...` — **one minor behind**, and drifting on its own schedule, since `latest`
moves and the SHA does not.

**How it was found.** The `Roles` table holds 24 rows the Go server wrote at startup, which looked
like the best possible oracle for the generated default-role table. Diffing them showed 13 of 24
roles disagreeing **in both directions**:

- *Generated-only*: `sysconsole_read_ai_recaps`, `manage_public_channel_auto_translation`,
  `manage_private_channel_auto_translation` — permissions 11.11.0 adds and 11.10.0 has never heard
  of.
- *Database-only*: `purge_bleve_indexes`, `create_post_bleve_indexes_job`,
  `sysconsole_read_experimental_bleve` — permissions 11.11.0 lists as **deprecated** and no longer
  grants by default.
- Plus app-layer augmentation: the server expands ancillary permissions before writing, so the
  persisted set is not `MakeDefaultRoles()`'s output under any version.

**What this costs.** Two things, and the second is the one that matters:

1. The live database cannot verify the generated role tables. Handled — the DB tests assert store
   behaviour (rows parse, round-trip, deleted rows resolve) rather than model content.
2. **Every `mm-api` parity test is comparing our 11.11.0 port against an 11.10.0 server.** The
   vertical slice's byte-identical claim for `/users/me`, the four migrated routes, the
   forwarded-route assertions — all of them measured against a server one minor out of step. Wire
   formats rarely change between minors, which is why nothing has caught fire, but "rarely" is not
   the standard the rest of this project holds itself to, and nobody had recorded the gap.

**Paid 2026-08-19: the image is pinned and the volume was recreated.** The alternative — re-pinning
the reference SHA to 11.10.0 — was rejected because every ported file cites line numbers in the
11.11.0 tree, in `MIGRATION.md` and in a `Port of ...` doc comment on essentially every public item.

`docker-compose.yml` now pins **`11.11.0-rc1`**, not `latest`. Two things about that tag:

- There is no final `11.11.0` image. The pinned SHA (2026-08-13) is pre-release, and the only
  published artifacts for the minor are `11.11.0-rc1` and the moving `release-11.11`. rc1 is the
  closest **fixed** tag; `release-11.11` would reintroduce exactly the drift this entry is about.
- Move the pin to `11.11.0` when it ships, and recreate the volume again — the migrations are
  one-way in the other direction too.

`X-Version-Id` now reports `11.11.0`, and **every suite passes against it**: the four `mm-api`
parity files (including the byte-identical `/users/me`), the store suite and the authorization
suite. So the skew had not in fact broken anything — but the claim is now measured rather than
hoped, which was the whole point.

**Two consequences worth knowing.**

1. Recreating the volume mints new ids, and three test files hardcoded them. They now discover what
   they need: the parity tests take the user id from the login response, and the authorization tests
   query for the admin and insert their own plain users. A hardcoded id from a recreated volume
   fails as *"permission denied"*, which is the most misleading possible symptom for a permission
   test.
2. A fresh volume has no team, and `parity_session_team_members` failed with the message it was
   written to produce — *"the fixture user belongs to no team, so the parity assertions would be
   vacuous"*. That test earned its keep. The README now documents the two setup calls.

**Related** [D-136], which is what re-running the role diff against the matched version revealed.

**Related** [D-069] (the `TZ` pin, the same class of "the environment is part of the oracle").

---

**Reopened 2026-09-06 — the skew is now measured, and it is not one-directional.** This entry
closed on "every suite passes against it", which was true of every route ported at the time and is
not a claim about routes not yet looked at. `GET /api/v4/bots` is the first one where the pinned
source and the running `11.11.0-rc1` image disagree about the **wire format**, and probing turned
up a second disagreement pointing the other way:

| | pinned SHA (2026-08-13) | running `11.11.0-rc1` |
|---|---|---|
| `system_owned` on a bot | **absent** — the string does not occur anywhere in `reference/mattermost/` | **present**, and it is the object's *first* key |
| `GET /users/{user_id}/channel_join_requests` | registered (`api4/channel_join_request.go:31`) | **404** |

Both differences fit one ordering: **rc1 is an earlier cut of 11.11.0 than the pinned SHA** —
`channel_join_requests` added after rc1, `system_owned` removed after it. Pre-release churn inside
a minor, which is exactly what a `-rc1` tag invites and what the note above anticipated without
being able to measure.

**What this costs.** The reference is neither a superset nor a subset of the forward target, so
"read the Go source" and "ask the running server" can give different answers for the same route,
and neither is wrong. Where they agree — every route served so far — nothing changes. Where they
disagree the route cannot be ported honestly: matching the source produces a body our own proxy's
target does not serve, and matching the server means reverse-engineering a field whose semantics
are not readable anywhere.

**What is owed:** run an image built from the pinned SHA. The Dockerfile is in the reference tree
and the build is a Go build, so this is buildable rather than blocked on a publish — and it would
also drop the qemu emulation, since it would be built for this host's architecture. Until then, a
route whose live shape does not match the source is **skipped and recorded**, not guessed at.

**Blocked on this so far:** `GET /api/v4/bots` (`getBots`).

---

**CLOSED 2026-09-08 — the forward target is now built from the pinned SHA.** Not an image: a
native binary, run on the host by `scripts/go-server.sh`, against the same Postgres. The published
image is still defined in `docker-compose.yml` behind a `published-image` profile and is no longer
started by `docker compose up -d`.

The one thing that made this look harder than it was: `server/go.mod` requires the **published**
`github.com/mattermost/mattermost/server/public v0.4.0`, not the copy sitting in the same
checkout, so a plain `go build` fails with a screen of undefined `model.` symbols that reads like a
broken clone. Upstream's `make setup-go-work` writes a `go.work`; the script writes an equivalent
one *outside* the reference tree, with absolute paths, so the reference stays untouched.

**Both measured differences are gone:** `GET /api/v4/bots` no longer carries `system_owned`, and
`api4/properties.go`'s routes answer their handler's own error instead of a mux 404.

**Two families are still unregistered, and it is not version skew.** `api4/view.go`'s seven routes
are inside `if api.srv.Config().FeatureFlags.IntegratedBoards`, and
`api4/channel_join_request.go`'s seven are inside `if !...FeatureFlags.DiscoverableChannels
{ return }`. Both are off by default at the pinned SHA, so `scripts/routes.py` counts fourteen
routes the running server does not serve. Turning either flag on is a deliberate act with its own
parity run — `DiscoverableChannels` also changes `getChannel`, which [D-153] pins as off — and it
is what those routes need before they can be ported, not a version problem.

**Two consequences worth knowing before the next session:**

1. **The migrations are one-way.** The pinned SHA is later than rc1, so its first boot migrated the
   shared database forward and rc1 can no longer read it. Falling back means recreating the volume.
2. **The suite got roughly six times faster** — 244s to 38s for `--test parity` — because the
   forward target is no longer emulated. That speed exposed six latent races in the parity suite
   itself, all of the same shape: a socket assertion counting frames on the *shared admin*, or
   waiting a fixed number of milliseconds for one. They are fixed rather than papered over; see
   `common::BROADCAST_STREAM` and `SocketProbe::collect_until`.

---

## D-168 · 41 of 700 mutation-plan anchors no longer match the tree

**Status** OPEN · **Severity** unverified · **Raised** 2026-09-06 (phase 2, mutation harness)

`scripts/preflight-plans.sh` — new, and the point of the entry — checks every committed plan's
`from` pattern against the current source using the same `printf %b` the runner uses. Today it
reports **42 stale anchors out of 700 plan lines**, spread over roughly a dozen plans.

Two failure modes, and the second is why this is not cosmetic:

| | what `mutate.sh` does | cost |
|---|---|---|
| **0 matches** | exits 3; `set -e` aborts the batch | loud — every mutation after it is thrown away |
| **2+ matches** | replaces the **first** occurrence | **silent** — the verdict belongs to a function nobody meant to test |

Both kinds are present. Some anchors rotted long ago; several rotted **today**, because a route
added handlers to a file an earlier plan already anchored on — `incoming-hooks.plan`'s
`api-trailing-newline` matched once when it was written and matches three times now that
`webhooks.rs` holds four handlers.

**What this does *not* invalidate.** Every plan run this session passed `mutate-batch.sh`'s own
pre-flight at the moment it ran, so each reported tally was measured against an unambiguous anchor.
The staleness is about **re-running** them later, which is exactly what a committed plan is for.

**What is owed:** re-anchor the rest, one plan at a time, re-running each to confirm the verdicts
still hold. **One of the 42 is done** — `oauth-apps.plan`'s `api-list-trailing-newline`, which had
gone ambiguous the same day when `getAuthorizedOAuthApps` landed in `oauth.rs` with a
byte-identical last line; the anchor now includes the two lines above the return and the re-run
reproduced the original tally exactly. **41 remain**, and that is the shape the rest should take:
re-anchor, then re-run. Fixing an ambiguous anchor *without* re-running its plan trades a
known-stale line for an unverified one, which is why the other dozen plans were left alone in the
session that found them.

**Where the pin lives:** the header comment on `scripts/preflight-plans.sh`, which explains both
failure modes and why the unescaping has to be the runner's own.

---
## D-131 · The role and scheme stores are read-only

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 2, role/scheme stores)

Ported: `RoleStore.Get`, `GetAll`, `GetByName`, `GetByNames`; `SchemeStore.Get`, `GetByName`,
`GetAllPage`, `CountByScope`. That is what a permission check needs, and it is deliberately where
this session stopped.

Not ported, and grouped by what each would cost:

- **Writes** — `RoleStore.Save`, `SavePreservingUnknownPermissions`, `Delete`,
  `PermanentDeleteAll`; `SchemeStore.Save`, `Delete`, `PermanentDeleteAll`. `Save` is not a simple
  upsert: it runs `validateForSave` (role_store.go:118), which is where MM-68830's
  unknown-permission tolerance lives, and `SchemeStore.Save` creates the scheme's **roles** in the
  same transaction (scheme_store.go:102) and calls `filterModerated` (:317). A write session, not
  an afternoon.
- ~~**`ChannelHigherScopedPermissions`**~~ — **landed 2026-08-19**, with the `IN` list
  parameterised rather than interpolated ([D-133]) and both upstream quirks reproduced ([D-132]).
- **`AllChannelSchemeRoles`**, **`ChannelRolesUnderTeamRole`** (:438, :478) — both only matter once
  schemes exist, which on Team Edition they never do.
- **`CountWithoutPermission`** (scheme_store.go:480) — a system-console statistic.

Nothing downstream is blocked: a permission check resolves roles by name, which `GetByNames`
covers.

---

## D-132 · `ChannelHigherScopedPermissions` splits the permission column differently from every other read, and keys the result on `""`

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 2, role store)

Two upstream quirks in the same twelve lines (role_store.go:431-434), both reproduced.

**1. `strings.Split(s, " ")`, not `strings.Fields`.** Every other read of `Roles.Permissions` —
`ToModel`, and therefore `Get`, `GetAll`, `GetByName`, `GetByNames` — uses `strings.Fields`, which
collapses whitespace runs and drops empties. This one splits on a single space with no collapsing.
Since the writer emits a **leading space** per entry, the resulting list always begins with an
**empty string**, and an empty column becomes `[""]` — a one-element list, not an empty one.

Harmless today: the only consumer is `MergeChannelHigherScopedPermissions`, which asks the list for
membership, and `""` is not a permission id. But "harmless" is a property of the current caller, not
of the function, and two splitters disagreeing about the same column is exactly the difference a
port irons out without noticing. Pinned by a test asserting the empty first element is *present*.

**2. The result map has an empty-string key.** Go writes all three role names for every row
unconditionally, and the first two UNION branches select `''` for the two names they do not carry.
So `map[""]` is written on every row, and its value is whichever row the database returned last —
which is a row order, not a defined answer. No caller looks up `""`, so the value never matters;
the *presence* is asserted, the value deliberately is not.

A port that skipped empty names would produce a map that is more sensible and not Go's.

---

## D-133 · Go builds the higher-scoped permissions query by string interpolation; this port binds

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 2, role store)

`channelHigherScopedPermissionsQuery` (role_store.go:411) assembles its `IN` list with
`strings.Join(roleNames, "', '")` and formats it straight into the SQL text. A role name containing
an apostrophe would break the statement or inject into it.

**Not reachable through the API today**: `IsValidRoleName` restricts names to `[a-z0-9_]`, and every
role that arrives over the wire passes it. It is reachable through any row written by something that
did not validate — a migration, an import, a direct `INSERT`, or a future Go caller that forgets.

This port binds `$1` as a `text[]` instead. For every legal role name the two produce identical
results, which is what the DB-backed test measures. **Deliberately not bug-compatible**: reproducing
a string-interpolated query to be faithful would be choosing fidelity over the one property the two
servers must never differ on, and the Strangler Fig has no mechanism for "our SQL injection matches
theirs".

Worth reporting upstream if this project ever files anything upstream. Recorded here either way, so
the difference is a decision rather than an accident.

---

## D-134 · Four stores still block sixteen functions in `authorization.go`

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 2, authorization)
**Narrowed** 2026-08-21 — title and scope rewritten; it is no longer "most of" the file.

**Ported: 20 of 36.** Every system-, team-, channel-, user- and post-scoped check, in both the
session-scoped and `askingUserId` forms:

`RolesGrantPermission`, `HasPermissionTo`, `SessionHasPermissionTo`, `…ToAndNotRestrictedAdmin`,
`…ToAny`, `…ToTeam`, `…ToTeams`, `…ToChannel`, `…ToChannels`, `…ToUser`, `…ToChannelByPost`,
`…ToReadPost`, `…ToReadChannel`, `HasPermissionToTeam`, `HasPermissionToChannel`,
`HasPermissionToUser`, `HasPermissionToChannelByPost`, `HasPermissionToReadChannel`,
`HasPermissionToResolveChannelMention`, `HasPermissionToChannelMemberCount` — plus
`GetRolesByNames` and the higher-scoped merge behind it.

**Remaining: 16, and every one is blocked on a store rather than a decision.**

| Blocked on | Functions |
|---|---|
| group store | `SessionHasPermissionToGroup` |
| sidebar-category store | `SessionHasPermissionToCategory` |
| bot store | `SessionHasPermissionToManageBot`, `SessionHasPermissionToUserOrBot` |
| property store (+ post store) | the 11 property-field functions, 6 public and 5 private |
| — (enterprise ABAC, permanently out of scope) | `HasPermissionToFileAction` |

`SessionHasPermissionToManageBot` is the one to read first when the bot store lands: it returns
`*model.AppError` rather than a bool, deliberately, so the failure can be told apart — and
`…ToUserOrBot` branches on the *error id and Where* (`store.sql_bot.get.missing.app_error` /
`SqlBotStore.Get`), so the error identity is wire-load-bearing, not decoration.

**Two claims in the original entry were wrong and are corrected here.** The by-post group was
listed as blocked on a post store — it is not. `GetForPost` returns a `Channel` and
`GetMemberForPost` returns a `ChannelMember`; both are **channel-store** queries that merely join
through `Posts`, and neither needs a post model. They were ported 2026-08-21 on that basis. And
`SessionHasPermissionToAndNotRestrictedAdmin` was listed as blocked on `Config`; it needed one
bool, now modelled in `mm-app/src/config.rs` — see [D-156] for what that costs.

Everything below is the original entry.

What is left, grouped by what each is actually waiting for:

- **A channel store** — `SessionHasPermissionToChannel`, `…ToChannels`, `…ToChannelByPost`,
  `…ToReadPost`, `…ToReadChannel`, `HasPermissionToResolveChannelMention`,
  `HasPermissionToChannelMemberCount`. This is the largest group and the most valuable: channel
  membership is what most api4 handlers gate on. It needs `ChannelStore.GetMember` and the channel
  scheme resolution, which is the same shape `team_store.rs` already does for teams.
  **Corrected 2026-08-19 (same day, next session).** The claim above — repeated from this entry's
  own original text — that `SessionHasPermissionToChannel` needs `ChannelStore.GetMember` **is
  wrong**. It calls `GetAllChannelMembersForUser` and `ChannelStore.Get`, and never touches
  `GetMember` (authorization.go:119, :106). `GetMember` landed first on the strength of that
  misreading; it is a correct port of a method the api4 handlers do need, but it did not unblock
  this check. **Both real prerequisites landed 2026-08-19** and
  `SessionHasPermissionToChannel` is now ported and verified against the running Go server.
  `…ToChannelByPost` and `…ToChannels` are still blocked; see [D-137] for which method each wants.
- **`Config`** — `SessionHasPermissionToAndNotRestrictedAdmin` reads
  `ExperimentalSettings.RestrictSystemAdmin`. config.go is translated lazily; this is one bool.
- **Other stores** — `…ToGroup` (group store), `…ToCategory` (channel category store),
  `…ToManageBot` (bot store, and it returns an `*AppError` rather than a bool, deliberately, so the
  failure can be told apart), the three property-field checks (property store).
- **`HasPermissionTo*`** — the `askingUserId` variants of everything above. They differ from the
  session variants in one way that matters: they load the user's roles from the database instead of
  taking them from the session, so they see role changes a live session does not.

Nothing here is blocked on a decision; each is blocked on a store.

---

## D-135 · A `jsonb` column holding JSON `null` was treated as a decode failure

**Status** CLOSED · **Severity** blocking · **Raised and closed** 2026-08-19 (phase 2, authorization)

`SqlUserStore.get` decoded `props`, `notifyprops`, `timezone` and `mfausedtimestamps` with
`serde_json::from_value`, mapping any error to `StoreError::Decode`. Those columns are `jsonb`,
which distinguishes SQL NULL from the JSON value `null` — and the Go server writes the latter.
**Four of the five users in the development database have `mfausedtimestamps = 'null'::jsonb`**, and
every one of them failed to load with

```
User.mfausedtimestamps held JSON that does not decode into the model type
```

**`GET /users/me` — the vertical slice's flagship route, the one whose byte-identical response is
the project's headline claim — was a 500 for four users out of five.** It passed every test because
the parity suite logs in as `sliceuser`, whose column holds `[]`.

Go's `json.Unmarshal` turns a JSON null into a nil map or slice without complaint, so both null
shapes mean "absent" and only a *type* mismatch is an error. Fixed by matching
`None | Some(Value::Null) => None` in both decoders. No wire change: the field carries `omitempty`
in Go and `skip_serializing_if` here, so an absent value is an omitted key either way — the fix
turns a 500 into exactly the bytes Go sends.

**Found by accident**, which is the part worth keeping. A `SessionHasPermissionToUser` test needed
to act on a user who was not a system admin, picked one at random from the database, and it denied.
The check was right; the store underneath it was not.

**The lesson for the corpus, not just the code:** a test suite that always reads the same row is
testing that row. The store suite now reads **every** user in the database and asserts each decodes,
which is what a regression test for this class has to look like.

**Related** [D-130] (the other "the environment is part of the oracle" finding from the same
session).

---

## D-136 · `MakeDefaultRoles()` is the seed, not the effective permission set

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 2, after [D-130])

With the server and the source finally on the same minor, the generated default-role table was
diffed against the 24 rows the Go server writes at startup. The result is asymmetric, and the
asymmetry is the finding:

- **Nothing is generated-only.** For all 24 roles, every permission our table claims is one the
  running server also grants. That was the dangerous direction — a port claiming a permission the
  reference does not grant would over-grant — and it is empty.
- **Ten roles have database-only extras.** `channel_admin` gains `create_post` and `add_reaction`,
  `system_user` gains `create_emojis`/`delete_emojis`, `team_user` gains the two playbook-create
  permissions, and so on.

The mechanism is not version skew this time. The `systems` table lists roughly forty completed
permission migrations — `add_channel_bookmarks_permissions`, `add_edit_file_attachment_permission`,
`EmojisPermissionsMigrationComplete`, `add_channel_auto_translation_permissions` and the rest — that
the server runs at startup and that **add permissions to existing roles**. `MakeDefaultRoles()` is
the seed a fresh install starts from; the persisted rows are that seed plus every migration since.

**What this means for the port, and it is not "fix the table".** The table is a faithful port of
`MakeDefaultRoles()` and should stay one. But it is **not** the answer to "what may a `system_user`
do" on a running server, and nothing should use it that way. `RolesGrantPermission` already reads
the `Roles` table rather than the generated constants, which is correct and now demonstrably load
bearing: answering from the table would deny `create_post` to every channel admin.

The migrations themselves live in `channels/app/` and are a phase-3 concern — they only matter for
standing up a *new* database, which this project never does. Recorded so the next person to compare
the two does not conclude the generator is broken.

---

## D-137 · The channel store is one method out of ninety

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 2, channel store)

Ported: `ChannelStore.Get`, `GetMember`, `GetAllChannelMembersForUser`, and the two separate role
resolvers behind them (`getChannelRoles` and `allChannelMember.Process` — see [D-142]).
`store.ChannelStore` (store/store.go:200-386) declares roughly ninety methods; these are three.

`GetMember` was ported first on the belief that it was what `SessionHasPermissionToChannel` needed.
It is not — that check uses the plural read — but it is what the api4 channel-member handlers use,
so the work stands. The corrected dependency is recorded in [D-134].

What the next channel-scoped work will actually reach for, in the order it will need it:

- **`GetMemberForPost`** (channel_store.go:2479) — `SessionHasPermissionToChannelByPost` and
  `…ToReadPost` both go through it. It is the same `channelMemberWithSchemeRoles` shape with a join
  through `Posts`, so `get_channel_roles` is already the whole hard part.
- **`GetAllChannelMembersForUser`** (:2554) — `…ToChannels` resolves many channels at once and Go
  answers it from a cache keyed on the user. The cache is the interesting part, not the query, and
  the standing "Rust reads through, never caches" decision from the vertical slice applies.
- **`Get` / `GetMany`** — the `Channels` row itself, which nothing ported yet needs but every
  channel *route* will.
- **`GetMemberCount`**, **`GetMemberCountsByGroup`** — `HasPermissionToChannelMemberCount`.

Nothing here is blocked on a decision, and unlike [D-131] there is no write-path question yet:
every one of the above is a read.

**Related** [D-134] (what in `authorization.go` this unblocks and what it does not).

---

## D-138 · `getChannelRoles` has no fixture oracle, and cannot have one

**Status** ACCEPTED · **Severity** verification gap · **Raised** 2026-08-19 (phase 2, channel store)

`CLAUDE.md` requires a behavioural corpus in `reference/dump/` for anything with branching logic.
`getChannelRoles` (channel_store.go:248) and `channelMemberWithSchemeRoles.ToModel` (:313) are
**unexported**, so the oracle program cannot call them — the same wall `getTeamRoles` hit in
[D-077], and the reason that port has no fixture either.

The three ways out, and why the chosen one is the chosen one:

- **Transcribe the Go source into the corpus.** This is what the fixture rule exists to prevent: the
  oracle would assert what the reading already believed.
- **Read it with `go/parser`**, as `behaviour_version.go` does for the unexported `versions` table.
  That works for *data*. `getChannelRoles` is a function with branches, so an AST read recovers the
  source text and not the behaviour.
- **Ask the running Go server.** `GET /api/v4/channels/{id}/members/{id}` returns the exact struct
  `ToModel` builds, and the server reads through — a `Roles` column changed underneath it is
  reflected on the next request, verified before the suite was written. Chosen.

`crates/mm-store/tests/db_channel_members.rs` is that oracle: fourteen role shapes, each written
into the shared membership row, asked of both servers, and compared as **whole serialised
`ChannelMember` documents** rather than just the role strings. The measured answers are transcribed
into unit tests in `channel_store.rs` so a regression still fails without Docker.

**What this costs.** The oracle needs `docker compose up`, a fixture user, and write access to rows
the Go server owns; it is skipped unless `MM_STORE_DB=1` **and** `MM_PARITY_STACK=1`. So the
default `cargo test` run checks the transcriptions, not Go. Three mutations were run against the
full suite to confirm the difference is real: swapping the channel/team scheme precedence, widening
the `Channels` join from INNER to LEFT, and reading `TeamScheme.DefaultTeam*Role` instead of
`DefaultChannel*Role`. **All three passed the unit tests and were caught only by the live oracle** —
which is the honest measure of what this entry is admitting.

**Related** [D-077], [D-130] (the environment is part of the oracle).

---

## D-139 · Test users persist between runs, by design, and one is now an input to another suite

**Status** ACCEPTED · **Severity** hygiene · **Raised** 2026-08-19 (phase 2, channel store) ·
**Corrected** 2026-08-19 (same day, next session)

The development database holds `mmrs_auth_plain` (`mmrsauthplainuserxxxxxxxxx`) after any run of
`db_authorization.rs`.

**The original entry said no purge covers `users`. That was wrong** — `db_authorization.rs:64`
deletes `WHERE id LIKE 'mmrsauth%'`, users included. The row survives because that suite purges at
the **start** of each test rather than the end, which is deliberate and documented in the file: a
failing assertion panics past any trailing cleanup, so start-of-test is the only purge that runs
unconditionally. The cost is that the last test of a run leaves its rows behind.

What remains true, and is why this entry stays open rather than being deleted: after [D-135], the
user-store suite reads **every** user in the database and asserts each decodes. A row left behind
by one suite is therefore an input to another. It is currently a *useful* input — an extra user
shaped differently from the ones the Go server creates for itself — but it arrived as a side effect
rather than as a fixture, and nothing states that the second suite depends on the first having run.

**What to do**: either give the user-store suite its own deliberately-shaped rows so it does not
depend on leftovers, or add a purge to the end *as well as* the start and accept that a panicking
test leaves rows behind until the next run cleans them. Not neither.

---

## D-140 · Every ported store read goes to the master; Go chooses master or replica per request

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-19 (phase 2, channel store)

`SqlChannelStore.GetMember` takes an `rctx request.CTX` and issues its query through
`s.DBXFromContext(rctx.Context())`, which is (context.go:31):

```go
if HasMaster(ctx) { return ss.GetMaster() }
return ss.GetReplica()
```

So Go reads from a **replica** unless the caller marked the request with
`RequestContextWithMaster` — which handlers do after a write, to get read-after-write consistency.
This port has one `PgPool` pointed at one database, so every read is a master read. `get_member`
does not take a context parameter at all, which is why the divergence is invisible at the call
site rather than merely unimplemented.

**Not specific to this file.** It is true of `session_store`, `user_store`, `team_store`,
`role_store`, `scheme_store` and `preference_store` equally; it is recorded here because
`GetMember` is the first ported method whose Go signature carries the `rctx` that selects the
handle, making the choice explicit rather than absent.

**Why this is accepted rather than owed.** The divergence is strictly in the safe direction: a
master read is never staler than a replica read, so nothing this port serves can lag something Go
serves. The failure mode is capacity, not correctness — we send read traffic to the primary that
Go would have spread. The development stack has one Postgres and no replica, so the two are
currently the same database and the divergence is unobservable.

**What would change this.** A deployment with real read replicas, at which point the store layer
needs a context parameter carrying the master/replica choice, and every ported method needs its Go
call sites checked for `RequestContextWithMaster`. Doing it now would be threading a parameter
through six stores to select between two names for one pool.

**Related** [D-087] (the other read-consistency finding, and the reason "Rust reads through, never
caches" is the standing decision).

---

## D-141 · `HydrateChannelPolicyActions` is unported, so `Channel.policy_actions` is always absent

**Status** OPEN · **Severity** incomplete · **Raised** 2026-08-19 (phase 2, channel store)

`App.GetChannel` calls `HydrateChannelPolicyActions` after the store read and **only logs** if it
fails (channel.go:2234), returning the channel either way. This port skips the call entirely, so
`policy_actions` is always `None`.

Go's own comment says it is "a no-op on channels with `PolicyEnforced=false`, keeping the cost on
the common no-policy path at zero" — and `PolicyEnforced` is an `AccessControlPolicies` row, which
is an **enterprise** feature and whose table is empty on the Team Edition stack. So on everything
this project can currently run, Go's hydration is also a no-op and the two agree.

**Why it is still owed.** The agreement is environmental, not structural: point this at a licensed
server with a channel policy and `policy_actions` silently differs. Any route that serialises a
`Channel` inherits the gap, which is why it is logged before such a route exists rather than after.

**Related** [D-136] (the other "Team Edition makes the divergence invisible" finding).

---

## D-142 · Go resolves channel-member roles two different ways, and both are on the wire

**Status** ACCEPTED · **Severity** divergence (upstream) · **Raised** 2026-08-19 (phase 2, channel store)

`getChannelRoles` (channel_store.go:248, behind `GetMember`) and `allChannelMember.Process`
(channel_store.go:480, behind `GetAllChannelMembersForUser`) both answer "what roles does this
channel member effectively hold". **They disagree**, and the disagreement is not cosmetic.

A scheme role id sitting literally in the `Roles` column — the un-migrated case Go's own comment
describes — is treated oppositely:

| | `getChannelRoles` | `Process` |
|---|---|---|
| `channel_user` in `Roles`, `SchemeUser` false | sets the flag, drops the literal, appends the **scheme's** user role | leaves `channel_user` in place, appends nothing |
| position of scheme-implied roles | always last | wherever the column put them |

**Measured, not deduced.** A channel was pointed at a scheme whose `DefaultChannelUserRole` was
`mmrs_dv2_channel_user` — a copy of `channel_user` with `read_channel` removed — and a member's
`Roles` column set to the literal `channel_user` with every scheme flag false. One request to the
running Go server, `GET /api/v4/channels/{id}/members/{uid}`, produced **both** answers at once:

- the response **body** reported `"roles": "mmrs_dv2_channel_user"` — a role that does not grant
  `read_channel`;
- the request **succeeded with 200**, which its gate (`SessionHasPermissionToChannel`, the only
  check on that handler) could only do by resolving the same member's roles to `channel_user`.

So Go told the client the member holds a role it was simultaneously not using, and granted on a
role it did not report. Neither answer is "the" answer.

**What this means for the port, and it is the opposite of the tempting fix.** Both functions are
ported, separately, as `get_channel_roles` and `process_all_channel_member_roles`. Unifying them —
which is exactly what a tidy-minded port does, since they are ninety percent identical — would
change one of the two observable behaviours. Sharing the implementation would be a **wire change**
in one direction and a **permission change** in the other, and the permission direction can
over-grant. A unit test asserts the two disagree on the measured input, so a future refactor that
merges them fails rather than silently drifting.

**Note the blast radius is narrow**: the two agree whenever the `Roles` column contains no literal
scheme id, which is every row in a database that has completed the scheme migration. That is why
this survives upstream, and why the test pins the agreement case as well as the disagreement.

---

## D-143 · Every channel permission check scans all of the user's memberships

**Status** ACCEPTED · **Severity** performance · **Raised** 2026-08-19 (phase 2, channel authorization)

`SessionHasPermissionToChannel` asks for **every** channel membership the user has and then looks
one up in the map. Go's comment at the call site is explicit that this is a cache decision, not a
query decision (authorization.go:333):

> We call GetAllChannelMembersForUser instead of just getting a single member from the DB, because
> it's cache backed and this is a very frequent call.

This port has no cache — the standing "Rust reads through, never caches" decision from the vertical
slice ([D-087]) — so it pays the full scan on **every** check. For a user in 500 channels that is
500 rows joined against `Channels`, `Teams` and `Schemes` twice, to answer a question about one.

**Why not just use `GetMember`.** Because the two resolve roles differently ([D-142]), so the
substitution is not behaviour-preserving. The correct narrowing is `GetAllChannelMembersForUser`'s
own query with `AND cm.channelid = $2` appended — same resolver, one row — which is a change worth
making deliberately with the oracle in place rather than as a side effect of this session.

**Not fixed now** because it is invisible on a development stack, and guessing at a performance
shape without a measurement is how the wrong thing gets optimised. Recorded so the first slow
channel route has somewhere to start.

---

## D-144 · The swallowed store error in `session_has_permission_to_channel` is untested

**Status** OPEN · **Severity** verification gap · **Raised** 2026-08-19 (phase 2, channel authorization)

Go wraps the membership read in `if err == nil` (authorization.go:121) and carries on to the
`manage_system` and team branches when it fails. That is the **one place in `authorization.rs`
where a database failure does not immediately deny**, and it is reproduced faithfully — but nothing
exercises it.

A mutation proves the gap rather than asserting it: replacing the fall-through with
`if read.is_err() { return (false, false) }` compiles, changes the semantics, and **passes the
entire suite**, cross-server oracle included. The test database always works, so the branch is
never taken.

**Why it was not closed.** Injecting a store failure needs `App` to be generic over its store, or
the store traits behind a trait object. Both are real design changes to a struct that currently
holds a concrete `SqlStore`, and doing that as a side effect of a porting session is how a
test-shaped tail starts wagging the dog. The unreachable-pool idiom the rest of the suite uses
cannot help here: it fails *every* store call, including the role lookups the fall-through then
depends on, so the check denies for the wrong reason and the test would pass vacuously.

**The risk if it is wrong** is bounded and in the safe direction — denying where Go grants is an
outage, not a breach — which is why this is logged rather than blocking.

**Related** [D-142] (the other finding from the same mutation run), and the mutation-harness rules
in `MIGRATION.md`'s `app/authorization.go` notes, which are what caught the bogus verdict from a
mutation that did not compile.

---

## D-145 · A parity suite passed while every request was being forwarded to Go

**Status** CLOSED · **Severity** blocking (test validity) · **Raised and closed** 2026-08-20 (phase 2, first gated route)

The channel-member route's five parity tests passed on their first run. All five were comparing
**Go against Go**: a stale `mm-api` process from an earlier session still held port 8066, the
freshly built binary logged `Address already in use` and exited, and every request to :8066 was
answered by the old binary — which had no channel route and therefore proxied.

Found by eye, not by a test: the response carried `x-mmrs-served-by: go`.

**What made it possible.** The marker header existed and nothing read it. `fetch_both` asserted a
200 from each server and compared bodies; a forwarded response satisfies both.

**Closed** by asserting in `fetch_both`/`fetch_both_raw` that the Rust side answered with
`x-mmrs-served-by: rust`, which immediately exposed a second gap: **error responses carried no
marker at all**, because only the success paths set it. So a 403 from a migrated route and a 403
forwarded to Go were indistinguishable to an operator mid-cutover as well as to the suite. Fixed
in `ApiError::into_response`.

**The general lesson, and it is the third instance of the same shape.** A test harness that cannot
tell whether it exercised the code under test will report success for anything: a stale *build*
([D-129] and the `app/authorization.go` notes), a mutation that failed to *compile* (same notes),
and now a stale *process*. Each was found by looking at something other than the pass/fail line.
Every harness in this project should be able to answer "did this actually run the thing" and fail
loudly when it cannot.

---

## D-146 · `ApiError` is 192 bytes by value and every API `Result` pays for it

**Status** OPEN · **Severity** cosmetic · **Raised** 2026-08-20 (phase 2, first gated route)

`ApiError` wraps `AppError` by value, and `AppError` is large because it *is* the wire format —
seven fields a client parses. Every handler returns `Result<Response, ApiError>`, so every one
moves 192 bytes on the error path.

Clippy's `result_large_err` only fires where the `Ok` side is small, which so far is two helpers in
`channels.rs`; both carry a local `#[allow]` with the reasoning. Boxing there alone would buy
nothing and would make those two signatures differ from every other in the crate.

**The real fix** is `ApiError(Box<AppError>)` crate-wide: `From<AppError>` boxes, `?` keeps
working, and field access still auto-derefs. It touches roughly ten construction sites across five
files. Deliberately not done as a side effect of migrating a route — it is a mechanical change
that deserves its own diff, and `mm-store`'s `StoreError::Invalid` already boxes its `AppError`
for exactly this reason, so the precedent is set.

---

## D-147 · The parity suite could only ask questions as a system admin, and one mutation still escapes

**Status** OPEN · **Severity** verification gap · **Raised** 2026-08-20 (phase 2, first gated route)

Every cross-server parity test authenticates as `sliceuser`, a `system_admin`. Branch 5 of
`SessionHasPermissionToChannel` grants on `manage_system` **regardless of which permission was
asked for**, so for that actor the handler could name any permission and every test would still
pass. A mutation replacing `PermissionReadChannel` with `PermissionManageSystem` survived the whole
suite.

**Mostly closed** by `a_non_admin_is_granted_by_membership_and_refused_without_it`, which creates a
real non-admin through Go's API, joins it to one of two fresh channels, and compares both the grant
and the refusal. That mutation is now caught.

**What still escapes**: swapping `read_channel` for another permission that `channel_user` *also*
holds — `create_post`, for instance — is invisible, because no actor in the suite distinguishes
them. Catching it needs a role granting one and not the other, i.e. a custom role written to the
`Roles` table, which Go caches by name and which the parity tests deliberately do not touch.

The undetected direction is the benign one: asking for a permission the member also has still
refuses everyone who should be refused. The dangerous direction — asking for something weaker or
unrelated that a non-member holds — is covered. Recorded rather than closed because "mostly" is
not "entirely".

---

## D-148 · A parity run silently removed a fixture user from a channel

**Status** CLOSED · **Severity** hygiene · **Raised and closed** 2026-08-20 (phase 2, first gated route)

Between two checks a session apart, `sliceuser` stopped being a member of `off-topic` — one of the
development database's two channels. Nothing failed; it was noticed only because an unrelated test
then needed two channels and found one.

**Reproduced but never attributed.** Running the parity file end to end removes the membership;
running any single test in it does not, restoring in between each time. The cause is an
interaction, and the most likely one is an earlier version of the sanitiser test that picked "the
first user who is not me" out of `GET /users`, added them to the **team**, and removed them at
teardown — and removing someone from a team removes them from every channel on it.

**Closed by removing the dependency rather than by finding the culprit.** Every fixture the suite
needs — channels, second users — is now created by it and unwound by it, and
`purge_api_fixtures` clears what a panicking run leaves. Two consecutive full runs now leave
byte-identical database state, and the two development channels survive both.

Two smaller findings fell out of the fix and are worth keeping:

- **Go archives rather than deletes.** `DELETE /api/v4/channels/{id}` sets `DeleteAt` and the name
  stays taken, so a second run's create fails with `save_channel.exists`. The same is true of a
  soft-deleted user's username. Neither can be undone through the API, which is why the purge
  reaches for the shared database.
- **`PublicChannels` is a shadow table** Go keeps in step with `Channels`, with its own
  `(Name, TeamId)` uniqueness. Purging `Channels` alone leaves it behind, and the next create fails
  with a **500** rather than the 400 a leftover `Channels` row gives. Two symptoms, one cause, and
  only the second names the table.

**The rule this settles**: a test that mutates rows it did not create has no business asserting
anything about them.

---

## D-149 · Our 400s carry less information than Go's, and it hides the validation order

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-20 (phase 2, first gated route)

A consequence of [D-092] sharper than "prose versus id". `AppError` serialises `id`, `message`,
`detailed_error`, `request_id` and `status_code` — **not `params`**. So for an invalid path
segment, the name of the offending parameter reaches a client *only* through the translated
message:

```
go:   "message": "Invalid or missing channel_id parameter in request URL."
ours: "message": "api.context.invalid_url_param.app_error"
```

Go's client is told **which** parameter to fix; ours is told only that one of them is wrong. Both
carry the same `id`, so code branching on `id` is unaffected — but a human, or any client
surfacing `message`, gets strictly less from us.

**It also makes a behaviour untestable over HTTP.** Go validates the channel id before the user id
(`RequireChannelId().RequireUserId()`, each returning early once an error is set), so a request
with both malformed reports `channel_id`. A mutation swapping the two survived every cross-server
test, because the ordering is invisible in a body that never names the parameter. It is pinned
instead by a unit test on `validate_ids` in `mm-api/src/channels.rs`.

**Paying off [D-092] closes this too**, and this is the concrete argument for doing so: it is the
first place where the missing translation costs a client information rather than only polish.

## D-150 · Go's router rejects path segments outside `[A-Za-z0-9]+` before any handler runs

**Status** CLOSED (2026-08-20) · **Severity** divergence · **Raised** 2026-08-20 (phase 2,
`getChannelUnread`)

Every id-shaped path parameter in api4 is registered with an explicit charset —
`{channel_id:[A-Za-z0-9]+}` (api.go:203, :223, and 91 occurrences in that file). gorilla/mux
treats it as part of the *route*, not as validation, so a segment containing anything else matches
no route at all and falls to the mux `NotFoundHandler`:

```
GET /api/v4/users/me/channels/no-pe/unread
go:   404 {"id":"api.context.404.app_error", ... ,"status_code":404}   ← and no request_id
ours: 400 {"id":"api.context.invalid_url_param.app_error", ... }       ← before the fix
```

axum's `{name}` matches a whole segment, so the request reached our handler and `IsValidId`
answered 400. **Different status, different id, different body, on a request Go never routed.**

Closed by a middleware layer (`mux_segments_or_forward`, `mm-api/src/lib.rs`) on the parameterised
routes: any `*_id` parameter outside the charset is **forwarded to Go** rather than handled.
Forwarding beats reproducing Go's 404 body — that body interpolates the request URL into
`detailed_error` and carries no `request_id`, and neither detail has to be kept in step if Go
writes it.

**One exception is real and is named in the code:** `plugin_id` is
`[A-Za-z0-9\_\-\.]+`. Every other `_id` parameter across all 21 distinct patterns in api.go is the
narrow class. Nothing here registers a plugin route yet; the exception exists so that adding one
does not silently inherit the wrong rule.

**The finding that outlives the entry.** This was invisible until a test asked for it, and it will
apply to *every* parameterised route added from here on — the two migrated ones shared the bug.
A route's path is wire format too: matching a request Go would not have routed is a divergence
before the handler writes a byte.

## D-151 · `messageChannelTypes` in `GetChannelUnread` is unreachable through its own route

**Status** OPEN · **Severity** untested-by-the-oracle · **Raised** 2026-08-20 (phase 2,
`getChannelUnread`)

`SqlChannelStore.GetChannelUnread` filters `Channels.Type IN (O, P, D, G)` (channel_store.go:937).
Deleting that predicate **passed the entire cross-server parity suite**, for two compounding
reasons:

1. A board (`BO`/`BP`) or space (`S`) channel cannot be created through the REST API on Team
   Edition, so no fixture the parity suite can build has one.
2. Even given one, `getChannelUnread`'s permission check calls `SqlChannelStore::Get` first, which
   applies the **same** filter, misses, and denies with a 403 — so the predicate in this query is
   dead code as long as the one in `Get` is correct.

Closed *at the store level* by `crates/mm-store/tests/db_channel_unread.rs`, which inserts a `BO`
channel with a membership row directly and asserts the miss, with an identically-shaped `O` row as
the control. That test is **transcribed from Go's SQL, not measured against Go** — `SqlChannelStore`
is not reachable from a test, and the route in front of it cannot express the input. If upstream
widens `messageChannelTypes`, the test keeps passing while the port drifts.

**What is still owed:** an oracle for `messageChannelTypes` itself. The list appears in at least
`Get`, `GetChannelUnread` and the search paths, and today each port transcribes it independently.
One shared constant plus one generated fixture would make a widening upstream a compile-time or
test-time event rather than a silent divergence. Cheap, and worth doing before the third
transcription.

**2026-08-20:** `GetByNames` landed as the third transcription (`get_by_names`, same file),
pinned by the same shape of store-level test (`db_channel_get_by_names.rs`). The shared oracle is
now overdue rather than merely worth doing.

## D-152 · `new_preview_post` cannot reproduce Go's two nil panics, by construction

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-08-20 (permalink.go)

`model.NewPreviewPost(post, team, channel)` guards `post == nil` and returns nil, then
dereferences `team` and `channel` with no check at all. Measured, not read: a nil team panics, a
nil channel panics, `(nil, nil, nil)` returns nil because the post is tested first.

The Rust port takes `team: &Team` and `channel: &Channel`, so there is no nil to pass and the two
panics are unrepresentable. `post` stays `Option<&Post>` because its guard is behaviour a caller
depends on.

**Why this is accepted rather than reproduced.** Reproducing it needs `panic!` in library code,
which this project forbids, and the alternative — returning an error — would invent an API Go does
not have. On every input Go survives, the two implementations agree exactly; the divergence is
only that a caller who would have crashed the Go server now fails to compile.

**What would reopen it:** a Go call site that *relies* on the panic (recovering it as control
flow), or upstream adding real nil handling — in which case the signature should follow. The
parity test asserts the panicking rows still panic, so an upstream change fails a test rather than
passing silently.

## D-153 · The discoverable-channels surface is pinned off rather than ported

**Status** OPEN · **Severity** deferred-feature · **Raised** 2026-08-20 (api4/channel.go `getChannel`)

Go's `getChannel` tries `serveDiscoverableNonMember` before answering 403 to a non-member of a
non-open channel (api4/channel.go:886). The whole surface is gated on
`FeatureFlags.DiscoverableChannels`, which is **false** at the pinned SHA (feature_flags.go:208)
and unset in this deployment, so the gate's first line returns "not served" and the 403 follows —
which is exactly what the port answers, asserted against the running Go server in
`parity_channel_get.rs`.

**What is owed if the flag is ever turned on:** `GetUser`, `IsDiscoverableJoinAllowed`,
`sanitizeDiscoverableChannel`, and — the real blocker — a feature-flag/config surface, which this
server does not have at all ([D-085] is the same gap for privacy settings). Until then every
deployment must run Go with the flag at its default; turning it on server-side would make the two
servers answer a non-member's GET differently, and nothing would fail loudly.

**Where the pin lives:** the doc comment on `channel_read_denied` in `mm-api/src/channels.rs`.

**2026-09-06 — and this is NOT unblocked by the config work, which is the point worth recording.**
[D-156] gave this server the configuration document Go persists, but `Store.Load` **clears
`FeatureFlags` before persisting** when `readOnlyFF` is set, which is the default
(config/store.go:306-310). Measured against the live row: it has no `FeatureFlags` key at all. So
`FeatureFlags.DiscoverableChannels` is precisely the class of setting the new source of truth
cannot see. Reading it needs the environment (`MM_FEATUREFLAGS_DISCOVERABLECHANNELS`), which only
agrees with Go when the operator sets flags that way — the same weaker guarantee the whole config
change was meant to escape. The deployment constraint therefore stands unchanged: run Go with this
flag at its default.

## D-154 · `getUsers` cannot see the licence, so ABAC-narrowed `not_in_channel` would diverge

**Status** OPEN · **Severity** deferred-feature · **Raised** 2026-08-21 (api4/user.go `getUsers`)

Go's `not_in_channel` arm asks `ChannelAccessControlled` (app/channel.go:4522) and, for a
policy-enforced **private** channel, replaces the listing with
`GetUsersNotInAbacChannel` — a narrowed candidate set. The `not_in_team` arm does the same when
`abac_match_only=true`. Both gates return false without an Enterprise Advanced licence *and*
`AccessControlSettings.EnableAttributeBasedAccessControl`, which is this deployment, so the
served arms match Go today and the parity suite measures that.

The port has neither a licence surface nor an access-control service, so it cannot detect the
enforced case. `abac_match_only=true` is forwarded on both `not_in_*` arms, which closes the
half a query parameter can name. The half it cannot: on a licensed server with ABAC on, a
policy-enforced private channel's `not_in_channel` list would come back **unnarrowed** from this
port — a listing Go was configured to restrict.

**What is owed:** `License()` plus `AccessControlSettings`, and then either the ABAC query or a
forward on `ChannelAccessControlled`. Until then this route must not be deployed against a licensed
Enterprise Advanced server with attribute-based access control enabled.

**2026-09-06 — half unblocked.** The config gap this entry pointed at ([D-085]) is closed:
`AccessControlSettings` is an ordinary section of the document `mm_app::config` now reads, so
`EnableAttributeBasedAccessControl` is two fields away rather than a subsystem away. The licence is
already reachable too, via `App::license_state`. What remains is genuinely the access-control
service, not the plumbing — so the honest next step here is a **forward** on
`ChannelAccessControlled` rather than the ABAC query, which would close the divergence without
porting an Enterprise surface this build cannot exercise.

**Where the pin lives:** the doc comment on `users::get_users` in `mm-api/src/users.rs`.

## D-156 · The two config settings the permission checks read cannot be read from Go's config

**Status** CLOSED · **Severity** divergence · **Raised** 2026-08-21 (phase 2, authorization.go)
**Closed** 2026-09-06 — the project owner chose to port config properly, and the first thing that needed
was a shared source of truth. `docker-compose.yml` now sets `MM_CONFIG` to the shared Postgres DSN,
so Go selects `config.DatabaseStore` (config/store.go:91) and persists the whole `model.Config` into
`Configurations.Value`. `mm_store::SqlConfigStore` reads that row and `mm_app::config::Config::load`
layers the `MM_<SECTION>_<SETTING>` overlay on top, which is Go's own order (store.go:285 → :292).
An operator editing configuration by *any* route — file, environment or System Console — now moves
both servers together, because there is no longer a file involved. The environment-only reading
this entry described is gone, not accepted.

`authorization.go` consults `model.Config` in exactly two places, and both are now ported:

| Setting | Read by | Go default |
|---|---|---|
| `ExperimentalSettings.RestrictSystemAdmin` | `SessionHasPermissionToAndNotRestrictedAdmin` (:31) | `false` |
| `ComplianceSettings.Enable` | `HasPermissionToReadChannel` (:475) | `false` |

**The Go server keeps its configuration in a file we cannot see.** `docker-compose.yml` mounts the
`mattermost-config` volume at `/mattermost/config` and leaves `MM_CONFIG` unset, so the config
store is `config.json` inside that volume. The strangler-fig deployment shares a *database*, not a
filesystem — this is the first ported value with no shared source of truth to read, which is why
it is a divergence rather than a lookup.

**What we do instead:** `mm-app/src/config.rs` reads the same `MM_<SECTION>_<SETTING>` environment
variables the Go server overlays on top of its file, defaulting to Go's own defaults. An operator
who configures the Go server by environment — which is how `docker-compose.yml` configures it
today — gets identical values on both servers automatically. **An operator who edits `config.json`
directly does not**, and that is the whole of this entry.

**Both settings over-grant when we are wrong**, which is why this is not filed as ACCEPTED:

- Missing a `RestrictSystemAdmin=true` admits a restricted admin Go denies.
- Missing a `ComplianceSettings.Enable=true` lets a non-member read a public channel that Go, with
  compliance on, confines to members.

Note the second is **not** made unreachable by Team Edition. `authorization.go:475` reads the
setting without consulting the licence, even though every compliance *feature* is licence-gated
(`app/compliance.go:18`) — so the setting alone moves the branch on an unlicensed server.

**What is owed:** either read `config.json` from a path the operator supplies, or have `mm-api`
ask the Go server for its effective config once at startup (`GET /api/v4/config` requires
`manage_system`, so this needs a service account or an admin token). Deferred because neither is
needed for a default deployment and both add a startup dependency on the Go server that the proxy
otherwise does not have.

**Where the pin lives:** the module doc on `mm-app/src/config.rs`, which states which direction
each setting fails.

**2026-09-06 — `MM_LICENSE` joins this, and it is the one that fails safe.** `getClientLicense`
needs to know whether the installation is licensed. Two of Go's three sources are readable from
here (`Systems.ActiveLicenseId`, and the disk file, which Go's own loader saves into that row);
the third is the `MM_LICENSE` environment variable, read by the same `Config::from_env`
arrangement above. An operator who sets it on the Go container and not on `mm-api` leaves us
believing the server is unlicensed — but unlike the two settings above, being wrong here makes us
**forward** rather than answer, so the client still gets Go's map. See
`mm-app/src/license.rs`.

**Also 2026-09-06:** `main.rs` was building the `App` with `App::new`, which takes
`Config::default()` — so none of this was read outside the tests. Now `Config::from_env`.

---

## D-158 · sqlx materialises a nil Go map before scanning, and only one ported store knows it

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-23 (phase 2, getPostsForChannel)

Reading `StringInterface.Scan` (model/utils.go:185) says a NULL column leaves the field nil, and a
nil Go map marshals as `null`. That is what `mm-store/src/post_store.rs` did, and it was wrong:
the running server answers `"props":{}` for a post whose `props` column is SQL NULL.

The scan never sees a nil map. **sqlx allocates one first** — `reflectx.FieldByIndexes` calls
`reflect.MakeMap` for any nil map on the path to the field it is about to scan into — so `Scan`'s
early return on NULL lands on an empty map. Slices get no such treatment, which is why a NULL
`fileids` column really does reach the client as `null`. Measured three ways on the same row:

| `posts.props` | Go answers |
|---|---|
| SQL `NULL` | `{}` |
| jsonb `'null'` | `null` — `json.Unmarshal` sets the map back to nil |
| jsonb `'[1,2]'` | 500, `app.post.get.app_error` |

Fixed for `Post` and pinned by `parity_channel_posts::a_null_props_column_is_an_empty_object_on_every_route`,
which asserts it on `GET /posts/{id}` as well — the divergence had been shipping there since that
route landed, undetected because no fixture had a NULL column.

**What is owed:** the same question for every other ported store that scans a Go **map** field out
of a nullable column. `Channel.Props`, `Session.Props`, `User.NotifyProps` and
`User.Props` are all `StringMap`/`StringInterface` over nullable columns, and each is one
`UPDATE … SET col = NULL` and one request away from an answer. None of them can be produced
through the REST API, which is why none was noticed; that is an argument for checking them, not
for assuming they are fine.

**Where the pin lives:** the module doc on `mm-store/src/post_store.rs`, with the table above.

---

## D-159 · Go caches the post etag for thirty minutes; we read it fresh every time

**Status** OPEN · **Severity** divergence · **Raised** 2026-08-23 (phase 2, getPostsForChannel)

`LocalCachePostStore.GetEtag` (localcachelayer/post_layer.go:74) memoises a channel's last post
time for `LastPostsCacheSec` — thirty minutes — and drops the entry only when a post is written
**through the store**. `mm-api` has no cache layer and reads the newest `Posts.UpdateAt` on every
request. Two consequences, one live and one waiting:

- **An empty channel diverges today.** With no posts there is no last post time, so Go caches the
  clock reading its *first* request took and repeats it for half an hour, while we stamp a fresh
  reading each time. Go can therefore answer 304 to a client echoing that etag back where we
  answer 200 with the same empty list. Bodies are identical; only the header and the status move.
  Pinned by `parity_channel_posts::an_empty_channel_stamps_the_etag_with_the_clock`, which asserts
  Go's two reads agree and ours is bracketed by the clock.
- **Any write that does not go through Go's store makes its etag stale.** Today that is only test
  fixtures writing rows directly. The moment `mm-api` serves a write — `createPost`, a reaction,
  anything that moves `Posts.UpdateAt` — Go will keep serving the pre-write etag for up to thirty
  minutes and 304 clients that should have been given the new page. This is the first place the
  Strangler Fig's shared database is not enough: the two servers also share a *cache invalidation
  protocol*, and we do not speak it.

**What is owed:** before the first post-mutating route ships, either invalidate through Go's
cluster-invalidation channel or accept and document a staleness window. Not before — a read-only
`mm-api` cannot make Go's cache wrong.

**Where the pin lives:** the doc on `mm_app::App::get_posts_etag` and the test named above.

---

## D-160 · The consolidated parity binary shares one database with no isolation between suites

**Status** OPEN · **Severity** test-harness flake · **Raised** 2026-09-02 (phase 2, getPostThread)

Commit 6c156a2 merged 35 `parity_*.rs` integration binaries into one `--test parity`. Build times
fell sharply and that was the right trade — but the 35 processes had been an isolation boundary as
well as a compilation unit, and nothing replaced it. All 296 tests now run as threads in **one**
process against **one** shared Mattermost database.

Two distinct consequences, and only the first is fixed:

- **Duplicate fixture names across modules became reachable and are now deterministic failures.**
  `team_channel_lists` and `channels_for_user` both created `mmrs-parity-pageteam`;
  `team_channel_lists` and `channels_for_team_for_user` both created `mmrs-parity-delteam`. As
  separate binaries they never overlapped; as threads the loser gets
  `store.sql_team.save_team.existing.app_error` and two or three tests fail on every run. **Fixed
  2026-09-02** by renaming the two non-`team_channel_lists` tags; a full-suite grep confirms these
  were the only duplicate pair. Any new suite must pick a tag unique across the whole binary — the
  note now sits at both call sites.
- **Residual cross-suite interference remains, and it is intermittent.** Over three consecutive
  full runs after that fix: one run fully green (296/296), one with three `sessions_for_user`
  failures, one with a single `teams_for_user` failure — a different suite each time. These read
  *global* lists (a user's sessions, a user's teams) that other suites mutate concurrently by
  logging fixture users in and creating teams. It is the same class as the tied-sort-key flake
  behind the `mmrs-parity-` DM purge, one level up: the fixture is no longer just leaked rows, it
  is another suite running right now.

Re-measured 2026-09-03 on an unmodified `cffe86d`, two consecutive full runs: 2 failures then
3, hitting `users_me`, `channels_for_user` and `channels_for_team_for_user` — a different set
again, and none of the suites named above. `users_me` fails on a *timestamp* rather than a list
(`update_at` 33 ms newer in the row than in the body it just fetched), which widens the class:
any suite that logs a fixture user in moves that user's `UpdateAt` under any suite reading it.
The single-module invocation the workflow actually uses — `scripts/parity.sh -p mm-api --test
parity users_me` — is green on every run.

**What is owed:** give the suites back an isolation boundary. The cheapest option that preserves
the build-time win is to mark the globally-scoped suites `#[serial]` (or gate them behind one
shared mutex) rather than re-splitting the binary; the thorough option is a per-suite fixture user
so no two suites read the same user's sessions or teams.

**Why it is not fixed here:** it is not this route's bug and the diagnosis crosses six suites.
`post_thread`'s own 16 tests passed in all three runs and in every mutation run.

**Partial mitigation, 2026-09-04 (reactions/emoji session).** `common::fetch_both_stable`'s
retry budget went from 8 attempts over ~1.8s to 12 over ~6s. What it waits out is not a race
inside one request but *the other suites in this binary still building their fixtures*: every
`create_team` joins the shared fixture user to a `town-square` and an `off-topic` and every
`create_channel` joins it to one more, so a global list genuinely changes underneath a reader for
as long as any suite is still setting up. The old budget sat inside that window. This narrows the
flake; it does not give the suites an isolation boundary, so the entry stays OPEN — measured the
same day across three full runs at 318 tests: one green, one losing
`channel_members_list::pages_split_cover_and_run_out_identically`, one losing
`teams_for_user::me_and_the_explicit_id_answer_the_same_teams`. A different suite each time, each
green in isolation, and neither of the two suites added that day ever among them.

**Where the pin lives:** the comments at the two renamed `create_team` call sites, and the
`ATTEMPTS` constant in `crates/mm-api/tests/common/mod.rs`.

## [D-166] `serde_json` sorts object keys where Go emits struct order — OPEN

**Owner:** unassigned. **Blocks:** serving any route whose response embeds a post carrying an
`attachments` prop.

`Post::strip_action_integrations` rewrites `props.attachments` by re-marshalling the decoded
`SlackAttachment` slice — the same thing Go does. Go's `encoding/json` emits a struct's fields in
**declaration order** (`id, fallback, color, pretext, author_name, …`); `serde_json::Value` is
backed by a `BTreeMap` and emits them **alphabetically**. The two bodies then differ by key order
inside `props.attachments` and by nothing else.

Nothing noticed until `getThreadsForUser`, because every earlier route that meets this prop
forwards for a different reason (`mm_app::post::REFUSED_PROPS`). That route now forwards a whole
page when any root post carries the prop — see `mm_api::users::serve_threads` — which is correct
but costs a proxy hop, and the same guard will be needed by every future route that embeds a post.

**The fix is one Cargo feature**, `serde_json/preserve_order`, which swaps the map for an
`IndexMap` and keeps insertion order. It is a workspace-wide change to every `Value` this port
produces, so it needs its own session and a full-suite run: it may equally *repair* latent
mismatches elsewhere or expose tests that were passing on alphabetical order. Do not fold it into
a route session.

## [D-155] orphaned fixture rows — CLOSED 2026-09-05

`purge_api_fixtures` selected by the `mmrs-parity-%` name prefix, which never reached the rows Go
authors on a fixture's behalf — a created team's `town-square` and `off-topic`, and the
`SidebarCategories` keyed on its `TeamId`. Deleting the team orphaned all of them.

Measured before the fix: **16,066 orphaned channels against 25 live ones**, ~32,000 posts hanging
off them, and **3,190 `Threads` rows whose root post no longer existed** against 4 real ones.

The purge now sweeps by the dangling reference rather than by name — a channel whose `TeamId`
names no team, a post whose channel is gone, a thread whose post is gone — which is what the old
note asked for. Such a row is unreachable through any API on either server, so nothing that
deletes it can be observed by a test. After the sweep: 0 orphans, 189 channels, 4 threads.

### [D-167] OPEN — whole-table parity reads still race concurrent fixtures

Three of five full-workspace runs on 2026-09-05 each failed a *different* test, and every one was
a read over shared state that another suite's fixture moved mid-run: the users-list etag
(`MAX(UpdateAt)` over every user), `channel_members_list`'s `OFFSET` paging, and an intra-suite
ordering race in `roles` that reproduces only under a narrow filter.

Each has been fixed as it surfaced — bracketed reads (`fetch_both_stable`), walk-and-deduplicate,
retry-until-settled. What is owed is the *deliberate* pass: enumerate the assertions that read a
whole shared table (users, roles, emoji, channel members) and convert them, rather than waiting
for each to fail. The suite now has 22 routes' worth of fixtures creating and deleting users and
teams concurrently, so the pressure only grows.

Not a port divergence: both servers read the same database, and every failure so far has been the
test's premise, not the answer.

**Update 2026-09-08 (reaction writes).** One instance of this was found, explained and closed, and
it was not a read race at all:

`purge_api_fixtures` deletes by shared prefix — including `channelmembers` and `teammembers` for
every `mmrsplain%` user — and its own comment asks for it to run "before any fixture is built". A
`OnceCell` on the purge alone cannot deliver that: whichever test trips it first runs the sweep
while other suites already have fixtures up, and their rows go with it. Measured twice in a row —
`channel_members_list::pages_split_cover_and_run_out_identically` lost two of its four members to a
purge triggered by another suite's `create_plain_user`.

The fix is one line: the purge now runs **inside `go_minted_token`'s `OnceCell`**. No stack-backed
test can build anything before it has a token, so the first caller holds every other one until the
sweep is done. That test has passed every run since. Every existing `purge_api_fixtures()` call
site keeps working as a no-op on an already-initialised cell.

**What remains is narrower than the entry above suggests.**
`threads_for_user::per_page_limits_the_list_and_not_the_totals` still fails roughly one run in
two. It passes on its own and fails when its **own module's** sixteen tests run together, so it is
an intra-suite race like the `roles` one — not a cross-suite purge, and not this port. It predates
the write-route work: it failed on the first full-suite run of the websocket-hub session, before
any of that code existed. Owed: the deliberate pass over that module's shared fixture, which is
one `OnceCell` mutated by several tests.

---

## D-169 · A 401 from an invalid token does not clear the session cookie

**Status** CLOSED · **Severity** divergence · **Raised** 2026-09-06 (phase 2, session-activity pair)
**Closed** 2026-09-06, same day — `ServiceSettings.SiteURL` is a modelled setting,
`GetSubpathFromConfig` is ported against a Go corpus (`fixtures/behaviour_subpath.json`), and the
extractor's rejection carries the cookie. Asserted against the running Go server across all three
token locations, and asserted *absent* when no token was sent — the branch turns on a token being
present and rejected, not on the 401.

The estimate below was right about the work and wrong about one thing: the parse-failure case
returns the **empty string**, not `/`, and `net/http` then omits the `Path` attribute entirely. A
port that treated the error as "no subpath, so root" would have scoped the cookie one level too
wide on exactly the misconfiguration where it matters.

`handlers.go:278` calls `c.RemoveSessionCookie(w, r)` immediately before substituting
`api.context.session_expired.app_error`, so Go's 401 carries

```
Set-Cookie: MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly
```

and ours carries no `Set-Cookie` at all. Measured against both servers with a bad bearer token.

The **id** half of that same branch was fixed in the session-activity session — we were returning
`App::GetSession`'s inner `api.context.invalid_token.error`, which Go discards. The cookie half was
not, because it needs something the id did not: `RemoveSessionCookie` sets the cookie's `Path` to
`GetSubpathFromConfig(c.App.Config())`, so paying this off means modelling
`ServiceSettings.SiteURL` as a **setting** (it is currently read only for its presence, as the
`isUpdate` discriminator) and porting the subpath extraction. That is its own small unit of work
and it belongs to no route.

Consequence: a browser whose session was revoked — by idle timeout, by an admin, or by the token
simply being wrong — keeps a dead `MMAUTHTOKEN` cookie when the 401 came from this server, and
sheds it when the same 401 came from Go. The webapp re-authenticates on the 401 regardless, so
this is a divergence in cleanup rather than in access.

**To pay off** add `site_url` to `mm_app::config::Config`, port `GetSubpathFromConfig`, and set the
cookie in `ApiError::unauthenticated`'s response. The parity assertion is one line against
`Set-Cookie` in `parity/session_activity.rs`, which already compares the bodies of this exact
branch.

---

## D-181 · Websocket reconnect replay is not ported

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-08 (websocket hub, phase 0 of the
write routes)

Go keeps a 128-slot **dead queue** of every frame it has written to a connection
(`web_conn.go:665`). A client that drops and reconnects presents its `connection_id` and
`sequence_number`; `PopulateWebConnConfig` finds the old connection, and `writePump` either drains
the frames it missed (`drainDeadQueue`) or — when the sequence is too old to be in the queue —
mints a *new* connection id, resets the sequence to 0 and re-sends `hello`.

`mm_app::hub` implements none of it. Every connection is fresh: `connection_id` and
`sequence_number` on the query string are read by nobody, so a reconnecting client silently
**loses every event raised while it was disconnected** rather than being told to refetch.

What makes this more than an efficiency gap is the third branch. Go's `hasMsgLoss` path is how a
client *learns* it has a hole: a second `hello` with a new connection id is the signal to reload
state. This server never sends one, so a client cannot distinguish "you missed nothing" from "you
missed an hour".

**Owed:** the dead queue, `PopulateWebConnConfig`'s three-way branch, and the `reuseCount == 0`
gate on `hello` that currently has only one reachable value.

## D-182 · A client on mm-api does not see events raised by Go-served routes

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (websocket hub)

The two servers each have their own hub and no bus between them. Go mirrors events across nodes
through `clusterIFace` (`cluster.go:189`); there is no cluster here, and the *other process* is
not a node — it is the half of the API that has not been migrated yet.

So a client connected to `:8066` sees events for the routes this server serves and **nothing** for
the ~560 routes still forwarded to Go. Before this session it saw nothing at all — `forward_to_go`
strips `Connection` and `Upgrade` as hop-by-hop headers, so the websocket route could never be
proxied — which is why this is a step forward rather than a regression, but it is a real hole for
as long as the strangler runs.

Two ways to close it, and the choice is not obvious:

1. **Listen to Go.** mm-api opens its own websocket to the Go server as a system client and
   re-publishes what it receives. Cheap, and wrong in one way: Go filters per connection, so what
   arrives is already scoped to *that* connection's user, not the whole event.
2. **Port the routes.** The hole shrinks to zero on its own as the migration proceeds, which is
   the project's actual direction ("the end state is a Go server that is not running").

Recorded rather than solved because (2) is the plan and (1) would be scaffolding on scaffolding.
It stops being a hole when the last publishing route is migrated.

## D-183 · Broadcast hooks are stripped but not run

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-08 (websocket hub)

`web_broadcast_hooks.go` rewrites an event *per connection* on the way out. The stock hook adds
the recipient's own mention count and follow state to a `posted` event, so two users receive
different bytes for the same post.

`mm_model::WebSocketEvent::without_broadcast_hooks` is ported and `App::publish` calls it, so the
hook fields never reach a client — that part is correct and is wire format. The hooks themselves
are dropped on the floor. Every recipient of a `posted` event from this server therefore gets the
*unhooked* payload, which is missing the fields the webapp uses to decide whether to badge the
channel.

**Owed with the first write route that publishes `posted`** — which is the route this becomes
visible on. Until then no event this server raises has a hook attached.

## D-184 · The MFA half of a websocket connection's authentication is not checked

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (websocket hub)

`WebConn.IsAuthenticated` is `IsBasicAuthenticated() && IsMFAAuthenticated()` (`web_conn.go:824`).
Only the first is ported: `MFARequired` does not exist in `mm-app`, so a connection whose user owes
MFA is treated as fully authenticated and receives every event they would otherwise be held back
from.

Narrow in practice — the HTTP side of MFA is not ported either, so a deployment that enforces MFA
is not one this server can serve at all — but it is a *fail-open* difference and belongs in the
backlog rather than a code comment for that reason. Closing it means porting `MFARequired`, which
is HTTP work that this route will then inherit for free.

## D-185 · Guests receive `user_updated` and `new_user` for users Go hides from them

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (websocket hub)

`ShouldSendEventToGuest` (`web_conn.go:852`) special-cases exactly two event types and asks
`UserCanSeeOtherUser` whether this guest may see the user the event is about. That function is not
ported, so `mm_app::hub::guest_visibility` implements the *default* arm — every other event passes
— and the two special cases are withheld unconditionally.

That is the safe direction (a guest sees less, not more), and it is deliberately not approximated:
guessing at the visibility rule would produce a confident wrong answer where a stated gap produces
none. It becomes wrong in the other direction only if `UserCanSeeOtherUser` would have returned
true, which for a guest is the minority case.

**Owed:** `UserCanSeeOtherUser`, which several `/users` routes will need anyway.

## D-187 · Binary (msgpack) websocket frames are refused

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (websocket hub)

Go's `readPump` decodes a binary frame with msgpack (`web_conn.go:485`), using the same
`msgpack:` tags `WebSocketRequest` carries beside its `json:` ones — so a client may speak either
encoding. This port handles text frames only and closes the connection on a binary one.

No stock Mattermost client sends msgpack over the socket today (the tags exist for the *cluster*
path), so nothing reachable is affected. It is owed rather than accepted because the tags are on
the wire type and a client is entitled to use them.

## D-188 · The six `wsapi` actions are not served

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-08 (websocket hub)

`channels/wsapi` registers six actions on the websocket router, and this port serves none of them:

| Action | What it needs |
|---|---|
| `ping` | nothing — four constants and `GetMillis` |
| `user_typing` | `PublishUserTyping`, a channel permission check, and the server-busy gate |
| `user_update_active_status` | `SetStatusOnline` / `SetStatusAwayIfNeeded` — status **writes** |
| `get_statuses` | `GetAllStatuses`, which reads Go's in-memory status cache, not a table |
| `get_statuses_by_ids` | `mm_app::status::get_user_statuses_by_ids`, already ported |
| `posted_notify_ack` | notification metrics, which do not exist here |

All six currently answer `api.web_socket_router.bad_action.app_error` at 500 — Go's *unknown
action* error — which is a wrong answer rather than a missing one, and that is why this is an
entry and not a note.

`get_statuses` is the one with a real question behind it: Go returns the contents of a cache this
server does not have, so "every row in `Status`" is a different answer on a freshly started Go
process. It needs measuring before it is ported, not translating.

**Two of the six are nearly free** (`ping`, `get_statuses_by_ids`) and should go first, with the
rest following the status-write routes that give them their app layer.

## D-189 · The route inventory read a literal gorilla segment as a parameter — CLOSED 2026-09-08

**Status** CLOSED · **Severity** unverified · **Raised and closed** 2026-09-08 (reaction writes)

`scripts/routes.py` normalised every `{name:regex}` to `{name}`. The websocket route is registered
as `{websocket:websocket(?:\/)?}` — braces used only to attach an optional trailing slash — so its
inventory path became `/api/v4/{websocket}` and the axum route `/api/v4/websocket`, the literal
path every client uses, failed to match it. The tool reported an unserved route this server had
been answering since the previous commit.

`normalise` now recognises a pattern that is a literal (after stripping an optional trailing-slash
suffix) and emits the literal segment. Verified against the whole inventory: exactly one of 764
paths changed.

Kept as a CLOSED entry rather than deleted because the *number* is the project's headline metric
and this is the first time the tool that produces it has been wrong.

## D-190 · A write served by mm-api leaves Go's local caches stale, and `/caches/invalidate` does not fix the reaction cache

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (reaction writes)

Go's store is wrapped in `localcachelayer`. Each cached read is memoised, and each cached *write*
purges its own entry — through `doInvalidateCacheCluster`, which touches the local cache **and**
sends a cluster message. This server writes the same database and does neither, so a row it
changes is invisible to Go for as long as Go's entry lives.

That much was expected. What is not is the second half, which was measured rather than assumed:

- `POST /api/v4/caches/invalidate` runs `InvalidateAllCachesSkipSend`
  (platform/cluster_handlers.go:137), which clears the **session**, **status**, **team**,
  **channel**, **user**, **post**, **fileinfo**, **webhook** and **link** caches;
- `LocalCacheStore.Invalidate()` (localcachelayer/layer.go:704) is the function that also clears
  `reactionCache`, `schemeCache`, `roleCache`, `emojiCache…` and the rest — and it is reached
  **only from the cluster-message handler**.

So on a single node there is no way to make Go re-read reactions. Measured: a reaction deleted
through `:8066` had `DeleteAt` set in the `Reactions` table and was gone from `:8066`'s own read,
while `:8065` still listed it after three explicit invalidations.

**Consequences, in order of importance:**

1. **A parity test must read back a Rust write through Rust.** Reading through Go asserts against
   a cache, and the failure looks exactly like a write that did not happen — it cost an hour here
   before the cause was found. `crates/mm-api/tests/parity/reaction_writes.rs` documents the rule
   at `reactions_on`.
2. **A real client on `:8065` sees a stale reaction list** after somebody else reacts through
   `:8066`. Not fixable from this side.
3. The set of affected reads grows with every write route: emoji, roles, schemes and channel
   member counts are all in the same "cluster-only" group.

**This is not owed work on the port** — it is a property of running two servers over one database,
and it disappears when the Go server does. It is OPEN rather than ACCEPTED because the *test* rule
it implies has to be applied by every future write group, and a group that forgets it will chase a
phantom bug.

## D-191 · The five status writes need a status cache before they can be ported

**Status** CLOSED 2026-09-08 · **Severity** blocking · **Raised** 2026-09-08 (write-route loop)

**Closed by the status-writes group.** `App::status_cache` exists, `PUT /users/{id}/status` is
served, and the three cache-dependent decisions are `away_is_needed`, `online_row_needs_writing`
and `is_user_away` in `crates/mm-app/src/status.rs` — extracted precisely because no HTTP request
can reach them, and mutated against unit tests instead. The four `custom` siblings are not blocked
on the cache at all; they are blocked on `UpdateUser`, which is the next group.

`PUT /users/{id}/status` and its four `custom` siblings are the first write family where **the
cache is the model rather than an optimisation**, and porting them mechanically would be wrong in
a way the tests would not immediately show.

`PlatformService.SetStatusOnline` (app/platform/status.go) reads the previous status from
`ps.GetStatus`, which is the **in-memory status cache**, and three separate decisions branch on
what it finds:

- `status.Manual && !manual` returns early — a manually set status overrides a non-manual one;
- `broadcast` is set only when the status *changed*, so a redundant write publishes nothing;
- the row is written with `SaveOrUpdate` when something changed and with `UpdateLastActivityAt`
  otherwise, and **only** when `LastActivityAt - oldTime > StatusMinUpdateTime`. A status touched
  more often than that never reaches the database at all.

So the previous value is not the `Status` row; it is whatever the process last put in its cache.
A port that read the row would take a different branch from Go on exactly the requests that
matter, and would write rows Go throttles away.

**Decided 2026-09-08, on the maintainer's direction that Go is temporary: option 1.**

`mm-app` gets its own status cache, mirroring `platform.statusCache`. The objection to it — that
two processes each hold an authoritative previous value and disagree about whether to broadcast —
is real, and it is a property of *running two servers*, not of the design. It is [D-182] and
[D-190] again, and like them it ends when the Go server does. Waiting instead would leave five
routes unported for a reason that expires, which is the shape of decision the "end state is a Go
server that is not running" rule exists to refuse.

The rejected alternative, for the record:

2. ~~Serve the status writes only once Go is gone, and forward them until then.~~ Rejected: it
   makes the strangler a reason to skip work, which is exactly what MIGRATION.md's 2026-09-07 note
   forbids.

**What the cache has to reproduce**, and each is a branch a table-reading port would get wrong:
the manual-override early return, the "did it change" test that decides whether to broadcast at
all, and the `StatusMinUpdateTime` throttle that decides whether the row is written. It is a
`RwLock<HashMap<String, Status>>` on `App` beside the hub, populated on read-through and written
on every set — the same shape `mm_app::hub` already has.

Related: `getUserStatus` is already served here and reads the **table**, which is why
`common::set_user_status` in the parity harness exists and says so. The read is safe because Go
writes the row on the paths a test uses; the write is not.

The four `custom` routes are lighter — they write `Users.Props["customStatus"]` rather than the
`Status` row — but they publish through the same `BroadcastStatus`, so they inherit the same
question about who decides that a status changed.

## D-192 · A `Deserialize` derive without `#[serde(default)]` rejects bodies Go accepts

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-08 (OAuth app writes)

Go's `json.Decode` into a struct leaves an **absent field at its zero value**. A serde derive
without `#[serde(default)]` makes an absent field a *decode error*. So any type a handler decodes
from a request body is stricter here than in Go unless it carries the attribute, and the
divergence is a 400 where Go answers 200.

Measured: `POST /api/v4/oauth/apps` with `icon_url` and `is_trusted` omitted — a perfectly ordinary
body — is a **201** on Go and was a **400** here, because `OAuthAppRequest` lacked the attribute.

**The three types this server decodes that were missing it are fixed** (`OAuthAppRequest`,
`OAuthApp`, `ClientRegistrationRequest`). The other five it decodes — `Reaction`, `Draft`,
`Preference`, `IncomingWebhook`, `OutgoingWebhook` — already had it.

**What is owed is the sweep.** 126 of `mm-model`'s deserializable structs have no
`#[serde(default)]`. Most are response types that no handler decodes, so most are harmless *today*
— but every one is a landmine for the write route that first decodes it, and the failure mode is a
plausible-looking 400 rather than a compile error. Two options, and the first is probably right:

1. Add `#[serde(default)]` to every `Deserialize` type in `mm-model` and let the parity fixtures
   prove nothing changed. It cannot break a *serialisation* test, because the attribute only
   affects decoding, and every fixture round-trip decodes a **fully populated** document where
   defaults never apply.
2. Add it per type as each becomes reachable, which is what has happened so far and is how this
   one was found — by a test failing after the code was written.

A `#[derive(Deserialize)]` on a wire type without `#[serde(default)]` should be treated as a
review error in this project, the same way a missing `rename` is.

## D-241 · `listCommands`'s built-in half needs the slash-command registry

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-08 (phase 2, command reads)

`GET /api/v4/commands` is served for `custom_only=true` and **forwarded otherwise**. The other
branch calls `ListAutocompleteCommands` / `ListAllCommandsByUser`, which merge the ~30 **built-in**
slash commands — registered by `app/slashcommands/`, with i18n-translated display names and
descriptions — with plugin-registered ones. Neither is in the database, so no store read can
produce them.

The same registry blocks two more routes outright: `GET /api/v4/teams/{team_id}/commands/autocomplete`
(`listAutocompleteCommands`) and `.../autocomplete_suggestions`
(`listCommandAutocompleteSuggestions`), which additionally runs each provider's autocomplete
matcher.

**What is owed:** port `app/slashcommands`' provider registry, or accept that these three stay
Go's. The i18n half is the harder one — the display strings come from `i18n/en.json` through
`c.AppContext.T`, which is [D-092]'s gap.

---

## D-170 · `/files/{file_id}/public` answers HTML, not JSON

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-08 (phase 2, gated reads)

Its sibling `/api/v4/files/{file_id}/link` is served — same gate, same error id, a JSON
`AppError`. This one is **not**, and the reason is the path rather than the handler: it sits
outside `/api/`, so `web.Handler` renders `utils.RenderWebAppError` instead — a `text/html`
redirect page whose target carries the message **and an ECDSA signature** made with the server's
`AsymmetricSigningKey` (a `Systems` row).

Measured: `GET /files/zzzz…/public` on the pinned server is a 403 with
`Content-Type: text/html` and a 695-byte body containing
`window.location = '/error?message=Public+links+have+been+disabled.&s=MEQCIF…'`.

So even the *refusal* is out of reach: matching it needs the signing key, Go's exact `s=`
construction, and the error-page template. Both `GET` and `HEAD` stay forwarded.

**What is owed:** port `AsymmetricSigningKey` loading and `RenderWebAppError`. It is a prerequisite
for anything else outside `/api/` — there are three such routes in the inventory.

---

## D-171 · Five read routes are blocked on in-process state this server does not share

**Status** OPEN · **Severity** unverifiable · **Raised** 2026-09-08 (phase 2, system reads)

Each of these reads something that lives **inside the Go process** and is not in the database, so
serving them would mean answering about *our* state while a client is asking about Go's:

| route | what it reads |
|---|---|
| `GET /api/v4/server_busy` | `platform.Busy`, an in-memory flag set by `POST /server_busy` — which we forward, so Go holds it and we would always answer "not busy" |
| `GET /api/v4/logs` | the server's own log buffer |
| `GET /api/v4/logs/download` | the same, as a file |
| `GET /api/v4/latest_version` | a cached fetch of `api.github.com/repos/mattermost/mattermost-server/releases/latest`; two servers with independent caches disagree |
| `GET /api/v4/agents`, `/agents/status`, `/llmservices` | the **plugin environment** — `IsActive("mattermost-ai")` and the bridge client. Three routes, one cause |

This is not the [D-087] staleness case: those answers converge, these are simply about a different
process. `server_busy` is the one that could be closed cheaply *if* the write half moved here too,
since then this server would own the flag.

**What is owed:** nothing yet — recorded so the next session does not re-derive it. The agents trio
closes with the plugin host; the busy flag closes when `POST`/`DELETE /server_busy` migrate.

---

## D-201 · The two servers agree on a file directory only by convention

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-09 (phase 2, file backend)

`FileSettings.Directory` defaults to `./data/` — **relative to the process working directory**.
The Go server is started from `reference/.build/mmroot` by `scripts/go-server.sh`, which overrides
it with `MM_FILESETTINGS_DIRECTORY`; that is an *environment* override, so it never reaches the
configuration document `mm-api` reads. Two processes, one config row, two different directories,
and no error to say so — every file route answers 404 while looking perfectly healthy.

`scripts/parity.sh` now passes the same variable to the server it launches, which closes it for
the development stack. It is recorded because nothing *enforces* it: a `cargo run -p mm-api` from
the repository root still points at the wrong place, and the symptom is indistinguishable from a
missing file.

**What is owed:** a startup check that the configured directory exists and is writable, logged
loudly. Not a hard failure — an S3 deployment has no directory at all.

---

## D-202 · The environment overlay normalises `WebserverMode` where Go does not

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-09-09 (phase 2, file backend)

`ServiceSettings.SetDefaults` rewrites `regular` to `gzip` (config.go:845) as a **mutation of the
config struct**, and `applyEnvironmentMap` runs *after* it — so on Go an environment override of
`MM_SERVICESETTINGS_WEBSERVERMODE=regular` survives as the literal `regular`, and every
`*WebserverMode == "gzip"` test then fails.

This port normalises both the document and the overlay. That is deliberately *more* than Go does,
and it is the safer direction: `regular` and `gzip` are the same mode everywhere the value is
read, so folding them can only make the two servers agree. Reproducing the asymmetry would mean
modelling the order of two functions to preserve a value that means the same thing.

Accepted rather than fixed. It is reachable only by setting that one variable to that one value.

---

## D-203 · `image.DecodeConfig` is reproduced as far as the magic prefix and no further

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-09-09 (phase 2, emoji image)

`GetEmojiImage` calls `image.DecodeConfig` for the **format name** alone, which becomes
`Content-Type: image/<name>`. `mm_app::imaging::detect_format` reproduces the registry's magic
prefixes exactly — png, jpeg, gif, bmp, tiff, webp, wildcards included — and stops there.

So the two servers differ on exactly one input: a stored emoji whose first bytes are a valid
signature and whose body does not parse. We answer `image/png` and 200; Go answers
`api.emoji.get_image.decode.app_error` and 500.

Accepted. `CreateEmoji` decodes every image it accepts (app/emoji.go:110), so a row that reaches
this route has already been parsed once by Go; closing it means carrying six header parsers to
compute a string that is then thrown away. The corpus in `fixtures/behaviour_filestore.json`
(`image_decode_config`) records which of Go's two failure modes each case takes, so the gap is
pinned rather than assumed.

---

## D-204 · The generated initials avatar is not reproducible, so three routes forward

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-09 (phase 2, profile image)

`users.GetDefaultProfileImage` rasterises a user's initials with `golang/freetype` over
`fonts/nunito-bold.ttf`. Every pixel of the result depends on that rasteriser's hinting and
anti-aliasing, so a near-match is worse than no match at all: the image is cached by the client
for a day under an etag we would have minted.

Two consequences:

- `GET /api/v4/users/{user_id}/image/default` is **entirely** that path. It is now *registered*
  here and answers its two refusals — the `RequireUserId` 400 and `GetUser`'s 404 — and forwards
  the image itself. Its bot branch (`botDefaultImage`) is a `//go:embed` of a fixed PNG and would
  be reproducible, by copying a binary out of the read-only reference tree; it is forwarded with
  the rest. See [D-411].
- `DELETE /api/v4/users/{user_id}/image` is the same rasteriser and forwards for the same reason,
  which is not obvious from its name: it **generates and stores** the avatar rather than removing
  anything.
- `GET /api/v4/users/{user_id}/image` is migrated but forwards when the stored `profile.png`
  does not read — which is also the branch that *writes* the generated image back when
  `LastPictureUpdate == 0`, so forwarding is doubly right.

In practice the served branch is the common one: every account gets a `profile.png` at creation.

**What is owed:** nothing until someone wants those bytes. If it is ever attempted, it needs the
same font file and a rasteriser that agrees with freetype pixel for pixel — measure before
committing to it.

---

## D-205 · A multi-range request is forwarded

**Status** ACCEPTED · **Severity** incomplete · **Raised** 2026-09-09 (phase 2, file bytes)

`http.ServeContent` answers two or more ranges with `multipart/byteranges`, whose boundary is
thirty bytes from `crypto/rand`. The response is therefore not byte-comparable against Go's even
when the parts are identical, so there is nothing a parity test could assert beyond the status.

`mm_api::serve_content` forwards instead. No Mattermost client asks for more than one range, and
forwarding costs one request and keeps the answer Go's own.

---

## D-206 · `getFile`'s content-reviewer branch is forwarded

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-09 (phase 2, file bytes)

`GET /api/v4/files/{file_id}?as_content_reviewer=true` runs four checks this port does not have —
`requireContentFlaggingEnabled`, `checkChannelFlaggable`, `requireTeamContentReviewer` and
`requireFlaggedPost` — and only then serves a *deleted* file's bytes. The licence gate in front of
them is Enterprise Advanced, so on this deployment every one of them refuses; which refusal is
Go's own business, and the request is forwarded whole.

The same treatment `getTeam` and `getChannelStats` already give the parameter.

---

## D-207 · Go's global security headers were missing from every migrated route — CLOSED 2026-09-09

**Status** CLOSED · **Severity** divergence · **Raised and closed** 2026-09-09 (phase 2, file backend)

`web.Handler.ServeHTTP` sets `Permissions-Policy`, `X-Content-Type-Options`, `Referrer-Policy` and
— for `GET` only — `Expires: 0` on **every** API response, before the handler runs; the gzip
wrapper adds `Vary: Accept-Encoding`. This port set none of them, on all 264 route+method pairs
migrated before this session.

It survived that long because **no parity test compared a response header**: `common::fetch_both`
asserts bodies, and every suite used it. The file-bytes suite compares every header in both
directions and found it on its first run.

Closed by `mm_api::go_global_headers`, an outermost layer guarded on `x-mmrs-served-by` so a
forwarded response — which already carries Go's own, including the two per-request headers this
cannot mint — is untouched. `X-Request-Id` and `X-Version-Id` are still ours to leave alone;
`Strict-Transport-Security` is gated on `TLSStrictTransport`, which defaults to `false` and is not
modelled.

**The lesson generalises:** a body-only comparison cannot see a header, and three of
`ServeContent`'s five answers are defined by the headers they *delete*. New suites should compare
headers.

---

## D-208 · A client that asks for gzip gets a compressed body from Go and an uncompressed one from us

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-09 (phase 2, file backend)

`WebserverMode` defaults to `gzip`, and Go wraps every API handler in `gzhttp.GzipHandler`. This
port reproduces the `Vary: Accept-Encoding` that wrapper adds ([D-207]) but **not the compression
itself**, so a request carrying `Accept-Encoding: gzip` gets `Content-Encoding: gzip` and a
compressed body from Go, and neither from us.

Invisible to the parity suite because `reqwest` is built here without its gzip feature and
therefore sends no `Accept-Encoding` — which is also why this is recorded rather than measured: a
browser would see it.

**What is owed:** a `tower-http` `CompressionLayer` on the locally-served routes, matched to
gzhttp's content-type and minimum-size rules. The rules are the work, not the compression.

---

## D-209 · `POST /api/v4/file/test` needs the whole `FileSettings` model

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-09 (phase 2, file backend)

`testFileStore` decodes a complete `model.Config` from the request body, rejects it unless
**every pointer field of `FileSettings`** is non-nil (`checkHasNilFields`, api4/system.go:1121),
desanitizes it against the running configuration to restore `FakeSetting` placeholders, and then
builds a backend from the *supplied* settings rather than the server's.

That is roughly forty-five modelled fields plus `config.Desanitize`, for one route whose local
branch has no mandatory fields at all. Deferred rather than approximated: a partial `FileSettings`
would make the nil check answer differently from Go's, which is the one thing this route is
mostly made of.

Both spellings — `/file/test` and the backwards-compatible `/file/s3_test` — stay forwarded.


## D-213 · `GET /api/v4/users/invalid_emails` has no cross-server comparison of its 200

**Status** OPEN · **Severity** untested · **Raised** 2026-09-10 (phase 2, user lookups)

The handler refuses with a **400** whenever `TeamSettings.EnableOpenServer` is on, and
`scripts/go-server.sh` pins that variable on through the **environment**. An environment override
never reaches the configuration document, so there is no value either server could be told to read
that would open the gate on :8065 — only a restart with different environment would, and that
moves the ground under every other suite in the parity binary.

So the route ships with:

* the 400, and its precedence over the permission check, compared against Go
  (`user_lookups::the_open_server_refusal_matches_and_precedes_the_permission`);
* the 200 served by a `SecondServer` with the variable off — **our answer only**, no Go
  counterpart (`with_the_open_server_off_the_route_serves_a_page`);
* the query underneath it against planted rows (`mm-store`'s `db_users_invalid_emails`).

What is owed is a Go server on a second configuration to compare the successful response against —
a second `reference/.build` instance on another port, which the harness has no notion of today.
`SecondServer` starts a second **mm-api**, not a second Go.

Until then: the shape of the response, the five store predicates and the paging are all tested, and
the *bytes* of a successful body are not. If a field of `model.User` were serialised differently on
this route than on the ones that are compared, nothing here would catch it.

---

## D-214 · `ExtendSessionExpiryIfNeeded` is not ported on any route

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-10 (phase 2, channel view)

Go calls `c.ExtendSessionExpiryIfNeeded(w, r)` at the end of `viewChannel` (api4/channel.go:2052)
— the request every client makes on every channel switch, and therefore the one that keeps a long
session alive. It rewrites `Sessions.ExpiresAt` to `now + sessionLength` and re-attaches the
session cookies with the new max-age (web/context.go:174, app/session.go:421).

Nothing in this port does either. The route was migrated anyway because the whole thing is behind
`ServiceSettings.ExtendSessionLengthWithActivity`, which Go defaults to `!isUpdate` — **false for
every persisted configuration document**, since `Store.Load` plants a `SiteURL` before calling
`SetDefaults` ([D-088] measured this). On this stack it is off, so both servers do nothing.

What is owed, when the setting is on:

* the 1%-of-session-length-or-one-day threshold, floored at five minutes, so a session's expiry is
  not rewritten on every request;
* `platform.ExtendSessionExpiry`, which updates the row **and** the session cache;
* `AttachSessionCookies`, which is a `Set-Cookie` on the response — the only piece of this that is
  wire-visible, and the reason it cannot be quietly skipped for ever.

Until then a client talking to the Rust server on a stack with the setting enabled would have its
session expire on schedule while the same client talking to Go would not.

---

## D-215 · No push-notification hub, so `clearPushNotification` does nothing

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-10 (phase 2, channel view)

`MarkChannelsAsViewed` ends by queueing a `notificationTypeClear` on
`Srv().PushNotificationsHub` for every channel in `channelsToClearPushNotifications`
(app/channel.go:3722, notification_push.go:406) — the badge-clearing message a mobile device gets
when the user reads a channel somewhere else.

This port has no hub and no device registry, so the list is computed and dropped.
`mm_store::channel_store::classify_unreads_and_mentions` produces it in full deliberately: its
notify-prop fall-through is three branches deep and getting it wrong would be invisible until
there *is* a hub, at which point the bug would look like a hub bug. `db_channel_view_reads` and
the `channel_store::tests` module assert it today; nothing on the wire does.

What is owed is the hub itself — `app/notification_push.go`, the `Sessions.DeviceId` fan-out and
the Mattermost Push Proxy protocol — which is a session of its own and blocks nothing. Every route
that clears or sends a push notification inherits this entry.

---

## D-216 · A view's status-cache write cannot be read back through any route we serve

**Status** OPEN · **Severity** untested · **Raised** 2026-09-10 (phase 2, channel view)

`App::set_active_channel` runs on every `POST /channels/members/{user_id}/view`. It puts the
user's status — with the new `ActiveChannel` and a fresh `LastActivityAt` — into
`mm-app`'s status cache, and broadcasts `status_change` when the status *string* changed. It
writes no row, which is Go's behaviour and is asserted
(`channel_view::a_view_writes_no_status_row_on_either_server`).

The cache entry itself is unreadable: `App::get_user_statuses_by_ids` — which both status routes
go through — reads the `Status` **table** only, as its own doc comment records, and Go reads its
cache first. So a mutation that deletes the `set_active_channel` call from `App::view_channel`
survives the whole suite, and is listed as such in `scripts/mutations/channel-view.plan`.

Two things are owed and they are the same work: make `get_user_statuses_by_ids` consult the cache
before the table, which is what Go does and would close both this and the `active_channel`/
`last_activity_at` staleness the status module already documents. It was not done here because it
changes an already-migrated route's answers and belongs in a session that can re-verify the status
parity suite against Go rather than one that would be changing it in passing.

---

## D-224 · Muting a sidebar category does not mute its channels

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-10 (phase 2, sidebar category writes)
**Blocked on** a `ChannelMembers` write in `mm-store/src/channel_store.rs`.

`UpdateSidebarCategories` ends in `muteChannelsForUpdatedCategories` (app/channel_category.go:164),
which reconciles the category's `muted` flag with its channels' `ChannelMembers.NotifyProps
["mark_unread"]` through `setChannelsMuted` (app/channel.go:4032) → `Channel().UpdateMultipleMembers`.
That store write is not ported, so on this server:

- muting a category sets `SidebarCategories.Muted` and leaves every channel in it unmuted;
- **no `channel_member_updated` event is published**, one of which Go sends per affected channel, so
  a connected client's membership state goes stale with no TTL to recover it;
- the same applies to a channel *moved* between categories of differing `muted`.

The **decision** is ported and has thirteen unit tests
(`mm-app/src/sidebar.rs::mute_reconciliation`): the index pairing between the updated and original
lists, both mute directions, the move-between-categories diff, the "moved outside these categories"
case Go declines to handle, and the fact that the two sources of mutes are not de-duplicated against
each other. `App::mute_channels_for_updated_categories` computes it on every update and logs a
`warn!` naming the channels a Go server would have touched, so the gap is visible in a running
server rather than only here.

What is owed is `setChannelsMuted` on top of a ported `UpdateMultipleMembers`, plus a parity test
that mutes a category and reads the channel member back. Not done in this session because
`channel_store.rs` belonged to two other agents; the reconciliation is deliberately a free function
so the write can be dropped in behind it without touching the tested part.

## D-225 · `system_usage`'s post counter asserts a whole-database number three times in one run

**Status** OPEN · **Severity** unverified · **Raised** 2026-09-10 (phase 2, sidebar category writes —
found by the full suite, not by the route)

`parity::system_usage::a_custom_typed_post_is_not_counted` reads
`GET /api/v4/usage/posts` three times and asserts the number does not move. That number is
`RoundOffToZeroesResolution(count, 3)` (app/usage.go:21) over **every** row of `Posts` with
`Type = ''`, so it is a global counter, and the three reads are ~40 seconds apart while 996 other
tests run.

Measured on a stack whose count had reached **466**: the reads answered `300`, then `400`, and the
third assertion failed. The suites that create posts add roughly a hundred user posts per full run
and do not all purge them, so the count walks upward and eventually sits near a bucket boundary —
at which point the test fails in the concurrent run and passes in isolation, indefinitely.

`the_usage_counters_need_no_permission`, in the same module, compares an admin's numbers against a
plain user's and fails the same way — two consecutive full runs failed a *different* one of the two,
which is what rules out a fixed bug in either. Both pass with `--test parity system_usage`, 16
passed in 0.88s.

**Not caused by the sidebar routes**: zero of those 466 posts belong to any `mmrssbwrite%` fixture,
and none of the eight category routes writes a post. Recorded rather than fixed because it is not
this session's route, and left as a `divergence`-free `unverified` because nothing about Go's answer
is in doubt — only the two tests' assumption that a global counter holds still for 44 seconds.

Note also that `cargo test --workspace` **fail-fasts**: this one failure stopped 45 of the 48 test
binaries from running at all, and the run still exited 0 through a pipe. Use `--no-fail-fast` when
reading a full-suite number.

What is owed is to make the assertion local: count the planted posts' contribution against a
*delta* the test controls, or seed the count to a bucket midpoint before reading. Note that
lowering the resolution is not available — the rounding is Go's, and asserting the raw count would
stop testing the route.
## D-235 · What the system posts still do not do

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, system posts)
**Supersedes** D-231, D-232 and D-233, which are paid off.

The twelve routes that owed a system post now write one, byte-compatible in `type`, `props`,
`message` and the `posted` event — `crates/mm-api/tests/parity/system_posts.rs` reads each one
back off the timeline. Four things `App.CreatePost` does that [`App::create_system_post`] does not:

- **The notification pass.** `SendNotifications` adds an implicit mention for
  `props["addedUserId"]` on a `system_add_to_channel` post (notification.go:1115) and then
  `IncrementMentionCount`s it. So a **re-add** of an existing member still answers Go's
  `mention_count: 1` against our `0`, which is the one place this is visible in a response body;
  `parity/channel_member_writes.rs` masks the two mention counters and asserts Go's value, so the
  exclusion cannot widen. `getExplicitMentions` over the message text is absent with it — with
  default keywords it finds nothing in these twelve sentences, but a user whose custom mention key
  matches one would be mentioned by Go and not by us.
- **`channel_mentions`.** `FillInPostProps` resolves a `~channel` mention into a prop. The header,
  purpose and display-name notices quote text a user wrote, so a header naming a channel gets a
  post with the prop missing and the client renders the raw `~name`.
- **A group channel's `channel_display_name`** in the `posted` event is Go's sorted member list
  (`PostNotification.GetChannelName`) and the stored display name here. Reachable only through a
  header or purpose patch on a GM, which is the one lifecycle route a GM allows.
- **`GetSystemBot`.** Go posts as the system bot when a removal has no remover and when an archive
  has no acting user, creating the bot account on first use. No api4 route reaches either — every
  caller carries a session — so both are logged and skipped.

One more, which is not a gap in the posts but in their translation: `DeleteChannel` and
`RestoreChannel` build their message with `i18n.GetUserTranslations(user.Locale)`, the **acting
user's** locale, where the other ten use the server's. Ours are English throughout ([D-092]).

---


**`POST /api/v4/channels` posts no join message.** Found by the agent that ported channel
creation, in the same session that paid off D-231's membership posts — the create path is a
route that did not exist when those were written, so it was never in their scope. Go's
`CreateChannelWithUser` calls `postJoinChannelMessage` for the creator. It is invisible in the
201, because Go marshals the channel before the post exists, and visible from the next read of
that channel onward in `total_msg_count` and `last_post_at`. No test re-reads a created
channel's row, so this suite cannot currently detect the gap widening.
## D-234 · Two channel-patch branches are forwarded because they write what this file does not own

**Status** OPEN · **Severity** forwarded route · **Raised** 2026-09-10 (phase 2, channel
lifecycle)

`PUT /api/v4/channels/{channel_id}/patch` is served here except for two bodies, both decided
before anything is written (`mm_api::channel_writes::patch_needs_go`):

- **`group_constrained` going from off to on.** Go then runs
  `DeleteGroupConstrainedChannelMemberships` in a goroutine, which removes every member who is not
  in one of the channel's groups. That is a `ChannelMembers` write. Setting the flag to `false`, or
  to `true` on a channel that already has it, writes no memberships and is served here — Go's
  condition is `*patch.GroupConstrained && (old == nil || !*old)` and both halves matter.
- **A non-empty `default_category_name` after the patch.** `addChannelToDefaultCategory` creates a
  custom sidebar category, or moves the channel into an existing one, and updates the category
  order. That is three `SidebarCategories`/`SidebarChannels` writes; the store is read-only today.
  Gated on `TeamSettings.EnableChannelCategorySorting`, whose Go default is `true`.

Both are pinned by unit tests and by a parity test asserting the answer comes back **without**
`x-mmrs-served-by: rust` and still took effect. What is owed is the two write groups, at which
point both conditions come out of `patch_needs_go` — the member half belongs with the rest of the
`ChannelMembers` writes, and the sidebar half with the sidebar category writes.
---

## D-221 · `createPost` needs the notification pipeline, not the post write

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-10 (phase 2, post writes)

`POST /api/v4/posts` is the one route of the post-write group this session did not take, and the
reason is not the row. `SqlPostStore.SaveMultiple` is a day's work — an insert, a `Channels`
counter update, a `Threads` upsert, `PostsPriority` and `PersistentNotifications` — and its
response body is `PreparePostForClient`, which is already ported for the read routes.

What is missing is everything `App.CreatePost` (app/post.go:173) does **after** the insert, and
each item is observable to a client:

* **`SendNotifications`** — mention parsing over the channel's members and their notify-prop
  keywords, which produces the `mentions` and `followers` fields on the `posted` event, the
  per-member `MentionCount`/`UrgentMentionCount` increments, the push and email fan-out, and the
  auto-responder. Nothing of this exists in the Rust tree.
* **`attachFilesToPost`** — binds `FileInfo` rows to the new post and *overwrites* the post when
  not all of them could be attached.
* **`followThreadIfNeeded` / `ThreadAutoFollow`** — a reply makes its author a thread follower,
  which is what `GET /users/{id}/teams/{id}/threads` reads.
* **post priority, persistent notifications and the acknowledgement rows**, all written inside the
  same store call.
* **the preview/permalink path** (`addPostPreviewProp`, `SanitizePostMetadataForUser`) and the
  plugin `MessageWillBePosted` / `MessageHasBeenPosted` hooks.

A `createPost` that returned the right JSON and dropped the notification pass would be wrong in
the way this project exists to avoid: no test of the response body would notice, and a connected
client would silently stop being told it had been mentioned. So the route stays forwarded, and the
work it is waiting on is the **notification engine** rather than anything about posts.

The same engine is what forwards **deleting a reply**: `App.DeletePost` on a reply runs
`RemoveNotifications` (notification.go:914), which re-derives the reply's mentions to decrement
`ThreadMemberships.UnreadMentions`. Deleting a *root* post does not — the whole function is behind
`post.RootId != ""` — so `DELETE /posts/{id}` is served for a root and forwarded for a reply. See
`mm_app::App::delete_post`.

---

## D-222 · The `PostEditTimeLimit` branch is ported and untested

**Status** OPEN · **Severity** unverified · **Raised** 2026-09-10 (phase 2, post writes)

`ServiceSettings.PostEditTimeLimit` is `-1` on a default-configured server, and
`postEditTimeLimitExpired` (api4/post.go:1052) returns `false` on that value before it looks at
anything else. So the **400** `api.post.update_post.permissions_time_limit.app_error` that three
of the four write routes raise has no cross-server oracle on this stack, and no mutation of that
branch can be caught — every one of them is unreachable rather than uncovered.

Two things are covered without it: the value's *sign* convention, which is not obvious (`-1` is
"no limit" and `0` means every post is already past its window — the opposite), and the unit
(seconds, multiplied by 1000 against `CreateAt`). Both are unit-tested in
`mm_app::post_write::tests`.

What is owed is a parity run with the setting changed on **both** servers, which needs
`scripts/go-server.sh` to set `MM_SERVICESETTINGS_POSTEDITTIMELIMIT` and a Go restart — the same
shape as the feature-flag run that [D-213] describes. Until then the branch is transcribed from
the Go source and not measured, and the one thing a reader should know is that the pin routes check
it **after** their no-op short circuit, so pinning an already-pinned ancient post is a 200 on both
servers and only a *change* can hit the 400.

---

## D-238 · There is no e-mail service, so four routes stay with Go and two writes are silent

**Status** OPEN · **Severity** incomplete · **Raised** 2026-09-11 (phase 2, auth writes)

`Srv().EmailService` has no counterpart in this tree. Two consequences, of different kinds.

**Four routes are forwarded** because sending is all they do: `POST /users/password/reset/send`,
`POST /users/email/verify/send`, `POST /users/{id}/email/verify/member`'s sibling and
`POST /email/test`. They are deliberately *not* registered in `mm-api`'s router — registering a
path with only some methods makes axum answer 405 to the rest, so an unregistered path is what
keeps them working. Their token-minting half is ported anyway (`mm_store::TokenStore::save`,
`mm_model::Token::new`), so whoever lands an e-mail service has the store underneath already.

**Two writes lose a side effect.** `App.UpdatePasswordSendEmail` sends a password-change notice
and `App.VerifyEmailFromToken` sends an address-change notice, both in `Srv().Go(...)` goroutines
whose failure Go only logs. The write commits either way and no response byte differs, so the
parity suite cannot see it — `mm_app::auth` logs a warning at each site instead. A user whose
password is changed through mm-api is not told about it, which is a security notification rather
than a courtesy.

The dependency is measurable only by reimplementing it (SMTP, templates, i18n), which the standing
decision at the head of this file says to forward rather than port.

---

## D-236 · CSRF is not checked on any migrated route

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, auth writes)

`web.Handler.ServeHTTP` calls `checkCSRFToken` (handlers.go:295) for every request whose token came
from the **cookie**: a non-GET request must then carry `X-CSRF-Token` matching the session's, or
`X-Requested-With: XMLHttpRequest`, or it is answered 401 with the session cookie cleared. Nothing
in `mm-api` implements it. `crate::auth::AuthenticatedSession` reads the cookie and asks no further
questions, and neither does `auth_writes::OptionalSession`.

This predates the auth vertical — every migrated write has had the gap since the first one — but
it was never written down, and the auth routes are where it stops being abstract: a cross-origin
form post can now change a password or log a user out through this server where it could not
through Go.

What is owed is the check itself in the two extractors, keyed on the token's `TokenLocation`
(already modelled) and the session's `props.csrf` (already stored and already read by
`Session::get_csrf`). The pieces are all present; the wiring is not. A parity test needs a
cookie-authenticated request, which the suite does not currently make — `go_minted_token` returns
a bearer token — so the fixture is the other half of the work.

---

## D-237 · A session revoked by mm-api is still accepted by Go until its cache is invalidated

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, auth writes)

[D-190] with a credential consequence, and measured rather than reasoned about:
`POST /api/v4/users/logout` served by mm-api deletes the `Sessions` row, and the *Go* server keeps
answering 200 to `GET /users/me` with that token until `POST /caches/invalidate` is called.
`PlatformService` memoises sessions by token and our `DELETE` does not reach that map. The window
is the cache entry's lifetime, not a request or two.

Pinned by `parity::auth_writes::a_session_revoked_here_is_gone_here_but_lingers_in_gos_cache`,
which asserts **both** halves: 401 from mm-api immediately, 200 from Go, then 401 from Go after an
invalidation. If that middle assertion ever fails, the cache is being invalidated somehow and this
entry can be closed.

The same shape applies to the password writes — a password changed through mm-api does not stop the
old one working against Go until the user cache is cleared — and three tests in that suite
invalidate explicitly for exactly that reason. Unlike the reaction cache in [D-190],
`/caches/invalidate` *does* clear both of these, so the cluster-message half of the fix would be
enough. It ends when the Go server does.
## D-239 · `POST /channels/direct` forwards a team-restricted installation

**Status** OPEN · **Severity** coverage gap · **Raised** 2026-09-11 (phase 2, channel creation)

`GetOrCreateDirectChannel` (app/channel.go:361) has a branch for
`TeamSettings.RestrictDirectMessage == "team"`: unless the caller holds `manage_system`, the two
users must share a team, *except* when one of them is a bot that a plugin has exempted
(`IsBotExemptFromDMRestrictions`). `mm_app::App::get_or_create_direct_channel` returns
`ChannelCreate::Forward` for the whole setting rather than reproducing half of it.

Two things are missing and only one of them is ours to fix:

- `Team().GetCommonTeamIDsForTwoUsers` — a store method this port does not have. It is **not**
  `get_common_team_ids_for_multiple_users`, which is already ported: the two-user variant filters
  deleted teams out and the multi-user one does not, so reusing it would allow DMs Go refuses.
- the bot exemption, which needs the plugin environment — the same wall
  [`RestrictedDm::Undecidable`] already documents for `CheckIfChannelIsRestrictedDM`.

The setting defaults to `"any"` (config.go:2620), so the forward is unreachable on this stack and
on a stock server. Paying it off means the store method plus a parity run with the setting changed
on **both** servers, which is the shape [D-213] describes.

---

## D-240 · A view-restricted caller is forwarded on both message-channel creates

**Status** OPEN · **Severity** coverage gap · **Raised** 2026-09-11 (phase 2, channel creation)

`createDirectChannel` and `createGroupChannel` both call `UserCanSeeOtherUser` (app/user.go:2710),
which consults `GetViewUsersRestrictions` and — when the caller *is* restricted — asks
`Team().UserBelongsToTeams` and `Channel().UserBelongsToChannels`. Neither store method is ported,
so `mm_app::App::user_can_see_other_user` answers `PrepareError::Unreproducible` and both handlers
forward.

This is the same forward `GET /users/by_auth_data` already takes, and the same reason: the
restricted branch is reachable only for a guest account or a deployment that has edited
`system_user`'s permissions. What is new is that it now gates a **write**, so the forward has to be
returned before anything is created — it is, in `serve_create_direct_channel` and
`serve_create_group_channel`, both of which decide it before the app layer is called at all.

Paying it off is two store methods and a fixture with a guest account; until then no test on this
stack can distinguish the refusal from the forward, because nobody here is restricted.
## D-242 · A team join does not bump `Users.UpdateAt`

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, team-member writes)

`App.JoinUserToTeam` (app/team.go:851) calls `Store().User().UpdateUpdateAt(user.Id)` between the
membership write and the sidebar categories, and treats its failure as a **hard** error —
`app.user.update_update.app_error`, 500. This port does not make that write: `UserStore` has no
`update_update_at`, and `crates/mm-store/src/user_store.rs` belonged to a sibling worktree for
the session that ported these routes.

It is on the wire. `Users.UpdateAt` is the `update_at` field of every user object and the input to
the profile etag, so after a join served by **this** server a client's `GET /users/{id}` reports
the old timestamp and a cached profile is not invalidated. `POST /teams/{id}/members` and
`POST …/members/batch` both have it; `addUserToTeamFromInvite` will inherit it.

What is owed: one method — `UPDATE Users SET UpdateAt = $2 WHERE Id = $1`, returning the value
written — appended to `UserStore`, called from `mm_app::App::join_user_to_team` where the comment
marking its absence sits, and a parity assertion comparing `GET /users/{id}`'s `update_at` across
the two servers after a join. The error branch comes with it: Go fails the whole join if the
update fails, which is a branch this port currently does not have.

The same method is one of the three things `DELETE /api/v4/teams/{team_id}/members/{user_id}`
is blocked on — `postProcessTeamMemberLeave` (app/team.go:1312) calls it too.

---

## D-243 · The team join and leave system posts are missing

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, team-member writes)

The channel-membership twin of this is [D-231]; this is the team half, and it is **larger**
because the setting that gates it defaults to `true`.

| Go function | Go site | when |
|---|---|---|
| `postJoinMessageForDefaultChannel` | app/channel.go:132 | every default channel a team join puts the user in |
| `postLeaveTeamMessage` | app/team.go:1440 | a self-removal from a team |
| `postRemoveFromTeamMessage` | app/team.go:1458 | somebody else did the removing |

All three sit behind `ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages`, which
`SetDefaults` sets to **`true`** (config.go:874) — unlike most `Experimental*` settings, and
unlike what the name suggests. So a stock Go server posts "user joined the team" into
`town-square` on every join and this port does not, and the divergence is the *default* rather
than a configuration nobody runs.

Blocked on post writes in the store: `mm_store::post_store` is read-only. The rest of the join is
ported and tested — the membership row, the sidebar categories, the default-channel memberships,
the `ChannelMemberHistory` rows and all three websocket events.

Two visible consequences beyond the missing message. The post moves `Channels.LastPostAt` and the
new member's `ChannelMembers.MsgCount`, so a parity test comparing a fresh member's channel row
has to mask both columns — `parity::team_member_writes::adding_a_member_agrees_and_joins_the_default_channels`
does, and says so. And the config field itself is **not** read by this port: when the post write
lands, `Config` needs `experimental_enable_default_channel_leave_join_messages` adding alongside
the other `ServiceSettings` fields, because both arms of that branch then matter.

## D-250 · The two thread read-state writes are blocked on `countThreadMentions`

**Status** OPEN · **Severity** unported route · **Raised** 2026-09-11 (phase 2, thread writes)

```text
PUT  /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}
POST /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/set_unread/{post_id}
```

Both reach `App.UpdateThreadReadForUser` (app/user.go:3234), whose second statement is
`a.countThreadMentions(rctx, user, post, teamID, timestamp)` (app/post.go:2505). That function
**writes its result to `ThreadMemberships.UnreadMentions`** — a column the Go server reads on
every threads-list request — so a partial or approximate port corrupts state shared with the
still-running Go server rather than merely answering wrongly. It is not a candidate for a
best-effort stub.

What it needs, none of which exists here:

| Go | where it would land | owner this round |
|---|---|---|
| `Group().GetGroups`, `GetGroupsByChannel`, `GetGroupsByTeam` | `crates/mm-store/src/group_store.rs` | not this worktree |
| `MentionKeywords` / `makeStandardMentionParser` | `crates/mm-app` (~370 lines across four Go files) | unported |
| `getExplicitMentions` + a Markdown `Inspect` walker | `crates/mm-app` | unported, and the Markdown walker has no crate yet |
| `Post().GetPostsByThread` | `crates/mm-store/src/post_store.rs` | not this worktree |

`clearPushNotification` (the `UnreadReplies == 0` arm) and the six-key `thread_read_changed`
event are straightforward once the count exists; the count is the whole blocker.

Both routes stay forwarded to Go. The *other* three routes of the family are served — see
`crates/mm-api/src/thread_writes.rs` — so `PUT …/threads/read` and both `/following` methods are
local while these two are not, which is visible as `x-mmrs-served-by` on an otherwise uniform
path family.
## D-260 · `/exportlink` is never reserved as a built-in trigger

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, command writes)

`ExportLinkProvider.GetCommand` (app/slashcommands/command_exportlink.go:32) returns `nil` — and
so frees the trigger `exportlink` for a custom slash command — unless all three of:

1. `FeatureFlags.EnableExportDirectDownload`, defaulted **`false`** (model/feature_flags.go:168);
2. `FileSettings.DedicatedExportStore`, already modelled as `Config::dedicated_export_store`;
3. the export file backend implementing `filestore.FileBackendWithLinkGenerator`.

`built_in_command_triggers` (crates/mm-app/src/command.rs) therefore never reserves it. On a
stock server that is exact and the parity suite asserts it — `POST /api/v4/commands` with trigger
`exportlink` is a 201 from both servers. With the flag on and a dedicated export store
configured, an operator could create a custom `/exportlink` here that Go refuses with
`api.command.duplicate_trigger.app_error`.

Closing it needs the feature-flag block in `Config`, which nothing else reads yet — no
`FeatureFlags.*` field is modelled at all — plus a decision about condition 3, which is a
property of a backend this server does not construct. The sibling `/test`, gated only on
`ServiceSettings.EnableTesting`, **is** handled: the field already exists and the list is
conditional on it.
---

## D-270 · The migrated writes do not append to `Audits`, and one served route reads that table

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, token writes)

`Context.LogAudit` (web/context.go:95) builds a `model.Audit` and calls `Store().Audit().Save`
**unconditionally** — no config gate, no feature flag. Five of the seven personal-access-token
writes call it, once on entry and again on success, so a token revoked through Go leaves two
`Audits` rows and the same revoke through `mm-api` leaves none.

That is observable through the API, which is what makes this an entry rather than a note:
`GET /api/v4/users/{user_id}/audits` is already served from Rust (`mm_api::audits`) and reads the
same table. So the two servers disagree about a user's audit history in proportion to how much of
their traffic each one answered.

It is **not** specific to this family. Every migrated write has the gap — the channel-member
writes, the team-member writes, the auth writes, the post writes — and it had not been written
down. Raised here because this is the first family whose Go handlers call `LogAudit` on *every*
route rather than on some of them, and because the reading route is already ported, so the
divergence can be measured rather than argued about.

What is needed: `AuditStore::save` (`mm-store/src/audit_store.rs` is read-only today) and a
`Context`-equivalent hook in `mm-api` that has the session, the request path and the client IP —
`mm_model::audit_record` is already ported in full. `LogAuditRec`/`MakeAuditRecord` are a
**separate** and much smaller question: those write to the audit *log* (mlog) rather than to the
database, so nothing over the API can see them and they need no entry.
---

## D-280 · `POST /api/v4/bots` cannot be compared with Go on this deployment

**Status** OPEN · **Severity** unverified · **Raised** 2026-09-11 (phase 2, bot writes)

`ServiceSettings.EnableBotAccountCreation` defaults to **`false`** (config.go:917) and the stack
leaves it there deliberately — `scripts/stack.sh`'s seeded bots are written straight to the tables
for exactly this reason, and turning it on changes what other routes answer. So every
`POST /api/v4/bots` against stack N is a 403 `api.bot.create_disabled`, and the route's success
path — the 201, the `Users` insert, the `Bots` insert, the rollback between them — has **no
cross-server test**.

What exists instead: `db_bot_store.rs` drives the two inserts and the rollback against the real
database, and `mm_api::bots`'s unit test pins the 201 and its trailing newline. What is missing is
the one thing only Go can supply — that the bot it creates for a given `BotPatch` is byte-identical
to the bot we create for the same patch.

What is owed, in the order it would be done: flip the row in `Configurations`, `POST /config/reload`
on the Go server so it re-reads, start a second mm-api with
`MM_SERVICESETTINGS_ENABLEBOTACCOUNTCREATION=true` (`common::SecondServer` already does this), create
one bot through each, and compare. The reload half is the risk: the configuration row is global and
`parity::config_source` compares the document, so the flip needs a lock of its own rather than the
`ACTIVE_LICENCE_ROW` pattern, which is a read/write lock over a different row.

---

## D-281 · A bot created through this server sends its owner no direct message

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, bot writes)

`App.CreateBot` (app/bot.go:139-158) finishes by opening a direct channel with the bot's owner and
posting `api.bot.teams_channels.add_message_mobile` — "Please add me to teams and channels you want
me to interact in." — into it **as the bot**, through `CreatePostAsUser`. That function is not
ported: it is the whole of `POST /api/v4/posts`, which checks the channel is not archived, refuses a
`system_`-prefixed type, applies the restricted-DM rule, runs `CreatePost` with webhooks enabled and
then marks the channel viewed. `mm_app::App::create_system_post` is **not** a substitute — it is a
different Go function with different side effects, and `add_bot_teams_channels` is not a system type.

The owner lookup *is* ported, because its non-`NotFound` branch is a wire-visible 500 and because it
is where the divergence begins. Two consequences: no DM, and Go's create can fail *after* both rows
are written (the DM or the post can error and Go returns that error) where ours cannot.

Blocked behind `CreatePostAsUser`. Unreachable today in any case — see [D-280].

---

## D-282 · Disabling a bot does not run `userDeactivated`

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, bot writes)

`App.UpdateActive` calls `userDeactivated` (app/user.go:1172) for every deactivation, and
`mm_app::App::update_active_for_bot` does not. Four things it does:

| Go call | consequence of omitting it |
|---|---|
| `SetStatusOffline` | the disabled bot keeps whatever `Status` row it had |
| `notifySysadminsBotOwnerDeactivated` | skipped for a bot anyway (`if !user.IsBot`), so no gap here |
| `disableUserBots` | **a bot that owns bots leaves them enabled** when `DisableBotsWhenOwnerIsDeactivated` is on, which is its default |
| `OAuth().RemoveAuthDataByUserId` / `PermanentDeleteAuthDataByUser` | the bot's OAuth grants survive its deactivation |

The cascade is the one with teeth, and it is also the one that will matter when
`PUT /users/{id}/active` lands: that route deactivates *people*, whose bots the setting exists to
disable. `disableUserBots` pages `GetBots` with `OwnerId` set and calls `UpdateBotActive` on each,
so the store side is already here; what it needs is the app function and an owner whose
deactivation is reachable.

The plugin hook `UserHasBeenDeactivated` is not in scope — there is no plugin host.

---

## D-283 · Session revocation does not handle an OAuth session

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-11 (phase 2, bot writes)

`PlatformService.RevokeAllSessions` (app/platform/session.go:329) branches on `session.IsOAuth`:
an OAuth session goes through `RevokeAccessToken`, which deletes the `OAuthAccessData` row as well
as the session. `mm_app::App::revoke_all_sessions_for_bot` removes every session the same way, so an
OAuth session is revoked but its access data survives — a row that can then never be cleaned up
through any route.

Unreachable from the bot routes: a bot's sessions are personal access tokens, never OAuth grants.
It becomes reachable the moment user deactivation lands. What is owed is `RevokeAccessToken`
(app/oauth.go), which needs `OAuthStore::remove_access_data` and `SessionStore::remove` — the
second already exists.

The function also lives in the wrong file. It is not bot-specific and belongs beside the other
session code; it is in `mm_app::bot` because the session store was another agent's file in the
round it was written. Move it when session revocation gets a route of its own.

---

## D-284 · `channel_writes::a_public_channels_archive_is_addressed_to_the_team` misses its event under load

**Status** OPEN · **Severity** test-harness · **Raised** 2026-09-11 (parallel route round)

Three independent worktrees reported this failing on a full run and passing in isolation; it has
**not** been reproduced on `main`, which is why this is an entry and not a fix.

```
panicked at crates/mm-api/tests/parity/channel_writes.rs:2105:5:
no channel_deleted arrived: [ ...48 events... ]
```

The test holds `common::BROADCAST_STREAM`, connects a `SocketProbe`, archives a public channel and
waits 5s for a `channel_deleted` addressed to the team. What it collects instead is 48 frames
belonging to *other* suites — `user_updated` for the `mmrsplaincs*` custom-status users,
`draft_created`/`draft_deleted`, `preferences_changed`, `sidebar_category_updated`. So the socket
is alive and receiving; the one frame under test is the only one missing.

**That rules out the obvious reading.** It is not a connect-then-act race (the connection is
plainly registered) and not the 5s window being too short for traffic in general. The lock does
not help here: it excludes the other tests that *count* frames, not the many that merely write and
broadcast.

**What is owed:** reproduce it before changing anything. The hypothesis worth testing first is
that a team-addressed broadcast is filtered against team membership the hub cached when the
connection opened, and that a sibling suite removing the shared admin from a team (there are
several: `teams_for_user.rs:66`, `users_list.rs:76`, `team_name_members.rs:82`) makes the hub drop
it. If that is it, the fix is a dedicated user for this test rather than the shared admin — not a
longer timeout, which would only make the flake rarer.

**Reproduction attempted and failed, 2026-09-11:** twelve further full-suite runs on the merge
stack, deliberately under load from four concurrent worktree builds, did not reproduce it. Three
*other* flakes surfaced in those runs and were fixed (`user_get`'s etag pair, and the bot pair
below); this one did not recur. So it is either rarer than 1 in 12 or it needs something a
worktree stack has and the merge stack does not — note that two worktrees shared stack 1 during
the round in which all three reports were made, which would put two servers and two purges on one
database. Check that before assuming the hub.

**Where the pin lives:** the doc comment on the test.

---

## D-300 · The licensed half of the seven CPA routes is forwarded, not served

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-11 (custom profile attributes)

`crate::custom_profile_attributes` serves the **unlicensed** contract of all seven routes and
forwards to Go the moment `LicenseState::Licensed` comes back. That is not a stopgap for these
routes' error paths — those are fully ported and compared — but it does mean the success path of
`POST`/`PATCH`/`DELETE /fields`, `PATCH …/values` and the two non-empty reads has never run here.

What a licensed server reaches that this side does not have, in the order it reaches them:

| behind the gate | Go |
|---|---|
| `AccessControlHook` — read filtering, owners, sync lock, access modes | `app/properties/access_control.go` |
| the attribute-validation hook — visibility, sort order, option and user-id checks, managed-flag authorisation | `app/properties/access_control_attribute_validation.go` |
| `TypeChangeValueCleanupHook` — clears dependent values on a type change | `app/properties/type_change_value_cleanup.go` |
| the group field limit | `app/properties/field_limit.go` |
| four websocket events | `custom_profile_attributes_field_{created,updated,deleted}`, `custom_profile_attributes_values_updated` |
| `App.UpsertPropertyValues`' value audit and broadcast | `app/property_value.go:169` |

**What is owed:** the write half of `mm_store::property_store` and the three hooks, behind whichever
route needs them first. Nothing here is blocked on the licence — a licence is not obtainable and is
not a reason to skip the work, only a reason nothing on this stack can *compare* it. When it lands
the comparison oracle has to be something other than the Go server beside it.

---

## D-301 · `property_store`'s two searches implement a subset of the predicates — CLOSED 2026-09-12

**Status** CLOSED · **Severity** coverage · **Raised** 2026-09-11 (custom profile attributes) ·
**Closed** 2026-09-12 by the four read routes of `api4/properties.go`, which needed all of it

Both searches now carry the whole predicate set as a **single statement whose branches are chosen
by bound parameters** — the third option the entry below did not consider, and the one that keeps
`query_as!`'s compile-time checking. `CASE` arms stand in for squirrel's `switch`, and each arm
names the Go line it reproduces. Every predicate is exercised by `parity/properties`, against a
fixture built so `create_at` order, `update_at` order and `id` order disagree.

`SqlPropertyStore::search_fields` and `search_values` implement the predicates the CPA routes set —
group, object type, target type, target ids, the implicit `DeleteAt = 0`, `PerPage` — and return
`StoreError::Argument` for any option that is set and unimplemented: cursors, delta mode
(`SinceUpdateAt`), `IncludeDeleted`, `ObjectTypes`, `LinkedFieldID`, the team/channel hierarchy and
the `Value` filter.

That is deliberate — a partially implemented predicate returns *wrong rows* silently, where a
refusal is loud — but it is still unfinished work, because `api4/properties.go`'s nine routes need
most of it. `searchPropertyFields` and `getPropertyValues` both take a cursor and a `since`, and
`getPropertyFields` scopes by team and channel.

**What is owed:** the missing predicates, which sqlx's compile-time checking cannot assemble from
an options struct the way squirrel does. Either a small set of purpose-shaped queries (one per
scope shape, which is what Go's `switch` already is) or `QueryBuilder`, which this crate does not
use anywhere yet and would be the first.

**Where the pin lives:** the module doc comment on `crates/mm-store/src/property_store.rs`.

---

## [D-330] `parity_views`' list order — CLOSED 2026-09-12, and it was never a port bug

Raised as a wire-order divergence: `views::include_total_count_and_pagination_agree` and
`views::the_list_is_byte_identical_including_the_props_key_order` disagreed with Go on row order on
every run, Go returning `CreateAt` order and our side returning `Id` order.

**Both tests were correct and so was the port.** `SecondServer::start` spawned mm-api and then
decided it was up by polling `{base}/system/ping`. When a stale mm-api already held the port, the
child failed to bind and exited, the ping was answered by the **stale process**, and `start`
returned `Some` wrapping a dead child — so the suite measured a binary from hours earlier. A
SecondServer from 00:31 held :8082. Killing it made both tests pass, three runs out of three, and
they have passed every run since.

What was established while chasing it, all of it now permanent:

* `crates/mm-store/tests/db_view_store.rs` exists. The view store had **no** DB-backed test before.
* Postgres, given the store's own `ORDER BY`, returns exactly Go's order for the rows the failing
  test produced. The store, the app layer and the handler never reordered anything.
* `SecondServer::start` frees its port before spawning **and** requires its own child to still be
  running. A ping cannot distinguish "mine came up" from "someone else's was already there", which
  was the whole defect.
* `scripts/mutations/view-routes.plan` now has its first valid run: 33 run, 28 caught, both controls
  survived.

Two entries in this file were wrong about this and are superseded by the above: the first blamed a
concurrent `total_count` race, the second a `CreateAt` tiebreak our store does in fact apply.

**The lesson worth keeping is not about views.** Twice in one session a stale server produced
confident, wrong conclusions — once through `scripts/parity.sh` (36 false failures after a
`git worktree move`) and once here. Both are fixed by killing on the **port**, which is the thing
that identifies a server, rather than on a path or a ping.

---

## D-331 · a NULL `jsonb` column is `{}` in Go, and the audit of the other sites is owed

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-12 (properties read routes)

`PropertyField.Attrs` is a Go **map**, and sqlx's `reflectx.FieldByIndexes` allocates a nil map
before scanning into it. `StringInterface.Scan` returns early for a nil driver value
(`model/utils.go:186`), so a SQL `NULL` leaves that freshly allocated empty map behind and
marshals as **`{}`** — while a jsonb `null` reaches `json.Unmarshal`, which zeroes the map, and
marshals as **`null`**. Measured on both, twice.

`mm-store`'s port had the two the wrong way round and it was invisible: the CPA reads that first
used `search_fields` can only return an empty page unlicensed, so no row ever carried an `attrs`.
Fixed for `PropertyFields`.

**What is owed:** the same question for every other `jsonb` column this crate reads into an
`Option`. `crates/mm-store` has roughly a dozen `None | Some(Value::Null) => None` sites —
`channel_store` (×4), `user_store` (×2), `team_store`, `post_store`, `job_store`, `draft_store`,
`role_store` — and each is correct **only if** Go's destination is not a bare map. A pointer or a
`*StringMap` destination really is nil for both cases; a plain `StringMap`/`StringInterface` is
not. The audit is one grep of the Go struct per site.

**Why it is not urgent:** Go's own writers never leave these columns SQL NULL — `Value()` on a nil
map emits the four bytes `null` — so the divergence needs a row written by a migration or by hand.
That is exactly how this one was found, and a migration adding a nullable `jsonb` would reach it
for real.

**Where the pin lives:** `PropertyFieldRow::into_field` in `crates/mm-store/src/property_store.rs`.

---

## D-332 · nothing proves `.sqlx/` still covers the workspace

**Status** OPEN · **Severity** tooling · **Raised** 2026-09-12 (properties read routes)

The cache itself is **fixed**: it held 87 entries against a store crate with several hundred
queries, so `SQLX_OFFLINE=true cargo check --workspace` failed on `audit_store`, `bot_store` and
many others — while `README.md` said the workspace builds with no database at all. Regenerated to
320 entries with `cargo sqlx prepare --workspace -- --all-targets`, and the offline build of every
target is clean.

**What is owed is the guard, not the cache.** It drifted for a long time and nothing noticed,
because every session runs with `DATABASE_URL` set and a live Postgres, so the offline path is
never exercised. One `SQLX_OFFLINE=true cargo check --workspace --all-targets` in `scripts/` — or
in whatever runs before a merge — is what turns the README's claim into something the tree
asserts rather than something a reader has to trust.

---

## D-340 · the `only_channel_admins` broadcast hook is not run, so a join request is announced to every channel member

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-12 (app/channel_join_request.go)

`broadcastChannelJoinRequestCreated` and `broadcastChannelJoinRequestUpdated` publish to
`Broadcast{ChannelId: …}` — the channel's whole membership — and then narrow the audience with
`useOnlyChannelAdminsHook`, whose `Process` **rejects** the event for any connection whose user is
not in the precomputed admin set (app/web_broadcast_hooks.go:519). The fan-out is the outer bound
and the hook is the filter.

[D-183] records that this server strips the hook fields and does not run the hooks. Until now that
was a fidelity gap — the stock `posted` hook *adds* fields. This is the first ported event whose
hook **removes recipients**, so dropping it does not degrade a payload, it widens an audience: a
plain member of a discoverable private channel would be told that a named user has asked to join
it, and with what status, where Go tells only the channel admins.

**What is owed:** `platform.HookedWebSocketEvent`'s reject path in the hub, plus the
`only_channel_admins` hook itself. Nothing smaller fixes it — the admin set is already computed
correctly and attached to the event (`channel_admin_user_ids`), so the missing half is entirely in
`mm-ws`.

**Why it is not urgent, and why that is not a reason to close it.** The seven routes that raise
these events are dark: `FeatureFlags.DiscoverableChannels` is false at the pinned SHA ([D-153]), so
nothing on this deployment can publish either event. The moment that flag is turned on this becomes
a disclosure bug, which is why it is recorded rather than left to the doc comment on
`publish_channel_join_request_event`.

**Where the finding lives in the code:** the module docs of
`crates/mm-app/src/channel_join_request.rs` and the doc comment on
`App::publish_channel_join_request_event`.

---

## D-350 · a session this server revokes keeps authenticating against Go

**Status** OPEN · **Severity** divergence (security-relevant) · **Raised** 2026-09-12 (session write family)

The Go server keeps sessions in an in-memory cache and invalidates it only from its own revocation
paths — `ClearUserSessionCache` (app/platform/session.go:105), which also fans out over the cluster
bus. This server has no session cache ([D-087]) and no way to reach Go's. So every route in the
session write family deletes the row and leaves Go serving the dead session until its entry ages
out.

**Measured, not inferred.** Delete a `Sessions` row by hand and `GET {go}/api/v4/users/me` still
answers **200** while `GET {rust}/api/v4/users/me` answers **401**.

This is worse than [D-087], which is a bounded staleness window on a *read*. Here a **security
control** appears to work from the client that issued it: the user sees "session revoked", their
own server agrees, and the process next door keeps honouring the credential.

What is owed is one of: a cluster message Go would accept (the enterprise bus [D-087] already
established we cannot reach), a shared cache, or — the cheap one — driving these four routes
through the proxy to Go for as long as Go is running, which trades the divergence for a forwarded
route. Not done, because the Go server is scaffolding and the end state is that it is not running
at all; the entry exists so the choice is made deliberately rather than by omission.

**Where the pin lives:** `parity::session_writes::go_cache_keeps_a_session_we_revoked` asserts the
divergence **in the direction it currently has**, so closing the gap fails that test and says so.

---

## D-351 · the all-users session revoke has no route-level parity test

**Status** OPEN · **Severity** test-coverage · **Raised** 2026-09-12 (session write family)

`POST /api/v4/users/sessions/revoke/all` is served from Rust and its **403** branch is compared
against Go. Its **200** branch is not, and cannot be under the current harness: succeeding means
`DELETE FROM Sessions` with no predicate, which logs out every other test running concurrently in
the same binary and the admin token they all share.

Covered instead by `mm_app::session::tests::the_all_users_revoke_removes_access_data_first`, which
pins the thing that actually matters — access data is deleted **before** sessions, so a revoked
client cannot trade its OAuth token for a fresh login. What is not pinned is the response bytes
(`{"status":"OK"}`, no trailing newline) against Go's own.

What would close it: a stack the suite owns exclusively for one test, or a serialised
`#[ignore]`-by-default test run by hand. Neither is worth a flaky suite for a fifteen-byte body
that `ReturnStatusOK` produces identically on three other routes in the same file.

---

## D-352 · `parity::emoji_list` fails on a different test each run

**Status** OPEN · **Severity** test-harness · **Raised** 2026-09-12 (observed during the session write family)

Not caused by the session work and not in a file it touches; recorded because it was hit three
times in a row and nothing had it written down.

The suite's tests create and delete `mmrsparity*` emoji concurrently and assert on counts and on
list membership, so each run fails on whichever test lost the race —
`an_empty_sort_is_the_unsorted_page` saw a `mmrsparitydoomed…` row another test had not deleted
yet, and `pagination_clamps_rather_than_refusing` found three emoji where it needs four. Running
the suite alone does not help, because the race is *within* it.

The shape is [D-284]'s and the memory note's "a failure naming a route you did not touch is usually
a concurrent write to shared state". What is owed is per-test emoji name prefixes, or a serial
marker on that one file.

---

## D-360 · the licensed half of the seven group writes is forwarded, not served

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-12 (group CRUD and membership)

`crate::groups` now serves the **unlicensed** contract of all seven writes in `api4/group.go` —
`createGroup`, `getGroupsByNames`, `patchGroup`, `deleteGroup`, `restoreGroup`, `addGroupMembers`,
`deleteGroupMembers` — alongside the ten reads it already answered. `requireLicense`
(api4/handlers.go:237) is the first statement of every one of them, ahead of `RequireGroupId` *and
ahead of reading the request body*, so an unlicensed server's whole contract is one 501 and that
501 is fully compared. Everything past it forwards.

Go loads its licence at startup and re-reads it only on a save, so `set_active_licence_id` moves
our answer and not Go's: on this stack the licensed side of these routes has no oracle beside it,
which is a fact to route around and not a reason the work is skipped.

What a licensed server reaches that this side does not have:

| behind the gate | Go |
|---|---|
| the `GroupStore` write surface — `Create`, `Update`, `Delete`, `Restore`, `UpsertMembers`, `DeleteMembers` | `channels/store/sqlstore/group_store.go` |
| `licensedAndConfiguredForGroupBySource` — four refusals keyed on source, two statuses | `api4/group.go:1566` |
| the custom-group permissions — `create_custom_group`, `edit_custom_group`, `delete_custom_group`, `restore_custom_group`, `manage_custom_group_members` — and `SessionHasPermissionToGroup` | `app/authorization.go` |
| `patchGroup`'s name derivation: `strings.ReplaceAll(strings.ToLower(DisplayName), " ", "-")` when `allow_reference` is turned on without a name, plus the user-name and mentionable-group collision checks | `api4/group.go:265` |
| six audit records and their `Auditable`/`LogClone` payloads | `model/group.go:45` |

Three branch-level facts are recorded here because no test on this stack can reach them and they
are the ones a later port will get wrong:

1. **`restoreGroup`'s non-custom refusal is a 501, not a 400.** Every other handler in the family
   answers `app.group.crud_permission` at `http.StatusBadRequest`; `restoreGroup` answers the same
   id at `http.StatusNotImplemented` (group.go:1367), which on the wire is indistinguishable from
   the licence error it sits behind.
2. **`deleteGroupMembers`' marshal-failure branch names `Api4.addGroupMembers`** (group.go:1516),
   copied from its neighbour. `Where` carries `json:"-"`, so it is a log-line difference only.
3. **`getGroupsByNames` short-circuits an empty list before the permission question.** An empty
   array writes a literal `[]` and returns (group.go:934), so a caller with no group permission at
   all gets a 200 — the `FilterAllowReference` computation happens after.

**What is owed:** `mm_store::group_store`'s write half and the custom-group permission checks,
behind whichever route needs them first. `mm_model::Group`'s validators are already ported and are
now pinned branch-by-branch against a generated oracle (`fixtures/behaviour_group.json`), so the
model layer is not the blocker; the store and the permission model are.

---

## D-370 · `deleteTeam?permanent=true` forwards when `EnableAPITeamDeletion` is on

**Status** OPEN · **Severity** unported-route-arm · **Raised** 2026-09-12 (team write family)

`DELETE /api/v4/teams/{team_id}` serves its **archive** arm from Rust, and the permanent arm's
refusal — the 401 whose id depends on whether the caller is a system admin — with it. What is
still Go's is the permanent deletion *itself*, reached only when
`ServiceSettings.EnableAPITeamDeletion` is true. It defaults to **false** and is unset on the
parity stack, so nothing here is reachable today.

`PermanentDeleteTeam` needs ten store methods across five stores that this tree does not have:
`Channel.GetTeamChannels`, `GetTeamSpaceChannels`, `PermanentDeleteMembersByChannel` and
`PermanentDelete`; `Post.PermanentDeleteByChannel`; `Webhook.PermanentDeleteIncomingByChannel`
and `…OutgoingByChannel`; `PostPersistentNotification.DeleteByChannel` for the team path;
`Team.RemoveAllMembersByTeam` and `Team.PermanentDelete`; plus `Command.PermanentDeleteByTeam`
and `App.PermanentDeleteChannel` to drive them. Writing that cascade blind is precisely what the
parity oracle exists to prevent: with the flag off, **no route-level test can exercise a single
one of those deletes**, and a wrong `DELETE` predicate destroys data silently.

The channel twin (`mm_api::channel_writes::delete_channel`) already forwards its permanent arm for
the same reason, so this is the established shape rather than a new exception.

What would close it: turn the flag on for one stack, port `PermanentDeleteChannel` and the ten
store methods, and compare the surviving rows in `Posts`, `ChannelMembers`, `Channels`,
`TeamMembers`, `Teams`, `Commands` and both webhook tables between two teams deleted by the two
servers.

---

## D-371 · the team write family forwards a licensed installation on two routes

**Status** OPEN · **Severity** unported-branch · **Raised** 2026-09-12 (team write family)

`deleteTeam` calls `cleanupTeamAccessControlPolicy` between the team write and the websocket
event, on **both** the archive and the permanent arm, and it needs the enterprise access-control
service. So `mm_api::teams::delete_team` forwards whole when `license_state()` says Licensed,
exactly as `mm_api::channel_writes::delete_channel` does for its channel-scope twin.

`searchTeams` has the same shape one step further out: `FilterNonQualifyingTeamsForUser` and
`AnnotateRecommendedTeamsForUser` both short-circuit unless `TeamMembershipAccessControlEnabled()`,
which is a constant `false` here — so the search is served in full rather than forwarded, and the
ABAC directory filter is simply absent. That is correct for an unlicensed server and **untested**
for a licensed one.

Both are unreachable on a `mattermost-team-edition` image with zero `Licenses` rows. Recorded so
the next person to install a licence knows which two routes change shape.

---

## D-390 · the licensed half of the three group syncable writes is forwarded, not served

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-12 (group syncables)

`crate::groups` now serves the **unlicensed** contract of the last three handlers in
`api4/group.go` — `linkGroupSyncable` (`POST .../link`), `unlinkGroupSyncable` (`DELETE .../link`)
and `patchGroupSyncable` (`PUT .../patch`) — which completes the file: all twenty route+method
pairs `InitGroup` registers are answered here, and `parity::group_syncables` re-measures every one
of them. `requireLicense` is the first statement of all three, above `RequireGroupId`,
`RequireSyncableId`, `RequireSyncableType` **and** `io.ReadAll(r.Body)`, so the unlicensed
contract is one 501 and it is fully compared. Everything past it forwards, for the reason
[D-360] gives: Go loads its licence at startup, so `set_active_licence_id` moves our answer and
not Go's, and the licensed side has no oracle beside it on this stack.

What a licensed server reaches that this side does not have:

| behind the gate | Go |
|---|---|
| `verifyLinkUnlinkPermission` — `IsSyncable`, an `AllowReference` gate, then a per-type switch | `api4/group.go:679` |
| its channel arm's **parent-team** question: a channel not yet synced via its team needs `invite_user` on the team, and the private/public channel type then picks `manage_private_channel_members` or `manage_public_channel_members` | `api4/group.go:705` |
| `verifySchemeAdminAssignmentPermission` — `manage_team_roles` / `manage_channel_roles`, skipped entirely when `patch.SchemeAdmin` is nil | `api4/group.go:573` |
| `GetGroupSyncable` / `UpsertGroupSyncable` / `UpdateGroupSyncable` / `DeleteGroupSyncable` | `channels/store/sqlstore/group_store.go` |
| `SyncRolesAndMembership` and `RemoveMembershipsFromUnlinkedSyncable`, both dispatched through `Srv().Go` **after** the response is written | `app/syncables.go` |

Four branch-level facts are recorded here because no test on this stack can reach them, and they
are the ones a later port will get wrong:

1. **`linkGroupSyncable`'s re-link deliberately discards the old row.** It upserts onto the
   existing syncable only when `DeleteAt == 0`; a fresh link *or a re-link of a soft-deleted row*
   starts from a zero-value `GroupSyncable`, so fields the caller did not set are not carried over
   from the previous incarnation (group.go:385). A port that always patched the existing row would
   resurrect `SchemeAdmin` from before the unlink.
2. **The two handlers differ in exactly one place.** `GetGroupSyncable` returning 404 is tolerated
   by `link` (it creates the row) and fatal to `patch`. Everything else — both verifiers, the
   `Patch` call, the async sync — is identical.
3. **Three routes, three response shapes.** `link` is a **201** with the marshalled syncable,
   `patch` a 200 with the same, `unlink` a 200 with `ReturnStatusOK`'s `{"status":"OK"}`.
4. **`RequireSyncableType` is unreachable through the mux.** The route pattern
   `{syncable_type:teams|channels}` refuses a third value before any handler, and `params.go:269`
   maps only those two strings onto `GroupSyncableType`. So its `SetInvalidURLParam("syncable_type")`
   branch is dead code for every HTTP caller, which is why a third value is *forwarded* for
   gorilla's own 404 rather than answered with a 400. Measured in
   `parity::group_syncables::a_third_syncable_type_is_forwarded`.

**What is owed:** `mm_store`'s `GroupSyncable` surface (the four CRUD methods plus
`TeamMembersToAdd`/`ChannelMembersToAdd`), the two permission verifiers, and `app/syncables.go`'s
membership reconciliation — behind whichever route needs them first. The team and channel member
*writes* they would build on are already ported (`mm_app::team_member`, `mm_app::channel_member`),
so the reconciliation loop is the blocker, not the membership primitives.

---

## D-380 · `createEmoji` forwards every image it does not measure, and every resize

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

`POST /api/v4/emoji` is served here for every refusal — the 501, both 413s, the multipart parse
400, the permission 403, the model's name errors, the duplicate, the missing image part, the
"not an image" 400 and the 1028×1028 refusal — and for the **write-through** image path, which is
`WriteFile` on the bytes exactly as they arrived. Three cases go to Go, and each forwards *before*
the file backend is touched, so a forwarded create leaves nothing behind:

| forwarded | why |
|---|---|
| any image that is not a PNG this port can measure | `image.DecodeConfig` is six decoders with six header grammars; `mm_app::imaging::decode_config` reproduces PNG's `parseIHDR` in full — length, CRC, compression, filter, interlace, the depth/colour-type pairs, the signed-`int32` dimension read — and answers `Undecidable` for the rest. A dimension guessed wrong is a wrong *refusal*, or a wrong acceptance, on a route that writes. |
| a paletted PNG (colour type 3) | `png.DecodeConfig` does **not** stop at IHDR for those: `cbPaletted(d.cb)` keeps the chunk loop going to `dsSeentRNS`, so it can fail on a PLTE chunk long after the dimensions were read. |
| any filename whose extension is not `.png` | `isGIF` is `mime.TypeByExtension(filepath.Ext(name))` and Go's `mime` package reads the host's `/etc/mime.types` at init, so *which* extensions mean `image/gif` is a property of the machine the Go server runs on. `.png` is in the built-in table and cannot be displaced, so it is the one extension safe to claim without consulting the host — and it is never the GIF branch. |
| any image over 128×128 | the resize path is `imaging.Fit` (Lanczos) plus `EncodePNG`, or `gif.EncodeAll` after `resizeEmojiGif`'s per-frame redraw and Floyd–Steinberg dither. A second implementation does not produce those bytes, and the emoji that lands is the resized one — so "close enough" is a different stored file. |

`parity::emoji_writes::a_resize_and_a_gif_filename_are_answered_by_go` measures the boundary in
both directions: 128×128 and 1028×1028 sit on the near side of their thresholds and are handled
here and by Go respectively, which is what makes an off-by-one in either limit visible.

**What is owed:** the GIF frame walk (`imgutils.CountGIFFrames`, an LZW decode per frame) and a
resize whose output is byte-identical to `imaging.Fit` + Go's PNG encoder. The second is the hard
one and may never be worth it; if it is not, the honest end state is that this route keeps a
forward for the resize path and the strangler does not fully retire here. Recorded now rather than
discovered later.

---

## D-381 · the multipart port does not decode RFC 2231 parameter continuations — CLOSED 2026-09-13

**Status** CLOSED · **Severity** divergence · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

**Closed by `86a802c`**, which was the condition this entry set: the continuation decoder before
the second multipart route ships. `parse_media_type` is now a function-for-function transcription
of `mime.ParseMediaType` including the `*`-attribute side map and the stitching loop, pinned over
46 rows rather than 23, and four refusals the first port had backwards are fixed with it. The
second and third multipart routes — `POST /api/v4/brand/image` and
`POST /api/v4/users/{user_id}/image` — shipped on 2026-09-13 against that decoder. What is left of
this entry is [D-410]: a decoded filename that is not valid UTF-8 is lossy in a Rust `String`.

The original text follows.

`mm_api::multipart::parse_media_type` reproduces `mime.ParseMediaType` for the forms a
`multipart/form-data` body actually carries — quoted strings with backslash escapes, lower-cased
attribute names, the duplicate-attribute error, the empty-value error, the trailing semicolon —
and is pinned against Go over a 23-row corpus (`fixtures/behaviour_emoji_upload.json`,
`parse_media_type`).

What it does not do is RFC 2231: `filename*=utf-8''x` and the `name*0=`/`name*1=` continuation
form. Go decodes those into the un-starred attribute; here `filename*` stays a separate attribute
and `filename` is absent, so such a part is read as a **value** where Go reads it as a **file**.
For `createEmoji` that means an image part sent that way would be dropped and the request answered
`api.context.invalid_body_param.app_error` naming `createEmoji`, where Go would have stored it.

No browser sends that form in a multipart body — it belongs to `Content-Disposition` on a
*response* — and `createEmoji` is the only route that reads multipart today. **What is owed:** the
continuation decoder, before the second multipart route (`POST /brand/image`,
`POST /users/{id}/image`) ships, and a corpus row for it.

---

## D-382 · the licensed half of `createTermsOfService` has no oracle on this stack

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

`POST /api/v4/terms_of_service` is `manage_system`, then `license == nil ||
!*license.Features.CustomTermsOfService` → **400**. This installation is unlicensed, so the 400 is
the whole route on the wire and it is compared against Go across six bodies — including bodies that
are not JSON, which Go never parses because the gate precedes `MapFromJSON`.

Everything past the gate *is* ported — `should_publish`, `App::create_terms_of_service`,
`TermsOfServiceStore::save` and `get`, all with tests — because licensing does not gate development
here. What is missing is an **oracle**: Go loads its licence at startup and re-reads it only on a
save, so planting an `ActiveLicenseId` row moves our answer and not Go's, and a licensed server to
compare against does not exist on this stack. The same shape as [D-360].

Two branch-level facts recorded because no test here can reach them:

1. **`App.CreateTermsOfService`'s `ErrInvalidInput` branch would nil-dereference in Go.**
   `termsOfService, err = Save(termsOfService)` assigns `nil` on failure, and the very next line
   reads `"id="+termsOfService.Id`. It is unreachable — the struct built there always has an empty
   `Id`, which is the only thing that raises `ErrInvalidInput` — and the port carries the empty-id
   string it would have produced.
2. **Re-posting identical text publishes nothing and returns the *existing* row**, id and
   `create_at` included, so a client cannot tell a no-op from a publish except by the id. The
   comparison is exact: not trimmed, not case-folded.

**What is owed:** a licensed oracle, which needs a second Go process started with `MM_LICENSE` the
way `scripts/go-discoverable.sh` starts one with a different config. Until then the licensed side
of this route and of the seven group writes are both untested in the same way.

---

## D-383 · a deleted emoji's reaction sweep does not invalidate Go's reaction cache

**Status** ACCEPTED · **Severity** divergence · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

`DELETE /api/v4/emoji/{emoji_id}` served here soft-deletes the emoji, renames its image and sweeps
the reactions that used it — all three in the shared database, all three verified against the
table. Go's `LocalCacheReactionStore` memoises `GetReactionsForPost` and is invalidated by **Go's
own** `DeleteAllWithEmojiName`, which never runs, so `GET /posts/{id}/reactions` on the Go server
can keep listing a reaction whose row is gone until the entry expires.

This is the staleness shape the emoji, terms-of-service and session ports already carry ([D-352]'s
neighbourhood): both servers agree on a settled database and disagree only inside a cache window.
It is ACCEPTED rather than OPEN because closing it means either reproducing Go's cache invalidation
over a channel we do not have, or retiring the Go server — which is the project's actual end state
and the thing that closes it.

The consequence for tests, which is the part that costs time: **assert the table, not the route**,
whenever this server writes something Go caches. `parity::emoji_writes::
deleting_an_emoji_removes_the_reactions_that_used_it` failed on its first run for exactly this
reason and now queries `reactions` directly.

---

## D-384 · `AppError.params` is on no wire, so only a unit test can pin an i18n parameter

**Status** ACCEPTED · **Severity** test-harness · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

`uploadEmojiImage`'s 1028×1028 refusal carries `MaxWidth` **and** `MaxHeight` as i18n parameters.
A mutation renaming the second key to the first — collapsing the pair to one key — survived the
entire parity suite, and it was right to: `params map[string]any` is **unexported** in Go
(utils.go:240) and `#[serde(skip)]` in `mm_model::utils::AppError`, so the field reaches neither
server's response body. It exists only to interpolate `Message`, and `Message` is the one field
[D-092] already tolerates as differing until an i18n bundle lands.

So a cross-server body comparison is *structurally* blind to every `params` key in the tree, and
no amount of fixture work on the parity side can change that. `mm_app::emoji`'s
`the_too_large_refusal_names_both_dimensions` pins the pair as a unit test instead, against the
oracle's own `png_header_1029x1028` bytes; the values themselves stay transcribed from
app/emoji.go:36 because Go will not hand them out. `an_over_tall_image_is_refused_by_the_height_half`
is its companion: `width > MAX || height > MAX` short-circuits, so only an input inside the width
limit and outside the height one ever evaluates the second comparison.

ACCEPTED rather than OPEN: nothing is owed here beyond the habit. The same blindness applies to the
three `Group*MaxLength` params already recorded in `reference/dump/behaviour_group.go`'s header and
to `TermsOfService.IsValid`'s `MaxLength`. **When a port builds a `params` map, pin it with a unit
test — a parity test cannot see it.**

---

## D-385 · a mutation on a write route leaves debris in the shared database

**Status** ACCEPTED · **Severity** test-harness · **Raised** 2026-09-12 (emoji writes and the terms-of-service pair)

`scripts/mutate.sh` restores the **source** on every exit, including SIGINT — which its own comment
is careful about. It cannot restore the **database**, and for a write route that is the more
consequential half: a mutation that disables a refusal performs the write it was supposed to
prevent, the test then fails (CAUGHT, correctly), and the row it wrote survives the rollback and
poisons every later run.

Two instances in one batch, both found by a clean `cargo test --workspace` afterwards rather than
by the batch:

1. `emoji-save-skips-validation` removed `SqlEmojiStore::save`'s `IsValid` call and inserted a live
   emoji named `grinning`. `db_emoji_and_terms_writes`'s purge swept the `mmrsew%` prefix, which
   that name cannot carry — it has to be a real system-emoji name to be the case under test. The
   suite then failed on every run until the row was deleted by hand.
2. `tos-licence-gate-inverted` published a terms-of-service revision, which became "the latest" and
   failed `parity::terms_of_service` twice over — and Go's `"latest"` cache kept serving it after
   the row was deleted, so the Go server had to be restarted as well.

Both fixtures now sweep what their own subject would have written: the emoji purge names the two
refused names explicitly, and `parity::terms_of_service::plant` deletes every revision newer than
its own latest.

ACCEPTED because there is nothing to build — the rule is the deliverable, and it belongs to every
write family from here on:

> **A test whose subject is "this write must not happen" has to purge the write it asserts
> against** — including values that cannot carry the suite's name prefix, which are exactly the
> interesting ones. And after any mutation batch over a write route, run the full suite once on a
> clean tree before quoting a tally; the batch's own verdicts do not see the debris they leave.

---

## D-400 · the pending-post-id deduplication cache is per-server while Go is still running

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-12 (createPost)

Go's `Server.seenPendingPostIdsCache` (app/post.go:28) is in-process, and so is
`mm_app::App::pending_post_ids`. While both servers run they are independent: a post created
through Go — because it was forwarded, or because a client talked to Go directly — leaves no entry
here, so a retry that this server answers creates a **second post** rather than returning the
first. The mirror case is the same.

This is [D-191]'s shape, not a new one: the status cache has the same property for the same
reason, and the same resolution — it ends when the Go process does. It is recorded rather than
fixed because the alternative is a shared cache (Redis, or a table), which is infrastructure
neither server has and which Go would not read anyway.

Narrower than it sounds in practice: the retry window is `pendingPostIDsCacheTTL`, thirty seconds,
and the webapp sends a pending id only on its own retries. It is listed as a divergence and not as
coverage because a duplicate message is user-visible.

One further gap inside our own cache. Go claims the pending id at the *top* of `CreatePost` and
holds it across the author lookup, the root fetch, `FillInPostProps`, the plugin hook and the
embed pipeline; we claim it after `refuse_create_post_shapes`, which is much later. So two
genuinely concurrent requests carrying the same pending id have a smaller window here in which the
second is answered with Go's 500 `api.post.deduplicate_create_post.pending`, and outside that
window we create two posts where Go creates one. Closing it means claiming earlier, which cannot
be done without claiming on shapes we then forward.

## D-401 · createPost serves one shape and forwards the rest

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-12 (createPost)

`POST /api/v4/posts` answers a plain root-level message in an open or private channel and
forwards everything else. `mm_app::post_create::App::refuse_create_post_shapes` is the complete
list and carries the Go branch behind each arm; what is owed, grouped by the subsystem that would
unblock it:

| forwarded shape | what it needs |
|---|---|
| a reply (`root_id` set) | `updateThreadsFromPosts` — a `Threads` row and a `ThreadMemberships` row; plus `ResolvePersistentNotification` and the CRT follower fan-out |
| `file_ids` | `FileInfoStore::attach_to_post`, and `Post().Overwrite` for the partial-attachment path |
| a `PostPriority` | `savePostsPriority`, `savePostsPersistentNotifications` |
| `burn_on_read` | the `TemporaryPost` and `ReadReceipts` stores, and `RevealBurnOnReadPostsForUser` |
| any non-default post type | `card` reads `FeatureFlags.IntegratedBoards`; `custom_*` is a plugin's |
| a DM or group message | `SendAutoResponseIfNecessary`, which writes a second post |
| a shared channel | the shared-channel sync service |
| a message with a link | `getFirstLink`, `getLinkMetadata`, the permalink preview and the `previewed_post` prop |
| a message with `@` or `~`, or a channel with a keyword-mention recipient | the mention engine and `Channel().IncrementMentionCount` |
| a channel whose team has an outgoing webhook | `handleWebhookEvents`, whose *response* Go turns into a post |
| `?silent=true` | the notification suppression the prop names |
| the nine props in `REFUSED_CREATE_PROPS` | the username/icon overrides and the integration-authority re-derivation |

The mention engine is the largest single unlock: it removes three rows at once and it is what
`SendNotifications` is built around.

## D-402 · email, push and plugin hooks do not fire for a post this server writes

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-12 (createPost)

A post served here publishes the `posted` websocket event and nothing else.
`sendNotificationEmail`, `sendPushNotification`, `SendAutoResponseIfNecessary`,
`MessageWillBePosted` and `MessageHasBeenPosted` are all absent.

Deliberately *not* turned into forward conditions, unlike the mention fan-out. The distinction is
whether the effect is observable: the mention fan-out writes `ChannelMembers.MentionCount`, which
any later read diverges on, while email and push leave the database untouched and the plugin hooks
have no environment to run in at all ([D-183], which `update_post` already ships).
`SendAutoResponseIfNecessary` *does* write a post, which is why DMs and group messages are
forwarded rather than covered by this entry.

What that costs: on a server with `EmailSettings.SendEmailNotifications` or
`SendPushNotifications` on, a message posted through this server notifies nobody. Both default to
false. Closing it means the notification pipeline, which is the same unlock [D-401] names.

---

## D-410 · a percent-decoded multipart filename that is not valid UTF-8 is lossy here

**Status** OPEN · **Severity** divergence · **Raised** 2026-09-13 (the four image routes)

`mm_api::multipart::percent_hex_unescape` (a port of `mime/mediatype.go:345`) yields arbitrary
bytes in Go and Go stores them in a `string`, which need not be valid UTF-8. A Rust `String`
cannot hold that, so `filename*=utf-8''%ff` decodes lossily here — the replacement character — and
byte-exactly there.

Nothing observable turns on it today. The four multipart routes this server answers read a
filename only to decide whether a part is a *file* or a *value* (`filename` present and non-empty),
and `createEmoji` reads its extension; none reads its content, and none puts it on the wire. The
first route that echoes an uploaded filename back to the client — `POST /api/v4/files` is the one
that will — makes it visible.

**What is owed:** carry the parameter map as `Vec<u8>` rather than `String`, or record the
divergence at the one call site that would show it, before a route echoes a filename.

---

## D-411 · every write on the four image routes is Go's; only the refusals are served

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-13 (the four image routes)

`POST /api/v4/users/{user_id}/image`, `DELETE /api/v4/users/{user_id}/image`,
`GET /api/v4/users/{user_id}/image/default` and `POST /api/v4/brand/image` answer every refusal
from here and forward the moment none has fired. Three different reasons, none of them the same
as the others:

1. **`SetProfileImage`** decodes the upload, rotates it by its EXIF orientation, `FillCenter`s it
   to 128×128 and re-encodes it as PNG — *every* accepted upload, PNG or not. There is no
   write-through case as there is for `createEmoji`, because Go never stores the client's bytes.
   Same reason as [D-380]: `imaging.Fit` plus Go's PNG encoder do not reproduce from a second
   implementation.
2. **`SetDefaultProfileImage`** and **`getDefaultProfileImage`** are the freetype rasteriser of
   [D-204] and nothing else — an FNV-1a hash picks one of 26 colours and the username's first
   character is drawn at 64pt through `fonts/nunito-bold.ttf`. The **bot** branch
   (`botDefaultImage`) is a `//go:embed` of a fixed PNG and *is* constant, but reproducing it
   means copying a binary out of the read-only reference tree.
3. **`SaveBrandImage`** re-encodes with `imgEncoder.EncodePNG`, so the stored bytes are Go's for
   every accepted upload including one that was already a PNG.

Each hand-over is before the file backend is touched, and a test says so rather than a comment:
`image_writes::a_profile_upload_that_go_refuses_is_forwarded_without_writing` sends a body every
refusal passes and Go's own decode then rejects, and checks `LastPictureUpdate` did not move;
`the_brand_upload_forwards_before_it_writes` does the same against `GET /api/v4/brand/image`, so
the archive `MoveFile` and the `WriteFile` are both provably past the forward.

**What is owed:** a decision about pixel-exact image work, which is the same decision [D-380]
deferred. Until it is taken these four are refusal-only, and the `Go server that is not running`
end state is not reached for them. The profile POST additionally needs `SetProfileImage`'s
`Users.UpdateAt` bump, its `LastPictureUpdate` write and its `user_updated` websocket event; the
DELETE needs `ResetLastPictureUpdate` and the same event.

---

## D-412 · three refusal families on the image routes have no Go oracle on this stack

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-13 (the four image routes)

Most of the refusals these four routes give are compared byte for byte against the live Go server —
every invalid id, every permission denial, every unparseable body, every missing `image` part and
the 400/404 split on a user that does not exist. Three families are not, because the stack's Go server cannot be asked for them without breaking
every other suite in the binary, and each is measured against a **second mm-api** with the setting
changed and expected values transcribed from the Go source instead:

| family | what it needs | test |
|---|---|---|
| the three 501s | `FileSettings.DriverName == ""`, which `file_bytes` depends on not being | `a_driverless_server_501s_in_three_different_places` |
| both size limits | `MaxFileSize` small enough to reach by sending bytes; it is 100 MiB here | `both_size_limits_are_where_go_puts_them` |
| the LDAP 409 | `LdapSettings.PictureAttribute` set, and `Users.AuthService` written by SQL | `ldap_owns_the_picture_only_when_an_attribute_names_it` |

Each names its transcription in its own doc comment. The *ordering* each family witnesses is
genuinely measured — the permission ahead of the storage check, the storage check ahead of the
body on one route and behind it on another — because those are visible from the second server
alone. What is transcribed is the status and the error id.

A fourth branch is not covered at all: `getDefaultProfileImage`'s `view_members` 403 is
unreachable here, since `GetViewUsersRestrictions` is `None` for every pair on this stack and
`user_can_see_other_user` forwards rather than answers when it is not.

**What is owed:** a driverless Go server, the way `scripts/go-discoverable.sh` and
`scripts/go-boards.sh` are third servers for a feature flag — it would turn the first family from
transcribed into measured, and the same trick with `MM_FILESETTINGS_MAXFILESIZE` would do the
second.

---

## D-413 · the profile-field lock forwards a licensed server

**Status** OPEN · **Severity** coverage · **Raised** 2026-09-13 (the four image routes)

`IsProfileImageLockedForUser` (app/user.go:1465) is a conjunction of four predicates, and the
third is `model.MinimumEnterpriseLicense(a.License())` — `LicenseToLicenseTier[SkuShortName] >=
EnterpriseTier`. `App::license_state` can see *whether* a licence row exists and never its SKU
tier, so that conjunct cannot be answered here.

`is_profile_image_locked_for_user` therefore evaluates the licence **last**, which reordering a
conjunction of pure predicates does not change: an unlicensed server is `Ok(false)` outright, and
a licensed one is forwarded *only when the other three already hold* — the caller lacks
`edit_other_users`, the account is email/password, and `LockProfileFieldsForEmailUsers` is `"all"`.
On a stock server that setting is `"none"`, so the forward is unreachable without an
administrator turning it on.

Both `setProfileImage` and `setDefaultProfileImage` check the lock **last**, so this forward too is
before any write.

**What is owed:** the SKU tier on `LicenseState`, which the same gap blocks in [D-300], [D-360],
[D-371] and [D-390]. One port of `LicenseToLicenseTier` closes all five.
