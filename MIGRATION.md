# Migration Ledger

Go source pinned at: mattermost@9dfbaeca99f4096388fd1c048a9e6d1d0a86743e (2026-08-13)
Current phase: 1 — Core Types
Next file: server/public/model/team.go

Re-clone the reference source with:
`git clone --depth 1 https://github.com/mattermost/mattermost.git reference/mattermost`
then `git -C reference/mattermost checkout 9dfbaeca99f4096388fd1c048a9e6d1d0a86743e`

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
| model/utils.go (IsValidEmail) | `mm-model/src/utils.rs` | DONE | 2,916 cases | Corpus-verified against Go: 128 hand-picked + 2,788 generated. Grammar is `dot-atom @ (dot-atom / [ip])`. |
| model/user.go | `mm-model/src/user.rs` | PARTIAL | 48 pass | Wire type + self-contained logic. Deferred: `IsValid` and `PreSave` (need `IsValidEmail`/`IsValidLocale` parser ports + CustomStatus + timezone defaults), custom-status accessors, `IsValidUserRoles`, `Etag`, `CleanUsername`, `GetTimezoneLocation`. `pre_save_partial` is named so deliberately — it does NOT hash passwords. |
| model/utils.go | `mm-model/src/utils.rs` | PARTIAL | 54 pass | See notes below. Deferred: `IsValidHTTPURL` (needs an RFC 3986 parser), `ParseHashtags` (goes with post.go), `Scan`/`Value` (go to mm-store), the io.Reader JSON helpers (serde replaces them), `Etag`/`NewRandomTeamName` (need consts from other files). |
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
