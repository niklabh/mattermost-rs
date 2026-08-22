# Migration Ledger

Go source pinned at: mattermost@9dfbaeca99f4096388fd1c048a9e6d1d0a86743e (2026-08-13)
Current phase: 1 — Core Types (with a phase 2-4 vertical slice landed, see below)

## The vertical slice — the architecture is proven end to end (2026-08-17)

**A client cannot tell which server answered.** `GET /api/v4/users/me` is served by Rust,
authenticated against a session row the **Go** server wrote, and its response is byte-identical
to Go's — 721 bytes, same fields, same key order, same trailing newline. Everything else is
forwarded to Go and comes back unaltered.

This was done ahead of finishing phase 1 deliberately. 40k lines of model code had never served a
byte, and `MIGRATION_STRATEGY.md:69` predicted exactly that risk ("months of unverifiable work").
Three assumptions had never been tested even once; all three now hold:

| Assumption | Verdict |
|---|---|
| sqlx can read the schema the Go server migrates | Yes — and the compile-time checker caught a v11-only `NOT NULL` on `Sessions.VoipDeviceId` at build time |
| A Go-minted token authenticates against the shared `Sessions` table | Yes — cookie, `Bearer` and `?access_token=` all work |
| A real client accepts our serialisation | Yes — byte-identical, after one 1-byte fix ([D-086]) |

**Run it** — `docker compose up -d`, then `cargo run -p mm-api`. The Rust server listens on
:8066 and forwards to the Go server on :8065; both share one Postgres. See the README.

**The finding that matters most is [D-087], and it is now decided.** Go answers `/users/me` from
an in-memory user cache that a login does not invalidate, so it serves an `update_at` **6.3
seconds stale and not converging** while we return the row's actual value. We are the correct
one, which is the uncomfortable part: the divergence cannot be closed by matching Go.

**Decision — Rust reads through, and stale-on-write is accepted:**

1. The Rust side **never caches**; every read goes through to Postgres.
2. Read routes migrate freely; we are never staler than Go.
3. Write routes migrate freely as well. A write we make is invisible to Go until its cache entry
   expires — **staleness, not corruption**. Port writes when convenient and accept the window.

Both alternatives are licensed away, measured rather than assumed. The cluster bus is
`einterfaces.ClusterInterface`, implemented only in the out-of-scope `enterprise/` tree. Redis
cache mode — which would have been elegant, since invalidation needs only a key name and never
the value encoding — makes the server connect to Redis, log `PONG`, and then refuse to boot:
*"Redis cannot be used in an instance without a license or a license without clustering."* On
Team Edition there is **no invalidation channel a second process can reach**; do not go looking
for one again.

An earlier version of this entry made writes wait on cache coherence. That was over-engineering
for a project with no users: it turned a bounded staleness window into a block on development.
Revisit per-entity when there are real users — sessions and permissions are where a stale Go
read would actually matter, not everything.

This also settles the `*_serial_gen.go` question: those 2,280 generated lines are the **msgpack
codecs for Go's cache**, and since we never populate that cache, they are confirmed out of scope.

Two beliefs held before this session were wrong and are now measured:

1. **`User::Sanitize(map[string]bool{})` strips nothing extra.** Go guards its whole flag block
   behind `if len(options) != 0` (user.go:702), so the *empty* map — which is exactly what
   `/users/me` passes — keeps email, full name and auth service. The intuitive reading ("no
   options means no permissions means strip everything") is exactly inverted, and acting on it
   would have blanked the email of every user viewing their own profile. `mm-model`'s port had
   this right already; a comment and a test written from the intuition did not.
2. **A session **id** must not authenticate.** `SessionStore.Get` matches `Token = $1 OR Id = $1`,
   so the row comes back either way, and only `session.Token != token` (session.go:95) rejects
   it. Dropping that line silently turns session ids — which appear in admin APIs and logs — into
   bearer credentials. Verified against both servers: 401 from each.

**Phase 2 is licence-unblocked as of 2026-08-17.** [D-031] is closed: the repository now carries a
split licence — AGPL-3.0-only at the root, Apache-2.0 on `mm-model` alone. The first `mm-store`
commit no longer trips anything. The **only** new standing rule is directional: an AGPL crate may
depend on `mm-model`, and `mm-model` may never depend on an AGPL crate or read
`server/channels/`. That was already the layering rule; it is now also a licensing one.

**Phase 2 is licence-unblocked as of 2026-08-17.** [D-031] is closed: the repository now carries a
split licence — AGPL-3.0-only at the root, Apache-2.0 on `mm-model` alone. The first `mm-store`
commit no longer trips anything. The **only** new standing rule is directional: an AGPL crate may
depend on `mm-model`, and `mm-model` may never depend on an AGPL crate or read
`server/channels/`. That was already the layering rule; it is now also a licensing one.
`channel_count.go` **does not exist** — checked 2026-08-17. The earlier note guessed at the name;
do not look for it again.

## The write path is complete — [D-108] and [D-002] are closed (2026-08-17)

`User::pre_save` landed. **`pre_save_partial` no longer exists**; the crate's one genuinely
dangerous function is gone rather than renamed around.

**The entry's premise was wrong, and that is the finding.** [D-108] said "Go uses
`golang.org/x/crypto/bcrypt`". At the pinned SHA bcrypt is the **legacy** hasher:
`hashers.latestHasher` is `DefaultPBKDF2()`, and the only caller of `User.PreSave` in the tree —
`channels/store/sqlstore/user_store.go:180` — passes `hashers.GetLatestHasher()`. So every password
the Go server writes into the shared `Users.Password` column is a PBKDF2 PHC string,
`$pbkdf2$f=SHA256,w=600000,l=32$<salt>$<hash>`. Doing literally what the entry asked would have had
the Rust server writing the superseded format into a column Go is migrating away from.

Both are ported. `Pbkdf2` is the default; `BCrypt` is owed regardless, because
`GetHasherFromPHCString` still routes old rows to it and they still have to verify.

**The parity is exact, not structural.** Both algorithms are deterministic given their salt, and
both stored formats carry it — so the tests decode the salt out of a hash **Go produced**,
recompute, and assert the whole Go string byte-for-byte. "Rust emits Go's bytes", not "Rust can
read Go's". The fixture's hashes are literals (both hashers salt randomly, so calling them per run
would rewrite the fixture — [D-032]'s defect), and the generator re-verifies every literal against
Go before writing, so a mistyped character fails the generator rather than a downstream test.

Three things the corpus produced rather than confirmed:

1. **bcrypt truncates at 72 bytes and the two Go layers disagree about it.** `x/crypto`'s
   `CompareHashAndPassword` **accepts** a 73-byte password against a hash of its first 72; the
   `hashers` package puts the length check back and rejects it. A port reproducing the crate
   rather than the package would authenticate a login the Go server denies. Found by the
   generator's own negative control failing.
2. **The 72-byte cap is bcrypt's**, and PBKDF2 — which has no such constraint — inherited it only
   because the package applies one rule to every hasher.
3. **`PreSave`'s timezone guard is `== nil`, not `len() == 0`**, while `NotifyProps` three lines
   above *is* `len() == 0`. An empty-but-present timezone map stays empty.

Where the code lives is a licensing consequence, not a preference: the hashers derive from
`server/channels/` and so live in **`mm-app`**, while the `UserPasswordHasher` trait is in
`mm-model` because Go declares it in `server/public` ([D-031]). Go draws the line in the same
place, which is why `PreSave` takes the hasher as a parameter at all.

**[D-109] closed 2026-08-18** — the verification half landed too, so the password surface is
complete in both directions. A row the Go server wrote verifies here, and a row we write verifies
there. Still owed: [D-110] (FIPS) and [D-112] (the oracle now builds against the AGPL tree).

**The methodological finding, which outlives the file.** The byte-vs-rune question in `MaxRunes`
took three attempts, and the first two were **green**. Draft one asserted a hand-picked "decisive"
input that decided nothing; a *mutation* — switching the port to count runes — passed the whole
suite and exposed it. Draft two concluded the two rules were indistinguishable; also wrong, because
a cut landing inside a multi-byte character separates them. So: **when a test passes on its first
run, mutate the thing it claims to measure.** The oracle protects against misreading Go; it does
not protect against a test that asserts something Go and the port agree on for the wrong reason.

### After that

**`authorize.go` landed 2026-08-18** and the OAuth code-exchange path is now ported end to end in
`mm-model`: `oauth.go`, `oauth_dcr.go` and `authorize.go` together. What is still missing to serve
a real authorization flow is the handler and the store, not the model.

**`job.go` and `timeutils/time.go` landed 2026-08-18.** The mutation pass found a real hole this
time rather than confirming the work: replacing `format_offset`'s `Z` branch with `+00:00` passed
the whole suite **twice**. First because the generator pins a +05:30 zone, so no corpus case had a
zero offset — and UTC is the common deployment, so the untested branch was the one most servers
take. Then, after adding a UTC section to the oracle, it *still* passed, because the new test
reassembled the string from the private helpers instead of calling `format_millis`. Fixed by
extracting `format_millis_in(millis, tz)` so both parity tests drive the production path; the
mutation now fails, along with three others aimed at the same function.

**The transferable part: a parity test that rebuilds the expected value from the same private
helpers the implementation uses is testing the fixture, not the code.** Call the public entry
point, and give it a seam for the ambient input rather than reimplementing around it.

**`view.go` landed 2026-08-18**, and the mutation discipline held: eight mutations against the
finished `view.rs`, eight caught, plus two no-op controls that passed — so the harness is
discriminating rather than merely noisy. Two of the eight targeted the `Index` param that
`AppError` keeps **unexported**, and both were caught only because the oracle reads it with
`reflect` + `unsafe`. Without that read, a port using the wrong column index would have shipped.

**Nine mutations, nine caught.** After the `phcparser` lesson — two green drafts of a claim that was
wrong — the finished `authorize.rs` was mutated deliberately: widening each PKCE charset, dropping
`VerifyPKCE`'s no-PKCE early return, "fixing" the copy-pasted `Where`, widening the expiry multiply
to `i64` before multiplying, changing `expires_in == 0` to `<= 0`, padding the base64url challenge,
tightening the public-client branch, and testing the fragment by substring instead of by parse.
Every one failed the suite. That is the evidence the corpus bites; a first-run pass is not.

**`permission.go` and `role.go` both landed 2026-08-19**, each split the same way: the data
generated out of Go, the logic hand-ported against a corpus. That is the whole model layer of
[D-094] — every permission, every default role, and the functions that combine them.

**What the permission system still needs, in order:**

1. ~~`model/scheme.go`~~ — **landed 2026-08-19**. The model layer of [D-094] is complete:
   every permission, every default role, the scheme that binds them, and the functions that
   combine them.
2. **The `Roles` and `Schemes` stores** in `mm-store` — reads only, to begin with. This is the next
   piece, and it is the first `mm-store` work since the vertical slice, so expect the session to be
   as much about the store's shape as about the two tables.
3. ~~**The checker itself**~~ — **done 2026-08-21.** `authorization.go` is 20 of 36 functions:
   every system-, team-, channel-, user- and post-scoped check, in both the session-scoped and
   `askingUserId` forms. The 674 api4 call sites now reach a ported check unless they name a
   group, a sidebar category, a bot or a property field ([D-134]). [D-094] is CLOSED.

**More `mm-model` files** — 125 in scope, if a break from the permission seam is wanted. By logic
density: `auditconv.go` (776 lines, 62 funcs), `access_policy.go` (755/18), `property_field.go`
(735/27).

[D-094] is closed, but the distinction it records still decides whether a route is portable
*without* a check that is still missing, and it remains the useful part:

- **Escapable** — the check guards something that cannot act on this route.
  `/users/me/teams/members` gates `SanitizeRoleData`, which is a no-op for one's own membership.
  Portable; migrated.
- **Not escapable** — the check decides the response. `/users/me/teams` gates `SanitizeTeam`,
  which strips `email` and `invite_id` per permission, with no self-shortcut. Serving it without
  the check leaks an invite id, which is enough to join the team. Forwarded, with a test keeping
  it forwarded.

### If porting model files instead

The sub-20-line set is nearly exhausted (22 files left under 20 lines, 141 in scope overall) and
its yield has been falling. The old guidance still applies when picking one: prefer a small file
**with a method** — the last four tech-debt entries came from a method (`Audits::Etag`), a tag
convention (`limits.go`), a pointer rule (`channel_search.go`) and a float (`analytics_row.go`),
not from a wire format alone. `cluster_info.go` (13) and `plugins_response.go` (13) remain
candidates; read both before folding them together, since a pair is only worth one session when
one oracle genuinely covers both.

**No file in the remaining sub-20-line set has produced a new tech-debt entry from its wire format
alone** — the last four came from a method (`Audits::Etag`), a tag convention (`limits.go`), a
pointer rule (`channel_search.go`) and a float (`analytics_row.go`). Prefer a small file **with a
method** over a smaller one without.

**Not** `post_deletion_report.go` (245 lines) — it imports `shared/i18n` for `TranslateFunc` and
half its methods render a translated report, so it needs an i18n decision first. Log one when it
comes up rather than inventing a `TranslateFunc` shim.

Everything still owed in the interactive-message surface needs a **decision**, not a session:
the crypto half of integration_action.go and `AddMmBlocksActionCookies` need a crate choice
([D-046]), and `ValidateMmBlocksActions` needs `shared/markdown` ([D-044]).

**Generate fixtures under `TZ=Asia/Kolkata`.** A plain `go run .` rewrites all twenty rows of
`behaviour_utils.json` on any host whose zone differs, because the day-bounds corpus reads the
server's local calendar ([D-008]). Nothing breaks — the Rust test rebuilds the instant in the
*recorded* zone — but it destroys the "a clean run touches only new files" signal. See [D-069].

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
| model/permission.go (2,789 ln) | Generate from Go, do not hand-translate — **done 2026-08-19**, see the progress row |
| model/config.go (5,795 ln) | Translate lazily, section by section |
| enterprise/ | Separate license; proxy to Go permanently |
| plugin host (hashicorp/go-plugin) | Keep Go process alive indefinitely |
| search (Bleve / Elasticsearch) | Deferred past Phase 5 |

Deferred work and known divergences live in `docs/TECH_DEBT.md`, not here. Log an entry there
whenever a session skips, approximates, or discovers-but-does-not-close something.

## Progress

| Go source | Rust target | Status | Tests | Notes |
|---|---|---|---|---|
| app/authorization.go (the remaining 13 checks) + store/sqlstore/channel_store.go (`GetForPost`, `GetMemberForPost`) | `mm-app/src/authorization.rs`, `mm-app/src/config.rs`, `mm-store/src/channel_store.rs` | PARTIAL | 14 pass + 8 DB | **Closes [D-094]; narrows [D-134] from "most of the file" to sixteen functions behind four named stores.** `authorization.go` is now 20 of 36 — every system-, team-, channel-, user- and post-scoped check in **both** the session-scoped and `askingUserId` forms. The thing to know is that the two families are *not* interchangeable and the differences are not uniform: the user-scoped forms variously drop the unrestricted branch, the empty-id screen, the `manage_system` shortcut and the existence check, while adding a `DeleteAt` filter the session form has no need of — so substituting one for its twin grants access Go refuses. Two measured consequences: `has_permission_to_user("", "")` is **true** (equal strings hit the self-check, which is the first line and has no empty screen), and the two by-post twins **disagree for a DM post** because only the session one guards its team fallback on a non-empty `TeamId`. The by-post group was listed in [D-134] as blocked on a post store; it was not — both queries return channel types and merely join through `Posts`. Config arrives as two bools that cannot be read from Go's `config.json` ([D-156]). 14 mutations, 13 caught, 2 controls survived; **the survivor was the finding** — the restricted-admin denial was asserted against an unreachable store, where deleting the branch entirely still denied, and it took a DB-backed test where the fallthrough genuinely grants. |
| store/sqlstore/channel_store.go (`GetMember`) | `mm-store/src/channel_store.rs` | PARTIAL | 19 pass + 5 DB | `getChannelRoles` (channel_store.go:248) is `getTeamRoles` with **three** levels of fallback rather than two — the channel's scheme wins, the team's scheme is the fallback, the constant is last — and the team-scheme level reads that scheme's `DefaultChannel*Role` columns, not its `DefaultTeam*Role` ones. Both are silent permission differences if reversed, and both were **measured** against the running Go server rather than read: 14 role shapes written into a shared `ChannelMembers` row, asked of both servers, compared as whole serialised documents. No fixture is possible ([D-138]) — the function is unexported. Three mutations confirmed the oracle: all three survived the unit tests. **Correction (same day):** this row originally claimed `GetMember` unblocked `SessionHasPermissionToChannel`. It does not — that check uses `GetAllChannelMembersForUser` — see [D-134]. `Get` and `GetAllChannelMembersForUser` landed the next session, along with the second role resolver ([D-142]). |
| store/sqlstore/channel_store.go (`Get`, `GetAllChannelMembersForUser`) | `mm-store/src/channel_store.rs` | PARTIAL | 19 pass + 5 DB | **The real prerequisites for `SessionHasPermissionToChannel`.** `Get` is *the message channel with this id* — a `Type IN (O,P,D,G)` filter Go states as `messageChannelTypes`, so a board is deliberately invisible — plus two `AccessControlPolicies` subqueries that are computed columns rather than stored ones. `GetAllChannelMembersForUser` carries **`allChannelMember.Process`, a second role resolver that disagrees with `getChannelRoles`** on the same row ([D-142]); the disagreement was measured out of a single Go response that reported one role set in its body while granting on another. |
| app/channel.go (`GetChannel`) | `mm-app/src/channel.rs` | PARTIAL | 2 pass | Store-error mapping only, and the two branches are not interchangeable: `SessionHasPermissionToChannel` logs a 500 and stays silent on a 404, so collapsing them erases the only signal that distinguishes an outage from a missing channel. Deferred: `HydrateChannelPolicyActions` ([D-141]). |
| app/authorization.go (`SessionHasPermissionToChannel`) | `mm-app/src/authorization.rs` | PARTIAL | 3 DB | **The first check verified branch-by-branch against the running Go server.** Sessions are *injected* into the shared table and Go accepts them, which is what made it possible to ask Go the same questions as a non-admin — the fixture user is a `system_admin`, for whom every case grants. 12 cases, then seven mutations: three survived and each named a real hole, two of which were closed by adding a role holding only `manage_system` and a member of an *archived* channel. The third ([D-144]) is untestable without inverting the store dependency, and is logged rather than papered over. |
| api4/channel.go (`getChannelMember`) | `mm-api/src/channels.rs` | PARTIAL | 6 pass + 7 parity | **The first route served past a real permission check** rather than around one, and the first with path parameters. Byte-identical against the running Go server, 546 bytes. Brings `RequireChannelId`/`RequireUserId`, the `me` alias (resolved *before* validation), the `invalid_url_param` error, and this call site's trailing newline ([D-086]). Nine mutations: seven caught, one closed by adding a non-admin actor ([D-147]), one unfalsifiable over HTTP because our 400 does not name the parameter ([D-149]). |
| app/channel.go (`GetChannelMember`) | `mm-app/src/channel.rs` | PARTIAL | 4 pass | Store-error mapping. The two ids differ only by an inserted `missing.` and one lives in `app/constants.go` rather than inline, so both are pinned by tests. Unlike `GetChannel`, neither branch carries `params`. |
| store/sqlstore/channel_store.go (`GetChannelUnread`) | `mm-store/src/channel_store.rs` | PARTIAL | 4 DB | **The unqualified column names carry the behaviour.** Go's `FROM Channels, ChannelMembers` is an implicit cross join whose four predicates name their columns bare, and each resolves to the only table that has it: `ChannelMembers` has no `Id` and no `DeleteAt`, `Channels` has no `ChannelId` and no `UserId`. So `DeleteAt = 0` is the **channel's** — which makes this the opposite of `GetMember`, whose api4 call site passes `includeDeleted = true`. The same two ids give a **404 here and a 200 there** on an archived channel, and both answers are Go's; asserted together so neither reads as a fixture accident. Three more: `MsgCount` is a *subtraction* (`TotalMsgCount - MsgCount`) with nothing forcing it non-negative; only `UrgentMentionCount` is coalesced, so a NULL in any of the other six is a Go **scan error and a 500**, reproduced with sqlx `!` overrides rather than `unwrap_or_default`; and `NotifyProps` is selected purely for the app layer, since `json:"-"` keeps it off the wire. The `Type IN (O,P,D,G)` filter is **unreachable through its own route** and needed a store-level test — [D-151]. |
| app/channel.go (`GetChannelUnread`) | `mm-app/src/channel.rs` | PARTIAL | 4 pass | Store-error mapping plus the one transform: `mark_unread = mention` zeroes `MsgCount` and `MsgCountRoot` and **nothing else**, so a muted channel still reports the mentions that pierce the mute. Unlike `GetChannel` and `GetChannelMember`, **both error branches carry the same id** (`app.channel.get_unread.app_error`) and vary only the status — a client cannot tell a missing channel from a broken database here. The shortcut is lifted into `apply_mark_unread_shortcut` so it can be pinned without Postgres, the same reason `validate_ids` exists. |
| api4/channel.go (`getChannelUnread`) | `mm-api/src/channels.rs` | PARTIAL | 15 pass + 10 parity + 4 DB | **The first route with two permission gates, and the first where the path's segment order disagrees with the validation order.** Registered under `BaseRoutes.ChannelForUser`, so the *user* id is the first segment — while the handler opens `RequireChannelId().RequireUserId()`, so the *channel* is validated first and wins when both are malformed. `SessionHasPermissionToUser` (`edit_other_users`) runs before `SessionHasPermissionToChannel` (`read_channel`) and **short-circuits it**, which matters because the second is a database read; neither the order nor the permission each gate names is visible in a response, so `first_denied_permission` takes the second gate as a closure and a test asserts it is never polled. Also closed [D-150]: Go's `{channel_id:[A-Za-z0-9]+}` is a **routing** rule, so a segment with a hyphen is a mux 404 where we answered 400 — now forwarded, which fixed `getChannelMember` too. Sixteen mutations run, sixteen caught, two no-op controls survived. |
| store/sqlstore/status_store.go (`GetByIds`) | `mm-store/src/status_store.rs` | PARTIAL | 1 pass + 2 DB | Go's six `COALESCE`s reproduced in the SQL (every column but the key is nullable; nothing over REST writes a NULL, so the DB suite plants one). No `ORDER BY` in Go, none here — the app layer owns the order. `Get` not ported: only `updateUserStatus` (PUT) reaches it. |
| app/platform/status.go (`GetUserStatusesByIds`) | `mm-app/src/status.rs` | PARTIAL | 4 pass | The status **cache** is not ported; every id is a miss and the table answers. Content agrees with Go wherever cache and row were written together (`PUT …/status`) or neither exists; a user with no row — or no user at all — is `{user_id, status: "offline"}`, never an error. Output is found-ids-sorted then missing-in-input-order, which is Go's warm-cache order. `ENABLE_USER_STATUSES` is a constant stand-in ([D-085]'s shape). |
| api4/status.go (`getUserStatus`, `getUserStatusesByIds`) | `mm-api/src/status.rs` | PARTIAL | 7 pass + 7 parity | **`getUserStatus` is the list lookup, not `GetStatus`**: an unknown id answers 200 offline, and the handler's 404 is reachable only with statuses disabled. No permission check on either. The POST body is `json.Decoder.Decode` into `[]string`: trailing bytes ignored, `null` elements become `""`, then sort+dedup, then `len == 26` **bytes** with no charset check — each branch a unit test and a cross-server refusal. `Encode` (newline) for the single, `Marshal` (none) for the list. |
| store/sqlstore/channel_store.go (`GetByNames`) | `mm-store/src/channel_store.rs` | PARTIAL | 4 DB | The team filter **only exists when `teamId` is non-empty** (Go omits the predicate, channel_store.go:1656) — the DM/GM case — folded into one `$2 = '' OR` here and pinned with two teams sharing a channel name. Third transcription of `messageChannelTypes`; [D-151]'s shared oracle is now overdue. `allowFromCache` dropped: no cache, never staler than Go. |
| app/channel.go (`GetChannelsByNames`, `FillInChannelProps`) | `mm-app/src/channel.rs` | PARTIAL | 6 pass | Only **open** channels render into `channel_mentions`, keyed by the mentioned channel's `Name` with `display_name` inside; the stale-prop delete branch is **dead through `getChannel`** (the store never selects `Props`) but ported and unit-pinned, same shape as [D-151]. The `len > 0` guard above both branches is load-bearing — without it an emptied header deletes a prop Go leaves. |
| api4/channel.go (`getChannel`) | `mm-api/src/channels.rs` | PARTIAL | 5 pass + 11 parity | **The first route whose permission block branches on the fetched row**: open channels ask the team (`read_public_channel`) first and the channel gate only on denial; non-open channels never consult the team gate; both denials name `read_channel` (`get_channel_denial`, pinned in-process after the inline version survived a mutation). `?as_content_reviewer=true` is **forwarded** — license-gated, and Go re-runs the steps it checks first, so ordering holds by construction. Discoverable-channels fallback pinned off — [D-153]. Fifteen mutations, fifteen caught (one after extracting the denial helper); the **no-op control failing was the real finding** — see notes. |
| store/sqlstore/team_store.go (`GetTeamsByUserId`) | `mm-store/src/team_store.rs` | PARTIAL | — | The **teams**, where `get_teams_for_user` (same file) returns the memberships. Two separate `DeleteAt = 0` predicates — the membership's and the team's — each resurrect a different thing if dropped, and both are **measured** over REST rather than transcribed (`parity_teams_for_user.rs`), unlike [D-151]'s type filter. |
| app/team.go (`GetTeamsForUser`, `SanitizeTeam`, `SanitizeTeams`) | `mm-app/src/team.rs` | PARTIAL | 3 pass | The sanitiser's **pairing** is the content: `manage_team` restores `email`, `invite_user` restores `invite_id`, and crossing them leaks an invite id — lifted into `apply_team_sanitize` so all four cells are pinned without a database. Both permission reads run unconditionally, as Go's do. |
| api4/team.go (`getTeamsForUser`) | `mm-api/src/teams.rs` | PARTIAL | 5 pass + 7 parity | **[D-094]'s "not escapable" example, now served** — the test that kept it forwarded now asserts the opposite. Gate: self by string comparison (no permission machinery runs), anyone else needs `sysconsole_read_user_management_users`. A plain member's own list lands in the sanitiser's **mixed cell**: `team_user` grants `invite_user`, so `invite_id` survives and only `email` is stripped — measured, the first draft of the test assumed both stripped and Go said otherwise. `json.Marshal` + `w.Write`, no newline ([D-086]); a nonexistent user is `[]` for an admin, never a 404. Nine mutations, nine caught, two controls survived. |
| store/sqlstore/channel_store.go (`GetMemberCount`, `GetGuestCount`, `GetPinnedPostCount`, `GetFileCount`) | `mm-store/src/channel_store.rs` | PARTIAL | 2 DB | Four `COUNT(*)`s with one trap each: member and guest counts join `Users` so a **deactivated** member's surviving row does not count; `SchemeGuest = TRUE` filters a NULL flag out by SQL three-valued logic, no `COALESCE`; the pinned `DeleteAt = 0` is the post's; `FileInfo.PostId != ''` drops uploaded-but-never-attached files. NULL-flag and deactivated-guest shapes are DB-test transcriptions ([D-151]'s shape); the rest is measured over REST. `allowFromCache` dropped as in `GetByNames`. |
| app/channel.go (`GetChannelMemberCount`, `GetChannelGuestCount`, `GetChannelPinnedPostCount`, `GetChannelFileCount`) | `mm-app/src/channel.rs` | PARTIAL | 1 pass | One 500-only branch each, and three error-identity traps pinned by a single test: `GetChannelGuestCount` **reuses `app.channel.get_member_count.app_error`** (no guest id exists in Go), two of the four pass the *store* method's name as `where`, and `get_pinnedpost_count` has no middle underscore — the same missing underscore as the wire tag. |
| api4/channel.go (`getChannelStats`) | `mm-api/src/channels.rs` | PARTIAL | 9 pass + 7 parity + 2 DB | One `read_channel` gate — **no open-channel team fallback**, so a team member who never joined a public channel is refused stats that `getChannel` would serve. A missing channel is a **403 even for the admin** (see notes; the first draft asserted 200-of-zeroes and both servers refused). `?exclude_files_count=true` puts `-1` on the wire and skips the `FileInfo` query, pinned by closure like the permission gates. Encoder newline ([D-086]). Twelve mutations, twelve caught, two controls survived. |
| store/sqlstore/team_store.go (`Get`) | `mm-store/src/team_store.rs` | PARTIAL | — | One row by primary key, `teamSliceColumns(true)`, **no `DeleteAt` filter** — an archived team serves here while vanishing from `GetTeamsByUserId`'s list, both measured over REST (`parity_team_get.rs`). Go's `team.Id == ""` guard (team_store.go:365) is ported though unreachable by PK semantics. |
| app/team.go (`GetTeam`) | `mm-app/src/team.rs` | PARTIAL | 2 pass | Store-error mapping with a one-gerund trap: the 404 is `app.team.get.**find**.app_error`, the 500 is `app.team.get.**finding**.app_error`. Nil params in both branches. |
| api4/team.go (`getTeam`) | `mm-api/src/teams.rs` | PARTIAL | 6 pass + 9 parity | **The first route with a system-scope fallback gate**: `view_team` computed unconditionally, and a *public* team (`AllowOpenInvite && Type == "O"` — both conjuncts pinned) falls back to `list_public_teams`, which plain users hold — so any authenticated user reads a public team they never joined, fully sanitised. **Both denials name `view_team`** (Go's comment says so explicitly). Fetch precedes the gate, so a missing team is a 404, and `?as_content_reviewer=true` is forwarded — Go reads the flag *after* `GetTeam`, so a missing team with the flag is still a 404, held by construction. Seven mutations, seven caught, two controls survived. |
| store/sqlstore/team_store.go (`GetTotalMemberCount`, `GetActiveMemberCount`) | `mm-store/src/team_store.rs` | PARTIAL | 2 DB | **"Total" is current memberships including deactivated users**: `TeamMembers.DeleteAt = 0` filters departures from both counts, and the single `Users.DeleteAt = 0` predicate is the whole difference between the two. Go's `ViewUsersRestrictions` parameter is dropped — the route forwards any restricted caller to Go, so no caller of this port can hold one. |
| app/authorization.go (`HasPermissionTo`) | `mm-app/src/authorization.rs` | PARTIAL | 1 pass | The **user-based** check: the user *row's* roles, fresh from the store, with no `is_unrestricted` shortcut — and a `GetUser` failure is a quiet `false` (Go discards the error), so a broken database denies rather than 500s here. |
| app/team.go (`GetTeamStats`) | `mm-app/src/team.rs` | PARTIAL | 2 pass | Go runs the two counts on goroutines and reads the **total**'s channel first, so its error wins when both fail — sequential awaits preserve that precedence; the concurrency is invisible on the wire. The active id inserts `active_` into the total's id, which is also the id `GetChannelGuestCount` borrows — three call sites now share it. |
| api4/team.go (`getTeamStats`) | `mm-api/src/teams.rs` | PARTIAL | 6 parity + 2 DB | `view_team` gate with **no public-team fallback** (a non-member can read a public team's body via `getTeam` and not its stats), and **nothing fetches the team** — so a missing id is a *200 of zeroes* for an admin and a 403 for a plain user, the exact opposite split of `getChannelStats` on the same shape; both measured. The restrictions path (`view_members` not held) **forwards to Go** — unreachable in this deployment, so the forward itself is transcribed, not measured. Six mutations, six caught; one control failure exposed a harness race (see notes), fixed and re-verified before the tally was trusted. |
| model/user_terms_of_service.go | `mm-model/src/user_terms_of_service.rs` | PARTIAL | 2 pass | Wire struct only, fixture-pinned; `IsValid`/`PreSave` land with the save route. |
| store/sqlstore/user_terms_of_service.go (`GetByUser`) | `mm-store/src/user_terms_of_service_store.rs` | PARTIAL | 1 DB | One row by PK (a user holds at most one acceptance). The nullable columns scan to zero values, as Go's non-pointer fields do. |
| app/user_terms_of_service.go (`GetUserTermsOfService`) | `mm-app/src/user_terms_of_service.rs` | PARTIAL | 1 pass + 1 DB | The 404 (`no_rows.` inserted into the 500's id) is a **normal outcome** at its one call site — `getUser` ignores it, so the id needed an mm-app DB test to be falsifiable at all. Closes [D-083]. |
| app/authorization.go (`HasPermissionTo` for `getUser`), api4/user.go (`getUser`) | `mm-api/src/users.rs` | PARTIAL | 6 pass + 9 parity | **`GET /users/{user_id}` served; [D-082]'s warning heeded and closed.** The `/users/*` namespace is api4's most crowded, so the handler serves only a segment that is *exactly* a valid 26-char id and forwards everything else — Go's literal GETs (`stats`, `known`, `autocomplete`, `tokens`) keep working unported and upstream additions cannot break it. `UserCanSeeOtherUser` on its nil-restrictions fast path (self, or user-based `view_members`); the restricted remainder forwards, like `getTeamStats`. ToS lands **before the etag** (its fields are etag inputs); sanitize split: self = lax empty map, other = strict `SanitizeProfile` (admin forces 4 flags, 2 of which have no config source). `/users/me` now shares the whole tail via `respond_with_user`. **Found and fixed: the missing-user 404 id is `app.user.missing_account.const`** — `.const`, the Go keyword — where this port had shipped `.error` unverified for three days ([D-151]'s lesson at the app layer). Eight mutations, eight caught, two controls survived. |
| store/sqlstore/user_store.go (`GetByUsername`) | `mm-store/src/user_store.rs` | PARTIAL | 1 pass + 1 DB | `usersQuery` with `Username = lower(?)` — the **parameter** is folded, never the column, and the fold is **unreachable over REST** (`IsValidUsername` rejects uppercase first; it serves Go's login paths) — DB-pinned, [D-151]'s shape. The shared row mapping moved to `user_from_row`, `query_as!` style. |
| app/user.go (`GetUserByUsername`) | `mm-app/src/user.rs` | PARTIAL | 1 pass | **One id for both branches** (`app.user.get_by_username.app_error`, status-only split) and it is *not* `MissingAccountError` — three lines from `GetUser`'s two-id shape in the same Go file. |
| api4/user.go (`getUserByUsername`) | `mm-api/src/users.rs` | PARTIAL | 1 pass + 5 parity | Fetch **before** visibility (the inversion of `getUser`), with the failure branch's existence-hiding 403 for restricted callers kept Go's-own via the pre-fetch `view_members` forward. `RequireUsername` answers the **body**-param 400 for a path segment; the mux class (`[A-Za-z0-9\_\-\.]+`) is wider than the validator (`[a-z0-9\.\-_]+`), so `SliceUser` routes and then 400s. Tail shared with both `getUser` variants via `respond_with_user`. Five mutations, five caught, two controls survived. |
| model/utils.go (`SortedArrayFromJSON`) | `mm-model/src/utils.rs` | DONE | 1 oracle (37 bodies) | `json.Decoder.Decode` into `[]string` then sort+dedup: trailing bytes ignored, `null` element → `""`, `null` body → empty list, and a **lone surrogate escape → U+FFFD** where serde rejects — reproduced by rewriting the escape before decoding. `status.rs` now uses it. |
| store/sqlstore/user_store.go (`GetProfileByIds`) | `mm-store/src/user_store.rs` | PARTIAL | 2 DB | `Since` filters only when **positive** (`0` and negative are no filter); **no `DeleteAt` predicate**; `ORDER BY Username`. View restrictions not ported (api forwards). |
| app/user.go (`GetUsersByIds`) | `mm-app/src/user.rs` | PARTIAL | 1 pass | One branch, one id (`app.user.get_profiles.app_error`, 500); no not-found. Sanitising stays in the api layer with the privacy stand-ins (D-085). |
| api4/user.go (`getUsersByIds`) | `mm-api/src/users.rs` | PARTIAL | 4 pass + 5 parity | `POST /users/ids`, the literal beside `{user_id}`; restricted callers forwarded before the body is read. **No self exception**: the caller's own row is `SanitizeProfile`d like the rest. **Go's order and `update_at` both come from `userProfileByIdsCache`** — order varies with recent requests, and `update_at` is stale after every login (`UpdateLastLogin` never invalidates); the suite compares as sets and patches each fixture user. Fifteen mutations, fifteen caught, two controls survived. |
| store/sqlstore/channel_store.go (`GetMembers`) | `mm-store/src/channel_store.rs` | PARTIAL | — | **`Limit > 0` and `Offset > 0` are guards, not clamps**: squirrel adds the clause only when positive, so `limit = 0` is *no limit* — expressed as `LIMIT CASE WHEN … END`, since Postgres reads `LIMIT NULL` as absent. No `ORDER BY`: pagination over heap order, identical across the two servers only because they share the table. The member row mapping moved to `channel_member_from_row`, shared with `GetMember`. `ChannelMembersGetOptions` flattened to the three used fields (the `allowFromCache` rule). |
| app/channel.go (`GetChannelMembersPage`) | `mm-app/src/channel.rs` | PARTIAL | — | `Offset = page × per_page` (wrapping, as Go's `int` product), `Limit = per_page`, one 500-only id (`app.channel.get_members.app_error`) — an empty channel is `[]`, never a miss. |
| api4/channel.go (`getChannelMembers`) + web/params.go (`page`, `per_page`) | `mm-api/src/channels.rs` | PARTIAL | 8 pass + 5 parity | **The first paginated route.** Go's pagination contract, measured: garbage and negatives fall to defaults (0 / 60) with **no 400 ever**, `per_page` clamps at 200 — and **`per_page=0` serves the whole channel**, because zero survives the parser and the store's guard reads it as unlimited. Gate is `read_channel` (missing channel → 403 like `getChannelStats`); `SanitizeForCurrentUser` blanks every row's timestamps to `-1` except the caller's own, mid-list. Encoder newline; an empty page is `[]`. Six mutations, six caught, two controls survived. |
| store/sqlstore/team_store.go (`GetByName`, `GetMember`, `GetMembers`) | `mm-store/src/team_store.rs` | PARTIAL | 1 DB (8 branches) | **`GetMembers` emits `LIMIT`/`OFFSET` unguarded, so `per_page=0` is an empty list — the opposite of the channel store.** Sort is a three-way branch on the raw string (`""` → `UserId`, `"Username"` → `Username`, anything else → no `ORDER BY`); `GetMember` has no `DeleteAt` filter where `GetMembers` does; `GetByName` is exact-match, no folding, and serves archived teams. Restrictions dropped (forwarded). |
| app/team.go (`GetTeamByName`, `GetTeamMember`, `GetTeamMembers`) | `mm-app/src/team.rs` | PARTIAL | 3 pass | **`GetTeamByName`'s fallback branch is a 404, not a 500** (team.go:979) — the only sibling that does this. `GetTeamMembers` shares `get_members.app_error` with `GetTeamMembersForUser`. |
| api4/team.go (`getTeamByName`, `getTeamMember`, `getTeamMembers`) | `mm-api/src/teams.rs` | PARTIAL | 10 pass + 13 parity | By-name gate is `&&`-short-circuited — a public team is admitted with **no** permission query, no `list_public_teams` fallback (unlike `getTeam`). gorilla registers `{team_id}` before `/name/`, so `GET /teams/name/{image,stats,members}` is Go's 400 on `team_id`; axum resolves the other way, so those three forward (see `TEAM_BY_NAME_SHADOWED_LITERALS`). `SanitizeRoleData` gated on `manage_team_roles` — the guard `getTeamMembersForUser` could skip is live here. 17 mutations, 17 caught, 2 controls survived. |
| store/sqlstore/channel_store.go (`getByName`, `GetChannels`) | `mm-store/src/channel_store.rs` | PARTIAL | 4 DB | `getByName`'s team filter is a literal `TeamId = ? OR TeamId = ''` — **not** `getByNames`'s omitted-predicate wildcard — so a DM answers under any team and an empty team id finds only teamless rows. `GetChannels` orders by `DisplayName`, includes teamless channels in every team's list, and answers **`ErrNotFound` for zero rows**. |
| app/channel.go (`GetChannelByName`, `GetChannelsForTeamForUser`, `FillInChannelsProps`) | `mm-app/src/channel.rs` | PARTIAL | — | The list's error `where` is `GetChannelsForUser` (copied from the sibling in Go, on the wire). `FillInChannelProps` is now the one-element case of the list version, which batches mentions per team; `HydrateChannelsPolicyActions` stays unported ([D-141]). |
| api4/channel.go (`getChannelByName`) | `mm-api/src/channels.rs` | PARTIAL | 6 pass + 8 parity | Name lower-cased **before** `RequireChannelName`; non-open non-member is a **404** with the store's `missing` id (not `getChannel`'s 403), `manage_team` admits a team admin to a private channel, and the open-branch 403 names `read_public_channel`. Mux class `[A-Za-z0-9_-]+` forwards a dotted segment. Encoder newline. |
| api4/channel.go (`getChannelsForTeamForUser`) | `mm-api/src/channels.rs` | PARTIAL | 6 pass + 7 parity | Mutations across the two routes: 17 run, 17 caught, 2 controls survived per suite.  `RequireUserId().RequireTeamId()` (user first), gates `edit_other_users` then `view_team` **before** query parsing; `last_delete_at < 0` is the one 400 (`+` in a query is a space, so only `%2B` is a sign). Etag computed before `FillInChannelsProps`, exact `If-None-Match` → 304 with `ETag`; zero channels is a 404. Encoder newline. |
| store/sqlstore/channel_store.go (`GetMembersForUser`) | `mm-store/src/channel_store.rs` | PARTIAL | 3 DB | **The team predicate is on `Teams.Id` through the LEFT join** (`= ? OR = '' OR IS NULL`), so a DM — and a membership whose channel names a team that no longer exists — is in every team's answer; **no `DeleteAt` filter**, so archived memberships are listed; `Type NOT IN ('S')` only, so a board is listed where `GetChannels` hides it. Heap order. Mutations: 5 run, 5 caught, 2 controls survived. |
| app/channel.go (`GetChannelMembersForUser`) | `mm-app/src/channel.rs` | PARTIAL | — | One 500-only id, **shared with `GetChannelMembersPage`** (`app.channel.get_members.app_error`; `where` differs). Empty is `[]`, the store builds the slice before appending. |
| api4/channel.go (`getChannelMembersForTeamForUser`) | `mm-api/src/channels.rs` | PARTIAL | 1 pass + 7 parity | `GET /users/{user_id}/teams/{team_id}/channels/members` served. Gates **team first** (`view_team`), then self-by-string or `manage_system` *through the team* — a team admin's `manage_team` is refused (measured). Every row is the target's, so an admin reading another user gets **every** `last_viewed_at`/`last_update_at` as `-1`; zero memberships is `[]` where the sibling list is a 404. Mutations: 5 run, 5 caught, 1 control survived. |
| store/sqlstore/channel_store.go (`GetChannelsByUser`) | `mm-store/src/channel_store.rs` | PARTIAL | 3 DB | Keyset page (`Id > from`, `LIMIT` unless `-1`) over every team in **`ORDER BY Id`**; the deletion filters test the **team** too through a `LEFT JOIN Teams` (`IS NULL` admits DMs), and `include_deleted` with `last_delete_at = 0` is no filter at all. `ErrNotFound` for an empty page. |
| app/channel.go (`GetChannelsForUser`) | `mm-app/src/channel.rs` | PARTIAL | — | Same two ids as the per-team sibling; the 404 is the handler's loop terminator, not only an error. |
| api4/channel.go (`getChannelsForUser`) | `mm-api/src/channels.rs` | PARTIAL | 5 pass + 6 parity | `GET /users/{user_id}/channels` served. Go **streams**: `[`, pages of 100 `Encode`d element by element (`}\n,{` separators, no newline after `]`), and **zero channels is a 200 whose body is `[` plus the 404 error JSON** — reproduced byte for byte. One gate (`edit_other_users`) before query parsing; `last_delete_at < 0` is the one real 400. Mutations: 16 run, 16 caught, 2 controls survived (one harness lesson: an inclusive-keyset mutant hung the unbounded page walk in the DB test until the loop got a bound). |
| store/sqlstore/team_store.go (`GetChannelUnreadsForAllTeams`) | `mm-store/src/team_store.rs` | PARTIAL | 3 DB | **The exclusion predicate is unconditional** (`TeamId <> ?` even for `''`), so DMs/GMs are hidden by default and surface as `team_id: ""` once any `exclude_team` is given; the sibling `GetTeamsForUser`'s conditional form would leak every DM. Deny-list is `NOT IN ('S')` — a board's counters feed the badge where `GetChannelUnread` refuses it; nothing coalesced. |
| app/team.go (`GetTeamsUnreadForUser`) | `mm-app/src/team.rs` | PARTIAL | 5 pass | Per-team fold: mentions always add, messages add unless that **row's** `mark_unread = mention`; one 500-only id. Go's list order is map-iteration (random per request), so the port emits first-appearance order and tests compare as sets. |
| api4/team.go (`getTeamsUnreadForUser`) | `mm-api/src/teams.rs` | PARTIAL | 1 pass + 7 parity + 3 DB | `GET /users/{user_id}/teams/unread` served; gate is self-by-string or **`manage_system`** (a `system_read_only_admin` can list another user's teams and is refused their badges — measured). `include_collapsed_threads=true` (literal string compare, not `ParseBool`) is **forwarded to Go** — the Threads store, `CollapsedThreads` and `PostPriority` config are unported — so on a CRT-enabled deployment most webapp traffic for this route is still Go's. Mutations: 10 run, 10 caught, 2 controls survived. |
| model/session.go | `mm-model/src/session.rs` | DONE | 20 pass | Strangler Fig critical path. Complete `IsValid`, `PreSave`, device-id validators. |
| model/team_member.go | `mm-model/src/team_member.rs` | DONE | 6 pass | Pulled ahead of its turn: `Session.TeamMembers` is on the wire, so session.rs cannot round-trip without it. `TeamMemberWithError`/`EmailInviteWithError` deferred. |
| model/team.go | `mm-model/src/team.rs` | DONE | 37 pass | First **complete** `IsValid` — every branch, all error ids. `Etag` landed with `channel_list.go`. |
| model/utils.go (IsValidEmail) | `mm-model/src/utils.rs` | DONE | 2,916 cases | Corpus-verified against Go: 128 hand-picked + 2,788 generated. Grammar is `dot-atom @ (dot-atom / [ip])`. |
| model/user.go | `mm-model/src/user.rs` | PARTIAL | 54 pass | Wire type + self-contained logic + the five custom-status accessors ([D-004] closed). Deferred: `pre_update`'s custom-status re-save, `IsValidUserRoles`, `CleanUsername`, `GetTimezoneLocation`. `Etag` landed with `channel_list.go`. `IsValid` and `PreSave` have since landed — see their own rows. |
| model/utils.go | `mm-model/src/utils.rs` | PARTIAL | 54 pass | See notes below. Deferred: `IsValidHTTPURL` (needs an RFC 3986 parser), `ParseHashtags` (goes with post.go), `Scan`/`Value` (go to mm-store), the io.Reader JSON helpers (serde replaces them), `NewRandomTeamName` (needs `IsReservedTeamName`). `Etag` and `ToJSON` have landed. |
| model/channel.go | `mm-model/src/channel.rs` | DONE | 41 pass | Complete `IsValid`/`IsValidBoard`/`Patch`/`PreSave`, all 12 wire types with a fixture, DM/GM naming. 15 of the 41 are oracle-driven. Deferred: `Scan`/`Value` (D-013). |
| model/channel_list.go | `mm-model/src/channel_list.rs` | DONE | 13 pass | Two `#[serde(transparent)]` newtypes plus their `Etag`. Unblocked by `CURRENT_VERSION`; also closed `Team::etag`, `User::etag` and `ChannelsWithCount` — D-010 and D-014 are both paid off. |
| model/utils.go (Etag) | `mm-model/src/utils.rs` | DONE | 11 diff cases | `etag(&[&dyn Display])` — Go is variadic over `any` with `%v`. `CURRENT_VERSION` is borrowed from `version.go` but **cannot drift**: the oracle records it and a test fails when the pinned SHA moves. |
| model/channel_member.go | `mm-model/src/channel_member.rs` | DONE | 30 pass | Complete `IsValid`, the six-key notify-props validator with both `allowMissingFields` modes, all 9 wire types with a fixture. Also closed the `DirectChannelForExport` half of D-014 in `channel.rs`. Deferred: `Auditable`. |
| model/channel_stats.go | `mm-model/src/channel_stats.rs` | DONE | 5 pass | Whole file. `PinnedPostCount` is tagged **`pinnedpost_count`** — no middle underscore — and the three `_()` accessors return `float64`, which is lossy above 2^53; the corpus drives the exact boundary so Rust's `as f64` rounding is measured against Go's rather than assumed. Go declares three accessors for five fields; there is deliberately no `FilesCount_`. |
| model/cluster_info.go | `mm-model/src/cluster_info.rs` | DONE | 3 pass | Whole file, six strings, no methods. The only thing that can drift is `IPAddress` being tagged **`ipaddress`**, one word, next to `schema_version` and `config_hash`. |
| model/read_receipt.go | `mm-model/src/read_receipt.rs` | DONE | 2 pass | Whole file — three fields, no `omitempty`, no methods. |
| model/team_search.go | `mm-model/src/team_search.rs` | DONE | 6 pass | Whole file: **three tag conventions in one struct** — `term` always present, seven `omitempty` pointers where `Some(0)` is a different document from `None`, and four `json:"-"` fields that are a security boundary rather than tidiness (`IncludePolicyEnforced` is server-controlled so a caller cannot surface governed teams). `IsPaginated` needs **both** pointers, so `page=0, per_page=0` is paginated while `page=5` alone is not. |
| model/permalink.go | `mm-model/src/permalink.rs` | DONE | 5 pass | Whole file. `NewPreviewPost` guards `post` and then dereferences `team` and `channel` unguarded — **both panic**, measured. The port takes those two by reference so the panic is unrepresentable ([D-152]); every input Go survives answers identically. Neither pointer field carries `omitempty`, so an absent preview is `null`, not a dropped key. |
| model/push_response.go | `mm-model/src/push_response.rs` | DONE | 4 pass | Whole file: five constants, a `#[serde(transparent)]` map newtype and three constructors. **`PushStatusErrorMsg` is `"error"`, not `"error_msg"`** — the constant name reads like the other value, and nothing in the Go source spells the wire form out. An empty message still writes the key. |
| — (tooling) | `reference/dump/behaviour_small_types.go` → `fixtures/behaviour_small_types.json` | DONE | drives 12 go_parity tests | One corpus for all six files: the 2^53 float boundary, four `IsPaginated` nil-ness combinations, `NewPreviewPost`'s one guard and two panics, the five push constants, and every zero value. |
| model/post_metadata.go | `mm-model/src/post_metadata.rs` | DONE | 10 pass | `PostMetadata`, `PostImage`, `PostTranslation`, plus `PostPriority` (whose Go home is post.go — the two files are mutually dependent). `Copy` reproduced including the two fields it drops. Deferred: `Auditable` ([D-028]). |
| model/post_embed.go | `mm-model/src/post_embed.rs` | DONE | 9 pass | Whole file except `Auditable` ([D-028]). Three output states for `data`, an `any` with `omitempty`. Wire format byte-for-byte against Go's **round-trip**, not its output — `data: null` is lossy in Go too. |
| model/post_acknowledgement.go | `mm-model/src/post_acknowledgement.rs` | DONE | 9 pass | Whole file. The only ported type whose `remote_id` has `omitempty`. Deferred: nothing. |
| model/file_info.go | `mm-model/src/file_info.rs` | PARTIAL | 23 pass | `FileInfo`, `GetFileInfosOptions`, `IsValid`, `PreSave`, `IsValidFilename`, `SanitizeFilename`, `IsImage`/`IsSvg`, `GetEtagForFileInfos`, `MakeContentInaccessible`. Wire format asserted **byte-for-byte**. Deferred: `Auditable` ([D-028]) and `NewInfo`'s mime lookup ([D-030]). |
| model/reaction.go | `mm-model/src/reaction.rs` | DONE | 12 pass | Whole file: `IsValid`, `PreSave`, `PreUpdate`, `GetRemoteID`. Reuses `is_valid_alpha_num_hyphen_underscore_plus` on **measured** evidence that Go's inline pattern is equivalent. Deferred: nothing. |
| model/emoji.go | `mm-model/src/emoji.rs` | DONE | 18 pass | Whole file except `Auditable` ([D-028]). The 4,464-entry system-emoji table is **generated** from Go into `emoji_generated.rs` rather than transcribed. Deferred: `Auditable`. |
| model/emoji_data.go | `mm-model/src/emoji_generated.rs` | GENERATED | — | 4,464 entries emitted by `reference/dump`. Never hand-edit; re-run the generator. Carries `#[rustfmt::skip]` so `cargo fmt` and the generator stay idempotent against each other. |
| model/post.go (chunk 1) | `mm-model/src/post.rs` | PARTIAL | 39 pass | The `Post` wire type, all 80 constants, complete `IsValid`, the pre-hooks, the props accessors and the predicate family. Wire format asserted **byte-for-byte** through `go_json_marshal`. Deferred: `propsIsValid`/`ValidateProps`, `Attachments`/`AllStrings`, `RewriteImageURLs`, `GetPreviewPost`/`ForPlugin`, `ToJSON`/`EncodeJSON`, `Auditable` ([D-028]), the `Rewrite*`/`ReportPost*` families, and `PreCommit`'s action-id step ([D-035]). `Clone` diverges by design ([D-036]). |
| model/utils.go (ArrayToJSON, StringInterfaceToJSON) | `mm-model/src/utils.rs` | DONE | 29 cases | The two marshallers `Post::is_valid` **measures** its three length caps with. A nil input is `"null"` — four runes against the cap, not `[]`/`{}`. |
| — (shared) | `mm-model/src/utils.rs::StringInterface` | CHANGED | 441 pass | Re-aliased from `HashMap` to `serde_json::Map`, which is sorted like Go's map marshalling. Closes the ordering half of [D-027]; see the note below. |
| — (tooling) | `reference/dump/behaviour_post.go` → `fixtures/behaviour_post.json` | DONE | 22 diff tests | 25 byte-exact wire probes, 51 `IsValid` cases, 17 notification-predicate cases, 25 mention-regex inputs, the reserved-props ordering corpus, and `PreSave`/`PreCommit`/`Patch`/`SanitizeProps` invariants. Corrected two conclusions a reading of the Go source had produced — see notes 3 and 8. |
| model/utils.go (IsValidHTTPURL) | `mm-model/src/utils.rs` | DONE | 3,529 cases | Closes [D-003]. Reproduces `net/url.ParseRequestURI`'s grammar rather than delegating to the `url` crate, which is WHATWG and would disagree both ways. 136 hand-picked + 2,881 generated + four exhaustive 0..127 byte sweeps. Four readings of the Go source were wrong; see the notes. |
| model/slack_compatibility.go | `mm-model/src/slack_compatibility.rs` | PARTIAL | 6 pass | `SlackCompatibleBool` only. The rest of the file is deprecated aliases onto `message_attachment.go` and lands with it. One accepted divergence ([D-037]). |
| — (tooling) | `reference/dump/behaviour_url.go` → `fixtures/behaviour_url.json` | DONE | 4 diff tests | The URL corpus plus a **diagnostics** section recording Go's actual `parse_error` and `Host` per input — that section is what converted four guesses about `ParseRequestURI` into measurements. Also the `SlackCompatibleBool` codec. |
| model/integration_action.go (chunk 1) | `mm-model/src/integration_action.rs` | PARTIAL | 15 pass | `PostAction` + its 10 satellite types, `IsValid`, `Equals`, `NormalizePostActionIntegrationFormat`, `PostActionPreserveState`. Wire format byte-exact over all 21 probes. Deferred: the `Dialog` family, ECDSA trigger ids, AES cookies, and the three `Post` methods that walk `props.attachments`. Two divergences ([D-038]). |
| — (shared) | `mm-model/src/utils.rs::MultiError` | DONE | 6 cases | `hashicorp/go-multierror`, not a Mattermost type. Reproduces `ListFormatFunc`'s exact layout and `Prefix`'s flattening. |
| — (tooling) | `reference/dump/behaviour_integration_action.go` → `fixtures/behaviour_integration_action.json` | DONE | 9 diff tests | 41 `IsValid` cases asserting the **full ordered message list**, 22 `Equals` cases, the multierror layout, a `recover`-probed panic, 16 format-normalisation inputs and 21 byte-exact wire probes. |
| model/message_attachment.go | `mm-model/src/message_attachment.rs` | DONE | 18 pass | Whole file: both wire types, both `IsValid`s, both `Equals`, `Stringify…`, `ParseMessageAttachment`, `ParseSlackLinksToMarkdown`. Wire format byte-exact. Two divergences, both from the bare `any` fields ([D-039]). `hex_color_regex` now lives here, its Go home, and `integration_action.rs` borrows it. |
| — (shared) | `mm-model/src/utils.rs::go_format_v` | DONE | 62 cases | Go's `fmt.Sprintf("%v")` for a JSON-decoded value, plus `go_format_float` for `%g`. Rust's `Display` never uses exponent form and `LowerExp` always does, so neither is substitutable. |
| — (shared) | `mm-model/src/utils.rs::json_values_equal_like_go` | DONE | 13 cases | Compares two decoded values the way Go does: every JSON number is a `float64`, so `1 == 1.0` and `1e2 == 100`. A plain `Value == Value` disagrees. |
| — (tooling) | `reference/dump/behaviour_message_attachment.go` → `fixtures/behaviour_message_attachment.json` | DONE | 11 diff tests | 42 `IsValid` cases, 26 `Equals` cases plus a JSON-decoded comparison corpus, 36 `%g` floats, 26 `%v` renderings, 21 Slack-link inputs, and a `recover`-probed panic. |
| model/post.go (chunk 2) | `mm-model/src/post.rs` | PARTIAL | 12 pass | `Attachments`, `AttachmentsEqual` and the non-interactive half of `AllStrings`. `Post` gained container-level `#[serde(default)]` — a partial post did **not** decode before, which is [D-043]. Deferred: `AllStrings`'s interactive half ([D-041]), `propsIsValid`/`ValidateProps` ([D-042]). Two divergences ([D-033] widened, [D-040] new). |
| — (tooling) | `reference/dump/behaviour_post_attachments.go` → `fixtures/behaviour_post_attachments.json` | DONE | 11 diff tests | 38 `Attachments` decode probes recording Go's returned slice verbatim, 20 `AttachmentsEqual` pairs with `recover` flags, and 45 `AllStrings` cases recorded under **both** option values so the unported half is measured rather than guessed at. |
| model/post_interactive_blocks.go | `mm-model/src/post_interactive_blocks.rs` | PARTIAL | 6 pass | The three human-string walkers and the three image-URL walkers, driving `Post::all_strings` (closes [D-041]) and `Post::interactive_blocks_image_urls`. Deferred as a unit: everything downstream of `appendMmactionIDsFromText`, which needs the 4,688-line `shared/markdown` parser ([D-044]). One accepted divergence reproduced ([D-045]). |
| — (tooling) | `reference/dump/behaviour_post_interactive_blocks.go` → `fixtures/behaviour_post_interactive_blocks.json` | DONE | 6 diff tests | 51 human-string cases and 27 image-URL cases, each recorded under **both** values of its flag. Every type mismatch in these walkers is a silent no-op, so the corpus drives each one individually rather than testing the happy path. |
| model/integration_action.go (chunk 2) | `mm-model/src/integration_action.rs` | PARTIAL | 13 pass | The whole `Dialog` family: 10 wire types, `Dialog`/`DialogElement`/`OpenDialogRequest`/`SubmitDialogResponse` `IsValid`, `EffectiveDateTimeConfig`, `IsValidLookupURL` and the date validators — including Go's `time.Parse` for the five layouts, reproduced rather than delegated. Wire format byte-exact over 26 probes. Deferred: the crypto half ([D-046]) and `ValidateMmBlocksActions` ([D-044]). |
| — (shim) | `mm-model/src/go_url.rs` | DONE | 7 pass | Go's `net/url`: `Parse`, `ParseRequestURI`, `URL::String`, `EscapedPath`/`EscapedFragment`, `escape`/`unescape`, `ParseQuery`, `Values::Encode`. **Not** the `url` crate, which is WHATWG. `is_valid_http_url` is now two lines on top of it and [D-003]'s 3,529 cases still pass unchanged — see the notes. Deferred: `ResolveReference`, `JoinPath`, error text ([D-049]). |
| model/mm_blocks_actions.go | `mm-model/src/mm_blocks_actions.rs` | PARTIAL | 8 pass | Whole file except `AddMmBlocksActionCookies` ([D-046]). Includes `Post::get_action`, whose Go home is integration_action.go — closes [D-047]. `StripMmBlocksActionSecrets` moved here from `integration_action.rs`, its Go home. One divergence ([D-050]). |
| — (tooling) | `reference/dump/behaviour_go_url.go` → `fixtures/behaviour_go_url.json` | DONE | 7 diff tests | 102 URLs through **both** `Parse` and `ParseRequestURI`, recording all 11 components plus the `String()` round trip; all 256 byte values through all six reachable escape modes; 30 unescape cases, 21 query corpora, 10 `Encode` corpora and 27 `MergeQueryIntoURL` cases. Byte-valued fields are recorded as base64 because a path can hold `0x80`. |
| — (tooling) | `reference/dump/behaviour_mm_blocks_actions.go` → `fixtures/behaviour_mm_blocks_actions.json` | DONE | 8 diff tests | A 44-case props corpus through `GetMmBlocksActionSpec` and `GetAction`, 13 `MmBlocksContextMap` inputs, 7 cookie lookups, 15 `ResolveMmBlocksAction` cases and 13 cookie-payload probes. `GetAction`'s synthesised action is asserted as marshalled JSON. |
| model/integration_action.go (chunk 3) | `mm-model/src/integration_action.rs` | PARTIAL | 6 pass | `StripActionIntegrations` and `GenerateActionIds`. Closes [D-035]; `pre_save`/`pre_commit` are now complete. `GetAction` landed with `mm_blocks_actions.go`; `StripMmBlocksActionSecrets` moved there. Deferred: the crypto half ([D-046]). One divergence ([D-048]). |
| model/post.go (ToJSON, EncodeJSON) | `mm-model/src/post.rs` | PARTIAL | 4 pass | Unblocked by chunk 3. `ToJSON` strips a **copy**, `EncodeJSON` strips the receiver and appends Go's encoder newline. Both marshal through `go_json_marshal`, asserted byte-for-byte wherever no attachment list is rewritten. `del_prop` corrected: Go materialises a nil `Props` into `{}`. |
| — (tooling) | `reference/dump/behaviour_post_actions.go` → `fixtures/behaviour_post_actions.json` | DONE | 10 diff tests | 34-case corpus run through six functions each, plus a 7-case `DelProp` probe. Generated ids are replaced with `<generated>` and counted so the fixture stays deterministic ([D-032]). Corrected one shipped test and one shipped behaviour — see notes 3 and 5. |
| — (tooling) | `reference/dump/behaviour_dialog.go` → `fixtures/behaviour_dialog.json` | DONE | 13 diff tests | 65 `DialogElement` cases and 16 `Dialog` cases asserting the **full ordered message list**, a 75-input date corpus run through three validators each, 15 time-interval cases, 21 lookup-URL inputs, 9 config-merge cases, 26 byte-exact wire probes and the `%q` corpus. |
| — (shared) | `mm-model/src/utils.rs::go_quote` | DONE | 2 pass, 27 cases | Go's `strconv.Quote`, i.e. `%q`. Rust's `{:?}` agrees on ordinary text and diverges on control characters, NBSP, U+200B, U+FEFF and U+0085 — which is most of what a validator interpolates when something is wrong. |
| — (shared) | `mm-model/src/utils.rs::StringMap` | CHANGED | 515 pass | Re-aliased from `HashMap` to `BTreeMap`, so it sorts like Go's map marshalling. Closes the rest of [D-027]'s ordering half; the `Dialog` wire probes are what forced it. |
| — (shared) | `mm-model/src/utils.rs::go_to_lower` | DONE | 2 pass, 30 cases | Go's `strings.ToLower`. **Not** `str::to_lowercase` — they disagree on `İ` and on final sigma. Replaced all six pre-existing `to_lowercase()` call sites. |
| model/preference.go | `mm-model/src/preference.rs` | DONE | 14 pass | Whole file: complete `IsValid` (every branch and error id), `PreUpdate`, `Preferences`, and all 42 constants pinned against Go. `PreUpdate` output is asserted **byte-for-byte**. Deferred: nothing. |
| model/status.go | `mm-model/src/status.rs` | DONE | 13 pass | Whole file. `STATUS_ONLINE` moved here from `user.rs::external`, and `STATUS_CACHE_SIZE` aliases `session::SESSION_CACHE_SIZE` rather than re-transcribing it — two more D-005 borrows closed. `to_json`/`status_list_to_json` are asserted **byte-for-byte** against Go. Deferred: nothing. |
| model/custom_status.go | `mm-model/src/custom_status.rs` | DONE | 21 pass | Whole file. First type whose timestamp is a real `time.Time`, not epoch ms — see `utils::go_time`. `USER_PROPS_KEY_CUSTOM_STATUS` moved here from `user.rs::external`. Deferred: nothing. The five `User` accessors that consume it landed the same day in `user.rs`. |
| — (shared) | `mm-model/src/utils.rs::go_time` | DONE | 51 cases | Go's `time.Time` JSON codec, not a Mattermost source file — same category as `go_json_marshal_string_map`. chrono's serde impl is **not** substitutable: four documented differences. |
| — (shared) | `mm-model/src/utils.rs::go_json_marshal` | DONE | 3 unit + 1 diff | `json.Marshal` with Go's HTML escaping for any `Serialize` value. Closes [D-022]. Use it — not `serde_json::to_string` — whenever a marshalled string is **stored** rather than sent. |
| model/version.go | `mm-model/src/version.rs` | DONE | 14 pass | Whole file. `CURRENT_VERSION` moved here from `utils.rs`, which now re-exports it — one definition, D-005's borrow closed. `VERSIONS`/`VERSIONS_WITHOUT_HOTFIXES` are unexported in Go; the oracle extracts the literal with `go/parser` so the transcription is checked. Deferred: nothing. |
| model/utils.go (ToJSON) | `mm-model/src/utils.rs` | DONE | 11 diff cases | `go_json_marshal_string_map` — the `map[string]string` case. Needed because the notify-props size cap **measures** Go's JSON, and serde_json escapes differently. |
| model/post_list.go | `mm-model/src/post_list.rs` | PARTIAL | 30 pass | Whole file except `WithRewrittenImageURLs` ([D-053]). All 16 methods, `NewPostList`, and the wire type. Wire format asserted **byte-for-byte** except where an attachment list is rewritten ([D-048]). Also landed `Post::for_plugin`, which `PostList::ForPlugin` is a wrapper over. Three divergences ([D-051] the unstable sort, [D-052] three panics, [D-033] widened). |
| model/wrangler.go | `mm-model/src/wrangler.rs` | DONE | 3 pass | Whole file. Ported alongside `post_list.go` because `BuildWranglerPostList` returns it — a 33-line struct with no logic. **No `json:` tags at all**, so the wire keys are Go's field names including the `EarlistPostTimestamp` typo. Deferred: nothing. |
| model/file_info_list.go | `mm-model/src/file_info_list.rs` | DONE | 22 pass | Whole file: the wire type, `NewFileInfoList` and all eight methods. Wire format asserted **byte-for-byte** across the corpus. **Not** a rename of `post_list.rs` — five things differ and each is measured, most usefully `ToSlice`'s always-nil result and a `MakeNonNil` that does not recurse. Three divergences ([D-058] the panics, [D-051] the unstable sort again, [D-033] the nil map value). Deferred: nothing. |
| — (correction) | `mm-model/src/post_list.rs` module docs | **FIXED** | — | The heading claimed `PostList::etag` is "order-independent". Its **first component is `Order[0]`**, so reversing `order` changes the etag; only the map half is iteration-order-independent. The code was already right — the doc was not, and it was about to be copied into this file. |
| — (tooling) | `reference/dump/behaviour_file_info_list.go` → `fixtures/behaviour_file_info_list.json` | DONE | 12 diff tests | 343 cases over a shared 14-document corpus, including `Extend` crossed with itself (196 pairs, each run twice to prove Go's randomised map iteration does not leak into the answer) and [D-051]'s tie corpus rebuilt for this type. Every case is `recover`-probed; 30 of Go's answers are a crash. |
| model/post_info.go | `mm-model/src/post_info.rs` | DONE | 7 pass | Whole file — one wire struct, no methods. The only type in the crate so far with **no `omitempty` on any field**, so its zero value is eight keys rather than `{}`. `channel_type` is a `String`, not an enum: Go's `ChannelType` is a defined string type that accepts anything, and unlike `Channel` there is no `IsValid` here to narrow it afterwards. Wire format asserted **byte-for-byte** over 13 documents. One instance of [D-057]. Deferred: nothing. |
| model/post_attributes.go | `mm-model/src/post_attributes.rs` | DONE | 1 diff test | Two constants, no types. Pinned against Go rather than transcribed on trust; the property-group machinery that consumes them (`property_field.go`, 735 lines) is unported. Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_post_info.go` → `fixtures/behaviour_post_info.json` | DONE | 3 diff tests | 14 wire probes covering every declared channel type plus one that is not a channel type at all, the no-`omitempty` zero value, and the two constants. |
| model/search_params.go | `mm-model/src/search_params.rs` | DONE | 23 pass | Whole file: `SearchParams`, all six date accessors, `splitWords`, `parseSearchFlags`, `ParseSearchParams` and `IsSearchParamsListValid`. `ParseSearchParams` asserted **byte-for-byte** over 186 (input, offset) pairs. Both term regexes are transcribed with **ASCII** `\d`/`\s`, because Go's Perl classes are ASCII and the `regex` crate's are Unicode — see the notes. One divergence ([D-057], the scalar half only). Deferred: nothing. |
| — (shared) | `mm-model/src/utils.rs::get_start_of_day_millis`, `get_end_of_day_millis` | **FIXED** | 209 cases | Both returned `None` for every `\|offset\| >= 86400` — they built a `chrono::FixedOffset`, which cannot hold a whole day, while Go's `time.FixedZone` takes any `int` of seconds straight from a client. Now plain arithmetic, and the offset parameter is `i64` (Go's `int`), not `i32`. |
| — (shared) | `mm-model/src/utils.rs::pad_date_string_zeros` | **FIXED** | 26 cases | Padded on `chars().count() == 1` where Go's `len(part) == 1` counts **bytes**, so a two-byte Arabic-Indic digit was padded here and not in Go. Shipped since the first utils session; the previous corpus was ASCII-only. |
| — (shared) | `mm-model/src/utils.rs::is_valid_hashtag`, `collapse_leading_hashes` | DONE | 71 words | `validHashtag` and `hashtagStart`, whose Go home is utils.go. `ParseHashtags`, the only other consumer, is still deferred. `#a` is **not** a valid hashtag — the pattern needs a letter and then one more letter or digit. |
| — (tooling) | `reference/dump/behaviour_search_params.go` → `fixtures/behaviour_search_params.json` | DONE | 11 diff tests | The four regexes extracted from the Go source with `go/parser` (they are unexported), then swept over 169 codepoints each; `strings.Fields` over the same sweep; 26 pad cases; 171 day-bound cases; 126 date-accessor cases; 186 end-to-end parse cases; and the wire corpus. The clock-dependent accessors record `uses_now` instead of a value. |
| model/draft.go | `mm-model/src/draft.rs` | DONE | 14 pass | Whole file: the wire type, `IsValid`, `BaseIsValid` (exported, so a public entry point of its own), `PreSave`, `PreCommit`, `GetProps`/`SetProps`. Wire format asserted **byte-for-byte** over 21 documents. **Not** a trimmed `Post` — five things differ and each is measured, most usefully that the message check runs *before* every base check, so the same broken object reports a different error id as a draft than as a post. `max_draft_size` is `i64`, not `Post::is_valid`'s `usize` ([D-059]). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_draft.go` → `fixtures/behaviour_draft.json` | DONE | 8 diff tests | 46 `IsValid` cases each recording **both** `IsValid` and `BaseIsValid` (id, detail, `Where` and status), 21 wire probes carrying the nil-ness of all four reference fields, 18 documents through each pre-hook, and the props accessors. The five cap-crossing cases *describe* their 800,000-rune padding instead of embedding it — 80 KB rather than 4 MB, see [D-060]. |
| model/channel_mentions.go | `mm-model/src/channel_mentions.rs` | DONE | 13 pass | Whole file, plus the three `Post` methods whose Go home is post.go — closes the last file-blocked item in post.go chunk 1. The `\B` in Go's pattern is **ASCII**; the `regex` crate's is Unicode, so the port spells it `(?-u:\B)` and a 164-codepoint sweep proves it ([D-062]). Six tests fail on the naive transcription, verified by making it. One divergence ([D-061], nil vs empty result). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_channel_mentions.go` → `fixtures/behaviour_channel_mentions.json` | DONE | 8 diff tests | A 164-codepoint sweep run through **four** positions in the pattern (656 probes), 44 `FromStrings` cases, 23 attachment cases and 12 posts through all four `Post` entry points. The sweep is the file — everything else is dedup and ordering. |
| model/mention_map.go | `mm-model/src/mention_map.rs` | DONE | 9 pass | Whole file: both newtypes, both codecs and the four **unexported** key constants, which the oracle recovers by encoding a one-entry map rather than transcribing. Two `#[serde(transparent)]` newtypes rather than one alias — the types differ only in which query keys they touch, so an alias would let a channel map encode itself under `user_mentions`. Two divergences ([D-063] the ordering, [D-064] non-UTF-8 query values). Deferred: nothing. |
| — (shared) | `mm-model/src/go_url.rs::Values::get_all`, `::set_all` | DONE | — | Go's two-value map read and direct map assignment. `Values::get` collapses "absent" and "present but zero-length", and `mentionsFromURLValues` branches on exactly that difference — `{"user_mentions": []}` takes the length check, not the not-found path. |
| — (tooling) | `reference/dump/behaviour_mention_map.go` → `fixtures/behaviour_mention_map.json` | DONE | 3 diff tests | 25 `FromURLValues` cases run through **both** types, recording the error text and Go's nil-ness; 12 maps through `ToURLValues`. The inputs are `map[string][]string`, not query strings, so a key with a zero-length slice is expressible. Go's own encoding is recorded only where map iteration cannot reorder it ([D-032]); the rest is pinned against a sorted reconstruction plus a round-trip flag. |
| model/scheduled_post.go | `mm-model/src/scheduled_post.rs` | PARTIAL | 18 pass | Whole file except `Auditable` ([D-028]). First type in the tree with a Go **anonymous field**: `Draft` is embedded, so `Deref` carries the method promotion and `Serialize` is **hand-written** — `#[serde(flatten)]` emits the flattened keys *last* and Go emits them *first* ([D-067]). Wire format asserted byte-for-byte over 13 documents. `repeat_timezone` uses `chrono-tz`, a new workspace dependency, because Go's `time.LoadLocation` is a **filesystem** lookup with no single answer ([D-065]). Two divergences ([D-065], [D-066] the aliased metadata). Deferred: `Auditable`. |
| — (tooling) | `reference/dump/behaviour_scheduled_post.go` → `fixtures/behaviour_scheduled_post.json` | DONE | 11 diff tests | 31 `IsValid` cases recording **both** entry points, 13 wire probes, a 50-name `time.LoadLocation` sweep, 9 documents through each pre-hook, 15 `ToPost` cases including all four priority errors with their `%v` map rendering, plus the three small mutators. `scheduled_at` is recorded as an **offset from now** and the unexported `scheduledPostMaxTimeGap` is read out of the Go source with `go/parser`, so the fixture stays deterministic ([D-032], [D-021]). |
| model/user.go (`IsValid`) | `mm-model/src/user.rs` | DONE | 6 pass | **Closes [D-002]'s `IsValid` half**, unblocked the same day by [D-001]. Eighteen branches, 34 corpus cases, checked in order — a user broken two ways reports the first failure. Three findings a reading gets wrong: a **remote user may hold an invalid email**, because only the *format* check sits behind `&& !u.IsRemote()` while emptiness and length do not; the timezone cap counts **runes of Go's marshalled JSON**, so a `<` costs six runes and the check goes through `go_json_marshal_string_map`; and `Props` gates the custom-status check by **nil-ness**, so an empty-but-present map still validates. Caps mix bytes (`Email`, `AuthData`, `Roles`) and runes (the four name fields, the timezone JSON) — driven at both boundaries with multi-byte input. One divergence ([D-107]): the `auth_data` branch formats a **pointer**, so Go's own detail holds an address that is **not stable across calls**; ours says `<pointer>`, chosen over the dereferenced value because `AuthData` is an SSO identifier and Go's accident keeps it out of the logs. |
| — (shared) | `mm-model/src/utils.rs::go_format_string_map` | DONE | 6 cases | Go's `%v` on a `map[string]string` — `map[a:1 m:13 z:26]`, sorted since Go 1.12, no quotes and no commas. Needed because `IsValid` interpolates a timezone map into an error detail, and nothing about `%v` announces that shape. |
| — (tooling) | `reference/dump/behaviour_user_is_valid.go` → `fixtures/behaviour_user_is_valid.json` | DONE | 6 diff tests | 34 `IsValid` cases recording id, where, status **and the detailed error**, 6 remote-email cases, 4 timezone-JSON probes with rune and byte counts side by side, 6 `%v` map renderings, and a two-call probe establishing that Go's `auth_data` detail is not reproducible. |
| model/user.go (`IsValidLocale`) | `mm-model/src/user.rs` + `locale_generated.rs` | DONE | 6 pass | **Closes [D-001]**, the oldest blocking entry, open since 2026-08-13. Go delegates to `x/text/language.Parse`, which validates against the **IANA subtag registry** rather than BCP 47 syntax — `xx` is syntactically perfect and rejected, `qaa`/`mul`/`zxx` look like nonsense and are accepted. So the table is **enumerated, not reasoned**: `UserLocaleMaxLength` is 5, the reachable space is **81,376,658** strings, and Go answers all of them in ~8s. The 234,421 accepted decompose into 190 + 8,794 + 327 component entries plus a rule, and the generator then **re-derives all 81 million answers from the tables and fails if one disagrees**. That caught the registry's 16 **grandfathered** `i-…` tags, which the first rule missed — they are now an exception list the generator derives from its own residual rather than one anybody typed. `i-en`, identical in shape, is correctly rejected. |
| model/user.go (generated) | `mm-model/src/locale_generated.rs` | GENERATED | — | 9,327 entries in four tables. Never hand-edit; re-run the generator. Each table carries `#[rustfmt::skip]` so `cargo fmt` and the generator stay idempotent, the same as `emoji_generated.rs`. |
| model/user.go (`PreSave`) | `mm-model/src/user.rs` | DONE | 12 pass | **Closes [D-108], and with it [D-002] entirely.** `pre_save_partial` is **deleted** — the crate's one genuinely dangerous function is gone rather than renamed around. Takes a `UserPasswordHasher`, exactly as Go does, because every implementation is in `server/channels/` and unreachable from this Apache-2.0 crate ([D-031]). Three things a reading gets wrong: the entry's own premise — Go writes **PBKDF2**, not bcrypt, and has since `latestHasher` changed; the timezone guard is `== nil` where `NotifyProps` three lines above is `len() == 0`, so an empty-but-present map stays empty; and the id is assigned **before** hashing, because both error branches interpolate `user_id=`. The custom-status re-save (user.go:546-550) landed with it. |
| shared/timezones/*.go | `mm-model/src/timezones.rs` | DONE | 8 pass | **Both files whole** — `timezones.go` (29 lines) and `default.go` (599, all data). `DefaultUserTimezone`'s `"true"` is a **string**, because `User.Timezone` is a `map[string]string`; a bool would change the wire format of every user object. The map is **fresh per call**, so a `&'static` would pass every serialization assertion and fail the first mutation. Marshalled key order is sorted, i.e. the reverse of the insertion order — free because of [D-027]. The 592 zones are **generated**, and the list is byte-sorted, which the oracle established after the first draft of the test asserted the opposite. |
| shared/timezones/default.go | `mm-model/src/timezones_generated.rs` | GENERATED | — | 592 entries emitted by `reference/dump`. Never hand-edit. Unlike [D-065]'s `time.LoadLocation`, this is a **compile-time literal in Go** rather than a host tzdata scan, which is exactly what makes it portable as a table. |
| channels/app/password/hashers/*.go (hash) | `mm-app/src/password/` | PARTIAL | 22 pass | **The `Hash` half of all three files**, in `mm-app` rather than `mm-model` because they derive from the AGPL tree ([D-031]). `Pbkdf2` is the default — `latestHasher` is `DefaultPBKDF2()` and `user_store.go:180` is the only `PreSave` caller. `BCrypt` is ported anyway: `GetHasherFromPHCString` still routes non-PHC rows to it. Both are pinned **byte-for-byte against Go's own output** — the tests decode Go's salt back out and recompute the whole stored string, which is exact rather than structural because both algorithms are deterministic given a salt. Two traps measured: bcrypt **truncates at 72 bytes** and `x/crypto` accepts a 73-byte password where the `hashers` package rejects it (a port matching the crate would authenticate a login Go denies); and the 72-byte cap is bcrypt's, inherited by PBKDF2 for no algorithmic reason. Deferred: the verification half ([D-109]) and FIPS ([D-110]). |
| — (tooling) | `reference/dump/behaviour_password.go` → `fixtures/behaviour_password.json` | DONE | 14 diff tests | The first oracle built from the **AGPL** half of the Go tree ([D-112]). Seven passwords — empty, ASCII, a phrase, multi-byte, an embedded NUL, and both sides of the 72-byte cap — each with the hash Go produced, plus the format decomposition of both hashers and the truncation asymmetry. The hashes are **literals** because both hashers salt randomly, so the generator **re-verifies every one against Go before writing**: a mistyped character fails the generator, not a downstream test. |
| channels/app/password/phcparser/parser.go | `mm-app/src/password/phcparser.rs` | DONE | 13 pass | Whole file. 434 lines of hand-written state machine, and **five readings of it were wrong**. A bcrypt hash **does not parse — and that is the hasher-detection mechanism**, not a bug. The first parameter name is checked against a **wider** character class than every later one, because it is scanned as `B64ENCODED` before the parser knows whether it is a name or a salt. `v` means three different things in three positions. A NUL byte is **swallowed** — `read` returns the `eof` sentinel, which *is* `rune(0)` — so `$x\0$a=1` parses as `$x$a=1`. And `parseToken` **discards the literal** on failure, so most error messages say `found ""` rather than naming the character. Error text reproduced exactly. |
| channels/app/password/hashers (verify) | `mm-app/src/password/` | DONE | 51 pass | **Closes [D-109].** Both `CompareHashAndPassword`s, both `IsPHCValid`s, `NewPBKDF2FromPHC`, `GetHasherFromPHCString`, `IsLatestHasher`. The router is the content: a **parse failure is not an error**, it is how a legacy bcrypt row is recognised, and `""` and `"not a hash at all"` route there too; an **unknown function id routes to bcrypt even when the string parses cleanly**, discarding the parsed PHC; but `$pbkdf2` with unusable parameters is a **hard error**. `IsPHCValid` compares parameters as **text**, so `w=0600000` is not a match for `w=600000`. `hashes_go_wrote_verify_here` is the test that matters — every hash the Go package produced verifies here. Deferred: `App.migratePassword` (needs the login route), FIPS ([D-110]). |
| — (correction) | `mm-model/src/user.rs::PasswordHashError` | **FIXED** | 2 pass | `hashers.ErrPasswordTooLong` is `fmt.Errorf("hashers: %w", model.ErrPasswordTooLong)` — the hasher hands `PreSave` the **wrapped** error. The flat `TooLong` variant [D-108] shipped could reproduce the text or the `errors.Is` behaviour but not both, so the 400's `detailed_error` was missing its prefix. Now a `Wrapped` variant plus `is_too_long()` walking the chain; `pre_save` branches on the predicate. Had it kept matching the bare variant against a wrapping hasher, **every too-long password would have taken the 500 branch**. |
| — (tooling) | `reference/dump/behaviour_phcparser.go` → `fixtures/behaviour_phcparser.json` | DONE | 13 diff tests | A 64-case corpus recording every parsed field **and every error string**, a 134-codepoint sweep driven through all four grammar positions (536 probes) via `Parse` rather than against the predicates, and a 24-row walk of the 256-byte boundary against three multi-byte pad widths. That last section exists because two earlier drafts of the byte-vs-rune claim were **wrong and green**; see [D-109]. |
| — (tooling) | `reference/dump/behaviour_password.go` (verify) → `fixtures/behaviour_password.json` | DONE | 7 diff tests | Extended with 36 compare verdicts (six hashes x six candidate passwords, through **both** hashers and through the *package* rather than `x/crypto`), 8 `IsPHCValid` parameter sets and 8 router inputs. The `exactly_72`/`appended` row is the one that matters: the package reports **too long** where the primitive would report a match. |
| model/authorize.go | `mm-model/src/authorize.rs` | DONE | 19 pass | **Whole file** — both wire types, both `IsValid`s, `PreSave`, `IsExpired`, the PKCE family and `ValidateResourceParameter`. The OAuth authorization-code surface, so a branch translated wrongly is an auth bypass; nine mutations were run against the finished port and all nine failed the suite. Four findings a reading gets wrong: **`AuthorizeRequest::is_valid` reports `AuthData.IsValid`** on all five branches it owns while its two delegated branches name the right type ([D-114]); **`IsExpired` multiplies in int32 and wraps**, so `i32::MAX` seconds gives a threshold one second *before* `CreateAt` ([D-115]); **`VerifyPKCE` returns `true` when no PKCE was stored**, accepting any verifier, which only `ValidatePKCEForClientType` contains; and **a trailing `#` is not a fragment**, so `https://x/#` passes RFC 8707 and `https://x/#f` does not. `expires_in` is guarded `== 0`, not `<= 0`, so a negative expiry validates. All five caps count **bytes**. One divergence ([D-113], the constant-time compare). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_authorize.go` → `fixtures/behaviour_authorize.json` | DONE | 16 diff tests | 42 `AuthData::IsValid` cases and 17 `AuthorizeRequest` cases asserting id, `Where`, status **and** detail; 16 `VerifyPKCE` inputs; 16 `ValidatePKCEForClientType` cases run under **both** client types; 18 resource URIs; and both PKCE charsets swept over 134 codepoints **through the public API** rather than against the regexes, so the length and method checks in front of them are measured too. `IsExpired` is recorded as **arithmetic** — the wrapped product beside the widened one — because the function reads a clock. Plus `fixtures/auth_data.json` and `authorize_request.json`. |
| model/view.go | `mm-model/src/view.rs` | DONE | 28 pass | **Whole file** — seven wire types, `IsValid`, `PreSave`, `PreUpdate`, `Patch`, `Clone`, `Auditable`, the kanban props codec and both unexported validators. **Not** a rename of `channel_view.go`, which is the mark-channel-read request. The content is `KanbanPropsFromProps`: it round-trips through JSON, so a malformed props map fails with **`encoding/json`'s own error text**, which lands in `detailed_error` on the wire and names **Go type names** (`KanbanGroupBy.group_by.columns of type []model.KanbanColumn`). Reproduced with a hand-written decoder rather than serde, because serde's text is entirely different — 14 message shapes pinned. Four more findings: a kanban view can **never** validate without props; nil and empty props give **different error ids** that the wire cannot distinguish ([D-117]); the title's length is counted **before** trimming and its emptiness **after**; and the three per-column errors carry `{"Index": i}` in `AppError.params`, which is **unexported in Go** and only a reflective read can check. One divergence ([D-116], the shallow clone). Eight mutations run against the finished port, eight caught. Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_view.go` → `fixtures/behaviour_view.json` | DONE | 11 diff tests | 45 `IsValid` cases recording id, `Where`, status, detail **and the unexported params**; 27 props documents through `KanbanPropsFromProps` with Go's exact error string; 5 `ToProps` shapes; `PreSave`/`Patch`/`Clone`/`Auditable`; a 140-codepoint `TrimSpace` sweep; and 13 wire probes. The params are recovered with `reflect` + `unsafe` — `AppError.params` is unexported, so nothing outside Go's model package can see it, and without that read nothing would catch the port using the wrong column index. Plus six wire fixtures from the reflection populator. |
| model/job.go | `mm-model/src/job.rs` | DONE | 17 pass | Whole file except the YAML pair ([D-119]). The content is a **counting** result: job.go declares **42** `JobType*` constants and `AllJobTypes` lists **24**, so `IsValidJobType` rejects **eighteen declared job types** — `recap`, `push_proxy_auth`, every `*_notify_admin`, all four migration jobs ([D-120]). Enumerated from Go by Go identifier, never transcribed. Two more: **`IsValid` never calls `IsValidJobType`**, so a job carrying one of the eighteen stores fine and only the (unported) scheduler turns it away; and `IsValidStatusChange` permits **4 of 81** pairs, including `in_progress → pending`, which reads like a bug and is how a worker requeues. `Auditable` **includes the payload**, unlike `View`'s, with an upstream `// TODO` beside it. Deferred: the YAML codec ([D-119]), `Worker` (needs `*Config`). |
| model/permission.go | `mm-model/src/permission.rs` + `permission_generated.rs` | DONE | 13 pass | **Generated, not hand-translated** — the 311 permissions and seven tables come out of `reference/dump/permission_gen.go`, which reads the literals from `initializePermissions` with `go/parser`, reads the tables from the linked package, and fails unless the two agree field-for-field. The generator needs both halves because Go has no reflection over package-level vars, so the **identifier** each permission is declared under — what all 674 `SessionHasPermission*` call sites name — is only in the source. Hand-written half: the `Permission` type, the six scope constants, `MakePermissionError`/`ForUser`, and the moderated-permission lookup. The content is a **naming** result: fourteen ids do not match their Go identifier, and six of those transpose the words (`PermissionPublicPlaybookCreate` is `playbook_public_create`), so the Rust statics are named from the **id**. Also measured: the 311 declared permissions partition exactly into `AllPermissions` (282) and `DeprecatedPermissions` (29) with no overlap and no orphans — **not** the job.go shape ([D-120]). Ten mutations run, ten caught. Deferred: nothing. |
| model/role.go | `mm-model/src/role.rs` + `role_generated.rs` | DONE | 28 pass | Whole file except the YAML pair ([D-119]'s shape). Split the way permission.go was: the 740 data lines — 24 default roles, seven id/permission lists, the sysconsole ancillary map — are emitted by `role_gen.go`, the ~25 functions are hand-ported against `behaviour_role.json`. The content is a **nondeterminism** result: three functions build their answer by ranging a Go map, and the oracle calls each fifty times to establish that the order really does vary rather than assuming it ([D-125]). Ours sort; the two whose order Go *does* fix — `PermissionsChangedByPatch` and `MergeChannelHigherScopedPermissions` — are asserted in order. Four more findings: `CleanRoleNames` **drops** a blank entry but **rejects** `" system_user "**, and on failure returns the *original* slice; `BuiltInSchemeManagedRoleIDs` is a misnomer, 11 of its 24 are not scheme-managed; `IsValidRoleName`'s `TrimLeft` is a **cutset**, so it is an all-characters test, not a prefix one; and `AddAncillaryPermissions` ranges the original slice header while appending to it, so expansion is one level deep. Two divergences ([D-126] the `*[]string` patch state, [D-127] the two unguarded dereferences that panic in Go). Seventeen mutations run: 13 caught, 4 survived — one intended no-op control, two provably equivalent (swapping the disjoint empty/length display-name branches; trimming a name that cannot contain whitespace), and one **unobservable**: whether `AddAncillaryPermissions` expands one level or many cannot be distinguished, because no sysconsole key's ancillary permission is itself a sysconsole key. Deferred: `MarshalYAML`/`UnmarshalYAML` ([D-128]). |
| model/scheme.go | `mm-model/src/scheme.rs` | DONE | 20 pass | Whole file except the YAML pair ([D-128]). Five wire types, `IsValid`/`IsValidForCreate`, `Patch`, `Sanitize`, four `Auditable`s, `SchemeConveyor::Scheme` and `IsValidSchemeName`. **This completes the model layer of [D-094].** The content is `IsValidForCreate`'s scope asymmetry, measured on a **203-cell grid** (7 scopes × 29 single-field mutations) rather than read off the switch: the three **channel** roles are required under *every* scope; the team, playbook and run roles are validated **only** under `team`, so a `playbook`-scoped scheme may carry an empty or malformed `default_playbook_admin_role`; and `channel` additionally requires the three team roles to be **empty** while permitting any playbook or run role. Three smaller ones: a scheme name needs **two** characters where a role name needs one (the two rules differ at exactly that input); Go's `$` is end-of-text, so `"ab\n"` is rejected and a PCRE-flavoured engine would accept it; and `SchemeRoles::Auditable` returns an **empty map**, so none of its three booleans reaches the audit log. `Scheme::Sanitize` blanks the **name** as well, where `Role::Sanitize` leaves it — asserted together so the asymmetry cannot be tidied away. Fourteen mutations run, fourteen caught. Deferred: the YAML codec ([D-128]). |
| sqlstore/role_store.go, scheme_store.go | `mm-store/src/role_store.rs`, `scheme_store.rs` | PARTIAL | 25 pass (6 DB-backed) | **Reads only** — `Get`/`GetAll`/`GetByName`/`GetByNames` and `Get`/`GetByName`/`GetAllPage`/`CountByScope`. The content is `Roles.Permissions`: it is **one text column**, written with a leading space per entry and read back through `strings.Fields`, so the write and read shapes are not symmetric and an empty column reads as `[]` rather than `null`. **None of the four role read paths filters `DeleteAt`** — `Delete` only stamps it and a permission check still has to resolve the role — while the scheme paths disagree with each other: `Get`/`GetByName` return a deleted scheme, `GetAllPage`/`CountByScope` do not. `GetAllPage` treats an empty scope as a wildcard; `CountByScope` treats it as a literal, so it counts nothing. Thirteen mutations run, twelve caught, one behaviourally equivalent (`GetByNames`' empty short circuit saves a round trip, nothing else). `ChannelHigherScopedPermissions` landed too — three UNIONed branches, exercised by a DB fixture that builds the whole graph (team scheme, two channel schemes, two teams, two channels), since none of the three fires against an empty `Schemes` table. Its `IN` list is **parameterised**, where Go interpolates it into the SQL text ([D-133]), and both of its upstream quirks are reproduced: it splits the permission column with `Split(" ")` where every other read uses `Fields`, so the list begins with an empty string, and the result map gains a `""` key ([D-132]). Seven more mutations, seven caught. Found [D-130]: the container runs **11.10.0** while the reference tree is **11.11.0**. Deferred: every write path, `AllChannelSchemeRoles`, `ChannelRolesUnderTeamRole`, `CountWithoutPermission` ([D-131]). |
| app/authorization.go, app/role.go | `mm-app/src/authorization.rs` | PARTIAL | 11 pass (6 DB-backed) | **[D-094]'s checker, and the point of the last four sessions.** `RolesGrantPermission`, `SessionHasPermissionTo`/`ToAny`/`ToTeam`/`ToUser`, and `GetRolesByNames` with `mergeChannelHigherScopedPermissions` behind it — so model, store and app now meet. The load-bearing line is that a **lookup failure denies** (authorization.go:386): a database blip must not grant, which a test asserts by pointing the store at an unreachable database. `SessionHasPermissionToUser`'s five branches are not in intuitive order — an empty target denies even for an unrestricted session, `manage_system` grants **before** the self check, and a **system-admin target denies** even to a holder of `edit_other_users`, which no built-in role separates, so the test makes a role that does. Eleven mutations run, eleven caught — but only after the pass **found two places where the suite was asserting less than it looked like it was**, which is the reason to run one at all. `edit_other_users` could be deleted outright with nothing failing, because branch 4 is masked by branch 5 for the only case the test exercised (a permission-less session acting on an *admin*, whom branch 5 denies anyway); and the higher-scoped merge could be skipped entirely, because with an empty `Schemes` table merging and not merging are the same operation. Both are now covered — a permission-less session against an **ordinary** target, and a test that builds the scheme graph so the merge is observable. Deferred: the channel/post/group/category/bot/property variants and the `askingUserId` family ([D-134]). **Superseded 2026-08-21** — the post variants and the whole `askingUserId` family landed; see the authorization row at the top of this table. |
| utils/timeutils/time.go | `mm-model/src/timeutils.rs` | DONE | 10 pass | 29 lines, ported alongside `job.go` because `Job`'s YAML codec is its only model-package consumer. Two traps, both measured: `time.UnixMilli` attaches `time.Local`, so **the offset in the output is the server's** — [D-008] in a second place; and Go's `.999` **elides trailing zeros and the decimal point**, so a whole second formats with no fraction at all (`.1`, `.01`, and nothing for `.000`). Neither `%.3f` nor `%.f` is substitutable. The round trip is **not total**: a five-digit year formats and will not parse back, though only at a non-zero offset. Error *text* not reproduced ([D-118]); verdict and value are exact. |
| — (tooling) | `reference/dump/behaviour_job.go` → `fixtures/behaviour_job.json` | DONE | 12 diff tests | All 42 job-type constants and all 7 statuses paired with their **Go identifiers**, so a swapped pair cannot pass a set comparison; `AllJobTypes` in order; 18 `IsValid` cases; the full **9×9** status-transition matrix, including statuses the switch never names; `Auditable` with and without a payload; and 14 millis rendered **twice** — once in the generator's zone and once at UTC. Plus `fixtures/job.json`. |
| — (tooling) | `reference/dump/timezones_gen.go` → `fixtures/behaviour_timezones.json` | DONE | 5 diff tests | The default map recorded three ways — value, Go's marshalled bytes, and the insertion order it does *not* marshal in — plus a two-call freshness probe, the three starting states of `PreSave`'s guard, and an order-sensitive digest over all 592 zones so the generated table is one assertion rather than 592. |
| — (hygiene) | `reference/dump/behaviour_user_is_valid.go`, `locale_gen.go` | **FIXED** | — | Two determinism leaks, both found by an unexplained `M` after a generator run. The `IsValid` corpus wrote [D-107]'s **heap address** into a committed fixture, so every run rewrote it — and the `auth_data_ptr` probe written specifically to record that properly had **no Rust test reading it**. Both fixed ([D-111]). And `locale_gen.go` emitted a trailing blank line that `cargo fmt` strips, so the generator and the formatter disagreed by one byte forever. |
| — (hygiene) | `reference/dump/main.go`, 11 × `mm-model/src/*.rs` | DONE | 945 pass | Two oracle-pipeline fixes and six decisions settled together. The generator now **pins its own timezone** rather than asking the operator to remember `TZ=Asia/Kolkata` — verified by running under `America/New_York` and getting a zero fixture diff, which closes [D-069]. And **37 call sites across 11 modules** were switched from `serde_json::to_string` to `go_json_marshal`; all tests still passed, because each was passing only for want of a `<` in its fixture ([D-027]). Decisions: port the locale table ([D-001]), forward rather than port `shared/markdown` ([D-044]) and OpenGraph ([D-105]), use RustCrypto for the OAuth crypto ([D-046]), accept the missing WebSocket event ([D-089]) and the untranslated error ids ([D-092]). The principle behind them is stated once at the top of `TECH_DEBT.md`: **reproduce what we can measure, forward what we cannot.** |
| model/link_metadata.go | `mm-model/src/link_metadata.rs` | PARTIAL | 10 pass | The half that does not need OpenGraph: the wire type, both time helpers, `IsSVGImageURL`, `PreSave` and the hash. Two traps pinned. `GenerateLinkMetadataHash` is **FNV-1, not FNV-1a** — `fnv.New32()` is 1, `New32a()` is 1a, almost every other ecosystem defaults to 1a, and this value is the table's **primary key**; the timestamp goes in little-endian *before* the URL and the result is a widened `uint32`, so it is never negative. `FloorToNearestHour` floors **downward**, so `-1` gives `-3_600_000` where truncating division gives `0` — `div_euclid`, not `/`. Also measured: `IsSVGImageURL` tests the **decoded** path only, so `a%2Esvg` **is** an SVG and `a.png?x=.svg` is not. No `json:` tags, PascalCase keys, `URL` not `Url` — third instance of the `wrangler.go` shape. Deferred: the OpenGraph surface ([D-105]) and, with it, `IsValid`, whose Go implementation is a **type assertion** a `Value` cannot answer ([D-106]). |
| — (tooling) | `reference/dump/behaviour_link_metadata.go` → `fixtures/behaviour_link_metadata.json` | DONE | 10 diff tests | 12 hash probes including pre-epoch and `int64::MAX` timestamps, 10 floor cases with three negatives, 16 `IsSVGImageURL` inputs, 13 `IsValid` cases **recorded for whoever ports it** though not yet asserted, and 3 wire probes. |
| model/product_notices.go | `mm-model/src/product_notices.rs` | DONE | 17 pass | Whole file: eight wire types, five defined string types and all fifteen functions. The content is that **the four `Matches` methods disagree about unknown values** ([D-102]) — an unrecognised audience returns `false` and hides a notice, an unrecognised instance type returns `true` and shows one, measured both ways. Two smaller traps pinned: `NoticeSKU`'s `e0`/`team` match the **empty string** rather than their own names, and `mobile` matches `mobile-ios` while `mobile-ios` does **not** match `mobile`. `NoticeClientTypeFromString` **rejects two of its own constants** and returns `all` alongside the error ([D-103]). `NoticeMessage` embeds `NoticeMessageInternal`, so `Serialize` is hand-written — [D-067] a second time ([D-104]). `ProductNoticeViewState` has **no tags at all**, PascalCase keys, third instance of the `wrangler.go` shape. |
| — (tooling) | `reference/dump/behaviour_product_notices.go` → `fixtures/behaviour_product_notices.json` | DONE | 14 diff tests | 28 audience cases, 64 client-type pairs, 10 instance cases, 49 SKU pairs — every value crossed with every other, known and unknown, because the four fallbacks differ. Plus 11 `FromString` inputs, 6 admin-only cases, 4 `recover`-probed nil receivers and 13 wire probes. The wire test goes through `go_json_marshal`: the corpus holds semver ranges like `">=1.2.3"`, so it reaches [D-022]'s HTML escaping where a tidier document would not. |
| model/oauth_dcr.go | `mm-model/src/oauth_dcr.rs` | DONE | 14 pass | Whole file, and it is mostly **redirect-URI allowlist matching** — where open-redirect bugs live. Four properties measured rather than read: an **empty allowlist permits everything** while a list of blanks denies everything; the matcher compares **bytes**, so `*` consumes both bytes of `é`; `*` stops at `/` and `**` does not, enforced **per URL component**, so a host wildcard cannot satisfy a path; and pattern validation substitutes `*`/`**` with the digit `1` before parsing, so `localhost:*` validates as `localhost:1`. `ClientURI` is checked format-**before**-length, so a malformed over-long value reports format. 44 hand-written glob probes, 25 pattern probes, 11 allowlist probes, **plus a systematic 3,240-pair sweep** crossing 45 URIs against 72 patterns — the treatment [D-003] gave `IsValidHTTPURL`, which [D-101] asked for and which landed the same day. 312 of the 3,240 match, so the corpus is not degenerate. Deferred: nothing. |
| model/oauth.go | `mm-model/src/oauth.rs` | PARTIAL | 16 pass | Three wire types and twelve of fourteen functions: complete `IsValid` (every branch, both `IsDynamicallyRegistered` exemptions, all error ids **and** the two branches that omit `app_id=` from the detail), `PreSave`/`PreUpdate`/`Etag`/`Sanitize`, `IsValidRedirectURL`, the auth-method pair, `ValidateForGrantType` with both private validators, and `Auditable`. Three oddities reproduced and measured ([D-100]): the callback cap measures **Go's slice rendering** `[a b c]`, not the URLs — one 28-byte URL renders to 30 bytes; `Name` is capped in **bytes** while `Description` is capped in **runes** in the same function; and `Auditable` emits `"callback_urls:"` with a trailing colon. The confidential-secret comparison uses the `subtle` crate, a **new workspace dependency**, because Go uses `crypto/subtle.ConstantTimeCompare` and that is a security property. **[D-099] now closed** — both DCR bridge functions landed once `oauth_dcr.go` existed. Two more findings: `NewOAuthAppFromClientRegistration` mints a secret whenever the auth method is `!= none`, so a `client_secret_basic` request that `IsValid` **rejects** still becomes a confidential client if it reaches the function; and `ToClientRegistrationResponse` takes a `siteURL` it **never reads**, confirmed by calling it with two different values. Borrows five constants from `access.go`/`oauth_metadata.go`. |
| — (tooling) | `reference/dump/behaviour_oauth.go` → `fixtures/behaviour_oauth.json` | DONE | 13 diff tests | 30 `IsValid` cases recording the error id **and** the detailed error, a 7-corpus `%s` rendering section that records the rendered string, its length **and** the naive sum for contrast, 10 grant-validation cases covering the constant-time compare's length behaviour, 6 redirect-URL probes, and 7 wire probes. Plus `oauth_app.json`, `oauth_app_request.json` and `intune_login_request.json`. |
| model/audit_record.go | `mm-model/src/audit_record.rs` | DONE | 12 pass | Whole file: five wire types, the `Auditable` **trait** (implemented for `Bot`/`BotPatch`), all twelve functions and every constant. Three findings, all measured: the field holding the event data is tagged **`event`** while `AuditKeyEventData` three lines above says `event_data`, so trusting the constant emits a key nothing reads; `AddMeta` **panics** on a zero-valued record where all four sibling adders lazily create their map ([D-097]); and `EventMeta` is declared, tagged and **never used** by `AuditRecord`, whose `meta` is an open `map[string]any`. `AddAppError` stores `err.Error()` — the formatted string with `Where` and detail — not `err.Message`. One further divergence ([D-098], the widened generic). |
| — (tooling) | `reference/dump/behaviour_audit_record.go` → `fixtures/behaviour_audit_record.json` | DONE | 10 diff tests | Six nil-map probes run side by side, which is the only way the `AddMeta` asymmetry is visible; five reflection-read tag lists; 8 wire probes covering nil-vs-empty maps and both `omitempty` halves of `AuditEventError`; the six-type parameter corpus; and `AddAppError` recorded alongside both `Error()` and `Message` so the port cannot pick the wrong one. Plus `fixtures/audit_record.json` and `event_meta.json`. |
| model/bot.go | `mm-model/src/bot.rs` | DONE | 28 pass | Whole file: `Bot`, `BotPatch`, `BotGetOptions`, `BotList`, both `IsValid`s, `PreSave`/`PreUpdate`, both etags, `Patch`/`WouldPatch`, `UserFromBot`/`BotFromUser`, `IsBotDMChannel`, `MakeBotNotFoundError` — **including `Auditable`**, unlike the [D-028] types. Two upstream bugs reproduced and pinned ([D-096]): `IsValidCreate` reports `…user_id.app_error` for an over-long **display name** (a copy-paste of the line above; no `display_name` id exists in the tree), and `BotList::Etag` passes a `delta` that is declared and never assigned, so every list etag carries a literal `0`. Both **measured from Go**, not read. `BotFromUser`'s `DisplayName` is the **username** via `GetDisplayName(ShowUsername)` — not the full name. One divergence ([D-095], the nil-patch panic). Borrows two constants from unported files. |
| — (tooling) | `reference/dump/behaviour_bot.go` → `fixtures/behaviour_bot.json` | DONE | 13 diff tests | 7 `IsValid` cases, 13 `IsValidCreate` cases asserting the id per input, 6 list-etag corpora, 8 patch/would-patch pairs with a `recover`-probed nil, 3 conversions each way, 8 `IsBotDMChannel` inputs and 6 byte-exact wire probes. Also `fixtures/bot.json` and `bot_patch.json` from the reflection populator. The `IsValidCreate` section is what turned "that error id looks wrong" into a measurement. |
| model/audit.go + audits.go | `mm-model/src/audit.rs` | DONE | 11 pass | **Both whole**, in one module. `Audit` is a plain seven-key struct; `Audits` is where the content is, and its `Etag` is **unlike every other list etag in the tree**: an empty list gives `""` rather than a versioned string, it reads element `[0]` instead of scanning for a maximum, and it emits one component rather than four. So an ascending list etags to its *oldest* row — measured, not read. `Audits` is `[]Audit`, the first **value**-element slice in the tree, so no [D-033]; `[null]` gives Go a zero-valued `Audit` instead, which widened [D-075]. One new entry ([D-076]). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_audit.go` → `fixtures/behaviour_audit.json` | DONE | 4 diff tests | The key list by reflection, 11 wire probes, 10 etag cases recording `first_create_at` and `max_create_at` **separately** — which is what makes "reads position, not recency" assertable — and a null-element probe that also records whether the element type is a pointer. |
| model/user_autocomplete.go | `mm-model/src/user_autocomplete.rs` | DONE | 12 pass | Whole file: three structs, six `[]*User` fields, no methods. The types are identical and the **tags** are not — `out_of_channel` appears in two of the three structs under **different rules**, with `omitempty` in one and without it in the other. That decides the Rust type per field: no `omitempty` → `Option<Vec<User>>` (nil is `null`, empty is `[]`), `omitempty` → `Vec<User>` with a length predicate, because Go drops nil and empty alike and an `Option` would invent a distinction. Measured byte-identical, not reasoned. Wire format asserted **byte-for-byte** over 17 documents. Six new instances of [D-033], one per field. Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_user_autocomplete.go` → `fixtures/behaviour_user_autocomplete.json` | DONE | 6 diff tests | All three key lists by reflection, 17 wire probes recording **key presence** and null-ness separately from value — which is the only way `omitempty`'s collapse is assertable — and six nil-element probes, one per field. |
| model/emoji_search.go + user_access_token_search.go | `mm-model/src/search_requests.rs` | DONE | 7 pass | **Both whole**, in one module — three fields between them, and neither justifies a session or an oracle alone. Snake_case tags, no `omitempty`, no methods, no validation: **nothing unusual, which is the finding**, recorded with evidence rather than asserted. `UserAccessTokenSearch` has one field, so its zero value is `{"term":""}` and not `{}`. Four divergences, all standing ([D-057] ×2, [D-040], [D-071]), and the parity test asserts the **count** so a new one cannot quietly join the exemption list. Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_search_requests.go` → `fixtures/behaviour_search_requests.json` | DONE | 3 diff tests | Both key lists by reflection, 9 wire probes, 10 decode shapes. Deliberately short — a corpus padded to look proportionate would imply surface these types do not have. |
| model/limits.go | `mm-model/src/limits.rs` | DONE | 10 pass | Whole file: `ServerLimits`, seven `int64` fields, no methods. Every key is **camelCase** — the third naming convention in the tree after snake_case and tagless PascalCase — so the keys are read off the Go struct tags by reflection and compared in order, because a mis-tagged field round-trips cleanly through its own serializer and only that comparison catches it. Zero is a documented **sentinel** on four fields and nothing carries `omitempty`, so it survives the wire. Wire format asserted **byte-for-byte** over 14 documents, seven of them single-field probes so a swapped tag cannot pass. Two divergences, both standing ([D-057], [D-071]). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_limits.go` → `fixtures/behaviour_limits.json` | DONE | 4 diff tests | The seven keys by reflection, 14 wire probes including one per field in isolation, seven spellings of one camelCase key, and 13 decode shapes. The casing sweep is what widened [D-040] a third time. |
| model/channel_search.go | `mm-model/src/channel_search.rs` | DONE | 13 pass | Whole file: the constant and all eighteen fields. `Page`/`PerPage` are `*int` **with** `omitempty` — the first fields in the tree that are both nillable and droppable, so absent / `Some(0)` / `Some(n)` are three distinct documents and a pointer-to-zero is **not** dropped. `TeamIds` has no `omitempty` three lines above them, so it takes the opposite convention in the same struct. `int` cited from [D-074] rather than re-swept. Wire format asserted **byte-for-byte** over 23 documents. Three divergences, one **new** ([D-075], `null` inside a `[]string`) and two standing ([D-057] into a bool, [D-040] the folded key). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_channel_search.go` → `fixtures/behaviour_channel_search.json` | DONE | 6 diff tests | The 18 keys by reflection, 13 wire probes, 10 pointer probes recording **key presence** separately from nil-ness and value — which is what makes the `omitempty` three-way assertable — and 20 decode shapes. |
| model/team_stats.go + users_stats.go + cluster_stats.go | `mm-model/src/stats.rs` | DONE | 9 pass | **All three whole**, in one module — 26 lines of Go, eight tagged fields, no methods. `ClusterStats` uses bare **`int`** where the other two use `int64`, the first in the tree; `i64` is a **measurement** rather than a habit, because the oracle records `strconv.IntSize` and drives eleven bounds through an `int` field and an `int64` field side by side, which agree on all eleven ([D-074]). Wire format asserted **byte-for-byte** over 17 documents. Five divergences across three standing entries ([D-057] ×2, [D-040] ×2, [D-071] ×1). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_stats.go` → `fixtures/behaviour_stats.json` | DONE | 4 diff tests | `strconv.IntSize`, 17 wire probes across the three types, 11 numeric bounds through **both** an `int` and an `int64` field with an `agree` flag per case, and 15 decode shapes. The `agree` column is the file — it is what turns "Go's `int` is 64-bit here" from a premise into a recorded fact. |
| model/analytics_row.go | `mm-model/src/analytics_row.rs` | DONE | 14 pass | Whole file. Eleven lines, and the **first `float64` on the wire in the crate** — which is the entire content. serde_json renders `1.0` where Go renders `1`, disagreeing on **12 of 29** measured values, so `Serialize` is hand-written and the number goes out through `utils::go_json_format_float` as a `RawValue`. `NaN`/`±Inf` are an **error** in Go, not `null`, and one bad row loses every good row in the slice — reproduced, so serialization is fallible for this type alone. Wire format asserted **byte-for-byte**. Two divergences ([D-057] `null` into the float, [D-033] a nil element). Deferred: nothing. |
| — (shared) | `mm-model/src/utils.rs::go_json_format_float` | DONE | 29 cases | `encoding/json`'s float encoder — the **third** float rendering in the crate and not substitutable by either of the others. Thresholds are `1e-6`/`1e21`, not `%g`'s `1e-4`/`1e6`, and a negative exponent loses one leading zero (`1e-7`) where a positive one keeps it (`1e+21`). See [D-073]. |
| — (tooling) | `reference/dump/behaviour_analytics_row.go` → `fixtures/behaviour_analytics_row.json` | DONE | 5 diff tests | 29 floats recorded in **all three** renderings side by side plus their raw bits, 20 decode shapes, 7 row probes, 6 slice probes, and the three unsupported values at all three nesting levels with Go's exact error text. Floats are reconstructed in Rust from the bits, not by parsing Go's decimal — two distinct floats can print the same. |
| model/channel_member_history.go + `_result.go` | `mm-model/src/channel_member_history.rs` | DONE | 11 pass | **Both files whole**, in one module — 29 lines of Go, the same four fields plus four more, and splitting them would duplicate the whole oracle. **Not one `json:` tag between them**, so every wire key is the Go field name in PascalCase; second instance of the `wrangler.go` shape. `UserEmail` carries `db:"Email"` and no json tag, so its wire key is `UserEmail` and its column is `Email` — the one visible tag is the wrong one to copy. `LeaveTime` is `*int64` without `omitempty`, so `Option<i64>` carries three states. Wire format asserted **byte-for-byte** over 13 documents. One divergence ([D-040], at its widest). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_channel_member_history.go` → `fixtures/behaviour_channel_member_history.json` | DONE | 5 diff tests | Both key lists read off the Go struct tags with reflection, 13 wire probes marshalled from Go **values**, 10 shapes through the nillable `LeaveTime`, and 6 spellings of one key through Go's case-insensitive fallback — which is what bounded [D-040]. |
| model/channel_data.go | `mm-model/src/channel_data.rs` | DONE | 10 pass | Whole file — two nillable pointers and one `Etag`. That method **guards `Member` and dereferences `Channel` three lines later**, so a nil member yields `0` and a nil channel crashes Go; ours answers with the zero-channel etag, a value the oracle measures rather than one we chose ([D-072]). Only four fields reach the etag — three from the channel, one from the member — so nine real changes leave it byte-identical and a client will not refetch. Wire format asserted **byte-for-byte** over 6 documents. One divergence ([D-072]). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_channel_data.go` → `fixtures/behaviour_channel_data.json` | DONE | 3 diff tests | 6 wire probes, 11 `Etag` cases each `recover`-probed (3 of Go's answers are a crash, and each records *which* pointer was nil), and 14 single-field mutations against a fixed baseline recording whether the etag moved. The corpus is built from Go **values and marshalled**, not written as JSON literals — see the note. |
| model/channel_view.go | `mm-model/src/channel_view.rs` | DONE | 11 pass | Whole file — two structs, no methods, no constructor. **Nothing has `omitempty`**, so both zero values are full objects. `last_viewed_at_times` is the first bare `map[string]int64` in the tree: `Option<BTreeMap<String, i64>>`, because nil and empty differ on the wire and Go sorts map keys by byte value. Wire format asserted **byte-for-byte** over 35 documents. Four divergences, three of them standing instances ([D-057] `null` into three scalars *and* into a map value, [D-040] the uppercase key) and one **new** ([D-071], a repeated struct field). Deferred: nothing. |
| — (tooling) | `reference/dump/behaviour_channel_view.go` → `fixtures/behaviour_channel_view.json` | DONE | 5 diff tests | 25 wire probes, 14 values in the map's **value** position — the one `file.go`'s duration corpus could not reach — 7 bool shapes, and 10 maps built in Go rather than decoded, so the recorded key order is Go's own and not an echo of the input. |
| model/unicode.go | `mm-model/src/unicode.rs` | DONE | 14 pass | Whole file — one function, `ContainsCJK`, and no types. The function is four lines and the content is four Unicode **script** tables, which Rust's std cannot answer and `unicode-general-category` answers a different question about, so the ranges are **generated** from Go into `unicode_generated.rs` rather than transcribed or taken from a crate. `RangeTable` carries a **stride** and three of the four tables use it, so a range is not an interval — reading the four stride entries as solid would admit 331 codepoints that are in none of the scripts. One caveat, not a divergence: the tables are the Go **toolchain's** Unicode 15.0.0, not the pinned tree's ([D-070]). Deferred: nothing. |
| model/unicode.go (Go stdlib tables) | `mm-model/src/unicode_generated.rs` | GENERATED | — | The 54 ranges of `unicode.{Han,Hiragana,Katakana,Hangul}` as `(lo, hi, stride)`, plus the Unicode version. Never hand-edit; re-run the generator. Carries `#[rustfmt::skip]` so `cargo fmt` and the generator stay idempotent against each other, the same as `emoji_generated.rs`. |
| — (tooling) | `reference/dump/behaviour_unicode.go` → `fixtures/behaviour_unicode.json` + `unicode_generated.rs` | DONE | 4 diff tests | The second generator that emits **Rust source**. 306 codepoint probes — every range edge from both sides, derived from the tables rather than hand-listed, plus a hand-picked set — each recording all four script verdicts *and* `ContainsCJK`, then 26 whole strings and the four tables entry for entry. Corrected three of its own annotations; see note 1. |
| model/file.go | `mm-model/src/file.rs` | DONE | 12 pass | Whole file: `MAX_IMAGE_SIZE`, `FileUploadResponse`, `PresignURLResponse`. Nineteen of its twenty lines are unremarkable and the twentieth is `Expiration time.Duration`, which goes on the wire as a bare **nanosecond** count — the only time-valued field in the crate that is not epoch milliseconds. Both plausible Rust types are wrong (`std::time::Duration` serialises as an object, `chrono::TimeDelta` has no serde impl), so it is a plain `i64`. Wire format asserted **byte-for-byte** over 19 documents plus a 13-value duration corpus. Three divergences, all instances rather than new ([D-057] `null` into `expiration`, [D-033] a nil `*FileInfo`, [D-040] `{"URL":…}`). Deferred: nothing. |
| model/file_info_search_results.go | `mm-model/src/file_info_search_results.rs` | DONE | 11 pass | Whole file: the matches alias, the wire type and `MakeFileInfoSearchResults`. It is `post_search_results.go` **minus the methods** — no `ToJSON`, `EncodeJSON`, `ForPlugin` or `Auditable` — so none of [D-054]'s three nil-embed panics has a counterpart and [D-028] gains no entry. Carries the same hand-written `Deserialize`: Go allocates the embedded `*FileInfoList` from **which keys are present**, and an explicitly-zero scalar allocates it just as an explicit `null` does. Wire format asserted **byte-for-byte** over 15 of the 17 corpus documents. Two divergences, both instances rather than new ([D-040] the uppercase key, now structural; [D-033] the nil map value). Deferred: nothing. |
| — (shared) | `mm-model/src/file_info_list.rs::FileInfoList::WIRE_KEYS` | DONE | 1 diff test | The five JSON keys the embed contributes, read off the Go struct tags by the oracle rather than transcribed. **All five fields**, unlike `PostList::WIRE_KEYS` — `PostList` has a `json:"-"` field that is promoted and yet an unknown key, and `FileInfoList` has none. |
| — (tooling) | `reference/dump/behaviour_file_info_search_results.go` → `fixtures/behaviour_file_info_search_results.json` | DONE | 4 diff tests | 17 wire probes recording the nil-ness of the embed *and* of `matches` alongside Go's bytes, 8 constructor cases, a 9-case matches corpus, and the promoted key set. The wire probes are the file: eight of the seventeen leave the embed nil, which is the state the hand-written `Deserialize` exists for. |
| — (tooling) | `reference/dump/behaviour_file.go` → `fixtures/behaviour_file.json` | DONE | 5 diff tests | 13 `time.Duration` values recorded as both the wire integer **and** `Duration.String()`, so a port that reaches for the human-readable form fails a test rather than a review; 17 raw JSON values decoded into the field with Go's accept/reject verdict and error text; 11 upload probes and 8 presign probes. |
| model/scheduled_post_recurrence.go | `mm-model/src/scheduled_post_recurrence.rs` | DONE | 18 pass | Whole file. Both `ScheduledPostRepeatType*` constants move here and `scheduled_post.rs` re-exports them — the last [D-005] borrow paid off by translating its owner. `ComputeNextScheduledAt` is the file, and it is calendar arithmetic in a named zone: `AddDate` keeps the **wall clock**, so the series is not `scheduled_at + 7n` days and crosses DST boundaries into local times that do not exist or exist twice. Three divergences ([D-065] widened for `"Local"` and the error text, [D-068] the unbounded loop). Deferred: nothing. |
| — (shared) | `mm-model/src/utils.rs::go_time::date_in_zone`, `::add_date_days` | DONE | 280 cases | Go's `time.Date` **normalisation** and `AddDate`. Go's doc declines to specify it ("the choice of time zone… is not guaranteed"), so the implementation is what is ported — reduced to two offset lookups, which the corpus proves exact. chrono's `LocalResult` is **not** substitutable and its obvious mapping is wrong in both arms; see the notes. |
| — (tooling) | `reference/dump/behaviour_scheduled_post_recurrence.go` → `fixtures/behaviour_scheduled_post_recurrence.json` | DONE | 8 diff tests | 280 `time.Date` probes and 295 `ComputeNextScheduledAt` cases, both **generated** rather than listed: the 16 DST transitions of ten zones are discovered by scanning the host's tzdata and bisecting, then every probe is placed relative to a discovered boundary. The transitions are written out as their own section so the Rust side asserts `chrono-tz` agrees with them *before* trusting an answer. Corrected one conclusion the Go source had produced — see note 2. |
| model/post_search_results.go | `mm-model/src/post_search_results.rs` | PARTIAL | 15 pass | Whole file except `Auditable` ([D-028]). `PostSearchResults`, `PostSearchMatches`, `MakePostSearchResults`, `ToJSON`, `EncodeJSON`, `ForPlugin`. Wire format asserted **byte-for-byte** over 18 of the 19 corpus documents. Carries a hand-written `Deserialize`: Go allocates the embedded `*PostList` from **which keys are present** and serde's `flatten` on an `Option` cannot express that. Two divergences ([D-054] three panics, [D-055] the shared `Matches` map) plus one instance of [D-040]. |
| store/sqlstore/session_store.go (`GetSessions`) | `mm-store/src/session_store.rs` | PARTIAL | 4 pass | `WHERE UserId = $1 ORDER BY LastActivityAt DESC`, plus **one** team-members query for the whole list rather than one per session — Go assigns the same members to every session (session_store.go:146). The row mapping is now a named `SessionRow` shared by both queries via `query_as!`, so `Get` and `GetSessions` cannot drift apart. |
| api4/user.go (`getSessions`) | `mm-api/src/sessions.rs` | DONE | 4 pass + 5 parity | Widened from `me`-only to any `{user_id}` behind `SessionHasPermissionToUser` (`edit_other_users`), evaluated **before** the `Sessions` read — so a plain caller gets the same 403 for another user and for nobody, and an admin gets `[]` for nobody. `/users/me/sessions` bytes unchanged (`parity_users_me_sessions` still pins them); `json.Marshal`, no newline ([D-086]). |
| app/session.go (`GetSessions`) | `mm-app/src/session.rs` | PARTIAL | 3 pass | Blunt by design: *any* store failure is `app.session.get_sessions.app_error` with a 500. Go has no not-found branch here, because a user with no sessions is an empty list rather than a miss. |
| — (tooling) | `crates/mm-api/tests/common/mod.rs` | DONE | — | Shared parity-test plumbing. Compiled into each test binary separately, so the login `OnceCell` is per-binary. Replaced three hand-rolled copies of the login helper. |
| app/team.go (`GetTeamMembersForUser`) | `mm-app/src/team.rs` | PARTIAL | — | A thin wrapper over the already-ported `TeamStore::GetTeamsForUser`; any store failure is `app.team.get_members.app_error` with a 500, and Go has no not-found branch. |
| api4/team.go (`getTeamMembersForUser`, `me`) | `mm-api/src/teams.rs` | PARTIAL | 4 pass | **The first route migrated *past* a permission check rather than around one.** Go guards `SanitizeRoleData` with `SessionHasPermissionToTeam`, which is unported — but that sanitiser is a **no-op when `UserId == currentUserId`** (team_member.go:147) and this route returns the caller's own memberships, so the guard cannot change the output. The sanitiser is therefore called unconditionally: provably identical to Go for `me`, and stricter rather than looser if the route is ever widened. Byte-identical against the running Go server, first try. |
| store/sqlstore/preference_store.go (`Save`) | `mm-store/src/preference_store.rs` | PARTIAL | 2 pass | **The first write in the port.** Go's exact upsert, `ON CONFLICT (userid, category, name) DO UPDATE`, inside a transaction — Go's comment is explicit that "if one fails, everything fails", and validation runs *inside* the loop, so a batch whose third entry is invalid must leave the first two unwritten. `save` (:65) and `saveTx` (:89) are byte-identical duplicates in the Go source; ported once. One divergence ([D-090], the clone). |
| app/preference.go (`UpdatePreferences`) | `mm-app/src/preference.rs` | PARTIAL | 4 pass | The ownership guard is the security boundary: any entry whose `user_id` differs from the path's is a **403 for the whole batch**, checked before the store is touched — without it, the upsert's `(UserId, Category, Name)` key would happily write preferences onto another account. Deferred: the sidebar sync ([D-091]) and the two WebSocket events ([D-089]). |
| api4/preference.go (`updatePreferences`, `me`) | `mm-api/src/preferences.rs` | PARTIAL | 4 pass | **The first write route.** Success body is `ReturnStatusOK` — `{"status":"OK"}` via `w.Write`, no newline. A batch containing any `flagged_post` entry is **forwarded to Go** rather than served, because that category needs a post lookup and a channel-read permission check we have not ported — the Strangler Fig applied *inside* a route. Both of Go's batch bounds reproduced, including that an empty batch is an error rather than a no-op. |
| store/sqlstore/preference_store.go (`GetAll`, `GetCategory`, `Get`) | `mm-store/src/preference_store.rs` | DONE | 2 DB tests | Go's `preferenceSelectQuery` with **no `ORDER BY`**, kept that way so both servers hand back the same order. `Value` is nullable in the schema but scanned as `string` in Go, so a NULL row fails the whole read — `"value!"` reproduces it. |
| app/preference.go (`GetPreferencesForUser`, `GetPreferenceByCategoryForUser`, `GetPreferenceByCategoryAndNameForUser`) | `mm-app/src/preference.rs` | DONE | 1 pass | The three reads answer "nothing there" three ways: `GetAll` empty is a 200 `null`, an empty category is the app layer's **404**, and a missing name is a **400** (`sql.ErrNoRows` wrapped as a plain error, not `ErrNotFound`). |
| api4/preference.go (`getPreferences`, `getPreferencesByCategory`, `getPreferenceByCategoryAndName`) | `mm-api/src/preferences.rs` | DONE | 5 pass + 5 parity | `{category}`/`{preference_name}` mux class is `[A-Za-z0-9_]+` (no hyphen) while `RequireCategory` is the format-strict lowercase pattern that *allows* a hyphen — so a hyphen is forwarded for Go's mux 404 and `Display_Settings` is our 400. `me` resolves before `RequireUserId`; `edit_other_users` gates before any read. |
| — (fix) | `mm-api/src/lib.rs::partially_migrated` | DONE | 1 pass | **Closes [D-093].** axum matches the path before the method, so registering `PUT` on `/users/me/preferences` made `GET` return 405 from our own router instead of reaching the proxy — silently breaking a working route by migrating a *different* method beside it. Every migrated path now carries `MethodRouter::fallback(forward_to_go)`. |
| — (fix) | `mm-api/src/error.rs::into_response` | DONE | — | Two thirds of [D-092] closed the moment error bodies were compared side by side. Go's response pipeline (`web/handlers.go:424-455`) sets `RequestId` and calls `WipeDetailed` unless developer mode is on — which **defaults to off**, so ours had been leaking `detailed_error` content Go withholds, and omitting `request_id` entirely. Only the i18n `Translate` step remains. |
| store/sqlstore/team_store.go (`GetTeamsForUser`) | `mm-store/src/team_store.rs` | PARTIAL | 9 pass | **Closes [D-077].** The wrapper is three lines; the content is `getTeamRoles` (team_store.go:100), which computes a member's **effective** roles from three booleans, three nullable scheme role names and whatever is already in the `Roles` column. The branch a reading would have missed: a scheme role id **in the `Roles` column sets its flag even when the column says false**, and is then excluded from `ExplicitRoles` — invisible in fresh data, where every `Roles` column is empty. `getTeamRoles` is unexported so `reference/dump` cannot reach it; verified instead by mutating the shared row and asking **both servers** the same question across six role shapes, all matching. The scheme-*derived* names stay provisional — `Schemes` is enterprise and the table is empty on Team Edition. |
| store/sqlstore/session_store.go (`Get`) | `mm-store/src/session_store.rs` | PARTIAL | 2 pass | **First `mm-store` content.** The Strangler Fig's load-bearing query: `Token = $1 OR Id = $1 LIMIT 1`, one bind parameter against two columns. Compile-time checked by sqlx against the real schema, which is how the v11-only `NOT NULL` on `VoipDeviceId` was found rather than assumed. Deferred: the `TeamMembers` second query ([D-077]) and the other 19 interface methods. Two divergences ([D-078] NULL defaulting, [D-079] token redaction). |
| store/sqlstore/user_store.go (`Get`) | `mm-store/src/user_store.rs` | PARTIAL | 1 pass | The `Users` LEFT JOIN `Bots` query, all 31 columns in Go's order. The join is not decoration: `is_bot` is `b.UserId IS NOT NULL`, so dropping it would make every bot a non-bot. Go's two `COALESCE`s are reproduced in SQL rather than defaulted Rust-side, so the database answers the same question for both servers. Deferred: the other ~100 interface methods. |
| app/session.go (`GetSession`) | `mm-app/src/session.rs` | PARTIAL | 3 pass | **First `mm-app` content.** Reproduces the `session.Token != token` check that stops a session **id** authenticating — the single most consequential line in the slice. Deferred: the session cache, the user-access-token path, and the idle timeout ([D-088]). |
| app/user.go (`GetUser`) | `mm-app/src/user.rs` | PARTIAL | 2 pass | Store-error mapping only: a miss is 404 `app.user.missing_account.error`, anything else is 500. Collapsing the two would report an outage as a missing account. |
| app/authentication.go (`ParseAuthTokenFromRequest`) | `mm-api/src/auth.rs` | PARTIAL | 10 pass | Four of six token locations. **The cookie is checked before the `Authorization` header** — reversing that would authenticate some requests as a different user than Go does. Go's 50-byte truncation of the *returned* value is reproduced; the char-boundary guard is unreachable, and a test proves it (a non-ASCII header value fails `to_str` first). Deferred: the cloud and remote-cluster headers ([D-081]). |
| api4/user.go (`getUser`, `me` only) | `mm-api/src/users.rs` | PARTIAL | 4 pass | **The first route served from Rust.** Etag, `If-None-Match` → 304, `Sanitize` for self, and the `Encode` newline ([D-086]). Body asserted byte-identical against the running Go server. Deferred: the permission check ([D-082], read it before adding `/users/{id}`), terms of service ([D-083]), `UpdateLastActivityAtIfNeeded` ([D-084]), real privacy settings ([D-085]). |
| — (proxy) | `mm-api/src/proxy.rs` | DONE | 3 pass | The Strangler Fig fallback. Strips hop-by-hop headers, preserves repeated ones (`Set-Cookie` above all), forwards bodies, and marks every response `x-mmrs-served-by: go` so a migrated route and a proxied one are distinguishable during cutover. Verified with a `POST` carrying a body: 201 through the proxy. |
| — (tooling) | `docker-compose.yml`, `crates/mm-api/tests/parity_users_me.rs` | DONE | 4 diff tests | The dev stack and the cross-server oracle. The test logs in **once per process** — a login mutates `UpdateAt`, and per-test logins made parallel tests move the field underneath each other. Gated on `MM_PARITY_STACK=1` so `cargo test` stays green without Docker. |
| — (licensing) | `LICENSE`, `crates/mm-model/LICENSE`, `NOTICE`, `README.md`, `Cargo.toml` | DONE | — | **[D-031] closed — phase 2 is unblocked.** Split licence, option (b), mirroring upstream: root is verbatim AGPL-3.0-only and `mm-model` **overrides** back to Apache-2.0, because it derives solely from `server/public/`. The rule this creates: AGPL crates may depend on `mm-model`, never the reverse — `mm-model` taking a type from `mm-store` would now breach the licence, not just the layering. The four AGPL crates carry the label while still holding zero AGPL-derived lines, deliberately. |
| — (shared) | `mm-model/src/post_list.rs::PostList::WIRE_KEYS` | DONE | 1 diff test | The six JSON keys the embed contributes. Read off the Go struct tags by the oracle rather than transcribed, so a field added upstream fails a test instead of silently changing which documents allocate the embed. |
| — (tooling) | `reference/dump/behaviour_post_search_results.go` → `fixtures/behaviour_post_search_results.json` | DONE | 8 diff tests | 19 documents through the wire format, `ToJSON`, `EncodeJSON` and `ForPlugin`, each recording the receiver **after** the call — which is what caught `ToJSON`'s side effect. Plus 7 constructor cases, an 11-case `PostSearchMatches` corpus and the promoted key set. Every case is `recover`-probed; 27 of Go's 76 answers are a crash. |
| — (tooling) | `reference/dump/behaviour_post_list.go` → `fixtures/behaviour_post_list.json` | DONE | 20 diff tests | 18 sections over a shared 14-document corpus, each method recording the nil-ness of all three collections **before and after** — which is how the materialisation table in the module docs was measured rather than read. Every one of the 193 cases is `recover`-probed; 11 of Go's answers are a crash. |
| — (tooling) | `reference/dump/behaviour_post_metadata.go` → `fixtures/behaviour_post_metadata.json` | DONE | 3 diff tests | 22 wire probes driving nil-against-empty for all seven collections, 9 `PostPriority` probes for the capitalised keys, and `Copy` measured for dropped fields and pointer aliasing. |
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

## Notes — model/audit.go, model/audits.go

1. **`Audits::Etag` returns `""` for an empty list.** Every other list etag in the crate returns a
   versioned string — `ChannelList` gives `11.11.0.0.0.0.0`. This one gives the empty string,
   which is not an etag at all, and a handler writing it into an `ETag:` header emits an empty
   header. See [D-076].

2. **It reads element `[0]` rather than scanning.** Go's comment asserts "the first in the list is
   always the most current" instead of the code establishing it. Measured: an **ascending** list
   etags to its oldest row, and an unsorted list to neither the newest nor the oldest. The
   correctness of the value is therefore a property of the query that produced the list, not of
   the function — the audit store port must keep its `ORDER BY CreateAt DESC`.

3. **One component, not four.** `Etag(o[0].CreateAt)` passes a single value, so the result is
   `<version>.<create_at>` where the channel lists produce five parts.

4. **`Audits` is `[]Audit`, not `[]*Audit`** — the first value-element slice in the tree, so
   [D-033] does not apply here for the first time in the crate. It has its own divergence instead:
   `[null]` gives Go a **zero-valued `Audit`**, seven keys and all, rather than nil or an error.
   That widened [D-075] from `[]string` to any non-pointer element and completed the three-way
   picture in that entry's table.

## Notes — model/user_autocomplete.go

1. **The same key obeys different rules in two structs of the same file.**
   `UserAutocompleteInChannel.out_of_channel` has no `omitempty`;
   `UserAutocomplete.out_of_channel` has it. Reading the tag per *type* rather than per *field*
   would get one of them wrong, and this is the clearest instance in the tree.

2. **`omitempty` on a slice collapses nil and empty, so the faithful type is `Vec`, not
   `Option<Vec>`.** Measured rather than reasoned: the corpus hands Go a nil slice in one case and
   an empty one in another and gets **byte-identical** documents back. An `Option` there would
   represent a state Go cannot express, and would then have to choose arbitrarily which of the two
   to emit. Without `omitempty` the opposite holds — nil is `null`, empty is `[]`, three states,
   `Option<Vec<T>>`.

3. **Getting rule 2 backwards is invisible locally.** Either choice round-trips cleanly through
   its own serializer; the difference only shows up as a missing or spurious key at a client.
   That is why the oracle records **key presence** separately from value, and why the assertion is
   `!vec.is_empty() == key_present` rather than a document comparison alone.

4. **Six more [D-033] instances**, one per field, driven individually rather than generalised from
   one — the entry's table cites fields, and `[]*User` with a `null` element is a document Go
   accepts and re-emits.

## Notes — model/emoji_search.go, model/user_access_token_search.go

1. **There is nothing unusual in either file, and that is worth recording.** Snake_case tags like
   most of the tree, no `omitempty`, no pointers, no methods, no constructors, no validation. The
   oracle demonstrates it rather than the module docs claiming it, and it is short on purpose: a
   corpus padded out to look proportionate to two ported types would imply surface these types do
   not have, and send the next reader looking for it.

2. **A single-field struct still emits its key.** `UserAccessTokenSearch{}` is `{"term":""}`, not
   `{}` — no `omitempty`. Reaching for one on a lone field looks harmless and would change the
   body a client receives.

3. **The four divergences are counted, not just exempted.** `the_only_divergences_are_the_standing_
   ones` asserts that exactly four cases diverge and names them, so a future change introducing a
   fifth fails the test instead of the new case being added to a skip list.

## Notes — model/limits.go

1. **Every key is camelCase**, which is the third naming convention in the ported tree —
   snake_case everywhere else with tags, tagless PascalCase in `wrangler.go` and
   `channel_member_history.go`, camelCase here. The hazard is that a mis-tagged field
   (`max_users_limit`) round-trips perfectly through its own serializer, so nothing local catches
   it; only comparing against Go's key list does.

2. **`max_users_limit` populates nothing in Go either.** Measured, not assumed — Go's
   case-folding fallback folds case and not punctuation, so the habit spelling is a silent no-op
   on both sides rather than a silent mis-read on one. That is the reassuring half of note 3
   below.

3. **A camelCase tag widens [D-040] relative to a snake_case one.** The Go field name
   `MaxUsersLimit` is itself a case-variant of the tag `maxUsersLimit`, so Go accepts both; a
   snake_case tag admits no PascalCase spelling at all. Four of seven probed spellings diverge
   here, against three of six for the tagless `ChannelMemberHistory`.

4. **Zero is a documented sentinel on four of the seven fields.** Go's comments say
   `postHistoryLimit` is "0 if no limits" and `lastAccessiblePostTime` is "0 if no limits
   reached"; `maxUsersLimit` and `singleChannelGuestLimit` read the same way. Nothing carries
   `omitempty`, so the zero is transmitted and the sentinel is usable — adding a
   `skip_serializing_if` for tidiness would turn "unlimited" into "unspecified".

5. **Nothing is validated.** A hard limit below the soft limit, an active count above both, a
   negative limit: all representable, all round-trip. These are computed figures on their way to
   an admin console, not a request body.

## Notes — model/channel_search.go

1. **`omitempty` on a pointer tests nil-ness, not the pointee.** `Page` and `PerPage` are `*int`
   with `omitempty`, so nil drops the key entirely and a **pointer to zero still emits
   `"page":0`**. Three states, three documents. Every other nillable field in the tree so far has
   been a pointer *without* `omitempty`, where the key is always present — so this is the first
   place the distinction exists, and a bare `i64` with a zero-skip predicate would collapse two of
   the three and drop a client's explicit `page=0`, which is the first page.

2. **`TeamIds` takes the opposite convention three lines above them, in the same struct.** No
   `omitempty`, so nil is `null`, empty is `[]`, and the key is always present. Reading the struct
   top to bottom, the convention changes twice.

3. **`null` inside a `[]string` is `""` in Go**, not a nil element and not a rejected document:
   `{"team_ids":[null]}` decodes to a one-element slice and re-marshals as `[""]`. That is
   [D-057]'s rule at array-element position and it is **new** — logged as [D-075]. It is *not*
   [D-033], which is about `[]*T` where the nil survives as `null` on the way out.

4. **`null` into `page` is not a divergence**, unlike every other `null`-into-a-scalar in the
   crate. The Go field is a pointer, so nil is a representable result and `Option<i64>` matches it
   exactly — including re-emitting as an absent key. Asserted on its own so the exemption list in
   the decode test is not misread as "all nulls diverge".

5. **Nothing validates, and three pairs of flags contradict each other** —
   `public`/`private`, `group_constrained`/`exclude_group_constrained`, and the two
   access-control-policy flags. Nothing in the model package reconciles them, and a `grep -rl` for
   a caller in `channels/store` found none, so whatever does lives above this port. Recorded
   rather than guessed at; a round-trip test pins that all four can be set at once.

## Notes — model/team_stats.go, model/users_stats.go, model/cluster_stats.go

1. **`ClusterStats` uses bare `int`; the other two use `int64`.** Go's `int` is platform-width, so
   the accepted wire range for those three fields is a property of the builder's target rather
   than of the type. Measured, not assumed: `strconv.IntSize` is 64 on the generating host and
   eleven numeric bounds agree between an `int` field and an `int64` field, including both `int64`
   extremes and the two values just past them. That agreement is what licenses `i64`. See
   [D-074].

2. **Go's case fold is against the `json:` tag, not the Go field name** — and this file is where
   that becomes visible, because it looks like a counterexample to what
   `channel_member_history.go` measured. `{"Total_Member_Count":5}` populates the field here while
   `{"channel_id":…}` did not populate `ChannelId` there. Same rule both times: the fold targets
   the *effective* name, which is the tag when one exists. `total_member_count` already has the
   underscores; `ChannelId` never will. Anyone implementing [D-040]'s boundary decoder needs this
   — folding against the Rust field identifier would be wrong in both directions.

3. **Nothing in any of the three validates anything.** `TeamStats` will happily report an active
   count above its total, and no id is checked. These are store counts on their way out.

4. **Five divergences, none of them new.** Two [D-057] (`null` into a string and into an int),
   two [D-040] (the folded keys), one [D-071] (a repeated field). A type this plain is a good
   check that the standing crate-wide entries are the *only* ones left at this size.

## Notes — model/analytics_row.go

1. **`encoding/json` renders a float differently from `%v` and differently from serde_json, and
   all three are live in this crate.** Measured over 29 values: `%v` (`utils::go_format_float`)
   disagrees with the JSON rendering on 10, serde_json on 12. The disagreements are on ordinary
   values — every integral float is in both sets, and an analytics count is an integer. Logged as
   [D-073].

2. **The JSON thresholds are `1e-6` and `1e21`**, not `%g`'s `1e-4` and `1e6`. So `1234567.0` is
   `1234567` on the wire and `1.234567e+06` in a log line, and `1e-6` is `0.000001` while
   `9.99999e-7` is `9.99999e-7`.

3. **A negative exponent loses one leading zero; a positive one keeps it.** Go's encoder rewrites
   a trailing `e-09` to `e-9`, so the wire carries `1e-7` and `1e+21`. The rewrite is exactly two
   characters wide — `1e-107` must not become `1e-17` — and is reproduced as the same narrow test
   Go performs rather than as general zero-stripping.

4. **`NaN` and the infinities are an error and emit nothing.** Not `null`, not `0`. Measured at
   three levels: the bare value, the row, and a slice where a good row precedes the bad one — the
   good row is lost too. That makes serialization fallible for this type in a way no other ported
   type is, and it is why `Serialize` is hand-written rather than derived.

5. **The float is emitted as a `serde_json::value::RawValue`.** There is no serializer method for
   "a numeric token I have already formatted" — `serialize_f64` hands the value back to
   serde_json's encoder, which is the thing being replaced. This turned on serde_json's
   `raw_value` feature, a feature flag on an existing dependency rather than a new crate.

6. **The oracle records each float's raw bits and the Rust side rebuilds from those.** Parsing
   Go's decimal rendering back into an `f64` would test the parser, and two distinct floats can
   print identically — `-0.0` and `0.0` compare equal under `==` and are different values, which
   is why the decode test compares bit patterns.

## Notes — model/channel_member_history.go, model/channel_member_history_result.go

1. **Neither file has a single `json:` tag**, so every wire key is the Go field name verbatim —
   `ChannelId`, `JoinTime`, `IsBot`. Second instance of this after `wrangler.go`, and the entire
   content of the port: writing them snake_case out of habit is the only way to get this wrong,
   and the key lists are read off the Go struct tags by the oracle rather than transcribed.

2. **`UserEmail` is tagged `db:"Email"` and has no json tag.** `encoding/json` does not read
   `db`, so the wire key is `UserEmail` while the column is `Email`. The only tag visible on the
   field is the one not to copy — a port that used it would rename the field on the wire.

3. **Go's case-insensitive fallback folds case but not punctuation.** Measured over six
   spellings: `channelid`, `CHANNELID` and `cHaNnElId` all populate `ChannelId` in Go;
   `channel_id` and `channel-id` do **not**. That bounds [D-040] usefully — the divergent set for
   a key is exactly its case-variants, not "any plausible spelling", which is what makes a
   boundary-decoder fix a finite transformation rather than a guess.

4. **A failed `LeaveTime` decode leaves a non-nil pointer to zero in Go.** The decoder allocates
   the pointer, fails to fill it, and reports the error — so a handler that ignored the error
   would read "left at the epoch" where the truth is "still present". Not comparable from Rust,
   where a decode is all-or-nothing; pinned in the fixture because the misreading is plausible.

5. **`null` into `LeaveTime` is the one scalar-null case that is *not* [D-057].** The Go field is
   nillable, so `Option<i64>` reproduces it exactly — nil in, `None` out, `null` back on the
   wire.

6. **The two structs do not share a type in Go.** `ChannelMemberHistoryResult` redeclares the
   first four fields rather than embedding `ChannelMemberHistory`, so there is no promotion to
   reproduce and no `Deref` here — unlike `scheduled_post.go`, which does embed.

7. **Go's comment says "these two fields" above four of them.** Left as upstream wrote it. It
   reads like the joined group grew without the comment following, which is worth knowing if a
   fifth appears.

## Notes — model/channel_data.go

1. **`Etag` guards one pointer and dereferences the other, three lines apart.** `Member` is
   nil-checked into a local; `Channel.Id`, `.UpdateAt` and `.LastPostAt` are read unguarded on the
   next line. A nil member gives `0`; a nil channel **panics**. Measured under `recover`, and the
   oracle records which pointer was nil for each crash so the attribution is not an inference.

2. **The nil channel is easy to reach.** Neither field has `omitempty`, so `{}`,
   `{"channel":null}` and any document carrying only a member all decode to one, and
   `ChannelData{}` from any code path has both nil. That is what separates this from the rest of
   the panic family ([D-052], [D-054], [D-058]), which need a specific malformed collection.

3. **Only four fields reach the etag and nine real changes are invisible to it.** Three come from
   the channel — `id`, `update_at`, `last_post_at` — and exactly one from the member,
   `last_update_at`. Changing the member's `roles`, `last_viewed_at`, `msg_count`,
   `mention_count` or `notify_props`, or the channel's `display_name`, `total_msg_count`,
   `delete_at` or `create_at`, leaves the etag byte-identical. Each is named in the parity test
   rather than counted, so a field becoming visible upstream fails with its own name in the
   message.

4. **A dotted id changes the component count.** `Etag` joins with `.` and escapes nothing, so
   `a.b` yields eight dot-separated parts where every other channel yields seven — the same trap
   as note 5 under `model/channel_list.go`, reached here through a field a client controls.

5. **The oracle marshals Go values instead of writing JSON literals**, and that was a correction
   rather than a preference. The first draft's hand-written partial documents could not be decoded
   at all, because `Channel` and `ChannelMember` are two of [D-043]'s 61 unfixed containers.
   Building from values yields the document the Go *server* emits, which is the one the wire
   format has to agree on — and it stops the file from silently becoming a D-043 test.

## Notes — model/channel_view.go

1. **`null` into a map value creates the key and sets it to zero.** This was the open question the
   corpus existed to answer: `{"last_viewed_at_times":{"a":null}}` gives Go a map where `a` is
   **present** with value `0`, not a map without `a`. Nothing in the `encoding/json`
   documentation says which it would be. We reject the document — [D-057] in a position it had
   not been measured in before.

2. **A failing map value still leaves the key in the map, set to zero.** Every rejected shape
   (`1.0`, `1e9`, a quoted number, an out-of-range integer, a bool, an object, an array) produces
   an error *and* a two-entry map with the good key intact and the bad key zeroed. A Go handler
   that ignored the unmarshal error would act on that. Same shape as note 3 under `model/file.go`,
   one level deeper.

3. **A repeated struct field takes the last value in Go and fails the decode here** —
   [D-071], new this session and crate-wide. `{"status":"first","status":"second"}` gives Go
   `"second"`. A repeated key inside the *map* is not affected: a `BTreeMap` overwrites exactly as
   Go's map does, so `{"a":1,"a":2}` is `{"a":2}` on both sides.

4. **Go's map key ordering is byte value, not collation.** `{"A":1,"B":4,"a":3,"b":2}` is the
   emitted order for those four keys — every uppercase letter before every lowercase one.
   `BTreeMap<String, _>` agrees because `String: Ord` is byte-wise.

5. **The bool is strict.** `"true"`, `1`, `0` and `""` are all rejected by Go, and by
   `serde_json` too. Only `null` differs, and it is accepted as `false` there.

6. **Neither type has an `IsValid`.** `channel_id` need not be an id, `status` is a free-form
   string with no constants declared for it, and an empty `prev_channel_id` is meaningful rather
   than missing — it is how a client says it arrived from nowhere.

## Notes — model/unicode.go

1. **A Go `RangeTable` entry is not an interval, and three of these four tables prove it.** Each
   entry carries a stride, and membership is `lo <= r <= hi && (r - lo) % stride == 0`. Han's
   `U+3005..U+3007` has stride 2, so 々 (U+3005) and 〇 (U+3007) are Han and 〆 (U+3006) between
   them is in no script at all. Katakana has entries of stride 288 and 15, Hiragana one of stride
   30 — each admitting exactly two codepoints out of a span of hundreds. Reading all four as
   solid intervals would admit 331 codepoints that are in none of the scripts.

   This corrected the oracle's own hand-written annotations: three of them asserted that U+3005,
   U+3007 and U+303B were Common rather than Han, which is what they look like. The generator
   was run before any Rust was written, which is the only reason those never became a test.

2. **The intuitive membership is wrong in both directions.** Not CJK: the ideographic space
   U+3000, the punctuation 。、「」, the katakana middle dot U+30FB, the prolonged sound mark
   U+30FC, the combining and spacing voiced marks U+3099–U+309C, and every fullwidth Latin form.
   All are Common or Inherited script. **Is** CJK: the iteration marks 々 U+3005, 〻 U+303B,
   ゝ U+309D and ヽ U+30FD.

3. **Hangul is three separate blocks, not just the syllables.** Jamo at U+1100, compatibility
   Jamo at U+3131 and the syllables at U+AC00, with gaps between them belonging to other scripts.
   14 ranges in total, and it is the only one of the four with no astral-plane entries.

4. **`unicode-general-category` cannot be reused here.** It is already a dependency, for the
   `unicode.IsLetter` gap in note 3 of the utils section, and it answers *general categories* —
   a different partition of the codepoint space with no member meaning "Han". Every CJK
   ideograph is `Lo`, and so is every Thai consonant.

5. **Go's loop is defined on invalid UTF-8 and ours cannot be.** `for _, r := range s` yields
   U+FFFD per malformed byte. A Rust `&str` cannot hold those bytes, so the case is unreachable
   rather than divergent — and U+FFFD is in none of the four tables, so a caller holding `&[u8]`
   can convert lossily without changing the answer.

6. **Nothing in `server/public/` calls it** except its own test, so no wire surface depends on
   this yet. Worth re-checking when the app layer lands.

## Notes — model/file.go

1. **`time.Duration` is an `int64` of nanoseconds on the wire, and it has a `String()` that is
   not.** `encoding/json` never calls `Duration.String()`, so `time.Hour` marshals as
   `3600000000000` and not as `"1h0m0s"`. The oracle records both columns for all 13 values so
   the distinction is visible rather than argued. Rust has no substitutable type:
   `std::time::Duration` serialises as `{"secs":…,"nanos":…}` and `chrono::TimeDelta` has no
   serde impl at all. Plain `i64`, and the doc comment on the field is load-bearing — it is the
   only time-valued field in the crate that is not epoch milliseconds.

2. **Go's integer decode is stricter than "a JSON number".** Measured over 17 values: `1.0` is
   **rejected**, though it is exactly representable; `1e9` is rejected for being spelled as a
   float; `"1h"` and `"3600000000000"` are rejected for being strings. `serde_json` agrees on all
   of them, which is the useful result — the crate needs no custom deserializer here, and now
   there is a test saying so rather than an assumption.

3. **A failed decode still leaves the earlier fields populated in Go.** Every rejection above
   reports an error *and* leaves `url` set, because `encoding/json` walks the object in document
   order and returns the first failure without unwinding. A Rust decode is all-or-nothing, so
   there is no partial value to compare; only the accept/reject verdict is asserted. It matters
   at the API layer, where a Go handler that ignores the unmarshal error would still see the
   `url`.

4. **`MaxImageSize` is written `int64(6048 * 4032)`** — an explicit conversion, so it is a typed
   constant, and the product (24,385,536) is computed at compile time. The Rust port keeps the
   expression and the oracle pins both factors, so the arithmetic is checked rather than the
   literal trusted.

5. **The zero `PresignURLResponse` is two keys, not `{}`.** Neither field is a pointer and
   neither carries `omitempty`, so an empty URL and a zero expiration are both transmitted.

## Notes — model/file_info_search_results.go

1. **A nil embed drops five keys, and it is the type's zero value.** `FileInfoSearchResults{}`
   marshals to `{"matches":null}`, not to five nulls plus matches — `encoding/json` skips every
   field whose index path runs through a nil pointer. `MakeFileInfoSearchResults(nil, nil)` gives
   the same document.

2. **Any recognised key allocates the embed, including one that carries no information.**
   `{"order":null}` allocates it, and so does `{"first_inaccessible_file_time":0}` — an explicitly
   zero scalar is indistinguishable from a real one to the decoder. `{"nope":1}` does not, and
   neither does `{"matches":…}` alone. So the round trip of a search response is not idempotent
   in general: `{"order":null}` comes back with five keys.

3. **`FileInfoList` contributes all five of its fields**, where `PostList` contributes six of
   seven. `PostList.BurnOnReadPosts` is `json:"-"`, which makes it a promoted field that is
   nevertheless an *unknown* key — a distinction `FileInfoList` does not have. Copying the
   `post_search_results.rs` key set without checking would have been wrong in both directions.

4. **The file has no methods.** That is the whole difference from `post_search_results.go`, and
   it removes three panics and an audit projection rather than adding anything. Nothing here
   strips action integrations, so there is no `&mut self` `ToJSON` and no shared-receiver trap.

5. **`MakeFileInfoSearchResults` initialises positionally** (`&FileInfoSearchResults{fileInfos,
   matches}`) rather than by field name, so a field added upstream breaks that line at compile
   time. Worth knowing before "fixing" it to named fields in a future port: the positional form
   is the only thing making the constructor self-maintaining.

6. **`matches` needs Go's HTML escaping.** It is a `map[string][]string` whose keys are file ids
   in practice but arbitrary on the wire; the oracle records `<a>&` as
   `<a>&` and U+2028 as ` `. `serde_json::to_string` emits neither — see
   [D-027].

## Notes — model/scheduled_post_recurrence.go, `utils::go_time::date_in_zone`

All of these are oracle results over 280 `time.Date` probes and 295 end-to-end cases. Three
contradict what the Go source suggests, and one contradicts what this ledger said before the
corpus was built.

1. **`time.Date` has no specification to port.** Its doc says only that for a local time that
   does not exist or exists twice, "the choice of time zone, and therefore the time, is not
   guaranteed". What exists is an implementation, and it looks the offset up on the wall clock
   **read as a UTC instant** — which is why the answer depends on the sign of the zone's own
   offset rather than on anything about the transition.

2. **A repeated local hour does *not* resolve to the earlier instant.** This ledger predicted it
   would, and the corpus refuted it: it takes the earlier instant in America/New_York and
   America/St_Johns and the **later** one in Europe/London, Antarctica/Troll, Africa/Casablanca,
   Australia/Sydney, Australia/Lord_Howe and Pacific/Chatham. All 34 ambiguous probes split
   exactly on the sign of the offset in force before the transition, with no exceptions. A port
   written to `LocalResult::Ambiguous(a, _) => a` passes in New York and is wrong in London.

3. **A skipped local hour splits the same way, and the intuitive direction is the rarer one.**
   02:30 on 2023-03-12 does not exist in New York and Go answers **01:30 EST** — before the gap.
   01:30 on 2023-03-26 does not exist in London and Go answers 02:30 BST — after it.
   Antarctica/Troll is the sharpest case: its winter offset is 0, so Go's `if offset != 0` guard
   skips the correction outright and a two-hour gap resolves two hours forward.

4. **The `start`/`end` interval boundaries Go's algorithm reads are not needed.** `utc < start`
   says the candidate sits in the interval *before* the one holding the pseudo-instant, so
   `lookup(start - 1)` is `lookup(utc)`; `utc >= end` says it sits in the one after, so
   `lookup(end)` is `lookup(utc)` again. Both branches collapse to "the offset at the candidate",
   which is why the port needs only an offset function and no transition table. The reduction
   also subsumes the `offset != 0` guard. It assumes at most one transition inside a 26-hour
   span; `chrono_tz_agrees_with_the_tzdata_the_oracle_ran_against` asserts that per boundary
   rather than leaving it as a premise.

5. **The series preserves the wall clock, not the elapsed time.** Four weekly steps from an EST
   wall clock land on the same local time of day in EDT, which is four weeks **minus an hour** of
   real time. Each step also adds to the *previous answer's* wall clock, so a step Go moved to
   escape a gap is inherited by every step after it — the answer cannot be computed as
   `scheduled_at + 7n` days by any arithmetic.

6. **The first `AddDate` is unconditional.** A post scheduled ten years from now still reports a
   next occurrence a week after that, not the scheduled time itself. The loop only ever adds.

7. **`!next.After(now)` is strict**, so a candidate landing exactly on `now_millis` is rejected
   and costs another week — measured at one-millisecond resolution either side.

8. **Every repeat type that is not `weekly` is an error, the empty string included.** So
   `ComputeNextScheduledAt` on a non-recurring scheduled post fails rather than returning the
   scheduled time, and the repeat type is checked **before** the timezone — a garbage zone on a
   `daily` post reports the type.

## Notes — model/mention_map.go

Two `map[string]string` newtypes and one codec. Everything below is an oracle result.

1. **Neither key present is success, not an error.** `mentionsFromURLValues` returns an allocated
   empty map when *both* `user_mentions` and `user_mentions_ids` are absent, and an error naming
   the missing one when exactly one is present. Reading the first case as an error would 400
   every mention-free request. A third shape — both present, both **zero-length** — is also
   success, and it is reachable only by building `url.Values` directly, never through
   `ParseQuery`.

2. **A key present with an empty slice takes the length check, not the not-found branch.** That
   is why `go_url::Values` gained `get_all` (Go's two-value map read) and `set_all` (Go's direct
   map assignment): `Values::get` collapses absent and present-but-empty into the same empty
   string, and this file is the first caller that can tell them apart.

3. **A repeated mention is an error only when the ids disagree.** The guard is
   `ok && oldId != id`, so the same pair twice collapses silently. With three entries the error
   names the **first** id seen and the first one that differs — `{a→first, a→second, a→third}`
   reports `first and second`, never `first and third`.

4. **Nothing validates the contents.** Empty mention, empty id, an id that is not a 26-character
   id, a mention that still carries its `~`, tabs and newlines — all stored verbatim. The pairing
   is positional and that is the whole contract.

5. **The four key constants are unexported.** The oracle recovers them by encoding a one-entry
   map (one entry, so no map-order ambiguity) rather than transcribing them. Same class of
   problem as `version.go`'s unexported release table, solved one level more cheaply — no
   `go/parser` needed, because `ToURLValues` already publishes the names.

6. **`ToURLValues` output order is random in Go.** It ranges a map, and `Values.Encode` sorts by
   key — of which there are only two — so the slice under each preserves map-iteration order. A
   two-entry map encodes two ways from one input. Ours is a `BTreeMap` and always emits the
   sorted ordering. Harmless because `FromURLValues` pairs by index and the two slices are
   permuted together, which is asserted rather than assumed: `round_trips` is true for all twelve
   corpus maps, and a hand-built reversed ordering decodes to the same map. See [D-063].

7. **`~` and `*` are not escaped by `QueryEscape`; `+` is.** `~town-square` survives the round
   trip literally while `a+b` becomes `a%2Bb` and a space becomes `+`. Already pinned by
   `behaviour_go_url.json`; worth restating because a mention key is exactly where a `~` shows up.

8. **Go's `url.Values` holds bytes, ours holds `String`.** `?user_mentions=%80` is a valid map key
   in Go and a fifth error variant here — [D-064]. `go_url::Values` itself models the bytes
   correctly; the narrowing happens at `StringMap`.

*(These notes were written on 2026-08-14 and were lost until 2026-08-17: a script wrote them to a
relative path while the shell was inside `reference/mattermost/`, so they landed in the read-only
Go tree instead of this ledger. Recovered verbatim.)*

## Notes — model/post_metadata.go

1. **`PostPriority` lives in post.go, and the two files are mutually dependent.** `PostMetadata`
   embeds `*PostPriority`; `Post` embeds `*PostMetadata`. Something has to break the cycle, so
   `PostPriority` is defined in `post_metadata.rs` and `post.rs` should re-export it — the same
   shape as `CURRENT_VERSION` living in `version.rs` with a `utils` re-export.

2. **`PostPriority.PostId` and `.ChannelId` serialise as `PostId` and `ChannelId`.** Go tags them
   `json:",omitempty"` — an *empty name* — and falls back to the Go field name, so two
   capitalised keys sit beside three snake_case ones in the same object. Third instance of this
   trap after `TeamForExport.SchemeName`; the comment calls them internal DB plumbing and they
   reach the wire anyway.

3. **Every `PostMetadata` field carries `omitempty`, collections included**, and Go's `omitempty`
   drops a nil slice *and* an empty one. So the two are indistinguishable on the wire, and a
   plain `Vec` with a length predicate is faithful — `Option<Vec>` would invent a distinction Go
   cannot express.

4. **`redacted_file_count` sits between `files` and `images`**, not at the end. Field order is
   emission order, so this matters for byte-exact comparison.

5. **`PostTranslation.Object` is a `json.RawMessage`, and an explicit `null` survives.**
   `RawMessage` is a `[]byte`; `omitempty` drops it when *empty*, but a RawMessage holding the
   four bytes `null` is not empty and re-emits as `null`. serde's `Option` collapses that by
   default — `null` deserialises to `None` and then disappears. Ported with a deserialiser that
   wraps `Value::Null` in `Some`, leaving `None` to mean only "key absent".

6. **`Copy()` drops `expire_at` and `recipients`.** It is documented "does a deep copy"; the two
   fields are simply absent from the struct literal it returns. Almost certainly fields added to
   the struct and not to `Copy`. Reproduced verbatim and pinned — see [D-034].

7. **`Copy()` is also shallow for everything except `Priority`.** `copy`/`maps.Copy` duplicate
   the *pointers*, so the copy shares every embed, emoji, file, reaction, acknowledgement, image
   and translation with the original. Only `Priority` is rebuilt. Rust owns its values, so ours
   is genuinely independent — a divergence in the safe direction, same class as [D-015].

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

## Notes — model/post.go (chunk 1)

post.go is 1,640 lines and is being translated across sessions. Chunk 1 is the `Post` wire type,
the constants and everything self-contained; what remains and what it waits on is tabulated in
the module docs of `post.rs`. All of the below are oracle results.

1. **`IsValid`'s type failure reports the type in a field called `id`.** The detail is
   `"id=" + o.Type`, not the post id — every other check in the function uses `"id=" + o.Id`.
   Clients parse `detailed_error`, so this is wire surface. Reproduced verbatim.

2. **`PostTypeEphemeral` is a declared post type that `IsValid` rejects.** It is not in the
   accepted switch and does not carry the custom prefix, so a post of that type is a 400. Easy
   to "fix" by adding it to the list; do not.

3. **`DelProp` does not panic on a nil `Props`, and the source suggests it should.** It sizes
   its copy `make(map[string]any, len(o.Props)-1)`, which is `make(map, -1)` when Props is nil
   or empty. That panics for a *slice*; for a **map** Go clamps the hint, so it is fine. Measured
   under `recover` after the reading predicted a panic — this is the second conclusion in this
   file the oracle reversed.

4. **`HasForceNotification` and `HasSilentNotification` are not symmetric.** Force does a type
   switch that answers true for **any non-empty string**, so `{"force_notification": "false"}`
   forces a notification, as does `"junk"`. Silent accepts only a real JSON bool, so
   `{"silent_notification": "true"}` is *not* silent. Since force wins in
   `IsNotificationSuppressed`, a post with `force_notification: "false"` and
   `silent_notification: true` is **not** suppressed. Three separate ways to get this backwards.

5. **`HasUnsafeLinks` requires the exact string `"true"`** — a real bool `true` does not count.
   A fourth truthiness convention in the same props map.

6. **`IsFromOAuthBot` is satisfied by a prop that was never set.** It reads
   `props[PostPropsOverrideUsername] != ""`, comparing an `any` against a string. An absent key
   is a nil interface, and `nil != ""` is **true** in Go — so `{"from_webhook": "true"}` alone is
   "from an OAuth bot", while `{"from_webhook": "true", "override_username": ""}` is not. A
   stored explicit `null` behaves like the absent key. Ported as-is.

7. **`CleanPost` does not clear `delete_at`.** It clears `id`, `create_at`, `update_at` and
   `edit_at` only, which is easy to mis-assume from the name.

8. **`PreCommit` sorts the file ids.** `RemoveDuplicateStrings` sorts before de-duplicating, so
   the stored `file_ids` order is not the submitted one. The Go comment says only "protect
   against duplicates" and the reordering is a silent side effect of the helper.

9. **`PreSave` clears `original_id` unconditionally** and forces `update_at` to `create_at` even
   when the caller set it ahead. `create_at` is taken from the clock only when it is exactly
   zero — a **negative** `create_at` survives, and `IsValid` accepts it, because both checks are
   `== 0` rather than `<= 0`. The same is true of `update_at`.

10. **`original_id` is length-checked in bytes and never validated.** `len(o.OriginalId) == 26`,
    so 26 exclamation marks pass and 13 two-byte characters pass. `channel_id`, `delete_at` and
    the `file_ids` *contents* are not validated at all.

11. **The three length caps measure Go's JSON, and a nil collection costs four runes.**
    `ArrayToJSON(nil)` is `"null"`, not `"[]"`; `StringInterfaceToJSON(nil)` is `"null"`, not
    `"{}"`. The props cap also pays Go's HTML escaping, so a single `<` costs six runes against
    the 800,000 limit rather than one. Both marshallers are ported to `utils` and pinned.

12. **`Filenames` carries `json:"-"` and is still validated.** It cannot arrive from a client and
    cannot be recovered from a marshalled post, so the oracle records it beside the JSON. Same
    shape as `FileInfo.Path`, but the consequence is opposite: an absent `Filenames` is valid.

13. **`SanitizeProps` keeps the notification markers for federated posts.** `force_notification`
    and `silent_notification` are stripped on every locally-originated path and **preserved**
    when `RemoteId` is non-empty, because the origin cluster already enforced its own authority.
    `add_channel_member` is stripped either way. The `from_*` identity markers are never
    stripped — they are render hints, still user-settable, and Go's comment says the whole
    impersonation surface is scheduled to default-strip in v12.

14. **`PreserveIdentityPropsFrom` carries five props and `force_notification` is not one of
    them**, even though `SanitizeProps` strips it. A prop stored as an explicit JSON `null` is
    also not carried, because `GetProp` returns a nil interface for it.

15. **`ContainsIntegrationsReservedProps` returns declaration order, not map order**, and tests
    membership rather than truthiness — a key holding `null` still counts. `from_bot` and
    `from_oauth_app` are *not* in the reserved set although `from_webhook` and `from_plugin` are.

16. **`findAtChannelMention`'s `\B`/`\b` anchors are asymmetric in a way worth pinning.**
    `a@channel` does not match but `-@channel` does; `@channel-` matches but `@channel_` does
    not, because `_` is a word character. `@here@all` yields `@here`. Rust's `regex` agrees with
    Go's RE2 on all 25 probes.

17. **`remote_id` and `is_following` are pointers with `omitempty`, which tests only nil-ness.**
    `Some("")` serialises as `""` and `Some(false)` as `false`; the keys disappear only for
    `None`. That is load-bearing for `SanitizeInput`, which sets `remote_id` to a pointer-to-empty
    rather than to nil, so the key stays on the wire.

## Notes — model/post.go (chunk 2)

`Attachments`, `AttachmentsEqual` and the non-interactive half of `AllStrings`. All oracle
results; three of them contradict what the Go source suggests.

1. **`Attachments()` is a decode, not a cast, and a `null` element survives it.** Go marshals
   each element of `props.attachments` and unmarshals it into a `MessageAttachment`. For a bare
   `null` that is `json.Unmarshal("null", &struct)`, which leaves the destination untouched and
   reports **no error** — so `[null]` yields one *zero* attachment. Every reading of that loop
   predicts it is dropped.

2. **One wrongly-typed key drops the whole element, and its neighbours survive.**
   `[{"title":"a"},{"title":123},{"title":"b"}]` is two attachments. So a client can silently
   lose an attachment to a single bad field rather than getting an error.

3. **serde accepts a JSON array as a struct; Go does not.** A derived `Deserialize` takes a
   sequence as the fields in declaration order, so `[[]]` decoded into a zero attachment until
   an explicit object guard was added. Go rejects it. This one is a Rust trap with no Go
   counterpart and would have been invisible without the `element_array` probe.

4. **Go strips nil `actions` and nil `fields` — and only those.** A nil element of an action's
   `options` is kept, and the attachment survives holding `"options":[null]`. Ours cannot decode
   that at all, so we lose the **attachment**, not the option; see [D-033], widened.

5. **`fields: [null]` and `fields: []` both come back as `[]`, while `fields: null` stays
   `null`.** The filter writes `decoded.Fields[:0]` over a slice that was non-nil, so an
   all-nil list is empty-but-present. `actions` does the same and is then dropped by `omitempty`.

6. **`AttachmentsEqual` crashes the Go server on an ordinary post.** It calls
   `MessageAttachmentField.Equals`, which reflects on a nil `Value` ([D-039]) — and a field with
   no `value` key is exactly that. Two of twenty corpus pairs panic. Ours answers.

7. **A malformed attachment is absent, not unequal.** Both sides go through `Attachments()`
   first, so a post whose only attachment is malformed compares **equal** to a post with none.

8. **`AllStrings` trims non-string values and does not trim string ones.** A string field value
   is appended with its original bytes (`"  fv  "` stays padded) unless it is whitespace-only; a
   number, bool, map or slice is rendered with `fmt.Sprint` and appended **trimmed**. The
   rendering is Go's `%v`, so `123456789` becomes `1.23456789e+08` and a map becomes
   `map[a:1 b:2]` — `utils::go_format_v` was already in place from `message_attachment.go`.

9. **A nil field value is skipped entirely**, so a field with no `value` key contributes only its
   title — and a field with a blank title and a real value contributes only the value.

10. **Go's `unicode.IsSpace` and Rust's `char::is_whitespace` agree on every probe**, including
    the two that look like spaces and are not: U+200B (zero-width space) and U+180E (Mongolian
    vowel separator) are **kept** as message text by both, while NBSP, U+1680, U+3000 and U+0085
    are all whitespace to both.

11. **`Post` did not decode a partial document.** Found by feeding it the corpus, which is
    written the way a client writes a post: `{"channel_id":"c","message":"hi"}` failed with
    `missing field 'id'` where Go zero-fills. Fixed with container-level `#[serde(default)]`;
    14 of the crate's 75 deserializable types now carry it and most of the rest still need it —
    see [D-043].

12. **`encoding/json` matches keys case-insensitively.** `{"Title":"t","TEXT":"x"}` is a
    populated attachment in Go and an empty one for us. Crate-wide, not an attachment problem;
    see [D-040].

## Notes — model/integration_action.go (chunk 2), the Dialog family

`DialogElement::IsValid` is the largest validator in the model package and the only one whose
failures are worth reading one by one. All of the below are oracle results.

1. **`checkMaxLength` decides "is this required?" by comparing the field *name string*.** It
   takes `fieldName string` and returns "cannot be empty" only when that string is
   `"DisplayName"` or `"Name"`. Ported with the same string comparison rather than a bool flag,
   because the name is also interpolated into both messages and the two must not drift apart.

2. **The `text`/`textarea` subtype failure reports the element's *type*.** Go writes
   `errors.Errorf("invalid subtype %q", e.Type)`, so a `text` element with `subtype: "nope"`
   reports `invalid subtype "text"`. Upstream bug, reproduced — a client parsing the message
   sees the wrong value in both languages.

3. **`min_length > max_length` fires on an otherwise untouched element**, because `max_length`
   defaults to 0. So `{"min_length": 1}` alone is invalid with `got 1 > 0`.

4. **An invalid `data_source` hides a bad default.** The default-in-options branch is an
   `else if e.DataSource == ""`, so `data_source: "nope"` reports only the data-source failure
   and never checks the default at all.

5. **`default value %q doesn't exist in options ` ends with a space.** Wire surface; do not tidy.

6. **The multiselect default strips *all* spaces before splitting**, rather than trimming each
   value, so an option whose value contains a space can never be matched. `" 1 , 2 "` is fine and
   `"with space"` is not.

7. **A valid datetime in a `date` field is a failure, not a pass.** `validateDateFormat` returns
   a warning phrased as an error, carrying the truncated date:
   `date field received datetime format "…", only date portion "2023-01-02" will be used`. The
   truncation is the **wall clock**, so `…T15:04:05-07:00` still reports `2023-01-02`.

8. **Go's `time.Parse` is stricter and looser than it looks, in five measured ways.** The hour is
   the only flexible field (`T5:04:05Z` parses, `T15:4:05Z` does not); year is exactly four
   digits, so `10000-01-01` fails and `0000-01-01` passes; a fractional second is accepted
   although **no layout mentions one**, after a period *or a comma*; the `-07:00` layout also
   accepts a bare `Z`; and `T`/`Z` must be uppercase. Reproduced by a hand-written scanner —
   chrono's `%Y-%m-%d` accepts non-padded components and would drift.

9. **Relative dates are 3 to 5 bytes with a case-sensitive unit**, and the middle goes through
   `strconv.Atoi`, which takes its own sign — so `++5d` and `+-5d` are **valid** patterns while
   `+1h` and `+1234d` are not.

10. **`IsValidLookupURL`'s traversal guard scans the whole URL**, and the prefix ends in `/`, so
    `/plugins//x` is rejected on a `//` that spans the boundary. The HTTP branch applies **no**
    traversal guard at all, so `https://example.com/../x` is valid. A first port that scanned
    only the part after the prefix passed every case but that one.

11. **Two composition rules in one file.** `Dialog::IsValid` wraps each element failure with
    `errors.Wrapf`, so one bad element is exactly **one** parent message containing a rendered
    `3 errors occurred:` block; `OpenDialogRequest::IsValid` appends the dialog's multierror,
    which `multierror.Append` splices in **flat**. The duplicate-name check also runs before the
    element's own validation, so a duplicated invalid element reports the duplicate first.

12. **`SubmitDialogResponse::IsValid` short-circuits on `error` or a *non-empty* `errors` map**,
    and then ignores everything else — including a `type` it would otherwise reject. An empty
    `errors` map does not short-circuit. It is also the only validator in the family returning a
    bare error rather than a list.

13. **The dialog icon URL and the element URLs use different validators.** `icon_url` is plain
    `IsValidHTTPURL`, so `/plugins/x/i.png` is **invalid**; `data_source_url` and the action
    button URL go through `IsValidLookupURL`, which accepts it.

14. **`EffectiveDateTimeConfig` merges by emptiness, not by presence.** An empty `min_date` or a
    zero `time_interval` inside `datetime_config` does not override the deprecated top-level
    field. `location_timezone` is copied unconditionally, and `manual_time_entry` is OR'd with
    the deprecated `allow_manual_time_entry` — Go's comment says `omitempty` makes an explicit
    `false` unrecoverable, which is exactly why.

15. **A zero `time_interval` means "omitted" and is not replaced by `DefaultTimeIntervalMinutes`.**
    The constant is a client hint; `IsValid` skips the range check entirely when the interval is
    zero, and otherwise requires 1..=1440 **and** a divisor of 1440.

## Notes — model/post_interactive_blocks.go

Three JSON dialects, six walkers, and no types at all — the whole file is `map[string]any`
traversal, where **every type mismatch is a silent no-op rather than an error**. That is what
makes it dangerous to port from a reading: a wrong key name produces an empty result, not a
failure, and an empty result looks like a legitimately empty post. All of the below are oracle
results.

1. **The same concept is spelled three different ways.** An image URL is `url` on an mm_blocks
   `image`, `image_url` on a Block Kit `image` or accessory, and `url` again on an Adaptive Card
   `Image` — but the Adaptive Card key is `url` while its Block Kit sibling is `image_url`, so
   `{"type":"Image","image_url":…}` yields nothing. Each wrong spelling is pinned as a no-op.

2. **The two `column_set` walkers disagree, and the image one finds nothing** for the shape a
   producer actually emits. It passes each *item* to the array walker instead of the items array,
   so an image surfaces only from an array of arrays. Reproduced, not repaired — [D-045].

3. **Block Kit's two text shapes are not interchangeable.** A `markdown` block reads a bare
   string at `text`; a `section` and a `header` read `text.text` off an object. Swapping them
   contributes nothing either way, so `{"type":"section","text":"hi"}` is invisible.

4. **`mmBlocksEnabled` gates all three dialects.** The parameter of
   `InteractiveBlocksImageURLs` is named for mm_blocks and also switches off Block Kit and
   Adaptive Cards. Attachment URLs are collected regardless of it.

5. **An empty `url` on a block is emitted; an empty one on an attachment is not.** The block
   walkers test only that the value is a string, while `appendAttachmentsImageURLs` tests each of
   its four fields for emptiness. So a block can put `""` into the URL list.

6. **A non-image accessory skips the rest of its block, not the rest of the list.** Go's
   `continue` inside the `section` case reads like a `break` at first glance because nothing
   follows it; the corpus confirms later blocks are still walked.

7. **An Adaptive Card's top-level `actions` are never walked for text**, and an `ActionSet`
   inside `body` is walked and still contributes nothing, because the item walker has no case
   for it. Two different routes to the same silence.

8. **Go declares two byte-identical pairs of functions** (`appendHumanStringsFromMmBlocks` /
   `…FromMmBlocksArray`, and `appendMmBlockImageURLs` / `appendMmBlocksArrayImageURLs`). Each
   pair is one function in the Rust port; the duplication carries no behavioural difference.

9. **The interactive strings are appended last**, after the message and all attachment text, so
   `AllStrings` with the option off is always a strict prefix of `AllStrings` with it on. Pinned,
   because a walker inserted in the wrong place would still pass a set-based comparison.

10. **The action-id half of the file cannot be ported without a markdown parser**, and porting
    the collectors around it would under-report ids — which turns into rejecting valid payloads
    one level up. Deferred as a unit; see [D-044].

## Notes — `StringInterface` is now a sorted map

`utils::StringInterface` was `HashMap<String, Value>` and is now `serde_json::Map<String, Value>`.
This is a wire decision, not a taste one, and it was made while porting `Post.Props` — the first
`StringInterface` whose fixture corpus contains characters that expose it.

Go's `encoding/json` **sorts map keys by byte value** when marshalling. A `HashMap` emits
iteration order, which is not merely unsorted but *unstable between runs*, so the same post could
serialise its props in different orders twice in one process. `serde_json::Map` is a `BTreeMap`
absent the `preserve_order` feature, so it sorts for free and `Post`/`Channel` props now match
Go's bytes.

Two consequences worth carrying forward:

- **`go_json_marshal` is no longer struct-only.** [D-022] documented that it fixes escaping but
  not key order, and was therefore unsafe on a `HashMap`. Any struct containing a
  `StringInterface` is now safe. `StringMap` is still a `HashMap`, so
  `go_json_marshal_string_map` remains the required call for that one — two aliases with
  different guarantees is its own trap, logged under [D-027].
- **Escaping is still unfixed and still silent.** `serde_json::to_string(&post)` differs from Go
  by bytes whenever a prop holds `<`, `>`, `&`, U+2028 or U+2029, while decoding to the same
  value. Cosmetic for a response body, wrong for anything stored or compared.
  `plain_serde_differs_from_go_only_by_html_escaping` pins both halves.

## Notes — IsValidHTTPURL (D-003), model/slack_compatibility.go

`IsValidHTTPURL` is the third "Go's stdlib does the real work" validator, after `IsValidEmail`
(`net/mail`) and `IsValidLocale` (`x/text/language`, still [D-001]). Go is two lines: a literal
`http://`/`https://` prefix test, then `net/url.ParseRequestURI` succeeding with a non-empty
`Scheme` and `Host`. All the behaviour is in the second line.

**Four things a reading of `net/url` predicted wrongly.** Each was corrected by the fixture's
diagnostics section, which records Go's actual error string and parsed `Host` per input:

1. **The port is everything after the FIRST colon, not the last.** `http://a:1:2` fails with
   `invalid port ":1:2" after host`. A `strings.LastIndex` reading accepts it, because `:` is a
   legal host byte and `:2` is a legal port.

2. **A `[` anywhere in a non-bracketed host is `invalid IP-literal`** — `a[b.com`, `a[b]` and
   `a[]b` all fail — even though `[` *is* in `shouldEscape`'s allow list for hosts. A stray `]`
   is fine: `http://a]b.com` is valid.

3. **A bracketed host must parse as a real IPv6 address**, not merely have a closing bracket.
   `[abc]`, `[]` and `[not an ip]` are rejected with `ParseAddr(...): unable to parse IP`, and
   `[1.2.3.4]` is rejected too — the brackets mean v6 specifically. `[::ffff:1.2.3.4]` passes.
   A `%25` zone must be non-empty, so `[::1%25eth0]` is valid and `[::1%25]` is not.

4. **`Host` includes the port**, and the emptiness test is on `Host`. So `http://:1` and even
   `http://:` are **valid** — the hostname is empty but `Host` is not. What actually fails the
   emptiness test is `http://`, `http:///path`, `http://?q` and `http://x@`.

**Three positions, three different rules**, established by sweeping 0..127 at each:

| position | rule | so this passes |
|---|---|---|
| host | a character class, plus a host-specific `%` rule | `a<b.com`, `a"b.com`, `a%80b.com` |
| path | well-formed `%` escapes only | `/a b`, `/a{b}`, `` /a`b `` |
| query | nothing is checked | `?q=%zz` |

Control bytes are rejected everywhere, by a single scan of the whole raw string before parsing.

Two more worth carrying to call sites: **`ParseRequestURI` does not strip a `#fragment`** the way
`Parse` does, so `http://x#f` is *invalid* (the `#` lands in the host) while `http://x/#f` is
fine; and a host `%` escape is rejected unless it encodes a byte >= 0x80, or is `%25` — so `%80`
is legal and `%41` is not, the reverse of the usual intuition.

### SlackCompatibleBool

**The case-insensitivity applies only to the quoted form, and not for the reason the code
suggests.** `UnmarshalJSON` lowercases its raw token and matches `true`, `"true"`, `false`,
`"false"`, which reads as though a bare `TRUE` were accepted. It is not — `TRUE` is not a valid
JSON token, so `encoding/json`'s scanner rejects it and the unmarshaler never runs. `"TRUE"` is
valid JSON and is accepted. Rust agrees, for the same reason.

Nothing else is accepted: not `1`/`0`, not `"1"`/`"0"`, not `null`, not `"yes"`, not `" true"`.
Worth contrasting with `parse_go_bool`, which `Session`'s props use and which *does* take
`1 t T TRUE True` — two bool parsers in the same crate with deliberately different rules.

The one divergence is [D-037]: Go compares the **raw** token, so `"\u0074rue"` is rejected
though it decodes to `true`. Serde sees the decoded string and accepts it.

## Notes — model/integration_action.go (chunk 1)

1. **`IsValid` accumulates every failure; nothing else in the tree does.** `PostAction` and
   `PostActionOptions` return a `*multierror.Error`, not an `*AppError`, and not the first
   failure. So the **count and order** of the messages are wire surface, not just the fact of
   failure. Ported as `utils::MultiError`, whose `Display` reproduces
   `multierror.ListFormatFunc` exactly — including the singular/plural split
   (`1 error occurred:` vs `2 errors occurred:`) and the trailing blank line.

2. **`multierror.Prefix` flattens, it does not nest.** Applied to a nested `*multierror.Error`
   it prefixes each contained message and splices them into the parent, so one invalid option
   with two empty fields contributes **two** messages, both reading `option at index 0 is
   invalid: …`. The separator is a single space and the prefix Go passes already ends in `:`.

3. **An empty integration URL yields two messages, not one.** The emptiness check and the shape
   check are independent `if`s rather than an else-branch, so `{"integration":{"url":""}}`
   reports `action must have an integration URL` **and** `action must have an valid integration
   URL`. The second message's grammar ("an valid") is Go's and is reproduced verbatim.

4. **The integration URL is not simply `IsValidHTTPURL`.** A plugin-relative path is accepted:
   `/plugins/x` and `plugins/x` pass, and so does the bare prefix `/plugins/`. But `./plugins/x`
   and `/pluginsx` do not — the test is a literal `strings.HasPrefix`, so a leading `.` defeats
   it. A non-plugin relative path like `/api/v4/x` is rejected.

5. **The action-style hex regex takes six digits only**, and it is a *different* regex from
   `channel.go`'s, which takes three or six. So `#abc` is a valid channel banner colour and an
   **invalid** action style, while `#a1b2c3` is valid for both. Case-insensitive on both sides.
   Go declares this one in `message_attachment.go`; it lives in `integration_action.rs` until
   that file lands.

6. **`Equals` silently ignores `Tooltip`, `Disabled` and `Style`** — see [D-038]. Two actions
   differing only in whether they are disabled compare equal.

7. **`Equals` panics on a nil option; `IsValid` reports it.** Same input, two behaviours. Ours
   can express neither, because `Vec<PostActionOptions>` cannot hold a nil ([D-033]).

8. **`PostActionPreserveState` partitions on key membership, `PreserveIdentityPropsFrom` on
   value nil-ness** — and they operate on overlapping prop sets. A prop stored as an explicit
   JSON `null` is therefore **retained** by the first and **not carried** by the second. Both
   are pinned; the pair is easy to conflate when porting call sites.

9. **`NormalizePostActionIntegrationFormat` never fails.** `TrimSpace` then `ToLower`, then a
   whitelist; every unrecognised input — including `""`, `"  "`, `"mm_blocks"` and `"mm block"` —
   becomes `attachment`. Both the `attachment` case and the `default` case return the same
   value, so the explicit `case` for it is redundant in Go.

10. **`MmBlocksActionCookie.Actions` is the only field in the chunk without `omitempty`**, so a
    zero cookie serialises as `{"actions":null}` while a zero `PostActionCookie` is `{}`.
    `PostActionOptions` is the mirror image: neither field has `omitempty`, so a zero one is
    `{"text":"","value":""}`.

11. **`PostActionIntegrationRequest.TeamName` has the wire key `team_domain`.** The Go field
    name and the JSON name disagree, like `FileInfo.CreatorId` → `user_id`.

## Notes — model/message_attachment.go

1. **`ts` and each field's `value` are bare `any`s validated by Go *type*, and JSON cannot
   produce the types they accept.** `Timestamp` takes `string` or `int64`; `Value` takes
   `string` or `int`. `encoding/json` decodes every number into a **float64**, so
   `{"ts": 123}` is **invalid** and so is `{"fields":[{"value": 123}]}`. No client can send a
   valid numeric timestamp. See [D-039]; this is reproduced, not repaired.

2. **`MessageAttachmentField.Equals` panics when either `Value` is nil** —
   `reflect.ValueOf(nil).Type()` panics, and a field with no `value` key is exactly that. So
   comparing two ordinary attachments crashes the Go server. Ours does not.

3. **The colour word list is not the action style list.** Attachments take
   `good`/`warning`/`danger`; `PostAction.Style` takes those three plus `default`, `primary` and
   `success`. Both share the **six**-digit hex regex, which is itself different from
   `channel.go`'s three-or-six one. Three hex-colour rules in the same package.

4. **The attachment URL checks do not accept a plugin path.** `PostAction`'s integration URL
   takes `/plugins/x`; `author_link`, `title_link`, `image_url`, `thumb_url` and `author_icon`
   are plain `IsValidHTTPURL`, so a plugin-relative path fails all five.

5. **A link with only a name is two failures, not one.** `author_link` set without
   `author_name` and with a bad URL reports both, because the checks are independent `if`s
   inside the same block. Same for `title_link`.

6. **Field failures are unprefixed; action failures are positional.** `IsValid` appends a
   field's errors bare — so two bad fields give two identical `value must be either a string or
   int` messages with nothing to say which — while actions get
   `action at index N is invalid: …`. The fields loop also runs **before** the image/thumb/footer
   URL checks, which is not the order the struct declares them in.

7. **`%v` is not JSON, and `StringifyMessageAttachmentFieldValue` stores it.** A float renders
   through Go's `%g` (`123456789` becomes `1.23456789e+08`, `1e6` becomes `1e+06`), a nil
   inside a container becomes `<nil>`, a slice becomes `[a b]` and a map becomes `map[k:v]` with
   sorted keys. Ported as `utils::go_format_v`; the float half needed its own shim because
   Rust's `Display` never uses exponent form and its `LowerExp` always does.

8. **`%g` switches to exponent form on the scientific exponent, at `< -4` or `>= 6`.** So
   `100000.0` prints as `100000` and `1000000.0` as `1e+06`; `0.0001` as `0.0001` and `0.00001`
   as `1e-05`. The exponent always carries a sign and at least two digits.

9. **`Stringify` leaves a nil value nil** rather than rendering it as `"<nil>"` — the guard is
   `if field.Value != nil`. It also drops nil attachments *and* nil fields, while
   `ParseMessageAttachment` drops only nil attachments and leaves nil fields in place. The two
   disagree, and the second can therefore write `"fields":[null,…]` into a post's props — see
   the D-033 note.

10. **`ParseSlackLinksToMarkdown` escapes nothing and its two groups differ.** The URL group
    rejects `<` and `|`; the text group rejects `>` but **accepts `|`**, so `<a|b|c>` becomes
    `[b|c](a)`. Neither matches empty, so `<a|>` and `<|b>` are left alone. A `]` in the text or
    a `)` in the URL produces malformed markdown, faithfully.

11. **`MessageAttachment.Equals` is complete** — all 17 fields — which is worth stating because
    the `PostAction.Equals` it calls is not ([D-038]).

12. **Only `actions` carries `omitempty`.** A zero attachment is
    `{"id":0,…,"fields":null,…,"ts":null}` with no `actions` key at all, and a zero field is
    `{"title":"","value":null,"short":false}`.

## Notes — model/integration_action.go (chunk 3), the `props.attachments` rewriters

`StripActionIntegrations` and `GenerateActionIds` look like they edit a post in place. They do
not: both **replace** `props.attachments` with whatever `Attachments()` decoded, and that decode
is lossy by design. All of the below are oracle results.

1. **`{"attachments": []}` comes out as `{"attachments": null}`.** `Attachments()` declares
   `var ret []*MessageAttachment` and only ever appends, so an empty result is a *nil* slice —
   and a nil Go slice marshals as `null`, not `[]`. Four inputs reach it: an empty array, and
   `attachments` holding a string, an object or a number. All four are stored as `null`, so a
   post that arrived with a malformed attachments prop leaves `pre_save` with the key present
   and null.

2. **An `attachments` prop holding an explicit JSON `null` is left alone**, because `GetProp`
   cannot tell it from an absent key and the `!= nil` guard skips the rewrite. Same wire result
   as the case above, opposite code path — worth knowing when reading the corpus.

3. **The rewrite fires even when nothing needed rewriting.** So an ordinary `pre_save` on a post
   with attachments normalises the client's payload: unknown keys vanish, an element with one
   wrongly-typed field is dropped entirely, and nil actions and fields are stripped. This is
   `Post::attachments`' documented behaviour arriving somewhere it is easy not to expect.

4. **`GenerateActionIds`' emptiness test is exact.** An id of `"  "` or `"x"` is kept, however
   unusable; only `""` is minted over. And a whitespace id survives `omitempty`, so it reaches
   the wire, while a blank one does not.

5. **`DelProp` materialises a nil `Props` into an empty map.** It builds `propsCopy` and assigns
   it unconditionally, and `props` carries no `omitempty` — so deleting any key from a post with
   `"props":null` leaves `"props":{}`. Reachable through `StripMmBlocksActionSecrets`. The Rust
   port previously skipped the assignment for a nil map and had a test asserting that; the
   `del_prop` oracle section reversed both.

6. **`StripMmBlocksActionSecrets` keeps a string and deletes everything else.** A string means
   `AddMmBlocksActionCookies` has already replaced the registry with one opaque encrypted blob,
   which is exactly what the client needs; the plaintext map, a number and an array are all
   deleted. An empty string is still a string and is kept. An explicit `null` is kept too, via
   the same `GetProp` collapse as note 2.

7. **`ToJSON` clones and `EncodeJSON` does not.** One leaves the receiver's integrations intact,
   the other destroys them permanently. Getting the pair backwards either leaks private plugin
   `context` to a client or silently drops it from a post about to be stored, so both halves are
   asserted for every case: the output *and* the receiver afterwards.

8. **`EncodeJSON` appends a newline and `ToJSON` does not.** `json.Encoder.Encode` terminates
   every value it writes; `json.Marshal` does not. A caller framing responses on that newline
   would block without it.

9. **Both marshal with Go's HTML escaping**, so `to_json` uses `utils::go_json_marshal` rather
   than `serde_json::to_string` — a post's props are exactly where `<`, `>`, `&`, U+2028 and
   U+2029 turn up. Pinned byte-for-byte by the `html_escaping_no_attachments` case, which is in
   the corpus specifically so the assertion can be byte-level rather than value-level ([D-048]).

10. **Go's `ShallowCopy` aliases the props map, and `ToJSON` is still non-mutating.** That looks
    like a bug waiting to happen and is not: `StripActionIntegrations` reaches props only through
    `AddProp` and `DelProp`, both of which swap in a *fresh* map rather than writing to the shared
    one. Measured — the receiver's integrations survive the call in Go too.

## Notes — Go's `net/url`, and `is_valid_http_url` rebuilt on top of it

`go_url.rs` is the fourth "Go's stdlib does the real work" port, after `net/mail`
(`IsValidEmail`), `x/text/language` (`IsValidLocale`, still [D-001]) and the *predicate* half of
`net/url` that [D-003] built. What forced the full parser was `MergeQueryIntoURL`, which takes a
URL apart, edits the query and puts it back — three steps with three different escaping rules.

1. **The old `is_valid_http_url` is gone and its corpus is the new parser's test.** [D-003]
   shipped ~200 lines reproducing `ParseRequestURI`'s grammar as a bool, verified over 3,529
   inputs. `is_valid_http_url` is now the two lines Go is — a prefix test, then `parse_request_uri`
   with a non-empty scheme and host — and **all 3,529 cases pass unchanged**. That is much stronger
   evidence for the parser than a fresh corpus would have been, because those cases were written
   to find grammar edges rather than to confirm one.

2. **`URL.String()` is not the identity on the input, and `RawPath` is why.** `setPath` stores the
   raw form *only* when it differs from the default escaping of the decoded path. So
   `http://x/a%41b` round-trips as `http://x/aAb` (the escaping was unnecessary and is dropped)
   while `http://x/a%2fb` survives verbatim (unescaping it would change the path's meaning). The
   same rule governs `RawFragment`. Anything not covered by those two — the host, the userinfo — is
   re-encoded canonically with no memory of how it arrived.

3. **`escape` has seven modes and they disagree on about thirty bytes.** The oracle runs all 256
   byte values through all six reachable ones rather than sampling, because the differences are
   exactly where a reading skims: `encodeFragment` leaves `!()*` unescaped and escapes `'`;
   `encodePath` escapes only `?` out of the whole reserved set; `encodeHost` allows `<`, `>` and
   `"`; and `encodeQueryComponent` escapes everything, with a space becoming `+` rather than `%20`.

4. **A URL component can hold bytes no Rust `String` can.** `unescape("%80", encodePath)` is the
   single byte `0x80`, and `https://example.com/%80` is an ordinary URL Go parses without
   complaint. `GoUrl`'s path, host, fragment and userinfo are therefore `Vec<u8>`; only the parts
   that are verbatim slices of the input (`scheme`, `opaque`, `raw_query`) are `String`. The
   fixture records every byte-valued field as base64 for the same reason.

5. **`ParseQuery` keeps what it can and reports the first failure, and `URL.Query()` discards the
   failure.** So one bad `%` escape costs one pair, not the query. A setting containing a `;` is
   both an error *and* dropped — semicolons stopped being a separator and are not silently
   tolerated either. An empty setting (`a=1&&b=2`) is skipped with no error at all.

6. **The port is everything after the FIRST colon for http and https, and after the last for
   everything else.** Go 1.26 gates this on the `urlstrictcolons` godebug, defaulting to strict for
   those two schemes only — which is why `http://a:1:2` fails as `invalid port ":1:2"` and
   `ftp://a:1:2` parses with host `a:1` and port `2`. [D-003] measured the http half; the ftp half
   is new here and is the reason `parse_host` takes the scheme as a parameter.

7. **A bracketed host must be a real IPv6 address, so the two Go checks collapse into one.** Go
   calls `netip.ParseAddr` and then rejects `addr.Is4()`; Rust's `Ipv6Addr` parser rejects every
   v4 form already. The zone is the exception — `netip` accepts `fe80::1%en0` and Rust does not —
   so it is split off first, and an empty zone is rejected by both.

## Notes — model/mm_blocks_actions.go

Every function here coerces out of an untyped `map[string]any` inside `Post.Props`, so **a type
mismatch is a silent miss rather than an error** — the same hazard `post_interactive_blocks.go`
has, with the extra sting that "no such action" is a legitimate answer. All oracle results.

1. **Three different things produce "no action", and only two are the same nil.** An entry with no
   `type` or an unrecognised one yields no spec at all; an `external` entry with **no `url`** does
   yield a spec, whose empty URL `GetAction` and `ResolveMmBlocksAction` then reject separately.
   The type match is case-sensitive, so `External` and `openurl` are both misses.

2. **`MmBlocksContextMap`'s fallback catches more than malformed input.** `null` decodes without
   error into a *nil* map and is rejected by the `m != nil` guard; `[1,2]`, `"a string"`, `7` and
   `true` are valid JSON that is not an object, so the decode errors. All five come back as
   `{"context": "<the raw text>"}`. `{}` is an object and is **not** wrapped — it stays empty. And
   `{"a":1} junk` is wrapped, because this one uses `json.Unmarshal`, which rejects trailing data
   — the opposite of `preference.go`'s theme decode, which uses a `Decoder` and accepts it.

3. **`stringMapFromPropValue` drops non-string values one at a time.** `{"k":"v","n":7}` yields
   `{"k":"v"}`, and only an all-non-string map collapses to nothing. So a query with one bad value
   still merges the good ones.

4. **`GetAction` synthesises a `PostAction` for an `external` spec and that object is wire
   surface** — it is what the click pipeline dispatches. `openURL` is never synthesised. An
   attachment action wins over the registry, matching on an exact id — including the **empty**
   id, so `GetAction("")` returns an action whose id was never set.

5. **A malformed spec URL returns nil rather than an unmerged URL.** Go's comment calls it belt
   and braces: firing the request without the static query params would be worse than a 404.

6. **`MergeQueryIntoURL` returns the input verbatim when there is nothing to merge**, which is not
   an optimisation — it is the difference between the URL being normalised and being passed
   through untouched. It also means a *malformed* URL comes back unchanged rather than reported,
   because `url.Parse` is never reached.

7. **`ParseDecryptedActionCookiePayload` succeeds on a bare `null`.** Go's `json.Unmarshal`
   returns early on a JSON null without writing to the destination and reports no error, so both
   the probe and the cookie decode "succeed" and the result is a *zero legacy cookie*. Same shape
   as [D-023]. A number, string, bool or array is an error — and the array case needed an explicit
   guard, because serde accepts a JSON array as a struct where Go does not.

## Notes — model/post_list.go, model/wrangler.go

Every item below is an oracle result. Several contradict what the source reads like.

1. **Nil and empty are different on the wire, and five methods disagree about which to
   produce.** Neither `order` nor `posts` carries `omitempty`, so a nil slice or map serialises
   as `null`. `NewPostList` and `Clone` materialise all three collections; `MakeNonNil` does
   `order` and `posts` but **not** `burn_on_read_posts`; `UniqueOrder` does only `order`;
   `StripActionIntegrations` does only `posts`. So a list can legitimately come off
   `StripActionIntegrations` holding `{"order":null,"posts":{}}`, and `ToJSON` on a zero list
   emits exactly that.

2. **`Clone` is not a copy — it normalises.** `Clone()` of a zero `PostList` is not equal to
   that list. Rust's `Clone` contract forbids that, so the port is `go_clone` and
   `#[derive(Clone)]` remains an honest copy. Getting the two confused turns `null` into `[]`
   for a client.

3. **`Clone` deep-copies the posts and aliases `HasNext`.** Measured both ways: mutating a
   cloned post does not touch the original, and writing through the cloned `*bool` does. Ours is
   an `Option<bool>` and is independent — [D-036], widened.

4. **`ToSlice`'s nil-vs-empty depends on `len(Posts)`, not on the result.** With one post and an
   empty `order` the answer is a zero-length **allocated** slice; with no posts at all it is
   nil. No Go call site can observe the difference, but the fixture records it and a reader
   would otherwise assume "empty result ⇒ nil".

5. **`ToSlice` walks `order`, so it can return nil elements and can miss posts entirely.** An
   order id with no post yields a nil `*Post`; a post with no order entry is on the wire and out
   of the slice. `AddOrder` takes an id without requiring a post, so both are reachable through
   the public API. Ported as `Vec<Option<&Post>>`.

6. **`AddPost` panics on every decoded list if the post is burn-on-read.** `BurnOnReadPosts` is
   `json:"-"`, so it is nil unless the list came from `NewPostList`, and `AddPost` assigns into
   it with no nil check. Two more methods panic on a missing post — see [D-052].

7. **`Etag` is order-independent, unlike `ChannelList::etag`.** The `v.Id > id` tie-break turns
   the running maximum into a max over the pair `(update_at, id)`, seeded with `(0, "0")` — which
   it has to be, because Go iterates a map here. The seed is reachable: a post with `update_at:
   0` and id `"p1"` wins it, one with id `"!"` does not.

8. **The first `Etag` component is `Order[0]`, which need not name a post.** An empty order
   contributes the empty string, so a zero list etags as `11.11.0..0.0` — three components, the
   first of them empty, exactly as `Team`'s zero etag has one.

9. **`ToJSON` strips a copy; `EncodeJSON` strips the receiver** — the same asymmetry `Post` has,
   and confirmed here by recording the receiver after each call. `EncodeJSON` also appends Go's
   encoder newline.

10. **`Extend` lets `other`'s post win an id collision while `order` keeps the earlier
    position.** `AddPost` overwrites the map entry, then `UniqueOrder` keeps the first
    occurrence of the id, so the post is replaced but not moved.

11. **`BuildWranglerPostList` mutates its receiver.** It runs `UniqueOrder` and `SortByCreateAt`
    before reading anything, so the caller's list comes back deduplicated and reordered. Ported
    as `&mut self` for that reason.

12. **Go's `sort.Slice` is genuinely unstable and `Order` is on the wire.** Ties agree with a
    stable sort at every size up to 20 *when the input is already grouped*; an interleaved
    20-element tie corpus comes out scrambled. See [D-051] — the one divergence in the file we
    chose not to close.

13. **`WranglerPostList` has no `json:` tags on any field**, so the wire keys are the Go field
    names: `Posts`, `ThreadUserIDs`, `EarlistPostTimestamp` (the typo is upstream's),
    `LatestPostTimestamp`, `FileAttachmentCount`. Both slices are built with `append` onto nil,
    so an empty result marshals its lists as `null` rather than `[]`.

14. **`ContainsFileAttachments` tests `!= 0`, not `> 0`.** Unreachable, since the count is only
    incremented. Reproduced anyway.

## Notes — model/post_search_results.go

Fifty-six lines, and three of its four methods are one-line wrappers. Every note below comes from
the embed being a **pointer** rather than a value, and all of them are oracle results.

1. **Which keys are present decides the embed's nil-ness, and that is wire surface.** Go allocates
   the embedded `*PostList` lazily, the first time a decode walks into it for a key it matches. So
   `{"matches":{}}` round-trips as `{"matches":{}}`, while `{"order":null}` round-trips as six
   keys — the same document plus `posts`, both post ids and `first_inaccessible_post_time`. An
   *unknown* key does not allocate it, and neither does `burn_on_read_posts`, which is `json:"-"`
   and therefore unknown here too.

2. **serde's `flatten` cannot express that**, which is why this type carries a hand-written
   `Deserialize`. `#[serde(flatten)] Option<T>` always deserialises as `Some` — the flat-map
   deserialiser answers `visit_some` unconditionally — so every `{"matches":…}` response would
   gain five keys the Go server does not send. `Serialize` **can** express it: serde's flat-map
   serialiser treats `serialize_none` as a no-op, which is exactly Go's "skip every field whose
   index path runs through a nil pointer".

3. **`PostSearchResults::ToJSON` mutates its receiver; `PostList::ToJSON` does not.** Both open
   with `x := *o`. `PostList`'s copies the struct that owns the map, so `StripActionIntegrations`
   swaps the map on the copy and the original keeps its integrations. This one copies a struct
   holding a pointer, so the strip lands on the shared list — after `results.ToJSON()`, the
   caller's own posts have lost their `integration` blocks. Two lines that read identically with
   opposite side effects, which is why the port takes `&mut self` here and `&self` there.
   `receiver_after` in the oracle is what settled it.

4. **`ToJSON`, `EncodeJSON` and `ForPlugin` crash on the nil embed** — nine of nineteen corpus
   documents, including the ordinary `{"matches":{…}}` that a search with no accessible posts
   produces. `Auditable`, the one method that is not a wrapper, has the nil check the other three
   lack. [D-054].

5. **`ForPlugin` hands back a copy that shares `Matches` with its receiver** while giving it an
   independent `PostList`. Probed by writing a key through the copy and reading it off the
   original. [D-055].

6. **`PostSearchMatches` values are nillable and the difference survives.** It is
   `map[string][]string`, so `{"p1":null}` re-emits as `null` and `{"p1":[]}` as `[]` — hence
   `Option<StringArray>` per value rather than a bare `Vec`. Keys sort byte-wise (`"A"` before
   `"a"` before `"é"`) and the map is HTML-escaped on the way out, so it must go through
   `go_json_marshal`.

7. **Go matches field names case-insensitively, so `{"ORDER":[]}` allocates the embed and fills
   `order`.** For us it is an unknown key and the embed stays nil — the two answers differ by six
   keys rather than by one. [D-040] again, and the widest consequence it has had yet; asserted
   explicitly in `an_uppercase_key_allocates_the_embed_in_go_and_not_here` rather than skipped.

## Notes — model/file_info_list.go

`PostList`'s twin. The session's real risk was porting it by copying `post_list.rs` and renaming,
so everything below is a place that copy would have been wrong. All oracle results.

1. **`ToSlice` never pre-allocates, so an empty `Order` returns a *nil* slice** — even when the
   map is full. `PostList.ToSlice` allocates whenever `Posts` is non-empty. No Go call site can
   see the difference (all of them range over the result or take its length), so both flatten to
   an empty `Vec` here, but the test asserts Go's bytes are `null` and ours are `[]` rather than
   quietly comparing them.

2. **`MakeNonNil` does not recurse.** `PostList.MakeNonNil` walks into every post and calls its
   `MakeNonNil`; this one materialises the two collections and stops.

3. **`AddFileInfo` nil-checks its map and then dereferences its argument** — the opposite order
   from `PostList.AddPost`, which checks nothing and crashes on the `BurnOnReadPosts` write.
   `AddFileInfo(nil)` panics in every one of the fourteen corpus states. [D-058].

4. **Each method materialises a different subset of the two collections, and no two agree.**
   `AddOrder` leaves `file_infos` nil; `AddFileInfo` leaves `order` nil; `Extend` materialises
   `order` through `UniqueOrder` but touches `file_infos` only if `other` had one — so
   `Extend` of two zero lists yields `{"order":[],"file_infos":null}`. The table in the module
   docs is measured, not read.

5. **`Etag` is the same function as `PostList.Etag`, character for character** — including the
   `Order[0]` prefix. This is the one place a difference was expected and there is none. It also
   corrected `post_list.rs`, whose module docs claimed that etag was "order-independent": only
   the *map* half is (the `(update_at, id)` maximum makes it immune to Go's randomised
   iteration), while reversing `order` changes the answer. The seed `(0, "0")` is reachable in
   both directions — a file with `update_at: 0` and id `zz` beats it, one with id `!!` does not.

6. **`Extend` takes every file from `other`, not only the ones in its order**, then appends
   `other`'s order and deduplicates. It ranges over a Go map, whose iteration order is
   randomised; the answer is stable only because the writes are keyed, which the oracle proves by
   running all 196 pairs twice and comparing rather than assuming.

7. **`SortByCreateAt` reproduces [D-051] exactly**, down to the permutation: at twenty elements
   with two interleaved tie groups, Go's `sort.Slice` yields
   `s15 s1 s19 s3 …` where a stable sort yields `s1 s3 s5 …`. Below thirteen it runs insertion
   sort and agrees. The create-at sequences are identical either way, which is what bounds the
   damage.

8. **There is no `Clone`, `ForPlugin`, `StripActionIntegrations` or `ToJSON`.** The type is a
   plain container — nothing here copies or sanitises, so none of `post_list.go`'s four copy
   semantics apply.

## Notes — model/post_info.go, model/post_attributes.go

Twenty-seven lines between them, one wire struct and two constants. Three things worth recording:

1. **`PostInfo` carries no `omitempty` on any of its eight fields** — the first ported type where
   that is true of the whole struct. Its zero value marshals to eight keys, so an "empty"
   response is never `{}` and nothing here is an `Option` or a skip predicate. Easy to
   over-engineer by pattern-matching on `SearchParams`, which is the exact opposite: everything
   omitempty except one field.

2. **`channel_type` is Go's `ChannelType`, a defined string type, and nothing validates it.**
   `json.Unmarshal` accepts any string into a defined string type, so `NOT_A_TYPE` decodes
   without complaint — measured, not assumed. `Channel` made the same `String`-not-enum call, but
   there `IsValid` narrows the set afterwards; here there is no method at all, so the string is
   the entire contract. `team_type` two fields down is a plain `string`, not a defined type — the
   asymmetry is upstream's and both are recorded side by side.

3. **The two `post_attributes.go` constants have no ported consumer.** They name the property
   group behind the Post Attributes feature, and `property_field.go` (735 lines) is unported.
   They land now because they are their own Go file and because a transcribed constant drifts
   silently; `PostAttributesPropertyGroupSchemaVersion` is an untyped `1` in Go, so it is `i64`
   here for the reason `SearchParams::time_zone_offset` is.

## Notes — model/search_params.go

The search box. All of the below are oracle results, and the first one found two bugs in code
that had already shipped.

1. **Go's `\d` and `\s` are ASCII; the `regex` crate's are Unicode — and both term patterns are
   *negated* classes, so the difference inverts.** A character Go does not count as a digit is
   one Go **strips**. Transcribed verbatim, `^[^\pL\d\s#"]+` would leave `٣hello` alone where Go
   returns `hello`, and would spare a NBSP that Go removes. Both patterns therefore spell the
   classes out as `[0-9]` and `[\t\n\x0C\r ]`. Note that `\s` excludes `\v` (U+000B) in Go, which
   the 169-codepoint sweep confirms and no reading of `regexp/syntax` makes obvious.

2. **`strings.Fields` two lines away splits on a *different* set.** It uses `unicode.IsSpace`, so
   `a b` is two words — while the same NBSP, had it been leading, would have been stripped
   as punctuation by a pattern that does not consider it whitespace. Rust's `split_whitespace`
   agrees with `strings.Fields` on the whole sweep, including the awkward ones (U+0085, U+1680,
   U+2007 split; U+200B and U+FEFF do not), so only the regex half needed intervention.

3. **`PadDateStringZeros` measures bytes.** `len(part) == 1` is a byte length, so a single
   Arabic-Indic digit is two bytes and is **not** padded. The shipped port counted `chars()` and
   padded it. Fixed; it had been green since the first utils session because that corpus was
   ASCII-only.

4. **`GetStartOfDayMillis` takes an unbounded offset in seconds.** `time.FixedZone` accepts any
   `int` and `SearchParams.TimeZoneOffset` comes straight off the wire, so `86400` and `1000000`
   are reachable. The shipped port built a `chrono::FixedOffset`, which stops at ±86399 and
   returned `None` for every one of them. Fixed by doing the arithmetic directly; the parameter
   is now `i64`, matching Go's `int`. `GetEndOfDayMillis` is exactly the start plus 86,399,999 —
   a fixed zone has no DST, so no day is a different length, and that holds for pre-epoch dates
   where the millisecond truncation could have gone the other way.

5. **Only two of the six date accessors fall back to the clock, and it is the *server's local*
   clock.** `GetAfterDateMillis` and `GetExcludedAfterDateMillis` set `date = time.Now()` when the
   parse fails, so an unparseable `after:` filter silently means "after tomorrow" rather than an
   error — and which day that is depends on the server's timezone ([D-008]). The other four
   return 0 or `(0, 0)`. The oracle records `uses_now` rather than a value; a clock-derived
   number in a committed fixture is wrong the next day.

6. **`in:` at the end of the input is not a flag — it is the term `in`.** A flag with an empty
   value consumes the *next* word, but at the end of input there is no next word, no branch
   fires, `isFlag` stays false, and the word falls through to the term path. There the trailing
   colon is trimmed as punctuation, leaving `in`. Same for `from:`, `on:` and the rest.

7. **`#a` is not a hashtag.** `validHashtag` needs `#`, a letter, and then at least one more
   letter or digit, so a one-letter tag is searched as a plain term and lands in a different
   params block with `ishashtag` false.

8. **A `-` immediately before an opening quote joins it, and an unclosed quote is not an error.**
   `-"a b"` is one excluded word; `a-"b"` is two. `"unclosed phrase` leaves the quote glued to
   its first word and splits the rest normally. Smart quotes (U+201C) are not quotes here — they
   are stripped as punctuation — and U+2212 is not a hyphen, so `−hello` is not an exclusion.

9. **The three date flags overwrite; the four list flags accumulate.** `on:a on:b` keeps `b`,
   while `in:a in:b` keeps both. `channel:` is an alias for `in:`, and flag names match with
   `EqualFold`, so `IN:x` and `In:x` both record the canonical `in`.

10. **`ParseSearchParams` returns one, two or three blocks**, and the third exists only when
    there are no terms of either kind but at least one filter. Every block carries the caller's
    timezone offset and none of them sets `OrTerms`, `IncludeDeletedChannels` or
    `SearchWithoutUserId` — those are for the caller to fill in afterwards.

11. **`IsSearchParamsListValid` indexes `paramsList[0]` inside its own loop** and is still safe
    on an empty list, because the loop body never runs. Measured under `recover` rather than
    reasoned about; the nil and empty lists are both valid.

12. **`splitWords` and `parseSearchFlags` are unexported**, so the oracle cannot call them and
    every parity case drives them through `ParseSearchParams`. The corpus is built so each branch
    of the two helpers changes the final output, but this is the weakest evidence in the file —
    a helper bug that cancels out in composition would not be caught.

## Notes — model/draft.go

`Draft` reads like a trimmed-down `Post` and porting it as one would be wrong in five places.
Every item is an oracle result.

1. **The message-length check runs first.** `Post.IsValid` checks the id and both timestamps
   before it looks at the message; `Draft.IsValid` checks the message and only then calls
   `BaseIsValid`. A draft that is broken in both ways reports `message_length`, where the same
   object as a post reports `id`. Two corpus cases exist solely to pin the ordering, and
   `the_corpus_proves_the_check_order` fails if they are ever dropped.

2. **`Where` is `Drafts.IsValid` — plural**, on every branch including `BaseIsValid`'s. `Post`
   uses the singular. Off the wire (`json:"-"`), on the server log.

3. **The details are `channelid=…`, not `id=…`**, and the three id branches (`user_id`,
   `channel_id`, `root_id`) carry no detail at all — the same asymmetry `channel.go` has, on
   different fields.

4. **`BaseIsValid` is exported and is its own entry point.** The store calls it directly to skip
   the message check, so it is `pub` here rather than a private half of `is_valid`, and the
   oracle records both answers for all 46 cases.

5. **Nothing validates `type`.** No accepted set, no `custom_` prefix rule, no length cap —
   `system_nope` and a thousand characters both pass. `Post` enforces all three.

6. **`priority` is a bare `StringInterface`, not `*PostPriority`.** It is measured by
   `PostPropsMaxRunes` — the same constant `props` is measured by, checked a second time — and
   otherwise never looked at, so a draft can hold a priority no post could.

7. **`props` has no `omitempty` and the other three reference fields do**, which gives four
   different wire shapes off one struct: nil `props` is `"props":null`, nil *or empty* `file_ids`
   and `priority` vanish, and an empty-but-allocated `metadata` is `"metadata":{}` — `omitempty`
   on a pointer tests the pointer, not the pointee. All four measured.

8. **`PreSave` zeroes `delete_at` unconditionally** and preserves a non-zero `create_at` (like
   `Post` and `User`, unlike `Team` and `Session`), bumping `update_at` to the clock either way.
   So `update_at == create_at` identifies a first save. `PreCommit` touches no timestamp at all.

9. **`PreCommit` materialises `props` and `file_ids` and not `priority` or `metadata`.** After it
   runs the wire form carries `"props":{}` — but `file_ids` is still absent, because `omitempty`
   drops the empty slice it just created. The nil-to-empty step is therefore invisible on the
   wire and load-bearing only in the DB.

10. **`RemoveDuplicateStrings` sorts.** The client's file order is discarded, and it is *byte*
    order, so `["b","A","a"]` becomes `["A","a","b"]`. Go's comment frames the call as a fix for
    duplicate ids and does not mention that it reorders.

11. **`maxDraftSize` is Go's signed `int`.** `0 > -1`, so a negative limit rejects even the empty
    message. Ported as `i64`; `Post::is_valid` took `usize` and cannot express it — see [D-059].

12. **The cap corpus is 800,000 characters per case.** Five such cases would have made the
    fixture 4 MB, so the padding is *described* (`{field, key, prefix, fill, count}`) and expanded
    on the Rust side rather than embedded. 80 KB instead. `behaviour_post.json` still embeds its
    two — [D-060].

## Notes — model/channel_mentions.go

The file is 96 lines and three functions, and almost all of its behaviour is one regular
expression. Every item below is an oracle result.

1. **Go's `\B` is ASCII; the `regex` crate's is Unicode.** Copying ``\B~[a-zA-Z0-9\-_]+`` into
   `Regex::new` compiles and is wrong. Measured over 164 codepoints: the set of characters that
   suppress a following mention is **exactly** `[0-9A-Za-z_]`, and not one of the 36 non-ASCII
   probes is in it — `é`, `日`, `٣`, `ｃ` (fullwidth `c`), `😀` and a combining acute all leave the
   mention findable in Go and hide it from a bare-`\B` port. The fix is `(?-u:\B)`. Third member
   of a family: `\d`/`\s` (search_params.go) and `unicode.IsLetter` (utils.go note 3) are the
   other two. See [D-062], which states the general rule.

2. **The character class is ASCII too**, and the sweep pins it in three positions (first, middle,
   last character of a name). A name is `[-0-9A-Za-z_]+`; `~chｃan` yields `ch`, not `chｃan`.

3. **`-` is not a word character but *is* a name character.** So `-~chan` finds `chan` (the `\B`
   holds) while `a~chan` finds nothing, and ` ~-` is the valid one-character mention `-`. The two
   roles of `-` are easy to conflate because they sit in the same pattern.

4. **The name is the match minus its leading `~`.** `~town-square` yields `town-square`. A port
   that returns the raw match feeds `~name` to every downstream lookup.

5. **Dedup is global, not per string.** The seen-map is allocated once outside every loop in all
   three functions, so a name repeated in a later string or a later attachment is dropped.
   Ordering is first appearance, and comparison is byte equality — `~Chan` and `~chan` are two
   distinct mentions.

6. **`ChannelMentionsFromAttachments` reads `pretext`, `text` and field *values* — not titles**,
   and not `fallback`, `author_name` or `footer` either. `Post.ChannelMentionsAll` reaches
   attachments through `AllStrings`, which **does** read titles. So the two functions disagree
   about the same attachment; both answers are pinned, including the contrast.

7. **A non-string field value is skipped, never stringified.** `{"value": 42}` contributes
   nothing, and neither does an array or an object containing a `~mention`.

8. **`(*Post).ChannelMentionsAll`'s doc comment contradicts its body.** The comment says
   "interactive blocks are omitted"; the call passes `OmitInteractiveBlocks: false`, which
   includes them. The corpus records every post under both option values, and four cases
   (`mm_blocks`, `blocks`, `cards`, all three at once) have the two disagree — so the body is
   what the port follows and a "fix" fails a test.

9. **`strings.Contains(s, "~")` is a pure short circuit.** A match requires a `~`, so skipping
   tilde-free strings cannot change the answer. Reproduced anyway; it is why a long tilde-free
   message costs nothing.

10. **Nothing matched is a nil slice, not an empty one** — `null` on the wire. None of the three
    functions can return an empty non-nil slice, so the states are indistinguishable in Go too
    for these callers. It stops being free if `FillInPostProps` stores the raw value; [D-061].

## Notes — model/scheduled_post.go

The first type in the tree with a Go **anonymous field**. Everything below is an oracle result.

1. **The embedded half comes FIRST on the wire.** Go inlines `Draft`'s nine keys ahead of
   `ScheduledPost`'s six, and `#[serde(flatten)]` emits flattened fields **last** — so a derived
   `Serialize` would silently reorder every scheduled post. `Serialize` is hand-written;
   `Deserialize` still uses `flatten`, which is safe because decoding is order-insensitive.
   `the_embedded_half_comes_first` asserts a scheduled post's JSON *starts with* its draft's JSON
   minus the closing brace, so a field added to `Draft` and forgotten fails a test — see [D-067].

2. **`Draft`'s base checks run twice per validation.** `IsValid` calls `Draft.IsValid` (message
   length *and* the base checks) and then `BaseIsValid`, which calls `Draft.BaseIsValid` again.
   Harmless, and reproduced rather than tidied.

3. **`id` is checked for emptiness only.** Unlike the draft's three ids it never reaches
   `IsValidId`, so `"nope"` is a valid scheduled-post id. Pinned.

4. **The empty-post check is `message` OR `file_ids`.** Either alone is enough, an empty *slice*
   counts as no files, and a whitespace-only message counts as a message — `len(s.Message) == 0`
   is bytes, with no trimming.

5. **`scheduledPostMaxTimeGap` is unexported and negative (-5000)**, so a `scheduled_at` up to
   five seconds in the *past* is valid. Read out of the Go source with `go/parser` rather than
   transcribed. The corpus keeps every offset at least a second clear of the boundary so the
   microseconds between building a case and validating it cannot flip an answer.

6. **`repeat_type` accepts exactly two values and one of them is `""`.** `"daily"` and `"Weekly"`
   are both rejected, and the detail carries `repeat_type=` as well as `id=`.

7. **A weekly repeat forbids files and demands a timezone.** Go's comment explains the first:
   files bind to the first post they are attached to, so later occurrences would send without
   them. `"Local"` is rejected explicitly — it loads fine and would make a persisted schedule
   depend on the server's host zone.

8. **`time.LoadLocation` is a filesystem lookup, so Go's accepted set is host-dependent.** On the
   macOS box that generated the fixture, `america/new_york`, `AMERICA/NEW_YORK`, `utc` and
   `America//New_York` are all accepted; on Linux they are not. `chrono-tz` was added for this and
   agrees with the corpus on 44 of 50 names — see [D-065], which lists all six disagreements and
   why each one is a host artifact rather than a port bug.

9. **`PreSave` clears `processed_at` and `error_code`; `PreUpdate` does not.** `PreUpdate` also
   skips `Draft::pre_save` entirely — it sets `update_at` itself and calls `pre_commit` — so
   `create_at` and `delete_at` survive an update where a save would have reset the latter.

10. **`ToPost` carries seven fields and drops seven.** No `id`, `create_at`, `update_at`,
    `delete_at`, `scheduled_at`, `processed_at` or `error_code`. An **empty** props map leaves
    `post.props` nil, because Go ranges the map and calls `AddProp` per key — zero keys, zero
    allocations.

11. **The priority conversion is all-or-nothing.** All three of `priority`, `requested_ack` and
    `persistent_notifications` must be present with the right type: Go's type assertion on an
    absent key yields the zero value with `ok=false`, so `{"priority":"urgent"}` alone is an
    error rather than a partial priority. An **empty** map is skipped and is not an error. The
    three messages interpolate the map with `%v`, which sorts its keys — and `%v` of the string
    `"true"` is `true`, so `{"requested_ack":"true"}` produces an error message that looks like it
    describes a valid bool.

12. **`ToPost` aliases in Go and then writes through the alias.** `Metadata` is assigned by
    pointer and `post.Metadata.Priority` is then set, so converting a scheduled post gives the
    *scheduled post* a typed priority it did not have. Ours clones. [D-066].

13. **`GetPriority` reads `metadata.priority`, not the draft's `priority` field.** The two are
    different fields — one typed, one an untyped client-supplied map — and only `ToPost` connects
    them. A scheduled post carrying `{"priority":{"priority":"urgent"}}` returns `None`.

14. **`RestoreNonUpdatableFields` restores six fields and `update_at` is not one of them.**
    Neither is `message`, `props`, `file_ids` or `scheduled_at` — all of those are meant to change.

15. **`SanitizeInput` never allocates a metadata.** It zeroes `create_at` and clears `embeds` on
    an *existing* metadata; a nil one stays nil.

## Notes — model/permission.go, `AppError::to_json`

1. **Fourteen permissions have an id their Go identifier would not produce, and six transpose the
   words.** `PermissionPublicPlaybookCreate` is `playbook_public_create`; `PermissionPrivatePlaybookView`
   is `playbook_private_view`; `PermissionSysconsoleReadIPFilters` is
   `sysconsole_read_site_ip_filters`, with a `site` segment the identifier does not mention. Every
   one of the fourteen is a plausible hand-translation error that **fails open**: a wrong id matches
   no role, so the check it guards answers the same way forever and nothing logs anything. This is
   why the Rust statics are named from the id rather than the identifier — a name derived from the
   value cannot disagree with it — and why `permissions_whose_id_disagrees_with_their_go_identifier`
   asserts both halves: Go's id resolves, and the plausible one does not exist.

2. **The 311 declared permissions partition exactly into the two tables.** `AllPermissions` has 282,
   `DeprecatedPermissions` 29, the intersection is empty, `AllPermissions` has no repeats, and
   nothing declared sits outside both. That is the *opposite* of [D-120]'s job types (42 declared,
   24 reachable), which is precisely why it is measured rather than assumed — the two files look
   alike and behave differently. The Rust tests treat `ALL ∪ DEPRECATED` as the declared set, and
   `declared_partitions_into_all_and_deprecated` is what licenses that.

3. **`ChannelModeratedPermissions` is not a list of permission ids.** Three of its five entries —
   `create_reactions`, `manage_members`, `manage_bookmarks` — name no permission at all; they are
   moderation *controls*. The map collapses fourteen permission ids onto them, and two ids
   (`create_post`, `use_channel_mentions`) map to themselves, which makes the distinction easy to
   miss. Go's value is a map, so it has no order; the generated table is sorted by key and the test
   asserts the sort, because the lookup is a binary search. One caller **does** range it —
   `Role.GetChannelModeratedPermissions` (role.go:713), which the next session ports — but only to
   find a key it already holds, writing into a map, so no result depends on the order. Checked
   rather than assumed: the first draft of this note claimed nothing iterated it, and that was
   wrong.

4. **`MakePermissionErrorForUser` writes `permission=` before the loop.** An empty permission list
   therefore yields a detail ending in a bare `permission=`, not one with the clause omitted. A
   port that joined the ids instead would drop eleven bytes from a string that reaches the server
   log. Both empty shapes — nil and empty slice — are in the corpus and produce the same bytes.

5. **`AppError::to_json` emitted its keys in alphabetical order, and the parity test could not see
   it.** The port built a `serde_json::Value` in order to substitute the folded `detailed_error`,
   and `serde_json::Map` is a `BTreeMap`, so every error body came out as
   `detailed_error, id, message, status_code` where Go marshals a struct in declaration order:
   `id, message, detailed_error, status_code`. The existing `utils::go_parity` test compared
   *parsed value graphs* with the comment "key order is not part of the contract" — the one
   comparison that is blind to exactly this. Measured, not argued: with the old assertion restored
   and the field order deliberately swapped, all 76 `utils::` tests still pass.

   Fixed by giving `AppError` a single wire projection (`AppErrorWire`) that both its `Serialize`
   impl and `to_json` go through, so field order is defined once, and by routing `to_json` through
   the existing `go_json_marshal` — which surfaced a **second** divergence in the same function:
   `to_json` was not applying Go's `<`, `>`, `&`, U+2028, U+2029 escaping either. The parity
   assertion is now byte-for-byte.

   **The transferable part:** a parity test that compares parsed JSON is testing the data, not the
   encoding. Where the goal is byte-identical output, assert the bytes — the value-graph comparison
   is the right tool only for a corpus whose key order genuinely varies.

6. **Nothing else in the tree applies Go's JSON escaping either** — `mm-api` serialises every
   response body with plain `serde_json::to_vec`. That is [D-121], and it is a live divergence on
   any payload containing one of the five characters; the vertical slice's byte-identical
   `/users/me` held because that payload contains none of them.

## Notes — model/role.go, `strconv.Quote`, `unicode.IsPrint`

1. **Three functions have no output order at all, and that is measured.**
   `ChannelModeratedPermissionsChangedByPatch` and `RolePatchFromChannelModerationsPatch` build
   their result by ranging a Go map, so consecutive calls with the same arguments return
   differently-ordered slices. The oracle calls each fifty times per case and records whether the
   order varied — it does, for every case with two or more results. Ours sort, the tests compare
   sets, and one assertion checks that at least one case *did* observe Go varying, so the set
   comparison cannot silently start over-accepting if upstream makes it deterministic. [D-125].

   The two functions whose order Go **does** fix are asserted in order:
   `PermissionsChangedByPatch` (it ranges the two slices) and
   `MergeChannelHigherScopedPermissions` — whose order is `AllPermissions` filtered to channel
   scope, not the role's own order and not the higher scope's. A port that rebuilt the merged list
   from the role's order passes every membership test and fails that one.

2. **`CleanRoleNames` drops a blank name but rejects a padded one.** `strings.TrimSpace` is used
   only for the emptiness test; the name that gets kept is the **untrimmed** one, so `"   "` is
   silently dropped while `" system_user "` fails validation and takes the whole call down with it.
   And on failure Go returns the **original** slice alongside `false`, not the partially cleaned
   one — so a caller that ignores the bool gets its input back rather than a truncated list.

3. **`BuiltInSchemeManagedRoleIDs` is a misnomer, and Go says so.** Eleven of its twenty-four
   entries carry `SchemeManaged: false` (`system_post_all`, `custom_group_user`, the four
   system-console roles, …). It is nonetheless the single source of truth for `IsBuiltInRole`,
   which is why the port keeps the name. The 24 ids and the 24 default roles are in bijection —
   measured, not assumed.

4. **`IsValidRoleName`'s `TrimLeft` takes a cutset, not a prefix.** `strings.TrimLeft(name, "a-z0-9_")`
   removes every leading character in the set and the check is that *nothing remains*, which makes
   it an all-characters test. The corpus enumerates all 128 ASCII bytes in second position rather
   than sampling, so the prefix misreading fails immediately.

5. **`AddAncillaryPermissions` expands exactly one level — and nothing can currently tell.** Go
   ranges the original slice header while appending to it, so an ancillary permission that is
   itself a sysconsole key would never be expanded. **No such key exists in the table**, which the
   mutation pass established rather than the reading: rewriting the port to expand recursively
   passes the entire suite. The one-level rule is therefore reproduced on the strength of the Go
   source alone, and the test that would catch a regression only arms itself if upstream adds a
   two-level key. Worth knowing before someone "simplifies" the loop.

6. **Go's `%q` is not Rust's `{:?}`, and the crate that stood in for `unicode.IsPrint` was wrong on
   5,812 code points.** role.go quotes a role name into two error messages. `go_quote` already
   existed and was right about the syntax — `\x7f` where Rust writes `\u{7f}` — but decided
   printability through the `unicode-general-category` crate, i.e. *that crate's* Unicode version.
   Measured across the whole code-point space: 5,812 disagreements with the Go toolchain for
   `IsPrint`, U+0897 among them. `IsLetter` and `IsNumber` — which `IsValidId` uses — had the same
   exposure.

   All three now come from tables emitted out of the Go toolchain, the corpus probes both sides of
   all 1,507 range boundaries, and the dependency is gone. No test changed its answer, which is the
   point: the divergence lived exactly where nothing was looking. [D-123], and the same shape as
   [D-070].

7. **Two panics we cannot reproduce, and one Go state we cannot represent.**
   `RolePatchFromChannelModerationsPatch` dereferences both `Name` and `Roles` unguarded, so a
   partial `ChannelModerationPatch` — a wire type — panics the Go handler; ours treats a missing
   `name` as matching nothing and a missing `roles` as disabling nothing, which is the direction
   that cannot remove a permission Go would have kept ([D-127]). And `RolePatch.Permissions` is
   `*[]string`, whose pointer-to-nil-slice state `Option<Vec<String>>` collapses — unreachable from
   the wire, since JSON `null` unmarshals to a nil pointer ([D-126]).

## Notes — model/scheme.go

1. **`IsValidForCreate`'s scope branches are not symmetric, and the asymmetry is the opposite of
   what the field names suggest.** Measured on a 203-cell grid — seven scopes against twenty-nine
   single-field mutations — rather than read off the switch:

   | | `team` | `channel` | `playbook` | `run` |
   |---|---|---|---|---|
   | empty channel role | reject | reject | reject | reject |
   | empty team role | reject | accept | accept | accept |
   | team role **set** | accept | **reject** | accept | accept |
   | empty/invalid playbook role | reject | accept | **accept** | accept |

   So the three **channel** roles are required under every scope, including `run`; the team,
   playbook and run roles are validated **only** under `team`; and `channel` is the only scope that
   forbids anything, requiring the three team roles to be empty while permitting any playbook or
   run role. A `playbook`-scoped scheme with a malformed `default_playbook_admin_role` is valid.

   A mutation making the playbook scope validate its own roles is caught by exactly one cell, which
   is the argument for enumerating the grid rather than picking cases.

2. **A scheme name needs two characters; a role name needs one.** `IsValidSchemeName` is
   `^[a-z0-9_]{2,64}$` — recompiled on every call, which is a performance quirk, not a semantic
   one — while `IsValidRoleName` accepts a single character. The two "name" rules differ at exactly
   that input, and a test asserts both sides of it.

3. **Go's `$` is end-of-text.** Unlike PCRE it does not also match before a trailing newline, so
   `"ab\n"` is rejected. Our predicate rejects it because the character class excludes `\n` at all —
   equivalent, but only provably so with the corpus, which probes `"ab\n"`, `"\nab"`, `"ab\ncd"` and
   `"ab\r"`.

4. **`SchemeRoles::Auditable` returns an empty map.** All three of `scheme_admin`, `scheme_user` and
   `scheme_guest` are dropped, so a membership's scheme roles never reach the audit log. Reproduced
   rather than corrected: an audit record that silently gained three fields is a divergence in the
   one stream where a difference is hardest to notice.

5. **`Scheme::Sanitize` blanks the name; `Role::Sanitize` does not.** Both blank display name and
   description. The two are asserted in a single test so the inconsistency cannot be tidied into
   consistency by someone reading only one of them.

6. **`SchemeConveyor` renames ten fields and only the `json:` tags line them up.** `TeamAdmin`
   carries what `Scheme` calls `DefaultTeamAdminRole`. The port matches the tags, and a test
   serialises both types to compare all fourteen shared keys — a swapped pair inside `Scheme()`
   would otherwise round-trip an export into the wrong slots and only surface as roles quietly
   landing on the wrong scope.

## Notes — sqlstore/role_store.go, scheme_store.go

1. **`Roles.Permissions` is one text column, and the write and read shapes are not symmetric.** Go
   writes `fmt.Sprintf(" %v", permission)` per entry (role_store.go:52), so the stored value starts
   with a space and every entry is space-prefixed; it reads back through `strings.Fields`, which
   collapses any whitespace run and drops empties. Two consequences the port has to carry: an empty
   column reads as an **empty list, not a missing one** (`strings.Fields("")` is `[]string{}`), so
   every row yields `Some(vec![])`; and the writer deduplicates while the reader does not, so a
   column containing a repeat returns the repeat. `split_whitespace` is `Fields`' rule exactly —
   `split(' ')` would return an empty first element on every real row, which is the mutation that
   proves the test corpus touches real data.

2. **No role read path filters `DeleteAt`, and the scheme paths disagree with each other.**
   `Delete` only stamps the column, and a permission check still has to resolve whatever a member's
   `Roles` column names, so all four role reads return deleted rows. For schemes, `Get` and
   `GetByName` return a deleted scheme while `GetAllPage` and `CountByScope` filter it out. Both
   reproduced; the DB test inserts a deleted role and a deleted scheme, because the Go server
   leaves none behind and without them `WHERE deleteat = 0` passes the whole suite.

3. **An empty `scope` means opposite things to the two scheme queries.** `GetAllPage` adds its
   predicate only when the argument is non-empty, so `""` is a **wildcard**. `CountByScope` has a
   bare `WHERE Scope = ?`, so `""` counts the schemes whose scope is the empty string — none of
   them. Asserted in both directions.

4. **The database is real data from the reference implementation, and that is exactly how [D-130]
   surfaced.** `Roles` holds 24 rows the Go server wrote at startup, which looked like the perfect
   oracle for the generated default-role table — until the diff showed 13 of 24 disagreeing in both
   directions. The container runs `:latest`, which is **11.10.0**; the reference tree is pinned at
   **11.11.0**. So the live database verifies *store* behaviour, not *model* content — and every
   `mm-api` parity claim in this repo has been measured against a server one minor out of step.
   That second consequence is the one worth acting on.

5. **`Schemes` is empty on Team Edition**, so the tests build their own graph — a team scheme, two
   channel schemes, two teams and two channels — because none of `ChannelHigherScopedPermissions`'
   three branches fires without one. Every id is `mmrs`-prefixed and the table is back to zero rows
   afterwards, checked rather than assumed.

   **Cleanup that only runs on the happy path is not cleanup.** The first version deleted its rows
   at the end of the test; a failing assertion panics and unwinds straight past that, which is not
   hypothetical — a mutation-testing run left three schemes behind and the *next* test failed for a
   reason unrelated to the code under test, which is precisely the failure that gets diagnosed as a
   store bug. Each test now purges by prefix at the **start**. Verified by re-running the no-op
   control immediately after a deliberately failing mutation: it survives, where before it did not.

   The tests also take a lock. They share one mutable database and two of them assert on counts and
   listings, so running in parallel makes them fail on each other's rows.

6. **`ChannelHigherScopedPermissions` is where the two servers' SQL deliberately differs.** Go
   interpolates the role-name list into the statement text with `strings.Join(roleNames, "', '")`;
   this binds a `text[]`. `IsValidRoleName` makes the hole unreachable from the API, but not from a
   row written by anything that skipped validation, and "our SQL injection matches theirs" is not a
   parity goal ([D-133]). For every legal role name the results are identical, which is what the DB
   test measures.

## Notes — app/authorization.go

1. **Fail closed is the whole file.** `RolesGrantPermission` logs and returns false when the role
   lookup fails. Every plausible "tidier" port — propagating the error and letting the caller
   `unwrap_or(true)`, or reading "no roles found" as "nothing to restrict" — turns a database blip
   into a grant. The test points the store at `127.0.0.1:1` with a lazily-connected pool, so any
   check that returns `true` has **proved it never touched the store**, and any check that reaches
   it denies.

2. **The store returns soft-deleted roles so that this function can skip them.** The two halves were
   ported in different sessions and only make sense together: `role_store.rs` has no `DeleteAt`
   filter anywhere, and `RolesGrantPermission` is where the filtering happens. The DB test soft-
   deletes a role *after* asserting the same row grants while alive, because the Go server leaves no
   deleted roles behind and there would otherwise be nothing to skip.

3. **`SessionHasPermissionToUser`'s branch order is not the intuitive one.** An empty target denies
   **before** the unrestricted check. `manage_system` grants **before** the self check — so the
   self-shortcut [D-094] relies on is the third branch, not the first. And with `edit_other_users`
   but not `manage_system`, a **system-admin target still denies**: the branch that stops an editor
   escalating. No built-in role grants `edit_other_users` without `manage_system`, so the test
   creates one; without it that branch is unreachable and a mutation deleting it survives.

4. **The higher-scoped merge must not fire when there is nothing to merge.** `GetRolesByNames`
   queries only when at least one returned role is scheme-managed, and merges only on a map hit. On
   Team Edition the map is always empty — but `MergeChannelHigherScopedPermissions` **replaces** the
   permission list with the channel-scoped subset, so a merge that fired unconditionally would strip
   `system_admin` down to channel permissions and silently drop `manage_system`. A test reads
   `system_admin` through the app layer and asserts it still holds all 100+ of them.

5. **The bug this session actually found was not in the code it was porting.** A test needed a
   non-admin user to act on, picked one from the database, and the check denied — because
   `SqlUserStore.get` could not decode the row. `jsonb` distinguishes SQL NULL from the JSON value
   `null`, the Go server writes the latter, and four of the five users then in the development
   database had `mfausedtimestamps = 'null'::jsonb`. **`GET /users/me` was a 500 for four users out
   of five**, and passed every test because the parity suite logs in as the fifth. [D-135].

   Not an artifact of the old version, either: after [D-130] recreated the volume on 11.11.0, the
   two users the server creates for itself still hold JSON `null` while the one created through the
   signup API holds `[]`. The distinction is *how the row was written*, not which release wrote it.

   The corpus lesson outlives the fix: a suite that always reads the same row is testing that row.
   The store suite now reads **every** user in the database and asserts each one decodes.

6. **Mutation testing has a failure mode of its own, and it is silent.** Twice now a mutation run
   has produced a wrong verdict because cargo reused a **stale build**: the file on disk said one
   thing and the binary under test another. The first time it showed up as a test that "failed"
   after the mutation was reverted; this time as a no-op control reported *caught* and a real
   mutation reported *skipped*, both of which are the opposite of the truth.

   Rewriting the file updates its mtime, which is normally enough — except when the write, the
   build and the test land inside the same fingerprint window. The reliable form, and what the
   verdicts in this file's row were re-measured with:

   - assert the **pristine tree is green** immediately before each mutation, and abort the run if it
     is not, because every verdict after that point is meaningless;
   - leave a clear mtime gap (a second is enough) between writing and building;
   - restore, gap, and re-verify.

   A mutation harness that can silently lie is worse than none: it produces confident evidence for
   claims nobody checked. Any verdict in this ledger from a run without the pristine-tree check
   should be treated as provisional if it ever matters.

   The verdicts for this file were re-measured under those rules, and the re-measurement is what
   found the two real gaps above. Worth stating plainly, because the headline number did not move:
   the first run also said "eleven caught". The difference is that it was wrong about two of them,
   and an all-caught run tells you nothing you did not already believe.

   One more thing the re-measurement showed: **a lean harness can create the appearance of a gap.**
   Dropping the unit suite to make the run faster turned `empty team id no longer denies` into a
   survivor, because that assertion lives in the unit tests. A survivor means "no test covers this",
   which is only true if the run actually executed the tests that do.

## Notes — sqlstore/channel_store.go (`GetMember`)

1. **Three levels of fallback, and the middle one reads a differently-named column.** A channel
   member's implied role is the **channel** scheme's default, else the **team** scheme's default,
   else the constant. Team members have only two levels, so `get_channel_roles` is not
   `get_team_roles` with different constants. Worse, the team level reads
   `TeamScheme.DefaultChannelUserRole` — the team scheme's *channel* defaults (channel_store.go:569).
   `DefaultTeamUserRole` sits right next to it in the same row, is the obvious-looking choice, and
   is wrong: it is what a *team member* falls back to. Substituting it hands channel members a
   team-scoped role name, which `RolesGrantPermission` then resolves against an entirely different
   permission set. Both mistakes fail silently — no error, just a member holding the wrong
   permissions — and both were caught only by the live oracle.

2. **The Go server does not cache `GetMember`, which is what makes the oracle possible.** This was
   checked before anything was built on it: change the `Roles` column underneath the running
   server, ask again, and the new value comes back on the next request. `GetMembers`-shaped reads
   *are* cached (`allChannelMembersForUserCache`), and `InvalidateAllChannelMembersForUser` is an
   empty function, so the same trick will not extend to the plural methods [D-137] lists. Verify it
   again per method rather than assuming.

3. **`Schemes` is empty on Team Edition, so the scheme branches had to be manufactured — and the
   permission layer nearly ate the experiment.** Inserting a scheme is easy. Inserting one whose
   role names do not exist in `Roles` makes the api4 handler return **403**, because
   `SessionHasPermissionToChannel` runs before it will answer and resolves the member's effective
   role names against that table. The oracle therefore clones the real
   `channel_user`/`channel_admin`/`channel_guest` rows under `mmrs_cs_` and `mmrs_ts_` prefixes,
   permissions included. Without that, the test measures the permission layer and reports it as a
   store divergence.

4. **The `Channels` join is INNER and that is load bearing.** A membership row whose channel is
   gone returns *nothing*. Widening it to LEFT — the reflex, since the other three joins in the
   same query are LEFT for good reasons — resurrects orphaned memberships with empty scheme
   defaults, and a permission check reading one grants against a channel that no longer exists.
   Not reachable through the API, so it is asserted directly against an inserted orphan.

5. **The oracle compares whole documents, not the fields under test.** Each case asserts our
   serialised `ChannelMember` equals Go's response body as a `serde_json::Value` graph. That costs
   nothing over comparing the five role fields and covers the other eleven — counters,
   `notify_props`, `last_update_at`, `autotranslation_disabled` — for free. It is also what
   confirms `SanitizeForCurrentUser` is a no-op on this route for a self lookup, rather than
   assuming it.

6. **`rolesInfo` is shared with the team store, in Go and here.** Go declares it once
   (team_store.go:92) and both `getTeamRoles` and `getChannelRoles` return it, so
   `channel_store.rs` imports `team_store::RolesInfo` rather than declaring a twin. The two
   functions stay separate — they are separate in Go and their fallback chains genuinely differ —
   but a field added to the struct upstream lands in one place.

## Notes — the channel permission path (`Get`, `GetAllChannelMembersForUser`, `SessionHasPermissionToChannel`)

*Multi-file session, at the user's request — CLAUDE.md's one-file-per-session rule was deliberately
set aside. Three Rust files and one correction to the previous session's ledger entry.*

1. **The previous session's premise was wrong, and the ledger said so first.** [D-134] recorded that
   `SessionHasPermissionToChannel` needed `ChannelStore.GetMember`. It does not: it calls
   `GetAllChannelMembersForUser` and `ChannelStore.Get`. `GetMember` was ported on the strength of
   that line and unblocked nothing. The port is fine — the api4 channel-member handlers do use it —
   but the *reason* it was chosen was a misreading of a ledger entry rather than of the Go source.
   The lesson is narrow and worth keeping: an entry that says "this needs X" is a claim, and the
   session that acts on it is the one that has to check it.

2. **Go has two role resolvers for one concept and both are observable — [D-142].** The finding of
   the session, and the one a tidy port would destroy. `getChannelRoles` (behind `GetMember`)
   rewrites a literal `channel_user` in the `Roles` column into the *scheme's* user role;
   `Process` (behind the plural read) leaves it alone. A single request to the running Go server
   demonstrated both at once: the response body said the member held `mmrs_dv2_channel_user`, while
   the permission gate on that same request granted `read_channel` — which only `channel_user`
   carries. They are ported as two functions, and a test asserts they still disagree.

3. **Injected sessions authenticate against the Go server**, which is the technique that made the
   whole cross-server oracle possible. Go resolves a token by reading the shared `Sessions` table,
   so a row written by `INSERT` logs in exactly as a real one does. Without it the oracle would be
   stuck asking questions as `sliceuser`, a `system_admin` for whom `manage_system` grants at
   branch 5 and every case therefore passes. This generalises to any check that needs a specific
   role shape.

4. **Go's caches shape what an oracle can measure, and two of them bit.** The `Roles` table is
   cached by name and `GetAllChannelMembersForUser` is cached per user. Mutating a role's
   permissions between two requests produced a stale answer that read as a divergence and was not
   one — an hour, and the reason the suite now creates a fresh user per case and never mutates a
   role's permissions. `GetMember` and `Get` are *not* cached in a way that interferes, verified
   before being relied on. The rule that survives: check whether the reference server caches the
   thing you are about to vary, before concluding anything from varying it.

5. **A mutation that does not compile reports "survived".** The harness greps for
   `test result: FAILED`; a build failure contains no such line. One mutation this session was
   malformed and was duly reported as a gap that did not exist. This is the same class as the
   stale-build failure recorded in the `app/authorization.go` notes, and the same fix applies:
   a verdict is only meaningful if the run it came from actually built and executed the tests.
   Re-run with the compile output visible before believing a survivor.

6. **Three of seven mutations survived the first time, and two named real holes.** Removing the
   `manage_system` branch changed nothing, because the only actor holding it was a `system_admin` —
   and `system_admin` grants `read_channel` outright, so the *next* branch granted anyway. A role
   holding `manage_system` and nothing else separates them. Likewise `includeDeleted` was
   unfalsifiable until a member of an **archived** channel existed. Both holes were invisible in a
   suite that passed, which is the argument for mutation testing stated as compactly as it can be.

7. **The empty-channel-id guard is unfalsifiable, and that is a fact about the code.** Removing it
   changes no observable behaviour: `get_channel("")` returns not-found, so the check denies either
   way. It is kept because Go has it and because it makes the intent local, but no test claims to
   cover it — the same honesty the `auth.rs` char-boundary guard gets.

8. **Archiving a channel does not revoke its members' roles.** Go passes `includeDeleted = true`
   to the membership read, so a member of an archived channel still passes a `read_channel` check.
   The reading that "deleted means gone" denies every member of every archived channel, and no test
   on a live channel can tell the difference.

## Notes — api4/channel.go (`getChannelMember`), the first gated route

1. **A whole parity suite passed while every request was proxied — [D-145].** Five green tests,
   none of which touched the code under test: a stale `mm-api` from an earlier session still held
   :8066, the new binary failed to bind and exited, and the old one forwarded everything. The
   `x-mmrs-served-by` header said `go` the entire time and nothing was reading it. It does now,
   and the fix immediately exposed that **error responses carried no marker at all** — so a 403
   from a migrated route and a forwarded 403 were indistinguishable to an operator mid-cutover
   too. This is the third instance of "the harness cannot tell whether it ran the thing", after a
   stale build and a mutation that failed to compile. The pass/fail line is not evidence on its
   own.

2. **`params` is not on the wire, and that hides a behaviour — [D-149].** `AppError` marshals five
   fields; `params` is not one. So Go's `Invalid or missing channel_id parameter in request URL.`
   names the offending segment only through its *translated* message, which we do not produce.
   Consequence: our 400s tell a client strictly less than Go's, and the validation **order** —
   channel before user — cannot be observed from any response we emit. A mutation swapping the two
   `Require*` calls survived every cross-server test. It is pinned by a unit test instead, and the
   handler grew a `validate_ids` function purely so that the order has something to be pinned on.

3. **The parity user is a system admin, which makes permission questions vacuous — [D-147].**
   `manage_system` grants at branch 5 whatever permission was asked for, so the handler could name
   any permission and every test would pass. Closed by creating a real non-admin through Go's own
   API, joining it to one of two fresh channels, and comparing the grant *and* the refusal. The
   pattern generalises: any route whose gate is permission-shaped needs an actor who can be
   refused, and `sliceuser` never can be.

4. **Comparing a row Go is still writing is a race, not a divergence.** Joining a channel triggers
   a system post and then unread-count updates, so a membership row seconds old is not quiescent:
   Go answered `mention_count: 1, last_update_at: …270` and we answered `0, …265` milliseconds
   apart, each correct for the instant it read. `fetch_both_stable` now reads Go, then Rust, then
   Go again, and compares only when Go's two reads agree. When the row never settles it says *that*
   — "the fixture never stopped changing" and "the two servers disagree" must not share a failure
   message.

5. **The suite was quietly deleting a fixture membership — [D-148].** `sliceuser` stopped being a
   member of `off-topic` at some point across two sessions. No single test does it; the full file
   does. Rather than keep hunting, every fixture the suite needs is now created and unwound by it.
   Two findings fell out: Go's `DELETE /channels/{id}` **archives** — the name stays taken, so the
   next run's create fails — and `PublicChannels` is a shadow table with its own name uniqueness,
   which turns the second failure into a 500 instead of a 400. The rule: a test that mutates rows
   it did not create has no business asserting anything about them.

6. **The permission check runs before the member is fetched, and the order is the security
   property.** Reversing it would let a caller distinguish "no such member" from "not allowed to
   look" — exactly the inference a 403 exists to prevent. Relatedly, a channel that does not exist
   is a **403** and not a 404, because the check's own `GetChannel` misses and it denies; that is
   asserted against Go rather than assumed.

7. **`me` is resolved before validation, not after.** `RequireUserId` substitutes the session's id
   for the literal `me` and *then* checks `IsValidId` (web/context.go:301). Validating first would
   400 on a request Go answers.

## Notes — api4/channel.go (`getChannelMembers`), and Go's pagination contract

1. **`per_page=0` means everything, and it takes two layers agreeing to get there.** The
   params middleware (web/params.go:234) defaults *negatives* and garbage but passes zero
   through; the store (channel_store.go:2192) adds `LIMIT` only when `Limit > 0`. Either layer
   "fixing" its half — the parser treating 0 as the default, or the store emitting `LIMIT 0` —
   turns "the whole channel" into 60 rows or none. Both halves are pinned: the parser by a unit
   table, the guard by a parity test whose mutation (`LIMIT CASE WHEN` → `LIMIT $2`) died
   against it.

2. **Pagination never 400s.** `?page=-3&per_page=nope` is the default first page of 60, not an
   error — `strconv.Atoi` failures and negatives fall to defaults silently. A port that
   validates pagination "properly" changes the wire.

3. **The list is paged over heap order.** Go's `GetMembers` adds `ORDER BY` only in its
   `UpdatedAfter` variant, which no ported route uses. Both servers page identically because
   they run the same query against the same table — a property of the shared database, not a
   wire guarantee, the same standing caveat the sessions suite's `team_members` fix recorded.

4. **`SanitizeForCurrentUser` inside a list keeps exactly one row intact.** The caller's own
   membership keeps its timestamps mid-list while every other row blanks to `-1`; the mutation
   handing the sanitiser each row's *own* id (a plausible loop transcription) un-blanks
   everything and died against three parity tests at once.

## Notes — api4/user.go (`getUserByUsername`), the sibling with everything inverted

1. **Three charsets stack, and each rejects differently.** The mux class
   (`[A-Za-z0-9\_\-\.]+`) decides *routing* — outside it is Go's 404, forwarded. The validator
   (`[a-z0-9\.\-_]+` plus length and the restricted list) decides the *400*, and it answers
   `invalid_body_param` — the body id, for a path segment (`SetInvalidParam`, not
   `SetInvalidURLParam`). The store's `lower(?)` would make the lookup case-insensitive, but
   the validator rejects uppercase first, so the fold is dead code through this route — it
   serves Go's login paths, which share the store method; ported and DB-pinned.

2. **The fetch-before-visibility inversion carries an anti-enumeration rule.** `getUser` gates
   then fetches; this handler fetches then gates, and on a fetch *failure* re-checks the
   caller's restrictions so a restricted caller gets a 403 rather than learning which usernames
   exist. Both restricted halves ride the same pre-fetch `view_members` forward as `getUser`,
   which keeps the existence-hiding answer literally Go's.

3. **The miss id is the 500's id** — `app.user.get_by_username.app_error` at both statuses, not
   `MissingAccountError` — so a client cannot correlate a missing id with a missing username
   across the two routes. Both facts pinned after the `.const` lesson one section down.

## Notes — api4/user.go (`getUser`), the route that found a three-day-old wrong guess

1. **`app.user.missing_account.const` — the id ends in the Go keyword, and the port had guessed
   `.error`.** `MissingAccountError` (app/constants.go:7) is a fossilised constant whose value
   nobody would transcribe correctly from memory. The vertical slice shipped `.error` and no
   test could catch it, because `/users/me` can never miss — the session's user always exists.
   The first route able to reach the branch surfaced it in its first parity run. The general
   lesson is [D-151]'s, now demonstrated at the app layer: an error id on an unreachable branch
   is a guess until some route can produce it, and it should be flagged as provisional rather
   than silently trusted.

2. **The serve-only-exact-ids rule replaces a sibling list that would rot.** Go's `/users/*`
   GET literals (`stats`, `known`, `autocomplete`, `tokens`) win over `{user_id:[A-Za-z0-9]+}`
   on the running server — measured, after mux registration-order reasoning proved
   inconclusive — and upstream keeps adding to them. Serving only exact 26-char ids and
   forwarding everything else means an invalid-id 400 is Go's own answer and a new upstream
   literal cannot be silently swallowed. [D-150]'s move, promoted from charset to whole rule.

3. **D-087 reaches user bodies and their etags.** Every fixture user is freshly created and
   logged in, so Go's user cache stably disagrees with the row on `update_at` — and
   `User.Etag` interpolates that field, so the two servers' etags differ exactly when their
   bodies do. The suite normalises `update_at` out of byte comparisons (as `parity_users_me`
   already did) and asserts the 304 round-trip **per server** rather than across them; a client
   behind the proxy only ever revalidates against whoever minted its etag.

4. **One ordering is transcribed, not measured: terms-of-service before the etag.** Moving the
   ToS branch after `HandleEtag` would change only the ETag *header* on a ToS-carrying body,
   and D-087 already exempts cross-server etag comparison — so no test in this deployment can
   see the difference. The doc comment on `respond_with_user` carries the constraint; a licensed
   deployment with real ToS churn is where it would bite.

## Notes — api4/team.go (`getTeamStats`), and the order of an unordered list

1. **The missing-team split is the mirror of `getChannelStats`, and the difference is which
   checker runs, not a policy.** Nothing in this handler fetches the team, and
   `SessionHasPermissionToTeam` reads only the session's memberships and roles — so a
   well-formed id that matches nothing is a **200 of zeroes for an admin** (system roles grant
   `view_team`) and a 403 for a plain user. `getChannelStats` 403s the same shape for *everyone*
   because `SessionHasPermissionToChannel` fetches the channel itself. Both splits are measured;
   a reader who assumes either route's answer for the other is wrong in a different direction
   each way.

2. **The restrictions machinery is forwarded, not ported, and the forward is unreachable
   here.** `GetViewUsersRestrictions` is nil whenever the caller holds system-wide
   `view_members` — which the default `system_user` role grants, so every caller in this
   deployment takes the fast path. The restricted case needs user-based team checks and
   dynamically-spliced restriction joins; the handler forwards it whole instead, and Go re-runs
   the id check and gate itself. Exercising that forward needs `view_members` stripped from
   `system_user` — a global role mutation no shared-database test should make — so the forward
   is transcribed, not measured, and a mutation deleting it would survive every suite. Recorded
   here rather than papered over.

3. **A no-op mutation control failed, and the finding reached two suites this session had not
   touched.** Swapping two named struct-initializer fields — provably inert — "killed" the
   sessions parity test. Cause: a session's `team_members` comes from a query with no
   `ORDER BY`; Go serves the order its **session cache** hydrated at some earlier table state
   while we re-read fresh, so after membership churn the two orders differ *stably* — which
   `fetch_both_stable`'s Go–Rust–Go window cannot detect, since Go agrees with itself both
   times. Two Go servers could disagree with each other the same way, so the order is not a
   parity property. The sessions and team-members suites now fall back to a structural
   comparison with `team_members` sorted by `team_id` — byte-exact first, order-tolerant only
   on that one list — and the control survived three consecutive runs under load before any
   api-suite verdict was trusted. Fourth instance of the rule: when a control fails, fix the
   harness before reading the tally.

4. **`teams[0]` of the admin's team list is a fixture landmine.** `GetTeamsByUserId` has no
   `ORDER BY`, so the shared helper's "first team of the fixture user" can resolve to a team
   another concurrently-running test *just created* — measured when two foreign plain users
   joined this suite's fresh fixture team mid-test and its member counts came back 4 where 2
   was seeded. Count-asserting tests now create their own home teams for outsiders and assert
   relations (`total > active`) rather than absolute numbers; the byte comparison against Go
   remains the oracle.

## Notes — api4/team.go (`getTeam`), the first system-scope fallback gate

1. **"Public" is a conjunction, and the surviving flag is the trap.** `AllowOpenInvite && Type ==
   "O"` — an invite-only team keeps its `AllowOpenInvite` column when the type changes, so either
   single-flag reading opens a team that is not public. All four cells are unit-pinned
   (`team_is_public`), and the `&&`→`||` mutation dies there.

2. **The fallback's shape is the inverse of `getChannel`'s.** There the *team* gate runs first
   and lazily; here `view_team` is computed **unconditionally** (Go assigns it before any branch)
   and `list_public_teams` — a roles-only, system-scope check — is polled only for a public team
   that `view_team` denied. Both denials name `view_team`; Go's own comment ("Fail with
   PermissionViewTeam, not PermissionListPublicTeams") is the one place upstream spells such a
   rule out, and it is pinned in `get_team_denial`.

3. **Forwarding the reviewer flag preserves an ordering it would be easy to reproduce wrongly.**
   Go reads `as_content_reviewer` *after* `RequireTeamId` and `GetTeam`, so the flag on a missing
   team is a 404, not the license 501. The forward hands Go the whole request and Go re-runs both
   steps, so the 404-before-501 ordering holds by construction — asserted for both subcases
   against the running server.

4. **A pre-existing parity flake surfaced and is fixed at the harness.** The sessions and
   team-members byte-comparisons embed `GetTeamsForUser`'s membership list, whose query has no
   `ORDER BY`; this session's team-churning fixtures reordered two rows between Go's read and
   ours, and the suite reported a divergence while both servers were right for the instant each
   read. Both tests now go through `fetch_both_stable` (Go–Rust–Go, compare only when Go's two
   reads agree) — the same fix the unread suite already wears, reaching the last two `fetch_both`
   byte-comparisons in the tree.

## Notes — api4/channel.go (`getChannelStats`), the route that never fetches its channel

1. **A missing channel is a 403, not a 404 and not a 200 of zeroes — even for the system
   admin.** The handler runs no `GetChannel`, so the intuition "nothing fetches, nothing 404s,
   the counts are just zero for anyone the gate admits" is half right and wholly wrong on the
   wire: `SessionHasPermissionToChannel`'s **own** channel fetch sits above every grant branch,
   `manage_system` included (authorization.rs:246), so the gate denies first. The parity test
   was written asserting the 200-of-zeroes and both servers refused in agreement — the wrong
   belief is preserved in the test's doc comment. The store's zero counts are real but reachable
   only from `db_channel_stats.rs`.

2. **The four counts are pairwise distinct in every fixture — 2/1/3/4 over REST, 3/1/2/4 at the
   store — applied from the `getChannelUnread` lesson rather than re-learned.** The
   count-wiring-swap mutation (`member_count: guest_count, guest_count: member_count`) was
   caught on the first run; last session the equivalent survivor cost a fixture rebuild.

3. **Team Edition cannot mint a guest, so the guest row is written straight into the shared
   database** (`SchemeGuest = TRUE`) *before either server first reads the channel* — Go's
   member-count caches have no entry for a channel nothing has asked about, so there is no
   stale copy to diverge on. Ordering, not luck: the same write after a first read would race
   Go's cache exactly as [D-087] describes.

4. **Go's error identities in the four app wrappers are load-bearing transcription traps:**
   `GetChannelGuestCount` reuses the member-count id, two wrappers pass the store method's name
   as `where`, and the pinned-post id drops the underscore its own words have. All three
   "tidy-ups" were run as mutations against the one test that pins all four wrappers; all three
   were caught.

## Notes — api4/team.go (`getTeamsForUser`), the route [D-094] said was not portable

1. **The self test is a string comparison, not a permission check.** Go compares the session's
   user id against the target before any machinery runs, so asking about oneself issues no role
   queries at all — different from `getChannelUnread`'s user gate, which runs
   `SessionHasPermissionToUser` even for self. Pinned by a closure the self case must never
   poll.

2. **A plain member's own team list is the sanitiser's mixed cell, and the first draft of the
   parity test got it wrong.** The default `team_user` role grants `invite_user` but not
   `manage_team`, so `invite_id` survives and `email` is stripped. The draft asserted both
   stripped; Go returned the invite id and the byte comparison had already passed — the failure
   was the test's explicit expectation, which is the assertion doing its job of keeping the byte
   comparison honest about *what* it is agreeing on.

3. **The two `DeleteAt` predicates are separately reachable over REST, so both are measured.**
   Archiving the team removes it from the list while its membership row survives; leaving the
   team removes it while the team survives; one fixture of each in the same test, plus an
   untouched third membership so the absences are not an empty list. Contrast [D-151], where the
   equivalent predicate needed a transcribed store-level test.

4. **`me` and the parameterised route coexist without conflict.** axum matches literals first,
   so `/users/me/teams/members` keeps its literal registration while `/users/{user_id}/teams`
   takes `me` as an ordinary value the handler resolves — the same alias rule as the channel
   routes, now on a path whose sibling is literal.

## Notes — api4/channel.go (`getChannel`), the first branching permission block

1. **The permission block branches on the fetched row, so the fetch comes first and a missing
   channel is a 404, not a 403.** The opposite of `getChannelMember`, where the permission
   check's own lookup is the fetch and a missing channel dies inside it as a 403. Both shapes are
   Go's, asserted against the running server; a reader who assumes one route's order for the
   other inverts an information-disclosure property.

2. **A non-open channel never consults the team gate.** `read_public_channel` is held team-wide
   by ordinary members, so handing private channels the open-channel fallback would leak every
   private channel to its team. Pinned in-process (`channel_read_denied` takes both gates as
   closures; tests assert the unconsulted one is never polled) because both denials answer the
   same 403 — and the denial *names* `read_channel` from both branches, which survived a mutation
   until `get_channel_denial` existed for a unit test to hold.

3. **The content-reviewer branch is detected first and forwarded, even though Go checks it
   third.** Forwarding the whole request lets Go re-run `RequireChannelId` and `GetChannel`
   itself, so every subcase — bad id, missing channel, no license — is answered by the server
   that owns the answer, and the ordering holds by construction rather than by reproduction.
   Same Strangler-inside-a-route pattern as the `flagged_post` preferences.

4. **A no-op control failed, and that was the session's most valuable verdict.** Reordering two
   SELECT columns "killed" tests three runs in a row — different tests each time, which is the
   signature of a race, not a mutation. Three DB fixture suites (`db_channel_unread`,
   `db_channel_members`, `db_channel_authorization`) purge-and-seed shared rows from concurrently
   running tests, and had been passing on scheduling luck; the harness's fresh rebuild shifted
   the timing and `teams_pkey` duplicate-insert failures fell out on **unmutated** source. All
   three are now serialised with a file-local mutex, and the control survived four consecutive
   runs before any store-suite verdict was trusted. The rule CLAUDE.md already states, now with a
   third instance: when a control fails, fix the harness before reading the tally.

5. **`fill_in_channel_props`'s delete branch is dead code through its only route, and its guard
   is not.** The store never selects `Props`, so the prop map is always `None` on entry and the
   stale-prop delete cannot fire over HTTP — ported anyway and unit-pinned ([D-151]'s shape).
   The `len(mentions) > 0` guard above it *is* behaviour: without it an emptied header would
   delete a stale prop Go leaves in place, and the unreachable-database unit test catches exactly
   that mutation.

## Notes — api4/channel.go (`getChannelUnread`), the second gated route

1. **A route's path is wire format — [D-150].** Go registers every id segment as
   `{channel_id:[A-Za-z0-9]+}`, and gorilla/mux treats that as part of the *route*: a segment with
   a hyphen in it matches nothing and falls to the mux 404 handler, which answers
   `api.context.404.app_error` with no `request_id`. axum's `{name}` matches the whole segment, so
   the same request reached our handler and got a 400 from `IsValidId`. Different status,
   different id, different body, on a request Go never routed — and `getChannelMember` had shipped
   with it a session earlier. Closed by forwarding rather than by reproducing the 404, so the
   answer is literally Go's; a reproduction would have had to remember that `Handle404` sets no
   request id.

2. **Three mutations survived a fully green parity suite, and each named a fixture that was
   asserting less than it looked like.**
   - Dropping the `TotalMsgCount - MsgCount` **subtraction** passed everything, because the reader
     had never viewed the channel, so its `MsgCount` was `0` and the subtraction was a copy. The
     fixture now has the reader catch up mid-way through.
   - Swapping `mention_count` and `msg_count` passed, because with that traffic they happened to
     be **equal**. The fixture now shapes five posts, one thread and two mentions so all four
     counters hold four different numbers, and the test asserts them pairwise distinct before
     comparing.
   - Deleting the `COALESCE(UrgentMentionCount, 0)` passed, because nothing reachable through the
     REST API can produce a NULL there. The test now writes one straight into the shared database.

   The pattern across all three: **a fixture built only out of the happy path leaves columns at
   values where the right answer and the wrong answer coincide.** Byte-for-byte equality against Go
   is not evidence when the bytes are zeroes.

3. **`DeleteAt = 0` in that query is the channel's, and it makes two routes disagree.** Go writes
   the predicate unqualified over `FROM Channels, ChannelMembers`; `ChannelMembers` has no
   `DeleteAt` column, so it can only be `Channels.DeleteAt`. `GetMember` takes an `includeDeleted`
   flag and api4 passes `true`. So on an archived channel `GET .../unread` is a **404** while
   `GET /channels/{id}/members/{uid}` is a **200**, and both are correct. Asserted in one test,
   because either half alone reads as an accident of the fixture.

4. **A permission gate that is second is also a query that is skipped.** Go returns before
   `SessionHasPermissionToChannel` when the user gate denies, and that check reads `ChannelMembers`
   and possibly `Roles`. Computing both up front would issue database work on behalf of a caller
   Go has already refused. `first_denied_permission` takes the second gate as a closure precisely
   so a test can assert it is never polled — the order, the short circuit, and which permission
   each gate names are all invisible over HTTP, since both gates answer the same 403 and
   `detailed_error` is wiped.

5. **A predicate can be dead code and still have to be ported.** The `Type IN (O,P,D,G)` filter in
   this query cannot be reached through its own route: the permission check calls
   `SqlChannelStore::Get` first, which applies the same filter and denies. Deleting it passed every
   cross-server test. It is covered now by a store-level test that inserts a `BO` channel directly
   — the first assertion in this crate that is **transcribed from Go's SQL rather than measured
   against Go**, and [D-151] says so rather than letting it pass for an oracle.

6. **The mutation harness left the mutated binary running.** Restoring the source is not restoring
   the system when the thing under test is a server: one run reported a genuine-looking 500 that
   was the *previous* mutation still bound to :8066. Third instance of [D-145]'s shape — the
   harness could not tell whether it had run the thing it claimed to.

## Notes — api4/preference.go (the three reads)

1. **A mux class and its validator can disagree in both directions.** The route is
   `{category:[A-Za-z0-9_]+}` but `RequireCategory` is `IsValidAlphaNumHyphenUnderscore(_, true)` —
   lowercase, two-plus characters, no edge `_`, hyphen *allowed*. So `display-settings` is a mux
   404 (forwarded, [D-150]) and `Display_Settings` routes and 400s. The orchestrating note for this
   session quoted the class with a hyphen; the pinned source has none — read, don't relay.

2. **Zero rows from `GetAll` is `null`, not `[]`.** sqlx's `scanAll` only `SetLen(0)`s the nil
   `model.Preferences`, and `json.Encode` of a nil slice is `null`. Unreachable for a living user
   (`CreateUser` seeds three preferences); the parity test deletes them through Go's own route to
   reach it. The empty-category 404 and the missing-name 400 sit one layer apart, so the same
   "nothing there" has three spellings across three routes.

3. **Two mutation verdicts were discarded before counting.** Dropping a `$n` from a `query_as!`
   fails to *compile*, which the harness reports as CAUGHT with no test name — a verdict about
   sqlx, not the tests. And `userid LIKE $1 || '%'` is equivalent to `=` for a full 26-char id, so
   its SURVIVED was a bad mutant. Both were re-run as compiling, genuinely wrong predicates
   (`left(userid, 25)`, `length(category) = length($2)`), which the seeded-columns store test
   caught. A harness line that reads "CAUGHT ()" should be treated as "did not run".

4. **Which permission the 403 names is not on the wire.** `make_permission_error` puts the
   permission id in `detailed_error`, which is wiped; swapping `edit_other_users` for any other
   permission survives every cross-server test and was not run for that reason. Same shape as
   `getChannelUnread`'s gate order — pinned in-process there, not yet here.


## Notes — api4/team.go (`getTeamByName`, `getTeamMember`, `getTeamMembers`)

1. **Router precedence is wire format, and the two routers disagree.** gorilla/mux tries routes
   in registration order and `BaseRoutes.Team` (`/teams/{team_id:[A-Za-z0-9]+}`) precedes
   `TeamByName`; `name` satisfies that class, so `GET /teams/name/stats` runs `getTeamStats`
   with `team_id = "name"` and 400s. axum gives a static segment precedence regardless of order.
   Only the **GET** literals collide (`image`, `stats`, `members`) — a PUT/POST-only literal like
   `patch` is a method mismatch mux skips, so Go serves it as a team name. Seven measured, three
   forwarded, pinned in a unit test so a fourth GET literal upstream is a one-line change.

2. **Same parser, same zero, opposite answer.** `per_page=0` serves the whole channel on
   `getChannelMembers` and an empty list on `getTeamMembers`, because one store guards
   `Limit > 0` and the other calls `.Limit(uint64(limit))` unconditionally. Neither handler knows;
   a port copying the channel store's `CASE WHEN` would have drifted, and the DB test holds it.

3. **A mutation harness that cannot build reports CAUGHT.** Four api-suite runs came back
   `CAUGHT ()` — empty test list — because Docker had gone down and the sqlx macros could not
   reach Postgres. A control that should have survived "died" too, which is what exposed it. The
   tally above is from the re-run with the stack up, every catch naming its test. Worth a guard
   in `mutate.sh` that distinguishes a build failure from a test failure; left as is this session.

## Notes — api4/status.go (`getUserStatus`, `getUserStatusesByIds`) and `getSessions` widened

1. **The single-user status route does not call `GetStatus`.** The orchestrating brief assumed
   it did; `api4/status.go:33` calls `GetUserStatusesByIds([]string{id})` and writes element
   zero. The difference is the route's behaviour: no row — and no user — is a 200 `offline`, not
   `app.status.get.missing.app_error`. `SqlStatusStore.Get` and `App.GetStatus` were therefore
   **not** ported; over REST only the PUT reaches them.

2. **Go's status cache is the parity risk, and it is one-directional.** `SaveAndBroadcastStatus`
   writes cache and row together, so anything set over `PUT /users/{id}/status` agrees. But
   `SetActiveChannel` (every channel view) and the websocket presence paths update the **cache
   only** — a user Go has seen recently can read `online` there and `away`/`offline` here, and
   because api4 writes the cached object with `json.Marshal` rather than `ToJSON`, Go's body can
   carry an `active_channel` key ours never will. The parity suite uses only freshly created users
   for that reason; the fixture admin is exactly the kind of user that diverges. Not fixable
   without porting the cache and its cluster messages; it is recorded here rather than in
   `TECH_DEBT.md` because it is a property of running two servers, not owed work on this one.

3. **`GET /users/status/ids` answers 404 from Go, not 405.** Measured: gorilla's method mismatch
   on the POST-only literal falls through to api4's own not-found handler. The route is
   registered POST-only and the GET is forwarded, so the answer stays Go's either way.

4. **Mutation harness verdicts are only as good as the database under them.** Four store
   mutations reported `CAUGHT ()` — empty failure list — while Postgres was briefly refusing
   connections: the `query_as!` macro failed to *compile*, which the harness counts as a
   failure. Re-run once the database answered: three caught, control survived. An empty
   parenthesis after `CAUGHT` is a compile error until proven otherwise.
## Notes — api4/channel.go (`getChannelByName`, `getChannelsForTeamForUser`)

1. **Two store functions one line apart, two team rules.** `getByNames` omits its team predicate
   when the team id is empty; `getByName` always writes `TeamId = ? OR TeamId = ''`. The by-name
   route therefore serves a DM under any team's path (measured), and an empty team id there is not
   a wildcard (DB test). Copying `get_by_names`'s `$2 = '' OR` into `get_by_name` would have
   passed every cross-server test.
2. **The by-name refusal is a 404, and `getChannel`'s is a 403, for the same caller on the same
   channel.** Both measured. The enum `ByNameRefusal` exists so the unit suite can see the split;
   over HTTP only the status shows, and the 404's id is the store's `missing` one, so a
   non-member cannot distinguish a private channel from none.
3. **`last_delete_at=+7` is zero on both servers.** `url.ParseQuery` turns `+` into a space before
   `Atoi` sees it; the first draft of the unit test expected 7 and the code was right.
4. **A list moves under a test from outside the test.** DMs are teamless and appear in every
   team's list, so another suite (or another worktree) opening a DM with the fixture user changes
   this route's body and etag mid-assertion. The suite uses a fresh team per test and reads
   Go-ours-Go with retry for both bodies and etags.
5. **The 304 carries `x-mmrs-served-by`.** The first stack run failed on the test helper, not the
   route: the Rust 304 answered `ETag` only. Added for diagnostics; Go's 304 is `ETag` only too
   and the extra header is ours on every served response.

## Notes — api4/channel.go (`getChannelsForUser`)

1. **The response is committed before the query runs.** Go writes `200` and `[` first, so every
   error after that — including the store's `not_found` for a user with no channels — lands
   *inside* the body, status line unchanged: `[{"id":"app.channel.get_channels.not_found…",
   …"status_code":404}`, no closing bracket, not JSON. Measured on Go (a fresh teamless user),
   and the one place `ApiError::into_wire` exists for. The per-team sibling 404s properly.
2. **The page boundary is invisible on the wire.** A page ends `}\n` and the next begins `,{`,
   exactly an element boundary, so `pageSize` cannot be caught by bytes — the unit tests pin it
   via the constant and the REST suite plants 100/101/200 channels to walk the loop for real,
   including the swallowed `not_found` on an exact multiple.
3. **Id order, not display-name order.** The webapp sorts client-side, so nobody notices, but the
   sibling's `ORDER BY DisplayName` is the plausible copy-paste and the parity test asserts the
   sorted ids.
4. **Two equivalent mutants, not run.** `< pageSize` → `<=` costs one extra query and the same
   bytes (the extra page is the swallowed `not_found`); dropping `$4 = 0 OR` from the
   `include_deleted` arm leaves `DeleteAt >= 0`, always true. Neither is load-bearing.
5. **A survivor can hang instead of surviving.** The inclusive-keyset mutant (`Id >= from`) made the DB test's page walk return the same row forever; the walk is now bounded so the mutant fails in milliseconds.

## Notes — api4/channel.go (`getChannelMembersForTeamForUser`)

1. **Two siblings, two gate orders, two empty cases.** `…/channels` gates user then team and
   404s on nothing; `…/channels/members` one segment deeper gates team then user and answers
   `[]`. The second gate is `manage_system` asked *through* `SessionHasPermissionToTeam`, so a
   team admin — who has `manage_team`, the plausible wrong constant — is refused, and only a
   team-admin caller can tell the two constants apart; the suite promotes one via `schemeRoles`
   to make the `manage_team` mutation die.
2. **The team filter is `Teams.Id`, not `Channels.TeamId`.** Through the LEFT join an empty or
   dangling `TeamId` both arrive as NULL and match the `IS NULL` arm, so the DM-in-every-team
   rule of the channel list holds here for a different reason, and a channel whose team row was
   deleted is listed under every team too. Writing the predicate on `Channels.TeamId` passes
   every REST test; the DB test seeds the dangling case.
3. **No deletion filter at all.** The channel list hides an archived channel by default; this
   route lists its membership unconditionally. Measured over both servers.
4. **`INNER JOIN channels` → `LEFT JOIN` is an equivalent mutant here**: an orphaned membership's
   `c.type` is NULL and `NULL NOT IN ('S')` is not true, so the type predicate drops the row the
   join would have admitted. Not run, since no test can see it; noted so nobody reads the join
   as load-bearing on its own.

## Notes — api4/user.go (`getUsersByIds`)

1. **Go's `update_at` on this route is whatever the cache last saw, and login does not refresh
   it.** `DoLogin` → `UpdateLastLogin` writes `Users.UpdateAt` (user_store.go:502) and nothing
   invalidates `userProfileByIdsCache`, which also backs `GET /users/{id}`. Measured seconds
   after a login: Go `…234499`, row `…304155`. The first parity run failed on exactly this — every
   compared user fresher in the port than in Go. An empty `PATCH` (`UpdateUser`, which does
   invalidate) makes a fixture coherent; the fixture admin, logged in once per binary and never
   patched, is kept out of every compared list. A `since` query is therefore answered against
   different timestamps by the two servers for a recently logged-in user, and that is Go's.
2. **Go's wire order is cache hits in request order, then database misses by username.** The
   port is always the query's `Username ASC`. Sets are compared; no client can rely on an order
   Go itself does not keep.
3. **The non-admin view has *more* keys than the admin's in one place.** `ClearNonProfileFields`
   sets `AuthData` to a pointer to `""`, which `omitempty` keeps, while dropping `notify_props`
   and `last_password_update`. A "fewer fields for non-admins" assertion failed against Go; the
   shape is pinned instead.
4. **`Since != 0` survived until a row with `UpdateAt = -5` existed.** Negative `since` and no
   filter coincide on every real row; the DB fixture now carries one that separates them.
5. **A stray failure during the `a3` mutation run came from `parity_channel_members_list`
   (`pages_split_cover_and_run_out_identically`), not from this suite** — cargo stops at the first
   failing binary, so the verdict was re-taken against `parity_users_by_ids` alone, where
   `since_drops_users_not_updated_after_it` caught it. Read a `CAUGHT (…)` whose named test is
   not yours as unverified.

## Notes — api4/team.go (`getTeamsUnreadForUser`)

1. **Half the route is forwarded, by query string.** `include_collapsed_threads=true` needs
   `ThreadStore.GetTeamsUnreadForUser` (three grouped queries over `Threads`/`ThreadMemberships`,
   plus `channelMembershipPredicate`) gated on two config values the Rust side cannot read. The
   handler forwards that variant whole, after the permission gate so both paths share one refusal.
   The webapp sends `true` whenever the user has CRT on, which on an `always_on` deployment is
   everyone — so this is a served route that most real traffic bypasses until the Threads store
   lands. The flag is a **string compare** against `true` (team.go:776), so `=1`/`=True` are served.
2. **`TeamId <> ''` is Go's default behaviour, not a bug to fix.** With no `exclude_team` the
   predicate hides every DM and GM (their `TeamId` is empty); with one, they all pass and fold into
   a `TeamUnread` with `team_id: ""`. Both halves are measured against Go. The sibling
   `GetTeamsForUser` in the same store file adds its exclusion conditionally, and copying that
   shape here passes every test except the DM one.
3. **Order cannot be asserted.** Go appends out of a map, so two consecutive Go answers for a
   two-team user differ about half the time and `fetch_both_stable` would never settle. The suite
   has its own `fetch_both_sorted` (settle-check and comparison both on the `team_id`-sorted
   list); the no-newline check is made on the raw bytes.
4. **The gate's plausible wrong constant needed a new actor.** `/teams` accepts
   `sysconsole_read_user_management_users`; this route wants `manage_system`. Team Edition refuses
   system-role assignment over REST, so the test writes `system_read_only_admin` into `Users.Roles`
   and re-logs-in. `system_user_manager` would have done per role.go, but the *persisted* role row
   in this database lacks the permission — the first attempt measured a 403 on the control.
5. **The DB test is transcribed for the type filter.** `NOT IN ('S')` admits a board here where
   `GetChannelUnread`'s `IN (O, P, D, G)` refuses it; neither row exists on Team Edition over REST,
   so both are planted and asserted against our own SQL, not Go's.

### Ledger row — merge into the route table above

| File | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| api4/channel.go (`getPublicChannelsForTeam`) | `mm-api/src/channels.rs` | PARTIAL | 3 pass + 7 parity + 4 DB | `GET /teams/{team_id}/channels`, `…/channels/private` and `…/channels/deleted` served. Gate is `list_team_channels` on the team for the first and third and **`manage_system` on the system** for `/private` (a team admin is refused — measured); `/deleted` asks `manage_system` a *second* time as `skipTeamMembershipCheck`, which widens rather than refuses. Offset paging (`page * per_page`, wrapping like Go's `int`): an out-of-range page and `per_page=0` are both `200 []`, an overflowing page is a 500 on both servers. The browse list joins Go's denormalised `PublicChannels` and reads team, `DeleteAt` and the sort key off **it** — see `mm-store/src/channel_store.rs::get_public_channels_for_team`. Mutations: 19 run, 19 caught, 2 controls survived. |

## Notes — api4/channel.go (`getPublicChannelsForTeam`, `getPrivateChannelsForTeam`, `getDeletedChannelsForTeam`)

1. **Three routes under one prefix, three different permission questions.** `/channels` and
   `/channels/deleted` gate on `list_team_channels` *through the team*; `/channels/private` gates
   on `manage_system` *through the system*, with no team argument, so a team admin is refused
   there and admitted on the other two. `/channels/deleted` then asks `manage_system` again and
   uses the answer as `skipTeamMembershipCheck` — a question, not a gate. Reading each gate
   rather than copying the first is the whole session: the plausible wrong constant is a
   different one for each route, and only a non-admin actor can tell them apart.
2. **The browse list believes a shadow table, not `Channels`.** `GetPublicChannelsForTeam` joins
   `PublicChannels` and writes its team predicate, its `DeleteAt = 0` and its `ORDER BY` on `pc`.
   Through REST the two tables always agree, so `pc.` → `channels.` survives every cross-server
   test; `db_channel_team_lists.rs` plants a row whose shadow says "this team, living, sorts
   first" while `Channels` says "other team, archived, sorts last", and all three mutations die
   on it. There is **no `Type = 'O'` predicate** — membership of the shadow *is* the type test,
   because `upsertPublicChannelT` deletes the row for any non-open channel.
3. **`per_page=0` is `LIMIT 0` here, and the whole channel on `getChannelMembers`.** Same parser,
   same zero, opposite answer — the second time this pair has appeared (see the `getTeamMembers`
   notes). These three stores call `.Limit(uint64(limit))` unconditionally; the member store
   guards `Limit > 0`.
4. **An overflowing page is a 500, not an empty list.** `page * per_page` is `int64` in Go and
   wraps, so `page=9223372036854775807&per_page=200` reaches the store as a negative offset and
   Postgres refuses it. Measured on both servers, with the same error id. Saturating the
   multiplication would have answered `200 []` and clamping it would have answered a *page*, so
   `page_offset` is a named function with its own unit test.
5. **`GetDeleted`'s 404 is unreachable on both servers.** Go maps `sql.ErrNoRows` to
   `ErrNotFound`, but `sqlx.Select` into a slice never returns that sentinel. Ported anyway
   (`App::get_deleted_channels`), because deleting the branch would silently promote the case to
   a 500 if the store ever gained one. A team with nothing archived is `200 []`.
6. **One mutation verdict was discarded before counting.** `OFFSET $3 + 1` fails sqlx's
   compile-time check, which the harness reports as `CAUGHT ()` — a verdict about the macro, not
   the tests. Re-run as a swapped `limit`/`offset` bind, which compiles and dies on the page
   walk. And one `CAUGHT (…)` named `malformed_ids_are_400s` from another suite; re-taken against
   `--test parity_team_channel_lists` alone, where it is caught by the right test. Both shapes
   are already in this ledger.
## Ledger rows — `getUsers` (appended; belongs in the route table above)

| File | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| store/sqlstore/user_store.go (`GetAllProfiles`, `GetProfiles`, `GetProfilesInChannel`, `GetProfilesNotInChannel`, `GetProfilesNotInTeam`, `GetEtagForProfiles`, `GetEtagForProfilesNotInTeam`) | `mm-store/src/user_store.rs` | PARTIAL | 2 pass + 8 DB | Five listings for nil view restrictions, no role filter and the default sort, plus the two etag queries. `tm.DeleteAt = 0` in the join **excludes** a former member from `in_team` and **includes** them in `not_in_team`; the `in_team` etag has no such condition at all, so a former member still moves it. `not_in_channel`/`not_in_team` take an offset (their Go caller multiplies) and carry no `DeleteAt` predicate whatsoever. |
| app/user.go (`GetUsersPage`, `GetUsersInTeamPage`, `GetUsersInChannelPage`, `GetUsersNotInChannelPage`, `GetUsersNotInTeamPage`, `GetUsersInTeamEtag`, `GetUsersNotInTeamEtag`) | `mm-app/src/user.rs` | PARTIAL | 1 pass | All five listings are one wire error (`app.user.get_profiles.app_error`, 500); only `where` differs and it is `json:"-"`. **The etag can never equal Go's**: Go interpolates two `*bool` config fields without dereferencing them, so its etag carries heap addresses — see the doc comment on `get_users_in_team_etag`. |
| api4/user.go (`getUsers`) | `mm-api/src/users.rs` | PARTIAL | 3 pass + 7 parity | `GET /users` served for five of Go's eight dispatch arms — unfiltered, `in_team`, `in_channel`, `not_in_channel`, `not_in_team` — including paging, `active`/`inactive`, the two etags and their 304. Forwarded, each by the condition Go itself dispatches on: `without_team=true`, `in_group`, `not_in_group`, any `sort`, any of `role`/`roles`/`channel_roles`/`team_roles`, `group_constrained=true` and `abac_match_only=true` on the two `not_in_*` arms, and `active=true&inactive=true`. Mutations: 21 run, 21 caught, 2 controls survived. |

## Notes — api4/user.go (`getUsers`)

1. **Go's etag for this route contains two heap addresses, and no other process can reproduce
   them.** `UserService.GetUsersInTeamEtag` (app/users/users.go:184, and its two siblings at 143
   and 188) interpolates `PrivacySettings.ShowFullName` and `ShowEmailAddress` — both `*bool` —
   with `%v` and **no dereference**, so Go answers
   `11.11.0.1787307018591.0x32494e83e753.0x32494e83e752.`. Every other api4 call site writes
   `*c.App.Config()...`. The port emits the values, so the two servers' etags differ in exactly
   those two components and each 304s only on its own — measured, and pinned from both sides.
   Behind the proxy a client only ever sees the etag of whichever server answered, so the
   consequence is a cache miss, never a wrong body.

2. **The `not_in_team` arm computes its etag from `in_team`.** `api4/user.go:1049` reads
   `GetUsersNotInTeamEtag(inTeamId, …)` inside the `notInTeamId != ""` branch. With `in_team`
   absent — the usual case — the etag is `MAX(UpdateAt).COUNT(Id)` over *every user with no team
   membership at all*, while the body lists users outside `not_in_team`. Reproduced, not
   corrected; the parity test asserts that adding `in_team` changes the etag on Go itself, which
   is what makes the mutation to the obvious parameter die.

3. **`active=true&inactive=true` is Go's `SetInvalidURLParam` without a `return`.** The handler
   carries on, serves a 200 with the full list, and `handleContextError` then appends an error
   object to the body it has already written — the `getChannelsForUser` shape. Forwarded rather
   than reproduced, and that is the rule's whole content: only *both* flags at once.

4. **`GET /users` with a NULL `Nickname` anywhere in the table is a 500 — on Go.** `sql: Scan
   error on column index 10 … converting NULL to string is unsupported`, surfaced as
   `app.user.get_profiles.app_error`; this port reads the row happily. No Mattermost server
   writes such a row, but `mm-app`'s `db_authorization` suite plants one and purges only at the
   start, so it survives between runs. Under `cargo test --workspace` the `parity_*` binaries
   run first and never see it; a standalone re-run afterwards fails four tests that are about
   something else. `parity_users_list` normalises the NULLs (it does not delete another suite's
   rows) before it starts.

5. **One shape is deliberately never compared: `page=0&per_page=100` with no other filter.**
   That is the only call on this route `LocalCacheUserStore` caches (user_layer.go:120,
   "hardcoded to the webapp call"), and the cache is stale after every login for the same reason
   `POST /users/ids` is. Every other query here — all four filtered arms included — reaches
   Postgres directly on both servers, which is why this suite needed none of the fixture
   patching the `getUsersByIds` one did.

6. **`per_page=0` means an empty page here and "everything" on the channel-member routes.** Same
   query parameter, opposite meaning: squirrel emits `LIMIT 0` for these queries, while
   `GetChannelMembers`'s store guards on `Limit > 0`. Pinned in the DB test and over HTTP.

7. **There is no `since` on this route.** `UserGetOptions.UpdatedAfter` exists and the store
   honours it, but `getUsers` never sets it — `since` belongs to `POST /users/ids`. Named here
   because the two handlers sit forty lines apart and share an options struct.
## Route rows — api4/team.go (`getTeamUnread`), appended 2026-08-21

| Go source | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| store/sqlstore/team_store.go (`GetChannelUnreadsForTeam`) | `mm-store/src/team_store.rs` | PARTIAL | 3 DB | The plural sibling's query with `TeamId = ?` in place of `TeamId <> ?` — same seven columns, same `DeleteAt = 0`, same `NOT IN ('S')` deny-list, same bare-name resolution. Both now share one row decode (`channel_unread_from_row`); `sqlx::query_as!` maps by **column name**, not position, which a reordering control proved. |
| app/team.go (`GetTeamUnread`) | `mm-app/src/team.rs` | PARTIAL | 3 pass | Singular fold. The `team_id` on the wire is the **parameter**, not any row's, so a user with nothing unread still gets an all-zero object naming the team. No collapsed-threads half exists in Go here at all, so the three `thread_*` counters are always zero. Shares `accumulate_channel_unread` with the plural fold. |
| api4/team.go (`getTeamUnread`) | `mm-api/src/teams.rs` | PARTIAL | 1 pass + 7 parity + 3 DB (+1 model fixture) | `GET /users/{user_id}/teams/{team_id}/unread` served, **nothing forwarded**. Two gates in order — `SessionHasPermissionToUser`, then `SessionHasPermissionToTeam(view_team)` — so a caller is refused for a team they cannot see *even asking about themselves*, which the plural sibling never does. Body carries a **trailing newline** (`json.NewEncoder`), where the plural sibling's `json.Marshal` + `w.Write` does not. Mutations: 12 run, 12 caught, 2 controls survived. |

## Notes — api4/team.go (`getTeamUnread`)

1. **The singular route is not the plural one with a filter, and Go does not share an
   implementation.** Different store call (`GetChannelUnreadsForTeam`, `TeamId = ?`), different
   gates (`SessionHasPermissionToUser` + `view_team`, not `manage_system`), different wire
   framing (`json.NewEncoder` → trailing newline, not `json.Marshal` + `w.Write`), and **no
   collapsed-threads branch at all** — `include_collapsed_threads` is never read, so nothing is
   forwarded and CRT changes nothing about this route. Four differences between two handlers 500
   lines apart in one file; assuming symmetry would have got every one of them wrong.
2. **A team with nothing unread is an all-zero object, not a miss.** Go builds the struct before
   the loop with `TeamId` from the *parameter*, so a well-formed team id that matches no row —
   for a caller who can see it — is a 200 whose `team_id` is the id that was asked for. Measured
   against Go on an id that exists nowhere.
3. **Gate order is not observable over HTTP, so it is pinned in-process.** `WipeDetailed`
   (model/utils.go:339) empties `detailed_error` outside dev mode and `message` is the
   untranslated third of [D-092], so a caller failing *both* gates gets a byte-identical 403
   whichever check ran first. `teams::team_unread_denied` lifts the pair out of the handler and a
   unit test asserts the order *and* that the team check is never evaluated once the user check
   refuses. The same lift as `validate_team_and_user_ids`, for the same reason.
4. **`SessionHasPermissionToUser` step 5 has no REST oracle on this deployment.** "Even
   `edit_other_users` cannot read a system admin" needs a caller holding `edit_other_users`, and
   **no persisted system role in this database carries it** — `system_manager`,
   `system_user_manager` and `system_read_only_admin` were all checked and all lack it — while
   Team Edition refuses system-role assignment over REST. That branch rests on the unit tests in
   `mm-app/src/authorization.rs`, not on measurement.
5. **A no-op mutation control found a fixture race, which is what controls are for.** The first
   `c2` run failed on `a_team_the_caller_cannot_see_…`: it byte-compared unread counters on the
   *shared* fixture team, and every other test in the binary was adding users to that same team —
   each of which posts a join message into `town-square`, which every member's team total then
   includes. This route sums over **all** the caller's channels in a team, so two tests sharing a
   team move each other's answer. Every fixture in `parity_team_unread.rs` now creates its own
   team. Had the control not been run, the suite would have been flaky-green.
6. **`sqlx::query_as!` maps result columns by name.** The control that reorders two independent
   `SELECT` columns survives, and the DB fixture's `mention_count`/`mention_count_root` are
   distinct values that would have caught a positional mapping. Worth knowing before anyone
   "tidies" a select list.
## Ledger additions — api4/role.go (appended 2026-08-21, branch `phase-2/par-roles`)

| Go file | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| app/role.go (`GetRole`, `GetRoleByName`, `GetAllRoles`) | `mm-app/src/role.rs` | PARTIAL | 6 pass | Reads go **straight to the store**: nothing on this path consults `MakeDefaultRoles`, so a patched row — not the compiled default — is what clients see, and no `DeleteAt` filter hides a deleted role. The merge that follows every read is here; `GetRolesByNames` keeps its own copy in `authorization.rs`. |
| api4/role.go (`getRolesByNames`, `getRoleByName`, `getRole`) | `mm-api/src/roles.rs` | PARTIAL | 10 pass + 11 parity | `POST /roles/names`, `GET /roles/name/{role_name}`, `GET /roles/{role_id}` served; `getAllRoles` and `patchRole` still forwarded. **An empty result is `null`, not `[]`** — the cache layer's nil slice, measured. Order is the request's (sorted), not the table's. `TrustRequester` is CSRF-only and unobservable here today. Mutations: 17 run, 17 caught, 2 controls survived. |

## Notes — api4/role.go (`getRolesByNames`, `getRoleByName`, `getRole`)

1. **`APISessionRequiredTrustRequester` changes exactly one thing: CSRF.** `TrustRequester` is
   read in a single expression (`web/handlers.go:509`) and only when a **cookie**-authenticated
   **non-GET** request has a session. Two of these three routes are GETs, so it can only ever
   matter for `POST /roles/names` from a browser without an `X-CSRF-Token` header — which Go
   accepts and `POST /users/status/ids` does not. This port has no CSRF check anywhere, so it
   agrees by having nothing to switch off. **When CSRF lands, these three must be exempt**; the
   note lives in `roles.rs`.
2. **A role on the wire is the database row, never the compiled default.** `MakeDefaultRoles()`
   seeds the table at startup and is not consulted again; `PUT /roles/{id}/patch` rewrites the row
   and every later read returns it. The parity suite patches a built-in away from its default and
   asserts the answer is the row's. Worth knowing: the seeded `system_post_all` row *already*
   diverges from its compiled default (the ancillary-permission migrations added `upload_file` and
   `use_group_mentions`), so the test picks a permission present in both to remove.
3. **An empty answer is `null`, not `[]`.** `SqlRoleStore.GetByNames` carefully returns
   `[]*model.Role{}`, and then `LocalCacheRoleStore.GetByNames` throws that away: it starts from a
   nil slice and ends `append(foundRoles, roles...)` (role_layer.go:70, :104). Appending nothing to
   nil is nil, so "no roles" is `null` on the wire — reachable with all-unknown names or a body of
   blanks that `CleanRoleNames` drops. `Vec::new()` serialising as `[]` was the invisible wrong
   answer; the handler special-cases it.
4. **Order comes from Go's cache, not its SQL.** The same cache layer returns hits **in request
   order** and appends only the misses. Names are cached for 30 minutes and every permission check
   populates it, so real traffic always sees request order — which `SortedArrayFromJSON` has
   sorted. Measured: five built-in names scrambled came back alphabetical, while the table's heap
   order for the same five is `team_user, channel_admin, system_user, system_admin, channel_user`.
   The port sorts by name and the suite warms Go's cache before each comparison.
5. **`scheme_id` is `null` for every seeded role**, not `""`. Guessed as `""` first; Go says
   otherwise.
6. **The higher-scoped merge needed a hand-built channel scheme to test at all.** It fires only for
   a scheme-managed role the `ChannelHigherScopedPermissions` UNION answered about, and that needs
   a channel scheme — enterprise-licensed, so unreachable over REST. Dropping the merge call
   entirely survived every test until the suite planted a scheme, three scheme roles and a channel
   using it directly in the database. It now pins the real behaviour: `manage_system` on the row
   vanishes (not channel-scoped), `read_channel` arrives from the higher scope, moderated
   `create_post` survives only because both list it, and the **admin** role takes everything from
   the higher scope regardless of moderation.
7. **A test fixture can take the shared stack down.** The first version of that fixture inserted
   its channel into the *fixture team* and omitted `TotalMsgCountRoot`/`LastRootPostAt`. Both are
   plain `int64` in `model.Channel`, the columns went NULL, and `GetTeamChannels` — which every
   `POST /api/v4/channels` calls through `GetNumberOfChannelsOnTeam`, with no `DeleteAt` filter —
   could no longer scan the team, 500ing channel creation for a sibling worktree. Fixed twice
   over: the fixture now owns its own team, spells out every column Go's model persists, and
   asserts through `assert_scannable_by_go` that no column backing a non-pointer Go field is NULL.
   The database's own nullability cannot answer that question — it is looser than Go's.
8. **`GET /api/v4/roles/names` is not this route.** gorilla registered `{role_id:[A-Za-z0-9]+}`
   before the literal `names`, so a GET there is `getRole("names")` and 400s on `IsValidId`.
   Registering the literal POST-only keeps that true through the method fallback and the proxy.
9. **A pre-existing flake, not ours.** `parity_channel_members_list::pages_split_cover_and_run_out_identically`
   failed once during a full-crate run here on a paging order tie and passes on its own. It stops
   cargo before later binaries, so two mutation verdicts named its suite and had to be re-taken
   against `parity_roles` alone.

## Ledger additions — api4/post.go `getPost` (appended 2026-08-22, branch `phase-2/par-posts`)

| Go file | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| store/sqlstore/post_store.go (`GetSingle`), post_priority_store.go, post_acknowledgements_store.go | `mm-store/src/post_store.rs` | PARTIAL | 5 unit + parity | Eighteen columns plus a correlated `ReplyCount`; six `Post` fields are **never selected** and their zero values are on the wire. NULL `props`/`fileids` stay `null`, they do not become `{}`/`[]`. |
| store/sqlstore/reaction_store.go (`GetForPost`) | `mm-store/src/reaction_store.rs` | PARTIAL | parity | Two `COALESCE`s, and the `DeleteAt` one appears in the predicate as well as the select list. `ORDER BY CreateAt` is wire surface. |
| store/sqlstore/emoji_store.go (`GetMultipleByName`) | `mm-store/src/emoji_store.rs` | PARTIAL | parity | `DeleteAt = 0` lives in Go's **shared** `emojiSelectQuery`, not in the method body. No `ORDER BY` — see note 4. |
| store/sqlstore/file_info_store.go (`GetByIds`) | `mm-store/src/file_info_store.rs` | PARTIAL | parity | `ORDER BY CreateAt DESC` is then overwritten by `orderFileInfosByID`; it only decides the tail. |
| app/post.go + app/post_metadata.go (`GetSinglePost`, `GetPostIfAuthorized`, `PreparePostForClientWithEmbedsAndImages`, `SanitizePostMetadataForUser`) | `mm-app/src/post.rs` | PARTIAL | 9 unit + parity | The pipeline is a **total function that can refuse**: shapes needing the markdown parser or a link fetch return `PrepareError::Unreproducible` and the handler forwards. `REFUSED_PROPS` and `message_may_contain_a_link` are the predicate. |
| api4/post.go (`getPost`) | `mm-api/src/posts.rs` | PARTIAL | 8 parity | `GET /api/v4/posts/{post_id}` served for reproducible shapes, forwarded otherwise. ETag, 304, `include_deleted` + `manage_system` gate all ported. Mutations: 23 run, 23 caught, 2 controls survived. |

## Notes — api4/post.go (`getPost`)

1. **The route declines, on purpose, and the suite tests both halves.** `PreparePostForClient`'s
   embed and image stages need Go's markdown parser (`getFirstLink`, `getImages` — [D-044]) and a
   live HTTP fetch of the link (`getLinkMetadata`), whose answer depends on what the remote host
   says at that moment. Neither is reproducible, so `mm_app::post` refuses those shapes and
   `posts.rs` forwards them. `parity_post_get::shapes_with_links_are_forwarded_and_still_match`
   asserts `x-mmrs-served-by: go` for each; the mirror test asserts ordinary messages are still
   served, or the route would forward everything and be worth nothing.
2. **The refusal predicate is a deliberate superset.** `message_may_contain_a_link` looks for
   `://`, `www`, `![` and `<`. Mattermost's autolinker recognises only a `www\d{0,3}\.` host, a
   scheme plus `://`, and the angle-bracket form — it has **no email rule**, so
   `someone@example.com` is not a server-side link even though the webapp renders one, and a
   markdown `[text](url)` is not an autolink either. Widening the needles is free; narrowing them
   is a wire-format bug no comparison of served shapes would catch.
3. **`preparePostFilesForClient` runs twice, and the second call is load-bearing in a window Go
   normally closes behind itself.** The deleted-post short circuit replaces the metadata between
   the two passes, so only the second one can put `files` back — but `DeletePost` soft-deletes a
   post's file infos from a **goroutine** (app/post.go:2013), so by the time a client asks, the
   files are usually gone and the metadata is `{}` either way. Measured, not assumed: the first
   fixture asserted `files` were present on a deleted post and failed. The suite now waits for
   that goroutine and restores the rows, which holds the transient state still and makes the
   second pass observable. See `App::prepare_post_for_client_with_embeds_and_images`.
4. **Two Go queries have no `ORDER BY` and are reproduced that way.** `GetMultipleByName` (emoji)
   and `GetForPost` (acknowledgements) both take Postgres's own row order. Adding an `ORDER BY`
   here would make us *disagree* with Go whenever its unordered scan came back differently, so a
   post with two or more custom emoji, or two or more acknowledgements, is not order-stable across
   the two servers. Not observed to flake — the fixtures use one of each — but it is a real
   divergence and the reason the rich-post fixture does not use two acknowledgements.
5. **`ServiceSettings.PostPriority` and `EnableCustomEmoji` default to `true`.** Every config
   field ported before this one defaults to `false`, and copying that pattern would have silently
   dropped `metadata.emojis` and `metadata.priority` from every response. `IsPostPriorityEnabled`
   reads the setting and **no licence at all**, so the branch is live on Team Edition even though
   *creating* a priority row over REST is licence-gated.
6. **Two fixtures had to be written straight to the database.** `POST /posts` with
   `metadata.priority` answers `license_error.feature_unavailable` here, and there is no other way
   to author a `PostsPriority` or `PostAcknowledgements` row. The read path is not licence-gated,
   so the suite plants the rows and both servers read them through their own store. Same technique
   for the NULL `Reactions.UpdateAt`/`DeleteAt` the two `COALESCE`s exist for — nothing reachable
   through REST writes one.
7. **`Post.PreSave` sorts `FileIds`.** `o.FileIds = RemoveDuplicateStrings(o.FileIds)`
   (post.go:740) sorts before deduplicating, so a post stores its attachments in alphabetical id
   order regardless of what the client sent — and `orderFileInfosByID` then reorders the store's
   `CreateAt DESC` result into *that*. Whether the two orders differ is a coin flip on random
   ids, and on the first run they agreed, which made the reordering invisible to a fixture that
   looked like it was testing it. The suite now plants `FileIds` directly to force them apart.
8. **`ReplyCount` is a thread count.** The subquery keys on `Posts.RootId` when the post is a
   reply and `Posts.Id` when it is a root, so a root reports its replies and a reply reports the
   same number including itself. The fixture uses three replies with one deleted so that
   "children only", "thread including the root" and "forgot `DeleteAt = 0`" each give a different
   wrong answer.
9. **The cloud-limit 403 and its header are unreachable and are not written.**
   `GetLastAccessiblePostTime` returns `0` without a licence carrying a `PostHistory` limit, so
   `app.post.cloud.get.app_error` never fires and `First-Inaccessible-Post-Time` is never set.
   Reproducing the header blind, against an oracle that cannot be run, would be a guess; the
   handler's doc comment says where it goes if a Cloud licence ever lands.
10. **`GetPostIfAuthorized` duplicates a fallback that cannot grant anything.**
   `HasPermissionToReadChannel` already falls back to `read_public_channel` for open **and**
   open-board channels; `GetPostIfAuthorized` repeats it for `O` only. The repetition changes
   nothing about *who* is allowed in — only which permission id the 403 names, which clients read.
11. **The mutation harness was lying, and the controls are what caught it.** `scripts/mutate.sh`'s
   `api` arm ran *every* mm-api suite on every mutation. Narrowing it with
   `--tests --test parity_post_get` looked right and was not: `--test X` **replaces** `--tests`
   rather than narrowing it, so cargo built all 32 targets and the first full run came back
   "25 caught, 0 survived" — with **both no-op controls caught**, which is the only reason the
   tally was thrown away instead of reported. `MUTATE_API_SUITE` now selects the binary properly,
   and a non-zero exit with no failing test is reported as a HARNESS FAULT rather than as CAUGHT.
   A mutation must also *compile*: the first `include_deleted` mutation left sqlx's `$2` unused
   and only ever produced a build error. See `scripts/mutations/post_get.plan`.
