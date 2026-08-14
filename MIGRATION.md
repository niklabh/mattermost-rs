# Migration Ledger

Go source pinned at: mattermost@9dfbaeca99f4096388fd1c048a9e6d1d0a86743e (2026-08-13)
Current phase: 1 — Core Types
Next file: server/public/model/post_metadata.go (136 ln) — both its leaf deps have landed

Re-clone the reference source by fetching the pinned SHA directly. A plain
`git clone --depth 1` fetches only the current tip, so the subsequent `checkout` fails as soon as
upstream moves past the pin:

```sh
git init reference/mattermost
git -C reference/mattermost remote add origin https://github.com/mattermost/mattermost.git
git -C reference/mattermost fetch --depth 1 origin 9dfbaeca99f4096388fd1c048a9e6d1d0a86743e
git -C reference/mattermost checkout FETCH_HEAD
```

## Verified line ranges (valid at the pinned SHA)

`server/channels/store/store.go` is 1,472 lines — never read it whole. Use `sed -n 'A,Bp'`.

| Interface | Range |
|---|---|
| TeamStore | 135,199 |
| ChannelStore | 200,386 |
| PostStore | 387,447 |
| UserStore | 448,550 |
| SessionStore | 551,600 |

## Out of scope (do not migrate)

| Path | Reason |
|---|---|
| model/client4.go (8,526 ln) | Go REST client, not server code |
| model/permission.go (2,789 ln) | Generate from Go, do not hand-translate |
| model/config.go (5,795 ln) | Translate lazily, section by section |
| enterprise/ | Separate license; proxy to Go permanently |
| plugin host (hashicorp/go-plugin) | Keep Go process alive indefinitely |
| search (Bleve / Elasticsearch) | Deferred past Phase 5 |

Deferred work and known divergences live in `docs/TECH_DEBT.md`, not here. Log an entry there
whenever a session skips, approximates, or discovers-but-does-not-close something.

## Progress

| Go source | Rust target | Status | Tests | Notes |
|---|---|---|---|---|
| model/session.go | `mm-model/src/session.rs` | DONE | 20 pass | Strangler Fig critical path. Complete `IsValid`, `PreSave`, device-id validators. |
| model/team_member.go | `mm-model/src/team_member.rs` | DONE | 6 pass | Pulled ahead of its turn: `Session.TeamMembers` is on the wire, so session.rs cannot round-trip without it. `TeamMemberWithError`/`EmailInviteWithError` deferred. |
| model/team.go | `mm-model/src/team.rs` | DONE | 37 pass | First **complete** `IsValid` — every branch, all error ids. `Etag` landed with `channel_list.go`. |
| model/utils.go (IsValidEmail) | `mm-model/src/utils.rs` | DONE | 2,916 cases | Corpus-verified against Go: 128 hand-picked + 2,788 generated. Grammar is `dot-atom @ (dot-atom / [ip])`. |
| model/user.go | `mm-model/src/user.rs` | PARTIAL | 54 pass | Wire type + self-contained logic + the five custom-status accessors ([D-004] closed). Deferred: `IsValid` and `PreSave` (still need `IsValidLocale` ([D-001]) + a password hasher + timezone defaults), `pre_update`'s custom-status re-save, `IsValidUserRoles`, `CleanUsername`, `GetTimezoneLocation`. `Etag` landed with `channel_list.go`. `pre_save_partial` is named so deliberately — it does NOT hash passwords. |
| model/utils.go | `mm-model/src/utils.rs` | PARTIAL | 54 pass | See notes below. Deferred: `IsValidHTTPURL` (needs an RFC 3986 parser), `ParseHashtags` (goes with post.go), `Scan`/`Value` (go to mm-store), the io.Reader JSON helpers (serde replaces them), `NewRandomTeamName` (needs `IsReservedTeamName`). `Etag` and `ToJSON` have landed. |
| model/channel.go | `mm-model/src/channel.rs` | DONE | 41 pass | Complete `IsValid`/`IsValidBoard`/`Patch`/`PreSave`, all 12 wire types with a fixture, DM/GM naming. 15 of the 41 are oracle-driven. Deferred: `Scan`/`Value` (D-013). |
| model/channel_list.go | `mm-model/src/channel_list.rs` | DONE | 13 pass | Two `#[serde(transparent)]` newtypes plus their `Etag`. Unblocked by `CURRENT_VERSION`; also closed `Team::etag`, `User::etag` and `ChannelsWithCount` — D-010 and D-014 are both paid off. |
| model/utils.go (Etag) | `mm-model/src/utils.rs` | DONE | 11 diff cases | `etag(&[&dyn Display])` — Go is variadic over `any` with `%v`. `CURRENT_VERSION` is borrowed from `version.go` but **cannot drift**: the oracle records it and a test fails when the pinned SHA moves. |
| model/channel_member.go | `mm-model/src/channel_member.rs` | DONE | 30 pass | Complete `IsValid`, the six-key notify-props validator with both `allowMissingFields` modes, all 9 wire types with a fixture. Also closed the `DirectChannelForExport` half of D-014 in `channel.rs`. Deferred: `Auditable`. |
| model/post_embed.go | `mm-model/src/post_embed.rs` | DONE | 9 pass | Whole file except `Auditable` ([D-028]). Three output states for `data`, an `any` with `omitempty`. Wire format byte-for-byte against Go's **round-trip**, not its output — `data: null` is lossy in Go too. |
| model/post_acknowledgement.go | `mm-model/src/post_acknowledgement.rs` | DONE | 9 pass | Whole file. The only ported type whose `remote_id` has `omitempty`. Deferred: nothing. |
| model/file_info.go | `mm-model/src/file_info.rs` | PARTIAL | 23 pass | `FileInfo`, `GetFileInfosOptions`, `IsValid`, `PreSave`, `IsValidFilename`, `SanitizeFilename`, `IsImage`/`IsSvg`, `GetEtagForFileInfos`, `MakeContentInaccessible`. Wire format asserted **byte-for-byte**. Deferred: `Auditable` ([D-028]) and `NewInfo`'s mime lookup ([D-030]). |
| model/reaction.go | `mm-model/src/reaction.rs` | DONE | 12 pass | Whole file: `IsValid`, `PreSave`, `PreUpdate`, `GetRemoteID`. Reuses `is_valid_alpha_num_hyphen_underscore_plus` on **measured** evidence that Go's inline pattern is equivalent. Deferred: nothing. |
| model/emoji.go | `mm-model/src/emoji.rs` | DONE | 18 pass | Whole file except `Auditable` ([D-028]). The 4,464-entry system-emoji table is **generated** from Go into `emoji_generated.rs` rather than transcribed. Deferred: `Auditable`. |
| model/emoji_data.go | `mm-model/src/emoji_generated.rs` | GENERATED | — | 4,464 entries emitted by `reference/dump`. Never hand-edit; re-run the generator. Carries `#[rustfmt::skip]` so `cargo fmt` and the generator stay idempotent against each other. |
| — (shared) | `mm-model/src/utils.rs::go_to_lower` | DONE | 2 pass, 30 cases | Go's `strings.ToLower`. **Not** `str::to_lowercase` — they disagree on `İ` and on final sigma. Replaced all six pre-existing `to_lowercase()` call sites. |
| model/preference.go | `mm-model/src/preference.rs` | DONE | 14 pass | Whole file: complete `IsValid` (every branch and error id), `PreUpdate`, `Preferences`, and all 42 constants pinned against Go. `PreUpdate` output is asserted **byte-for-byte**. Deferred: nothing. |
| model/status.go | `mm-model/src/status.rs` | DONE | 13 pass | Whole file. `STATUS_ONLINE` moved here from `user.rs::external`, and `STATUS_CACHE_SIZE` aliases `session::SESSION_CACHE_SIZE` rather than re-transcribing it — two more D-005 borrows closed. `to_json`/`status_list_to_json` are asserted **byte-for-byte** against Go. Deferred: nothing. |
| model/custom_status.go | `mm-model/src/custom_status.rs` | DONE | 21 pass | Whole file. First type whose timestamp is a real `time.Time`, not epoch ms — see `utils::go_time`. `USER_PROPS_KEY_CUSTOM_STATUS` moved here from `user.rs::external`. Deferred: nothing. The five `User` accessors that consume it landed the same day in `user.rs`. |
| — (shared) | `mm-model/src/utils.rs::go_time` | DONE | 51 cases | Go's `time.Time` JSON codec, not a Mattermost source file — same category as `go_json_marshal_string_map`. chrono's serde impl is **not** substitutable: four documented differences. |
| — (shared) | `mm-model/src/utils.rs::go_json_marshal` | DONE | 3 unit + 1 diff | `json.Marshal` with Go's HTML escaping for any `Serialize` value. Closes [D-022]. Use it — not `serde_json::to_string` — whenever a marshalled string is **stored** rather than sent. |
| model/version.go | `mm-model/src/version.rs` | DONE | 14 pass | Whole file. `CURRENT_VERSION` moved here from `utils.rs`, which now re-exports it — one definition, D-005's borrow closed. `VERSIONS`/`VERSIONS_WITHOUT_HOTFIXES` are unexported in Go; the oracle extracts the literal with `go/parser` so the transcription is checked. Deferred: nothing. |
| model/utils.go (ToJSON) | `mm-model/src/utils.rs` | DONE | 11 diff cases | `go_json_marshal_string_map` — the `map[string]string` case. Needed because the notify-props size cap **measures** Go's JSON, and serde_json escapes differently. |
| — (tooling) | `reference/dump/behaviour_post_leaves.go` → `fixtures/behaviour_post_leaves.json` | DONE | 8 diff tests | 13 `PostEmbed` wire probes driving `omitempty`-on-an-interface, 5 acknowledgement probes, 10 `IsValid` cases, and the three-way `remote_id` comparison. Records Go's own **round-trip** alongside its output, because one case is lossy in Go. |
| — (tooling) | `reference/dump/behaviour_file_info.go` → `fixtures/behaviour_file_info.json` | DONE | 11 diff tests | 12 byte-exact wire probes, 25 `IsValid` cases, a 44-name filename corpus run through **both** `IsValidFilename` and `SanitizeFilename`, plus `PreSave`, mime predicates, etags and `MakeContentInaccessible`. |
| — (tooling) | `reference/dump/behaviour_reaction.go` → `fixtures/behaviour_reaction.json` | DONE | 4 diff tests | 22 `IsValid` cases, `PreSave`/`PreUpdate` invariants over 5 starting states each, and a 32-input **regex-equivalence** corpus running Go's two emoji-name patterns side by side. |
| — (tooling) | `reference/dump/behaviour_emoji.go` → `fixtures/behaviour_emoji.json` + `emoji_generated.rs` | DONE | 8 diff tests | The first generator that emits **Rust source**, not just a fixture. Also pins 22 `IsValidEmojiName` cases, 16 `IsValid` cases, `PreSave` invariants, the reverse-unicode map and 16 `EmojiPattern` scans. |
| — (tooling) | `reference/dump/behaviour_preference.go` → `fixtures/behaviour_preference.json` | DONE | 3 diff tests | 52 `IsValid` cases across all six branches, 21 `PreUpdate` cases, and every exported constant. Each `IsValid` case embeds the preference as Go-marshalled JSON, so a wire drift and a logic drift fail the same test. |
| — (tooling) | `reference/dump/behaviour_status.go` → `fixtures/behaviour_status.json` | DONE | 5 diff tests | The three marshallers plus the constants. Records each result **twice** — as parsed JSON for readability and as an exact byte string — so the Rust side can assert field order and the stripped key, not just an equal `Value` graph. |
| — (tooling) | `reference/dump/behaviour_custom_status.go` → `fixtures/behaviour_custom_status.json` | DONE | 10 diff tests | 19 `time.Time` marshal cases and 38 unmarshal cases pinning Go's RFC 3339 codec, 63 `PreSave`/validity cases, and 14 statuses run through `Contains`/`Add`/`Remove`. The clock-dependent cases record an **offset from now** rather than an instant, so the fixture stays deterministic. |
| — (tooling) | `reference/dump/behaviour_version.go` → `fixtures/behaviour_version.json` | DONE | 7 diff tests | 52 `SplitVersion` cases and 34 shared inputs through each of the three lookups, plus both unexported tables. First oracle to read the **Go source** (`go/parser`) rather than call the package — `versions` is unexported, so calling it cannot recover the list. |
| — (tooling) | `reference/dump/behaviour_channel_member.go` → `fixtures/behaviour_channel_member.json` | DONE | 8 diff tests | 58 notify-props cases (29 inputs × both flag values), 13 `IsValid` cases, the ToJSON encoding corpus, and the `SetChannelMuted` truth table. |
| — (tooling) | `reference/dump/behaviour_channel.go` → `fixtures/behaviour_channel.json` | DONE | 15 diff tests | 67 whole-channel `IsValid` cases plus `Patch`, `PreSave`, `Sanitize`, both regexes and the DM/GM helpers. Each case embeds the channel as **Go-marshalled JSON**, so a wire drift and a logic drift both fail the same test. |
| — (tooling) | `reference/dump/behaviour.go` → `fixtures/behaviour_utils.json` | DONE | 12 diff tests | Behavioural oracle: runs a corpus through the real Go funcs and records the answers. Caught two bugs a reading of the source did not. Extend the corpus when translating anything with branching logic. |
| — (tooling) | `reference/dump/` → `fixtures/` | DONE | 10 fixtures | Parity oracle. Reflection-populated from zero values, so adding a type is one registry line; deterministic output (FNV of field path — no rand/time.Now, keeps diffs clean). Fails the run if a declared top-level key is missing from the JSON. Re-run and commit after adding a type. |

## Notes — model/utils.go

Non-obvious semantics found while translating. Each cost real time to discover; none is
recoverable by re-reading the Go source casually.

1. **`NewId()`'s doc comment is wrong.** It claims `[A-Z0-9]`. The alphabet is z-base-32
   `ybndrfg8ejkmcpqxot1uwisza345h769` — lowercase, and missing `l`, `v`, `0`, `2`. Do not
   "correct" a validator to match the comment.

2. **`IsValidId` does not check the alphabet.** It accepts any 26-**byte** string whose runes
   are all Unicode letters or numbers, so 13 two-byte letters pass. Length is bytes, not runes.

3. **`unicode.IsLetter` != `char::is_alphabetic()`.** Go tests general category `L`; Rust's
   method tests the Alphabetic *property*, which also covers `Other_Alphabetic` — so it
   accepts U+0345 and similar combining marks that Go rejects. `mm-model` depends on
   `unicode-general-category` solely to close this gap. The same trap applies anywhere Go
   uses `unicode.IsLetter`/`IsNumber`.

4. **`GetTimeForMillis` returns LOCAL time, not UTC** (`time.UnixMilli` attaches
   `time.Local`). `GetStartOfDayMillis`/`GetEndOfDayMillis` then read the calendar date off
   that local zone, so **their results depend on the server's timezone**. For
   `1700000000000` a UTC+05:30 host reports the start of Nov 15; a UTC host reports Nov 14.
   Any Rust caller doing date arithmetic inherits this. Treat as a Go bug we must reproduce.

5. **`AppError.Error()` emits a trailing `"W: "`** when `Message == NoTranslation` and the
   detail is empty — the separator is written before the message is skipped.

6. **`LimitBytes` can return invalid UTF-8** (it slices at an exact byte offset). A Rust
   `String` cannot represent that, so `limit_bytes` truncates at the nearest char boundary
   below the limit. This is the one deliberate behavioural divergence in the file.

7. **Go nil map marshals to `null`, not `{}`.** Struct fields that Go can leave nil must be
   `Option<StringMap>`, or `user.go` and friends will drift on the wire.

## Notes — model/user.go

1. **Three field shapes decide the wire format and all three are easy to miss.** `props` and
   `notify_props` have `omitempty`, which drops nil **and** empty maps — but nil vs empty is
   semantically meaningful (`MakeNonNil` and `GetOriginalRemoteID` branch on nil), so they are
   `Option<StringMap>` plus an emptiness skip predicate. `timezone` has **no** `omitempty`, so
   a nil map must serialise as `null`. `auth_data` is `*string` + `omitempty`, so `Some("")`
   serialises as `""` — `Sanitize` depends on that; it sets a pointer to empty, not nil.

2. **`GetRoles` and `IsInRole` split differently.** `GetRoles` uses `strings.Fields` (any
   whitespace run); `IsInRole` uses `Split(" ")`. A double space is harmless — Split yields an
   empty middle element and both roles still match — but a **tab** makes `IsInRole` miss every
   role while `GetRoles` still returns them. Verified against Go, not reasoned.

3. **`UpdateMentionKeysFromUsername` writes a leading comma.** When any key survives the value
   becomes `",key1,key2"` — Go concatenates onto an emptied string. Reproduced as-is.

4. **`ToPatch` does not carry `RemoteId`**, although `Patch` applies it. Round-tripping a user
   through `ToPatch` silently drops it.

5. **`InvalidUserError` leaves a leading space** in `detailed_error` when `user_id` is empty:
   the format string always starts with `" %s=%v"`.

6. **`PreUpdate` sanitizes the name fields twice** (user.go:555-558, then again at 565-568).
   Idempotent, so the repeat is not reproduced.

7. **`IsValidUserAuthService` is inferred, not read.** Its Go body was not opened this session;
   the accepted set comes from the auth-service constants. Confirm when `ldap.go`/`saml.go` land.

8. **Constants borrowed from six other Go files** live in `user::external` (role, ldap, saml,
   config, custom_status, shared_channel, status). Move each into its own module as those
   files are translated.

## Notes — IsValidEmail

Verified against Go over 2,916 inputs (128 hand-picked + 2,788 deterministically generated),
not reasoned about. The accepted grammar is much narrower than RFC 5322 because Mattermost
composes three checks:

1. `isLower` — input must equal its own lowercasing.
2. `mail.ParseAddress` succeeds **and** `addr.Address == input`. This equality does most of
   the work: display names, angle brackets, comments and every quoted local part normalise to
   something different from the input and are therefore rejected.
3. At most one `@`.

What survives is exactly `dot-atom "@" ( dot-atom / "[" ip "]" )`.

- **Non-ASCII is atext.** `日本@example.com`, `ünicode@x.com`, even `a\u00A0b@x.com` (NBSP) and
  emoji are accepted. Go's parser treats any rune > 127 as valid atext (RFC 6532).
- **The bracketed domain is an IP, not free `dtext`.** `a@[::1]` and `a@[127.0.0.1]` pass;
  `a@[abc]`, `a@[1.2.3]`, `a@[01.2.3.4]` (leading zero) and `a@[fe80::1%eth0]` (zone) do not.
  Rust's `IpAddr` parser agrees with Go's on every probe. The `IPv6:` prefix Go also accepts is
  unreachable — its uppercase fails check 1.
- **Domains need no dot**: `a@b` is valid. Hyphens and underscores are fine anywhere in the
  domain (`a@-b.com`, `a@b_c.com`) because they are atext; only empty labels fail.

`IsValidLocale` is measured but not ported — it needs the IANA subtag registry embedded. See
[D-001] in `docs/TECH_DEBT.md`; it blocks `User::is_valid` ([D-002]).

## Notes — model/team.go

1. **Go's error ids do not match the fields they guard.** A too-long `Name` returns
   `model.team.is_valid.url.app_error`, while an invalid `DisplayName` returns
   `...is_valid.name.app_error`. Both email failures share `...is_valid.email.app_error`. Clients
   key off these strings, so they are wire surface — do not tidy them.

2. **`IsReservedTeamName` is a prefix test**, not equality (`strings.Index(s, value) == 0`). So
   `administrators`, `apiary` and `postmaster` are all reserved team names.

3. **`CleanTeamName` removes every occurrence of a reserved word, not just the prefix that
   triggered it.** `adminxadmin` becomes `x`, which is then too short to be a valid team name, so
   Go falls back to `NewId()`. The intuitive answer (`"x"`) is wrong; the oracle caught it.

4. **`Team::PreSave` overwrites `CreateAt` unconditionally**, unlike `User::PreSave` which
   preserves a non-zero value. An inbound `create_at` on a team is always discarded.

5. **The three pointer fields have no `omitempty`.** `scheme_id`, `group_constrained` and
   `policy_id` serialise as `null` when nil; the keys are always present.

6. **`TeamForExport.SchemeName` has no json tag**, so the wire key is the Go field name verbatim,
   capital S included, sitting alongside the inlined snake_case `Team` fields.

## Notes — model/channel.go

All of these are oracle results, not readings. Several contradict what the source suggests.

1. **`ChannelNameMaxLength` (64) is never enforced.** `IsValid` calls
   `IsValidChannelIdentifier`, which checks only the *minimum* length. A 200-character channel
   name is valid. The constant is used solely to truncate group-DM display names.

2. **An empty `display_name` is valid**, unlike `Team`, which rejects it. Only the 64-rune
   maximum is checked.

3. **`creator_id` is length-checked, never validated.** `len(o.CreatorId) > 26` — bytes — so
   `"nope"` passes and a 27-byte valid-looking id fails. Its error also carries no `id=` detail
   while the checks around it do.

4. **The DM/GM name-collision guard covers `S`, `BO` and `BP`, not just `O`/`P`.** Any channel
   that is not `D` or `G` is rejected if its name is 40 lowercase hex digits or `id__id`. Go
   re-tests `Type != Direct` inside a branch that already guarantees it; that inner test is dead
   code and is not reproduced.

5. **The banner text limit is bytes, the display-name and header limits are runes.** 400
   snowmen (1,200 bytes, 400 runes) is rejected as too long; a 65-rune display name is rejected
   at 65 runes regardless of encoding.

6. **`discoverable` requires `P` exactly.** An `O` channel with `discoverable: true` is invalid,
   which is easy to miss because discoverability sounds like an open-channel feature.

7. **`ChannelPatch.ManagedCategoryName` is accepted on the wire and then ignored.** `Patch`
   applies the other nine fields and silently drops this one — see D-016. `default_category_name`
   *is* applied, and trimmed.

8. **`Patch` trims `display_name` and `default_category_name` only.** `name`, `header` and
   `purpose` are stored with surrounding whitespace intact.

9. **A non-nil but empty `banner_info` patch still materialises the banner.** Patching with
   `{"banner_info": {}}` turns a nil `banner_info` into `{enabled: null, text: null,
   background_color: null}` — the key stops being `null` on the wire even though no value
   changed.

10. **`PreSave` preserves a non-zero `create_at`** (like `User`, unlike `Team` and `Session`),
    forces `update_at` to equal it, and zeroes `extra_update_at`. It sanitizes `name` and
    `display_name` but **not** `header` or `purpose`.

11. **`IsValidBoard` checks only three things** — type, `team_id`, `display_name`. A board with
    an empty id and a zero `create_at` passes it. It supplements `IsValid`; it does not replace
    it.

12. **`GetOtherUserIdForDM` returns the *first* member for a non-member caller.** It compares
    the caller against `user1` only, so passing an unrelated user id yields `user1` rather than
    an error or an empty string.

13. **`Channel.Type` must stay a `String`.** Go's `ChannelType` is a defined string type, so
    `json.Unmarshal` accepts any value into it. A Rust enum would harden a forward-compatible
    read into a parse failure the moment a newer Go server writes a new type.

## Notes — model/channel_list.go, model.Etag

1. **`delta` is always zero.** Both list `Etag`s declare `var delta int64` and never assign it,
   so the third component of every list etag is a literal `0`. It is not a placeholder we can
   fill in — the Go server emits `0` and clients compare the whole string.

2. **The list `Etag` is not "the newest channel".** Each comparison is against a single running
   `t` that the previous comparison may already have raised, so `t` is the maximum over *all*
   compared fields of *all* channels, and `id` is whichever channel last raised it. For
   `[{a, last_post_at: 300}, {b, update_at: 400}]` the answer is `b.400`, not `a.300` — the id
   and the timestamp can come from different channels.

3. **Comparisons are strictly greater-than**, so ties keep the *earlier* channel, and a list
   whose timestamps are all negative keeps the initial `id = "0"` and `t = 0`.

4. **An empty or nil list yields `<version>.0.0.0.0`** — `id` is initialised to the string
   `"0"`, not to an empty string.

5. **`Etag` escapes nothing.** Parts are joined with `.`, so a part containing a dot silently
   changes the component count. A zero `Team` yields `11.11.0..0`: an empty component is normal.

6. **`CurrentVersion` is a `var`, not a const** (`versions[0]`, version.go:155), and nothing in
   the model package reassigns it. It was transcribed as `utils::CURRENT_VERSION` and guarded by
   `current_version_matches_go`, which reads the value out of the oracle — so bumping the pinned
   SHA to a new release fails a test instead of silently changing every etag in the tree. Since
   `version.go` landed the constant lives in `version::CURRENT_VERSION` and `utils` re-exports
   it; both paths still resolve and both drift tests still run.

7. **Go's `%v` and Rust's `Display` agree** for every type a call site passes: strings,
   integers, and bools (`true`/`false` in both). They would *not* agree for floats, where Go's
   `%v` is `%g`. No call site passes one.

## Notes — model/channel_member.go

1. **`SetChannelMuted` ignores its argument.** It reads `IsChannelMuted()` and writes the
   opposite value, so it is a toggle with a setter's name: `SetChannelMuted(false)` on an
   unmuted channel *mutes* it. Verified across every starting value, both arguments. Ported
   as-is; see D-019.

2. **`SetChannelMuted` panics in Go on a nil `NotifyProps`** — assignment to a nil map. The
   Rust port creates the map instead (D-018). This is the one behavioural divergence in the
   file and it replaces a crash, not a result.

3. **Missing notify-props keys are an error for two of the six.** `desktop` and `mark_unread`
   use `if v, ok := props[k]; ok || !allowMissingFields`, so with the flag off their absence is
   itself a failure. `push`, `email`, `ignore_channel_mentions` and
   `channel_auto_follow_threads` use a plain `ok` and may be omitted freely. `ChannelMember::
   IsValid` passes `false`, so **a member with nil or empty `notify_props` is invalid.**

4. **The email failure's detail says `push_notification_level=`.** A copy-paste bug in Go
   (channel_member.go:171) that clients already parse. Reproduced verbatim.

5. **Three validators reject `"default"` where a fourth accepts it.**
   `IsChannelMarkUnreadLevelValid` takes only `all`/`mention`;
   `IsChannelAutoFollowThreadsValid` only `on`/`off`; `IsSendEmailValid` takes
   `default`/`true`/`false` but **not** the notify levels. Only `IsChannelNotifyLevelValid`
   takes the four-level set.

6. **The length guards fire before the value check and share its error**, so a 21-character
   `desktop` value reports `notify_level=<the whole 21 characters>`. The limits differ per key:
   20, 20, 20, 20, 40, and 3.

7. **`ToJSON` is measured, not transmitted, by the 800,000-rune cap** — and Go's
   `encoding/json` is not serde_json. It sorts keys by byte value, HTML-escapes `<`, `>` and
   `&` into six-rune sequences, and escapes U+2028/U+2029. It *does* use `\b` and `\f`
   shorthand, which matches serde_json. Ported as `utils::go_json_marshal_string_map` and
   pinned byte-for-byte over eleven corpus cases.

8. **`SanitizeForCurrentUser` writes `-1`, not `0`**, to both `last_viewed_at` and
   `last_update_at` for other users' memberships — the same sentinel style as
   `TeamMember::SanitizeRoleData`.

9. **`SetChannelMembersRequest.ChannelAdmins` is `*[]string` and the nil/empty distinction is
   load-bearing.** `null` preserves existing admin roles; `[]` sets them declaratively and
   demotes every current admin. `Option<Vec<String>>` carries it; a plain `Vec` would silently
   turn "preserve" into "demote everyone".

10. **`ChannelUnread`/`ChannelUnreadAt` carry `NotifyProps` with `json:"-"`.** Populated by the
    store for the notification logic; never on the wire.

## Notes — model/custom_status.go, `utils::go_time`

1. **`ExpiresAt` is a `time.Time`, not epoch milliseconds.** It is the first field in the tree
   that is, and it goes on the wire as RFC 3339. It has no `omitempty`, so the zero time
   serialises as `"0001-01-01T00:00:00Z"` — never omitted, never `null`.

2. **chrono's serde impl would drift on four counts**, all measured:
   - Go trims trailing zeros from the fraction (`.5`, `.12`, `.1`, `.01`, `.12345678`); chrono's
     `SecondsFormat::AutoSi` pads to 3/6/9 digits.
   - Go writes `Z` for a zero offset; chrono's `DateTime<FixedOffset>` writes `+00:00`.
   - Go preserves the zone it holds, so `12:00:00+05:30` re-emits as `+05:30` and **not** as
     `06:30:00Z`. That is why the field is `DateTime<FixedOffset>`, not `DateTime<Utc>`.
   - Go's `UnmarshalJSON` ignores `null`, leaving the receiver untouched; serde rejects it.

3. **Go's RFC 3339 parse is stricter than RFC 3339.** `T` and `Z` must be **uppercase** —
   `2026-08-14t12:00:00z` is rejected. `+0530` is rejected but `+23:59` is accepted and `+99:99`
   is not. `+00:00` and `-00:00` both collapse to UTC and re-marshal as `Z`. A signed year
   (`-026-…`) is rejected, as is the leap second `23:59:60` and `2023-02-29`.

4. **More than nine fractional digits is accepted and truncated, not rounded**:
   `.1234567891` becomes `.123456789`. An all-zero fraction (`.000000000`) parses and then
   marshals with no fraction at all.

5. **Marshalling fails outside year `[0, 9999]`.** Go returns
   `"Time.MarshalJSON: year outside of range [0,9999]"`, which is what makes `Contains` and
   `Remove` fallible at all — they marshal before doing anything else, so even a status the
   emptiness guard would reject can produce that error. The check order is load-bearing.

6. **`{duration: "", expires_at: zero}` is valid; `{duration: "date_and_time", expires_at:
   zero}` is not.** An absent duration is special-cased by the first branch of
   `AreDurationAndExpirationTimeValid`; a *named* duration always demands a future expiry, and
   the zero time is in the past. `PreSave` respects the same rule and leaves the empty duration
   alone rather than promoting it.

7. **`Contains` and `Remove` compare marshalled bytes; `Add` dedups on `Text` alone.** So
   adding `{emoji: "z", text: "three"}` to a list holding `{emoji: "c", text: "three"}`
   *replaces* it, while `Contains` on the same pair is false. Two statuses differing only in
   `expires_at` are likewise "not contained" but still collide in `Add`.

8. **An empty status can be added but never removed.** Both `Contains` and `Remove` early-return
   when `Emoji == "" && Text == ""`; `Add` has no such guard and prepends it like any other.

9. **`Add` caps at 5, `Remove` does not.** A list that is already over the cap stays over it
   after a removal.

10. **`PreSave` truncates by runes, which can split a grapheme.** 101 base-plus-combining pairs
    are 202 runes and come back as 100 runes ending on a bare combining mark — 150 bytes from
    303.

11. **`RuneToHexadecimalString` pads to four digits but never truncates**, so `U+1F600` renders
    as five (`1f600`). Go's parameter is an `int32` that can be negative, where `%04x` would
    emit a sign; a Rust `char` cannot be, and no call site passes one.

## Notes — model/post_embed.go, model/post_acknowledgement.go

Both are leaves under `post_metadata.go`, which is a leaf under `post.go`. `post.go` is **not**
the next file after `file_info.go`: `Post.Metadata` is a `*PostMetadata`, and `PostMetadata`
needs `PostEmbed` and `PostAcknowledgement` first.

1. **`omitempty` on a Go `any` tests `IsNil()`, not emptiness.** `PostEmbed.Data` therefore
   *emits* `""`, `0`, `false`, `{}` and `[]` — only a nil interface is dropped. Every intuition
   about `omitempty` from the string and int fields is wrong here.

2. **`Data` has three output states, not two.** Nil interface → key omitted. Typed nil pointer
   stored in the interface → `"data":null`, because the interface itself is not nil. Anything
   else → the value. `Option<Value>` covers all three.

3. **That round trip is lossy in Go too.** An explicit `data: null` decodes to a nil interface,
   so re-marshalling drops the key — Go loses it exactly as we do. The oracle records Go's own
   unmarshal→marshal result next to its output, and the Rust test asserts against *that*;
   asserting against the original bytes would have meant diverging from Go to look "correct".

4. **`PostEmbedType` is a defined string type**, so an unknown value round-trips unchanged. Kept
   as `String` for the same reason `Channel.Type` is.

5. **`PostAcknowledgement.RemoteId` is the only ported `remote_id` with `omitempty`.**
   `Reaction.RemoteId` and `FileInfo.RemoteId` are the same `*string` under the same JSON name
   and write `null` when nil; this one disappears. Pinned by a test that serialises all three
   zero values side by side.

6. **`PostAcknowledgement.PreSave` does not materialise `remote_id`**, unlike
   `Reaction::pre_save` and `FileInfo::pre_save`. A nil stays nil and therefore stays off the
   wire.

7. **`acknowledged_at` is never validated** — zero and negative both pass `IsValid`. That
   matters because `PreSave` fills it only when it is exactly zero, so a negative timestamp
   survives both.

8. **The error id says `model.acknowledgement.…`, not `model.post_acknowledgement.…`** — the
   type name and the error namespace disagree.

## Notes — model/file_info.go

1. **`MiniPreview` is a `*[]byte`, and Go's `encoding/json` base64-encodes `[]byte`.** serde_json
   would emit `[1,2,3]` where Go emits `"AQID"`. Ported with a custom codec (`go_bytes`) and
   pinned byte-for-byte. This is what justified the `base64` dependency.

2. **Three nil-ish states collapse to two on the wire.** A nil pointer and a pointer to a *nil
   slice* both marshal as `null`; only a pointer to an **empty** slice marshals as `""`. So
   `Option<Vec<u8>>` loses nothing Go could express.

3. **`IsValid` requires a non-empty `Path`, and `Path` carries `json:"-"`.** A `FileInfo`
   decoded from a client request is therefore **always invalid**. This reads as a port bug and
   is not; there is a test asserting the round trip produces exactly that failure.

4. **Four fields never reach a client** — `path`, `thumbnail_path`, `preview_path`, `content`.
   `content` is extracted document text, so a leak would be a real one.

5. **The JSON key for `CreatorId` is `user_id`.** The Go field name and the wire name disagree,
   which is easy to miss when eyeballing a struct.

6. **`creator_id` accepts two magic strings** besides a real id: `nouser` and
   `BookmarkFileOwner` (`"bookmark"`, borrowed from channel_bookmark.go). Case-sensitive —
   `NoUser` fails. `channel_id` and `delete_at` are never checked at all.

7. **The filename limit is codepoints**, via `utf8.RuneCountInString` — 256 two-byte characters
   is a valid name. `"..."` is valid; only the bare `"."` and `".."` are rejected.

8. **`SanitizeFilename` NFC-normalizes before truncating, and that is load-bearing.** 200
   decomposed `é` (`e` + combining acute) are 400 codepoints going in and 200 coming out, so a
   port without normalization truncates a different string and stores a different name. This is
   what justified the `unicode-normalization` dependency — measured, not assumed.

9. **Sanitizing is not validating.** `""`, `"."`, `".."`, `"/"` and an all-control-character
   input all sanitize to `""`, which `IsValidFilename` then rejects. Go's own doc comment says
   callers must treat an empty result as failure.

10. **`IsImage` tests the prefix `"image"`, not `"image/"`** — so `"images/png"` and `"imagex"`
    are images. `IsSvg` is exact equality, so `"image/svg+xml; charset=utf-8"` is **not** an SVG.

11. **`PreSave` is the gentlest in the tree** — every step is conditional. `update_at` is raised
    to `create_at` only when it is *behind*; one already ahead is left alone, and nothing reads
    the clock for it (unlike `Reaction::pre_save`).

12. **`GetEtagForFileInfos` pairs `infos[0].post_id` with the max `update_at` over the whole
    list**, so the two halves can come from different elements. Same trap as the channel-list
    etags. An empty list yields a bare `Etag()` — version only, no components.

13. **`Path::extension` is not `filepath.Ext`.** Rust treats a leading dot as a stem, so
    `".hidden"` has no extension; Go scans back to the last dot and returns `"hidden"`. Caught
    by the oracle on the first run — ported as `go_filepath_ext`.

14. **`NewInfo`'s mime lookup is not portable.** `mime.TypeByExtension` reads the host's
    `mime.types` files: this host answered `text/plain; charset=utf-8` for `.txt` and
    `video/mp4` for `.mp4`, neither of which is in Go's builtin table. The mime type is a
    parameter in the Rust port and the database decision is deferred — see [D-030].

## Notes — model/reaction.go

1. **Reacting with an emoji is not the same as creating one.** `Reaction.IsValid` checks the
   name against a pattern and the 64-byte limit but **never** against the system-emoji table, so
   `grinning` is a legal reaction and an illegal custom emoji. Two validators share
   `EmojiNameMaxLength` and diverge on everything else.

2. **Go compiles its own emoji-name regex inline** (reaction.go:31) rather than calling
   `IsValidAlphaNumHyphenUnderscorePlus`, and writes the character class differently:
   `^[a-zA-Z0-9\-\+_]+$` against utils.go's `^[a-zA-Z0-9+_-]+$`. They *look* equivalent, and
   the oracle runs both over 32 inputs — including `a-z`, `-`, `]`, `^` and `$`, which probe
   whether either reads the hyphen as a range — to establish it. The Rust port reuses the shared
   validator on that evidence; if upstream ever changes one pattern, the test fails.

3. **`channel_id` and `delete_at` are not validated at all.** `channel_id: "nope"` passes, an
   empty one passes, and an already-deleted reaction is valid.

4. **The two timestamp failures carry no `detailed_error`**, while the three checks before them
   do. Asymmetric like `Emoji`'s, and equally wire surface.

5. **`PreSave` reads the clock twice.** `create_at` is filled from one `GetMillis()` only when
   zero, then `update_at` from a *separate* call — so a brand-new reaction can have `update_at`
   a millisecond ahead of `create_at`. `Emoji::pre_save` copies one into the other instead.
   Reproduced as two calls.

6. **`PreSave` zeroes `delete_at`; `PreUpdate` does not.** Saving a deleted reaction undeletes
   it; updating one keeps it deleted.

7. **`remote_id` is `*string` with no `omitempty`**, so the key is always present and nil
   serialises as `null`. Both pre-hooks materialise nil to `Some("")`, so a nil only survives on
   a reaction that has been through neither. `GetRemoteID` collapses nil and empty to `""`, so
   it cannot tell "never set" from "explicitly local".

## Notes — model/emoji.go

1. **The system-emoji table is generated, not transcribed.** `model.SystemEmojis` is 4,464
   entries in a 4,473-line `emoji_data.go`. `reference/dump` emits it to
   `crates/mm-model/src/emoji_generated.rs`, sorted by name for binary search. Getting it wrong
   is not cosmetic: a missing entry lets a user create a custom emoji the Go server refuses, and
   one the Go server would then shadow.

2. **Ordinary-looking names are already taken.** `a`, `+1`, `100` and `mattermost` are all system
   emoji names, so `IsValidEmojiName` rejects them with a *different* error id
   (`model.emoji.system_emoji_name.app_error`) from the pattern failure
   (`model.emoji.name.app_error`). Clients distinguish the two.

3. **Not every table value is a code-point sequence.** `mattermost` maps to the literal string
   `mattermost`. And Go's map index cannot distinguish a miss from an empty value, which is why
   `GetSystemEmojiId` returns a bool — ported as `Option`.

4. **`GetEmojiNameFromUnicode` returns the alphabetically first of several names**, plus how many
   share the sequence. `1f1e8-1f1e6` has three. Lookup is case-sensitive: `1F600` misses.

5. **`IsValid` ignores `delete_at` entirely and never validates `creator_id`** — just
   `len(...) > 26`, in bytes, so `"nope"` is an acceptable creator and empty is fine. Same shape
   as `Channel.CreatorId`. The `id` and `creator_id` failures carry **no detail at all** while
   the two timestamp failures carry `id=`; the asymmetry is wire surface.

6. **`EmojiPattern` is a scanner, not a matcher** — unanchored. `::::` finds nothing (one
   character minimum between colons), and overlapping references share their delimiter, so
   `:a:b:c:` yields `:a:` and `:c:`: the middle name is swallowed because the leftmost match
   consumed its opening colon.

7. **`PreSave` overwrites `create_at` unconditionally** (like `Team` and `Session`, unlike `User`
   and `Channel`), copies it to `update_at`, and mints an id only when absent.

8. **`Auditable()` has an upstream copy-paste bug**: it reports `"delete_at": emoji.CreateAt`
   (emoji.go:34). Not ported — see [D-028] — but recorded so nobody "fixes" it on the way in.

## Notes — `strings.ToLower` is not `str::to_lowercase`

Found while porting `Emoji::PreSave`, but it was a **pre-existing divergence in six already-
shipped call sites**, not an emoji problem.

Go applies Unicode's *simple* (1:1) lowercase mapping per rune. Rust's `str::to_lowercase`
applies the *full* (1:many) mapping and implements the Final_Sigma context rule. Measured over 30
inputs, they disagree twice:

| input | Go | `str::to_lowercase` |
|---|---|---|
| `İ` (U+0130) | `i` | `i` + U+0307 |
| `ΟΔΟΣ` | `οδοσ` | `οδος` |

`utils::go_to_lower` takes the first character of Rust's full mapping, which reproduces the
simple mapping; the character-level API has no context, so Final_Sigma cannot apply. All six
call sites now use it — `is_valid_email`'s `isLower` check, `normalize_username`,
`normalize_email`, the mention-key lowercasing in `User::pre_update`, `is_reserved_team_name`
and `clean_team_name`. A team slug or an emoji name that lowercases differently in the two
servers is a divergence on a shared database, so this was not theoretical.

## Notes — model/preference.go

1. **`IsValid` mixes bytes and runes, four lines apart.** `Category` and `Name` use `len()` —
   bytes — with a 32 limit; `Value` uses `utf8.RuneCountInString` with a 20,000 limit. So 32
   two-byte characters is an invalid *category* (64 bytes) but 20,000 of them is a perfectly
   valid *value* (40,000 bytes). Both boundaries are pinned in both units.

2. **An empty `Name` is valid; an empty `Category` is not.** Only the category has a
   non-emptiness check.

3. **The theme check uses `json.Decoder.Decode`, not `json.Unmarshal`.** A `Decoder` reads the
   *first* JSON value and never looks for EOF, so `{"a":"b"} garbage` is **valid**.
   `serde_json::from_str` rejects trailing content, so it is the wrong tool — `decode_theme`
   drives a `Deserializer` directly and deliberately never calls `end()`.

4. **`null` is a valid theme value, and so is `{"a":null}`.** Go zeroes a map destination on
   JSON null without error, and ignores a null when the destination is a primitive — leaving the
   key present with `""`. `{"a":1}` is an error, `{"a":null}` is not.

5. **`IsValid` reads the decode's *error*; `PreUpdate` ignores it and reads the *value*** — and
   Go's decoder produces both at once. A type error is recorded **and** the key is still
   inserted holding the zero value, which is why `{"a":"#abc","b":1,"c":"#def"}` comes out of
   `PreUpdate` as `{"a":"#abc","b":"#ffffff","c":"#def"}`: `b` was stored as `""`, then failed
   the colour regex. `decode_theme` returns the map and the error flag together for this reason.

6. **An undecodable theme becomes the literal string `"null"`.** `PreUpdate` ignores the decode
   error, so `props` stays nil, and `json.Marshal` of a nil Go map is `null` — which is written
   straight back into `Value`. Reachable from `garbage`, `""`, `[]` and `null` alike.

7. **`PreUpdate` is a normaliser, not just a sanitiser.** The re-marshal sorts keys and applies
   Go's HTML escaping, so `{"z":"#abc","a<b":"#def"}` becomes
   `{"a\u003cb":"#def","z":"#abc"}`. Anything after the first JSON value is dropped.

8. **Only `image`, `type` and `codeTheme` are exempt** from the colour check. Every other value
   must match `^#[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$` — three *or* six hex digits — or it is
   replaced with `#ffffff`. An empty string is not a colour and is replaced too.

9. **Go's RE2 and Rust's `regex` agree on `$`.** Neither matches before a trailing newline
   without the `m` flag, unlike Perl. Pinned, because a Perl-minded reading would accept
   `#abc\n`.

10. **The DM/GM limit is checked only under its own category *and* name.** `999` is valid under
    `display_settings`/`limit_visible_dms_gms` and under
    `sidebar_settings`/`show_unread_section`; only the exact pair is range-checked.

11. **`strconv.Atoi`'s error is checked here**, unlike `SplitVersion` in version.go which
    discards it. So an overflowing limit is *invalid* rather than saturating to `i64::MAX`. `+5`
    is accepted (Atoi takes a leading sign), `05` is accepted, and ` 5`, `5.0`, `0x5` and `1_0`
    are not.

## Notes — model/status.go

1. **`dnd_end_time` is in SECONDS.** Every other timestamp in the model package is epoch
   milliseconds; Go documents the exception in a comment (status.go:32-33) rather than in the
   type, and both fields are plain `int64`. Nothing in either language catches a caller that
   mixes them up.

2. **`active_channel` is on the wire in the struct and off it in practice.** It carries a `json:`
   tag *and* `omitempty` *and* `db:"-"`, and both `ToJSON` and `StatusListToJSON` blank it on a
   **copy** before marshalling — so the key is dropped by `omitempty`, and the receiver keeps its
   value. Serialising a `Status` with serde directly is therefore not equivalent to `to_json`;
   it leaks the field. Verified both halves against Go.

3. **`StatusListToJSON` never emits `null`.** It builds `make([]Status, len(u))`, which is
   empty-but-non-nil even for a nil input, so an absent list is `[]`. A port that handed a nil
   Go slice to the encoder would write `null` and break any client that indexes the result.

4. **`StatusMapToInterfaceMap` keys its result by `s.UserId`, not by the map key it read.** The
   two agree at every call site, which is exactly why picking the wrong one would go unnoticed.
   Pinned with a case where they deliberately differ.

5. **Only the exact string `offline` is filtered** by that function. An *empty* status survives
   and is emitted as `""` — "omitted means offline" is a convention about the output, not a
   normalisation of the input.

6. **`StatusCacheSize` is `SessionCacheSize`**, not an independent 35000. Aliased to
   `session::SESSION_CACHE_SIZE` so the two cannot drift apart in Rust the way they could in Go.

7. **`DNDExpiryInterval` is a `time.Duration`** — an `int64` of nanoseconds, and the only
   nanosecond quantity in the model package. The oracle records `60000000000`.

8. **The struct carries `xml:` tags too.** Nothing in the migration targets an XML encoder, so
   they are not reproduced. If one ever appears, note its names are the Go field names
   (`UserId`, `DNDEndTime`), not the snake_case JSON ones.

## Notes — model/user.go's custom-status accessors

1. **There are five, not four.** `GetCustomStatus` (user.go:791) and `CustomStatus` (user.go:799)
   are byte-identical duplicates in the Go source. Both are ported.

2. **`GetCustomStatus` discards the unmarshal error**, so it returns a non-nil status far more
   often than it looks. `{}`, `{"emoji":"a"}`, and even `"a string"`, `0`, `true` and `[]` all
   come back non-nil — the decoder allocates the pointer before it discovers the value is not an
   object. Only an absent key, `""`, the literal `null`, and *syntax* errors give nil. A type
   error keeps whatever decoded before it; see [D-026] for the one shape we do not reproduce.

3. **Missing keys must zero-fill.** Go's `encoding/json` leaves an absent field at its zero
   value, so `{}` and `{"emoji":"a"}` are both valid inbound custom statuses. `CustomStatus`
   needs `#[serde(default)]` for that — without it serde rejects a partial object the Go server
   accepts, which any client sending less than the full shape would hit.

4. **`ValidateCustomStatus` reduces to a much narrower test than a full decode**: the prop must
   be syntactically valid JSON that is not `null`. Ported against that predicate rather than
   against `get_custom_status`, so the [D-026] divergence cannot leak into `User::is_valid`.

5. **`ClearCustomStatus` writes `""`, it does not remove the key.** A cleared status therefore
   still has the prop present, and `ValidateCustomStatus` returns true for it.

6. **`SetCustomStatus` stores marshalled bytes, so escaping is wire surface.** Go writes
   `{"emoji":"\u003cb\u003e",...}` into `Users.Props`; serde_json would write `<b>`. Fixed by
   routing through `utils::go_json_marshal` — see [D-022], closed.

7. **`SetCustomStatus(nil)` is not an error and not a no-op** — Go marshals the pointer, so it
   stores the four bytes `null`. Unrepresentable with a `&CustomStatus`; the oracle records it.

## Notes — model/version.go

1. **`SplitVersion` returns a saturated bound, not 0, on numeric overflow.** It discards every
   `strconv.ParseInt` error, and Go returns `(MaxInt64, ErrRange)` for too-large input — so
   discarding the error keeps `9223372036854775807`. `SplitVersion("99999999999999999999.0.0")`
   is `(9223372036854775807, 0, 0)`. A `parse::<i64>().unwrap_or(0)` port answers `0`. Ported as
   `parse_int64_go` and pinned over 52 corpus cases.

2. **Overflow beats syntax, left to right.** `"99999999999999999999abc"` is `MaxInt64` in both
   languages, because the overflow is detected at digit 20 before the `a` is reached — but
   `"abc99999999999999999999"` is `0`. Go and Rust agree because both scan left to right and
   return at the first problem. Measured, not assumed.

3. **`ParseInt` with an explicit base 10 rejects `_` separators**, unlike base 0. `"1_000"` is
   `0`, and so are `0x10`, `0b1`, `1e3`, `" 1"`, `"1 "` and Arabic-Indic digits. `+`/`-` signs
   are accepted by both. Only `nil`-safe atoms differ, and none do.

4. **Nothing ever looks at the patch component.** All three lookup functions reduce their input
   to `fmt.Sprintf("%v.%v.0", major, minor)` first, so `11.11.5`, `11.11` and `11.11.0.1` are
   the same query as `11.11.0`, and the five hotfix entries in `versions` (`4.8.1`, `4.7.2`,
   `4.7.1`, `1.2.1`, `0.7.1`) are unreachable through the public API.

5. **A hotfix claims the dedup slot, and its predecessor is the base release's.**
   `versionsWithoutHotFixes` keeps the *first* entry mapping to a given `major.minor.0`, so
   `4.8.1` becomes the `"4.8.0"` entry and the later real `4.8.0` is dropped.
   `GetPreviousVersion("4.7.2")` is `"4.6.0"`, not `"4.7.1"`. 137 releases collapse to 132.

6. **`GetPreviousVersion` cannot distinguish "unknown" from "oldest".** Both return `""` —
   `"garbage"` and `"0.5.0"` are indistinguishable to a caller.

7. **`IsPreviousVersionsSupported` indexes `[0]`..`[3]` unchecked** and would panic on a table
   with fewer than four entries. Ported as `take(4).any(…)`, which is the same answer without
   the edge; the entries are distinct so at most one can ever match.

8. **The `Build*` vars are `-ldflags` injection points**, empty by default. Rust has no
   link-time string injection, so they read `MM_BUILD_*` at compile time via `option_env!` and
   fall back to `""`. The variable names are ours — the Go build has no equivalent — and a test
   pins the default to Go's zero value so nothing leaks into a build-info response.

## Notes — model/session.go, model/team_member.go

1. **`strconv.ParseBool` is not `str::parse::<bool>()`.** Go accepts `1 t T TRUE true True`
   and `0 f F FALSE false False`; Rust accepts only `true`/`false`. `Session::is_mobile`,
   `is_saml` and `is_oauth_user` all go through it, and session props are written by several
   code paths, so the wider set is reachable. Ported as `parse_go_bool` and corpus-verified.

2. **The bool props are not consistent with each other.** `IsMobile`/`IsSaml`/`IsOAuthUser` use
   `ParseBool`, but `IsBotUser` and `IsGuest` use exact `== "true"`. So a prop of `"1"` makes a
   session mobile but **not** a bot. Faithful to Go; do not unify them.

3. **`Session.IsOAuth` (struct field) and `Session.IsOAuthUser()` (prop) are different things.**
   `IsIntegration` reads the field; `IsSSOLogin` reads the prop. Easy to conflate.

4. **`IsExpired` treats a non-positive `ExpiresAt` as "never expires"**, and compares strictly
   greater-than, so a session is not expired at the exact millisecond of its expiry.

5. **`PreSave` overwrites `CreateAt` unconditionally and never sets `ExpiresAt`** — expiry is
   the caller's job. Same `CreateAt` behaviour as `Team`, opposite of `User`.

6. **`Sanitize` strips only the token.** `props` survives, CSRF value included.

7. **`IsValidDeviceId` strips a terminal `-v<N>` suffix**, and Go's `Atoi` accepts a leading
   `+`, so `apple_rn-v+2:token` is valid. Rust's `parse::<i64>()` agrees. A negative `N` is not
   stripped. The colon split takes the **first** colon, so `apple_rn:tok:en` is valid.

8. **`TeamMember.CreateAt` carries `json:"-"`** — persisted, never on the wire.

9. **`TeamMember::SanitizeRoleData` sets `DeleteAt` to `-1`**, not 0, for other users. That
   sentinel reaches the client.
