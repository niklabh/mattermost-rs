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
| model/user_terms_of_service.go | `mm-model/src/user_terms_of_service.rs` | DONE | 5 pass | Whole file, pinned branch-by-branch against `fixtures/behaviour_terms_of_service.json`. **All three `IsValid` branches report the *user* id as their detail**, including the one about `terms_of_service_id`. |
| store/sqlstore/user_terms_of_service.go (`GetByUser`, `Save`, `Delete`) | `mm-store/src/user_terms_of_service_store.rs` | DONE | 3 DB | One row by PK; `Save` is an `UPDATE`-then-`INSERT` upsert, so re-accepting rewrites `CreateAt`. `Delete` matches **both** columns and never checks the row count, so rejecting a revision you never accepted is a silent success. |
| app/user_terms_of_service.go (`GetUserTermsOfService`, `SaveUserTermsOfService`) | `mm-app/src/user_terms_of_service.rs` | DONE | 1 pass + 1 DB + 5 parity | The 404 (`no_rows.` inserted into the 500's id) is a **normal outcome** at its one call site. `accepted` picks between two different store calls, not a column value, and only the save branch lets `IsValid`'s 400 through. Closes [D-083]. |
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
| api4/user.go (`revokeSession`, `revokeAllSessionsForUser`, `revokeAllSessionsAllUsers`, `handleDeviceProps`, `attachDeviceIds`) | `mm-api/src/sessions.rs` | DONE | 16 pass + 11 parity | All four write routes. The gate differs per route and is the whole substance: the two `{user_id}` routes are `SessionHasPermissionToUserOrBot` (403 `edit_other_users`), the all-users route is `SessionHasPermissionTo(manage_system)` with **no self branch**, and `handleDeviceProps` has none at all. The doc comments carry the check *orders*, which decide which of five error ids a client gets. |
| app/session.go (`RevokeAllSessions`, `RevokeSessionsFromAllUsers`, `RevokeOtherSessionsFor…DeviceId`, `SetExtraSessionProps`, `AttachDeviceId`, `SetSessionExpireInHours`) | `mm-app/src/session.rs` | PARTIAL | 11 pass | `RevokeAllSessions` moved here from `mm-app/src/bot.rs`, which is what [D-283] asked for. Two orderings are load-bearing and tested rather than commented: access data is deleted **before** sessions (a token outlives its session, so the other order is a relogin window), and the mobile expiry is measured from `CreateAt`, not from now, on any server where `ExtendSessionLengthWithActivity` is off. |
| store/sqlstore/session_store.go (`RemoveAllSessions`, `UpdateDeviceId`, `UpdateProps`), oauth_store.go (`RemoveAllAccessData`) | `mm-store/src/session_store.rs`, `oauth_store.rs` | PARTIAL | — | `UpdateDeviceId` writes **four** columns, and the fourth is `ExpiredNotify = false` — reset with the new expiry, because the flag records that the *old* one was already announced. Both device columns are always written, so the caller must pass the existing value back for the one it is not changing; see `attach_device_ids`. |
| app/config.go (`GetCookieDomain`), `SessionLengthMobileInHours` | `mm-app/src/config.rs` | PARTIAL | 4 pass | The mobile length's default is a **two-step cascade**, not a constant: hours, else days × 24, else `isUpdate ? 180 : 30` days — so 4320 on any upgraded server and 720 on a fresh one, confirmed against what the live Go server wrote. `cookie_domain` is `url.Hostname()` behind a default-off flag; the hostname split needs a **numeric** port to strip one, which is why it is oracle-driven. |
| — (tooling) | `reference/dump/behaviour_session_write.go` → `fixtures/behaviour_session_write.json` | DONE | 6 diff tests | Four corpora, every one calling the **real** upstream function rather than a transcription: `semver.StrictNewVersion` (60 cases, 23 accepted), `url.Hostname` over raw hosts and over SiteURLs, and `net/http`'s own `Cookie.String` over the ten shapes `attachDeviceIds` can emit. The cookie corpus caught two bugs a hand-written assertion had already passed. |
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
| api4/user.go (`getUsers`) | `mm-api/src/users.rs` | PARTIAL | 3 pass + 7 parity | `GET /users` served for five of Go's eight dispatch arms — unfiltered, `in_team`, `in_channel`, `not_in_channel`, `not_in_team` — including paging, `active`/`inactive`, the two etags and their 304. Forwarded, each by the condition Go itself dispatches on: `without_team=true`, `in_group`, `not_in_group`, any `sort`, any of `role`/`roles`/`channel_roles`/`team_roles`, `group_constrained=true` and `abac_match_only=true` on the two `not_in_*` arms, and `active=true&inactive=true`. Mutations: 23 run, 23 caught (3 only after the fixture gained the requests that separate the gate's three comparisons), 3 controls survived. |

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
| store/sqlstore/emoji_store.go (`GetMultipleByName`) | `mm-store/src/emoji_store.rs` | PARTIAL | parity | `DeleteAt = 0` lives in Go's **shared** `emojiSelectQuery`, not in the method body. No `ORDER BY` — see note 4. `Save` and `Delete` landed with the emoji writes; the table's uniqueness is `(Name, DeleteAt)`, so a name is free again after a delete. |
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
---

## `GET /api/v4/teams` — `getAllTeams` (2026-08-22)

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/teams.rs` — `get_all_teams`, `all_teams_opts` | DONE, served from Rust |
| app | `crates/mm-app/src/team.rs` — `get_all_teams_page`, `get_all_teams_page_with_count`, `team_membership_access_control_enabled` | DONE |
| store | `crates/mm-store/src/team_store.rs` — `get_all_page`, `analytics_team_count` | DONE |
| model | `mm-model::team::TeamsWithCount` (already ported) + `fixtures/teams_with_count.json` | fixture added |

Tests: 6 unit (`teams::tests`, the permission matrix), 1 unit (`team::tests`, the ABAC constant),
2 model (fixture round-trip and the empty shape), 10 DB (`crates/mm-store/tests/db_team_all_page.rs`),
16 cross-server (`crates/mm-api/tests/parity_teams_all.rs`). Mutations: 18 run, 18 caught, 2
controls survived.

The three things a reader would otherwise get wrong, each pinned in the doc comment on the thing
it constrains:

1. **`per_page=0` is an empty page here.** Squirrel renders `Limit(0)` as a literal `LIMIT 0`, so
   `?per_page=0` returns `[]` — the exact opposite of `getChannelMembers`, where the same
   parameter reaches a store-side `Limit > 0` guard and means *no limit*. Same parser, two
   meanings, one route apart. Measured against the running Go server both ways.
2. **`GetAllPage` has no `DeleteAt` filter, and `AnalyticsTeamCount`'s reads backwards.** Archived
   teams are listed *and* counted: the count's deleted filter engages only on an explicit
   `IncludeDeleted = false`, and `getAllTeams` never sets it. Adding `deleteat = 0` would look
   like a bug fix and would shorten every System Console team page.
3. **The count ignores `exclude_policy_constrained`.** Only `DeleteAt` and `AllowOpenInvite` reach
   that query, so `?exclude_policy_constrained=true&include_total_count=true` reports a total that
   counts the teams its own list omits. Making the two agree would be the divergence.

Also worth knowing:

- **The neither-permission 403 is not `SetPermissionError`.** Go builds it inline with
  `api.team.get_all_teams.insufficient_permissions`; the `exclude_policy_constrained` gate a few
  lines above it *is* `SetPermissionError` and answers `api.context.permissions.app_error`. Two
  different ids from one handler, both measured.
- **`allowopeninvite` is nullable and Go filters with `=`.** A team whose column is NULL is in
  neither the public-only nor the private-only listing. Writing the private filter as
  `IS NOT TRUE` — the natural-looking Rust — is a real divergence; the DB suite seeds a NULL row
  because the REST create path cannot produce one.
- **The ABAC directory surface is dark and stays unwritten.**
  `FilterNonQualifyingTeamsForUser`, `AnnotateRecommendedTeamsForUser` and `for_directory` all
  short-circuit on `TeamMembershipAccessControlEnabled()`, which is false here — the feature flag
  defaults *true* (feature_flags.go:173), so the licence is the only term that matters and the
  deployment image is `mattermost-team-edition` with zero rows in `Licenses`. That constant is a
  unit test, not a comment, so flipping it fails rather than silently turning a branch on.
- **The permission matrix needed roles that do not exist.** No pair of built-in roles gives
  private-without-public, so `parity_teams_all.rs` writes four rows into `Roles` and assigns each
  to its own user through Go's `PUT /users/{id}/roles`. Both servers read the same table, so every
  cell of the matrix is measured rather than reasoned about.
- **Nothing in the suite asserts an absolute count.** The route lists the whole `Teams` table,
  which sibling worktrees write to; every assertion is either a byte comparison of the two
  servers' answers to one request or a membership question about ids the suite created. The
  fixture also asserts up front that no two teams share a display name, because `ORDER BY
  DisplayName` carries no tiebreak on either side and a tie would flake as a byte diff that has
  nothing to do with the handler.
## Ledger additions — api4/user.go (`autocompleteUsers`, appended 2026-08-22, branch `phase-2/par-autocomplete`)

| Go file | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| store/sqlstore/user_store.go (`Search`, `SearchInChannel`, `SearchNotInChannel`, `performSearch`, `sanitizeSearchTerm`) | `mm-store/src/user_store.rs` | PARTIAL | 4 unit + 12 db | `UserSearchOptions` is ported down to the two fields this route varies; the other seven are hard-coded to the values `autocompleteUsers` pins, and the table in the doc comment says which and why. The per-term `AND` clauses are one `NOT EXISTS (… unnest(terms) … WHERE NOT (…))` rather than N appended `WHERE`s — same set, one compile-checked statement. |
| app/user.go (`SearchUsersInTeam`, `AutocompleteUsersInTeam`, `AutocompleteUsersInChannel`) | `mm-app/src/user.rs` | PARTIAL | 12 db (store-level) | All three mint the same `app.user.search.app_error` 500 and differ only in `where`, which is `json:"-"`. Sanitisation stays in the api layer (D-085), unlike Go. The two channel halves run **sequentially** where Go uses an `errgroup`: latency only, and this crate carries no runtime dependency. |
| api4/user.go (`autocompleteUsers`) | `mm-api/src/users.rs` | DONE | 9 unit + 15 parity | `GET /api/v4/users/autocomplete`, all three arms served. The response body ends in a **newline** (`json.NewEncoder`, where the `getUsers` sibling in the same file uses `json.Marshal` and has none — [D-086]). `out_of_channel` and `agents` are `omitempty` and `users` is not, so an empty in-channel list is `[]` and an empty out-of-channel list is a *missing key*. Mutations: 20 run, 20 caught, 2 controls survived. |

## Notes — api4/user.go (`autocompleteUsers`)

1. **`agents` is never on this deployment's wire.** `GetUsersForAgents` goes through
   `agentsBridge` to the `mattermost-plugin-ai` bridge; without the plugin the call errors, the
   handler's `appErr == nil` guard fails, `Agents` stays nil and `omitempty` drops it. Measured on
   every arm, not inferred — `Vec::new()` serialising as `[]` where Go emits nothing has been the
   invisible wrong answer here twice, so `agents_never_appears_on_any_arm` asserts Go's key set as
   well as ours and fails if the bridge ever becomes reachable.
2. **The limit block has three failure modes and only one of them is the default.** `limit, _ :=
   strconv.Atoi(...)` discards the error, and Go's `Atoi` returns a *different value* for a syntax
   error (`0`) than for a range error (the saturated bound). So `?limit=12abc` is `LIMIT 0` — an
   empty list with a 200, not 100 results — while `?limit=99999999999999999999` is `MaxInt64`
   clamped to 1000. The clamp is **one-sided**: `?limit=-1` reaches Postgres and 500s on both
   servers. All four measured against the running Go server; a Rust `parse::<i64>()` that treats
   every error alike gets the overflow case wrong.
3. **"Never autocomplete on emails" moves a search column, not a response field.**
   `AllowEmails: false` picks `UserSearchTypeNames` over `UserSearchTypeAll`, so `Email` is not a
   `LIKE` target — while the `email` field is still returned for anyone the search *did* match,
   subject to the privacy settings. The parity suite plants a token that exists only in one user's
   address, asserts it finds nobody, and then asserts that same user's `email` is on the wire when
   found by name.
4. **Sanitisation differs in both directions, and each direction is a `null`-vs-empty trap.**
   `ClearNonProfileFields(asAdmin=false)` empties `notify_props`, whose `omitempty` then drops the
   key; it also sets `auth_data` to a pointer to `""`, which `omitempty` **keeps** because the
   pointer is not nil. So a non-admin's response carries `auth_data: ""` and no `notify_props`,
   and an admin's carries the real `notify_props` and no `auth_data`. There is no self exception:
   unlike `getUser`, the caller's own row is sanitised like everyone else's.
5. **The permission gates run before the missing-team-id check.** `?in_channel=X` with no
   `in_team` is a 500 (`api.user.autocomplete_users.missing_team_id.app_error`) — but only for a
   caller who could have read `X`; otherwise the `read_channel` gate answers 403 first and the
   channel's existence is not leaked. Reversing the two is a mutation the suite catches.
6. **Order is `ORDER BY Username ASC` in all three queries and nothing else.** No ranking, no
   `DISTINCT` (that only arrives with `applyViewRestrictionsFilter`, whose callers are forwarded),
   so the result is fully deterministic and the parity suite compares bytes rather than sets.
7. **A pre-existing flake that takes the shared Go server down — [D-157].**
   `db_user_profile_lists::the_in_team_etag_has_no_membership_deletion_condition` fails
   intermittently on a clean tree (two consecutive `get_millis()` fallbacks colliding in the same
   millisecond). The panic skips that file's trailing purge, and the six rows it leaves have
   twelve NULL columns Go's `model.User` cannot scan — so **Go's own `GET /api/v4/users` then
   500s for every worktree**, which failed four `parity_users_list` tests here until the rows were
   cleared by hand. Cost this session two mutation re-takes and one full-suite re-run.
## Ledger additions — api4/channel_category.go, the read side (appended 2026-08-22, branch `phase-2/par-sidebar`)

| Go file | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| model/channel_sidebar.go | `mm-model/src/sidebar_category.rs` | DONE | 10 pass | `SidebarCategory`, `SidebarCategoryWithChannels` (Go embeds, so `#[serde(flatten)]` — nine inlined keys then `channel_ids`), `OrderedSidebarCategories`, `SidebarChannel` (`SortOrder` is `json:"-"`). **`IsValidCategoryId`'s regexp is unanchored** and its two halves are `[a-z0-9]` where `IsValidId` accepts upper case, so the two branches disagree about case and `zzfavorites_<26>_<26>!!` is valid; corpus in `fixtures/behaviour_sidebar_category.json`. The three arrays carry no `omitempty`, so nil is `null` and empty is `[]` — modelled `Option<Vec<_>>`. |
| store/sqlstore/channel_store_categories.go (the three reads; the five writes landed 2026-09-10, see below) | `mm-store/src/sidebar_category_store.rs` | PARTIAL | 14 pass + 40 parity | **The answer is not what is in `SidebarChannels`.** Every read appends *orphans* — channels the user is a member of that appear in no category of theirs — to the Channels or DMs category, so on a normal server most of a user's sidebar is rows the join never returns. Orphans come last, in `DisplayName` order, after the explicit channels' `SortOrder` order. Ported in its own module rather than into `channel_store.rs`; Go hangs them off `ChannelStore`. |
| app/channel_category.go (the three reads; the four writes landed 2026-09-10, see below) + app/authorization.go (`SessionHasPermissionToCategory`) | `mm-app/src/sidebar.rs` | PARTIAL | 16 pass | One error id (`app.channel.sidebar_categories.app_error`) for every branch of all four; only the status code moves. `SessionHasPermissionToCategory` is **not** `SessionHasPermissionToUser` — no unrestricted branch, no `manage_system`, no self shortcut, and it compares `category.UserId` against *both* the session and the path's `user_id`. The create-on-empty branch **is** ported now (2026-09-10) and nothing is forwarded. |
| api4/channel_category.go (`getCategoriesForTeamForUser`, `getCategoryOrderForTeamForUser`, `getCategoryForTeamForUser`; the five writes landed 2026-09-10, see below) | `mm-api/src/sidebar.rs` | DONE | 7 pass + 40 parity | All eight methods on the three paths served. **The path parameter is `{category}`, not `{category_id}`** — Go's mux class is `[A-Za-z0-9_-]+` and a default category id is `{type}_{userId}_{teamId}`, so the `_id` suffix would have enrolled it in the shared `[A-Za-z0-9]+` middleware and forwarded the common case. Two framings in one Go file: `/order` carries a trailing newline, the other two do not. Mutations: 21 run, 21 caught, 2 controls survived. |

## Notes — api4/channel_category.go (the read side)

1. **`getCategoryForTeamForUser`'s first gate is not the one the other two use.** The two list
   handlers gate on `SessionHasPermissionToUser`; the singular one gates on
   `SessionHasPermissionToCategory`. Both refusals name `model.PermissionEditOtherUsers`, which is
   what makes them so easy to conflate — and the difference is a real hole: `SessionHasPermissionToUser`
   grants outright when the path names the caller (its self shortcut), so a port that reached for it
   would hand any user any *other* user's category by naming itself in the path.
   `parity_sidebar_categories::a_category_belonging_to_someone_else_is_refused_even_when_naming_yourself`
   is that request, and Go 403s it.
2. **A missing category is a 403, not a 404, for anyone without `edit_other_users`.** The gate
   fetches the category itself and denies on the miss, so `GetSidebarCategory`'s own 404 is
   unreachable through the route unless the caller short-circuits the gate. Both answers are
   asserted, side by side, so neither reads as a fixture accident.
3. **Most of the body is not in the table it comes from.** `getOrphanedSidebarChannels` adds every
   channel the user is a member of that has no `SidebarChannels` row *for that user on that team* —
   public and private ones on the current team, DMs and GMs regardless of team. Joining a channel
   writes a membership row and nothing else, so a port that returned the join alone answers with a
   nearly empty sidebar and looks entirely plausible. Its `sq.Or{}` guard is load-bearing: with both
   selectors false squirrel renders an empty disjunct, which matches everything, so Go returns
   early — and so does this, with a unit test on a deliberately unreachable pool.
4. **`order` beside `{category_id}`: same answer, different reasons.** gorilla picks the literal
   because `api4/channel.go` registers it first (:80 against :82); axum picks it because a static
   segment beats a parameter outright. No reverse-shadowing case here, unlike `/teams/name/{team_name}`.
   Pinned by a test that reads the *shape* of the answer: an array can only have come from the order
   handler, since the category handler would have 400'd on `IsValidCategoryId("order")`.
5. **Two write framings in one file, forty lines apart.** `getCategoriesForTeamForUser` and
   `getCategoryForTeamForUser` end in `json.Marshal` + `w.Write`; `getCategoryOrderForTeamForUser`
   ends in `json.NewEncoder(w).Encode`. So one of the three carries a trailing newline ([D-086]) and
   the suite asserts the byte on all three.
6. **The empty-categories branch is a write, and is forwarded.** Go creates the three default
   categories inside the GET, migrating the user's favourites into `SidebarChannels` as it goes.
   Two servers racing to insert the same deterministic ids is not a thing to reproduce, so the
   handler forwards and Go performs its own migration. Reachable only where the rows are missing;
   joining a team creates them. `GetSidebarCategoryOrder` has **no** such fallback, so the same user
   in that state gets three categories from `/categories` and `[]` from `/categories/order`.
7. **sqlx infers nullability from the column, not from the join.** `SidebarChannels.ChannelId` is
   `NOT NULL` in the schema and NULL in every `LEFT JOIN` row for a category with no explicit
   channels; without an explicit `AS "channelid?"` every empty category failed the whole query with
   `UnexpectedNullError`. It cost a debugging cycle because `StoreError::Db`'s `Display` is its
   context string alone — so `mm-app/src/sidebar.rs` logs `?err` rather than the crate's usual
   `%err`, and says why.
8. **Two pre-existing tests asserted `…/channels/categories` was forwarded**, in
   `parity_channels_for_team_for_user.rs` and `parity_channel_members_for_team_for_user.rs`. Both
   were correct when written and both failed here, which is the assertion doing its job; both are
   inverted rather than deleted, and now guard against the routes silently *stopping* being served.
   The forwarding claims for this family live in `parity_sidebar_router.rs`.
9. **This suite's fixtures are prefixed `mmrssidebar`, not `mmrs-parity-`.** `common::purge_api_fixtures`
   runs once per test *binary* and binaries run concurrently, so sharing the prefix lets another
   suite's start-up delete this one's team mid-run. The local purge selects by **team**, not by name,
   which also closes [D-155] for these rows: Go authors `town-square`, `off-topic` and three
   `SidebarCategories` on the fixture's behalf and none of them carries a prefix.
10. **The subject of every byte comparison is a freshly created user, not the fixture admin.** The
    admin accumulates DMs from every other suite, whose empty display names tie under the orphan
    query's `ORDER BY DisplayName` and make the comparison order-random. One DM is created
    deliberately, so the `D`/`G`-to-DMs dispatch is observable without a tie.

11. **The three comparisons in `SessionHasPermissionToCategory` needed three separate requests,
    and the suite had none of them.** `category.UserId == session.UserId`,
    `category.UserId == userID` and `category.TeamId == teamID` all look like the same check, and
    a fixture where the caller asks about its own category on its own team satisfies all three at
    once — so removing any one of them survived. Each now has a request that isolates it: your own
    category with someone else in the path (only the path comparison can refuse); their category
    with them in the path, on a team you can see (only the session comparison can refuse); and your
    own category named under a *different team you are a member of* (only the team comparison can,
    since the `view_team` gate behind it would grant — which is why a third fixture team exists).
    Three mutations survived until these landed; all three are caught now.
12. **A no-op control failing was the real finding, again.** Mid-session `parity_users_list` began
    failing against the **Go** server — [D-157], a sibling suite's assertion panicking past its own
    purge and leaving `Users` rows whose non-pointer columns are NULL, which 500s `GET /users` for
    every worktree. `scripts/mutate.sh`'s `api` suite ran *every* parity binary, so that one broken
    suite turned every verdict into a false CAUGHT, control included. The harness now takes
    `MUTATE_API_TARGETS` (`--test parity_sidebar_categories --test parity_sidebar_router`) so a
    verdict can only come from the suite under test, and `${=…}` so a multi-word value splits at
    all under zsh. Every tally above was re-taken through it.

## D-157 closed — the etag flake that took the shared Go server down, 2026-08-22

`crates/mm-store/tests/db_user_profile_lists.rs` asserted that two consecutive
`get_etag_for_profiles` calls on a memberless team differ. They tie whenever both land in the
same millisecond — about one run in three — and the panic skipped the trailing `purge`, leaving
six rows whose twelve NULL columns Go's `model.User` cannot scan. From that moment Go's
`GET /api/v4/users` 500s for **every worktree sharing this database**.

Both halves are fixed, and both were measured rather than reasoned about:

1. The assertion now pins the *property* — that the fallback suffix is the clock, not any stored
   `UpdateAt` — so it needs no sleep and cannot tie. Twelve consecutive runs pass where the old
   one failed roughly one in three.
2. That file's `insert_user` now spells out every column backing a non-pointer Go field, so a
   panicked-past purge leaves rows Go can still read. Verified end-to-end against the running Go
   server: a row inserted the old eight-column way makes `GET /api/v4/users` return **500**, the
   same row inserted the new way returns **200**.

The finding lives in the doc comment on `insert_user` and beside the assertion; the backlog entry
is gone rather than marked closed, per *docs/TECH_DEBT.md is a backlog, not a diary*.

## `GET /api/v4/channels/{channel_id}/posts` — `getPostsForChannel` (2026-08-23)

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/posts.rs` — `get_posts_for_channel` | PARTIAL, the page branch served |
| app | `crates/mm-app/src/post.rs` — `get_posts_page`, `get_posts_etag`, `prepare_post_list_for_client`, `sanitize_post_list_metadata_for_user`, `get_next_post_id_from_post_list`, `get_prev_post_id_from_post_list` | PARTIAL |
| store | `crates/mm-store/src/post_store.rs` — `get_posts` (both branches), `get_etag`, `get_post_id_around_time`, `get_visible_post_id_around_time`, `get_priority_for_posts`, `get_acknowledgements_for_posts` | PARTIAL |
| app | `crates/mm-app/src/config.rs` — `enable_burn_on_read`, `feature_flag_burn_on_read`, `Config::burn_on_read` | DONE |
| model | `mm-model::post_list::PostList` (already ported) | — |

Served: the plain page, with and without `collapsedThreads`, `skipFetchThreads` and
`include_deleted`, plus the etag and both pagination cursors. Forwarded, by the handler rather
than by the router because these are query parameters: `since > 0`, `after`, `before`,
`collapsedThreadsExtended`, an unparseable `since`, and any page holding a post the metadata
pipeline already refuses.

Tests: 15 cross-server (`crates/mm-api/tests/parity_channel_posts.rs`), 4 DB
(`crates/mm-store/tests/db_post_channel_page.rs`), 3 unit (two in `post_store::tests` for the
NULL-versus-JSON-null split, one in `config::tests` for the burn-on-read conjunction).
Mutations: **21 run, 18 caught, 1 explained survivor, 2 controls survived**
(`scripts/mutations/getposts-for-channel.tsv`).

What a reader would otherwise get wrong, each pinned in the doc comment on the thing it
constrains:

1. **A NULL `props` column is `{}` on the wire, not `null`** — and this was a *live bug in the
   already-shipped `getPost`*, found by predicting the opposite and measuring. sqlx allocates a
   nil map before scanning into it, so `StringInterface.Scan`'s early return on NULL lands on an
   empty map; slices get no such treatment, which is why NULL `fileids` really is `null`. A jsonb
   `'null'` is a third answer again (`null`, because `json.Unmarshal` resets the map). [D-158]
   carries the audit owed for every other ported store that scans a Go map.
2. **`GetEtag` takes `collapsedThreads` and drops it.** `q.Where(sq.Eq{"RootId": ""})` at
   post_store.go:954 discards the builder squirrel returns by value, so the filter never reaches
   the SQL and both modes share one etag. Applying it — the obvious reading — makes our 304s
   disagree with Go's.
3. **`MakeNonNil` runs on the plain branch and not the collapsed one, and it does not matter.**
   The asymmetry is real and the wire difference it predicts does not exist, for the reason in
   (1): the map was never nil. Removing the call is therefore a mutation that survives, and it is
   reported as an explained survivor rather than as a test gap.
4. **`getRootPosts` does not select roots.** There is no `RootId = ''` predicate: the window is
   every post in the channel, replies included. The collapsed-threads query is the one that filters
   to roots. `getParentsPosts` then fetches the *threads* those rows belong to and adds them to
   `posts` **without** adding them to `order`, so a page's map is routinely larger than its order.
5. **`skipFetchThreads` decides whether `reply_count` is computed at all**, not what is returned.
   With it off Go selects no `ReplyCount` column, so every post on the page reports `0`; with it on
   a correlated subquery counts the thread. Both measured against Go on the same fixture.
6. **The burn-on-read cursor is the live one.** `ServiceSettings.EnableBurnOnRead` and
   `FeatureFlags.BurnOnRead` both default to `true`, so `getCursorPostId` reaches
   `GetVisiblePostIdAroundTime` — the query carrying the `ReadReceipts` subquery — and the plain
   lookup beside it is the fallback. Neither is reachable from the parity suite (the receipt-aware
   half needs a burn-on-read post, which the metadata pipeline refuses and forwards), so both are
   covered at the store level instead.
7. **Priority and acknowledgements are fetched for `order`, not for `posts`.**
   `PreparePostListForClient` calls the per-post pipeline with *empty* opts — `IncludePriority`
   false — and then batches both reads over the order alone. A root pulled in only as a parent
   therefore carries no `metadata.priority` even when its row exists. Both rows are planted
   directly, because the write paths are licence-gated on Team Edition and the read paths are not.
8. **The two 403s this route can raise are byte-identical.** `include_deleted` without
   `manage_system` reports `read_deleted_posts` and an unreadable channel reports
   `read_channel_content`, but `MakePermissionError` puts the id in `DetailedError` and that field
   is stripped for a non-admin caller. So the gate *order* — which Go fixes deliberately, to keep a
   missing channel from leaking through a permission error — is not observable through the route,
   and no test can pin it. Said here rather than asserted falsely.
9. **Go caches the etag for thirty minutes** ([D-159]). For a channel with posts the cached value
   and our fresh read agree; for an *empty* channel Go repeats the clock reading its first request
   took while we take a new one, so Go can 304 where we answer 200. It becomes a live hazard the
   moment `mm-api` writes a post, because a write that does not go through Go's store never
   invalidates it.

Three of those verdicts were findings rather than confirmations:

- **`make-non-nil` survived, and stays in the code.** Removing `list.make_non_nil()` changes no
  byte, for the reason in (1) — the map was never nil. Reported as an explained survivor rather
  than deleted: it is Go's line, and it becomes load-bearing again the day the props handling
  changes.
- **Two mutations of the `before` half of the cursor queries survived at first.** `before` and
  `after` are separate statements, and `db_post_channel_page` only ever stepped forwards, so half
  of both queries had no oracle — including the half `prev_post_id` actually uses. Four
  assertions later, both are caught.
- **`scripts/mutate.sh` grew `MUTATE_STORE_TARGETS`**, mirroring the api one. `--tests` builds all
  twenty mm-store binaries per mutation, which is minutes of rebuild and lets a suite that never
  saw the change decide the verdict — the failure the api narrowing already existed to prevent.
  Two harness faults on the way there were the plan's fault, not the code's: a mutation that
  drops a bind parameter does not compile, and zsh's `read` collapses an empty tab-delimited
  field, which splices the *suite* name into the source.

Also worth knowing:

- **`?since=0` is not a `since` request.** Go's test is `since > 0`, so an explicit zero falls
  through to the page branch, etag and all — while `?since=1` selects a different store query
  entirely. `?after=` and `?before=` behave the same way: empty is absent.
- **`getPostsCollapsedThreads` ignores two of its own options.** `include_deleted` and
  `skipFetchThreads` are in the struct and in neither the query nor the response; the 403 gate in
  front of `include_deleted` still fires, which is the only trace the parameter leaves.
- **Participants are stub users.** Without `collapsedThreadsExtended` each is a zero-valued
  `model.User` carrying only an id, so it serialises with every non-`omitempty` key at its zero
  value. Reproducing that means constructing the same zero value, not something tidier.
- **An empty channel is not reachable through the API.** Creating a channel makes Go write a
  `system_join_channel` post for its creator, so the store test and the parity fixture both delete
  the row directly — otherwise the clock-stamped etag branch has no fixture at all.

---

## Experiment, 2026-08-24 — the rest of `model/` ported without running a single tool

**Two phases.** Phase one was the experiment: port every remaining model file using **only file
reads and writes** — no `cargo check`, no `cargo test`, no `cargo clippy`, no fixture generation.
Phase two, in the same session, put the result through the normal discipline: build, fixtures,
behaviour oracles, mutation testing. The **Verdict** below is the part worth reading; the
experiment's own predictions are kept unedited underneath it so the two can be compared.

### What landed

112 new modules in `crates/mm-model/src/` — 110 ports plus two internal helpers (`go_bytes.rs`,
`serde_helpers.rs`) — bringing the crate to 191 declared modules and ~87,500 lines. They cover
every remaining file in `server/public/model/` except the exclusions below. Four
files were **generated from the Go source** rather than transcribed, because a registry of that
size is where a hand-copy silently loses an entry: `migration.rs` (61 keys), `feature_flags.rs`
(41 flags), `audit_events.rs` (357 events), `config.rs` (53 structs, ~1,300 fields).

### What is deliberately not ported

| Go file | Why |
|---|---|
| `client4.go`, `client4_route.go`, `websocket_client.go` | Go REST/WS **client**, out of scope (CLAUDE.md) |
| `*_serial_gen.go` (session, user, team_member, utils) | msgp/easyjson codecs — serde replaces them |
| `ai_bridge_test_helper.go`, `map.go` | test helpers |
| `config.go`'s `SetDefaults`/`IsValid`/`Sanitize` | thousands of lines of per-field logic; `MIGRATION_STRATEGY.md` says translate config lazily, section by section |
| `manifest.go: FindManifest`, `packet_metadata.go: ParsePacketMetadata` | need a YAML parser |
| `saml.go`'s metadata tree, `shared_channel.go`'s `SyncMsg` XML codec, `xml_helpers.go`'s decoder | need an XML codec |
| `remote_cluster.go: Encrypt/Decrypt` | need AES-GCM + scrypt ([D-046]'s neighbours) |
| `auditconv.go: AuditModelTypeConv` | a `type switch` over `any`; Rust dispatches statically |

### The findings worth keeping

These are in the code, in the doc comment on the thing they constrain. The ones most likely to
bite:

- **`Bitmask.IsBitSet` ignores its argument** (`remote_cluster.rs`) — `return *bm != 0`. Every
  `IsOptionFlagSet` caller reads "has any option" as "has this option". Reproduced.
- **`auditCommandArgs` logs `team_id` and `trigger_id` swapped** (`auditconv.rs`). Every
  slash-command audit entry the Go server has ever written has them the wrong way round.
- **`entry` outranks `enterprise`** (`license.rs`) — `EntryTier == EnterpriseAdvancedTier == 30`,
  so `MinimumEnterpriseLicense` is true for the cheapest SKU.
- **`AcceptedNetworkRequestGroups` can never match** (`metrics.rs`) — `processLabel` lower-cases
  the value first and every accepted value is capitalised, so the label always falls back.
- **`GetSiteURL`'s second branch is dead** (`remote_cluster.rs`): it re-tests a value the first
  branch already rewrote.
- **Config wire keys are Go field names**, and only three fields in 5,795 lines carry
  `json:",omitempty"` (`config.rs`). Everything else writes `null`, which is what lets the server
  tell "unset" from "set to zero".
- **`PropertyValue.Value` is a `json.RawMessage`** and `serde_json::Value` is not (`property_value.rs`):
  Go's `SanitizePropertyValue` returns the *original bytes* so callers can skip a write by
  identity, and that identity comparison does not survive the port.
- **`WebSocketEvent` has two encoders that disagree byte-for-byte** (`websocket_message.rs`) — the
  precomputed path emits a space after each colon.
- Several Go methods **panic** on inputs this port instead rejects or treats as absent
  (`ChannelModerationPatch.Roles`, `Features.ToMap`, `MessageExport.PreviewID`,
  `ChannelBookmarkAndFileInfo.MiniPreview`, `manifest.MeetMinServerVersion`). Each is noted at the
  call site; all are the safe direction.

### Verdict — what the tools found afterwards

The draft was closer to correct than the experiment's own risk section predicted, and the errors
it did contain were concentrated exactly where a reader would guess: hand-reimplemented Go
standard-library behaviour, not the wire format.

| Check | Result |
|---|---|
| `cargo check --workspace` | **2 errors** in ~21,000 new lines, both the same cause: `WebSocketResponse` derived `Clone`/`PartialEq` while holding a boxed `AppError` that is neither |
| `cargo clippy --all-targets -- -D warnings` | 6 findings — four `collapsible_match`, one `derivable_impls`, one `ptr_arg` on a speculative helper that was deleted |
| Fixture generation | 246 registry entries added; **2 rejected by the generator** (`GroupSyncable`, `PluginPropertyOption`) because their wire form is not their struct — see below |
| Wire round-trip, 245 types | **3 failures**, all real: `AutocompleteData`'s nil-able slices, a `float64` serialised as `65.0` where Go writes `65`, and one `any` field the reflective filler could not type |
| Behaviour oracles | **3 genuine translation bugs** in reimplemented Go stdlib: `path.Clean`, Masterminds' leading-zero rule, and `time.Time.String` on a 5-digit year |
| Model behaviour oracle, 19 corpora / ~250 cases | **0 failures** — every validator, builder and custom codec answered as Go did on the first run |
| `scripts/mutate.sh` | **15 run, 15 caught, 2 controls survived.** Each mutation died in the test that should have killed it, not in an unrelated suite |

Two lessons, both worth more than the line count:

1. **The wire format survived; the hand-written algorithms did not.** Every serialization bug was
   mechanical and caught by a fixture. Every *logic* bug was in code reimplementing a Go library
   (`path.Clean`, `semver`, `time`), where there is no fixture to compare against unless one is
   deliberately built. That is an argument for writing the behaviour corpus **first** for anything
   that reimplements a dependency.
2. **A type whose `MarshalJSON` does not describe its struct cannot have a reflective fixture.**
   `GroupSyncable` renames a `json:"-"` field per its type and errors on a third;
   `PluginPropertyOption` emits its inner map unwrapped; `IntegrityCheckResult` and
   `AutocompleteArg` hold an `any` the filler fills with a string that the type's own reader
   rejects. The first two are covered by `behaviour_sweep_models.json` instead; the last two are
   pinned by an `overrides` entry giving the `any` its real shape. **Changing those overrides
   rewrites committed fixtures** — `autocompletearg.type`/`.data` (three paths) and
   `integritycheckresult.data` are new and hand-populated, the only hand-written values in
   `reference/dump/main.go`.

Two parser rewrites came out of it, both in `manifest.rs`:

- `StrictVersion` now follows Masterminds' actual step order (metadata, then prerelease, then the
  numeric segments) and returns a typed `VersionParseError`, because `Manifest.IsValid` folds the
  reason into the message a plugin developer sees. `NewVersion` is the **loose** parser in
  v3.5.0 (`CoerceNewVersion = true`), which accepts leading zeros; `StrictNewVersion` does not.
  That asymmetry is what `MeetMinServerVersion` depends on.
- `ManifestError` gained `InvalidSettingsSchema`, reproducing Go's `errors.Wrap(err, "invalid
  settings schema")` prefix.

### Parity risk — the experiment's own assessment, left unedited

- **Nothing here compiles-checked, ran, or was tested.** No `cargo` invocation of any kind.
- **No fixtures were generated and no test was written.** Every wire claim in these 95 modules is
  an unverified reading of the Go source — exactly the failure mode `fixtures/` exists to prevent.
- Mechanical checks *were* run in place of the compiler, and they pass: every `use crate::…` and
  inline `crate::…` path in the new files resolves to an item that exists; `lib.rs` declares all
  191 modules with no duplicates and no orphans; brace/paren balance is clean. That is evidence of
  **absence of one class of typo**, not of correctness.
- Known compile risks that a mechanical check cannot see: trait bounds on derives (one was caught
  by hand — `Channel` is `PartialEq` but not `Eq`, so `ChannelWithBookmarks` cannot derive `Eq`),
  `#[serde(flatten)]` interactions, and closure borrow lifetimes.
- **Anything from this section needs `cargo check`, then fixtures, then tests, before it is
  believed.** The right next step is not another model file.

### Next

Route work resumes. Nothing in these 112 modules unblocks a route on its own, and per CLAUDE.md a
model file is only worth its test suite once a route needs it — the tests written here exist
because the draft needed *verifying*, not because a route arrived.

What is still untested, and should be treated as unverified until a route needs it: the ~35
modules with no `json:`-tagged type and no branching logic (constant tables, marker structs), and
every method listed in "deliberately not ported" above.

---

## `GET /api/v4/posts/{post_id}/thread` — `getPostThread` (2026-09-02)

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/posts.rs` — `get_post_thread` | PARTIAL, everything but a negative `perPage` and `collapsedThreadsExtended` |
| app | `crates/mm-app/src/post.rs` — `get_post_thread` | PARTIAL |
| store | `crates/mm-store/src/post_store.rs` — `get_thread`, `get_thread_root`, `get_thread_replies`, `get_collapsed_thread_root`, `get_collapsed_thread_replies`, `shave_extra_row`, `GetPostThreadOptions`, `ThreadDirection` | PARTIAL |

Served: both store branches, all nine validation 400s, both cursors in both directions with and
without the `fromPost` tie-break, `perPage` with `has_next`, the etag and its 304. Forwarded:
`collapsedThreadsExtended=true` (needs `SanitizeProfile`), a **negative** `perPage`, and any
thread carrying a burn-on-read post.

Tests: 16 cross-server (`crates/mm-api/tests/parity/post_thread.rs`).
Mutations: **29 run, 27 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/post-thread.plan`). Five of the 27 survived the first pass and each was a
gap in the fixture — see below.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **The two sibling handlers parse the same flags differently.** `getPostThread` compares
   `r.URL.Query().Get(f) == "true"`; `getPostsForChannel`, forty lines earlier in the same file,
   calls `strconv.ParseBool`. So `?skipFetchThreads=1` is **true** for the channel page and
   **false** for the thread, and likewise `t`, `T`, `TRUE`. Measured on all five spellings —
   reusing `query_flag_is_true` here is the shortcut that would have looked right.
2. **`has_next` has three states from one handler.** The collapsed branch always assigns it, so
   `collapsedThreads=true` emits `"has_next":false` even unpaginated; the non-collapsed branch
   assigns it *inside* the `!skipFetchThreads` block, so `skipFetchThreads=true` **omits the key**
   (`*bool` + `omitempty`); everything else emits a real boolean.
3. **Asking for a reply's thread means two different things.** The non-collapsed branch resolves
   `RootId` and returns the whole thread; the collapsed branch uses the requested id *literally*
   as the root, so it returns the reply **alone**. Measured: 5 posts against 1.
4. **`fromUpdateAt` is direction-gated on one branch only.** The collapsed query applies it only
   when `direction == "down"` (post_store.go:697); the non-collapsed one applies it in both
   directions (:845). An `up` request with a cursor is therefore unfiltered on one branch and
   filtered on the other.
5. **A negative `perPage` panics the Go server, and that is now measured rather than reasoned.**
   `Limit(uint64(perPage + 1))` turns `-1` into `LIMIT 0`; zero rows then match `perPage+1 == 0`,
   `hasNext` is set, and `posts[:len(posts)-1]` panics — the connection closes with no response
   and the log says `slice bounds out of range [:-1]` at post_store.go:895. `-2` and below render
   a `uint64` too large for a Postgres `LIMIT` and come back 500 (`pq: bigint out of range`).
   Forwarded, so a client keeps getting Go's answer, panic included.
6. **The permission check runs after the query**, the reverse of `getPost`. A missing post is a
   404 for a caller with no rights to it, where `getPost` would have answered 403 — Go reads the
   thread first (api4/post.go:893) and calls `GetPostIfAuthorized` second (:918).
7. **Every reply in a non-collapsed thread reports the *root's* `reply_count`.** It comes from a
   `WITH replycount` CTE cross-joined into the select, not a correlated subquery, so one number is
   stamped on every row. On the collapsed branch the replies report `0`, because `postsQuery` has
   no such column at all.
8. **The nine validation 400s share one id and are still distinguishable — through `message`.**
   `WipeDetailed` blanks `detailed_error` and the params map is `json:"-"`, which reads like the
   nine are one response; but `Translate` interpolates `params["Name"]`, so Go answers `Invalid or
   missing perPage in request body.` and, for the `fromPost` branch whose "parameter name" is a
   whole sentence, `Invalid or missing if fromPost is set, then fromCreateAt must also be set in
   request body.` Ours is the untranslated id ([D-092]); the suite asserts Go's nine messages so a
   branch wired to the wrong name is caught now rather than when i18n lands.

### The five survivors, and what each one was

`mutate.sh` replaces the **first** occurrence of a pattern, and for all three thread queries that
is the `direction == ""` statement — the one this port keeps separate because Go emits no
`ORDER BY` for it. No test paired an absent direction with `perPage` or a cursor, so three
mutations landed on a live code path with no oracle. The same shape as the `before`-half survivors
in the `getPostsForChannel` session; worth expecting the next time a query is split in two.

| Survivor | What the suite could not see | Fixture added |
|---|---|---|
| `thread-limit-off-by-one` | `LIMIT perPage` instead of `perPage + 1`, unordered statement only | `?perPage=2` with no `direction` |
| `thread-updateat-cursor-direction` | `>`/`<` swapped, unordered statement only | `?fromUpdateAt=…` with no `direction` |
| `ct-updateat-direction-gate` | the collapsed branch's `direction == "down"` gate inverted, unordered statement only | `?collapsedThreads=true&fromUpdateAt=…` |
| `thread-has-next-ge` | `len == perPage + 1` weakened to `len >= perPage` | `?perPage=4` — a page **exactly** the window's size, the only value that separates the two |
| `app-notfound-status-swapped` | the app layer's 500 branch, which nothing reached | a post whose `props` column is a jsonb **array**, planted directly; both servers 500 |

All six re-ran caught after the fixtures landed. One mutation in the first pass was a **harness
fault of the plan's making** — it dropped a bind parameter, so sqlx's arity check refused to
compile it; rewritten to keep `$2` bound, it is caught.

### One repair outside this route

The full parity binary was failing two `team_channel_lists` tests on every run, and it was not
this route. Commit 6c156a2 merged 35 test binaries into one process without replacing the
isolation those processes provided: `team_channel_lists` and `channels_for_user` both created a
team named `mmrs-parity-pageteam`, and `team_channel_lists` and `channels_for_team_for_user` both
created `mmrs-parity-delteam`. Concurrent creates, one winner. Renamed the two non-owning tags;
those were the only duplicates in the binary. Residual intermittent cross-suite interference
remains and is [D-160].

Two things about the port's shape, since they are decisions rather than findings:

- **`direction == ""` gets its own SQL statement.** Go emits **no** `ORDER BY` for it, and a
  degenerate sort key is not the same thing — it lets Postgres reorder rows Go returns in scan
  order. The cursor predicates and the `LIMIT` *are* parameterised into one literal (identical
  truth tables, still visible to a mutation); only the `ORDER BY` could not be.
- **`GetPostThreadOptions` is a second struct** rather than more fields on `GetPostsOptions`. Go
  has one type serving both queries; they overlap in three fields and disagree about two of them,
  and keeping them apart is what lets each one's documentation say what its own query does.

## `AppError` boxed across `mm-app` and `mm-api` (2026-09-03) — toolchain, not a route

No route work. `rustc`/`clippy` 1.98 turns on `result_large_err` for `AppError`, which is 192
bytes against the lint's 128-byte threshold: 84 functions across `mm-app` and `mm-api` returned it
unboxed, so every success path was as wide as the failure path. `mm-model` had already made this
decision and documented it — `AppResult<T> = Result<T, Box<AppError>>` — and the two crates simply
never adopted it. They do now: app-layer signatures are `AppResult<T>`, `ApiError` holds a
`Box<AppError>`, and `AppError::boxed` (utils.rs) is `new` in a box so a call site stays one
expression. `clippy` 1.97 separately started flagging `for_kv_map` at `post.rs:729`. No wire
format moved; the 296-test parity suite and 2,154 unit tests are the guard. The rationale lives on
`AppError::boxed` and on `ApiError`. Environment note for a fresh machine: fixtures render in
local time, so the suite needs `TZ=Asia/Kolkata`.

## `GET /posts/{post_id}/reactions`, `GET /emoji/{emoji_id}`, `GET /emoji/name/{emoji_name}` (2026-09-04)

Three routes in one session because they share a stack lock, a rebuild and a mutation plan;
splitting them would have paid for all three twice.

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/reactions.rs` — `get_reactions` | DONE |
| api | `crates/mm-api/src/emoji.rs` — `get_emoji`, `get_emoji_by_name` | DONE |
| app | `crates/mm-app/src/reaction.rs` — `get_reactions_for_post` | DONE |
| app | `crates/mm-app/src/emoji.rs` — `get_emoji`, `get_emoji_by_name`, `emoji_storage_available` | DONE |
| store | `crates/mm-store/src/emoji_store.rs` — `get`, `get_by_name` | DONE |
| config | `crates/mm-app/src/config.rs` — `file_driver_name` | DONE |

`reaction_store.rs::get_for_post` already existed, ported for `metadata.reactions` on `getPost`,
so `getReactions` needed no new store code. Forwarded and unchanged: `POST /reactions`,
`DELETE .../reactions/{emoji_name}`, `/emoji/{emoji_id}/image`, the `/emoji` collection, and
`/emoji/autocomplete`.

Tests: 22 cross-server (8 in `parity/post_reactions.rs`, 14 in `parity/emoji_get.rs`); the parity
binary goes 296 → 318.
Mutations: **17 run, 15 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/reactions-emoji.plan`), after one plan line was repaired — see below.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **`getReactions` answers `null`, not `[]`, and that is the common case.** Go's store leaves
   `[]*model.Reaction` nil when nothing matches and the handler marshals it straight through; a
   nil slice marshals to `null`. Most posts carry no reactions, so the four bytes `null` are what
   this route mostly returns. Every other list route in this port answers `[]`, which is why the
   empty case is spelled out in the handler rather than left to `serde_json`.
2. **`getReactions` has no 404, and an unknown post is a *403*.** The app layer has no not-found
   branch — zero rows is a successful read. But `SessionHasPermissionToReadPost` cannot resolve a
   channel for an id that names nothing and falls back to a bare system-level
   `read_channel_content` check, which an ordinary user fails. An unknown post id is therefore
   403 for a plain user and `null`/200 for a system admin; both halves are asserted.
3. **The permission check runs *before* the query here**, the reverse of `getPostThread` — which
   is precisely why the two routes disagree about what an unknown post looks like.
4. **The two emoji config gates are not the same gate.** The handler checks `EnableCustomEmoji`
   and answers **501**; `App.GetEmoji` checks it again, same error id, and answers **403**. The
   handler runs first, so the 403 is unreachable through these routes. Ported anyway: a non-REST
   caller would see it, and a port that dropped it would answer 200 where Go fails.
5. **`FileSettings.DriverName` is a `String`, not a `bool`, and its default is `"local"`.** The
   app layer refuses with `api.emoji.storage.app_error` (403) when it is the **empty string** —
   not when it names a driver we do not implement. A default of `""` would 403 every emoji read.
6. **The shadowing problem is the reverse of axum's instinct.** gorilla registers the
   `PathPrefix("/emoji")` subrouter *before* `PathPrefix("/emoji/{emoji_id}")`, so
   `/emoji/autocomplete` never reaches `getEmoji`. axum prefers a literal too — but only a
   *registered* one, and this router does not register it. `EMOJI_SHADOWED_LITERALS` forwards the
   one literal that ordering owns. `names` and `search` are deliberately **not** in the list: they
   are POST-only in Go, so a GET falls past them and does reach `getEmoji` with
   `emoji_id = "names"` — a 400 on both servers, pinned rather than papered over.
7. **`{emoji_name}` is not id-shaped**, so the id-charset middleware must not apply. Go's mux class
   `[A-Za-z0-9\_\-\+]+` and `RequireEmojiName`'s `^[a-zA-Z0-9\-\+_]+$` are the same character set,
   so the only thing the validator adds is the length limit. A segment outside the charset is a Go
   mux 404 and is forwarded; a segment inside it but too long reaches the handler on both servers
   and 400s on both. The boundary is **bytes**, and it is `> 64` — not `>=`.
8. **Two sibling routes, two different terminators.** `getEmoji` ends with
   `json.NewEncoder(w).Encode` (**trailing newline**); `getReactions` ends with `json.Marshal` +
   `w.Write` (**none**).

### The harness fault, and why the first tally was wrong

`emoji-byname-column` (`AND name = $1` → `AND id = $1`) was scored **SURVIVED** on the first pass.
It was not a test gap: `mutate.sh` replaces the **first** occurrence of the pattern, and
`get_by_name`'s own doc comment quotes the predicate — `` `WHERE deleteat = 0 AND name = $1` `` —
eleven lines above the SQL. The mutation rewrote prose, changed no behaviour, and could not have
been caught by anything. Re-anchored on the newline and the block's 15-space indent so it can only
match the statement; it is CAUGHT by `an_emoji_by_name_is_byte_identical`.

**The general rule this earns:** a mutation pattern that also appears in a comment is a harness
fault, not a finding. An audit of all seventeen lines found this was the only one; the check is
cheap — for each pattern, confirm its first occurrence in the file is not inside a `//`.

### Three repairs outside these routes

- **The power outage left a mutation applied.** `mutate.sh` restores the source from a `TERM INT
  HUP` trap, and a hard power loss runs no trap. `crates/mm-api/src/emoji.rs` was found holding
  `emoji_name.len() >= EMOJI_NAME_MAX_LENGTH` where Go (`web/context.go:600`) has `>` — mutation
  12 of 17, frozen mid-run. Restored, and the re-run confirms
  `a_name_over_sixty_four_bytes_is_a_400_on_both` catches it. Auditing every plan line's `from`
  against the working tree is how it was found, and is worth doing after any interrupted run.
- **A NULL-column fixture made `db_user_search` match everything.** `db_authorization::insert_user`
  omitted `nickname`, `firstname` and `lastname`, and the schema permits NULL in all three. The
  search's `NOT EXISTS (… WHERE NOT (…))` is then three-valued: a NULL column makes the chain NULL
  rather than false, the row is never excluded, and it matches **every** search term. That suite
  purges at the start and never at the end, so the row outlived it and failed 9 of 12
  `db_user_search` tests — intermittently, since which test ran last decides whether the row is
  left behind at all. The Go server never writes one (148 rows in the development database, zero
  NULLs), so the **fixture** was fixed to write `''`; the query is Go's shape and stays untouched,
  because a `COALESCE` there would make us diverge from Go rather than agree with it.
- **A parity test asserted an ordering neither server promises.**
  `teams_for_user::me_and_the_explicit_id_are_byte_identical` compared `/users/me/teams` against
  `/users/{id}/teams` **byte for byte** — two separate reads of `get_teams_by_user_id`, which
  carries no `ORDER BY` precisely because Go's does not and its callers do not sort. Row order is
  whatever Postgres returns. The assertion held only while the fixture user belonged to few teams;
  once six leaked fixture teams had accumulated (`mmrssidebarmain`, `mmrspostthreadteam` and
  friends — none of them matching the `mmrs-parity-%` purge prefix), it failed on **every** run and
  in isolation, with the same six ids in two different orders. The claim the test owes is that
  `me` resolves to the session's id, which is about *which* teams come back; it now asserts that
  over sorted ids and is renamed `me_and_the_explicit_id_answer_the_same_teams`. Sorting alone was
  not enough: thirteen suites in this binary create teams with that same token, so a team can be
  born *between* the two reads and belong to the second alone — the full-suite run still failed on
  a set difference of one. The pair is therefore re-read until the two agree, the tactic
  `fetch_both_stable` already uses one level down. The Go-against-us half stays byte for byte,
  since that comparison was never exposed to either problem.

### Parity risk

`LocalCacheEmojiStore` is not reproduced — Go can answer `GetByName` from a thirty-minute cache
entry for a row that has since changed, and we always read the row. The difference is staleness,
not wire format, and the suite asserts against a settled database. It bites the *tests* rather
than a client: the SQL purges delete rows the Go server never hears about, which is why every
fixture emoji is named with a timestamp.

## `GET /channels/{id}/pinned`, `GET /posts/{id}/files/info`, `GET /files/{id}/info`, `GET /emoji` (2026-09-04)

Four routes in one session, on the same reasoning as the previous one: they share a stack lock, a
rebuild and a mutation plan.

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/channels.rs` — `get_pinned_posts` | DONE |
| api | `crates/mm-api/src/posts.rs` — `get_file_infos_for_post` | DONE |
| api | `crates/mm-api/src/files.rs` — `get_file_info` (new) | DONE |
| api | `crates/mm-api/src/emoji.rs` — `get_emoji_list` | DONE |
| app | `crates/mm-app/src/file.rs` — `get_file_info`, `mini_preview_would_be_generated`, `has_permission_to_file_action` (new) | DONE |
| app | `crates/mm-app/src/post.rs` — `get_file_infos_for_post_with_migration` | DONE |
| app | `crates/mm-app/src/channel.rs` — `get_pinned_posts` | DONE |
| app | `crates/mm-app/src/emoji.rs` — `get_emoji_list` | DONE |
| store | `crates/mm-store/src/channel_store.rs` — `get_pinned_posts` | DONE |
| store | `crates/mm-store/src/file_info_store.rs` — `get`, and a repair to `get_by_ids` | DONE |
| store | `crates/mm-store/src/emoji_store.rs` — `get_list` | DONE |

Forwarded and unchanged: every other `/files/` route (`""`, `/thumbnail`, `/preview`, `/link`,
`/public`, `POST /files`), `POST /emoji`, `/emoji/autocomplete`, and every non-GET method on the
four migrated paths.

Tests: 37 cross-server (21 in `parity/file_info.rs`, 9 in `parity/channel_pinned.rs`, 7 in
`parity/emoji_list.rs`); the parity binary goes 318 → 355.
Mutations: **28 run, 26 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/pinned-files-emojilist.plan`) — on the *second* run; the first was void and
what voided it is below.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **`GetByIds` drops `archived` and `Get` keeps it — and this port had it wrong.** Both queries
   select `FileInfo.Archived` from `fs.queryFields`. `Get` scans into `model.FileInfo`, so the
   column reaches the wire; `GetByIds` scans into the store-private `fileInfoWithChannelID` and
   converts with a `ToModel()` that assigns twenty of twenty-one fields and **silently omits that
   one** (file_info_store.go:75). `archived` is `json:"archived"` with no `omitempty`, so both
   answers are on the wire and they differ. `get_by_ids` was returning the column, which is a
   divergence on `metadata.files` for `getPost`/`getPostThread`/`getPostsForChannel` as well as on
   the new route. It now selects the column and throws it away, mirroring Go rather than Go's
   intent, and both halves are asserted against the running server.

   The column is `false` for every row the API can create — `Save` does not list it among its
   INSERT columns, and the only writer of `true` is `MakeContentInaccessible`, which mutates a
   loaded struct for the cloud file limit and never reaches Postgres. So the divergence is
   invisible until a fixture writes it, which is what `common::set_fileinfo_column` exists for.

2. **The empty file list is `null`, not `[]`, and every empty list shares one etag.** Two nils,
   both load-bearing. `GetByIds` short-circuits `if len(items) == 0 { return nil, nil }`, and
   neither `orderFileInfosByID` nor the two filters after it materialise a slice, so
   `json.Marshal` renders `null` — the same trap as `getReactions`, reached by a different route.
   And `GetEtagForFileInfos` on an empty list is a bare `model.Etag()`: **`CurrentVersion` and
   nothing else**, no post id and no timestamp. So every post with no attachments on the whole
   server shares one etag and a client that cached one gets a 304 for all the others. The first
   version of that test asserted the opposite — that the empty etag was clock-stamped and could
   never match — and Go answered 304 to it.

3. **`getPinnedPosts` is the only post list in the port that is oldest-first.** `ORDER BY CreateAt
   ASC` against every other query's `DESC`, and `order` is on the wire.

4. **The `/pinned` refusal reports `read_channel_content`; `getChannel`'s reports
   `read_channel`.** Same channel, same user, same underlying denial, two different permission
   names — because `SetPermissionError` is passed a literal rather than whatever the check
   decided. Only the untranslated `message` carries the name, so this is invisible over HTTP
   today ([D-092]).

5. **A public channel cannot test a refusal, and five tests were written before that was
   measured.** `HasPermissionToReadChannel` falls back to `read_public_channel` on the *team* for
   an open channel, and `create_plain_user` puts its user in the team — so the "non-member" is
   served, not refused, and Go answered 200 to every assertion expecting a 403. Both suites now
   create a **private** channel for the refusal half and keep the public one for the fallback
   half, which is the assertion that stops the refusal test passing on a port that refuses
   everybody. `common::create_channel_typed` carries the note.

6. **Two writes on read paths, both forwarded rather than reproduced.**
   `generateMiniPreview` reads the original out of the file backend, encodes a thumbnail, returns
   it *and upserts it into the row*; `MigrateFilenamesToFileInfos` inserts `FileInfo` rows for a
   pre-3.5 post. Neither is reachable without a file backend. Both are refused with
   `PrepareError::Unreproducible` and forwarded — and one qualifying file forwards the whole
   `files/info` array, because there is no way to serve half of a JSON list. The mini-preview
   branch is narrower than it looks: the upload path already generates a preview for every image
   it accepts (app/file.go:1016), so a NULL on an image means a row written before that code, by
   a plugin, or by hand.

7. **`getFileInfo`'s permission block is three branches and the middle one is the surprise.** A
   file you uploaded is readable without any channel permission — `CreatorId == session.UserId`
   short-circuits — but a *bookmark* file is not, because the literal owner `"bookmark"` can never
   equal a real user id and the first branch exists solely to close that hatch. Collapsing the two
   into one `!perm` denies a user their own file after they leave the channel; collapsing them the
   other way hands every user every channel bookmark.

8. **The channel lookup runs before the permission check, so a NULL `ChannelId` is a 404 for its
   own uploader.** The column is nullable because it was added after `FileInfo` existed; the
   upload path has filled it in ever since (app/file.go:774), so only a pre-migration row arrives
   with the `COALESCE`d empty string — and `GetChannel("")` misses two lines before the
   `CreatorId` escape hatch would have applied.

9. **`FeatureFlags.PermissionPolicies` defaults to `true`, so Go's ABAC block runs — and fetches
   the post a second time.** `HasPermissionToFileAction` returns `true` on its first line without
   an enterprise `AccessControl` service, and the extra `GetSinglePost` raises
   `app.post.get.app_error`/404 — *the same id and status* the app layer's own `GetSingle` raises
   a few lines later. `AppError.Where` is `json:"-"`, so nothing distinguishes them on the wire
   and the duplicate read is dropped. The gate itself is ported and called at both of Go's call
   sites, so an evaluator has somewhere to land.

10. **`getEmojiList` has none of the gates its siblings have.** `App.GetEmojiList` goes straight
    to the store: no `EnableCustomEmoji` check and no `FileSettings.DriverName` check, unlike
    `App.GetEmoji`, which would refuse even if the handler did not. The handler's 501 is the only
    thing guarding the query. Its `where` is `getEmoji`, not `getEmojiList` — Go's own copy-paste.

11. **The default emoji page has no `ORDER BY` at all**, and `?per_page=0` is `LIMIT 0` — the
    empty list, not "no limit", which is what a zero means to the channel and post pagination
    helpers. The unsorted page is therefore compared as a **set** across the two servers and only
    `?sort=name` is compared as bytes; asserting byte order on an unordered query is the mistake
    `teams_for_user` made and had to have repaired.

### The first mutation run was void, and the control is what said so

`control-binding-rename-api` — a pure rename of a local binding — came back **CAUGHT**, which
means the harness was noisy and none of the other twenty-seven verdicts meant anything. The
culprit was one test of mine failing on every single mutation, and the cause is a finding about
the route rather than about the harness:

**Forwarding to Go repairs the row.** `an_image_with_no_mini_preview_is_forwarded` asserted that
both file routes hand a previewless image to Go — using *one* fixture file for both. Go serves the
first forward by encoding the thumbnail **and upserting it**, so by the second request the row has
a preview and we serve it ourselves. The fixture now carries two previewless images, one per
route, and the assertion says why.

The same fixture had a second version of the same mistake: clearing `minipreview` **before**
creating the post that carries the file is useless, because `createPost` runs Go's own
`PreparePostForClient`, which fetches the attachments' metadata and regenerates exactly the
preview the fixture was removing. The column is cleared after the post exists.

Three more survivors, all in `getEmojiList`, and all one fixture defect:

**A fixture built in alphabetical order cannot see a sort.** With only this suite's rows in the
table — which is what a filtered mutation run leaves behind — an unordered `SELECT` returns them
in insertion order, so creating `lista`, `listb`, `listc` in that sequence made the *unsorted*
page already sorted and `?sort=name` a no-op. Dropping the `ORDER BY`, swapping it for
`createat`, and never passing the flag at all: three mutations, three survivors, one cause. The
fixture now creates the three in reverse.

**And `page * per_page` is invisible at `per_page = 1`,** because the product equals `page`. The
pagination assertion used a page size of one; it now uses two, and checks that page 1 starts at
offset 2.

### `fetch_both_stable` stopped waiting for quiet and started bracketing instead

Three suites began failing intermittently in the full run as soon as these fixtures landed —
`channels_for_user`, `teams_unread`, `team_members_route`, and `channel_members_list` — and none
of them was a divergence.

The helper read Go, then us, then Go again, and required **Go's two reads to be identical** before
comparing. That works for a row settling after one write and cannot work for a list that is
genuinely growing: Go joins a channel's creator to it, so every `create_channel` anywhere in this
binary lengthens `/users/me/channels` and `/users/me/teams/members` underneath a concurrent
reader. Raising the budget 8 → 12 → 20 attempts did not help, because the list never went quiet
for a whole read triple — the test failed with "never settled" while both servers were perfectly
agreed. (The fixture user is in **55 teams**; that is its own problem, [D-155].)

It now accepts our answer when it equals **either** Go read. That is the stronger check: a correct
port answers something Go also answered at some instant inside the window, so it matches one of
the two; a wrong port matches neither, whatever the list is doing. The quiescence test is kept as
a *third* acceptance, because both brackets compare bytes and a route whose element order is Go's
heap order (`/users/me/teams/members`) fails them on ordering alone every time — its own
order-normalising assertion would never get to run. Match a bracket, or find Go still, or retry.
`teams_unread`'s local copy of the helper got the same treatment.

Five consecutive full-suite runs clean afterwards, against one failure in three before.

### And one existing test was asserting an ordering neither server promises

`channel_members_list::pages_split_cover_and_run_out_identically` claimed that two pages of two
cover the full list **in the same order**. `SqlChannelStore.GetMembers` (channel_store.go:2181)
adds an `ORDER BY` only when `UpdatedAfter > 0`, which this route never sets — so `LIMIT`/`OFFSET`
runs against an unordered scan on both servers, and a row can appear on two pages while another
appears on none. It did: page 0 came back `[a, b]` and page 1 `[c, a]` against a full list of
`[a, b, c, d]`, with both servers agreeing byte for byte on every page. The claim is now about the
**set**, retried; the Go-against-us comparison is untouched, because it was never exposed to the
problem. Same class as the `teams_for_user` repair one session earlier — the third instance now,
so it is worth stating as a rule: **a `LIMIT`/`OFFSET` page of an unordered query is not a
sequence, and any test treating it as one is asserting something the database is free to break.**

### The budget in `fetch_both_stable` moved before it was replaced, 12 → 20

Recorded because it is the step that did not work. [D-160] had already moved this number once for
the same symptom, and moving it again looked like the obvious repair; it bought two clean runs and
then failed again at 20. Widening a wait is the wrong shape of fix for a list that never stops
changing — see the bracketing entry above.

### Tooling — the mutation plan format gained a sixth field

`scripts/mutate-batch.sh` now takes an optional per-line `filter`, overriding `MUTATE_FILTER`.
A plan covering four routes has no single filter that fits: the `api` suites are named after
their routes, libtest takes one filter, and leaving it unset lets an unrelated suite decide every
verdict. Before this, a four-route plan had to be split into four files, each paying for its own
pair of controls.

`common::delete_post` moved out of `parity/post_get.rs`, where it was private, into the shared
harness — `channel_pinned` needs it to plant a deleted pin and a deleted reply, without which the
two `DeleteAt = 0` predicates in the pinned query could be deleted outright and every assertion
would still pass.

## `GET /users/{id}/channels/{id}/posts/unread`, `GET /posts/{id}/edit_history`, `GET /channels/{id}/timezones` (2026-09-04)

Three routes: one large and two small, sharing a stack lock, a rebuild and a mutation plan.

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/posts.rs` — `get_posts_for_channel_around_last_unread`, `get_edit_history_for_post` | DONE |
| api | `crates/mm-api/src/channels.rs` — `get_channel_members_timezones` | DONE |
| app | `crates/mm-app/src/post.rs` — `get_posts_for_channel_around_last_unread`, `get_posts_around_post`, `get_post_id_after_time`, `get_edit_history_for_post` | DONE |
| app | `crates/mm-app/src/channel.rs` — `get_channel_member_last_viewed_at`, `get_channel_members_timezones` | DONE |
| store | `crates/mm-store/src/post_store.rs` — `get_posts_around`, `get_posts_around_parents`, `get_edit_history_for_post` | DONE |
| store | `crates/mm-store/src/channel_store.rs` — `get_member_last_viewed_at`, `get_channel_members_timezones` | DONE |

Forwarded and unchanged: `collapsedThreadsExtended=true` on the unread route, the three writes under
`ChannelForUser` (`/view`, `/notify_props`, `/roles`), `GET /api/v4/system/timezones`, and every
non-GET method on the three migrated paths.

Tests: 32 cross-server (18 in `parity/channel_posts_unread.rs`, 9 in `parity/post_edit_history.rs`,
6 in `parity/channel_timezones.rs`, plus one repair to each of two existing suites); the parity
binary goes 355 → 387.
Mutations: **35 run, 32 caught, 3 controls survived, 0 harness faults**
(`scripts/mutations/unread-edithistory-timezones.plan`) — on the *third* run; the first was void
and the second still had one real survivor. What each run found is below.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **`/posts/unread` is two entirely different responses behind one route.** With something
   unread it is a window around the **first unread post** — that post's thread, then
   `limit_before` older posts, then `limit_after - 1` newer ones — and it carries **no `ETag`**.
   With nothing unread the around-query returns an empty `order`, Go throws the whole thing away
   and re-fetches a plain first page, and *that* branch is the only one that computes an etag. So
   whether this route can answer 304 depends on whether the caller is caught up. The fallback's
   page size is `limit_before`, not `limit_after` and not the 60-per-page default; its `UserId` is
   the **session's** where the around-query above it uses the **path's**.

2. **`order` is rebuilt from scratch, and the thread is deliberately left out of it.**
   `GetPostThread` on the unread post returns the whole thread in `order`; Go replaces that with
   `[]string{}` and puts back only the unread post itself. The thread's other replies stay in
   `posts` and re-enter `order` only if the before/after windows also return them. Keeping the
   thread's order would show replies in the centre channel that the centre channel never showed.

3. **The before-window is guarded and the after-window is not.** Go tests
   `if _, ok := postList.Posts[lastUnreadPostId]; ok` before the first and leaves the second
   unguarded. The guard exists for a cloud filter that is inert here; the asymmetry is ported
   anyway, because a port that treated the two the same would diverge in the one case where Go
   does not.

4. **`reply_count` on the unread window is zeroed and then restored, and only the second step is
   an accident.** `getPostsAround` scans into `postWithExtra`, whose embedded `Post.ReplyCount`
   receives the query's subquery — and then `processPost` runs
   `p.Post.ReplyCount = p.ThreadReplyCount` unconditionally, with `ThreadReplyCount` selected only
   on the collapsed branch. Every non-collapsed window post therefore leaves that function
   reporting zero replies. The parents pass then re-fetches **every** window post (its id list is
   built from each post's own id, not just its root) into a row type `processPost` never touches,
   and `AddPost` overwrites the map entry. So the zeroing is unobservable and a client sees real
   counts. All three steps are reproduced rather than cancelled out on paper: narrow the parents
   query and the zero becomes visible. The parity suite predicted zeroes and Go answered `1`.

5. **`limit_after == 0` is the only pagination value on the route that is a 400.**
   `web.ParamsFromRequest` clamps everything else — negatives and garbage to 60, over-200 to 200 —
   so the handler's check can only fire for a literal `?limit_after=0`. `limit_before=0` is legal
   and asks for no history at all.

6. **The three boolean flags are compared against the literal string `"true"`**, not parsed with
   `strconv.ParseBool`. `?collapsedThreads=1` is **false** on this route where it would be true on
   one using the parser.

7. **Almost every refusal on `/edit_history` is the same 403, including the missing post.** Go
   *discards* `GetSinglePost`'s 404 and raises a permission error instead, so an unknown post is a
   403 here where the same id is a 404 through `getPost`. The channel gate and the authorship
   check produce the same body. The one genuine 404 is a post that exists, is yours, and has never
   been edited — the store's `ErrNotFound` for an empty result.

8. **`/edit_history` is the only post read in the port that skips `PreparePostForClient`.** The
   app layer sets `metadata.files` by hand and nothing else, so an entry's `metadata` is `{}` or
   `{"files":[…]}` — never `emojis`, `reactions`, `embeds`, `priority` or `acknowledgements`. It
   calls `GetByIds` with `includeDeleted = true`, the only call site in the tree that does: a
   history entry is an old version of a post and its attachments may since have been deleted.
   `postsQuery` selects no reply-count subquery either, so every entry reports `reply_count: 0`.

9. **`/timezones` refuses a non-member on a *public* channel, and its neighbours do not.** The
   permission is `read_channel` through `SessionHasPermissionToChannel`, which falls back to the
   **team** — where `team_user` grants `read_public_channel` and not `read_channel`.
   `getPinnedPosts` goes through `HasPermissionToReadChannel`, which has an explicit open-channel
   fallback. So the same user, on the same public channel, is served `/pinned` and refused
   `/timezones`. Measured: the parity test assumed the fallback applied here too and Go answered
   403.

10. **`/timezones` never fetches its channel, so an unknown id is a 403 rather than a 404** — the
    permission check simply finds no membership. Its sibling `/pinned` fetches the channel first
    and 404s on the same id. Both are asserted side by side, because a single status assertion
    says nothing about which of the two shapes the route has.

11. **Three transformations sit between the timezone query and the wire, and the last one is the
    only thing giving the route a stable order.** The SQL has no `ORDER BY` and no filtering; the
    app layer drops members whose `automaticTimezone` **and** `manualTimezone` are both empty
    (an *and* — half a timezone survives), resolves each survivor through `GetPreferredTimezone`
    (which reads the automatic field only when `useAutomaticTimezone` is the exact string
    `"true"`), and then deduplicates through a helper that **sorts**. The empty answer is `null`,
    not `[]`: `var timezones []string` is never allocated.

### The first mutation run was void, and the second found four fixture gaps

Run one: 35 run, 26 caught, 8 survived, **1 harness fault** — which voids it. Run two, after the
repairs below: 35 run, 31 caught, 3 controls survived, 0 faults, with one real survivor left; run
three confirmed the final tally after that survivor's fixture landed.

- **The harness fault was a mutation that could not compile.** Dropping `NOT $3` from the parents
  query left `$3` bound and unused, which `sqlx::query_as!` rejects at build time. Inverting the
  flag instead of removing it is the mutation that was actually wanted.
- **A "disable the gate" mutation that disabled nothing.** `tz-permission-gate`'s replacement
  bound `&session` and changed no behaviour, so it was scored a survivor for a reason that had
  nothing to do with the tests. Both of these are the same class as the reactions/emoji plan's
  comment-matching fault: **a mutation is only evidence once you have checked it is the change you
  meant**.
- **The `edit_post` channel gate could not be separated from the authorship check.** Every actor
  in the fixture failed both, so disabling the first changed no status. It needs an author who has
  **left the channel** — `edit_post` is granted by `channel_user` and not by `team_user`, so such
  a user passes the authorship check and fails the channel one. The suite now carries one, and
  asserts it can still read the same post through `getPost`, so the 403 is about `edit_post`
  rather than about having lost all access.
- **`limit_before`'s clamp is invisible below 200 posts.** The fixture now has a channel with 210.
- **`COALESCE(LastViewedAt, 0)` needed a NULL nothing can write.** `common::null_out_member_column`
  gained the column, and a channel whose member row has been nulled directly takes the
  never-viewed branch — etag and all.
- **The collapsed window's `RootId = ''` had no reply to exclude.** The before- and after-windows
  are separate statements with separate copies of the predicate, and the fixture only had a reply
  in the after half. It now has one in each.
- **The parents pass had nothing to add.** With `skipFetchThreads` off, `RootId IN (…)` pulls in
  every reply of every windowed post — but every reply in the fixture was already *in* the window
  on its own account. The suite now carries a reply written after everything else to a root deep
  in the before-window: outside the window in both directions, so it can only arrive through that
  predicate. Both halves are asserted, since `skipFetchThreads=true` must drop it again.

### Two existing suites asserted that these routes were forwarded

`channel_pinned::the_neighbouring_channel_routes_are_still_forwarded` listed `/timezones`, and
`channels_for_user::deeper_sibling_routes_are_still_forwarded` listed `/posts/unread`. Both were
correct until this session and are now owned by the new suites; each list keeps a replacement
sibling so it still asserts something.

### One thing deliberately left unported

`posts.reverse()` in `get_posts_around` — Go's `prepareThreadedResponse(reversed: !before)` — is
reproduced but is unobservable: `SortByCreateAt` re-sorts the whole list at the end, so the
intermediate order cannot reach a client. It is in the code and named in the mutation plan's
preamble as a line deliberately **not** mutated, so a future reader does not mistake a survivor
there for a test gap.

## `GET /users/stats`, `GET /users/known`, `GET /users/{id}/terms_of_service` (2026-09-04)

Three small user routes. The terms-of-service one needed no app or store work at all — both
landed earlier for `getUser`'s sake — so it is a handler and a route registration.

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/users.rs` — `get_total_users_stats`, `get_known_users`, `get_user_terms_of_service` | DONE |
| app | `crates/mm-app/src/user.rs` — `get_total_users_stats`, `get_known_users`, `get_view_users_restrictions` | DONE |
| store | `crates/mm-store/src/user_store.rs` — `count_total_users`, `get_known_users` | DONE |

Forwarded and unchanged: `/users/stats/filtered`, `POST /users/{id}/terms_of_service`, and any
`/users/stats` request from a caller without `view_members`.

Tests: 12 cross-server (5 in `parity/users_stats.rs`, 2 in `parity/users_known.rs`, 5 in
`parity/user_terms_of_service.rs`), plus one repair to `parity/user_get.rs`; the parity binary
goes 387 → 400.
Mutations: **19 run, 16 caught, 3 controls survived, 0 harness faults**
(`scripts/mutations/stats-known-tos.plan`) — on the second run; the first had one real survivor,
below.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **`/users/{user_id}/terms_of_service` reads its path parameter with the router and with
   nothing else.** The handler's first line is `userId := c.AppContext.Session().UserId`. There
   is no `RequireUserId`, no `me` resolution and no comparison against the segment — so the route
   answers **your own** acceptance record whatever id is in the path, and a short segment that
   every other `{user_id}` route 400s is a plain 200 here. That looks like an authorization hole
   and is the opposite of one: it cannot disclose another user's record because it never looks
   one up, and a port that "fixed" the parameter by honouring it would create the hole it appears
   to have. Pinned from both directions — the accepted user sees its record through the admin's
   id, and the admin sees its own 404 through the accepted user's.

2. **`/users/stats` counts bots.** `IncludeBotAccounts: true` is a literal in Go's options struct,
   so the number is larger than any member list on a server with plugins installed. The suite
   asserts it against a count written straight against the database rather than against the other
   server, because two servers agreeing on a wrong `WHERE` clause is exactly what a cross-server
   comparison cannot see. The other two predicates are `DeleteAt = 0` and
   `RemoteId = '' OR RemoteId IS NULL` — an `OR` over both spellings, because the column is
   nullable *and* the ordinary write path stores the empty string. The development database holds
   169 of the second and 3 of the first, so dropping either half is observable; that was checked
   before the mutations were written, not after they passed.

3. **The restricted caller is forwarded, and the branch is unreachable anyway.** A caller without
   `view_members` sends Go off to build a team-and-channel filter and apply it as two inner joins
   — which, with no `DISTINCT`, counts a user once per matching (team, channel) pair.
   `system_user` grants `view_members` outright (model/role.go:1179), so no account a REST call
   can create reaches it; the only built-in role without it is `system_guest`, and guests are
   licensed. Rather than ship SQL no test can reach, `App::get_view_users_restrictions` returns a
   two-valued verdict and `mm_api::users` forwards the restricted case. The suite reaches it by
   writing a role name nothing defines into one user's `Roles` column — per-user, so it cannot
   disturb a concurrent suite the way editing `system_user` would.

4. **`/users/known` has no permission check and needs none.** Neither the handler nor the app
   layer asks anything. The answer is derived from the caller's own channel memberships, so it can
   only name people the caller already shares a channel with. It is a list of **ids**, so there is
   no sanitisation question, and the store filters neither `DeleteAt` nor channel type — an
   archived channel and a direct message both count.

5. **"These two users share no channel" cannot be arranged inside one team.** Joining a team
   auto-joins its `town-square`, and Go refuses to remove anyone from a default channel
   (`api.channel.remove.default.app_error`) — so any two members of a team already know each
   other, and the first version of this fixture, which created a team of its own precisely to
   avoid that, was wrong: a fresh team has fresh default channels. The suite now spans two teams
   and makes its one shared channel with a **direct message**, which needs no team at all.

### Two fixture facts that were wrong before they were measured

- **A fresh team is not an empty one** — above.
- **A test suite's own teams belong to whoever creates them.** Go joins a team's creator to it and
  to both default channels, so building this fixture as the shared admin put four more channels
  into `/users/me/channels` while `channels_for_user` was byte-comparing that list, and broke it.
  The teams are now created by a plain "founder" user (creating a team is a `system_user`
  permission); the admin still *adds* the members, which does not make the adder one. This is the
  structural fix the previous session's ledger predicted would be needed instead of another
  budget increase in `fetch_both_stable`.

### One survivor, and it was an assertion pointed at the wrong server

`known-trailing-newline` removed our newline and nothing failed. `users_known` compares as a
**set** — the route has no `ORDER BY` — so the bytes are never equated, and the file's one
`ends_with(b"\n")` was checking *Go's* body. An assertion about the other server's output says
nothing about ours. Both sides are now checked.

The general rule, which applies to every set-compared route in this suite: **a
normalised comparison hides everything it normalises away, so each normalised property needs its
own assertion on our side.**

### One pre-existing flake, fixed at the root rather than waited out

`channels_for_user::include_deleted_and_last_delete_at_filter_channels_and_their_teams`
byte-compares `/users/me/channels` — a list every `create_team` and `create_channel` anywhere in
the binary lengthens, because Go joins a channel's creator to it. It failed about one full-suite
run in four and never in isolation. [D-160] had already moved `fetch_both_stable`'s budget twice
for this symptom and the previous session replaced waiting with bracketing; neither helps a list
that is genuinely still growing when the comparison runs.

The test now creates **its own user** and builds every fixture row as that user, so nothing else
in the binary touches the list it reads. Four consecutive full-suite runs clean afterwards. This
is the same fix as the founder in `users_known` above, and it is the general answer whenever a
suite byte-compares something the shared fixture user owns: **give the assertion an actor nobody
else is writing to.**

### Housekeeping

`create_team` had three byte-identical private copies in three suites; it moved into `common`
when a fourth wanted it. `plant_terms_of_service_row` moved out of `parity/user_get.rs` for the
same reason. `common::create_direct_channel` is new.

`parity/user_get.rs` listed `stats` among the `/users/` literals that must be forwarded; it is
ours now, and the list keeps two entries so it still asserts what it was written to assert.

## `POST /channels/{id}/members/ids`, `POST /teams/{id}/members/ids`, `POST /teams/{id}/channels/ids`, `GET /roles` (2026-09-04)

Four routes. The first three are one shape — `model.SortedArrayFromJSON` over the body, a gate,
one store call — which is why they share a test file and one mutation plan; the fourth needed only
a handler, because `App::get_all_roles` and `role_store::get_all` had landed with
`getRolesByNames` and had no caller until now.

| Layer | File | Status |
|---|---|---|
| api | `crates/mm-api/src/channels.rs` — `get_channel_members_by_ids`, `get_public_channels_by_ids_for_team`, `ids_from_body`, `read_body`, `malformed_channel_id_parameter` | DONE |
| api | `crates/mm-api/src/teams.rs` — `get_team_members_by_ids` | DONE |
| api | `crates/mm-api/src/roles.rs` — `get_all_roles` | DONE |
| app | `crates/mm-app/src/channel.rs` — `get_channel_members_by_ids`, `get_public_channels_by_ids_for_team` | DONE |
| app | `crates/mm-app/src/team.rs` — `get_team_members_by_ids` | DONE |
| store | `crates/mm-store/src/channel_store.rs` — `get_members_by_ids`, `get_public_channels_by_ids_for_team` | DONE |
| store | `crates/mm-store/src/team_store.rs` — `get_members_by_ids` | DONE |
| store | `crates/mm-store/src/error.rs` — `StoreError::Argument`, a new variant | DONE |

Forwarded and unchanged: a **guest** session on `POST /teams/{id}/channels/ids` (below), any caller
without `view_members` on `POST /teams/{id}/members/ids`, every non-POST method on the three new
POST paths, and `PUT /roles/{id}/patch`.

Two existing parity tests asserted these paths were forwarded and now assert they are served:
`parity/team_channel_lists.rs` for `/teams/{id}/channels/ids` and `parity/roles.rs` for `/roles`.
Both moved to the served side rather than losing the assertion.

Tests: 14 cross-server (12 in `parity/by_ids_lists.rs`, 2 in `parity/roles.rs`), one unit test in
`mm-api::channels`, and five store-level assertions folded into
`mm-store/tests/db_team_members.rs`; the parity binary goes 400 → 414.
Mutations: **36 run, 33 caught, 3 controls survived, 0 harness faults**
(`scripts/mutations/by-ids-and-all-roles.plan`) — on the third run. The first two are below; both
were the harness, and three real survivors from the first run are below that.

Each finding lives in the doc comment on the thing it constrains. The ones a reader would
otherwise get wrong:

1. **Three sibling routes, three different answers to "nothing matched".** Channel members and
   team members both serve `[]` with a 200. `getPublicChannelsByIdsForTeam` serves a **404**, and
   it comes from `SqlChannelStore`'s `len(data) == 0` rather than from any lookup of the team — so
   a request naming only private, archived or other-team channels is a 404 with no channel
   involved, and a well-formed team id that names no team is a 404 for the same reason rather than
   as a missing team. Asserted together in one test so neither reads as a fixture accident.

2. **`DeleteAt = 0` is on the team query and not the channel one.** A departed team member is
   absent; a deactivated user's channel membership is returned. Structural rather than
   inconsistent: `TeamMembers` is a tombstone table where leaving sets `DeleteAt`, while
   `ChannelMembers` rows are deleted outright, so the channel query has nothing to filter.

3. **The same request shape, one byte apart.** `getChannelMembersByIds` and
   `getPublicChannelsByIdsForTeam` use `json.NewEncoder(w).Encode` and end in a newline;
   `getTeamMembersByIds` marshals and calls `w.Write`, and does not — as does `getAllRoles`.
   [D-086] again, now with two handlers a reader would expect to agree.

4. **The gate is last in all three by-ids routes.** Both body 400s precede the permission check,
   the reverse of `getChannelMembers` and of every paginated sibling. Over HTTP that is only
   visible by asking the same refused caller twice — once with a good body, once with `[]` — which
   is what the tests do.

5. **`null` is not a parse error.** `SortedArrayFromJSON` returns `(nil, nil)` when the body
   decodes to a nil slice, so `null` lands on `invalid_body_param` beside `[]`, while `{}`, `[1]`,
   `"x"` and an empty body land on `api.payload.parse.error`. Reading `err != nil || obj == nil`
   as "a nil decode is a failure" — the obvious reading — swaps the two ids.

6. **`getPublicChannelsByIdsForTeam` names two different parameters in its two 400s**, two lines
   apart: `channel_ids` for an empty array, `channel_id` — singular — for an id failing
   `IsValidId`. Neither name reaches the wire (`AppError.params` is `json:"-"`; our `message` is
   the untranslated id, [D-092]), so no cross-server test can tell them apart: the [D-149] shape.
   Rather than accept an unfalsifiable mutation, the loop was split into
   `malformed_channel_id_parameter` and a unit test pins the name in-process.

7. **`SanitizeForCurrentUser` blanks two fields, not "the sensitive ones".** `LastViewedAt` and
   `LastUpdateAt` become `-1` on every row but the caller's own; `msg_count`, `mention_count` and
   `notify_props` are all on the wire for other members. The first draft of the parity test
   asserted the counters were blanked too, and Go disagreed — the name is wider than the method.

8. **The guest branch is forwarded rather than ported.** Go re-checks `read_channel` on every
   returned channel when `session.IsGuest()`, and one denial fails the whole request. `IsGuest`
   reads a session prop written at login for a guest account, and guests are licence-gated here,
   so the loop is unreachable — the rule that forwards `ViewUsersRestrictions` rather than
   shipping SQL no test can reach.

9. **Go builds a `props` map and an `idQuery` string in `GetPublicChannelsByIdsForTeam` and then
   uses neither.** The live query is squirrel's `sq.Eq{"pc.Id": channelIds}`. Dead code, not a
   second code path — worth saying because it reads like the query being built.

10. **`getAllRoles` would answer `[]` where `getRolesByNames` answers `null`.** That route's
    `null` is `LocalCacheRoleStore.GetByNames`'s nil slice; there is no
    `LocalCacheRoleStore.GetAll`, so this one reaches `SqlRoleStore.GetAll`, which builds
    `[]*model.Role{}` before appending. Unobservable — the `Roles` table is seeded by the
    migration and never empties — so it is recorded rather than tested.

### A whole-table route cannot be byte-compared while its own suite writes to that table

`all_roles_matches_go_byte_for_byte` failed on its first run and the port was not the reason.
`GET /roles` returns the entire `Roles` table, and `parity/roles.rs` writes to it: two tests
insert and remove synthetic rows, one patches `system_post_all`. So the row *set* changes under
the comparison.

The fix is the `users_known` rule applied to a table rather than an actor. The test now brackets
its fetch with `fetch_both_stable`, compares **row-wise by name** over the rows both servers
returned, and asserts the two normalised properties separately on our own body — no trailing
newline, and the shared names in the same relative order. Byte equality is still asserted, but
only when the two name lists match, which is the ordinary case; that is the assertion that
actually pins the encoding, and everything above it exists so a concurrent fixture cannot make it
lie.

### Three survivors, and what each one was

Every one was a gap in the tests rather than a shrug, and two of the three are shapes worth
expecting again.

- **`tm-ids-empty-guard`** — deleting `SqlTeamStore.GetMembersByIds`'s empty-id-list guard changed
  nothing the parity suite could see, because `getTeamMembersByIds` answers `invalid_body_param`
  for an empty array long before the store is called. The guard is unreachable through its own
  route, the [D-151] shape; it now has a store-level oracle in `db_team_members.rs`, and the
  mutation moved to the `store` suite. That test also pins the three other things the query does
  that REST cannot isolate: the `DeleteAt` filter, the `TeamId` scoping against a user who is a
  member of two teams, and the *absence* of a `Users.DeleteAt` filter.
- **`pc-ids-validation-before-gate`** — moving the per-id `IsValidId` loop past the `view_team`
  gate. Invisible to every caller the gate admits, and the test used the admin, who is never
  refused. **Only a refused caller can see the order of a check that precedes a refusal**, which
  generalises: any "A runs before B" claim where B is a denial needs an actor B actually denies.
- **`roles-all-drops-deleted`** — adding `WHERE deleteat = 0` to `SqlRoleStore.GetAll` changed
  nothing, because nothing the migration seeds has a non-zero `DeleteAt`. The test now plants a
  soft-deleted role of its own and asserts both servers return it. The first draft leaned on a
  synthetic row another test in the same file plants; an assertion that depends on another test's
  timing is not an oracle.

### Two fixture collisions the full-suite run exposed, both cross-suite

Neither was in the routes, and neither shows up unless every test runs at once.

- **A store fixture tripped an api tripwire, from another crate.** `parity/teams_all.rs` opens by
  scanning the whole `Teams` table for a tied `DisplayName`, because its route orders by that
  column with no tiebreak — and `mm-store/tests/db_team_members.rs` seeded three teams all named
  `mmrs team members`. `cargo test --workspace` runs both crates together, so the rows were
  visible to it; and because that store test purges on the way *out*, a **failing** run left the
  tie behind, which then failed all fifteen of `teams_all` until the next store run cleaned up.
  A mutation deliberately breaking the store test is enough to trigger it, which is how this was
  found. The three teams now derive distinct display names from their `name`.
- **[D-160] again, on the test next to the one that was fixed.** The previous session gave
  `channels_for_user::include_deleted_...` its own user so nothing else in the binary could grow
  the list it byte-compares. Its sibling `the_list_is_byte_identical_in_id_order` compares the
  *same* shared list and did not get the same treatment; it failed one full-suite run and passed
  in isolation, the signature. It now builds every fixture row as a dedicated owner too. **The
  rule is the fix, not the instance:** any test byte-comparing a globally-growing collection
  needs an actor nobody else writes to, and applying that to one test in a file is not applying
  it to the file.

### `scripts/mutate-batch.sh` now validates a plan before running any of it

A twenty-minute run died on its eighth line with `SKIPPED (pattern not found)`, and `set -e` threw
away the twenty-eight mutations after it. The cause was `\'` in a plan pattern: `printf %b`
unescapes `\n` and `\\` but passes an unknown escape like `\'` through untouched, so the pattern
looked for `b\'\n\'` in a file that contains `b'\n'`. A Python-side check had already read the
plan as fine, because `unicode_escape` *does* unescape `\'` — a validator that decodes differently
from the runner is not a validator.

`mutate-batch.sh` now walks the whole plan first, decoding each `from` with the same `printf %b`
the loop uses, and refuses to start unless every pattern occurs **exactly once** in its file. Once
matters as much as at-least-once: `mutate.sh` replaces the first occurrence, so an ambiguous
anchor silently mutates whichever copy comes first and returns a verdict about a function nobody
meant to test. Three anchors in this session's plan were ambiguous and were caught this way.

A second run then lost a line to the *same* escape in a **`to`** field, which pre-flight was not
reading — the expensive half, because the pattern applies, the crate does not compile, and the
verdict is gone. Both fields are now scanned for any escape `printf %b` will not expand.

`scripts/mutate.sh` gained the other half of that lesson. Its `restart_server` reported a build
failure and a server that never came up as one **HARNESS FAULT** reading "does not compile, or the
server never came up", and the two need opposite responses: a compile error means the plan is
wrong, a slow start means the verdict was lost to load. Two faults in this session were the second
and were investigated as the first. A build failure now prints the compiler's own lines, and the
start is retried once with a 30-second budget rather than 10 — this machine carries Postgres, the
Go server and a concurrent cargo alongside it.

## `POST /api/v4/posts/ids/reactions` — `getBulkReactions` (2026-09-04)

Served. `crates/mm-api/src/reactions.rs` (`get_bulk_reactions`),
`crates/mm-app/src/reaction.rs` (`get_bulk_reactions_for_posts`),
`crates/mm-store/src/reaction_store.rs` (`bulk_get_for_posts`); 12 parity tests in
`crates/mm-api/tests/parity/post_bulk_reactions.rs`. The webapp posts this once per channel load
with the ids of every post it just rendered.

**The one thing a reader would otherwise get wrong: an empty request is a 500, not a 400.**
`getBulkReactions` has no length check — alone among api4's by-ids handlers — so `[]` and `null`
both reach `constructArrayArgs`, which emits the literal `PostId IN ()`, which Postgres will not
parse. Measured against the running Go server, both bodies answer 500
`app.reaction.bulk_get_for_post_ids.app_error`. The refusal is ported into the *store*, where
Go's lives, because hoisting it into a tidy 400 in the handler would change the status. The
second thing: this route's empty value is `[]` where its neighbour `GET /posts/{id}/reactions`
answers `null` — `populateEmptyReactions` (app/reaction.go:148) writes a literal empty slice for
every requested id, including ids that name no post at all. Both are documented on the code.

### The `COALESCE` survivors from the `getReactions` session are now reachable

That session recorded two mutations it could not kill: `COALESCE(UpdateAt, CreateAt)` and
`COALESCE(DeleteAt, 0)` exist for rows written before a backfill migration, and nothing reachable
over REST produces the NULL they defend against — Go's `SaveReaction` always writes both columns.
The fixture here plants the NULLs directly (`plant_nulls`, guarded by a re-check that fails loudly
if the planting did not happen), so both coalesces are live branches and both mutations die. The
same trick applies to the sibling route's suite, which still has the survivors.

Mutation run: **14 run, 12 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/bulk-reactions.plan`). Every anchor in that plan runs down to
`WHERE postid = ANY($1::text[])`: the two queries in `reaction_store.rs` have byte-identical
SELECT lists and differ only in their WHERE, so any anchor inside the column list alone is
ambiguous and would mutate whichever query comes first.

## `POST /api/v4/posts/ids` — `getPostsByIds` (2026-09-04)

Served. `crates/mm-api/src/posts.rs` (`get_posts_by_ids`, `parse_post_ids`),
`crates/mm-app/src/post.rs` (`get_posts_by_ids`), `crates/mm-app/src/channel.rs`
(`get_channels`), `crates/mm-store/src/post_store.rs` (`get_posts_by_ids`),
`crates/mm-store/src/channel_store.rs` (`get_many`); 14 parity tests in
`crates/mm-api/tests/parity/posts_by_ids.rs` plus 3 unit tests. The webapp calls this to hydrate
permalinks, search hits and thread roots.

**The one thing a reader would otherwise get wrong: this query has no `DeleteAt` filter.** Every
other multi-post read in the store excludes soft-deleted rows; `GetPostsByIds`'s only predicate is
`p.Id IN (…)`, so a deleted post is served with its `delete_at` set and its message already
blanked by the delete. Two more that are close behind: an all-unknown id list is a **404**
(`ErrNotFound` for zero rows) while a list with one known id among unknowns is a 200 that mentions
the misses nowhere; and an unreadable post is **dropped silently**, where the neighbouring
`POST /posts/ids/reactions` refuses the whole request with a 403.

`StripActionIntegrations` is ported but currently unreachable: the only posts with an integration
to strip carry an `attachments` prop, which `REFUSED_PROPS` forwards to Go before the strip runs.
Kept, and documented at the call site, because narrowing that refusal set without it would start
leaking `integration` blocks. `First-Inaccessible-Post-Time` is set on every 200 and is always
`0` without a Cloud `PostHistory` licence — measured, not assumed.

### A survivor the parity suite could not have caught

`ApiError::invalid_param("post_ids")` → `"post_id"` survived the first run, and no fixture could
have fixed it: the parameter reaches a client only through the translated `message`, and this port
serves the raw error id instead ([D-092]), so both spellings are byte-identical on the wire. The
fix was to extract `parse_post_ids` and assert the `Name` param in a unit test — which is also
where the 1000-id cap's off-by-one and the de-duplication-before-counting rule are now pinned.
That mutation's plan line runs against the `unit` suite for the same reason.

Mutation run: **17 run, 15 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/posts-by-ids.plan`).

## `GET /api/v4/users/{user_id}/posts/flagged` — `getFlaggedPostsForUser` (2026-09-05)

Served. `crates/mm-api/src/posts.rs` (`get_flagged_posts_for_user`), `crates/mm-app/src/post.rs`
(`get_flagged_posts`), `crates/mm-store/src/post_store.rs` (`get_flagged_posts`); 12 parity tests
in `crates/mm-api/tests/parity/flagged_posts.rs`. The webapp's "Saved messages" panel.

**The one thing a reader would otherwise get wrong: Go's team filter is missing its
parentheses.** `buildFlaggedPostTeamFilterClause` emits `AND B.TeamId = ? OR B.TeamId = ''`
(post_store.go:609) onto a `WHERE ChannelId IN (members…)`, and `AND` binds tighter — so the
predicate that runs is `(members AND TeamId = ?) OR TeamId = ''`, whose second disjunct has **no
membership check at all**. Every DM and GM has an empty `TeamId`, so a flagged DM post answers for
*any* team id, including one that names nothing. Measured, and reproduced as
`(members AND ($4 = '' OR teamid = $4)) OR ($4 <> '' AND teamid = '')`, the same truth table in
one statement. Second: **`page` is an offset.** The handler hands `c.Params.Page` to the store's
`offset` with no multiplication (api4/post.go:493), so `?page=1&per_page=1` skips one post.

One divergence, deliberate: the handler skips the `GetChannels` call when no post survived the
store, because our `get_many` raises `ErrNotFound` for zero rows and Go answers that request
`200 {"order":[],"posts":{},…}`. The channel map cannot be observed when no post consults it.

### Three survivors, and only one of them was unfixable

- **`page * per_page` is indistinguishable from `page` when `per_page` is 1**, which every
  pagination case used. The suite now also pages two at a time.
- **The handler's per-channel read gate looked unreachable**: the store already requires a
  `ChannelMembers` row, and an ordinary member can always read the channel. The fixture now plants
  a membership row with **no roles** — which `POST /channels/{id}/members` cannot create — so the
  subquery matches, `read_channel` does not, and the gate is what refuses. Go agrees.
- **`app.post.get_flagged_posts.app_error` is only produced by a store failure**, which nothing
  reachable over HTTP causes. Dropped from the plan rather than tolerated silently.

`Posts.DeleteAt = 0` needed the same treatment: `DeletePost` deletes the post's flagged-post
preferences with it, so over REST no flag ever points at a deleted post. The fixture plants the
preference row back.

Mutation run: **15 run, 13 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/flagged-posts.plan`).

### [D-160] a third time, now on a *paginated* unordered scan

`channel_members_list::pages_split_cover_and_run_out_identically` compared each page of
`GET /channels/{id}/members` byte for byte between the two servers. That query has no `ORDER BY`,
so a page is a window onto a scan whose order Postgres does not promise to repeat between two
executions — and `fetch_both_stable`'s third acceptance (Go quiescent across the window) lets a
stable-but-different window through. It held while `channelmembers` was quiet and failed in the
full-suite run once this session's fixtures began writing to that table; it passed in isolation,
the signature. Pages are now compared as **sets of rows** sorted by `user_id`, which keeps every
field under comparison and gives up only the ordering claim neither server makes, plus a new
assertion that both servers page over the same membership. The unpaged byte-for-byte check is
untouched. **The rule, again: an unordered read may not be byte-compared as a sequence — and a
paginated one may not be byte-compared at all.**

## `GET /teams/name/{team_name}/channels/name/{channel_name}` — `getChannelByNameForTeamName` (2026-09-05)

Served. `crates/mm-api/src/channels.rs` (`get_channel_by_name_for_team_name`,
`validate_team_name_then_channel_name`, and the extracted `serve_named_channel`),
`crates/mm-app/src/channel.rs` (`get_channel_by_name_for_team_name`); 10 parity tests in
`crates/mm-api/tests/parity/channel_by_name_for_team_name.rs` plus 1 unit test. The webapp
resolves permalinks this way, because a link carries names and not ids.

**The one thing a reader would otherwise get wrong: the team lookup's *failure* branch is also a
404.** Go writes `app.team.get_by_name.app_error` with `http.StatusNotFound` (channel.go:2368),
so a genuine database failure resolving the team answers 404 where every sibling answers 500.
Beyond that this is `getChannelByName` with the team named: same permission block, same
`FillInChannelProps`, same trailing newline — now shared rather than copied, since the two
handlers differed only in the `where` they stamp on a refusal, and `where` is not a field of the
JSON error body.

### The survivor was a validator whose gap is one character wide

Disabling `IsValidChannelIdentifier` on this path survived the first run. The obvious probe — a
one-character channel name — does **not** 400: there is no minimum-length rule on a channel name,
measured, unlike the two-character minimum on a team name. The reachable gap is the *first*
character: the mux class `[A-Za-z0-9_-]+` accepts a leading `-` or `_` and the validator requires
alphanumeric. The suite now asks for both, and for the one-character name that must still be a
404, so the test cannot pass against a validator that simply rejects everything short.

Mutation run: **12 run, 10 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channel-by-team-name.plan`). Two of those mutations land in the shared tail
and are caught by the older `channel_by_name` suite, which is why the plan filters on both module
names — narrowing to one would let the other decide the verdict.

## `GET /teams/{team_id}/channels/autocomplete` — `autocompleteChannelsForTeam` (2026-09-05)

Served. `crates/mm-api/src/channels.rs` (`autocomplete_channels_for_team`),
`crates/mm-app/src/channel.rs` (`autocomplete_channels_for_team`),
`crates/mm-store/src/channel_store.rs` (`autocomplete_in_team` plus the three term helpers);
13 parity tests in `crates/mm-api/tests/parity/channel_autocomplete.rs` and 3 unit tests. The
Ctrl+K quick switcher, so it fires once per keystroke — the busiest channel read in the app.

**The one thing a reader would otherwise get wrong: `?name=*` is not a wildcard.**
`sanitizeSearchTerm` strips the escape character `*` *before* escaping `%` and `_`, so a term of
`*` sanitises to the empty string — and an empty sanitised term makes `searchClause` return nil,
which Go **omits from the query** rather than adding as an always-false predicate. `?name=*` and
`?name=` return the same 50 channels. Two more: `includeDeleted` is hardcoded `true` in the app
layer, so archived channels are listed; and `FillInChannelsProps` is deliberately *not* called
here, alone among the channel lists, with Go's own comment saying why.

The search clause is `LIKE … OR to_tsquery(…)` and **both halves are load-bearing**: `?name=town
square` matches `town-square` only through the full-text half (no column holds the string with a
space in it), and `?name=copen` matches `mmrs-parity-acopen` only through the LIKE half
(`to_tsquery` matches lexeme *prefixes*, and `copen` starts no lexeme). Go interpolates
`default_text_search_config` into the SQL text; this passes it as a parameter cast to `regconfig`,
which keeps the statement a single compile-checked literal.

Mutation run: **20 run, 18 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channel-autocomplete.plan`). Both survivors of the first run were terms the
*other* half of the clause could also match — the fix was a mid-word term, not a weaker assertion.

### Three repairs the full-suite run forced, all cross-suite

- **A fixture tag is a shared namespace.** `create_plain_user(.., "autoc")` builds the username
  `mmrsplainautoc`, which is exactly the prefix `parity/users_autocomplete.rs` searches for — so
  this suite put an extra user in the middle of that one's corpus and failed three of its tests
  while passing in isolation. Renamed to `chanac`.
- `team_channel_lists::the_sibling_literals_are_still_forwarded_to_go` asserted `/autocomplete`
  was forwarded. It no longer is; `/search_autocomplete` still is, and is a different handler.
- **[D-160], fourth instance, and the previous fix was not enough.**
  `channel_members_list::pages_split_cover_and_run_out_identically` was changed last session to
  compare each page as a *set of rows* instead of bytes. It failed again, and the diff showed the
  two servers had selected **genuinely different members** — not the same ones reordered. A page
  of an `ORDER BY`-less query is a window onto a scan Postgres does not promise to repeat between
  executions, so no per-page comparison across the two servers can hold. The test now asserts
  only what both servers do promise: that two pages of two cover the whole membership, per server,
  and that the two coverings agree — retrying when they do not. The byte-for-byte wire check lives
  on the unpaged read, where there is no window to disagree about.

## `GET /api/v4/emoji/autocomplete` — `autocompleteEmojis` (2026-09-05)

Served. `crates/mm-api/src/emoji.rs` (`autocomplete_emojis`), `crates/mm-app/src/emoji.rs`
(`search_emoji`), `crates/mm-store/src/emoji_store.rs` (`search` and
`sanitize_emoji_search_term`); 9 parity tests in
`crates/mm-api/tests/parity/emoji_autocomplete.rs` plus 2 unit tests. The `:` picker, so it fires
once per keystroke.

**The one thing a reader would otherwise get wrong: the match is case-sensitive.** There is no
`LOWER` on either side of the `LIKE`, unlike the channel autocomplete ported one session earlier —
so `?name=MMRS` finds nothing while `?name=mmrs` finds the list. Two more: it is a **prefix**
match (`prefixOnly` is hardcoded `true`, so the pattern is `name%` and never `%name%`), and
`?name=\` matches **everything**, because `sanitizeSearchTerm` strips the escape character before
escaping `%` and `_` with it, leaving the bare `%`. Go's escape character here is a **backslash**,
not the `*` the channel search uses, and `sq.Like` emits no `ESCAPE` clause — Postgres' default is
what makes it work.

This handler is also the only emoji read with **no `EnableCustomEmoji` gate of its own**. Its
siblings answer 501 and shadow the app layer's 403; here the 403 is what a client would see, which
is why it is pinned by a unit test rather than by the parity suite — nothing over HTTP can turn
the setting off.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/emoji-autocomplete.plan`). Two survivors of the first run: the disabled-gate
status, fixed with the unit test above; and, twice over, a `LIMIT` mutation that has to stay
`LEAST($2, 2::bigint)` — a bare `2` drops the bind and an untyped `2` fails sqlx's type check, and
both surface as a harness fault rather than a verdict.

### Three stale assertions the route retired

`emoji_get::the_autocomplete_literal_is_forwarded`, an assertion inside `emoji_list`, and the
unit test `only_the_get_literals_are_shadowed` all encoded "gorilla's registration order owns
this literal, so we forward it". axum owns it now, for the same reason and in the same direction.
The first is kept — renamed to `the_autocomplete_literal_does_not_land_on_get_emoji`, because what
it guards is `getEmoji`'s routing rather than autocomplete's behaviour — and
`EMOJI_SHADOWED_LITERALS` is now empty but retained, since it is the only thing between a future
`GET /emoji/<literal>` of Go's and a 400 from a handler that thought it had an id.

## `GET /teams/{team_id}/channels/search_autocomplete` — `autocompleteChannelsForTeamForSearch` (2026-09-05)

Served. `crates/mm-api/src/channels.rs` (`autocomplete_channels_for_team_for_search`),
`crates/mm-app/src/channel.rs` (`autocomplete_channels_for_search`),
`crates/mm-store/src/channel_store.rs` (`autocomplete_in_team_for_search` and
`autocomplete_in_team_for_search_direct_messages`); 14 parity tests in
`crates/mm-api/tests/parity/channel_search_autocomplete.rs`. The search box's channel suggestions.

**The one thing a reader would otherwise get wrong: a direct message is listed under the *other
user's username*.** Go selects `channelSliceColumns(true, "C")` — which already contains
`C.DisplayName` — and then appends `OtherUsers.Username AS DisplayName`. Two output columns of
the same name, and the scan takes the last, so the `Channel` handed back carries a display name
the `Channels` row does not have. A DM's stored display name is empty, so a port that used it
would list a blank. Reproduced by selecting the username into that position rather than by
relying on a duplicate-column rule.

Three more, none of them shared with the `/autocomplete` sibling one literal away:

- **No permission gate at all.** The sibling asks for `list_team_channels` and 403s a non-member;
  this route has nothing, and a foreign team answers **200 with an empty list** — the
  `ChannelMembers` join does the work. A port that shared a gate between the two would refuse
  requests Go serves.
- **Membership is required for every channel, public ones included**, so the switcher lists
  channels this does not.
- **It can return more than fifty.** Two fifty-row queries are `UNION`ed, the union is limited to
  fifty again, and then up to fifty direct messages are *appended*. Measured: 58 rows for an empty
  term.

**Parity risk, stated plainly:** Go merges the two passes and sorts with `sort.Slice`, which is
**unstable**. Two channels whose lower-cased display names are equal come back in an order Go
itself does not repeat, and no port can match that. `sort_by` here is stable and keeps
union-then-DM order for ties — *an* order Go could have produced, but not one it promises. Every
fixture in the suite has a distinct display name so the question never arises; a caller with two
identically-named channels is outside what this port can guarantee.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channel-search-autocomplete.plan`). Four survivors on the first run, and one
of them was a **weak mutation rather than a fixture gap**: turning the membership `JOIN` into a
`LEFT JOIN` plus `cm.userid IS NULL` is inert, because every channel has at least its creator as
a member and the NULL row it looks for never exists. Dropping the predicate is the mutation that
means something. The other three were real gaps — every fixture's name and display name said the
same words, so the `LIKE` list could be narrowed to `Name` alone, the full-text branch could be
neutered, and the term's trim could be dropped, all unnoticed. `mmrs-parity-sadisp` is now
displayed as `Zephyrine Quokka`, and the suite asks for a mid-word fragment (only `LIKE` finds
it), the two words reversed (only `to_tsquery` finds it), and a padded mid-word term (only a
trimmed `LIKE` finds it).

### An observation, not yet a diagnosis

Twice now — this session and the emoji-autocomplete one — the **first** full-suite run after a
rebuild has reported a couple of parity failures and aborted early, with two immediate re-runs
clean. The failure output was not captured either time, so there is nothing here but the pattern:
first run after `cargo` rebuilds, a handful of parity tests, never reproducible. Capture the log
on the first run rather than the third when it next happens.

## `GET /users/{user_id}/channel_members` — `getChannelMembersForUser` (2026-09-05)

Served. `crates/mm-api/src/users.rs` (`get_channel_members_for_user`,
`parse_page_allowing_negative`), `crates/mm-app/src/channel.rs`
(`get_channel_members_with_team_data_for_user_with_pagination`),
`crates/mm-store/src/channel_store.rs` (`get_members_for_user_with_pagination`,
`get_members_for_user_with_cursor_pagination`); 13 parity tests in
`crates/mm-api/tests/parity/channel_members_for_user.rs`. Every channel the caller belongs to,
across every team — the webapp asks once per load. First route to reach
`model.ChannelMemberWithTeamData`, which was ported earlier and had no caller.

**The one thing a reader would otherwise get wrong: `?page=-1` is a sentinel, not a page.** It
selects a **newline-delimited stream** (`application/x-ndjson`, one member object per line) that
the handler walks a hundred rows at a time; anything else — including no `page` at all, which
parses to `0` — selects the ordinary JSON array. The shared `parse_page` clamps negatives to the
default because every other route treats them as garbage, so this route needs its own parser.

**And the stream stops on a 404 it only sometimes swallows.** The cursor store call raises
`ErrNotFound` for an empty page rather than returning `[]`, and the loop reads that as "done" —
but Go's guard is `fromChannelID != "" && err.Id == MissingChannelMemberError`, so a caller whose
*first* page is empty gets the 404 itself, with `Content-Type: application/x-ndjson` already set
on it. A user with no channel memberships is the only shape that reaches it, and the REST API
cannot create one (Go joins every new team member to the default channels), so the fixture deletes
the rows directly.

One divergence, not on the wire: Go writes each page to the socket as it reads it; this collects
the walk and answers in one body. The bytes are identical — the tests compare them — but Go's
first line arrives sooner and holds less memory. Recorded at the call site rather than hidden.

Mutation run: **22 run, 20 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channel-members-for-user.plan`).

### Five survivors, one cause

The fixture user held fewer memberships than a single page, so the streaming loop ran **exactly
once** — and its cursor (`ChannelId > ?`), its page-size test (`len < 100`) and its cursor advance
(`.last()`) were all dead code. Three mutations survived on that one gap: widening `>` to `>=`
repeats a row, widening `<` to `<=` stops after the first page, and `.first()` walks the same page
forever. `cmfupaged` now carries **150 planted memberships** — one `INSERT … SELECT`, because
creating that many channels over REST would dominate the suite — so the first page comes back
exactly full and the walk takes two turns. The other two survivors were narrower: the three team
`COALESCE`s need a channel with **no team** (the plain user now has a DM), and the streaming
branch's sanitiser needs a caller reading somebody *else's* list.

**The lesson generalises past this route:** a loop whose fixture fits in one iteration is not
tested, it is only executed. Any paginated walk needs a fixture that crosses a page boundary.

## `POST /api/v4/users/group_channels` — `getUsersByGroupChannelIds` (2026-09-05)

Served. `crates/mm-api/src/users.rs` (`get_users_by_group_channel_ids`),
`crates/mm-app/src/user.rs` (`get_users_by_group_channel_ids`),
`crates/mm-store/src/user_store.rs` (`get_profile_by_group_channel_ids_for_user`,
`UserWithChannelRow`, `MAX_GROUP_CHANNELS_FOR_PROFILES`); 9 parity tests in
`crates/mm-api/tests/parity/users_group_channels.rs` plus 1 unit test. The member profiles behind
a group message's avatar row.

**The one thing a reader would otherwise get wrong: an empty list is a *parse* error.** Go writes
`if err != nil || len(channelIds) == 0 { … PayloadParseError … } else if len(channelIds) == 0 {
SetInvalidParam("channel_ids") }` — the second arm cannot be reached, because the first already
caught the empty list. So `[]` and `null` answer 400 `api.payload.parse.error`, never
`invalid_body_param`, which is the opposite of every other by-ids route in api4 *and* the
opposite of what the dead branch says this one meant to do. Measured.

**There is no permission check on this route** — not in the handler, not in the app layer. The
access rule is an `EXISTS` subquery inside the store's SQL asserting the caller is a member of
each channel it answers for, so a group channel you are not in is *absent from the map* rather
than a 403. Anything that "tidied" that subquery into a forgotten app-layer gate would list every
group channel's members to anyone; the doc comments on all three layers say so.

Two smaller ones: `MaxGroupChannelsForProfiles` **truncates** the id list to fifty rather than
refusing a longer one, and it does so *after* `SortedArrayFromJSON` has sorted — so it is the
fifty lowest-sorting ids that survive, not the first fifty the client wrote, and nothing in the
response says a channel was dropped. And Go builds the `EXISTS` with `fmt.Sprintf`, interpolating
the session's user id straight into the SQL text; this binds it as a parameter instead. Same rows,
and worth naming rather than silently fixing.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/users-group-channels.plan`).

### A test whose premise was wrong, and two survivors that were the same mistake

The first sanitisation test asserted a plain caller sees no email address. It failed against
**both** servers — this deployment has `ShowEmailAddress` on — which is the fixture being wrong,
not the port. What `asAdmin` actually changes on the wire is narrower: the admin's copy carries
`notify_props` and a plain caller's carries `auth_data` instead. The rewritten test pins that
pair, and that no caller ever sees a credential.

Then `Type = 'G'` survived a mutation widening it to "any message channel", because every id the
suite passed was already a group channel. The fixture now includes an ordinary channel and a
direct message the caller *is* a member of, and asserts both come back absent. **The rule
generalises: a filter is only tested by a fixture the filter actually excludes** — the sibling of
last session's "a loop whose fixture fits in one iteration is executed, not tested".

### Next: `getThreadsForUser`, and why it was not this session

`GET /users/{user_id}/teams/{team_id}/threads` is the highest-value unserved read left — the
Threads view — and it is the first route with **no ported neighbours at all**: no thread model, no
thread store, no thread app layer. It needs `model.Thread`/`ThreadResponse`, five store functions
(`GetThreadsForUser` plus four counters that Go runs concurrently), participant hydration, and a
fixture that builds threads, replies, memberships and unread state. That is a session's whole
budget and then some, and it should start cold rather than be tacked onto the end of another.

## `GET /users/{user_id}/teams/{team_id}/threads` — `getThreadsForUser` (2026-09-05)

Served, for the default option set plus `?extended`. `crates/mm-api/src/users.rs`
(`get_threads_for_user`, `serve_threads`), `crates/mm-app/src/thread.rs` (new),
`crates/mm-store/src/thread_store.rs` (new — five queries); 16 parity tests in
`crates/mm-api/tests/parity/threads_for_user.rs`. The Threads view, and the first route to reach
`mm-model`'s `thread.rs`, which was ported long ago with nothing behind it.

**The one thing a reader would otherwise get wrong: participants are id-only stubs.** Without
`?extended=true` each is a `User` with every field but `id` at Go's zero value — so the wire shows
`"username": ""` rather than omitting it. And the embedded post carries **no computed fields**:
`reply_count` is `0` and `participants` is `null` on it however many replies the thread has,
because this query has no reply-count subquery. The thread's own `reply_count` beside it is the
real number. Both measured.

### What is forwarded, and why that is the honest shape

`since`, `before`, `after`, `unread`, `deleted`, `totalsOnly`, `threadsOnly` and `excludeDirect`
each rewrite the store query, and each is handed to Go rather than guessed at — the list is a
constant, and a parity test asserts every one of them forwards, so adding one to the handler
without a fixture fails the suite. A page whose root post carries an `attachments` prop is
forwarded too, for a reason that is not about threads at all: see **[D-166]**.

Two divergences worth naming. Go runs the four counters and the list **concurrently** with an
`errgroup`; this runs them in sequence, which is a latency decision and not a wire one. And
`sanitizeThreadResponse` sanitises participants as a **non-admin** whoever asks — the literal
`false`, not `IsSystemAdmin()` — which is the opposite of every other route that hydrates users.

Mutation run: **23 run, 21 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/threads-for-user.plan`).

### Seven survivors, one shape

Every one was a branch the fixture never reached: no thread carried a `PostsPriority` row, so the
urgency `CASE`, the urgent-mentions counter *and* the config flag that gates both were all dead at
once; none had `Following = false`; none had `LastViewed` exactly equal to `LastReplyAt`, which is
what the strict `<` in the unread counter turns on; none had a deleted reply; and the post
sanitiser had nothing to sanitise, because `createPost` strips `force_notification` on the way in
just as `SanitizeProps` strips it on the way out. A second actor now carries the first four, a
third carries the attachments post — alone, because forwarding is per page — and the notification
prop is planted straight into the table.

**The rule this session adds:** a column-driven branch — a priority row, a boolean flag, an
exact-equality boundary — is dead code until a fixture row carries that value. It is the sibling
of "a loop needs a fixture that crosses a page boundary" and "a filter needs a fixture it
excludes".

### One thing I broke and repaired

`crates/mm-model/src/thread.rs` was **already ported**, and I wrote a new one over it before
checking; `git checkout` restored it, and the original is better than what I wrote (its
`participants` is `Option<StringArray>`, which distinguishes Go's nil from an empty array). Check
for the file before creating it — `mm-model` holds many types with no route behind them yet.

### Three different flaky tests in three full-suite runs

`channel_members_list` (fixed earlier today), then `teams_unread`, then `roles` — each passing in
isolation, each a cross-suite artefact of concurrent fixtures rather than anything about the route
under test. `teams_unread`'s bracketing helper is already as good as the pattern gets and still
lost to three teams being created beside it; `roles` compares a scheme role another suite is
mutating. Both are **observations, not diagnoses** — recorded so the next session sees the shape
rather than re-deriving it. A store test *was* diagnosed and fixed: `db_user_profile_lists`
asserted the whole development database fit in one 200-row page, which stopped being true after a
night of fixtures; it now walks the pages, which is what its own failure message asked for.

## Suite stability, not a route (2026-09-05)

Three consecutive full-suite runs had each failed **one different test** — `channel_members_list`,
then `teams_unread`, then `roles` — every one passing in isolation. This session spent itself on
that instead of an eleventh route, because a suite that fails somewhere different each run cannot
tell anyone whether the next route works.

Two causes, both found and fixed. Two consecutive clean runs afterwards: **2412 passed, 0 failed**.

### The development database had 16,066 orphaned channels

`purge_api_fixtures` deleted by name prefix, which never reached the `town-square` and `off-topic`
Go creates for each fixture team — so deleting the team orphaned them, every run, for as long as
the project has had this suite. An orphan's dangling `TeamId` arrives as NULL through the
channel-member join and is therefore listed under *every* team, and its empty display name ties
under `ORDER BY DisplayName`: exactly the ordering flakes that have been patched one test at a
time for two days. Alongside them sat 3,190 `Threads` rows whose root post no longer existed,
against 4 real ones.

The purge now sweeps by the **dangling reference** — a channel whose team is gone, a post whose
channel is gone, a thread whose post is gone — which is what [D-155]'s own note asked for and is
now closed. 16,066 orphans → 0; 16,253 channels → 189; 3,194 threads → 4.

### And one fixture was writing into other suites' rows

With the noise gone, the remaining failures collapsed onto a single suite and said the same thing
every run: `channel_members_list` expects its channel to hold four members and found five.
`channel_members_for_user`'s bulk fixture — added yesterday to make the streaming walk cross a
page boundary — planted 150 memberships into **150 arbitrary existing channels**, whichever had
the lowest ids. When another suite's fixture channel fell in that range, its member count changed
underneath it.

It now creates 150 synthetic channels in its own team and plants into those. **A fixture may only
write rows it owns**: selecting existing rows by anything other than its own prefix is writing
into somebody else's test.

### What this says about the earlier "flakes"

Several tests were relaxed over the last two days — comparing pages as sets, then as counts, then
retrying until a read settled — on the reasoning that an unordered scan may reshuffle. That
reasoning was sound and those changes are still right. But the *frequency* was not inherent: it
was 16,000 junk rows and one fixture writing where it should not. A test that has been relaxed
twice is worth re-reading as evidence about the environment rather than the assertion.

## `GET /users/{user_id}/teams/{team_id}/threads/{thread_id}` — `getThreadForUser` (2026-09-05)

Served, including `?extended`. `crates/mm-api/src/users.rs` (`get_thread_for_user`),
`crates/mm-app/src/thread.rs` (`get_thread_membership_for_user`, `get_thread_for_user`),
`crates/mm-store/src/thread_store.rs` (`get_membership_for_user`, `get_thread_for_user`);
9 parity tests in `crates/mm-api/tests/parity/thread_for_user.rs`. The single-thread twin of
`getThreadsForUser`, and what the webapp asks for when a thread is opened.

**The one thing a reader would otherwise get wrong: the two 404s carry different error ids.**
No `ThreadMemberships` row — an id that names nothing, or a thread this user never replied to —
refuses at the membership lookup with `app.user.get_thread_membership_for_user.not_found`. A row
that exists with `Following = false`, which is what `DELETE …/threads/{id}/following` leaves
behind, gets past that lookup and is refused by the store with
`app.user.get_threads_for_user.not_found`. Both are 404s, both reachable from the wire, and a
client branching on the id can tell them apart. `team_id` is validated by `RequireTeamId` and then
never read again — not by the permission checks, not by the store — so a thread answers the same
under any team's path.

Two divergences from the list route beside it. The post is `LEFT JOIN`ed here, not inner-joined,
so `"post": null` is reachable for a `Threads` row that outlived its root. And `LastViewedAt` and
`UnreadMentions` come from the `ThreadMembership` argument rather than the join — Go assigns them
after the query returns (thread_store.go:607), and the unread-replies subquery binds that same
`LastViewed` as a value.

Mutation run: **17 run, 15 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/thread-for-user.plan`). Full suite: 2421 passed, 0 failed.

### Five survivors, and the state the REST API quietly took back

The first run caught 9 of 16. Every survivor was a branch no fixture row reached, and three of
them shared a cause worth naming: **posting into a channel marks its threads viewed for the
poster.** With collapsed threads enabled Go moves `LastViewed` to *now* for the poster's thread
memberships in that channel, so a read mark set early in a fixture is silently overwritten by the
next reply. `unread_replies` was therefore always 0, which made the subquery's cutoff, its
`DeleteAt = 0` filter and its `RootId` join indistinguishable from each other and from nothing.
The fixture now marks read **last**, at an explicit timestamp rather than `now()`, and asserts the
row still holds it.

The same write reached the fixture from a second direction: `other_methods_are_forwarded` was
PUTting `/threads/{followed_root}/following` to check the sibling route still forwards, and Go's
follow route sets `LastViewed` too. Yesterday's rule was *a fixture may only write rows it owns*;
this is the same rule one level down. **A forwarding test should touch nothing** — it reads a
header, so its path can name an id that does not exist, and now does.

The third was `sanitizeThreadResponse`. Its three props (`add_channel_member`,
`force_notification`, `silent_notification`) **cannot be set through `POST /posts`** — Go strips
them from client input at creation — so the branch is dead unless the row is written directly. The
fixture plants them, plus one key that must survive. Writing that test found the key is
`silent_notification`, not `silent`; the shorter name passes through both servers untouched.

Two mutations were **dropped rather than carried as survivors**, because neither asks a question
this route can answer:

- Blanking the `details` string Go passes at app/user.go:3094 is invisible from the wire.
  `detailed_error` is wiped at the api boundary unless `EnableDeveloper` is on. The value is still
  carried in the port, because it is Go's.
- `t.postid = $1` → `>= $1` is a coin flip on a table holding a handful of threads: a `>=` scan
  usually returns the same row. `store-thread-binding` asks the same question — which value
  reaches `$1` — deterministically, by binding `user_id` instead.

## The `me` alias, on four routes that had lost it (2026-09-05)

Not a route — a wire bug in a *class* of routes. `crates/mm-api/src/channels.rs` (`resolve_me`),
`crates/mm-api/src/users.rs`, `crates/mm-api/src/posts.rs`; 2 parity tests in
`crates/mm-api/tests/parity/me_alias.rs`.

`RequireUserId` (web/context.go:296) substitutes the session's user id for the literal `me`
**before** calling `IsValidId`. Every api4 route with a `{user_id}` segment therefore accepts it,
and the webapp prefers it to the real id on most reads. Four served routes validated first and so
answered **400 where Go answers 200**: `/users/me/channel_members`, `/users/me/posts/flagged`,
`/users/me/teams/{team}/threads` and `.../threads/{thread}` — the four most recently added, all
shipped in the last two days. The older routes each carry their own copy of the resolution and
were correct; the copies are now one `resolve_me` helper.

**The one thing a reader would otherwise get wrong: no route's own suite can find this.** Each
tests its route with an explicit id, which is the one input that cannot show the bug. The test is
therefore shaped like the bug — `every_served_user_route_accepts_me` walks all twenty served
`{user_id}` routes and compares statuses across both servers, and **a new route with a `{user_id}`
segment belongs in that list**. Its sibling asserts the alias resolves to the *session's* user
rather than merely to something valid, because a substitution of the wrong id still answers 200.

Mutation run: **7 run, 5 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/me-alias.plan`). Full suite: 2423 passed, 0 failed.

### And one flaky test whose premise was global state

`users_stats::a_deactivated_user_leaves_the_count` failed a full-suite run with `62 -> 62`. It
asserted `after < before` on `total_users_count` — a **global** counter that fifty other suites
move throughout the run, so one concurrent `create_plain_user` cancels the drop exactly. The
failure said nothing about the route. It now asserts what is actually observable: the doomed row
carries a non-zero `DeleteAt`, and the route's total equals the database's count of rows that
predicate leaves, read in the same bracket as `the_count_matches_the_database_including_bots`
uses. A route that failed to exclude the row would be one higher than the database whoever else
was creating accounts.

## `GET /users/{user_id}/teams/{team_id}/drafts` — `getDrafts` (2026-09-05)

Served. `crates/mm-api/src/drafts.rs` (new), `crates/mm-app/src/draft.rs` (new),
`crates/mm-store/src/draft_store.rs` (new — one query), `crates/mm-app/src/config.rs`
(`allow_synced_drafts`); 7 parity tests in `crates/mm-api/tests/parity/drafts.rs`. The webapp asks
for this once per team load, and `mm-model`'s `draft.rs` had been ported with nothing behind it.

**The one thing a reader would otherwise get wrong: the `{user_id}` segment is decorative.** The
handler passes `c.AppContext.Session().UserId` to the app layer, never `c.Params.UserId`, so
`/users/{anybody}/teams/{team}/drafts` returns **the caller's own** drafts — measured with a second
user's id in the path. And nothing in this handler validates an id: there is no `RequireUserId`,
no `RequireTeamId`, and its first statement is `if c.Err != nil`, so `/users/short/teams/{team}/…`
is a 200 where every neighbouring route gives 400. The router's `[A-Za-z0-9]+` charset is the only
filter either segment passes through.

Three more measured shapes. The feature gate is a **501** (`api.drafts.disabled.app_error`) and it
runs *before* the permission check, so a caller holding nothing still gets the 501 rather than a
403. The permission checked is `view_team` and the one reported is `create_post` — reproduced, but
not observable, because `SetPermissionError` puts it in `DetailedError` and the api boundary wipes
that unless `EnableDeveloper` is on. And every draft carries `"metadata": {}` whether or not it
has files: `getFileInfosForDraft` returns `(nil, nil)` for a draft with no file ids, which is the
*success* path, and `omitempty` on a pointer tests the pointer.

A draft holding a file whose mini preview would have to be generated is **forwarded** — Go reads
the file backend and writes the row back, which this port cannot do. Same `PrepareError::Unreproducible`
path `getFileInfo` uses, and the same narrow guard.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/drafts.plan`). Full suite: 2430 passed, 0 failed.

### The first batch was void, and all three causes were mine

- **A control that does not compile voids the run.** `control-rename-binding` renamed a binding
  used three lines below its anchor. A control is supposed to survive; one that fails to build
  reports a harness fault and throws away the other seventeen verdicts.
- **INNER → LEFT JOIN is inert here.** `cm.userid = $1` stays in the `WHERE` clause and discards
  exactly the NULL-extended rows a left join would add. The mutation now breaks the join *column*,
  which is the decision a reader could actually get wrong.
- **No fixture row had a NULL `Props`.** `upsertDraft` always writes at least `{}`, so the
  "NULL is an empty map, not nil" branch was dead. Probed against Go first — a planted NULL reads
  back as `"props": {}` — then planted in the fixture. `Props` and `Priority` are both `varchar`
  columns holding JSON text, unlike `Posts.Props`, so this port parses them itself and an
  all-`{}` fixture cannot tell a working decoder from one that returns the empty map for
  everything.

### Drafts orphaned by a deleted channel

`purge_api_fixtures` now sweeps them, by the same dangling-reference rule as [D-155]. Nine had
accumulated against two live ones: `getDrafts` inner-joins `ChannelMembers`, so an orphaned draft
is invisible to the only route that would otherwise reach it.

## `GET /api/v4/users/email/{email}` — `getUserByEmail` (2026-09-05)

Served. `crates/mm-api/src/users.rs` (`get_user_by_email`), `crates/mm-app/src/user.rs`
(`get_user_by_email`), `crates/mm-store/src/user_store.rs` (`get_by_email`); 10 parity tests in
`crates/mm-api/tests/parity/user_by_email.rs`.

**The one thing a reader would otherwise get wrong: this is not `getUser` with a different
lookup.** It omits two whole blocks the other two single-user reads have, and both omissions are
on the wire:

1. **No terms-of-service branch**, so `terms_of_service_id` and `terms_of_service_create_at` are
   never present here — not even for an admin, not even for the caller themselves.
2. **No `is_self` case in the sanitiser.** `getUser` and `getUserByUsername` call
   `user.Sanitize(map[string]bool{})` when the target is the caller, keeping every field. This one
   always calls `SanitizeProfile(user, IsSystemAdmin())`, so **looking yourself up by email
   returns the stranger's view of you**: no `notify_props`, `auth_data` blanked to `""`.

Reusing this crate's shared `respond_with_user` tail would have silently added both back; two of
the mutations exist to catch exactly that tidy-port mistake.

Two more measured shapes. The gate is on the sanitize *option*, not on a permission:
`GetSanitizeOptions(isAdmin)["email"]` is `ShowEmailAddress || isAdmin`, and a false value is a
403 **before** the lookup, so nothing leaks about whether the address exists. And `GetByEmail` is
`Where("Email = lower(?)")` — the **parameter** is lowered, not the column — so a row whose stored
address has capitals is unreachable by email on both servers, whichever case the caller sends. Go
cannot write such a row; the fixture plants one.

The route is registered as a **wildcard** (`/users/email/{*email}`) because gorilla's pattern is
`{email:.+}`, whose `.` matches a slash. `GET /users/email/verify` therefore lands here as the
invalid address `verify` rather than on the `POST /users/email/verify` route beside it — measured,
and asserted.

Mutation run: **13 run, 11 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/user-by-email.plan`). Full suite: 2440 passed, 0 failed.

### Two things the mutation harness taught, again

- **A control must cover its binding's whole life.** Renaming only the declaration is a compile
  error, which the batch reports as a harness fault and which voids every other verdict in the
  run. Three batches have now been lost to it, so the rule is written into the plan files.
- **Removing a config gate is inert when the config makes the gate always pass.** `ShowEmailAddress`
  is on here, so deleting the `email` option check changes nothing. The mutation now inverts the
  gate's polarity instead, which at least pins that it reads that option and not a constant. The
  refusing half needs a server with `ShowEmailAddress` off and is not testable on this deployment.

### And a store test whose premise was one page of a shared table

`db_user_profile_lists::not_in_team_lists_the_left_member_and_never_the_current_ones` read page
zero of `GetProfilesNotInTeam` at 200 rows and expected its two fixture users. That listing is
everyone outside the fixture's team — the whole development database — and the parity suites
create `mmrsplain%` users concurrently, which sort *before* `mmrsulist-` and pushed the later of
the two off the page. It now walks every page. That surfaced the second half: paging by
`OFFSET page * perPage` over a table being inserted into returns the same row twice ([D-160]), so
the walk is deduplicated. The claim is about membership of the listing; the paging itself is
pinned by `parity/users_list.rs`.

## `GET /teams/name/{team_name}/exists` — `teamExists` (2026-09-05)

Served. `crates/mm-api/src/teams.rs` (`team_exists`, plus a fix to `get_team_by_name`); 8 parity
tests in `crates/mm-api/tests/parity/team_exists.rs`. The join and signup flows ask this before
offering a team.

**The one thing a reader would otherwise get wrong: "exists" means "you can see it".** The route
never 404s and never 403s — a name that matches nothing and a team the caller may not see are the
same `{"exists":false}` with a 200, which is the point: it must not tell a stranger which private
teams are out there. Visibility is three branches:

```go
(teamMember != nil && teamMember.DeleteAt == 0) ||
(team.AllowOpenInvite && SessionHasPermissionTo(list_public_teams)) ||
(!team.AllowOpenInvite && SessionHasPermissionTo(list_private_teams))
```

Note what is **not** there. `getTeamByName` guards on `AllowOpenInvite || Type != TeamOpen` and
falls back to `view_team` *on the team*; this one ignores `Type` entirely and asks for a
**system-level** list permission. A team with open invites off is therefore visible to an admin
and to nobody else, however public its type. And a **left** membership does not count — the row
survives a `DELETE` with a non-zero `DeleteAt`, so a user who left a private team stops being able
to see that it exists.

Wire format: `w.Write([]byte(MapBoolToJSON(resp)))`, so **no trailing newline** ([D-086]).

Mutation run: **11 run, 9 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/team-exists.plan`). Full suite: 2448 passed, 0 failed.

### `web/params.go` lowercases the name segments, and two served routes did not

`params.TeamName = strings.ToLower(props["team_name"])` (params.go:178), and the same for
`channel_name`. It applies to **every** route with those segments, before any handler sees them —
so `/teams/name/MMRS-PARITY-X` is the same request as the lowercase one. The two channel routes
already did this; **both team routes validated the raw segment** and answered 400 where Go answers
200. `get_team_by_name` had shipped with the bug; `team_exists` was written with it. Fixed in both,
and the test covers both, because no single route's suite would have found it — the same shape as
the `me` alias two entries up.

### Two test premises that were wrong, not the port

`IsValidTeamName` is `isValidAlphaNum` plus a minimum length of **2** — so `ab` is valid and only a
single character is too short — and it carries **no reserved-name check**: `signup`, `login` and
`admin` are all valid team names here, whatever `CleanTeamName` next door suggests. Uppercase is
valid too, for the lowercasing reason above.

### And a survivor that was a dead branch

`member.is_some_and(|m| m.delete_at == 0)` → `member.is_some()` survived the first batch: every
membership row the fixture could reach had `DeleteAt == 0`, so the two agreed everywhere. A second
plain user now joins the private team and leaves it, which is the only way to produce the row that
tells them apart.

## `POST /api/v4/users/usernames` — `getUsersByNames` (2026-09-05)

Served. `crates/mm-api/src/users.rs` (`get_users_by_names`), `crates/mm-app/src/user.rs`
(`get_users_by_usernames`), `crates/mm-store/src/user_store.rs` (`get_profiles_by_usernames`);
6 parity tests in `crates/mm-api/tests/parity/users_by_names.rs`. The webapp posts the usernames
it found in a page of posts, so this fires once per channel load with whatever `@mentions` were
on screen.

**The one thing a reader would otherwise get wrong: this query has no `DeleteAt` filter at all.**
Every neighbouring user query has one, and adding it here is the likeliest wrong port — a
**deactivated** account is returned like any other, which is what lets a client render an old
mention. `GetProfilesByUsernames` takes a `UserGetOptions` carrying only `ViewRestrictions` and
never reads `Active`, `Inactive` or `Role`.

Nothing validates a username either, on the list or on its members, and there is no not-found: a
request for five names can answer with two, and the caller cannot tell "no such user" from "not
allowed to see them" — the same guarantee `getUsersByIds` gives. The two 400s are Go's order,
`SortedArrayFromJSON` first (`api.payload.parse.error`) and the empty list second
(`invalid_body_param`); a body of `null` reduces to zero names without an error and so lands on
the *second*. `json.Marshal` + `w.Write`, so **no trailing newline** ([D-086]).

Mutation run: **9 run, 7 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/users-by-names.plan`). Full suite: 2454 passed, 0 failed.

Three mutations are deliberately absent, each because nothing on the wire could see it: the app
layer's error id (reachable only from a store failure), the `Name` in the empty-list 400
(`params` is not serialised into an `AppError`), and dropping the restrictions fast-path forward
(no fixture caller has non-nil restrictions). Writing the plan also caught one of my own
mutations doing nothing — it renamed `let body` to `let mut body` without appending the newline it
was named for. **A mutation that does not do what its name says is worse than none**: it reports
a catch that belongs to a different change.

## `POST /api/v4/emoji/names` — `getEmojisByNames` (2026-09-05)

Served. `crates/mm-api/src/emoji.rs` (`get_emojis_by_names`, `get_emoji_name_literal`),
`crates/mm-app/src/emoji.rs` (`get_multiple_emoji_by_name`); 7 parity tests in
`crates/mm-api/tests/parity/emoji_by_names.rs`. The store's `get_multiple_by_name` already
existed with only the post-metadata path behind it. The webapp posts the emoji names it found in
a page of posts, once per channel load, beside `POST /users/usernames`.

**The one thing a reader would otherwise get wrong: system emoji names are filtered out of the
*request*, not the answer.** Go compacts the list in place before querying, so `["+1"]` asks the
database for nothing and returns `[]` — asking for a built-in is neither an error nor a hit. This
route answers about *custom* emoji only.

The four refusals are ordered, and the order is on the wire: decode 400, then the empty-list 400,
then the `EnableCustomEmoji` **501**, then the 200-name cap's 400. An empty body on a server with
custom emoji disabled is the 400, not the 501; a 201-name body on that same server is the 501, not
the cap's 400. `json.NewEncoder(w).Encode` gives a **trailing newline** and `[]` rather than
`null` — both the store and the filtered-to-nothing branch allocate.

Mutation run: **10 run, 8 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/emoji-by-names.plan`). Full suite: 2461 passed, 0 failed.

### Registering a literal route changes what happens to its other methods

Go registers `/emoji/names` for `POST` only, so a `GET` fails the method match and gorilla
**falls through** to `/emoji/{emoji_id}`, where `RequireEmojiId` rejects the literal — a 400 both
servers produced from the same handler, and one `emoji_get`'s suite has asserted since it was
written. axum has no such fallthrough: once `/api/v4/emoji/names` is a route it answers every
method, so the `GET` silently began forwarding. Spelled out as `get_emoji_name_literal`.
**Adding a literal route is a change to the `{id}` route beside it**, and only the full-workspace
run sees it — a filtered run of the new suite passes either way.

### Two mutations dropped as untestable rather than carried as survivors

Removing the system-emoji filter entirely changes nothing observable: a custom emoji **cannot be
named like a built-in** (`IsValidEmojiName` refuses `model.emoji.system_emoji_name.app_error`), so
an unfiltered `smile` reaches the query and matches no row. Only a direct `INSERT` could tell them
apart, and that row would outlive the suite's `mmrsparity`-prefix purge. The surviving
`is_none`/`is_some` mutation still pins the predicate's direction. And removing the
`custom.is_empty()` early return is inert here for a reason worth knowing: it exists in Go because
`constructArrayArgs` emits `Name IN ()` for zero names, which Postgres rejects — the guard is what
stops a 500. This port binds `name = ANY($1)`, which is legal and empty for an empty array.

### `emoji_list` was comparing a whole shared table unbracketed

It reads every emoji while the other emoji suites create and soft-delete rows throughout the run,
with plain `fetch_both` — so one side carried a `mmrsparitydoomed` row the other had already lost.
[D-160]'s shape. Now `fetch_both_stable`.

## `POST /api/v4/emoji/search` — `searchEmojis` (2026-09-05)

Served. `crates/mm-api/src/emoji.rs` (`search_emojis`, `get_emoji_search_literal`),
`crates/mm-model/src/utils.rs` (`decode_one_from_json`); 7 parity tests in
`crates/mm-api/tests/parity/emoji_search.rs`. The app's `search_emoji` and the store's `search`
already existed behind `autocompleteEmojis`. The emoji picker posts this on every keystroke.

**The one thing a reader would otherwise get wrong: this is the only emoji route whose config
refusal is a 403.** The five others check `EnableCustomEmoji` in their *handler* and answer 501,
which shadows the app layer's 403. `searchEmojis` has no handler check at all, so the same feature
flag gives a client a different status depending on which emoji route it asked. Recorded on
`App::search_emoji`, whose other caller does have the handler check.

The two 400s carry the same id **and** the same parameter name — `SetInvalidParamWithErr("term")`
for a body that will not decode, `SetInvalidParam("term")` for an empty term — so `not json`,
`[]`, `{}`, `null`, `{"term":""}`, `{"prefix_only":true}` and an empty body are one answer between
them. The limit is `web.PerPageMaximum` (200) as a literal; there is no `per_page` on this route.

Mutation run: **9 run, 7 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/emoji-search.plan`). Full suite: 2468 passed, 0 failed.

### `json.NewDecoder(r.Body).Decode` is not `json.Unmarshal`

It reads **one** value and ignores what follows, so `{"term":"a"}{"term":"b"}` decodes to the
first object and answers 200 where `serde_json::from_slice` would call it trailing characters and
400. `decode_one_from_json` deserializes from a `Deserializer` without calling `end()`, which
reproduces that, and it inherits `replace_lone_surrogates` — the other half of the difference,
since Go decodes a lone `\uD800` to `U+FFFD` where serde fails the whole body. A parity test sends
the two-object body and a mutation swaps the helper back to `from_slice`.

### Two mutation lessons, both repeats

`AppError.params` is not serialised, so a mutation that renames a parameter (`emoji_id` to `term`)
is invisible from the wire — the same finding `users-by-names.plan` recorded, made again. The
mutation now swaps the error *id*, which differs by one word and is on the wire. And clippy was
run *after* the batch rather than before, so a lint fix landed in a file the batch had already
run against and the tally had to be earned twice.

## `POST /api/v4/channels/stats/member_count` — `getChannelsMemberCount` (2026-09-05)

Served, for the two deterministic id-resolution cases. `crates/mm-api/src/channels.rs`
(`get_channels_member_count`), `crates/mm-app/src/channel.rs`,
`crates/mm-store/src/channel_store.rs`; 8 parity tests in
`crates/mm-api/tests/parity/channels_member_count.rs`. The webapp posts the sidebar's channel ids
to render their member counts.

**The one thing a reader would otherwise get wrong: Go's answer for a partially-resolvable id
list depends on an in-memory cache, so it is not a function of the database.** `GetChannels` goes
through `localcachelayer` (channel_layer.go:261), which reads each id from `channelByIdCache` and
queries **only the misses**; the sqlstore then returns `ErrNotFound` when the query it actually ran
matched nothing (channel_store.go:1062). For `[known, unknown]` that is a **404 when the known
channel is cached** and a **200 with one entry when it is not** — measured, and a repeat of the
same request flipped it.

Two shapes are deterministic and are served: **every** id resolves (Go queries a subset that all
exist, whichever way the cache falls) and **no** id resolves (nothing can be cached, so the whole
list is queried and matches nothing → 404). Anything in between is forwarded. Reading only the
sqlstore would have produced a port that answered 200 where Go answers 404 roughly half the time.

The same cache layer is why an **empty list is `{}` with a 200** rather than the 404 the sqlstore
alone would give: with zero ids it returns before querying.

Two smaller shapes. The count's `INNER JOIN Users … AND Users.DeleteAt = 0` means a **deactivated
member is not counted** — and there is no `ChannelMembers` deletion column, because leaving a
channel deletes the row outright. And every requested id is seeded to `0`, so a channel nobody is
in is `"<id>": 0` rather than an absent key. The permission loop runs to completion before any
count is read, one refusal refuses the whole request, and the reported permission is
`list_team_channels` whichever branch said no.

Mutation run: **11 run, 9 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channels-member-count.plan`). Full suite: 2476 passed, 0 failed.

### Two harness rules, both re-learned

A SQL mutation must keep every bound parameter **used** — replacing `cm.channelid = ANY($1)` with
`IS NOT NULL` leaves `$1` unbound, which sqlx refuses at compile time, and a compile error is a
harness fault that voids the whole run rather than a verdict. Neutralise with `OR TRUE` instead.
And the seeded-zeros branch was dead until the fixture had a channel with **no members at all**:
every channel has its creator, so leaving is the only way to produce one.

## `POST /teams/{team_id}/channels/search` — `searchChannelsForTeam` (2026-09-05)

Served. `crates/mm-api/src/channels.rs` (`search_channels_for_team`),
`crates/mm-app/src/channel.rs` (`search_channels`, `search_channels_for_user`),
`crates/mm-store/src/channel_store.rs` (`search_in_team`, `search_for_user_in_team`); 10 parity
tests in `crates/mm-api/tests/parity/channel_search.rs`. The "Browse channels" dialog.

**The one thing a reader would otherwise get wrong: private channels are never results, in either
branch.** Both store queries select the channel columns from `Channels` but join `PublicChannels`
— Go's denormalised shadow table — for the team filter, the `ORDER BY` and both halves of the
search clause. That table holds public channels only, so the second branch is *the public channels
you are in*, not *your channels*. A port that searched `Channels` directly would leak private
channels into the browse dialog.

Two more. `includeDeleted` is a literal `true` in both app functions, so the `DeleteAt = 0`
predicate is never added and **archived channels are results** — that is what the dialog's
archived tab reads. And a caller who is neither a lister nor a team member gets `GetTeamMember`'s
**404**, not a 403: Go calls it for the side effect of its error.

Mutation run: **13 run, 11 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/channel-search.plan`). Full suite: 2486 passed, 0 failed.

### The second branch is unreachable, and forcing it exposed the session model

`list_team_channels` is granted by **`team_user`**, which every team membership carries, so a team
member is always a lister and `SearchChannelsForUser` never runs through this route. Stripping the
roles behind a live session does not reach it either: **Go's session cache still holds the
`TeamMembers` it was built with**, so Go stayed on the first branch while this port — reading the
`Sessions` row alone — fell to the second. That is a state the REST API cannot produce, so the
test was dropped and `store-membership-subject` came out of the plan with the reason recorded,
rather than being carried as a survivor. The divergence belongs to the session model, not to this
route.

### serde builds a struct from a JSON array; Go's decoder refuses one

`[]` deserialized to `ChannelSearch { term: "" }` and answered **200 where Go answers 400**.
Checking the route shipped one iteration earlier, `POST /emoji/search` had the same hole and was
worse: `["term", true]` was a **200 on this server and a 400 on Go's**, measured. Both handlers
now decode to a `serde_json::Value` and match on `Object`, and both suites assert the
positional-array case.

### And a neighbour's assertion, caught only by the full run

`team_channel_lists` pins *which router claims each `/channels/<literal>` path*, and had `search`
in its forwarded list. Registering the route flipped it. Asserted the other way rather than
dropped, which is the idiom that suite already used when `/ids` moved — the second time this
session that adding a route changed a neighbour's expectations, and the second time only the
full-workspace run saw it.

## `POST /api/v4/users/search` — `searchUsers` (2026-09-05)

Served for the default option set. `crates/mm-api/src/users.rs` (`search_users`),
`crates/mm-store/src/user_store.rs` (`UserSearchOptions` gains `allow_emails` and
`allow_inactive`, honoured by all three search queries); 9 parity tests in
`crates/mm-api/tests/parity/users_search.rs`. The add-members dialog and the admin console's user
list. `App::search_users_in_team` already existed behind `autocompleteUsers`.

**The one thing a reader would otherwise get wrong: the validation order is the wire.** `limit` is
defaulted to 100 **before** `term` is checked, so `{}` is the *term* 400 and never the limit one;
and `limit` is range-checked **last**, after every permission check, so a body carrying both a
team the caller cannot see and a bad limit is the **403**. Both measured against the running Go
server before any code was written.

All three 400s share one id — `api.context.invalid_body_param.app_error` — and differ only in the
parameter name, which `AppError.params` never serialises. A client cannot tell `props` from `term`
from `limit`, and no mutation in the plan tries to.

Eleven body fields pick a different branch of `App.SearchUsers`' dispatch (user.go:2412) or add a
filter `performSearch` builds. Each is **forwarded whole**, and the suite asserts that even at its
*zero value* — `{"role": ""}` is Go's, because the field's presence is what the port refuses to
approximate, not its value.

`AllowEmails` and `AllowFullNames` are a permission rather than a preference: a system admin
searches `Email`, `FirstName` and `LastName` unconditionally, everybody else only as
`ShowEmailAddress`/`ShowFullName` allow. **The columns the query matches on differ per caller**,
not just the columns the response shows — which is why the fixture plants an address whose local
part appears in no username, the only way to prove the email column is searched at all.

`json.Marshal` + `w.Write`, so **no trailing newline** ([D-086]), and `[]` rather than `null`.

Mutation run: **17 run, 15 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/users-search.plan`). Full suite: 2495 passed, 0 failed.

### Two harness rules, one of them written the iteration before

The first batch aborted in preflight: a mutation was anchored on the doc comment of the item
*before* it rather than after. The second was voided by a fault — `store-email-column` deleted the
arm carrying the only use of `$6`, and sqlx refuses a query with an unused parameter. That is the
rule added to the loop after `channels-member-count.plan` hit it, applied here to a mutation
written before the rule existed. It now neutralises with `AND FALSE` instead of deleting.

## `GET /api/v4/users/stats/filtered` — `getFilteredUsersStats` (2026-09-05)

Served for the non-role option set, and **this removes a forward**: `users_stats.rs` had a test
pinning the filtered variant as Go's, which now asserts the opposite. `crates/mm-api/src/users.rs`
(`get_filtered_users_stats`), `crates/mm-app/src/user.rs`, `crates/mm-store/src/user_store.rs`
(`count`); 6 parity tests in `crates/mm-api/tests/parity/users_stats_filtered.rs`.

**The one thing a reader would otherwise get wrong: `in_team` wins over `in_channel`.** The
store's `else if` (user_store.go:1497) means a request naming both filters on the **team alone** —
measured: the two together return the team's count, not the intersection.

Three more measured shapes. An **unparseable boolean is `false`, not a 400**: `strconv.ParseBool`'s
error is discarded, so `?include_deleted=yes` counts as off. The team join carries
`tm.DeleteAt = 0` and the channel join does not, because leaving a channel deletes the row
outright while leaving a team soft-deletes it. And `json.NewEncoder(w).Encode` gives this route a
**trailing newline**, unlike the unfiltered `/users/stats` beside it, which uses `w.Write`.

The three role parameters add a join, an `IN` list and their own `CleanRoleNames` 400; each is
forwarded at any value, including the empty string Go itself treats as absent.

Mutation run: **14 run, 12 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/users-stats-filtered.plan`). Full suite: 2501 passed, 0 failed.

### Two branches that were dead until a row reached them

`include_remote_users` moved nothing: this installation has **no remote users at all**, and no API
creates one, so the fixture plants a `RemoteId`. And `tm.DeleteAt = 0` was indistinguishable from
no predicate until the fixture had a team **everybody left** — `TeamMembers` rows survive a leave
— whose count is 0 only because of it. An absolute assertion is safe there, unlike the
whole-table counts in the same suite, because no other suite writes to that team.

## Suite stability, again — and this time it is write pressure, not orphans

Five full-workspace runs while finishing this route: **two green (2495, 2501) and three failing, a
different test each time** — `roles::all_roles_matches_go_byte_for_byte`,
`users_list::the_etag_arms_match_go…`, `channel_members_list::pages_split_cover_and_run_out…`.
Every one is a read over shared state that a concurrent writer moved:

- the users-list **etag is `MAX(UpdateAt)` over every user**, so an etag minted a moment before
  the conditional request is legitimately stale and Go answers 200. Fixed here: the mint is
  retried until it survives its own round trip, and a 200 on an etag that did *not* move is still
  the failure the test asserts.
- `channel_members_list` pages by `OFFSET`, and a membership removed between page 0 and page 1
  shifts rows into a duplicate — [D-160]'s signature exactly.
- the `roles` failure reproduces **only under a narrow `--test parity parity::roles` filter** and
  not in a full run, which makes it an intra-suite ordering race rather than a port divergence.

The aggravator is this session's own fixtures: twenty-two routes' worth of suites now create,
deactivate and delete users and teams throughout a run. The bracketed idiom
(`fetch_both_stable`, walk-and-deduplicate) exists and works; it has been applied one failing test
at a time. **See [D-167]** — the remaining whole-table reads should be converted deliberately
rather than as each one fails.
## `GET /api/v4/license/client` — `getClientLicense` (2026-09-06)

Served, for an unlicensed installation. `crates/mm-api/src/license.rs`,
`crates/mm-app/src/license.rs`, `crates/mm-store/src/system_store.rs`; 8 parity tests in
`crates/mm-api/tests/parity/license_client.rs`. Every webapp load calls this before it renders
anything.

**The one thing a reader would otherwise get wrong: there is no query to port.** Go answers from
a map built **at startup** (`PlatformService.LoadLicense`, platform/license.go:49) out of
`MM_LICENSE`, `Systems.ActiveLicenseId`, or a licence file on disk — and the disk case writes the
row, so two of the three are visible to a second process. `mm-app/src/license.rs` reads those two
and returns [`LicenseState`]; `Licensed` means "forward", because the client map is derived from a
signed licence body that is not ported. Only the `nil`-licence answer, `{"IsLicensed":"false"}`,
is ours.

`MM_LICENSE` set on the Go container's environment and not on ours is the one case we get wrong,
and it fails safe: we forward. Same arrangement as [D-156], which now records it.

Two 400s, in an order that matters: an absent **or empty** `format` is
`api.license.client.old_format.app_error`, and any other value is `SetInvalidParam("format")` —
whose id says *body* param for a query-string parameter. The comparison is case-sensitive, so
`format=OLD` is the second error. `w.Write` (license.go:51), so **no trailing newline**.

`read_license_information` is deliberately **not** checked: on the unlicensed path Go's two
branches converge, because `GetSanitizedClientLicense` only deletes keys and the fallback map has
none of them. A permission check whose outcomes are indistinguishable cannot be tested; it lands
with the licensed map.

Mutation run: **17 run, 15 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/license-client.plan`).

### `main.rs` was building the `App` on Go's defaults, not on the environment

`mm_app::config`'s own module doc said `main.rs` uses `Config::from_env`; it used `App::new`,
which takes `Config::default()`. So every `MM_<SECTION>_<SETTING>` [D-156] arranged to be read was
read by the tests and by nothing else — a deployed server disagreed with a configured Go server on
all ten settings, silently, and `MM_LICENSE` would have joined them. Fixed here because this route
is the first whose *answer* depends on an environment value.

## `GET /api/v4/bots` — `getBots`: **blocked**, and it reopens [D-130] (2026-09-06)

Not served. The route was picked, the Go source read, and the port abandoned before a line was
written, because the **pinned reference and the running image disagree about the body**.

`model.Bot` (bot.go:24) declares nine fields and `mm-model/src/bot.rs` already ports all nine with
a fixture. The running `11.11.0-rc1` answers with **ten**, and the extra one — `system_owned` — is
the object's first key:

```json
{"system_owned":false,"user_id":"rcw3d9…","username":"calls", …}
```

The string `system_owned` occurs **nowhere** in `reference/mattermost/`. Probing for the reverse
found it: `GET /users/{user_id}/channel_join_requests` is registered in the pinned tree
(`api4/channel_join_request.go:31`) and 404s on the running server. Both differences fit one
ordering — rc1 is an *earlier* cut of 11.11.0 than the pinned SHA — so the reference is neither a
superset nor a subset of the forward target.

[D-130] closed on "every suite passes against it". That was true of every route ported at the
time; it was not a claim about routes nobody had looked at yet. Reopened with the measurement, and
with what is owed: build an image from the pinned SHA, which would drop the qemu emulation as
well.

**The rule this establishes:** when the live server's shape does not match the source, the route is
skipped and recorded. Matching the source would serve a body our own proxy's target does not;
matching the server would mean inventing semantics for a field no readable source describes.

## `GET /api/v4/users/{user_id}/audits` — `getUserAudits` (2026-09-06)

Served. `crates/mm-api/src/audits.rs`, `crates/mm-app/src/audit.rs`,
`crates/mm-store/src/audit_store.rs` (and `StoreError::OutOfBounds`); 11 parity tests in
`crates/mm-api/tests/parity/user_audits.rs`. The webapp's *Profile → Security → View Access
History*. `mm-model`'s `audit.rs` was already ported, so this is store, app and edge only.

**The one thing a reader would otherwise get wrong: an empty page is `null`, not `[]`.**
`SqlAuditStore.Get` declares `var audits model.Audits` — a nil slice — and sqlx's `Select` appends
into it, so a no-row query never allocates. The bot store beside it starts from `[]*model.Bot{}`
and answers `[]` for the same shape of query: the difference is the initialiser, not the query.
The first version of this port assumed `[]` and a probe of the running server said otherwise.

Two more worth naming. The refusal names **`edit_other_users`** — a write permission gating a
read, which is Go's own choice. And `ORDER BY CreateAt DESC` has **no tiebreak**, so two rows
sharing a millisecond have no defined order on either server; the suite asserts the set as well as
the bytes rather than adding an `Id` tiebreak that would make our order *more* defined than Go's.

`json.NewEncoder(w).Encode`, so there is a trailing newline. No etag, though `model.Audits` has
one — the absence is Go's.

Mutation run: **16 run, 14 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/user-audits.plan`).

### Three survivors, and each was a fixture that could not tell right from wrong

- **`page * per_page` versus `page`.** The pagination test used `per_page = 1`, where the two are
  numerically equal. The subject now logs in three times — six rows — and the test pages by two.
- **The store's `limit > 1000` bound** is unreachable through this route, because
  `web.ParamsFromRequest` clamps `per_page` to 200 first. It has a unit test of its own now,
  against a `connect_lazy` pool that opens no connection, so the branch is tested where it can be
  rather than carried as an `api` survivor.
- **The permission *name* in the 403** is not on the wire at all: `MakePermissionError` puts it in
  `detailed_error`, and `handleContextError` wipes that whenever `EnableDeveloper` is off. The
  mutation was dropped rather than carried, and the plan says why.

### `serde_json::Value` cannot assert field order

Two assertions in this session compared `value.as_object().keys()` against Go's field order and
failed against *alphabetical*: a `serde_json::Value` object is a `BTreeMap` unless the
`preserve_order` feature is on. Both now assert the key **set** through `Value` and the field
**order** on the bytes. Anything in this repo asserting order through a parsed `Value` is asserting
nothing.

## `GET /api/v4/hooks/incoming` — `getIncomingHooks` (2026-09-06)

Served. `crates/mm-api/src/webhooks.rs`, `crates/mm-app/src/webhook.rs`,
`crates/mm-store/src/webhook_store.rs`, plus `Config::enable_incoming_webhooks`; 10 parity tests
in `crates/mm-api/tests/parity/incoming_hooks.rs`. The webapp's Integrations page. `mm-model`'s
`incoming_webhook.rs` was already ported with fixtures, so this is store, app and edge.

**The one thing a reader would otherwise get wrong: `team_id` chooses the *scope of the permission
check*, not just the filter.** With a team the two permissions are asked on that team
(`SessionHasPermissionToTeam`); without one they are asked at **system** scope
(`SessionHasPermissionTo`). A team admin is therefore allowed on `?team_id=<its team>` and refused
on the bare route, in the same second — and a port that collapsed the two checks would fail
**open**. That caller is the only fixture in the suite that can tell them apart, and building it
needed `PUT /teams/{id}/members/{id}/schemeRoles`, because no role on a stock server grants a
webhook permission except `system_admin` and `team_admin`.

Two more. `manage_others_incoming_webhooks` does not gate the route — it **clears the user
filter**, so the same request returns one user's hooks or everyone's depending on a permission the
response says nothing about. And `include_total_count=true` turns the **array into an object**, a
JSON type change rather than an added field; the count is taken with the same cleared filter the
page used, so it cannot disagree with the array beside it.

`json.Marshal` + `w.Write`, so **no trailing newline**. An empty list is `[]`, not `null` — both
store functions start from `[]*model.IncomingWebhook{}` (webhook_store.go:179, :199), which is the
exact opposite of `getUserAudits`' nil slice and is why both are asserted.

Mutation run: **19 run, 17 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/incoming-hooks.plan`).

### The fixture is planted, because the API cannot build the rows that matter

Only an admin may create an incoming webhook, so the API cannot produce a hook owned by a
non-admin — the row that makes `manage_others_incoming_webhooks` observable at all. Nor a
soft-deleted hook, nor two hooks sharing a display name. All five rows go straight into
`IncomingWebhooks`; Go caches only `GetIncoming(id)` (localcachelayer/webhook_layer.go:41) and
never the list queries, so a direct write is visible to both servers at once.

The two hooks sharing a display name are inserted in **reverse id order** on purpose: without the
`Id` half of `ORDER BY DisplayName, Id` the heap order wins, and the mutation dropping it is
invisible to a fixture whose display names are all distinct.

### A mutation that was a no-op, not a survivor

Widening the team branch's predicate to `($1 = '' OR teamid = $1)` looked like a mutation and is
not: that branch is entered only when `team_id` is non-empty, so the added disjunct is always
false. Replaced with one that drops the predicate outright — which the other team's planted hook
can see. Distinguishing the two is the difference between a finding about the tests and a wasted
one.

## `GET /api/v4/hooks/outgoing` — `getOutgoingHooks` (2026-09-06)

Served. Extends `crates/mm-api/src/webhooks.rs`, `crates/mm-app/src/webhook.rs` and
`crates/mm-store/src/webhook_store.rs`, plus `Config::enable_outgoing_webhooks`; 9 parity tests in
`crates/mm-api/tests/parity/outgoing_hooks.rs` and 1 store test in
`crates/mm-store/tests/db_webhook_owner_filter.rs`.

**The one thing a reader would otherwise get wrong: there are *three* scopes and `channel_id`
wins.** `channel_id` is checked first, then `team_id`, then neither, and the branches are
exclusive — a request carrying both is a **channel** request and the team is ignored entirely.
Each branch asks the same permission pair at a different scope (`…ToChannel`, `…ToTeam`, plain).
The suite's team admin is the only caller that answers differently to all three.

Two more. The owner column is **`CreatorId`** here and `UserId` on the incoming table
(webhook_store.go:295 versus :188) — the same predicate off different columns. And
`trigger_words`/`callback_urls` are `model.StringArray` stored as JSON inside a `varchar`:
`StringArray.Scan` leaves the field **nil** for a SQL NULL, so `null`, `[]` and a populated array
are three distinct answers. The fixture plants one hook of each.

Go's error ids are not symmetric and one is a copy-paste: the team function reports
`app.webhooks.get_outgoing_by_team.app_error` while both the channel function **and the unscoped
list** report `…get_outgoing_by_channel.app_error` (webhook.go:827) — the whole-server list naming
a channel it never had. Reproduced, with a unit test so that "fixing" it fails.

Mutation run: **20 run, 18 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/outgoing-hooks.plan`).

### The owner filter cannot be reached over HTTP, on either route

Both list routes narrow to one owner unless the caller holds `manage_others_*_webhooks` — and on
a stock server the only roles granting `manage_own_*` are `system_admin` and `team_admin`, **both
of which also grant `manage_others_*`**. So every caller that can reach either route arrives with
the filter already cleared, and a mutation swapping the outgoing table's `CreatorId` for another
column survived the entire `api` suite.

The branch is not unreachable in principle — a custom scheme could grant one and not the other —
so it is tested where it *is* reachable: `tests/db_webhook_owner_filter.rs` calls the store
directly with an owner id, for both tables. This is the second route this session where a branch
had to be tested below the edge rather than dropped ([`getUserAudits`]'s 1000-row bound was the
first).

### `MUTATE_FILTER` filters test *names*, not targets

`MUTATE_FILTER=db_webhook_owner_filter` — the name of a file in `tests/` — matches no test
function, so cargo runs **zero** tests, exits 0, and the mutation is reported SURVIVED. Two were,
before this was noticed. `scripts/mutate.sh` now says so at the top, and the rule is: a CAUGHT line
must name a test, and a SURVIVED line on a mutation you predicted should be checked against the
filter before it is believed.

### And the users-list etag stopped settling, which is [D-167] arriving

`the_etag_arms_match_go_except_for_gos_two_pointer_components` failed the first full run after
this session's three route suites landed — *"Go's etag never settled"*. That etag is
`MAX(UpdateAt)` over **every** user outside the team, and the three suites between them create
seven plain users, promote two of them and log one in three times, so `MAX(UpdateAt)` moves for as
long as any suite in the binary is still building fixtures. The message is a statement about write
pressure, not about the port, and it blocked every full run.

It now **brackets** instead of waiting for quiet — the rule `fetch_both_stable` already uses, and
[D-167]'s own prescription: a correct port answers an etag Go also answered at some instant inside
the window, so it matches one of the two reads; a wrong one matches neither however busy the table
is. Quiescence is kept as a third acceptance.

## `GET /api/v4/hooks/{incoming,outgoing}/{hook_id}` — `getIncomingHook`, `getOutgoingHook` (2026-09-06)

Served. Extends `crates/mm-api/src/webhooks.rs`, `crates/mm-app/src/webhook.rs` and
`crates/mm-store/src/webhook_store.rs`; 9 parity tests in
`crates/mm-api/tests/parity/single_hooks.rs`. The integration **edit** screen, where the list page
sends you.

**The one thing a reader would otherwise get wrong: the two routes disagree about the channel.**
`getIncomingHook` looks the hook's channel up and refuses a caller who cannot read it;
`getOutgoingHook` never looks at a channel at all. So a team admin who is not a member of a
private channel is refused the incoming hook in it and **served the outgoing hook in the same
channel**. That is Go's asymmetry, and the suite pins it in one test.

Three more. A soft-deleted hook is a **404** — `DeleteAt = 0` is in the single-row `WHERE`, so it
is not found rather than returned with `delete_at` set, and "deleted" is indistinguishable from
"never existed". The app layer's 404 and 500 **share one id** and differ only in status
(webhook.go:632, :634). And both single reads use `json.NewEncoder(w).Encode`, so they carry a
**trailing newline** that the two list routes in the same file do not.

Ids in the fixture carry twelve digits of the clock: `GetIncoming(id, true)` is the only webhook
read Go serves from a cache (localcachelayer/webhook_layer.go:41), held for thirty minutes with no
invalidation a direct write can reach.

Mutation run: **15 run, 13 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/single-hooks.plan`).

### Two gates that look alike, and only one of them is reachable

Gate two is the team permission (plus the channel, on the incoming route); gate three is "you may
read a hook you did not create only with `manage_others_*`". Four mutations survived the first run
because both gates produce **the same 403 with the same body** — the permission name lives in
`detailed_error`, which `handleContextError` wipes.

- **Gate three is unreachable over HTTP entirely**: the only roles granting `manage_own_*` also
  grant `manage_others_*`, so nobody who passes gate two can fail gate three. It is now a named
  function, `refused_for_ownership`, shared by both handlers and tested as a truth table. Naming
  the rule once instead of writing it twice is the better code anyway.
- **Gate two is reachable, but only against a hook the caller owns** — otherwise gate three
  refuses the same caller identically. The fixture now plants an incoming *and* an outgoing hook
  owned by the plain user, and the outgoing one is what caught the last survivor.

### A `DELETE` in a forwarding test is a real `DELETE`

`other_methods_are_forwarded` pointed at the fixture's own hook, and the forward did what it says:
Go soft-deleted it, so whichever test ran next found a 404 where it expected a 200. It now uses
the fixture's deliberately-absent id. The forward is what is under test; the deletion was
collateral.

### The `MUTATE_FILTER` trap, twice in one session

Having documented in the previous route that `MUTATE_FILTER` matches **test names**, three
mutations here were then filtered on `refused_for_ownership` — a *function* name — and all three
were reported SURVIVED against a run of zero tests. The rule is worth restating as a check rather
than a fact: **a SURVIVED verdict on a mutation you predicted would be caught is a claim about the
filter until the CAUGHT lines around it name real tests.**

## `GET /api/v4/channels/{channel_id}/common_teams` — `getDirectOrGroupMessageMembersCommonTeams` (2026-09-06)

Served, except for one branch. `crates/mm-api/src/common_teams.rs`,
`crates/mm-app/src/common_teams.rs`, two new queries in `crates/mm-store/src/team_store.rs`; 10
parity tests in `crates/mm-api/tests/parity/common_teams.rs` and 1 store test in
`crates/mm-store/tests/db_team_get_many.rs`. The webapp asks it before offering to convert a group
message into a channel.

**The one thing a reader would otherwise get wrong: a bot member makes this route unanswerable.**
Go skips a bot when `IsBotExemptFromDMRestrictions` says so, and that function's last test reads
`pluginsEnvironment.Available()` — the plugin manifests **loaded in the running server's memory**.
A plugin-owned bot is exempt in Go and unknowable here, so a channel with an active bot member is
forwarded. The two earlier branches (the system bot by username, a bot the caller owns) are
portable and deliberately **not** implemented alone: answering two thirds of a rule is how a port
gets a wrong answer confidently.

Three more. A channel that does not exist is a **403**, not a 404 — nothing looks it up before the
permission check, and the check answers false when it cannot fetch it, which is the same answer a
stranger gets and the reason this route cannot be used to probe for DMs. The **guest gate runs
first**, before any permission question, and its id names group-message *conversion*
(`api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error`) on a route that only
reads. And there are **three empty answers**: `[]` for no common team, `null` for a caller who is
not an active member (Go's nil slice, channel.go:4275), and the forward.

Mutation run: **20 run, 18 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/common-teams.plan`).

### Five survivors, and each named a row the fixture did not have

- a **left** team membership (`TeamMembers.DeleteAt != 0`),
- an **archived** team (`Teams.DeleteAt != 0`) that everyone is in,
- a **deactivated** channel member,
- a caller the team sanitiser actually strips fields for — the fixture admin has `manage_system`,
  so nothing was stripped and dropping `SanitizeTeams` was invisible,
- and `GetMany`'s two unreachable behaviours: it has **no `DeleteAt` predicate**, and it turns an
  empty result into a typed not-found the app layer answers **404** to. Its only migrated caller
  never passes an empty list and never passes an archived team's id, so both are tested at the
  store instead.

The guest branch is reachable only in one order: **log in first, then promote to `system_guest`.**
Guest accounts are disabled on this server so a guest cannot log in, but an existing session
survives the role change and `IsGuest()` reads the user row, not the session.

`createGroupChannel` puts the **requesting user** in the channel, so the "not a member" fixture had
to be created with another user's token — listing three other users is not enough to stay out of
it.

### A mutation that could not be written in SQL

Forcing the intersection into a union by rewriting `HAVING COUNT(...) = $2` changes the type sqlx
infers for the column, and the mutated source stops compiling — a harness fault, not a verdict.
The plan mutates the **bound value** instead (`1` rather than the id count), which is the same
semantic change with the query untouched. Worth remembering for any `query_as!` whose shape a
mutation would disturb.

### A store test's fixture took another suite's module down

`db_team_get_many.rs` planted two teams sharing the display name "mmrs get many".
`parity/teams_all.rs` reads the whole `Teams` table and **refuses to run** when two rows share a
display name, because `ORDER BY DisplayName` has no tiebreak — so all sixteen of its tests failed,
fifteen of them on the `OnceCell` retry rather than on the real cause. `cargo test --workspace`
runs the store tests and the parity binary against the same database at the same time; a store
fixture is not private just because it lives in `mm-store`.

The rule this adds to the ones already recorded: **a planted row must be unique in whatever column
some other suite sorts by.** Distinct ids are not enough.

## `GET /api/v4/teams/{team_id}/channels/recommended` — `getRecommendedChannelsForTeam` (2026-09-06)

Served, for an unlicensed installation. `crates/mm-api/src/channels.rs`
(`get_recommended_channels_for_team`); 7 parity tests in
`crates/mm-api/tests/parity/recommended_channels.rs`. The browse-channels modal asks it every time
it opens.

**The one thing a reader would otherwise get wrong: the whole reachable answer is three bytes.**
`GetRecommendedPublicChannelsForUser` (app/channel.go:4620) returns `model.ChannelList{}` before
doing anything unless the licence is **Enterprise Advanced** *and*
`AccessControlSettings.EnableAttributeBasedAccessControl` is on. Neither holds here, so the
attribute-based scan below that gate is unreachable and the answer is `[]` — for a member, for an
admin, for a team with a hundred channels. A licensed installation is forwarded, the same boundary
`getClientLicense` draws.

That makes the **permission check** the only thing this route decides: `list_team_channels` on
that team, checked before the licence gate, so a non-member gets a 403 where a member gets `[]`.
And the empty answer is `[]`, never `null` — a composite literal, not a nil slice, which is the
same distinction [`getUserAudits`] resolves the other way.

Mutation run: **10 run, 8 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/recommended-channels.plan`).

### The licence-row lock moved into `common`

Removing the forward survived the first run: on an unlicensed server the branch never fires.
`license_client` already had the answer — plant a valid `Systems.ActiveLicenseId` and assert
`x-mmrs-served-by: go` — but its `RwLock` was module-private, and two suites writing one global
row need one lock between them. It now lives in `common::ACTIVE_LICENCE_ROW`, with
`common::set_active_licence_id`, and both suites take it: shared while they expect us to answer,
exclusive while they make the server look licensed.

## `GET /api/v4/oauth/apps`, `.../{app_id}`, `.../{app_id}/info` — the three OAuth app reads (2026-09-06)

Served. `crates/mm-api/src/oauth.rs`, `crates/mm-app/src/oauth.rs`,
`crates/mm-store/src/oauth_store.rs`, plus `Config::enable_oauth_service_provider`; 8 parity tests
in `crates/mm-api/tests/parity/oauth_apps.rs` and 1 store test in
`crates/mm-store/tests/db_oauth_apps_by_creator.rs`. The System Console's *Integrations → OAuth
2.0 Applications* page, and the consent screen behind `/info`.

**The one thing a reader would otherwise get wrong: `client_secret` is on the wire for two of the
three.** Only `/info` calls `Sanitize()` (model/oauth.go:164), which blanks that one field and
nothing else. The list hands every app's secret to anyone with `manage_oauth`, and the single read
hands it to the creator or a system-wide admin — which is what the console page needs, and what a
"safe-looking" tidy-up would break. Both directions are asserted.

Three more. The **list's refusal is not a permission error**: Go builds
`api.command.admin_only.app_error` by hand (oauth.go:147), so a *command* id refuses an OAuth
route, while the single read beside it uses `api.context.permissions.app_error` for the same
missing permission. The single read's not-found and failure ids differ by one word
(`get_app.find` / `get_app.finding`) — unlike the webhook single reads, which share one id and
change only the status. And `/info` has **no permission check at all**: any session may read any
app's public description.

`GetApps` has no `ORDER BY` and the table has no `DeleteAt` column, so paging is over the heap and
there is nothing to filter.

Mutation run: **20 run, 18 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/oauth-apps.plan`).

### Three survivors, all of them shapes seen earlier this session

- **`page * per_page` tested at `per_page = 1`**, where it equals `page`. Now two pages of two over
  three rows, asserting the pages are **disjoint** — the union is the same either way.
- **The single read's first gate**, invisible because its 403 and the second gate's are the same
  bytes. It is now tested against an app the caller **created**: only then does the second gate
  pass, so only then is the first the one refusing. The same trick the single-hook routes needed.
- **`GetAppByUser` is unreachable over HTTP**: it is selected only for a caller with `manage_oauth`
  and not `manage_system_wide_oauth`, and the one role granting either grants both. Tested at the
  store, where the *unconditional* creator predicate — Go adds it with a plain `Where`, unlike the
  webhook stores' guarded ones — makes an empty id match nothing rather than everything.

### `sqlx::query_as!` cannot share a column list

The three queries repeat thirteen columns. A `macro_rules!` expresses that, and breaks compile-time
checking: `query_as!` needs a string **literal**, so a macro-assembled query is not verified
against the database at all. The repetition is deliberate — checked repetition beats reuse the
compiler cannot see.

### The purge has to precede *every* fixture write, not most of them

The full run failed once in `channel_posts_unread` with *"No team member found for that user ID
and team ID"* — a suite that had never touched OAuth, failing to add a user to a channel. The
cause was ordering: `purge_api_fixtures` is a `OnceCell`, and the **first** caller runs it. A suite
that created a team without purging first, and only later triggered the purge through some other
suite's helper, had its own team deleted out from under it.

The window widened on 2026-09-06 when `create_plain_user` started awaiting the purge (it had to —
see the first commit of the day). The fix is to close it in the same direction: `create_team` and
`create_channel_typed` await it too, so the purge is guaranteed to be the earliest write of the
run. **Every path that creates a fixture purges first**, and the `OnceCell` makes that once.

## `GET /api/v4/users/{user_id}/oauth/apps/authorized` — `getAuthorizedOAuthApps` (2026-09-06)

Served. Extends `crates/mm-api/src/oauth.rs`, `crates/mm-app/src/oauth.rs` and
`crates/mm-store/src/oauth_store.rs`; 7 parity tests in
`crates/mm-api/tests/parity/authorized_oauth_apps.rs`. The *Security → OAuth 2.0 Applications*
panel in a user's own settings.

**The one thing a reader would otherwise get wrong: the join ignores the preference category.**
`InnerJoin("Preferences AS p ON p.Name = o.Id AND p.UserId = ?")` (oauth_store.go:151) and nothing
else. Authorizing an app writes a preference in the `oauth_app` category, but the query never says
so — **any** preference row whose `Name` equals an app id authorises that app for that user. The
fixture plants one in a different category to pin it. Narrowing the join to the category is the
obvious fix, it is a security *improvement*, and it would answer differently from the server we
forward to.

Two more. **This list is sanitised and the admin list is not** — `GetAuthorizedAppsForUser` blanks
`client_secret` on every app (app/oauth.go:642), so the same rows come back with the secret through
`GET /api/v4/oauth/apps` and without it here. Of the four OAuth reads, two sanitise and two do not,
and the two that do sanitise in **different layers** (the app layer here, the handler for `/info`);
each is ported where Go put it. And the gate is `SessionHasPermissionToUser`, not `manage_oauth` —
its refusal names `edit_other_users`, a write permission on a read, as `getUserAudits` does.

`json.Marshal` + `w.Write`, so no trailing newline.

Mutation run: **14 run, 12 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/authorized-oauth-apps.plan`).

### Two suites, one table

This suite plants apps under `mmrsauthzd%` because `parity/oauth_apps.rs` purges `mmrsoauth%` on
the way in — two suites sharing a prefix would delete each other's rows mid-run. And because
`getOAuthApps` has **no filter at all**, that suite's pagination test could no longer assert its
pages *equal* the whole table; it asserts they are disjoint and drawn from it. Disjointness is what
catches an offset of `page` rather than `page * per_page`; the union never could.

## `GET /api/v4/terms_of_service` — `getLatestTermsOfService` (2026-09-06)

Served. `crates/mm-api/src/terms_of_service.rs`, `crates/mm-app/src/terms_of_service.rs`,
`crates/mm-store/src/terms_of_service_store.rs`; 4 parity tests in
`crates/mm-api/tests/parity/terms_of_service.rs`. The webapp asks for this at login when custom
terms of service are switched on.

**The one thing a reader would otherwise get wrong: the not-found and failure ids do not resemble
each other.** `app.terms_of_service.get.no_rows.app_error` at 404 and
`app.terms_of_service.get.app_error` at 500. That is a **third** convention in one tree: the
webhook single reads share one id and change only the status, the OAuth single read has two ids one
word apart, and this one has two that are plainly different. Each is reproduced as found.

Two more. A session is required and **nothing else** — no permission, because the terms are what a
user must read before they can use the server. And `ORDER BY CreateAt DESC LIMIT 1` has **no
tiebreak**, so two revisions published in the same millisecond have no defined order on either
server.

Mutation run: **9 run, 7 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/terms-of-service.plan`).

### The fixture cannot be torn down, because Go caches the success

Go caches the answer under the key `"latest"` (localcachelayer/terms_of_service_layer.go:47) and
invalidates it only on `Save` — which is licence-gated here, so the table can only be written by
hand and a direct write is invisible to that cache for as long as it holds a row. The suite
therefore plants **fixed ids, fixed timestamps and fixed text** with `ON CONFLICT DO NOTHING`:
every run writes the same two rows, so whatever Go cached earlier is byte-identical to what the
table holds. *Changing the fixture text will make one run disagree*, and the fix is to wait the
cache out.

Note the miss is **not** cached (`if allowFromCache && err == nil`), so an empty table is a live
query on both servers — it is the success that is sticky.

### Two mutations dropped rather than carried

- **The store's "empty table is not-found"** needs an empty table, and this suite exists to keep two
  rows in it; emptying it even briefly would leave Go serving a row that no longer exists. There is
  no arrangement in which the branch is both reachable and safe. The app layer's mapping of that
  error to a 404 is unit-tested; what is untested is one `ok_or_else`.
- **The session requirement** is enforced by the extractor's *type*, so the only mutation of it is
  one that does not compile. There is nothing for a test to catch that the type system does not
  already refuse.

## `GET /channels/{id}/moderations`, `/bookmarks`, `/member_counts_by_group` — three licence gates (2026-09-06)

Served, for an unlicensed installation. `crates/mm-api/src/channels.rs`
(`get_channel_moderations`, `list_channel_bookmarks`, `get_channel_member_counts_by_group`,
`licence_gate`); 6 parity tests in `crates/mm-api/tests/parity/licence_gated_channels.rs`. All
three are real client routes on a licensed server — the System Console's moderation panel, the
channel bookmark bar, and the group-membership counts a group-constrained channel shows.

**The one thing a reader would otherwise get wrong: the licence check is the *first* statement in
each handler**, ahead of `RequireChannelId` and ahead of every permission question
(channel.go:2973, channel_bookmark.go:447). So `/channels/abc/moderations` — an id far too short
to be valid — answers the **licence** error rather than a 400, and a plain user who cannot read
the channel gets the licence error rather than a permission one. On moderations that is two
different 403s, and the ordering decides which.

The errors are **not the same shape**: a `403` for moderations and for the group counts, a `501`
for bookmarks — two statuses and three ids between them, one calling itself "not permitted" and
another "not implemented", twenty files apart.

A licensed installation is forwarded — everything behind the gates (scheme-derived moderations,
the bookmark store, the group-membership tables) is unported. The decision lives in one
`licence_gate` helper rather than three copies.

Mutation run: **10 run, 8 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/licence-gated-channels.plan`).

### A segment outside the mux charset is refused *before* the licence gate

`/channels/not-an-id/bookmarks` is Go's own 404, because gorilla never routes it — the handler,
and therefore the licence check, never runs. Our charset middleware forwards it for the same
reason, so the two agree by construction rather than by copying the gate. Asserted, because "the
licence error comes first" is true only *within* the handler.

## `GET /api/v4/limits/server` — `getServerLimits` (2026-09-06)

Served, for an unlicensed installation. `crates/mm-api/src/limits.rs`,
`crates/mm-app/src/limits.rs`; 5 parity tests in `crates/mm-api/tests/parity/server_limits.rs`.
`loadMe()` fans out to this on **every** login and config refresh, which is why Go goes to the
trouble of skipping the count queries for non-admins.

**The one thing a reader would otherwise get wrong: the non-admin answer is built by the
*handler*, not the app layer.** `App.GetServerLimits(false)` still returns the seat limits and only
skips the counts (app/limits.go:59); the handler then throws even those away and rebuilds a
`ServerLimits` with five explicit zeros, keeping the two post-history fields (limits.go:32-43). The
same call answers differently depending on who asked, and the difference is applied twice in two
places. Both are ported where Go put them.

Two more. **There is no refusal** — every session gets a 200, and a non-admin's answer is all
zeros. And "admin" is an **and of two system-scoped permissions**, `manage_system` *and*
`sysconsole_read_user_management_users`, not a role check.

On the unlicensed path the answer is two constants (200 and 250, hard-coded in `app/limits.go`),
one `COUNT(*)`, and five zeros — every one of the five is licence-derived, and
`shouldTrackSingleChannelGuests` returns false the moment the licence is nil, so the guest scan
never runs and `activeUserCount` is the *unadjusted* count. A licensed installation is forwarded.

Mutation run: **10 run, 8 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/server-limits.plan`).

### Two survivors, one fixed and one measured away

- **The `&&` of the two permissions** has no reachable branch: the only role granting
  `manage_system` also grants the sysconsole read. It is now `counts_are_visible_to`, a named
  function with a truth table, tested where it can be — the fourth time this session that a rule
  had to move below the edge to be testable.
- **`include_deleted`** could not be distinguished, because the development database holds **zero**
  deleted users — measured, not assumed — and `DELETE /users/{id}` through the API left no
  `DeleteAt != 0` row behind when the fixture tried to make one. Making one means writing a `Users`
  row by hand, and `parity/roles.rs` records what a hand-written row missing a column Go scans into
  a non-pointer field once did to the whole stack. The mutation was dropped with that reason rather
  than carried.

## `GET /api/v4/groups` and `/api/v4/users/{user_id}/groups` — a fourth licence gate (2026-09-06)

Served, for an unlicensed installation. `crates/mm-api/src/groups.rs`; 7 parity tests in
`crates/mm-api/tests/parity/groups.rs`. The System Console's *User Management → Groups* page and
the group list on a user's profile.

**The one thing a reader would otherwise get wrong: this gate is the *generic* one.**
`requireLicense` (api4/handlers.go:237) returns `api.license_error` at **501** with a blank
`where`, shared by every group route — where the three channel gates each have an id of their own
and two of them answer **403**. Four gates in this session, four conventions; each is asserted
rather than assumed to match its neighbour.

It is the first statement in both handlers, so `/users/abc/groups` and a request about *someone
else's* groups both get the licence error rather than the 400 and the 403 that a licensed server
would give. Query parameters are never read.

Mutation run: **5 run, 3 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/groups.plan`).

A mutation of the **blank `where`** was written and then dropped: `AppError.Where` carries
`json:"-"` in Go and `#[serde(skip)]` here, so no parity test can see it and it survived exactly as
it had to. That Go leaves it blank on purpose, where every other error in the tree names its
caller, is recorded in the module doc — which is where someone changing the line will look.

### And a check the harness did not have: `scripts/preflight-plans.sh`

Every mutation plan is committed so a later session can re-run it. Nothing checked that they still
*apply*. Running the same pre-flight `mutate-batch.sh` does, over all 42 plans at once, reports
**42 stale anchors out of 700 lines** — some old, several introduced today, because a route that
adds a handler to a file an earlier plan anchors on can turn a unique pattern into an ambiguous
one. One was fixed and its plan re-run to the same tally, as a worked example; 41 remain. `mutate.sh` replaces the **first** occurrence, so an ambiguous anchor silently moves the
verdict to a function nobody meant to test. [D-168] records the backlog; the tallies reported this
session are unaffected, because each plan passed its own pre-flight at the moment it ran.

The first version of this checker unescaped `\n` in Python and reported **77** problems — it
disagreed with the runner on every pattern containing an escaped backslash. `mutate.sh`'s own
header already says why that is worthless: *a validator that decodes differently from the runner is
not a validator.* The committed script uses `printf %b`, as the runner does.

## Configuration gets a shared source of truth (2026-09-06)

Not a route. The project owner took three standing decisions — port config properly, reproduce
Go's lenient JSON decoding, and close the session-activity pair — and this session is the first of
them. New: `crates/mm-store/src/config_store.rs`, `crates/mm-store/tests/db_config_active.rs`,
`crates/mm-api/tests/parity/config_source.rs`, `scripts/dump-config-fixture.sh`,
`scripts/mutations/config-source.plan`, `fixtures/config_active.json`. Changed:
`crates/mm-app/src/config.rs`, `crates/mm-api/src/lib.rs`, `crates/mm-api/src/main.rs`,
`docker-compose.yml`.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| config/database.go (`Load`) | `mm-store/src/config_store.rs` | PARTIAL | 5 DB | The read half. `WHERE active` is not a tidy spelling of `active = true`: Go deactivates by setting `Active = NULL` (database.go:199) and leans on a UNIQUE constraint, so a widened predicate returns superseded revisions. |
| config/store.go (`Load` layering) | `mm-app/src/config.rs` | PARTIAL | 26 pass | Document then environment, in Go's order. Fifteen settings, grown by named reader; the document already holds all 47 sections, so each new field is one line plus an assertion. |

**The decision that shaped it: `MM_CONFIG` now points at the shared Postgres.** Go's default
backing store is `config.FileStore` over a Docker volume this process cannot see, which is why
[D-156] existed at all — the two servers shared a database but not a configuration, and every
ported permission gate that consults a setting was reading an assumption. Pointing `MM_CONFIG` at
the shared DSN makes Go select `config.DatabaseStore` (store.go:91) and keep the whole
`model.Config` as one JSON document in `Configurations.Value`. That closes [D-156] and [D-085]
outright rather than accepting either.

### Three facts about the document, all measured rather than read

1. **It is the config Go persists, not the one it runs on.** `Store.Load` builds two configs and
   writes back the one *without* the environment applied (`s.backingStore.Set(loadedCfgNoEnv)`,
   store.go:321). The live row says `ServiceSettings.SiteURL == ""` while the server beside it runs
   on `MM_SERVICESETTINGS_SITEURL=http://localhost:8065`. So a reader that stops at the document
   disagrees with the running server on exactly the settings someone bothered to change — the
   overlay is mandatory, and `parity/config_source.rs` pins the difference against both servers
   using the unauthenticated `/api/v4/config/client`.
2. **`FeatureFlags` is not in the document at all** — the section is cleared before persisting when
   `readOnlyFF` is set, which is the default (store.go:306-310). A flag can only come from the
   environment. This is why [D-153] is **not** unblocked by any of this, which was the outcome
   worth knowing.
3. **Absent means Go's *default*, not the zero value.** Every setting in config.go is a pointer and
   `SetDefaults` fills the nil ones, so eight of the fifteen modelled settings default to `true`. A
   `#[serde(default)]` — the tidier spelling — would have read an empty document as eight features
   switched off.

### The document is its own oracle, and it caught nothing, which is the good outcome

Thirteen defaults had been transcribed by hand from a 5,795-line Go file and asserted against line
numbers a human read — which catches a typo in the test and nothing in the world.
`every_default_matches_what_go_actually_wrote` now asserts all fifteen against what a Go server
wrote after running `SetDefaults` itself. Every one agreed. That is the first evidence the
transcription was right rather than merely self-consistent.

### Two survivors that physical row order explains

18 mutations run, 14 caught, 2 no-op controls survived — plus **2 genuine survivors**, both
mutations of the store's `WHERE active`. The cause is not a weak assertion: the active row was
written at first boot and sits at `ctid (0,1)`, a seeded superseded row lands at `(0,6)`, and a
widened predicate's sequential scan therefore reaches the correct row first and `fetch_optional`
takes it. The wrong answer *is* reachable in production — a long-lived server accumulates revisions
and any reordering can put one first — so they stay in the plan rather than being deleted to tidy
the tally. Closing them needs the seeded row to physically precede a row that belongs to the Go
server, which this suite will not rewrite.

The first run of the plan was **void**: `MUTATE_FILTER=config_active` is a *file* name, matches no
test name, ran zero tests and reported two SURVIVEDs that meant nothing. That is the exact trap
`mutate.sh`'s own header documents, hit again.

### The overlay was untestable, and two mutations proved it

`Config::apply_env` read `std::env` directly. No `MM_` variable is set in a test process, and
`std::env::set_var` races every other test in the binary — so the overlay could only ever be
exercised with nothing set, under which it is indistinguishable from doing nothing. Two mutations
that deleted it entirely survived. `apply_env_from` and `load_with_env` now take the lookup as a
parameter, and three tests drive a fake environment in both directions. Both mutations are caught.

### A rename broke five committed anchors, and they are repaired

`AppState`'s two privacy fields became accessors, so `state.show_full_name` gained parentheses in
25 call sites — and in five anchors across four plans, which `preflight-plans.sh` then reported as
0-match. Re-anchored mechanically. Stale anchors are back to **41 of 718**, the pre-existing
[D-168] level, rather than the 46 this change briefly caused. This is the first time the pre-flight
check has caught a regression it was written for.

### Not done, and owed

`.sqlx` is committed for offline builds and has not been regenerated since `910ad66`; the new query
validated against the live database instead. `sqlx-cli` is not installed here, so it stays stale —
pre-existing, and not something this session should install a toolchain to fix.

## The session-activity pair — [D-084] and [D-088] close together (2026-09-06)

Not a route: the second of the project owner's three standing decisions, and the half of session
handling that only makes sense as one change. New: `crates/mm-api/tests/parity/session_activity.rs`,
`crates/mm-store/tests/db_session_activity.rs`, `scripts/mutations/session-activity.plan`.
Changed: `crates/mm-store/src/session_store.rs`, `crates/mm-app/src/session.rs`,
`crates/mm-app/src/config.rs`, `crates/mm-api/src/auth.rs`, `crates/mm-api/src/users.rs`,
`scripts/dump-config-fixture.sh`, `fixtures/config_active.json`.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| platform/status.go (`UpdateLastActivityAtIfNeeded`) | `mm-app/src/session.rs` | DONE | 4 unit, 4 parity | The "if needed" is a five-minute throttle on `model.SessionActivityTimeout`, not a cache lookup — [D-084]'s own guess. Called on `getUser` and `getUsers` and deliberately **not** on `getUserByUsername`, which shares the whole rest of its tail. |
| app/session.go (`GetSession` idle branch) | `mm-app/src/session.rs` | DONE | 13 unit, 4 parity | Four exemptions, each asserted alone: a conjunction passes with three of them dropped. |
| store/sqlstore/session_store.go (`UpdateLastActivityAt`, `Remove`) | `mm-store/src/session_store.rs` | DONE | 4 DB | The first writes this store makes. `UPDATE` matches `Id` **only** where `Get` and `Remove` take an id *or* a token — and an `UPDATE` matching nothing succeeds, so getting it wrong is silent. |
| config.go (`SessionIdleTimeoutInMinutes`, `ExtendSessionLengthWithActivity`) | `mm-app/src/config.rs` | PARTIAL | 8 pass | Seventeen settings now. One of these has no constant default — see below. |

**`ExtendSessionLengthWithActivity` has no constant default, and that is the finding.** Go writes
`new(!isUpdate)` (config.go:729) where `isUpdate` is `ServiceSettings.SiteURL != nil`
(config.go:4289). `Store.Load` plants a `SiteURL` of `""` before calling `SetDefaults` when the
document has none (store.go:280), so **every document a running server persists is an update** and
the value is `false` — while a fresh config defaults it `true`. Since `true` disarms the
idle-timeout check outright, resolving this default the way every neighbouring field resolves
would have silently switched off the thing this session ported. The live row confirms it: `SiteURL
= ""`, `ExtendSessionLengthWithActivity = false`, `SessionIdleTimeoutInMinutes = 43200`.

This also broke `every_default_matches_what_go_actually_wrote`, which had been asserting
`from_document(fixture) == Config::default()`. Both values are correct for their input; the test
now compares against the adjusted default and a second test pins the rule in both directions.

### The parity suite found a divergence on every migrated route

Nothing had ever compared a **401 body** against Go's. The idle timeout needed one, and it failed
on its first run:

```
go:   "id": "api.context.session_expired.app_error"
ours: "id": "api.context.invalid_token.error"
```

`App::GetSession`'s error id never reaches a client. `handlers.go:277-280` keeps a 500 and
replaces every other failure with the generic `session_expired` — so a wrong token, an expired
session, a session id used as a token and a session revoked for idleness are one indistinguishable
answer, which is deliberate: none of them tells a caller whether the credential exists. We were
returning the inner id, on every route that takes a session. Fixed in `auth.rs`, and
`an_unknown_token_gets_the_same_refusal_as_an_idle_one` is the regression test.

The same branch also calls `RemoveSessionCookie`, which we still do not — [D-169], left open
because it needs `SiteURL` as a *setting* and a port of `GetSubpathFromConfig`.

### Two clocks became parameters, and that is what made the boundaries testable

`session_is_idle_past_timeout(config, session, now)` and `activity_write_is_due(now, last)` take
the time rather than reading `get_millis()`. With the clock inlined, "idle by exactly the timeout"
cannot be constructed — every fixture is already a few milliseconds past it by the time the
comparison runs — so `>` and `>=` are the same function and a mutation of one into the other
survives. Both boundaries are now asserted at the millisecond, and both mutations are caught. Same
shape as `apply_env_from`'s lookup parameter in the config session.

### Planting a session is the vertical slice in reverse

Go caches sessions **by token** (platform/session.go:50), so a row this suite edits behind its back
is invisible to a Go server that has already seen that token. Every assertion therefore plants a
session with a fresh, never-seen token: Go's first request with it is a guaranteed cache miss. A
row *neither* server minted authenticates against both, which is only true because they share one
`Sessions` table — the same fact the vertical slice proved, running the other way.

The shared `go_minted_token` session is never used here: one of these tests deliberately gets a
session revoked, and revoking the suite-wide credential would take every other file down with it.

### A prefix purge is a race, not a cleanup

The first run failed with `LastActivityAt` reading back as `None` — nothing to do with the port.
The parity tests share one binary and run concurrently, and each test was ending with a
`DELETE ... WHERE id LIKE 'mmrssessactv%'` sweep that deleted the sessions its neighbours were
midway through asserting on. Purging by exact token fixed it. [D-160]'s class, self-inflicted.

Mutation run: **27 run, 25 caught, 2 controls survived, 0 harness faults** — no genuine
survivors, which is unusual enough to say plainly: the two clock parameters above are why, since
both boundary mutations would otherwise have been unkillable.
(`scripts/mutations/session-activity.plan`). One mutation was written and dropped before the run:
deleting the `WHERE` from `Remove`. It runs against the shared development database, so a
`DELETE FROM sessions` with no predicate would log out the Go server and every other suite's
fixture token mid-run; `remove_matches_either_the_id_or_the_token` covers the same decision by
asserting the *other* seeded session survives.

## The cookie half of the same branch — [D-169], closed the same day (2026-09-06)

Raised and paid off in one sitting, because it is the other statement in `handlers.go:277-280` and
leaving it open would have meant re-deriving the whole branch later. New:
`crates/mm-model/src/go_path.rs` (moved), `reference/dump/behaviour_subpath.go`,
`fixtures/behaviour_subpath.json`, `scripts/mutations/session-cookie.plan`. Changed:
`crates/mm-api/src/auth.rs`, `crates/mm-app/src/config.rs`,
`crates/mm-model/src/command_autocomplete.rs`, `crates/mm-model/src/lib.rs`,
`crates/mm-api/tests/parity/session_activity.rs`, `reference/dump/main.go`.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| web/context.go (`RemoveSessionCookie`) | `mm-api/src/auth.rs` | DONE | 6 unit, 2 parity | `MaxAge: -1` renders as `Max-Age=0`, and an empty `Path` is omitted rather than sent empty. |
| utils/subpath.go (`GetSubpathFromConfig`) | `mm-app/src/config.rs` | DONE | 3 go_parity | Four outcomes from three branches: `/` three ways, `""` on a parse failure. |
| net/http (`sanitizeCookiePath`) | `mm-api/src/auth.rs` | DONE | 1 unit | `0x20..0x7f` except `;`. A **space is valid** in a cookie path, which looks wrong and is not. |

**The error is thrown away at the call site, and that changes the answer.** `RemoveSessionCookie`
writes `subpath, _ := GetSubpathFromConfig(...)` (context.go:181), so a `SiteURL` Go cannot parse
gives `subpath == ""` and the header carries **no `Path` at all** — not `Path=/`. A port that
mapped the error onto the root would widen the cookie's scope on precisely the misconfiguration
where a narrow scope was the point. `Config::subpath` therefore returns a `String` rather than a
`Result`: there is no caller that could act on the error, and typing it would invite one to.

### The oracle is transcribed glue over Go's own ingredients

`channels/utils` imports goldmark, which is not in the generator's `go.sum`, so
`behaviour_subpath.go` cannot call `GetSubpathFromConfig` directly. Instead its eight lines are
transcribed and the two things that actually do the work — `url.Parse` and `path.Clean` — are Go's
own. Same arrangement `behaviour.go` uses for the unexported identifier regexes, with the same
standing rule: copy any upstream change character for character. Two invariants are asserted inside
the generator so a botched transcription fails there rather than downstream.

`go_path` moved out of `command_autocomplete.rs` into its own module when this became its second
caller. It sits beside `go_url` now, which is where a Go stdlib port with its own oracle belongs.

### Two mutations survived, and both were real gaps

- **The 500 arm of the session rejection was unreachable from any test.** Getting there needs the
  session store to fail, which no parity test can arrange against a healthy database — so a
  mutation that cleared the cookie on a database error survived the whole suite. The mapping is now
  `SessionRejection::for_get_session_error`, a named function over the error and a subpath closure,
  and both arms are unit-tested. The closure also means the config is not consulted on the branch
  that does not need it, and the test asserts that by panicking if it is.
- **Nothing had ever driven `MM_SERVICESETTINGS_SITEURL`** — the one variable the Go container
  beside us actually sets. With no test setting it, the document's value alone is indistinguishable
  from no overlay at all.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/session-cookie.plan`), after the two survivors above were closed rather than
recorded.

### An unrelated fixture drift, left alone

Re-running the generator rewrote `behaviour_scheduled_post.json` and
`behaviour_scheduled_post_recurrence.json`: `time.LoadLocation("america/new_york")` **fails** on
this machine where the committed fixture says it succeeds. That is this host's tzdata, not a Go
change and not this session's work, so both files were reverted rather than committed. Worth
knowing before the next generator run: the corpus is zone-dependent by design ([D-032]'s
neighbour), and a lowercase zone name is apparently loadable on some systems and not others.

## Go's lenient JSON key matching — [D-040], the third standing decision (2026-09-06)

New: `crates/mm-model/src/go_json.rs`, `reference/dump/behaviour_json_fold.go`,
`fixtures/behaviour_json_fold.json`, `scripts/mutations/json-fold.plan`. Changed:
`crates/mm-model/src/post.rs`, `crates/mm-model/src/message_attachment.rs`,
`crates/mm-model/src/integration_action.rs`, `crates/mm-model/src/lib.rs`,
`reference/dump/behaviour_post_attachments.go`, `fixtures/behaviour_post_attachments.json`,
`reference/dump/main.go`.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| encoding/json fold.go, decode.go:699 | `mm-model/src/go_json.rs` | DONE | 19 (3 go_parity) | Exact name first, then the fold. Keys are rewritten to their exact spellings; the derived `Deserialize` is untouched. |
| model/message_attachment.go, integration_action.go | five `GoFields` consts | DONE | 4 schema | Hand-maintained name lists, checked against the structs by a test that fails to compile when a field is added. |

**Go folds ASCII *up*, and the whole non-ASCII surface is two runes.** [D-040] expected this to
need `utils::go_to_lower` and a Unicode fold table. It does not: `foldName` upper-cases ASCII and
pushes everything else through `foldRune`, and a sweep of every scalar value — recorded in the
fixture, so a Unicode revision would fail the test rather than open a hole — finds exactly **two**
runes whose fold lands on an ASCII byte: U+017F LATIN SMALL LETTER LONG S → `S`, and U+212A KELVIN
SIGN → `K`. Every `json:` name in the tree is ASCII, so those two are the entire reachable set, and
a rune that folds to something non-ASCII can be passed through unchanged and still give the right
*answer*. The oracle proves both are live: `{"tſ":123}` populates `ts`, and `{"title_linK":"l"}`
populates `title_link`, on the real Go decoder.

### Two rules that look like one

`{"title":"exact","TiTlE":"folded"}` and `{"TiTlE":"folded","title":"exact"}` give **different**
answers in Go: it resolves each key as it reads it and the last assignment wins. So "exact beats
folded" is not a rule at all — it is a consequence of ordering. Two *folded* keys resolve the same
way, last-wins, which is why the exact-name set is captured **before** any rename rather than
tested with `contains_key` as renames land; the tidier spelling makes the second folded key lose
and a mutation of it is caught.

**The ordering itself is not ours to reproduce, and does not need to be.** `serde_json::Map` is a
`BTreeMap` — `preserve_order` is deliberately off, because Go marshals a `map[string]any` with
sorted keys and turning it on would change every props object we emit — so the author's order is
gone before the remap runs. It is gone on Go's side too: both servers read these props out of the
same `jsonb` column, and Postgres orders keys by (length, bytewise). Two keys that fold together
differ only by case and therefore have equal length, which is exactly where `jsonb`'s ordering and
`BTreeMap`'s coincide. The generator now records Go's answer for the **sorted** spelling of each
corpus document alongside the author's, and the parity test asserts against that one; comparing
against the author's order would be asserting a fact neither server can observe.

### The remap must run before the nil strip, and a corpus case says so

`strip_nil_elements` looks for the literal keys `actions` and `fields`, because `Vec<PostAction>`
cannot hold the nil that Go's `[]*PostAction` can. Run it first and a payload writing `Actions`
keeps its nil into the decode, which drops the whole attachment. Reversing the two lines survived
the entire suite until `case_insensitive_nil_action` was added.

### Schemas are hand-maintained, so they are checked

`every_schema_covers_its_struct` builds each type with an **explicit struct literal** — no
`..Default::default()` — so adding a field to `MessageAttachment` or `PostAction` fails to compile
there until someone updates the schema too. Three further tests assert no two names in a schema
fold together (so declaration order cannot decide anything), every name is ASCII (the precondition
the two-rune table rests on), and every nested key is also one of the schema's own names.

Mutation run: **18 run, 16 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/json-fold.plan`). The first run had a **harness fault** — a mutation that
produced uncompilable Rust — and one survivor, the step-ordering one above; both were fixed and the
plan re-run whole, since a harness fault voids the tally.

## Ten routes: system, usage, permissions — and a denominator that counts itself (2026-09-07)

New: `scripts/routes.py`, `crates/mm-api/src/{system,usage,permissions}.rs`,
`crates/mm-app/src/{system,usage,utils}.rs`, `reference/dump/behaviour_round_off.go`,
`fixtures/behaviour_round_off.json`, `scripts/mutations/system-usage.plan`,
`crates/mm-api/tests/parity/system_usage.rs`. Changed:
`crates/mm-store/src/{audit_store,post_store,file_info_store,team_store,lib}.rs`,
`crates/mm-app/src/{audit,config,lib}.rs`, `crates/mm-api/src/{audits,lib}.rs`,
`crates/mm-api/tests/common/mod.rs`, `crates/mm-api/tests/parity/{users_me,channel_timezones}.rs`,
`reference/dump/main.go`.

**The numerator and denominator are now derived, not typed.** `scripts/routes.py` resolves
gorilla's `BaseRoutes` table out of `api.go` — separately for `Init` and `InitLocal`, which
register onto different routers — and matches parentheses rather than lines, because four
registration shapes exist and half of them span lines. It lands on **764** route+method pairs, the
figure CLAUDE.md already names. Served: **110**, up from 100. 593 pairs are on the HTTP router and
171 on the local-mode socket; 483 HTTP pairs remain. `scripts/routes.py --todo` is the work queue.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| api4/system.go (`getSystemPing`) | `mm-api/src/system.rs` | DONE | 5 unit, 2 parity | Five boundaries forward: `get_server_status=true`, a non-empty `device_id`, a positive goroutine threshold, a licence, Elasticsearch. |
| api4/system.go (`getSupportedTimezones`) | `mm-api/src/system.rs` | DONE | 1 parity | A compile-time table on both servers, not the host's tzdata. |
| api4/system.go (`getAppliedSchemaMigrations`) | `mm-api/src/system.rs`, `mm-store/src/lib.rs` | DONE | 1 parity | `ORDER BY Version DESC`; the permission is *any* sysconsole read. |
| api4/system.go (`getOnboarding`) | `mm-app/src/system.rs` | DONE | 2 unit, 1 parity | A missing row is synthesised as the string `"false"` — never a 404, never a bool. |
| api4/system.go (`getAudits`) | `mm-api/src/audits.rs` | DONE | 2 parity | The same app call as `getUserAudits` with an **empty** user id, which the store reads as "no filter". |
| api4/usage.go (three counters) | `mm-api/src/usage.rs`, `mm-app/src/usage.rs` | DONE | 2 unit, 2 parity | No permission check on any of them. |
| api4/permission.go (`appendAncillaryPermissionsPost`) | `mm-api/src/permissions.rs` | DONE | 4 unit, 1 parity | `null`, `[]` and a malformed body are one 400. |
| api4/cluster.go (`getClusterStatus`) | `mm-api/src/system.rs`, `mm-app/src/system.rs` | DONE | 1 parity | `[]` and never `null`; licensed installations forward. |
| channels/utils (`RoundOffToZeroesResolution`) | `mm-app/src/utils.rs` | DONE | 3 go_parity | See below — the corpus found a real float divergence. |

### The empty user id was ported as "match nothing", and that was wrong

`SqlAuditStore.Get` drops its `WHERE` when the user id is empty, returning every user's rows. The
earlier port turned that into an unconditional predicate on the reasoning that nothing reachable
passed an empty id — true of the one route migrated at the time, false of the API. `getAudits`
passes one deliberately, so the old branch would have answered `GET /api/v4/audits` with `[]` on a
server holding thousands of audit rows: a wrong answer dressed as a safe one. Two literal queries
now, not one built at runtime, so `query_as!` still checks both.

### Go's `math.Log10` is not the platform's, and it is off by one at 10^15

`RoundOffToZeroesResolution` truncates `math.Log10(|n|)` to get a magnitude. Go implements `Log10`
in pure Go as `Log(x) * (1/Ln10)` over the FreeBSD `e_log.c` polynomial, so it is a **different
function** from C's `log10` — and the first run of the new corpus proved it: at
`n = 999_999_999_999_999` Rust's `f64::log10` returns exactly `15.0` where Go returns `14.999…`,
moving the answer from `900000000000000` to `0`. Going the other way, at `n = 10^15` **Go** is the
one that is off by one against the exact magnitude.

So neither float is trustworthy and they fail in opposite directions. The port computes the
magnitude exactly instead, and the 954-row corpus is what says that is safe rather than merely
tidier: every disagreement is at a magnitude of 15 or more, while the two real call sites pass
resolutions **3** and **8**, where `min(zeroes, resolution)` clamps to the resolution regardless.
A separate test pins *which* inputs disagree, so a change to Go's `Log10` that moved the
divergence down to 10^8 would fail loudly rather than cancel out.

### `/usage/posts` is the sharpest instance of [D-087] yet, and we are the correct one

Go passes `AllowFromCache: true` into a **size-1, thirty-minute** cache that nothing invalidates
when a post is written (`localcachelayer/layer.go:342`). Measured: Go answered `{"count":400}`
against a table holding 18 user posts, and answered `{"count":10}` — our bytes exactly — the
instant its caches were cleared. The parity test therefore calls
`common::invalidate_go_caches` first, because a byte comparison against that cache tests when Go
last looked rather than whether the query is right.

### Three writers across ten routes, and they are not grouped by file

`model.ToJSON` (ping), `json.Marshal` + `w.Write` (timezones, schema, cluster, all three usage
counters, ancillary) and `json.NewEncoder(w).Encode` (onboarding, audits). The first two write no
trailing newline and the third does — and `getOnboarding` sits within sixty lines of
`getAppliedSchemaMigrations` in the same Go file using the other one. The ping additionally
marshals a `map[string]any`, so its keys are **byte-sorted** and the lower-case `status` lands
last; a struct would have emitted it first.

### Two parity tests were using now-migrated routes as their "still forwarded" canary

`users_me::an_unmigrated_route_is_forwarded_to_go` pointed at `/system/ping` and
`channel_timezones` at `/system/timezones`. The first moved to
`GET /api/v4/config/client?format=old`, chosen for the same properties (no session, no near-term
migration); **move it again rather than deleting it** — it is the only assertion in the suite that
the proxy fallback still exists. The second became the stronger assertion the collision now
warrants: the global table and a channel's members' zones must be *different* answers.

### `POST /system/onboarding/complete` is deliberately still Go's

`CompleteOnboarding` installs marketplace plugins in goroutines and calls each plugin's `OnInstall`
hook. There is no plugin host here, so the GET is migrated and the POST on the same path falls
through `partially_migrated` to the proxy.

### The mutation run, and the four passes it took

**44 run, 42 caught, 2 controls survived, 0 harness faults** — reached over four passes, because the
first one found twelve survivors and every one of them was a real gap:

| Survivor | What the suite could not see |
|---|---|
| `audits-page-ignores-the-permission` | Every audit test used an **admin** token, so a deleted `read_audits` check looked identical to a present one — on a route that returns every user's IP addresses. |
| `posts-usage-uses-the-system-prefix-instead` | Every post here is either untyped or `system_*`, so `Type = ''` and `Type NOT LIKE 'system_%'` agree. Fixed by planting typed posts — **twenty-five of them**, because the route rounds and one post rounds away. |
| `round-off-posts-resolution-is-eight` | `min(zeroes, resolution)` makes 3 and 8 the same number below ten thousand posts. Fixed with a named `POSTS_RESOLUTION` and a unit test, which is the only thing that *can* see it. |
| three `teams-usage-*` and `teams-get-all-filters-deleted` | Nothing ever set `CloudLimitsArchived`, so the counter was zero however it was computed. Fixed by planting a deleted, archived team. |
| two `onboarding-*` | The stored value was already `"false"`, which is also what the missing-row branch synthesises. Fixed by planting `"true"` and then removing the row. |
| `migrations-permission-is-all-not-any` | An admin holds `manage_system` *and* every sysconsole read. Fixed with `system_read_only_admin` — 53 sysconsole reads, no `manage_system`. |
| `ancillary-output-is-deduplicated` | The inputs chosen had no ancillary permission in common. The table has exactly one overlapping pair; it is now the fixture. |

Two of those fixes were **tests reimplementing the code they tested** — `onboarding_row` and
`is_cloud_archived` each existed twice, once in the handler and once in the test module, so a
mutation of the real one left the copy passing. Both now live in production code.

### A no-op control came back CAUGHT, twice, and that was the suite's fault

`GET /api/v4/audits` is append-only and newest-first, so page 0 shifts every time **anything** in
this binary writes an audit row — and `getOnboarding` makes Go write one on every read. Byte-
comparing that page is not an oracle, it is a race: at twelve retry windows it failed 2 runs in 5,
and reported a control mutation of an unrelated SQL predicate as caught. It now issues both reads
with `join!` and compares the **contiguous run** the two pages share, since new rows only ever
prepend; six consecutive runs are clean where three of five were before. A control that fails means
the verdicts mean nothing — this one meant it twice.

### Two harness bugs, both fixed in `scripts/`

- **A plan line with an empty `to` silently truncates the run.** `read` with `IFS=$'\t'` treats a
  tab as IFS *whitespace*, so adjacent tabs collapse and every later field shifts left: `mutate.sh`
  is handed the suite name as its replacement text. The run stops with no tally, having reported
  verdicts only for the lines before it — and leaves the last mutation **applied**. That cost a
  44-mutation run at line six and corrupted `post_store.rs` until it was noticed.
  `mutate-batch.sh` now rejects an empty `from` or `to` in pre-flight.
- **`set_user_roles` does not change what a session can do.** `SessionHasPermissionTo` reads
  `session.Roles`, copied at login and never re-read, so granting a role and reusing the old token
  gets a 403 from **Go**. `common::login_plain_user` mints a fresh one.

## The seven `/schemes` routes, four reads and three refusals (2026-09-07)

New: `crates/mm-api/src/schemes.rs`, `crates/mm-app/src/scheme.rs`,
`crates/mm-api/tests/parity/schemes.rs`, `scripts/mutations/schemes.plan`. Changed:
`crates/mm-store/src/{team_store,channel_store}.rs`, `crates/mm-model/src/scheme.rs`,
`crates/mm-api/src/lib.rs`, `crates/mm-app/src/lib.rs`, `crates/mm-api/tests/common/mod.rs`.

110 → **117 of 764**.

| Go file | Rust | Status | Tests | Note |
|---|---|---|---|---|
| api4/scheme.go (`getSchemes`) | `mm-api/src/schemes.rs` | DONE | 2 parity, 1 unit | The **empty** scope is one of three accepted values; `playbook` and `run` are refused. |
| api4/scheme.go (`getScheme`) | `mm-api/src/schemes.rs` | DONE | 1 parity | Trailing newline, unlike `getSchemes` twenty lines above it. |
| api4/scheme.go (`getTeamsForScheme`) | `mm-api/src/schemes.rs`, `mm-store/src/team_store.rs` | DONE | 1 parity | Wrong scope is a **400**, and the teams are sanitized on the way out. |
| api4/scheme.go (`getChannelsForScheme`) | `mm-api/src/schemes.rs`, `mm-store/src/channel_store.rs` | DONE | 1 parity | Excludes only `'S'`, where `ChannelStore::get` excludes everything but `('O','P','D','G')`. |
| api4/scheme.go (`createScheme`, `patchScheme`, `deleteScheme`) | `mm-api/src/schemes.rs` | DONE | 1 parity | 501 on an unlicensed server — the whole route here — after each one's own id and body checks. |
| app/scheme.go (`IsPhase2MigrationCompleted`, four reads) | `mm-app/src/scheme.rs` | DONE | 2 unit | Every read is gated on a `Systems` row, and its refusal is a **501**. |

### The three writes are complete, not stubbed

Each begins with the same licence test and answers 501 before any permission check or database
access, so on an unlicensed server that 501 **is** the route — there is no reachable path past it.
A licensed installation is forwarded, because the test's other two clauses read
`Features.CustomPermissionsSchemes` and `SkuShortName` out of the signed licence body.

What makes them worth porting rather than proxying is the **order**, which differs in all three:
`createScheme` decodes the body first, so `{` is a 400 and a well-formed body is a 501;
`patchScheme` validates the id, then decodes, then tests the licence; `deleteScheme` goes straight
from the id to the licence. Reversing any pair answers 501 to a request Go answers 400 to.

### `model.Scheme` was missing `#[serde(default)]`, and the route found it

Go decodes a request body into `model.Scheme` with `json.NewDecoder`, which fills every absent
field with its zero value — so `{"name":"x"}` is a valid scheme. Without `default`, serde demanded
all eighteen fields and `POST /api/v4/schemes` answered **400** where Go answered its 501. This is
a model-layer defect that only a route could surface: the fixture round-trip test passes either
way, because the fixture is fully populated by construction.

### Three fixture findings, none of them about the port

- **The purge deletes what the fixture just planted.** `purge_api_fixtures` is a `OnceCell` that
  `create_team` triggers; planting schemes before the binary's first `create_team` has them
  deleted moments later. The scheme fixture now awaits the purge explicitly first.
- **Go caches schemes by id.** Re-planting a fixed id with a new `CreateAt` leaves Go serving the
  previous run's row from its local cache while this port reads the new one — a parity failure on
  a timestamp with nothing wrong on either side. Planted ids are now unique per run.
- **`model.Scheme` is eighteen fields, not sixteen.** Counted from the running server rather than
  from the struct, which is the only way to be sure the four playbook and run role names are on
  the wire even on a server that never fills them.

### The mutation run, and the four fixture findings it forced

**30 run, 28 caught, 2 controls survived, 0 harness faults** — over three passes. The first pass
had **two harness faults**: dropping a SQL predicate left `$1` bound and unused, which sqlx
rejects at compile time. The mutations were rewritten to keep the parameter (`AND $1::text IS NOT
NULL`), which is what a "drop the filter" mutation should have looked like anyway.

Seven real survivors, and every one was a fixture that could not tell two answers apart:

| Survivor | What the fixture could not see |
|---|---|
| the permission swap | **No stock role separates the three sysconsole reads** — `system_admin`, `system_manager`, `system_read_only_admin` and `system_user_manager` all hold all three. Fixed with planted single-permission roles. |
| `SanitizeTeams` removed | An admin can manage every team, so the sanitizer was a no-op for the only session the suite had. The teams reader now holds no `manage_team`. |
| both `page * per_page` | At page 0 the offset **is** the page. Fixed with three schemes and three teams, and `?per_page=2&page=1`. |
| `ORDER BY DisplayName` → `Name` | `create_team` derives both from one tag, so they always agree. The fixture teams are now renamed to invert the two orderings. |
| `Type <> 'S'` → `Type IN ('O','P','D','G')` | Without a **board** channel the two filters return the same rows. `POST /channels` will not create one, so it is planted. |
| the migration gate | The `Systems` row is present on any server Go has started, so only the `true` arm is reachable. The decision moved into a named function with a truth table. |
| `SchemeId = $1` → `IS NOT NULL` | Every team with *any* scheme had *this* scheme. A second scheme with its own team and channel now exists. |

**One survivor was a test that discarded its own answer.** `let (page_one, _) = fetch_both(...)`
kept Go's body and threw ours away, so the assertion was about the Go server — which is never
mutated. Worth recording twice over: the fix was written once against a line `cargo fmt` had
already split, so the edit silently did not apply and the mutation survived a second time.

**And one unit test asserted nothing at all.** `assert_ne!` on two `Response`s compares their
*debug* output, which renders any body as `Body(UnsyncBoxBody)` — so a test that two encodings
differ passed whatever the bytes were. The byte-producing half is now its own function.

### The suite got slower to churn, and three older tests had to catch up

The schemes fixture creates four users, five teams and two channels in one burst, which is enough
to break three tests that had nothing to do with schemes:

- `fetch_both_stable`'s default budget went from 12 windows to **24**, with the backoff capped so
  a *real* divergence still fails quickly rather than four times slower.
- `users_list::the_unfiltered_list_matches_go` asserted its fixture users were on page 0 of the
  user list. `per_page` is clamped to 200 and this database holds more users than that, so it was
  really testing how many users happened to exist; it now walks the pages.
- `system_usage::the_usage_counters_need_no_permission` byte-compared a live `COUNT(*)` of teams
  with no retry. The status check stays unretried — 200 is 200 — and only the body is bracketed.

## Fifteen `/data_retention` routes: a 501, and the checks in front of it (2026-09-07)

New: `crates/mm-api/src/data_retention.rs`, `crates/mm-api/tests/parity/data_retention.rs`,
`scripts/mutations/data-retention.plan`. Changed: `crates/mm-api/src/lib.rs`.

117 → **132 of 764**.

`App.DataRetention()` is `einterfaces.DataRetentionInterface`, registered only by the enterprise
build, so on the Team Edition binary beside us it is always nil and every app function in
`app/data_retention.go` answers `ent.data_retention.generic.license.error` at 501 before touching
anything. That refusal is the whole of each route here; a licensed installation is forwarded.

### The routes are worth porting because their check *order* differs in almost every handler

| Route | Order before the refusal |
|---|---|
| `getGlobalPolicy` | nothing — Go's comment says "No permission check required" |
| `getPolicies`, `getPoliciesCount` | permission |
| `getPolicy`, `getTeamsForPolicy`, `getChannelsForPolicy` | permission, **then** the id |
| `deletePolicy` | the id, **then** permission |
| `createPolicy` | body, then permission |
| `patchPolicy` | body, then the id, then permission |
| `addTeamsToPolicy` and three siblings | the id, then body, then permission |
| the two per-user routes | the user id, then self-or-`manage_system` |

### `RequirePolicyId` is dead on eleven routes, and reproducing it would be the bug

Go calls `c.RequirePolicyId()` and **does not check `c.Err`** — `getPolicy` goes straight on to
`c.App.GetRetentionPolicy(...)`, whose error *overwrites* the 400 the id check just set. So
`GET /api/v4/data_retention/policies/short` is a **501, not a 400**. Measured against the running
Go server, because no reading of the handler suggests it, and a port that "helpfully" validated
the id would answer 400 to eight requests Go answers 501 to.

The two per-user routes are the exception — they do check — so a malformed user id there really is
a 400. Two conventions in one file, forty lines apart.

### Two body shapes, two different 400s, and a `null` that is not an error

`createPolicy` and `patchPolicy` decode a `RetentionPolicyWithTeamAndChannelIDs` and fail with
`api.context.invalid_body_param.app_error` naming `policy`; the four id-list routes use
`model.SortedArrayFromJSON` and fail with `api.payload.parse.error`, which names nothing. And
`SortedArrayFromJSON` returns `(nil, nil)` for a JSON `null` — no error — so `null` reaches the
licence refusal while `[` does not.

### The two `/search` children are **not** migrated, and they look identical

`searchTeamsInPolicy` and `searchChannelsInPolicy` sit in the same file with the same shape and are
not licence-gated at all: they call `SearchAllTeams` / `SearchAllChannels` with a `policy_id`
filter and answer **200** on this server — measured, not assumed. They belong with `/teams/search`
and `/channels/search`, where the search machinery will land. `scripts/routes.py --todo` lists
them.

### The handler bodies are one line each, and the variation is data

Fifteen near-identical functions would have buried the orderings that are the whole content of
this file. A `Route { name, body, gate, body_first }` makes the table above reviewable against the
code, and the `route!` macro keeps each handler to its Go name.

### The mutation run

**24 run, 22 caught, 2 controls survived, 0 harness faults.** One real survivor, and it was the
one gate the fixture could not see: the per-user routes use `manage_system`, and the readers held
neither that nor it — a plain user is refused by both rules and an admin is admitted by both. The
suite now asks with the *read* permission alone, which only the correct rule refuses.

Notably the "reproducing the dead id check" mutation was **caught**: adding a `policy_id`
validation makes eight routes answer 400 where Go answers 501.

### Two more churn flakes, and the number behind them

The suite creates **about 240 users and dozens of teams per run** — measured, not estimated: the
oldest `mmrsplain%` row is four minutes old on a database holding 246 users. The purge works; that
is simply the load. Adding three fixtures pushed two long-standing tests over the edge, and both
were asking a question the population had outgrown:

- `users_list::the_four_filtered_arms_match_go` checked that the outsider appears in the
  `not_in_team` arm. That arm returns nearly every user, and `per_page` is clamped to 200, so page
  0 no longer contains them. The byte comparisons are unchanged — they compare the same page on
  both servers — and only the membership assertion walks.
- `session_team_members` compared the admin's membership count between Go and our store. **Creating
  a team makes the creator a member**, and every suite that needs a team creates one with that same
  admin token, so the count moves between the two reads. It now brackets Go either side, like the
  other moving-target comparisons.

## Seventeen routes whose first statement is a licence test (2026-09-07)

New: `crates/mm-api/src/licensed_features.rs`,
`crates/mm-api/tests/parity/licensed_features.rs`, `scripts/mutations/licensed-features.plan`,
`scripts/probe-routes.py`. Changed: `crates/mm-api/src/lib.rs`.

132 → **149 of 764**. All thirteen of `content_flagging.go` and the four write halves of
`channel_bookmark.go`.

### These are a different shape from `/data_retention`, and the module says so

`/data_retention` is a family of refusals with *live* checks in front of them, and reproducing
that ordering is most of the work. Here the licence test is the **first statement of every
handler**, so nothing else on the request is ever consulted: not the body, not the permission, not
the id. A caller with no permission and a malformed body gets the same 501 an administrator does,
and the parity suite asserts each of those three separately — every one is a check a reader might
add "for symmetry" with a neighbouring file, and every one would be a divergence on every request.

The two gates are **not** the same test. Content flagging needs
`MinimumEnterpriseAdvancedLicense` — a licence *tier*, so an Enterprise licence below Advanced is
still refused — while channel bookmarks need only `License() != nil`. Both collapse to "refuse"
with no licence at all, which is the only case this server answers.

### `scripts/probe-routes.py` is how this group was chosen

It asks the running Go server what each unmigrated route answers, with ids that do not exist and
without sending anything that could create or destroy. The 461 remaining HTTP pairs group into
families: 12 answer `api.data_spillage.error.license`, 12 `api.recap.disabled.app_error`, 11
`api.remote_cluster.service_not_enabled.app_error`, 11 `api.license_error`. Reading seventeen
handlers to discover they share one refusal costs more than asking once.

**And it corrected a wrong assumption immediately.** `listChannelBookmarksForChannel` was assumed
to be an ordinary read — the parity suite asserted so — and answers **501** like its four
siblings. It was already ported, in `channels.rs`; all five bookmark routes are refusals, and this
module holds four. `channels::licence_gate` is reused rather than re-derived, because "what counts
as licensed" is one question with one answer.

### The refusal that is not ours

`requireContentFlaggingEnabled` has a second arm — `ContentFlaggingSettings.EnableContentFlagging`
— answering `api.data_spillage.error.disabled` at the same status. It sits *behind* the licence
test, so this server can never produce it. Recorded because the two ids differ by one word.

### The mutation run

**12 run, 10 caught, 2 controls survived, 0 harness faults.** The one survivor was the licensed
branch: every test ran unlicensed, so "refuse" and "refuse or forward" were the same program. A
mutation that never forwards would be silently wrong on any deployment that has a licence, and
nothing here could see it. `a_licence_row_hands_every_route_back_to_go` flips the row for all
seventeen and restores it before asserting, so a failure does not leave the stack licensed for the
rest of the binary.

Half the plan swaps one family's error id for the other's — which is the real risk when two
families share a module — and all of those were caught.

### A latent test bug the growing database finally exposed

`db_user_profile_lists::not_in_team_pages` walks every page of `GetProfilesNotInTeam`. The two
store methods it sits beside do **not** take the same second argument — `get_all_profiles` takes a
**page**, `get_profiles_not_in_team` takes an **offset**, matching Go, where `GetAllProfiles`
multiplies internally and `GetProfilesNotInTeam` does not — and the helper passed a page index to
both. So it walked offsets 0, 1, 2, … and re-read 199 of every 200 rows.

Nothing noticed while the only assertions were about the fixture's own users. It failed the moment
the development database crossed ~250 users: every "page" came back full and the fifty-iteration
guard ran out. The guard was doing its job; the bug was two arguments that look alike.

### One more stale forwarding canary

`licence_gated_channels::other_methods_are_forwarded` asserted that
`POST /channels/{id}/bookmarks` was still Go's. It is now ours. The assertion is **inverted rather
than deleted** — the property it guarded, that the two methods on that path agree, still matters —
and an unregistered method on the same path is the canary now. That is the third time this session
a "still forwarded" test has had to move; they are worth keeping, and worth moving rather than
removing.

## Fifteen `/recaps` routes behind a **configuration** gate (2026-09-07)

New: `crates/mm-api/src/recaps.rs`, `crates/mm-api/tests/parity/recaps.rs`,
`scripts/mutations/recaps.plan`. Changed: `crates/mm-app/src/config.rs`,
`crates/mm-api/src/lib.rs`.

149 → **164 of 764**. All of `recap.go` and `scheduled_recap.go`.

Every one of the fifteen handlers opens with `requireRecapsEnabled(c)`, which is
`Config.AIRecapsEnabled()` — `FeatureFlags.EnableAIRecaps && AIRecapSettings.IsEnabled()`. On a
stock server the feature flag is `false`, so all fifteen answer `api.recap.disabled.app_error` at
501 before consulting anything else.

### It looks like the licence families and differs in the way that matters

**An operator can turn this on**, from the environment, without touching a licence. The moment
they do, every one of these routes must stop being answered here and go back to Go — there is no
AI recap engine on this side and never will be. So the gate is read from the live configuration on
every request rather than decided at startup.

That also makes the *enabled* half testable in a way a licence gate's is not: the routing decision
is a pure function of `Config`, so the branch that must forward is pinned by driving the config
directly, without restarting a process the suite did not start.

### `AIRecapSettings.IsEnabled()` inverts the intuition

It is `s == nil || s.Enable == nil || *s.Enable` — an **absent** setting means *enabled*. Only the
feature flag is off by default. A port reading the setting as `unwrap_or(false)` would refuse
recaps on a server whose operator had switched the flag on and left the setting alone, which is
the normal way to enable them. `ai_recap_settings_enable` is therefore `Option<bool>` all the way
through, and the document reader carries an absent key across as absent rather than resolving it
to a default like every neighbouring field.

The Go source marks the flag `FEATURE_FLAG_REMOVAL: EnableAIRecaps — Remove this when GA is
released`, so this gate has a shelf life; when the flag goes, the setting alone decides.

### The mutation run, and a new way to test an environment-only gate

**14 run, 12 caught, 2 controls survived, 0 harness faults.** The survivor was the gate itself
replaced by a constant `false` — never forward — which with recaps off is the *same program*. It
is wrong only on a server that has enabled them, which is exactly the case the suite could not
reach: the flag has no database representation, so nothing the parity fixtures can write turns it
on.

`common::SecondServer` closes that. It starts **the same binary** `parity.sh` just built, on its
own port, with the same database and upstream and only the variables under test changed, and kills
it when the guard drops — panic or not. `an_enabled_server_forwards_every_recap_route` uses it to
assert all fifteen forward with the flag set, and then asks the ordinary server on :8066 the same
question to show the difference is the configuration and not the build.

That helper is reusable: several more families ahead are gated on settings that live only in the
environment.

## Cloud and connected workspaces: twenty-five routes, three shapes (2026-09-07)

New: `crates/mm-api/src/{cloud,connected_workspaces}.rs`,
`crates/mm-api/tests/parity/cloud_and_workspaces.rs`, `scripts/mutations/cloud-workspaces.plan`.
Changed: `crates/mm-api/src/lib.rs`.

164 → **189 of 764**. Twelve `/cloud` routes and thirteen across `remote_cluster.go` and
`shared_channel.go`.

### The cloud family refuses with a **400**, and everything around it with a 501

`c.App.Cloud()` is an enterprise interface, nil on Team Edition, so every one of these handlers
answers `api.server.cws.needs_enterprise_edition` at **400 Bad Request** (cloud.go:58). Every other
licence-shaped refusal this server produces is a 501. A port that reached for the neighbouring
shape would answer the right id with the wrong status, which is what a client branches on.

The same nine-line helper has a second arm — `CloudSettings.Disable`, answering
`api.server.cws.disabled` at **422** — reached only after the interface exists. Three ids and three
statuses in one helper, and this server can produce exactly one of them.

### The connected-workspace gate moves relative to the permission check

`startRemoteClusterService` refuses without a licence *before* it looks at the config, so
`GetRemoteClusterService()` is always nil here and all thirteen answer
`api.remote_cluster.service_not_enabled.app_error` at 501. What makes them worth porting is where
that gate sits:

- `remote_cluster.go` checks a permission **first** — two different ones, and one is an *either* —
  so an unauthorised caller gets 403;
- **two `shared_channel.go` routes consult the service before the permission**, so the same caller
  gets 501 instead;
- `getRemoteClusterInfo` has no permission check at all.

### `RequireRemoteId` is not `IsValidId`, and the port got it wrong

`RequireRemoteId` (web/context.go:745) tests `== ""` and nothing else — so `short` passes it and
the service gate answers 501 — while `RequireTeamId` and `RequireChannelId`, used on the two
`/sharedchannels` routes in the *adjacent file*, are the usual `IsValidId` and answer 400. The
first version of this port applied the familiar shape to all six and answered 400 on four routes
Go answers 501 to. The parity suite caught it on its first run.

Since gorilla's `{remote_id:[A-Za-z0-9]+}` cannot produce an empty segment, the check is
unreachable through the router — but it is ported as written, because reproducing the *other*
convention is what went wrong.

### Six routes in these files are deliberately not migrated

`remoteClusterAcceptMessage`, `remoteClusterPing` and `remoteClusterConfirmInvite` use
`api.RemoteClusterTokenRequired` — a remote cluster's own token, not a session — as do the two
file-streaming routes. `handleCWSWebhook` uses `api.CloudAPIKeyRequired`. Answering any of them
from a session-authenticated handler would change who can reach them. `getPreviewModalData` has no
cloud gate at all (404, measured) and `canUserDirectMessage` is an ordinary read that answers 200.

### The mutation run

**14 run, 12 caught, 2 controls survived, 0 harness faults.** Both survivors were the same gap:
`manage_secure_connections` and `manage_shared_channels` are two permissions, one route accepts
*either*, and **no stock role separates them** — an admin holds both, a plain user neither. Planted
single-permission roles make the three cases distinguishable: the create wants the first only, the
invite wants the second only, and the listing accepts either, which is what tells
`SessionHasPermissionToAny` from a single check.

That is the third group whose survivors were all "no stock role can tell these apart". It is worth
stating as a rule: **a permission check is untested until a session exists that holds exactly one
side of it**, and on this server that nearly always means planting a role.

## The last fourteen: four small families, four refusals, two statuses (2026-09-07)

New: `crates/mm-api/src/{compliance,feature_gates}.rs`,
`crates/mm-api/tests/parity/gated_families.rs`, `scripts/mutations/gated-families.plan`.
Changed: `crates/mm-app/src/config.rs`, `crates/mm-api/src/lib.rs`.

189 → **203 of 764**, which is 103 routes migrated in this session.

| Family | Gate | Refusal |
|---|---|---|
| `compliance.go` | the enterprise interface | `ent.compliance.licence_disable.app_error`, 501 |
| `ip_filtering.go` | a licence — **cloud**, at Enterprise tier | `api.context.ip_filtering.not_available.app_error`, 501 |
| `ai_bridge_test_helper.go` | `ServiceSettings.EnableTesting`, default **false** | `api.ai_bridge_test_helper.disabled.app_error`, 501 |
| `scheduled_post.go` | `ServiceSettings.ScheduledPosts` (default **true**), *then* a licence | `api.scheduled_posts.license_error`, **400** |

### One gate, two arms, one status

`requireScheduledPostsEnabled` answers `api.scheduled_posts.feature_disabled` when the setting is
off and `api.scheduled_posts.license_error` when it is on and there is no licence — **both 400**.
The setting defaults to `true`, so the licence arm is the one a stock server reaches; a second
server with the setting off proves the other arm, and that the status does not move with it.

### `ip_filtering` needs four things, and we can only establish one

`ensureIPFilteringInterface` wants the interface **and** a licence **and** `license.IsCloud()`
**and** `MinimumEnterpriseLicense`. A self-hosted Enterprise installation is refused as firmly as
an unlicensed one. This server answers only the "no licence at all" case and forwards the rest,
where Go applies the other three.

### Compliance checks its own things first, and `RequireReportId`'s result **is** checked

Three permissions across four routes — and the download's is not the read one its sibling uses on
the same resource, so a role that may list and read reports may still not download one. Unlike
`RequirePolicyId` in `/data_retention`, whose 400 the app error overwrites, both id checks here
test `c.Err` and return: a malformed report id really is a 400. Two files, two conventions, and
the parity suite asserts this one rather than inheriting it from the neighbour.

### The mutation run

**18 run, 16 caught, 2 controls survived, 0 harness faults — clean on the first pass**, the only
group in this session to manage that. Half the plan swaps one family's id or status for a
neighbour's, which is what four look-alike families invite, and the fixtures were built from the
start with the four lessons the earlier groups paid for: a session holding exactly one side of
each permission, a second server for each configuration gate, both bodies compared, and a
neighbour row so a filter is distinguishable from no filter.

### A pre-existing flake, fixed by asserting the property instead of the answer

`threads_for_user::per_page_limits_the_list_and_not_the_totals` failed once in a full-suite run —
both servers agreed byte-for-byte, and it passes three times out of three in isolation. It pinned a
hard-coded thread id, so under load something about the fixture's ordering moved and the failure
was about the wrong thing. It now asserts what the route actually guarantees: `per_page=1` returns
the **head of the unpaged list**, checked against the list itself, which the ordering test beside it
already pins. Neither depends on a timestamp race any more.

## The websocket hub: what a write route will broadcast into (2026-09-08)

New: `crates/mm-app/src/hub.rs`, `crates/mm-api/src/websocket.rs`,
`crates/mm-api/tests/parity/websocket.rs`, `scripts/mutations/websocket-hub.plan`.
Changed: `crates/mm-app/src/lib.rs`, `crates/mm-api/src/lib.rs`, `crates/mm-api/tests/common/mod.rs`,
`Cargo.toml`, `crates/{mm-app,mm-api}/Cargo.toml`.

203 → **204 of 764**. One route, `GET /api/v4/websocket` — and it is phase 0 of the write work
rather than a route in its own right: **every real mutation in Go publishes an event**, so a port
that persists the right row and broadcasts nothing is wrong in a way no HTTP comparison can see.
This is the thing the write routes will be measured against.

### The model layer was already there, and this is the first time it has been reachable

`mm-model`'s `websocket_message.rs` and `websocket_request.rs` (783 lines) were ported
breadth-first and had never been called by anything. They needed no changes. That is the argument
for CLAUDE.md's rule stated from the other end: the work was correct, it simply sat unexercised
until a route arrived to exercise it.

### Two encodings, and a client can tell them apart

Go writes a queued frame one of two ways and this is wire format, not an optimisation:

- `Hub.Broadcast` calls `PrecomputeJSON` (web_hub.go:718), so every **broadcast event** leaves
  through `precomputedJSONBuf` — hand-concatenated, **a space after each colon**, no trailing
  newline: `{"event": "posted", "data": {…}, …}`.
- Everything else — `hello`, and every `WebSocketResponse` — goes through `json.Encoder.Encode`,
  which is compact **and appends a newline**: `{"event":"hello",…}\n`.

Both were measured against the running server. `OutgoingFrame::Event` therefore carries a
`precomputed` flag and the write pump picks the encoder from it. A port that used one form for
everything would be half wrong whichever form it picked.

`seq` is assigned in the write pump, not in the hub, and **only to events** — a response
interleaved between two events does not consume a number.

### The extractor that answered before the handler

`WebSocketUpgrade` as a handler argument is an extractor, and an extractor that rejects answers
*before* the handler body. Go checks the session first, in `ServeHTTP`, ahead of the handler. The
difference is visible: `GET /api/v4/websocket?access_token=…` is **401** on Go —
`handlers.go:281` refuses a non-OAuth session in the query string on every route — and was **400**
here until the upgrade was moved inside the handler. Caught by the parity suite on its first run.

### The fan-out order is the behaviour

`ShouldSendEvent`'s five addressing fields are **not a union**: `connection_id` wins over
`user_id`, which wins over `omit_users`, which wins over `channel_id`, which wins over `team_id`,
and the later field is never consulted. An event addressed to this connection but a different user
*is delivered*. The decision is split into a pure `addressing_verdict` precisely so that ordering
can be tested without a database — see `mm_app::hub`.

Two further rules that read wrongly if skimmed: `omit_users` tests **key presence**, not the
bool's value, so `{u: false}` still omits; and `notInThread` ANDs its two arms, so a connection
that has told us about neither thread view is never "not in thread" and typing is not withheld
from it.

### A socket is not isolated the way a request is

Three of the four parity tests failed on their first full-suite run — not on their answers but on
their *counts*. Other suites were creating users and teams, and Go broadcast `new_user`, `posted`
and `user_added` onto the admin's socket between a request and its reply: nine frames where the
test expected two. `SocketProbe::responses` now selects the frames that answer a request
(`seq_reply`, or `status` when the seq was zero). Worth stating because every write route's test
will have the same shape, and because the failure was Go's hub demonstrating it works.

### What is deliberately not here

Reconnect replay ([D-181]), cross-process events ([D-182]), broadcast hooks ([D-183]), the MFA arm
([D-184]), guest visibility ([D-185]), msgpack frames ([D-187]) and the six `wsapi` actions
([D-188]). `hello`'s `server_version` and `server_hostname` cannot match Go's and never will —
stated in `App::hello_message`, not in the backlog.

`crates/mm-ws` remains a six-line stub and is now definitively dead: the hub belongs in `mm-app`
(Go puts it in `channels/app/platform`) and the connect handler in `mm-api` (Go puts it in
`api4`), so a separate binary has nothing to hold. It is left in the workspace rather than removed
in the same commit as the thing that replaced it.

### The mutation run

**20 run, 18 caught, 2 controls survived, 0 harness faults** — after a first pass of 13 caught, 5
survived and 2 harness faults. Every one of the three real survivors was the same species of test
bug:

- the slow-queue test filled the queue to `SEND_SLOW_WARN` **symbolically**, so mutating the
  constant moved the threshold and the fill together and the test saw nothing. It now asserts the
  literal 128;
- `should_send_event_to_guest` was a method on `App`, so no unit test could reach it and replacing
  its body with `true` survived. It is a free function now;
- `unregister` was checked with `conn_count_for_user`, which filters on `is_active` — so a
  connection left behind in the user index was invisible to it. The test now asserts on the index
  a broadcast actually iterates.

The two harness faults were the same mistake twice: a mutation that referenced a constant not
imported at module scope, which fails to compile rather than failing a test.

## The first two writes that mutate a row and broadcast (2026-09-08)

New: `crates/mm-api/tests/parity/reaction_writes.rs`, `scripts/mutations/reaction-writes.plan`.
Changed: `crates/mm-store/src/{reaction_store,post_store}.rs`,
`crates/mm-app/src/{reaction,channel,common_teams,config}.rs`,
`crates/mm-api/src/{reactions,lib}.rs`, `scripts/routes.py`.

204 → **206 of 764**: `POST /api/v4/reactions` and
`DELETE /api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}`.

Every earlier "write" this server serves is a POST-shaped read or a licence refusal. These two are
the first that change a row, and the first that publish through the hub built in the previous
commit.

### A write is tested on three surfaces, and each server reads back its own

The answer, the row, and the event. The third is new and the second turned out to be the hard one:

**Go's reaction cache cannot be made to see our write, and `POST /api/v4/caches/invalidate` does
not help.** `InvalidateAllCachesSkipSend` (platform/cluster_handlers.go:137) clears the session,
status, team, channel, user, post, fileinfo, webhook and link caches. `reactionCache` is cleared
only by `LocalCacheStore.Invalidate()`, which is reached **only from the cluster-message handler**.
Measured: a reaction deleted through `:8066` had `DeleteAt` set in the table and was gone from
`:8066`'s own read, while `:8065` listed it after three explicit invalidations. Recorded as
[D-190], whose most important consequence is a rule for every future write group: **read a Rust
write back through Rust**. An hour went into this one looking like a failed delete.

Each server therefore writes to its **own** post. Sending the same reaction to both would make the
second call an upsert over the first's row, and an upsert returning the original `create_at` looks
exactly like agreement.

### The answer and the row disagree about `create_at`, on both servers

`PreSave` mints a fresh `CreateAt` because the incoming reaction has none, and the response is
marshalled from that struct — but the insert is `ON CONFLICT (UserId, PostId, EmojiName) DO
UPDATE` over four columns and `CreateAt` is not among them. So re-reacting after a withdrawal
answers with a new timestamp while the stored row keeps its original. Both halves are asserted;
"fixing" either would diverge from Go on the other.

### Two framings, ten lines apart in one Go file

`saveReaction` uses `json.NewEncoder(w).Encode` and its body carries a **trailing newline**;
`getReactions` beside it uses `json.Marshal` + `w.Write` and does not. The delete answers
`ReturnStatusOK`, also unframed. Asserted, not assumed.

### gorilla decides two of the delete's four refusals

The path is
`/users/{user_id:[A-Za-z0-9]+}/posts/{post_id:[A-Za-z0-9]+}/reactions/{emoji_name:[A-Za-z0-9\_\-\+]+}`
(api.go:127). `short` is inside the id class, so the route matches and `RequireUserId` answers
**400**; `not an emoji` is outside the emoji class, so **no route matches** and Go answers its 404
page — `RequireEmojiName`'s 400 is unreachable through the router, exactly as `RequireRemoteId`'s
check was in the connected-workspaces group. axum's `{emoji_name}` matches anything, so the port
answered 400 where Go answers 404 until the classes were reproduced; the segment is now checked
and a failure **forwards**, which reproduces the 404 body byte for byte including the URL quoted
in its `detailed_error`.

### Three things this server declines rather than approximates

`ReactionWrite::Forward` carries the reason, and the handler hands the whole request to Go:

| Reason | Why |
|---|---|
| a `burn_on_read` post | needs the `ReadReceipts` store |
| a live `PersistentNotifications` row | Go runs `ResolvePersistentNotification` **after** the insert and returns its error, so declining afterwards is not reproducible |
| a bot in a restricted DM | `IsBotExemptFromDMRestrictions` is a plugin decision |

The middle one nearly swallowed the whole group. `IsPersistentNotificationsEnabled()` is
`PostPriority && AllowPersistentNotifications` and **both default to true**, so the first version
forwarded every reaction and the parity suite's first run reported `x-mmrs-served-by: go`. Go's own
function gives up on its first three lines — the post's author is exempt, the feature can be off,
and above all the post has to *be* a persistent-notification post — so those three are ported and
only a live row forwards. That needed one new store query
(`PostStore::has_persistent_notification`), which is the smallest thing that turns "always
forward" into "almost never".

### `CheckIfChannelIsRestrictedDM`, and a guard that was missing

Ported in full, which needed the `requestingUserID != ""` guard added to
`get_direct_or_group_message_members_common_teams_as_user`. Go's two entry points differ only in
whether they pass a user, and the empty one exists **precisely to skip** the membership
short-circuit (channel.go:4227). Without the guard the new caller would be told nobody is a member
and get `NotAMember` every time. Unreachable before, because the only existing caller passes a
session's user.

`len(teams) == 0` means **restricted**, which reads backwards until you notice the function's name
is a question about restriction rather than permission.

### The mutation run found a real bug: `str::to_lowercase` is not `strings.ToLower`

**Twenty-seven mutations, and the one that mattered survived the first pass.** Removing the
handler's lower-casing changed nothing the suite could see, which led to asking *why* — and the
answer was that the port had the wrong lowercase entirely.

Go's `strings.ToLower` is the **simple** per-rune case mapping. Rust's `str::to_lowercase` is the
**full** Unicode one. They disagree on input a client can send:

| input × 22 | raw | Go | `str::to_lowercase` |
|---|---|---|---|
| `İ` U+0130 | 44 bytes | 22 bytes (`i`) | 66 bytes (`i` + U+0307) |
| `Ⱥ` U+023A | 44 bytes | 66 bytes (`ⱥ`) | 66 bytes |

The emoji name is lower-cased **before** the 64-byte cap, so the first row is a 404 from the emoji
lookup on Go and was a 400 from our own length check; the second is a 400 on both, and only
because the cap is applied after lowering. Both measured against the running server, and both are
now fixtures.

`mm_model::utils::go_to_lower` already existed, pinned against a Go corpus over 30 inputs, and its
doc comment names this case in as many words: *"an emoji name that lowercases differently in the
two servers is a divergence on a shared database."* The port used the wrong one anyway. Three call
sites fixed.

The *app* layer's lower-casing is now an **equivalent mutant** — the handler has already
normalised — and is recorded as such in the plan rather than left as a standing survivor. Go
carries the same redundancy.

### A harness fix that was not optional

`channel_members_list::pages_split_cover_and_run_out_identically` failed two full-suite runs in a
row and passed alone. The cause is not paging: `purge_api_fixtures` deletes `channelmembers` for
every `mmrsplain%` user, and a `OnceCell` on the purge alone runs it whenever the *first* test
trips it — which is while other suites already have fixtures up. This group added two
`create_plain_user` calls and made the window wider.

The purge now runs inside `go_minted_token`'s `OnceCell`. No stack-backed test can build anything
before it has a token, so the sweep finishes before any fixture exists — which is what the purge's
own comment always asked for. See [D-167], where the remaining instance is narrowed to an
intra-suite race in `threads_for_user` that predates this work.

### The route inventory was under-counting by one, and it was the websocket

`scripts/routes.py` normalised `{websocket:websocket(?:\/)?}` to `{websocket}` — a *parameter* —
so `/api/v4/websocket`, the literal path every client uses, failed to match its own inventory row
and the previous commit's route read as unserved. Fixed and verified: exactly one of 764 paths
changed. [D-189].

## Drafts and deletePreferences: four writes, three of them idempotent (2026-09-08)

New: `crates/mm-api/tests/parity/draft_and_preference_writes.rs`,
`scripts/mutations/draft-preference-writes.plan`.
Changed: `crates/mm-store/src/{draft_store,preference_store,channel_store}.rs`,
`crates/mm-app/src/{draft,preference}.rs`, `crates/mm-api/src/{drafts,preferences,lib}.rs`.

206 → **210 of 764**: `POST /drafts`, the two `deleteDraft` registrations, and
`POST /users/{user_id}/preferences/delete`.

### `POST /drafts` with an empty message deletes the row and answers `201 null`

Go returns `(nil, nil)` after deleting and the handler writes `http.StatusCreated` and *then*
encodes the nil pointer. So the answer to a request that destroyed a row is **`201` with a body of
the four bytes `null`**, newline-terminated. Three independent things a port gets wrong — the
status, the body, and whether it deleted at all — and each has its own mutation.

It also publishes **nothing**: Go returns before the `draft_created` publish, so a draft removed
this way is invisible to the user's other sessions until they refetch. `deleteDraft` does publish
`draft_deleted`.

### The header is `Connection-Id`, with no `X-`

Measured, not read: the first version used `X-Connection-Id` and Go published an empty
`omit_connection_id` for a request that carried the header. The constant lives in `client4.go`,
which this project never reads, so the value is repeated in `mm_api::drafts` — the same choice
`mm-model` made for `StatusFail`.

It matters more than a header name usually does. `omit_connection_id` is read by the hub **before**
`user_id`, so getting it wrong changes *who is skipped* rather than how many events go out — the
tab that saved the draft would be told about its own save.

### The answer and the row disagree about `create_at`, again

Same shape as the reaction upsert and for the same reason: `PreSave` mints a fresh `CreateAt`
because the incoming draft has none, the response is marshalled from that struct, and the conflict
clause updates seven columns of which `CreateAt` is not one. Both halves asserted.

### `deletePreferences` validates the whole batch before deleting any of it

Two loops, not one. A batch naming another user is a **403** with *nothing* deleted; fusing the
loops would delete the entries preceding the bad one. The parity suite checks the survivor count
after the refusal, which is the only way to see the difference.

It publishes **two** events, and the first — `sidebar_category_updated` — carries an empty data
map, with Go's own `TODO` beside it. A port that published only `preferences_deleted` would leave
the webapp's sidebar stale after unfavouriting a channel.

Nothing is forwarded here, unlike the *update* path: `deletePreferences` has no flagged-post
branch, and its sidebar cleanup touches only `favorite_channel`, which is now ported.

### The mutation run, and two mutations that did not compile

**22 run, 20 caught, 2 controls survived, 0 harness faults** — after a first pass of 17 caught, 1
survived and **2 harness faults**, which void a tally rather than reduce it.

Both faults were the mutation's fault, not the code's, and are worth naming because they are easy
to repeat: `message = message` inside `ON CONFLICT DO UPDATE` is an *ambiguous column reference* in
Postgres (the name exists on both the target and `excluded`), and a replacement that removed a
call left unbalanced braces. The first is now `message = drafts.message`; the second replaces the
event type rather than the call.

The one real survivor was a missing fixture: nothing drafted to an **archived** channel. That gate
is only reachable because Go fetches the channel with `Get(id, true)` — `allowFromCache`, not
include-deleted — so the read does not filter `DeleteAt` and an archived channel comes back.

### A literal route in axum does not fall through, and gorilla's does

`/preferences/delete` is registered `POST`-only in Go, so a `GET` falls past it onto
`{category:[A-Za-z0-9_]+}` and is answered by `getPreferencesByCategory` — with `delete` as the
category, which nobody has, so it is a 404. **axum has no fall-through**: a literal route claims
the path for every method, and registering `POST` alone handed the `GET` to the proxy.

No client could tell — Go's answer is the same 404 either way — but this server stopped *serving*
a read it used to serve, and `preference_reads::the_refusals_match_by_status_and_id` asserts
`x-mmrs-served-by: rust` on exactly that path. It failed on the first full-suite run after the
POST was added, which is the served-by header doing the job it was added for. The literal route
now answers the `GET` too, by delegating to the category handler.

The same change broke `preferences::an_unmigrated_method_on_a_migrated_path_still_reaches_go`,
whose probe *was* `POST /preferences/delete` — a route that is no longer unmigrated. It now probes
`DELETE /users/me/preferences`, a method Go does not register on a path this server does serve, so
it still tests the method fallback rather than this route.

### `max_draft_size` is a deployment artifact

`determineMaxDraftSize` reads `character_maximum_length` for `Drafts.Message` out of
`information_schema` and divides by four ("assume a worst-case representation of four bytes per
rune"). It is **16383** on this stack and 1000 on a server that never widened the column, so
hard-coding either would refuse messages Go accepts or accept ones it refuses. Queried, like the
tzdata lookup in `ScheduledPost`.

## The seven webhook writes: two near-identical families that disagree everywhere (2026-09-08)

New: `crates/mm-api/tests/parity/webhook_writes.rs`, `scripts/mutations/webhook-writes.plan`.
Changed: `crates/mm-store/src/webhook_store.rs`, `crates/mm-app/src/{webhook,config}.rs`,
`crates/mm-api/src/{webhooks,lib}.rs`, `crates/mm-api/tests/common/mod.rs`.

210 → **217 of 764**. Pure CRUD with no websocket events, which is what makes it a good group
after the reaction and draft ones: the interesting part is entirely in the *differences* between
the incoming and outgoing families, and they differ in almost every place they could.

| | incoming | outgoing |
|---|---|---|
| create answers | `201` | `201` |
| **update answers** | **`201`** | **`200`** |
| non-open channel | irrelevant | `api.outgoing_webhook.disabled.app_error` at **403** |
| empty trigger words | irrelevant | **400** on create, **500** on update |
| delete checks the channel | yes, via `restrictedChannel` | no |
| update checks the channel read | **only when not open** | no |

### One id, two statuses, in one function

`CreateOutgoingWebhook` uses `api.outgoing_webhook.disabled.app_error` for its feature gate at
**501** and again for a non-open channel at **403**. A client branching on the id alone cannot
tell "the feature is off" from "that channel is private".

The line below it is dead: `if channel.Type != Open || channel.TeamId != hook.TeamId` — the first
disjunct already returned two lines earlier — so the second error is reachable only through the
team mismatch. Ported as written, because the two ids differ.

### An update that omits `token` destroys it

`UpdateOutgoingWebhook` copies `CreatorId`, `CreateAt`, `DeleteAt`, `TeamId` and a fresh
`UpdateAt` off the old hook. **`Token` is not in that list**, and the store's `SET` writes it —
so editing a hook's display name without echoing the token back blanks the integration's
credential. Measured on the running server before it was asserted, and reproduced: a port that
preserved the token would answer a value Go does not have. `regen_token` is how you recover.

### The intersection check has no `DeleteAt` predicate, and it broke the suite

Two outgoing hooks collide when they share a channel **and** a callback URL **and** a trigger
word — all three. `GetOutgoingByTeam(-1, -1)` fetches every hook on the team **including deleted
ones**, so a trigger word freed by deleting a hook stays reserved for ever.

That is a Go behaviour worth knowing, and it is also why `webhook_writes` passed on its first run
and failed on its second: the first run's soft-deleted hooks blocked the second run's trigger
words. `purge_api_fixtures` now sweeps `mmrs%`-named hooks and hooks whose team or channel is
gone.

### The channel lock is not a refusal

Without `bypass_incoming_webhook_channel_lock` the hook is silently forced to
`ChannelLocked = true` and pinned to the channel it named — a `201` that is not what was asked
for. It is the only permission in this group that changes the *result* rather than refusing.

### The mutation run, and a test edit that silently did not apply

**23 run, 21 caught, 2 controls survived, 0 harness faults**, after two earlier passes. The three
real survivors were each a different kind of gap:

- **`trigger_words` absent and `trigger_words: []` are different inputs.** The guard is
  `trigger_words.as_ref().is_none_or(|w| w.is_empty())`, and a fixture that only *omits* the key
  short-circuits on the `None` — so replacing the emptiness test with `false` changed nothing it
  could see. Both spellings are now sent.
- **Two hooks in different channels never collide**, however much else they share, because the
  intersection loop's first test is the channel. Nothing exercised that until a fixture with two
  channels existed.
- **The incoming update's answer is built by the app layer, not read back**, so a mutation
  swapping `displayname` and `description` in the store's `SET` list was invisible. The test now
  reads the row.

The second of those cost an extra cycle for a duller reason worth recording: **a scripted edit to
the test file matched nothing and reported no error**, so the fixture the mutation was supposed to
catch never existed and the survivor looked like a puzzle about the code. The mutation was
reproduced by hand against a running server before the real cause was found. An edit that must
apply should assert that it did.

### Migrating a method retires the test that watched it be forwarded

Three `other_methods_are_forwarded` tests broke — `incoming_hooks`, `outgoing_hooks` and
`single_hooks` — because their probes were `POST /hooks/incoming`, `POST /hooks/outgoing` and
`DELETE /hooks/{incoming,outgoing}/{id}`, all now served here. Their *purpose* is that a method
this server does not register still reaches Go rather than meeting axum's 405, so each now probes
`PATCH`, which Go registers on neither path. The same repointing the preferences suite needed in
the previous commit; expect one per group from here on.

### `a_team_and_channel_the_user_is_in` returns a **direct message**

The first channel of the fixture user's first team is a DM: type `D`, `team_id` empty. Every
webhook permission here is team-scoped, so both servers answer 403 — agreement about the wrong
thing. The test that used it now creates its own open channel. Worth recording because that helper
is used by a dozen suites and the DM only matters where the team id does.

## The five OAuth app writes, and a decode that was stricter than Go's (2026-09-08)

New: `crates/mm-api/tests/parity/oauth_app_writes.rs`, `scripts/mutations/oauth-app-writes.plan`.
Changed: `crates/mm-model/src/{oauth,oauth_dcr}.rs`, `crates/mm-store/src/oauth_store.rs`,
`crates/mm-app/src/{oauth,config}.rs`, `crates/mm-api/src/{oauth,lib}.rs`.

217 → **222 of 764**: create, update, delete, `regen_secret`, and the Dynamic Client Registration
endpoint.

### `#[serde(default)]` is not decoration, and its absence is a 400 where Go answers 201

Go's `json.Decode` into a struct leaves an absent field at its **zero value**. A serde derive
without `#[serde(default)]` makes an absent field a decode **error**. `OAuthAppRequest` lacked the
attribute, so `POST /oauth/apps` with `icon_url` and `is_trusted` omitted — an ordinary body — was
a 400 here and a 201 on Go.

Three of the eight types this server decodes from a request body were missing it, all three in the
OAuth files; the other five already had it. Fixed, and the *class* is recorded as [D-192]: 126 of
`mm-model`'s deserializable structs have no `#[serde(default)]`, most harmless today because no
handler decodes them, each a landmine for the write route that first does. The failure mode is a
plausible 400, not a compile error.

### `is_public` is stored nowhere

It decides only whether a secret is generated, and the **emptiness of that secret** is what
`IsPublicClient` reads afterwards to refuse a regeneration. So the flag survives as the absence of
a value rather than as a column, and `regen_secret` on a public client is a 400 — giving it a
secret would silently convert it to a confidential client.

### The update copies the secret off the old app; the outgoing-webhook update does not

`UpdateOAuthApp` copies `Id`, `CreatorId`, `CreateAt`, **`ClientSecret`** and
`IsDynamicallyRegistered`, so a body cannot rotate a credential and `regen_secret` exists as its
own route. The webhook group two commits ago found the opposite: an outgoing-hook update that
omits `token` **destroys** it. Two adjacent CRUD families, opposite answers to the same question.

### `is_trusted` is cleared and preserved, never refused

Without `manage_system`, create clears it to `false` and update restores the old value — a `201`
or `200` for an app that is not what was asked for. The same "not a refusal" shape as the incoming
webhook's channel lock.

### The delete is a cascade, and its statement order matters

Four statements in one transaction: the app, then every `Sessions` row whose token appears in
`OAuthAccessData` for it, then the `OAuthAccessData` rows, then the `oauth_app` preference rows.
**The sessions go before the tokens they join against** — reversing them leaves every session
issued through the app alive, because the `USING` join has nothing left to match.

Go follows the delete with `InvalidateAllCaches()`. This server has no such cache and cannot reach
Go's, so a session issued through a deleted app stays live in Go's memory until it expires — the
sharpest instance of [D-190] so far, because the row really is gone.

### The create path's gate needed a second server to be visible at all

`EnableOAuthServiceProvider` is **`true`** on the shared stack, so none of the four feature gates
fires against `:8066` and a mutation swapping the create path's id for the other three's was
invisible — it survived the first run of this plan. The create answers
`api.oauth.register_oauth_app.turn_off.app_error`; update, delete and regenerate all answer
`api.oauth.allow_oauth.turn_off.app_error`. Same 501, one line apart in the Go source, two ids.

A `SecondServer` with the setting off reaches it, the same device `/recaps` and the gated families
use. Worth stating as a rule: **a configuration gate is untested until a server exists with the
setting on the other side**, which for this project nearly always means a second process.

### DCR is not an `AppError` route

`registerOAuthClient` takes **no session** ("Session and permission checks removed for DCR endpoint
to allow external client registration") and answers a **DCR error envelope** —
`{"error", "error_description"}` at 400 — never `c.Err`. A port reaching for `ApiError` would
answer the right status with a body no RFC 7591 client can parse.

`EnableDynamicClientRegistration` defaults to **false**, so a stock server's entire reachable
behaviour here is the decode and the two gates, and that is what is served. The decode runs
**first**, so a malformed body answers `invalid_client_metadata` even though the feature is off.
A deployment that enables DCR is forwarded, along with Go's 2/sec rate limit, which this server
has no equivalent of.

## Four team writes, and the write load finally broke the shared fixtures (2026-09-08)

New: `crates/mm-api/tests/parity/team_writes.rs`, `scripts/mutations/team-writes.plan`.
Changed: `crates/mm-model/` (23 files — see below), `crates/mm-store/src/team_store.rs`,
`crates/mm-app/src/{team,config}.rs`, `crates/mm-api/src/{teams,lib}.rs`, and four existing parity
suites.

222 → **226 of 764**: `updateTeam`, `patchTeam`, `restoreTeam`, `regenerateTeamInviteId`.

### `Sanitized: true` means the submitted team is not what gets written

`teamService.UpdateTeam` fetches the stored team and copies **seven** fields onto it —
`DisplayName`, `Description`, `AllowOpenInvite`, `CompanyName`, `AllowedDomains`,
`LastTeamIconUpdate`, `GroupConstrained` — plus `Name`, conditionally. Everything else a client
sends is discarded, **including `Email`, `Type`, `InviteId`, `DeleteAt` and `SchemeId`**. A port
that wrote the submitted struct would let a client change a team's type or un-archive it through
an ordinary update; the parity suite plants all four and asserts each is ignored.

The name is taken only when it is non-empty, changed, **and not `-`** — a sentinel meaning "leave
it alone" — and an occupied name answers with the *old* team at 400.

### Turning open invitations off mints a new invite id

`patch.AllowOpenInvite != nil && !*patch.AllowOpenInvite` regenerates `InviteId`, invalidating
every invite link already handed out. Nothing in the request names it. Turning them *on* does not,
so the branch is on the value rather than on the change — asserted both ways.

### The socket carries less than the response

`sendTeamEvent` runs `Team.Sanitize`, which clears `Email` and `InviteId`; the HTTP answer runs the
**session-aware** `SanitizeTeam`, which leaves them for a caller with `manage_system`. So
`regenerate_invite_id` publishes an event whose team has **no invite id** — the one thing the route
exists to produce. Both halves asserted.

### The conditional permission is decided differently by the two routes

`invite_user` is required by `updateTeam` when `AllowOpenInvite` or `AllowedDomains` **differ from
the stored team**, and by `patchTeam` when they are merely **present in the patch**. So
`{"allow_open_invite": <the value it already has>}` needs the permission on one route and not the
other.

### `#[serde(default)]`, swept

[D-192] bit a third group running: `Team` and `TeamPatch` lacked the attribute, so
`PUT /teams/{id}` with a partial body was a 400 here and a 200 on Go. Rather than wait for a
fourth, the attribute is now on **every** `Deserialize` struct in `mm-model` — 90 of them across
23 files. The one exemption is `Permission`, a static descriptor with no `Default` that no handler
decodes.

The sweep cannot break a serialisation test: the attribute affects decoding only, and every
fixture round-trip decodes a fully populated document where defaults never apply. All 1,437 model
tests pass unchanged.

### Every stock team role grants `invite_user`

Which is why three separate mutations of the conditional permission survived the first run: an
ordinary member of the team already holds it, however narrow the *system* role is. Separating
`manage_team` from `invite_user` needs a **roleless team membership**, planted directly —
`POST /teams/{id}/members` always writes `team_user`.

With that fixture the sharpest difference between the two routes becomes visible: naming
`allow_open_invite` with the value it already has is a **403 on `patchTeam`** and a **200 on
`updateTeam`**, because one decides by presence and the other by change.

Three further mutations are recorded in the plan as **equivalent** rather than as gaps: the
store's `create_at`/`update_at` assignments are redundant given what the app layer has already
done (Go carries the same redundancy), and `invalid_param("id")` versus `"team_id"` is invisible
because `AppError.Params` is `json:"-"` and never reaches a client.

### Four existing suites had to be made churn-proof, and that is this group's real cost

Adding write routes to a suite that shares one database with a running Go server changes what the
*read* suites can assume. Four broke, each differently, and none of them was a port divergence:

| suite | why it broke | fix |
|---|---|---|
| `teams_all` | its fixture refuses to build when two teams share a display name — and this suite renamed both of its teams to `"mmrs renamed"`. The panic left its `OnceCell` half-built, so the next test retried the build and collided on the team *name*, taking out ten tests | distinct display names per server |
| `session_team_members` | compared a membership **count** across two instants; creating a team makes the creator a member, and this suite creates eight | compare the **intersection** — every team both reads saw must agree in every field, with near-total overlap required |
| `session_activity` | `/users/me`'s etag folds in `Users.UpdateAt`, shared with every suite, so the conditional read became a 200 | re-read the etag and retry, re-planting the aged activity each time |
| `user_audits` | `Audits` is append-only and every Go-served write in another test adds a row to the admin's list | compare the intersection, as above |

The pattern is one rule: **an assertion over a whole shared table cannot survive a suite that
writes**. Count it, and you are counting the rest of the run. [D-167] predicted exactly this and
asked for the deliberate pass; four of them are now done, driven by failures rather than by the
sweep it asked for.

## `PUT /users/{id}/status` — the first route whose model is a cache (2026-09-08)

New: `crates/mm-api/tests/parity/status_writes.rs`, `scripts/mutations/status-writes.plan`.
Changed: `crates/mm-store/src/status_store.rs` (`get`, `save_or_update`),
`crates/mm-app/src/{status,config,lib}.rs`, `crates/mm-api/src/{status,lib}.rs`,
`crates/mm-api/tests/common/mod.rs` (`status_row`).

**227/764.** One route, four setters, and a design decision that had been open since the read side:
[D-191] is now closed in favour of giving `mm-app` its own `statusCache`. Three decisions inside
`SetStatusOnline` branch on the *previous* status, and the previous status is not the `Status` row
— it is whatever the process last cached. A port that read the table would take a different branch
from Go on exactly the requests that matter, and would write rows Go throttles away. While both
servers run the two caches are independent, so the parity suite drives and reads **each server
through itself**; the divergence ends when Go stops.

Two surfaces the wire cannot show, so the suite reads the row directly: `manual` is on the wire but
`prev_status` carries `json:"-"`, and `dnd_end_time` is **seconds** — the only timestamp in the
package that is — floored to a whole minute by `truncateDNDEndTime`.

**The guards the route cannot reach.** Every caller passes `manual: true` and `force: false`, so
the manual-override guard in `SetStatusOnline`/`SetStatusOffline`, all three "IfNeeded" conditions,
and the `StatusMinUpdateTime` throttle are unreachable over HTTP. They were extracted into
`away_is_needed`, `online_row_needs_writing` and `is_user_away` and are mutated against unit tests
instead — the alternative was shipping a third of the ported logic unexamined.

Leaving out-of-office is **forwarded**: it needs `DisableAutoResponder`, which is not ported. The
answer is `getUserStatus`'s own body, not `{"status":"OK"}`.

### Two more shared-fixture failures, and the first one inside a single suite

Both surfaced in the full run for this group and neither is about statuses.

`threads_for_user::per_page_limits_the_list_and_not_the_totals` asserts `total == 2` while
`a_thread_in_a_channel_the_caller_left_is_excluded` re-inserts a channel membership, checks the
thread reappears, and deletes it again. Inside that three-statement window the fixture user follows
**three** threads, so the count is correct for the instant it was read and wrong for the assertion.
This is [D-167]'s rule turned inward: it is the first instance *within* one suite rather than
between two. Fixed with a module `Mutex` over the three tests that assert an exact count — narrower
than a second fixture user, because the window is three statements wide.

`users_group_channels` failed on the admin's `Users.UpdateAt` moving between the Go read and the
Rust read. `post_both_raw` had no quiescence bracket at all — `fetch_both_stable_within` has had one
since the schemes suite, and every POST comparison in the tree was still doing a single unbracketed
pair. `post_both_raw_stable` is that bracket for POSTs, now used at all ten call sites. Any suite
whose answer embeds a `User` needs it; the route was never in question.

## `UpdateUser`, and the four custom-status routes it was blocking (2026-09-08)

New: `crates/mm-api/tests/parity/custom_status_writes.rs`,
`scripts/mutations/custom-status-writes.plan`.
Changed: `crates/mm-model/src/user.rs` (`UserUpdate`), `crates/mm-store/src/{error,user_store}.rs`,
`crates/mm-app/src/{user,status,preference,config}.rs`, `crates/mm-api/src/{status,lib}.rs`,
`crates/mm-api/tests/common/mod.rs` (`user_props`).

**231/764, and `api4/status.go` is complete at 7/7.** A custom status is not a `Status` row — it
lives in `Users.Props["customStatus"]` as a JSON string — so all four routes were blocked on
`UpdateUser` rather than on the status cache, and porting it is what this group is really about.

**`UserStore::update` is the security boundary of every update route in the server.** Thirteen
fields are copied from the stored row onto the submitted user, and two more (`Roles`, `DeleteAt`)
when the caller is untrusted, which every api4 caller is. Without them a client could set its own
password hash, mark its own email verified, clear its own failed-login count, turn off its own MFA,
grant itself a role, or un-deactivate its own account by naming the field in a request body. Those
copies are the first eleven mutations in the plan.

`sendUpdatedUserEvent` publishes **three** `user_updated` events, not one: an admin copy flagged
`ContainsSensitiveData`, a member copy flagged `ContainsSanitizedData`, and — because both omit the
subject — the subject's own copy, sanitised with `Sanitize(nil)` rather than `SanitizeProfile`, so
it keeps the profile fields the other two strip. The hub already addressed both flags.

**[D-089] is closed.** `UpdatePreferences` was the last route still carrying the original "a write
served here publishes no WebSocket event" gap; its `sidebar_category_updated` and
`preferences_changed` now go out, with `preferences` as a JSON **string** as Go sends it. Its
sibling `DeletePreferences` had published its pair since it was written, and that asymmetry inside
one route was the thing worth fixing.

Not ported, and named rather than guessed at: the three background mails, `UpdateDefaultProfileImage`
on a username change (needs the image pipeline — the consequence is a stale initials avatar), and
three in-process caches this server does not have.

### Sixteen survivors, one cause — and the store is where a boundary gets pinned

The first run of `custom-status-writes.plan` was **19 caught, 16 survived, 2 harness faults**. The
survivors were not sixteen findings; they were one. `UserStore::update`'s field copies are the
security boundary of every update route, and **no ported route lets a client submit a `User`** —
the custom-status routes write `Props` and nothing else, so deleting the password copy, the roles
copy or the MFA copy is invisible over HTTP. A parity suite cannot reach them and no fixture in it
ever will.

They now run against a new `crates/mm-store/tests/db_user_update.rs`, which hands the store a
deliberately poisoned `User` — the layer that *can* be given one. Writing it found three places
where the expectation was wrong and the code was right:

| expected | actual |
|---|---|
| a fixture user may carry both `auth_data` and a password | `IsValid` runs **before** the copies and refuses that pair (`auth_data_pwd`); `authdata` is uniquely indexed too |
| `UpdateMentionKeysFromUsername` adds the new username | it only **removes** the old one, and leaves a leading comma — `",keepme"` |
| a gitlab user's email change clears verification | gitlab is an **OAuth** service, so the email is pinned before the clear is reached; SAML is the SSO-but-not-OAuth case |

Two of the sixteen were faults in the plan rather than gaps in the tests, and both are worth
naming. The gate-ordering mutation moved the decode *expression* above the gate but not the
refusal it guards — a genuine no-op that looked like a missing test for twenty minutes. And two
mutations deleted a sqlx bind parameter, which breaks compile-time type inference rather than the
code: **a mutation must keep every parameter it uses**, so swap two same-typed columns instead.
The same trap the team-writes plan hit.

Re-run: **34 run, 32 caught, 2 controls survived, 0 harness faults.**

## The three job reads, and an empty list that is `null` on one route and `[]` on the next (2026-09-08)

`GET /api/v4/jobs`, `GET /api/v4/jobs/{job_id}` and `GET /api/v4/jobs/type/{job_type}` are served.
New: `crates/mm-store/src/job_store.rs`, `crates/mm-app/src/job.rs`, `crates/mm-api/src/jobs.rs`,
`crates/mm-store/tests/db_job_data_filter.rs`, `crates/mm-api/tests/parity/jobs.rs`,
`scripts/mutations/jobs.plan`. `mm-model/src/job.rs` was already ported and needed no change.

### The empty answer depends on a slice initialiser three layers down

Four Go store methods build the same query and differ only in how they declare the destination:
`var jobs []*model.Job` marshals as `null`, `jobs := []*model.Job{}` marshals as `[]`. Measured on
the running server, not inferred — `?job_type=data_retention` gives `null` and
`/type/data_retention` gives `[]`, same database, same zero rows. Adding `?status=success` moves
`getJobs` to the *third* method and it answers `[]`. A `Vec` cannot carry that, so the decision is
an explicit `nil_when_empty` flag at the API edge (`mm_api::jobs::encode_jobs`); the store trait
documents which side each method is on.

### `SessionHasPermissionToReadJob` returns a nil permission, and that is a 400

An unknown job type produces `(false, nil)`, and every caller branches on the **nil**, answering
`api.job.retrieve.nopermissions` with **400** — not a 403. Modelled as
`mm_app::job::ReadJobPermission`, an enum, so the case cannot be destructured away into a bool.
`scheduled_recap` is the interesting instance: it is in `AllJobTypes`, so it passes
`IsValidJobType` and still lands on the 400.

### Four survivors, four different fixture gaps

First run **21 run, 15 caught, 4 real survivors**. None were shrugs:

| survivor | why it survived | fix |
|---|---|---|
| the two `page_in_memory` mutations | that branch needs an access-control job, which needs a licence — no HTTP request on Team Edition reaches the sort | re-pointed to the `unit` suite, where the tests already existed |
| `data_retention` → `read_jobs` | every stock role granting one grants the other, so both answers coincide | a planted role holding `read_jobs` alone (`parity::jobs::read_jobs_does_not_open_the_types_with_their_own_permission`) |
| the JSONB data predicate | `Jobs` holds no row with a `team_id` in its `Data`, so a broken filter and a correct one both return `[]` | `db_job_data_filter.rs`, which plants the rows |

And a fifth that was the harness, not the code: `MUTATE_FILTER=db_job_data_filter` is a **file**
name, so cargo ran zero tests and reported SURVIVED — the trap `mutate.sh`'s own header warns
about. The tests are named `job_data_filter_*` now so the filter can select them.

Re-run: **24 run, 20 caught, 4 controls survived, 0 harness faults.**

## The forward target is now built from the pinned SHA (2026-09-08)

`docker compose up -d` starts Postgres alone; `scripts/go-server.sh start` builds
`reference/mattermost/server` and runs it on :8065. The published `11.11.0-rc1` image stays in
`docker-compose.yml` behind a `published-image` profile as the documented fallback. This closes
[D-167] — see that entry for what it cost and what it did **not** fix.

Three things a later session should not have to rediscover:

1. **`go build` fails in the reference tree for a reason that looks like a broken checkout.**
   `server/go.mod` requires the *published* `server/public v0.4.0`, so every `model.` symbol added
   since that release is undefined. `scripts/go-server.sh` writes a `go.work` outside the tree.
2. **`GET /api/v4/bots` no longer carries `system_owned`**, and `api4/properties.go`'s routes are
   registered. Those were the two measured disagreements between the source and the image.
3. **`api4/view.go` and `api4/channel_join_request.go` are behind feature flags**, not behind the
   version skew — `IntegratedBoards` and `DiscoverableChannels`, both off at the pinned SHA. That
   is fourteen routes `scripts/routes.py` counts and the server does not serve.

### The suite is six times faster, and that broke six tests

`--test parity` went from 244s to 38s once the target stopped running under qemu. Six tests then
failed intermittently on unchanged code, and every one was a latent race the emulator had been
hiding:

| shape | fix |
|---|---|
| counting `user_updated`/`preferences_changed` frames on the **shared admin**, while a sibling test writes to the same user | `common::BROADCAST_STREAM`, a mutex every broadcast-counting test holds |
| waiting a fixed 600–1200ms for an event that now arrives on a different schedule | `SocketProbe::collect_until`, which waits for the frame and then briefly for a second one |
| `GET /users/me/sessions` embedding `team_members` from Go's **session cache** while we read the table | compare everything but `team_members`, then assert Go's set is a **subset** of ours — being ahead is [D-087] working as decided, being behind would be a real divergence |

Eight consecutive full runs are green.

## The two bot reads, and the 404 that is a security answer (2026-09-08)

`GET /api/v4/bots` and `GET /api/v4/bots/{bot_user_id}` are served — the first route this project
recorded as **blocked** and then unblocked. New: `crates/mm-store/src/bot_store.rs`,
`crates/mm-app/src/bot.rs`, `crates/mm-api/src/bots.rs`,
`crates/mm-store/tests/db_bot_store.rs`, `crates/mm-api/tests/parity/bots.rs`,
`scripts/mutations/bots.plan`. `mm-model/src/bot.rs` was already ported.

### A refused read and a missing bot are the same document

`getBot` answers `store.sql_bot.get.missing.app_error` — a **404** — to a caller who may not read
the bot, built by the same `MakeBotNotFoundError` a miss uses. Go's comment: "pretend like the bot
doesn't exist at all, to avoid revealing that the user is a bot." A 403 here would leak exactly
what the 404 hides. `getBots`, one function away, answers a plain 403 — there is no id in the
request to confirm or deny. Both are asserted on the *bodies*, not the statuses.

### The `ETag` is written only on the 304

`HandleEtag` (web/context.go:230) sets `HeaderEtagServer` inside the `if et == etag` branch and
nowhere else, so a 200 from either route carries no `ETag` at all. Measured, because the intuitive
reading is the opposite.

### Go builds eight statements; the port builds one

`GetAll`'s three options are assembled into the `WHERE` at runtime, and `OnlyOrphaned` adds a
second `JOIN Users`. `query_as!` checks a literal, so they are parameters here — with two rewrites
that are exact rather than approximate: an absent clause becomes a satisfied one, and the inner
join becomes a left join plus `o.Id IS NOT NULL`. The mutation run then proved the null check is a
**no-op**: `o.DeleteAt <> 0` is already NULL for a plugin-owned bot, so three-valued logic excludes
it either way. Kept for intent, recorded as a control.

### Six survivors, one cause

First run **22 run, 14 caught, 6 real survivors**. Five were the same gap: **no stock role holds
`read_bots` without `read_others_bots`**, so every branch that tells them apart is unreachable over
HTTP — including one mutation that let any caller read any bot. No REST route can create a deleted
bot or one owned by someone else either. A planted role and two planted bot rows fix all five; the
sixth was the SQL no-op above.

Re-run: **22 run, 19 caught, 3 controls survived, 0 harness faults.**

## The eight remaining `group.go` reads: ten routes, one answer (2026-09-08)

Every `GET` in `api4/group.go` is now served. Modified: `crates/mm-api/src/groups.rs`,
`crates/mm-api/src/lib.rs`, `crates/mm-api/tests/parity/groups.rs`,
`scripts/mutations/groups.plan`. No store or app work — `requireLicense(c)` is the **first
statement** of all ten handlers, checked one by one, so on an unlicensed server the whole family
collapses to the generic `api.license_error` at 501.

The only thing a port can get wrong here is which requests reach the gate, so that is what the
suite pins:

- **`{syncable_type:teams|channels}` is an alternation of literals, not a character class.**
  `/groups/{id}/teams` is a 501 and `/groups/{id}/team` is gorilla's mux 404 — two answers to what
  looks like one route. The handler carries the charset and forwards the miss; `routes.py` keeps
  counting it as one parameterised route because the pattern holds a `|`.
- **`members` and `stats` are literal siblings** of `{syncable_type}` and must not be read as
  syncable types. axum prefers the literal, which is the order gorilla registers them in.

Two second gates are named and not ported: `getGroupStats` and
`getGroupsAssociatedToChannelsByTeam` consult `License().Features.LDAPGroups` and answer
`api.ldap_groups.license_error` at **403** — reachable only with the licence that makes the whole
route forward.

Mutations: **9 run, 7 caught, 2 controls survived.**

## Ten reads that refuse before they read anything (2026-09-08)

`hosted_customer.go`, `license.go`, `saml.go`, `ldap.go`, `system.go`,
`custom_profile_attributes.go`, `user.go`, `outgoing_oauth_connection.go` (×2) and `job.go` — ten
`GET`s across eight files, all served. New: `crates/mm-api/src/gated_reads.rs`,
`crates/mm-api/tests/parity/gated_reads.rs`, `scripts/mutations/gated-reads.plan`; three fields
added to `mm_app::config::Config`.

`licensed_features.rs` holds the routes whose licence test is the handler's first statement. These
are the next shape along: the gate always fires, but it is not always first, not always the
licence, and **not always the same status** — three of the ten answer 403 where the family's
convention is 501, and `api.ldap_groups.license_error` appears at 501 here and at 403 in
`group.go`. Neither the id nor the status can be inferred from a neighbour, which is the reason
they are one module with one table.

Two of them take **no session at all** — `getSamlMetadata` and `getSessionAttributesManifest` are
`APIHandler`, not `APISessionRequired` — so an anonymous request gets the refusal where the other
eight get a 401. Adding an extractor "for consistency" would turn a 501 into a 401.

### The config fixture had drifted, and the test that should have caught it agreed with the drift

`scripts/dump-config-fixture.sh` projected **six** sections and seventeen keys while `Document` had
grown to fourteen sections and thirty-eight, so eight sections of Go's own output were never
compared against anything —
`the_fixture_covers_every_document_sourced_setting` asserted a hardcoded `17` rather than the
struct's own shape. The list is now the struct's keys and the count is 40. Regenerating found one
real difference and it is benign: `AIRecapSettings.SetDefaults` writes `Enable = true`, so a
persisted document carries `Some(true)` where `Config::default()` carries `None` — and
`IsEnabled()` treats absent as enabled, which is why the field is an `Option`.

### A hand-planted fixture row was making **Go** answer 500

`mm-app`'s `db_authorization_by_post` seeds a team without `LastTeamIconUpdate`, so the column was
NULL — and `SqlTeamStore.GetAllPage` scans it into an `int64`. For as long as that row existed,
`GET /api/v4/teams` was a 500 **on the Go server**, and twelve parity tests in two unrelated suites
failed on "should return 200". It surfaced only because the two binaries ran in a different order
than usual. The column is written now, and `purge_api_fixtures` sweeps the prefix.

Mutations: **20 run, 17 caught, 2 controls survived** after re-pointing three that were measuring
the wrong suite — two config defaults that only the config oracle can see, and the
restricted-admin check, whose branch needs `ExperimentalSettings.RestrictSystemAdmin` and is
therefore mutated at the app layer where a test can plant it.

## The four personal-access-token reads (2026-09-08)

`GET /api/v4/users/tokens`, `/users/tokens/non_compliant/count`, `/users/tokens/{token_id}` and
`/users/{user_id}/tokens`. New: `crates/mm-store/src/user_access_token_store.rs`,
`crates/mm-app/src/user_access_token.rs`, `crates/mm-api/src/tokens.rs`,
`crates/mm-app/tests/db_user_or_bot.rs`, `crates/mm-api/tests/parity/user_access_tokens.rs`,
`scripts/mutations/user-access-tokens.plan`; `SessionHasPermissionToManageBot` and
`SessionHasPermissionToUserOrBot` ported into `mm-app`, and one config field.

### Four routes, four different permission rules

Nothing is shared. The two installation-wide reads want `manage_system`; the two token-scoped ones
want `read_user_access_token` **and then** `SessionHasPermissionToUserOrBot` — and on
`getUserAccessToken` that second check runs **after** the fetch, on the token's owner, so a caller
with the first permission and not the second gets 404 for an id that does not exist and 403 for one
that does.

### "Or bot" is resolved by reading a failure, and only one failure counts

`SessionHasPermissionToUserOrBot` tries the bot path and falls through to the user check **only**
on `store.sql_bot.get.missing.app_error` from `SqlBotStore.Get`. A refusal that *hides* an existing
bot carries the same id with `where` = `permissions` and must not fall through — otherwise a
caller holding `edit_other_users` reads every bot's access tokens. Matching on the id alone would
let that through.

**The parity suite could not see that, and the mutation run proved it.** Over HTTP both answers are
the same 403, because the route refuses either way when the caller cannot reach the bot. The test
moved to `crates/mm-app/tests/db_user_or_bot.rs`, where the function returns a `bool`; the parity
test stays, because it is what proves the routes consult the function at all.

### The count route reads nothing

`MaximumPersonalAccessTokenLifetimeDays` defaults to 0, `maxUserAccessTokenExpiry` returns
`(0, false)` for anything `<= 0`, and the caller returns 0 **before** the query — so
`{"count":0}` is the answer even with a never-expiring token planted, which is what the fixture
plants to prove it.

### The rows had to be planted

`POST /users/{user_id}/tokens` needs `ServiceSettings.EnableUserAccessTokens`, which is off, so
`UserAccessTokens` is empty on this deployment and every route answers `[]` or `{"count":0}`.
Planted rows carry a **distinct secret per fixture** — the column is uniquely indexed — and the
suite asserts that literal appears in no response body.

Mutations: **22 run, 20 caught, 2 controls survived** after re-pointing the two user-or-bot lines
to the app suite and adding a fixture that separates `manage_system` from `read_user_access_token`
(no stock role does).

**Churn:** `parity/user_get.rs`'s "literal siblings are forwarded" list lost `tokens`, which is now
ours. That list has been shrinking with each migration and is down to one entry.

## `getCommand`, and half of `listCommands` (2026-09-08)

`GET /api/v4/commands/{command_id}` is served in full; `GET /api/v4/commands` is served for
`custom_only=true` and forwarded otherwise. New: `crates/mm-store/src/command_store.rs`,
`crates/mm-app/src/command.rs`, `crates/mm-api/src/commands.rs`,
`crates/mm-api/tests/parity/commands.rs`, `scripts/mutations/commands.plan`; one config field.

### Every one of `getCommand`'s four failures is the same 404

No such command, no `view_team`, no `manage_own_slash_commands`, or neither the creator nor a
holder of `manage_others_slash_commands` — all four are `SetCommandNotFoundError`, and Go's comment
says why: a 403 would tell a caller that a command id is real and which team it belongs to. The id
is `store.sql_command.save.get.app_error` — a store **save** id on a read, built by the handler,
and it does not match the id `App.GetCommand` produces, which is unreachable through this route.

### The forwarded half, and why

Without `custom_only` the handler merges the **built-in** slash commands — a registry in
`app/slashcommands/` with translated display names — with plugin-registered ones. Neither is
derivable from the database. Everything before that branch is served: the missing `team_id` (a
**body**-param error for a query parameter), the `view_team` gate, and the
`manage_own_slash_commands` gate `custom_only` adds. The forwarded branch is compared as a **set**:
`ListAllCommandsByUser` ranges over a Go map, so two consecutive reads of the same server disagree
byte for byte.

### Eight survivors, one mistake

First run **21 run, 11 caught, 8 real survivors**, and all eight were the same fixture error: the
refusal test used a caller who failed *every* gate, so removing any one left another to refuse, and
the list test's team held nothing the store's predicates were meant to exclude.
`each_gate_refuses_on_its_own` now uses three callers that each fail exactly one gate — with a
positive control for each — and the list fixture carries a soft-deleted command and one in another
team. Re-run: **21 run, 19 caught, 2 controls survived.**

**Churn:** `custom_status_writes`'s `preferences_changed` count is now scoped to the
`recent_custom_statuses` category. Three other suites write preferences for the shared admin, and
with the native forward target they now overlap; the sibling `sidebar_category_updated` cannot be
scoped at all — its data map is empty by Go's own TODO — so it is asserted as "at least one".

## Four more gated reads, and one of them is a 200 (2026-09-08)

`GET /api/v4/files/{file_id}/link`, `/api/v4/cloud/preview/modal_data` and
`/api/v4/license/load_metric` join `mm_api::gated_reads`; two config fields added.

`load_metric` is the odd one and the reason it is worth a note: unlicensed, `licenseUsers` is 0,
the `if licenseUsers > 0` guard is never taken and the metric keeps its zero — so the body is
`{"load":0}` **without a database read**. It is the member of the family most likely to be served
unconditionally by mistake, because the answer looks like a constant; a mutation that did exactly
that survived until the licence-boundary test learned to include it.

### `/files/{file_id}/public` is not served, and its sibling is

Same gate, same error id, and a completely different response: the public route's path is outside
`/api/`, so `web.Handler` renders a signed HTML redirect page rather than a JSON `AppError`. Its
403 carries an ECDSA signature made with the server's `AsymmetricSigningKey`. See [D-170].

Mutations: **28 run, 25 caught, 2 controls survived**, after one survivor that was a missing entry
in the licence-boundary list rather than a gap in the code.

Also recorded this session: [D-241] (the built-in slash-command registry, three routes) and
[D-171] (five routes that read in-process state this server does not share).

## `getInviteInfo` — the only unauthenticated route that returns data (2026-09-09)

`GET /api/v4/teams/invite/{invite_id}`. New: `team_store::get_by_invite_id`,
`App::get_team_by_invite_id`, `teams::get_invite_info`,
`crates/mm-store/tests/db_team_invite_id.rs`, `crates/mm-api/tests/parity/invite_info.rs`,
`scripts/mutations/invite-info.plan`.

`APIHandler`, not `APISessionRequired` — the join-by-link page shows a team's name to someone who
has not signed in. The two other sessionless routes this server answers are both refusals, so this
is the one place where adding a session extractor "for consistency" would break a sign-up flow.

Three things carry it:

- **A closed team is a 403 and an unknown invite a 404**, so the pair distinguishes "no such
  invite" from "that invite is for a team you may not see this way". The invite id goes into
  `detailed_error`, which `WipeDetailed` blanks.
- **The body is four fields in an anonymous struct**, not a `Team` — so `email`,
  `allowed_domains` and the invite id itself never leave the server, not by sanitisation (this
  route calls none) but because they were never in the struct.
- **`Teams.InviteId` is not unique.** Go guards with `inviteId == "" || team.InviteId != inviteId`,
  and the empty half is load-bearing: rows with an empty invite id exist — Go ships a
  `GetByEmptyInviteID` for them — so without it an anonymous caller would get an arbitrary team.
  No HTTP request can reach that guard (an empty path segment is not a route), so it is tested at
  the store: `db_team_invite_id.rs`.

Mutations: **11 run, 9 caught, 2 controls survived** — after a first run with a **harness fault**
(a mutation that added a struct field without an initialiser and did not compile; a fault voids the
tally) and one survivor that moved to the store suite.

**Churn:** `channels_for_team_for_user` now compares a channel page with
`assert_same_channels`, which falls back to a set when the bytes differ. Direct-message channels
all have an empty `DisplayName`, so any two of them are tied under `ORDER BY DisplayName` and the
two servers may order them differently — and every plain user a suite creates leaves one more DM
behind, so this session's fixtures made a latent tie into a recurring flake.

### The last recurring flake, and it was not a tie

`reaction_writes::the_unique_emoji_limit_counts_distinct_undeleted_emoji` failed roughly one run in
five with `cake should be accepted below the limit` — which reads as an off-by-one in the fifty-emoji
count and is not one. The body said `app.post.get.app_error` 404: the **post had been deleted**
mid-test. `a_team_and_channel_the_user_is_in` returns the *first* channel the admin is in, which is
whichever channel some other suite created most recently, and a fifty-one-request test is open long
enough for that suite's fixtures to be swept underneath it. The test now creates its own team and
channel. Six consecutive full runs green afterwards.

`users_stats_filtered` moved to `fetch_both_stable` for the same class of reason: it is a **live
count of users**, and a plain Go-then-Rust pair compares two different instants, so a
`create_plain_user` landing between the two reads is a difference of one. Five consecutive full
runs green.

## The local file backend, and 21 routes on it (2026-09-09)

`platform/shared/filestore`'s **local** driver is ported, along with the app-layer wrappers and
`web.WriteFileResponse` — which is where almost all of the work is. 264 → **285 of 764**
route+method pairs.

| route | notes |
|---|---|
| `GET`/`HEAD /api/v4/files/{file_id}` | `GetByIds`, not `GetFileInfo`: sees deleted rows, skips the mini-preview repair, and 404s a deleted file with the same id it uses for an absent one |
| `GET`/`HEAD .../thumbnail`, `.../preview` | both claim `image/jpeg`; an empty path is a **400** with `file_id=` in the detail, not a 404 |
| `GET`/`HEAD /files/{file_id}/public` | the success path only — every failure renders signed HTML ([D-170]) and is forwarded |
| `GET /api/v4/users/{user_id}/image` | served when the stored `profile.png` reads; the generated-initials avatar is forwarded ([D-204]) |
| `GET /api/v4/teams/{team_id}/image`, `/emoji/{id}/image`, `/brand/image` | three images, three different cache headers, three different permission rules |
| `DELETE /api/v4/brand/image` | a missing image is a **404** here and a **success** on the two archive deletes |
| `GET /exports`, `GET`/`DELETE /exports/{name}`, `POST .../presign-url` | presign-url always refuses at the feature flag, with Go's `eport` typo on the wire |
| `GET /imports`, `DELETE /imports/{name}` | the import listing filters `.tmp`; the export listing does not |
| `GET /api/v4/uploads/{id}`, `/users/{id}/uploads` | `UploadSession` rows, no file backend involved |

Behavioural oracle: `fixtures/behaviour_filestore.json`, generated by driving the real
`LocalFileBackend`, `web.WriteFileResponse` and `image.DecodeConfig` through `httptest` and a
temporary directory.

### The finding that matters most is not in any of these routes

**No parity test had ever compared a response header.** `fetch_both` asserts bodies, and every
suite before this one used it. Go's `web.Handler.ServeHTTP` sets `Permissions-Policy`,
`Referrer-Policy`, `X-Content-Type-Options` and — for `GET` only — `Expires: 0` on every API
response, and the gzip wrapper adds `Vary: Accept-Encoding`; **all 264 previously migrated pairs
were shipping without them**, byte-identical in the body and materially different on the wire.
`mm_api::go_global_headers` is the fix and it applies to every route this server answers. The new
suite compares every header in both directions, which is what found it on its first run.

### Three things `http.ServeContent` does that reading the handler will not tell you

1. **`contentSize` does not set `Content-Length`.** `WriteFileResponse` sets it and `ServeContent`
   overwrites it from the seek size, so the parameter survives only in `gzip` mode as
   `X-Uncompressed-Content-Length`. `getFileThumbnail` passing `0` changes nothing.
2. **A 416 loses `Cache-Control`, `Last-Modified` and `Accept-Ranges`** — the first two to
   `serveError`, the third because the line that sets it is *below* the range handling.
3. **The Unix epoch is "no modification time" to `setLastModified` and a real date to
   `checkIfRange`.** One value, two meanings, four call sites.

### Two readings that were wrong, and the oracle is why they did not ship

`os.IsNotExist` maps **ENOENT only** — not ENOTDIR, which the first draft of `filestore.rs` said
it did. And `RequireUserId` substitutes `me` **before** validating, so `/users/me/uploads` is the
caller's own list; the first version of `uploads.rs` answered 400 to a request Go answers 200,
and the parity suite caught it.

Mutation testing: **38 run, 36 caught, 2 controls survived, 0 harness faults**
(`scripts/mutations/file-backend.plan`). One real survivor, closed by a corpus case: no range in
the corpus began at byte `size` exactly, where `parseRange`'s `start >= size` and a loosened
`start > size` give a 416 and an empty 206.

### The suite lost eight tests to a row another binary planted

`common_teams` failed on `POST /channels/group` with `app.user.get_profiles.app_error`, which has
nothing to do with the route it covers: `mm-app`'s `db_authorization` suite plants a `Users` row
with a NULL `LastPictureUpdate`, Go's `sqlx` scan cannot take it, and **which suite pays is a
matter of execution order** — adding this session's suite moved it. `users_list` already carried
the repair; it now runs inside `purge_api_fixtures`, before any fixture exists.

## Where the read routes stand (2026-09-09)

**33 of the 106 unserved `GET`/`HEAD` pairs on the HTTP router were migrated this session**, taking
the inventory from 231/764 to 264/764. `scripts/routes.py --todo | grep -E '^(GET|HEAD)'` is the
live number; it reads **73**.

The 73 are not one queue. They fall into four groups, and the next session should pick from the
first:

**Ordinary work (~30).** ~~`report.go` (2)~~ *— done 2026-09-09, see below*, `post.go` (2),
`channel.go` (4), `team.go` (4),
`user.go`'s `auth_data`/`invalid_emails`/`uploads` (3), `properties.go` (3) and
`custom_profile_attributes.go` (2) behind one property service, `access_control.go` (4),
`shared_channel.go` (1), `saml/certificate/status` (1), `config.go` (3), `api.go`'s `/manualtest`
(1). Nothing here is blocked; it is the same shape as the nine units above.

**Blocked on a subsystem, and the subsystem is buildable (~20).** *Closed 2026-09-09* — the local
file backend landed and took 21 route+method pairs with it (see the entry above). What is left of
this group is `GET /api/v4/users/{id}/image/default` and the fallback half of
`/users/{id}/image`, both of which need initials-avatar rendering ([D-204]), `image.go`'s proxy
route, and `POST /api/v4/file/test` ([D-209]).

**Blocked on a decision or a large port (~16).** `plugin.go` (5) and `agents.go` (3) need the
plugin environment; `view.go` (3) and `channel_join_request.go` (4) are behind the
`IntegratedBoards` and `DiscoverableChannels` feature flags, and turning either on changes routes
already served ([D-153]); `command.go`'s two autocomplete routes need the built-in slash-command
registry ([D-241]).

**Recorded as not-portable (~7).** [D-171]'s five — `server_busy`, `logs`, `logs/download`,
`latest_version` and the agents trio's shared cause — plus `/files/{id}/public` ([D-170]), whose
errors are signed HTML rather than JSON.

### What the session cost the test suite, and paid back

The forward target moving from an emulated image to a native build made `--test parity` six times
faster, and that exposed **nine** latent races — four socket assertions counting frames on the
shared admin, two fixed-window waits, a live user count compared across two instants, a tied
sort key on direct-message channels, and a fifty-one-request test whose post another suite deleted
underneath it. All nine are fixed rather than retried, and the suite is 906 tests over ~40s.

## The two user-report reads, and a keyset cursor that disagrees with its own tiebreaker (2026-09-09)

`GET /api/v4/reports/users` and `GET /api/v4/reports/users/count` — the System Console's *User
Management → Users* table and its total. `mm-model`'s `report.rs` was already ported, so this was
a store query, two app functions, two handlers and a parity suite:
`mm_store::UserStore::get_user_report`, `mm_app::App::get_users_for_reporting`, `mm_api::reports`.

Go builds the report from nine `squirrel` fragments, a keyset cursor whose *sort column is a
parameter*, and an outer `SELECT … FROM (…)` that re-sorts a backwards page. All of it is one
`query_as!` here, with the sort key computed in a lateral join — `k_num` for `CreateAt`, `k_txt`
for the six text columns, each NULL when the other is in use, so `ORDER BY` can name both and let
the unused one tie.

### The `Users.Id` tiebreaker does not follow the sort

`GetUserReport` decides its direction twice: from `SortDesc`, then again from `Direction` when a
cursor is present, where `prev`-on-ascending and `next`-on-descending both flip it to `DESC`. The
cursor predicate follows that flip — `<` for `DESC`, `>` for `ASC`. The `ORDER BY`'s `Users.Id`
tiebreaker is written with no direction at all and is therefore **always ascending**, even on a
descending page whose predicate reads `Users.Id <`. Reproduced rather than corrected; the test
that pins it is `every_sort_column_and_direction_matches`, which passes only because three of the
seven sort columns are empty on every fixture user and tie on the id alone.

### `direction=prev` reverses a page it did not paginate

The reversing wrapper is applied on `Direction == "prev"` **whether or not a cursor was given**.
With no cursor the inner query still sorts ascending, so `?direction=prev` returns the *same first
page*, handed back reversed — not the page before. A reader expecting "prev means the previous
page" writes the wrapper into the cursor branch and gets a test failure only from
`a_prev_page_with_no_cursor_is_the_tail_in_forward_order`.

### The count route reads six of the thirteen parameters

`getUserCountForReporting` calls `fillUserReportOptions` and **not** `fillReportingBaseOptions`,
and `App.GetUserCountForReport` does **not** call `IsValid`. So `?sort_column=nonsense` is a 400
on the list and silently ignored on the count, and `?date_range=previous_month` narrows the
aggregates on the list and cannot change the count at all — the date range reaches only the
`PostStats` join condition, which the count query has not got.

Two more small ones on the wire: the three boolean filters compare against the literal string
`"true"`, so `?hide_active=1` is **false**; and `page_size` goes through `strconv.ParseInt` with
the error discarded, so `?page_size=abc` is 50 rather than a 400.

### A `CreateAt` cursor that is not a number is a 500

Go binds `FromColumnValue` as a string and lets Postgres coerce it against a `bigint`, so
`?sort_column=CreateAt&from_id=…&from_column_value=yesterday` fails the query and answers
`app.report.get_user_report.store_error`. Rust has to parse, and parsing quietly with `.ok()`
would compare against SQL NULL and answer **200 with an empty page** — the plausible wrong answer.
`ReportFilter::from_options` returns a `StoreError::Argument` instead; measured against Go, not
inferred.

### `PostStats` is a materialized view nothing refreshes

`MAX(ps.LastPostDate)`, `COUNT(ps.Day)` and `SUM(ps.NumPosts)` all read `poststats`, which only
`RefreshPostStatsForUsers` — a scheduled job, reachable from no route — ever populates. Against an
unrefreshed view every `total_posts` is absent and every `days_active` is `0`, the date-range join
condition is dead, and four of this session's mutations survive catching nothing. The suite
refreshes it, and plants posts on four separate days — today, the first of this month, the first
of last month, and a hundred days back — because posting through the API only ever writes today's
date, and `ps.Day >= start` / `ps.Day < end` are then unreachable in both directions.

### The bot fixture was poisoning a route it has nothing to do with

`common::plant_bot` wrote `MfaUsedTimestamps` as `'{}'` — a JSON **object**, where Go scans a
`model.StringArray`. Any `GET /api/v4/users` that returned that row was a 500 *from Go*, for every
caller. It never showed as a bug because the bots suite deletes its rows on the way out, so the
poison lasted exactly as long as that suite did; this session planted a bot and left it behind,
and three `users_list` tests failed on a row they never asked for. Now `'null'::jsonb`, which is
what the Go server itself writes and what `purge_api_fixtures` normalises to. Same family as the
NULL `LastPictureUpdate` the previous entry records: a hand-planted row that only Go cannot read.

### Mutation testing: 51 run, 49 caught, 2 controls survived

Three real survivors on the first pass, each the same shape — the right answer and the wrong
answer coincided:

* `count-guest-single-counts-many` swaps `= 1` for `> 1` in the **count** query's guest filter.
  The fixture had one single-channel guest and one multi-channel guest, so both readings counted
  one user. A third guest, on the `> 1` side, separates them.
* `report-channel-count-includes-dms` adds `'D'` to `c.Type IN ('O','P')`. No fixture user had a
  direct message. One DM — opened **after** the channel-count surgery, since that deletes every
  membership row but one and a DM membership is a row like any other — closes it.
* `report-search-is-prefix-only` narrows the `Username` arm to `term%`. `create_plain_user`
  derives the email from the username, so every term matching one matches the other and the five
  `LIKE` arms can never disagree. A user whose email shares no substring with their username
  tests the `Username` and `Email` arms one at a time.

The batch also **aborted at mutation 47 with the mutation still applied**, because a plan line
with an empty `to` field shifts `read -r`'s remaining fields: the replacement text became the
literal string `api`, which is what ended up in `reports.rs`. `preflight-plans.sh` is what caught
it — a zero-match anchor in a file nobody had edited. A plan's `to` should be a no-op statement,
never empty.

### The ledger's own number was 21 short

The section above records the inventory as **231/764 to 264/764**. `scripts/routes.py` against
that commit reads **285/764**, and the commit message for the file-backend work says
`264 -> 285` — so the prose was written before those 21 pairs were registered and never caught
up, while the commit had it right. Reading the number out of `scripts/routes.py` rather than out
of the previous entry is the whole reason that script exists. This session takes it to
**287/764**; `306 HTTP pairs remain`.

## `auth_data`, `invalid_emails`, and an escape every error body was missing (2026-09-10)

`GET /api/v4/users/auth_data` and `GET /api/v4/users/invalid_emails`, the last two literal-path
reads in `api4/user.go`. 287 → **289 of 764**. Store, app and handler:
`mm_store::UserStore::{get_by_auth_data, get_users_with_invalid_emails}`,
`mm_app::App::{get_user_by_auth_data, get_users_with_invalid_emails}`, and two handlers in
`mm_api::users`.

### Go's error bodies are HTML-escaped and ours were not

`ApiError::into_wire` used `serde_json::to_vec`. Go writes error bodies with `encoding/json`,
whose default escapes `<`, `>` and `&` — so `model.NoTranslation`, the literal `<untranslated>`,
goes out as `<untranslated>`. **Every error body on every route** went through the
unescaped encoder; it had simply never mattered, because no ported route had put one of those
three characters in an `id`, a `where` or a surviving `detailed_error` until this one. Now
`mm_model::utils::go_json_marshal`, which the model layer already had for exactly this reason.

### `getUsersWithInvalidEmails` reads its configuration before its permission

`TeamSettings.EnableOpenServer` being **on** is a 400, and it is checked first — so an
unprivileged caller on an open server is told a configuration value they have no permission to
read. The 400's id is `model.NoTranslation`, so the body carries no error id at all, and the
detail that says *why* is wiped with every other `detailed_error`.

**The stack's Go server pins that variable on**, through the environment, where it cannot reach
the configuration document. So the 200 path has no Go counterpart at any price short of a second
Go server: the refusal is compared, the success is served by a `SecondServer` with the gate open
and asserted against itself, and the query is tested against planted rows in `mm-store`'s
`db_users_invalid_emails`. [D-213] holds what is owed.

### `Users.Roles != 'system_guest'` is an exact comparison

Every other guest predicate in the user store is `LIKE '%system_guest%'`. This one is `!=` against
the whole column, so an account whose roles are `system_guest system_user` **is** reported as
having an invalid email. Pinned by `store_invalid_emails_a_guest_with_a_second_role_is_kept`,
because "fixing" it to a `LIKE` changes which accounts an administrator is told to chase.

Two more from the same query: the domain list is **not trimmed**, so `"a.com, b.com"` yields
`" b.com"` and a `LIKE '% b.com%'` no address matches; and there is no `@` anchor, so configuring
`invalid` allows every `@mmrs.invalid` address and every address that merely contains the word.

### `getUserByAuthData` is `getUser` with four differences

Same shape, and each difference is on the wire: the gate is `IsSystemAdmin` rather than
self-or-admin; the terms-of-service lookup is unconditional rather than self-or-admin; the
sanitisation is always `SanitizeProfile` — so an admin looking up **their own** account by auth
data gets a smaller answer than `GET /users/me` gives them — and last activity is not touched.

### One launch environment, sourced twice

`scripts/parity.sh` set `MM_FILESETTINGS_DIRECTORY` on the server it starts and `scripts/mutate.sh`
did not, so every `api`-suite mutation ran against a server configured unlike the one the tests
were written against. Both now source `scripts/mm-api-env.sh`, which also carries
`MM_TEAMSETTINGS_ENABLEOPENSERVER` — without which this session's two servers disagreed about a
route for a reason that had nothing to do with the port.

### The error-body helper required a divergence

`assert_error_bodies_match_except_known_gaps` asserted the set of differing keys **equals**
`["message", "request_id"]`. When the id is the untranslated sentinel both servers write the same
`message`, only `request_id` differs, and the helper failed the comparison for agreeing with Go
more closely than it expected. It now asserts a subset.

### Mutation testing: 27 run, 25 caught, 2 controls survived

Three real survivors, and each one was a test in the wrong place rather than a missing assertion:

* `invemail-rows-are-not-sanitised` deletes the `Sanitize` call. The planted rows already had an
  empty password, an empty MFA secret and a zero `LastLogin` — the three fields it blanks — so
  sanitised and unsanitised were the same row. The fixture now plants values for it to remove.
* `app-authdata-invalid-input-is-a-404` swaps the two status codes `App::get_user_by_auth_data`
  maps its store errors onto. The 400 is **unreachable through the route**, because the handler
  refuses an empty `value` before the app is called, so no parity test can distinguish them. It
  now has `mm-app`'s `db_user_by_auth_data`, where the app is called directly.
* `config-open-server-defaults-on` flips the default of `enable_open_server`. Both servers set
  that variable in the environment, where the overlay wins, so the *default* is never read on a
  running server. It is now a unit test on `Config::default()` — with a second one pinning the
  overlay itself, which is the half that had been missing and let the two servers disagree in the
  first place.

The first of those is the familiar shape (a fixture where the right and wrong answers coincide);
the other two are not — they are branches no route reaches, which no amount of fixing the parity
suite would have covered. `scripts/mutations/user-lookups.plan` runs across four suites for that
reason: `store` for the SQL, `app` for the status mapping, `unit` for the configuration, `api` for
the handlers.

## The channel a client says it is looking at (2026-09-10)

`POST /api/v4/channels/members/{user_id}/view` and
`POST /api/v4/channels/members/{user_id}/mark_read` — the first **channel writes** in the port,
and the first route a real client calls on every single channel switch. 289 → **291 of 764**.
Store, app and handlers: `mm_store::channel_store::{get_channels_with_unreads_and_with_mentions,
update_last_viewed_at, get_board_channel}`, `mm_store::ThreadStore::mark_all_as_read_by_channels`,
`mm_app::channel_view` (a new module), and two handlers in `mm_api::channels`.

### The deny-list is one type wide, and it is not the allow-list next to it

`GetChannelsWithUnreadsAndWithMentions` filters `Channels.Type NOT IN ('S')`
(`nonMessageBackingChannelTypes`, channel_store.go:52). Every neighbouring channel query — `Get`,
`GetMany`, `GetChannelUnread` — uses the `IN (O, P, D, G)` **allow-list** instead. So a board
(`BO`/`BP`) the caller is a member of **is** marked read by this route while `SqlChannelStore.Get`
would call the same channel missing. Narrowing it to the allow-list is invisible over HTTP (a
board id is refused by the handler before the query runs) and silently strands board read-state;
`db_channel_view_reads` is where that is pinned.

### `readMultipleChannels` never answers the 400 it raises

The handler calls `c.RequireUserId()` and then **does not return** (channel.go:2067). Go's `c.Err`
is one slot, so the malformed-id 400 is overwritten by whatever refuses next: an unparseable body
is `api.payload.parse.error`, an empty list is `invalid_body_param`, a caller without
`edit_other_users` is a 403, and a caller *with* it reaches `MarkChannelsAsViewed`, whose user
lookup fails on the malformed id and gives a 500. Its sibling `viewChannel`, one `if` away,
returns early and answers the 400. Both are asserted against Go
(`a_malformed_path_user_id_answers_differently_on_the_two_routes`).

The one path that would let the 400 survive — the app call succeeding for a malformed id — cannot
happen, and if it could, Go would write the success body and then **append the error JSON to it**.
Recorded in the handler rather than reproduced.

### The write answers from `Channels` and the timestamps come from the old row

`UpdateLastViewedAt` is one CTE: a `WITH c AS (SELECT … FROM Channels …)`, an `UPDATE` in a second
CTE, and `SELECT Id, LastPostAt FROM c`. Two things follow that `UPDATE … RETURNING` would get
wrong — a channel the user is **not a member of** is in the answer, and "empty" means no
*channel* matched, not that no membership was updated. That is the only thing that raises
`ErrInvalidInput`, which the app layer turns into a 400.

Inside the statement, `LastViewedAt` and `LastUpdateAt` are both
`greatest(cm.LastViewedAt, c.LastPostAt)` — evaluated against the **pre-update** row, so the two
columns end up equal and neither is a clock reading. And the map the store returns is discarded:
the routes answer with `max(LastPostAt, LastViewedAt)` from the *read*, computed before the write.

### `collapsed_threads_supported` is the only half of the thread decision a client can move

`updateThreads` is `ThreadAutoFollow && (!collapsedThreadsSupported || !isCRTEnabled)`. The shipped
`ServiceSettings.CollapsedThreads` is `always_on`, which makes `IsCRTEnabledForUser` return true
**without reading the user's preference at all** — so the expression reduces to
`!collapsedThreadsSupported`. A client that renders threads itself gets no thread write and no
`thread_read_changed`; one that says nothing gets both. `mark_read` passes the literal `true`, so
it never publishes a thread event where `view` with the same body would. All three are asserted on
the socket.

### The two routes disagree about which refusal comes first

`view` gates on `edit_other_users` **before** it reads the body; `mark_read` parses the body
first. The same request — another user's id, `not json` — is a 403 on one and a 400 on the other.

### Three things this port does not do on these routes

* **`ExtendSessionExpiryIfNeeded`** ([D-214]). Off on every persisted configuration document, so
  both servers do nothing here today; it is a `Set-Cookie` when it is on.
* **`clearPushNotification`** ([D-215]). There is no hub. The channel list it would consume is
  computed in full anyway, because its notify-prop fall-through is three branches deep and would
  be invisible until there *is* a hub.
* **Reading the status cache back** ([D-216]). `SetActiveChannel` writes only to it, and
  `get_user_statuses_by_ids` reads the table — so the one mutation with no catcher is the one that
  deletes that call.

### Mutation testing: 31 run, 29 caught, 2 controls survived

Three real survivors on the first run, and none of them was a fixture where the right and wrong
answers coincided:

* `app-view-drops-the-prev-channel` — no test sent a `prev_channel_id` naming a real channel, so
  the field every client fills on every switch was untested. Now
  `the_previous_channel_is_marked_read_as_well`.
* `app-invalid-input-is-a-500` — the 400/500 split is **unreachable through the route**: the ids
  handed to `UpdateLastViewedAt` came out of a join against `Channels`, so no request can produce
  the empty result that raises `ErrInvalidInput`. Go has the same dead branch. Extracted as
  `update_last_viewed_at_error` so the reproduction has an oracle a unit test can reach.
* `api-view-checks-the-body-before-the-gate` — the mutation was wrong, not the tests: it moved
  `read_body`, which only fails on a transport error, rather than the decode. Moving the decode is
  caught.

One harness fault on the first run, from a control that did not compile; the tally above is the
re-run.

## Marking everything read, and a feature flag turned on deliberately (2026-09-10)

`PUT /api/v4/channels/members/{user_id}/direct/read` and
`PUT /api/v4/users/{user_id}/teams/{team_id}/read` — shift-escape in the webapp. 291 →
**293 of 764**. Two more store queries
(`get_team_channels_with_unread_and_mentions`, `get_direct_messages_with_unread_and_mentions`),
two more app functions in `mm_app::channel_view`, and two handlers.

### The flag is on now, on both servers

`FeatureFlags.EnableShiftEscapeToMarkAllRead` defaults to **false**, and with it off both routes
are a 501 with no comparable 200 anywhere. `FeatureFlags` is stripped before the configuration
document is persisted (config/store.go:306), so the environment is the only place either server
can read it from: `scripts/go-server.sh` and `scripts/mm-api-env.sh` both set it now, and
`go-server.sh`'s standing note — "turning one on is a deliberate act with its own parity run" — is
what this is. Unlike `IntegratedBoards` and `DiscoverableChannels` ([D-153]) this flag is read in
exactly two places in the whole Go tree and changes no route already served.

The **501** therefore has no cross-server oracle: it is asserted against a `SecondServer` started
with the flag off, which is our answer only. Same shape as [D-213], and the gate is the *first*
line of each handler — ahead of `RequireUserId`, so a malformed id gets the 501 too.

### These two are not `MarkChannelsAsViewed` with a different WHERE

Three differences, and the first is the one a port would get wrong:

* **The thread store is handed every channel, not the unread ones.** A thread reply does not bump
  `Channels.TotalMsgCount`, so a channel whose counters are caught up can still hold unread thread
  replies; Go passes the whole membership set for exactly that reason (its comment is at
  app/channel.go:3566) and lets the thread statement's own `LastReplyAt > LastViewed` clause bound
  the write. `the_team_route_marks_threads_read_in_channels_that_were_already_read` is the test
  that can tell the two apart — it views the channel first, saying it supports collapsed threads,
  which leaves the channel read and the thread unread.
* **The thread write is unconditional and comes before the early return.** Only the channel write
  and its `multiple_channels_viewed` are behind "something was unread", so a second press is
  silent except for the thread event.
* **There is no `ThreadAutoFollow` gate and no `collapsedThreadsSupported`** on either route.

### The two thread events are scoped differently, and that is the whole difference at the end

The team route publishes **one** `thread_read_changed` with a `team_id` — the client turns it into
a single `ALL_TEAM_THREADS_READ`. The direct route has no team to broadcast on, so it publishes
one **per channel**, channel-scoped, sharing a single timestamp, and the client turns each into
`ALL_THREADS_IN_CHANNEL_READ`. Both are gated on CRT being on for the user, which the shipped
`always_on` default makes unconditional.

### `MarkAllDirectAndGroupMessagesViewed` reuses the team query's error id

Its store failure is reported as
`app.channel.get_channels_by_team_with_unreads_and_with_mentions.app_error` (channel.go:3624) —
the *team* id, on the route that has no team. Reproduced rather than corrected: the id is what a
translated message keys off.

### `readAllInTeam`'s two gates, and what a non-member gets

`SessionHasPermissionToUser` first, then `SessionHasPermissionToTeam(view_team)`. A caller acting
on their own account who is not in the team gets the **`view_team` 403**, not a 404 — there is no
`GetTeamMember` on this path to produce one, unlike its neighbours in the same file.

### Mutation testing: 20 run, 18 caught, 2 controls survived

No real survivors. One harness fault on the first run and it was a mutation, not the code: a
predicate rewritten to `AND ($3 = $3)` left the bind parameter untyped, so `sqlx::query!` could not
infer `&[String]` and `mm-store` failed to compile. Rewritten as
`AND (threads.channelid = ANY($3) OR TRUE)`, which keeps the parameter used and still deletes the
scope.

## Numbered stacks, and what a fresh one found (2026-09-10)

No routes. The stack-backed suites talked to one Postgres, one Go server on :8065 and one mm-api
on :8066, so `stack-lock.sh` serialised every checkout on the machine — which made a twenty-minute
mutation batch block every other worktree and left the documented parallel-worktree pattern unable
to run in parallel. A stack is now a numbered triple (`postgres 5432+k`, `go 8065+100k`,
`mm-api 8066+100k`), the lock is per stack, and stack 0 is the historical layout byte for byte.
**Two full workspace suites on two stacks: 150s wall, 3141 tests each.**

The base URLs follow the stack through `option_env!` at **compile** time, with a `build.rs` per
crate for `rerun-if-env-changed`, because `common::GO` and `common::RUST` are `&'static str`
consts inside roughly thirteen hundred inline format captures; a runtime value would mean
rewriting all of them. `SecondServer::start` shifts its literal port at runtime instead.

### The first run against a fresh stack failed fifteen tests, and none of them was a flake

That is the finding, and it is worth more than the parallelism. Every one had been passing for
months on a database old enough to have accumulated the shape it assumed:

* **A genuine port bug.** Go renders a SQL `NULL` `jsonb` column as `{}` and a JSON `null` as
  `null`; `JobRow::into_job` collapsed both to `None`, and its doc comment asserted they were the
  same. No Mattermost worker writes a SQL NULL — every null-ish `Jobs` row on a real deployment is
  the product-notices worker's JSON null — so the assertion could only ever see one of the two
  shapes. `scripts/stack.sh seed` plants both now.
* **One NULL is a 500 for eleven tests.** A dozen `mm-store` and `mm-app` suites `INSERT INTO
  teams` without `LastTeamIconUpdate`, which `GetAllTeams` scans into a plain `int64` — so
  `GET /usage/teams` 500s and `teams_all`'s eight tests go with it, depending on which binary
  `cargo test --workspace` happened to run first. Normalised in the purge, exactly as the `Users`
  repair beside it already was, and for the reason that one already gives.
* **Three tests that proved nothing.** `bots` needs a bot with a description, `jobs` needs both
  null shapes, and both say so in their own assertion message. A fresh database has neither.
  `config_source` asserted the literal `:8065`.

The lesson is not about ports. **A suite that has only ever run against one long-lived database
has untested dependencies on that database**, and the cheapest way to find them is to stand up a
new one.

## The five sidebar-category writes, and a `GET` that writes (2026-09-10)

`POST`/`PUT` on `…/channels/categories`, `PUT` on `…/categories/order`, `PUT`/`DELETE` on
`…/categories/{category_id}`. All eight methods on the three paths are served now; the family is
complete apart from `getManagedCategories`, which is licensed and feature-flagged and was not
started.

| Go source | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| store/sqlstore/channel_store_categories.go — `CreateInitialSidebarCategories`, `CreateSidebarCategory`, `UpdateSidebarCategoryOrder`, `UpdateSidebarCategories`, `DeleteSidebarCategory` | `mm-store/src/sidebar_category_store.rs` | DONE | 12 pass (`tests/db_sidebar_category_writes.rs`) | Each one transaction, with Go's statement order inside it — categories before channels, and the category updates in **id** order, both for deadlock avoidance against a concurrent transaction. `UpdateSidebarCategories` also writes `favorite_channel` **`Preferences`** rows, and its two branches are asymmetrical: Favorites deletes the *original* channel list and re-adds the new one, every other type deletes the *request's*. |
| app/channel_category.go — `createInitialSidebarCategories` and the four writes | `mm-app/src/sidebar.rs` | DONE | 16 pass | Four websocket events with **two payload conventions**: `order` is a JSON array, `updatedCategories` a marshalled string. None of the four omits the originating connection (Go passes `""`), unlike the draft and preference writes. `muteChannelsForUpdatedCategories` is ported as far as the decision only — see [D-224]. |
| api4/channel_category.go — the five writes | `mm-api/src/sidebar.rs` | DONE | 40 parity | The per-category refusal on the collection route is a **400** naming `category`, not the 403 its singular sibling answers from the same gate. `/order` is the one route whose decode failure is `api.payload.parse.error` with no `Name`, and `null` is not a decode failure there at all. Mutations: see the tally below. |

Four things a reader would otherwise get wrong, each of them a test:

1. **`validateSidebarCategory` validates nothing.** It silently *drops* every channel the caller is
   not a member of, logs it, and de-duplicates the rest — so a request naming somebody else's
   private channel succeeds and simply does not contain it. Its one error branch is a 400 with
   `api.invalid_channel`, reached when `GetChannelsForTeamForUser` answers its **404** for a user
   who is in no channel on the team. And because `RemoveDuplicateStringsNonSort` returns a non-nil
   `[]string{}`, `channel_ids` is never `null` on any create or update answer.
2. **`GET .../categories` writes.** Go creates the Favorites/Channels/DirectMessages triple inside
   the read when the user has none, migrating their `favorite_channel` preferences into
   `SidebarChannels` as it goes. That case used to be forwarded to Go on the grounds that two
   servers would race to insert the same ids; it is not forwarded any more, because the
   deterministic `{type}_{userId}_{teamId}` ids and the primary key on `SidebarCategories.Id` are
   Go's own mechanism for making that race converge. Only the *missing* types are inserted, so a
   user who has Favorites and lost Channels gains Channels alone.
3. **Go's `json.Marshal` escapes `<`, `>` and `&`; `serde_json` does not.** `display_name` is
   arbitrary user text, so a category called `Q&A` differed by nine bytes. All five writes and —
   this was the latent half — **all three reads** now go through `mm_model::utils::go_json_marshal`.
   The same applies inside the `sidebar_category_updated` event, whose payload is a marshalled
   string.
4. **Five fields are read-only, per field and per category type.** `UserId`, `TeamId`, `SortOrder`
   and `Type` come back from the row; `DisplayName` is read-only unless the category is `custom`;
   `Muted` is read-only for **Direct Messages** alone. Two loops later, Go branches on the
   *request's* `Type` rather than the row's — so a Channels category mislabelled `direct_messages`
   has its channels deleted and not reinserted. Reproduced, and pinned by
   `db_sidebar_category_writes::a_category_mislabelled_direct_messages_has_its_channels_deleted_and_not_restored`.

Two branches are **unreachable through HTTP** and are therefore covered only in `mm-store`:
`UpdateSidebarCategoryOrder`'s store-level `ErrInvalidInput` → 400 (the handler's own per-id
permission loop refuses first, and a duplicate id becomes a length mismatch → 500 before it), and
`UpdateSidebarCategories`' 500-for-everything mapping (the same gate turns an unknown id into a
400). Both are stated in the doc comments on the functions that carry them.

The parity suite runs **two subject users on one team**, so channel ids appear identically in both
servers' answers and only the user id and the minted category id need substituting out. Two teams
would have made the channel ids differ too, leaving nothing to compare. Fixtures are prefixed
`mmrssbwrite`, cleared by the suite's own purge, for the reason `mmrssidebar` gives.
## Channel membership, written (2026-09-10)

All six member-write routes on `/api/v4/channels/{channel_id}/members…`: `addChannelMember`,
`setChannelMembers`, `removeChannelMember`, and the `/roles`, `/schemeRoles` and `/notify_props`
updates. 298 → **304 of 764**. `Channel.SaveMember` was the single most-shared unported store
method in the tree — `scripts/deps.py` counted **18** unserved routes waiting on it — which is why
this group went first; `POST /api/v4/channels` is the next of the eighteen and needs nothing new.

New: `mm_store::channel_member_history_store`, `mm_store::group_store` (one query),
`mm_app::channel_member`, `mm_api::channel_member_writes`. Appended to `ChannelStore`:
`save_member`, `update_member`, `update_member_notify_props`, `remove_member`,
`get_all_channel_member_ids_by_channel_id`, `get_channel_of_type`. Also
`ThreadStore::delete_memberships_for_channel`.

### The join and leave system posts are not written, and one of them shows in a body

Four `Posts` writes were missing when this group landed; they were written the next session — see
*The system posts twelve served routes owed*. What remains is that `PostAddToChannelMessage`'s
mention is Go's **notification pass**, not the post: a **re-add** of an existing member still
answers Go's `mention_count: 1` against our `0` ([D-235]). Masked in the parity suite with Go's
value asserted, so the exclusion cannot widen.

### `ReturnStatusOK` is the one success body here that is not encoder-framed

`w.Write([]byte(MapToJSON(m)))`, not `json.NewEncoder(w).Encode` (web/web.go:127) — so no trailing
newline, where `addChannelMember`'s body and every NDJSON line have one. Four tests caught the
first version. See `mm_api::channel_member_writes::status_ok`.

### `MapFromJSON` never returns nil, which kills a branch and softens two routes

`json.NewDecoder(...).Decode(&map[string]string)`'s error is **discarded** and a nil map replaced
with an empty one (utils.go:507). So `updateChannelMemberNotifyProps`' `if props == nil {
SetInvalidParam }` is unreachable, and `PUT …/notify_props` with a body of `[]` is a **200**. On
`/roles`, `{"roles": 5}` is not a 400 either: the value is dropped and the request fails four layers
down with `unset_user_scheme`. Both measured, both asserted.

### Nothing validates a notify-prop *value* on the update path

`UpdateChannelMemberNotifyProps` (app/channel.go:1519) copies out ten known keys and drops the rest,
and neither it nor the store calls `IsChannelMemberNotifyPropsValid`. `{"desktop": "banana"}` is a
200 that stores `banana` — while the *same* value reaching `ChannelMember::IsValid` through the add
path is a 400. The existing `mm_model::channel_member::is_channel_member_notify_props_valid` is
correct and simply not on this path.

### The write is a merge, and that is why the route is usable

`notifyprops = notifyprops || $1::jsonb` (channel_store.go:2075). A client saving only `desktop`
keeps its `mark_unread` — i.e. keeps its mute. `SET notifyprops = $1::jsonb` is one character away
and would clear it.

### `/roles` and `/schemeRoles` write disjoint halves of the same row

`/roles` **sets** the three scheme flags from the submitted names and writes `ExplicitRoles`;
`/schemeRoles` sets the flags from three booleans and (on a migrated server) leaves `ExplicitRoles`
alone. So `channel_admin` alone on `/roles` is a 400 — the flags are set, not patched — and neither
route can move a member in or out of guest: `prevSchemeGuestValue != member.SchemeGuest` is
`changing_guest_role`, and `scheme_guest: true` is `user_and_guest`. Every id in both families is
spelled `api.channel.update_channel_member_roles.*`, including the ones raised from
`UpdateChannelMemberSchemeRoles`.

Note the inverted gate in `UpdateChannelMemberSchemeRoles`: `if err = IsPhase2MigrationCompleted();
err != nil` strips the built-in channel roles when the migration has **not** finished, and discards
the error. Reading it the other way round drops a member's explicit roles on every call.

### `addChannelMember` takes three body shapes and the answer's shape follows a *key*

`user_ids` wins when it is an array; anything else falls through to `user_id`, which is why
`{"user_ids": "x"}` reports `user_id or user_ids`. The answer is a bare object when the body carried
a `user_id` **key**, exactly one member resulted, and it is that user — so `{"user_ids":["x"]}`
answers `[{…}]` and a body with both keys answers `{…}`. `{"user_ids": []}` is a `201` with
**`null`**: the member slice is never appended to and Go encodes a nil slice as `null`.

### A partly-refused multi-add answers two JSON documents

The per-id loop's `SetPermissionError` sets `c.Err`, a later success does not clear it, and
`handleContextError` runs after the handler — on top of a `201` that is already committed. So adding
`[self, someone-else]` as a user who may only add themselves yields the member array **and then the
403 envelope**, concatenated. Reproduced via `ApiError::into_wire`, the same split
`getChannelsForUser`'s streaming error uses.

### Two `user_added` events and two `user_removed`, with different payloads

`AddUserToChannel` publishes one addressed to the **channel** (with the added user in `omit_users`)
and one addressed to the **added user**; Go's comment says why — a cluster node that has not seen
the new membership yet would filter the first one out for that user. The two `user_removed` events
carry **different keys**: `user_id`+`remover_id` on the channel-addressed one, `channel_id`+
`remover_id` on the user-addressed one, which has no channel in its broadcast and nothing else to
tell the client which channel it just left. `channel_member_updated` is addressed to the member's
user id and to **no channel**, with the member as a JSON *string* under `channelMember`.

### `ChannelMemberHistory` is written on both paths and shows in no response

Asserted twice — at the store level in `mm-store/tests/db_channel_member_writes.rs` and through the
route via `common::channel_member_history`, because without the second the app layer could stop
calling it and every HTTP assertion would still pass. `LogLeaveEvent`'s `LeaveTime IS NULL` is what
keeps a closed stay closed; dropping it rewrites the whole audit trail for that membership, and it
is best-effort by design (no open stay is a warning, not an error).

### `setChannelMembers` is NDJSON, and its diff order is a Go map iteration

Four phases in order — removals, additions, promotions, demotions — each a batch, each one line.
`added` and `removed` are forced from nil to `[]` **in the handler's callback**, so both keys are
always arrays while `promoted`/`demoted`/`errors` are `omitempty`; a no-op still emits exactly one
`{"added":[],"removed":[]}`. Go builds `toAdd`/`toRemove` by ranging over a `map`, so **which ids
land in which batch is not stable across runs on the Go side** — anything asserting on these lines
has to sort. Two divergences, both deliberate: this port **buffers** rather than streaming (same
bytes, no per-batch flush) and `errors[].error` is `where: <id>` where Go has `where: <translated
message>, <detail>` ([D-092]).

### Three of the six reject board and space channels; three do not

The guards are on the three `PUT …/{user_id}/…` handlers only (api4/channel.go:2147-2159 and
siblings). `addChannelMember`, `setChannelMembers` and `removeChannelMember` have neither, so a
board id there reaches `GetChannel` and gets its **404** rather than the guards' 400. The two guards
are also asymmetric: `rejectBoardChannelByID` tests `err == nil`, so a database failure reads as
"not a board", while `rejectSpaceChannelByID` fails **closed** and returns anything that is not a
404.

### What is forwarded, and why each one

Group-constrained channels (`FilterNonGroupChannelMembers`), attribute-based access control, shared
channels, guest sessions (`UserCanSeeOtherUser`'s restricted branch), a `post_root_id` (a
`ThreadMemberships` write), a discoverable private channel (the join-request queue), and a channel
carrying a `default_category_name` (`addChannelToDefaultCategory` writes `SidebarChannels`).
`set_channel_members` resolves all of them **before its first write**, because a half-applied
reconcile handed to Go would be applied twice.

### Mutation testing: 56 run, 54 caught, 2 controls survived

`scripts/mutations/channel-member-writes.plan`. Five mutations survived the first pass and each was
a finding about a **fixture**, not a shrug:

* **Two `LastUpdateAt` writes were untestable against themselves.** `PreSave` and `PreUpdate` both
  stamp `model.GetMillis()`, and the save and the update in one test land in the same millisecond —
  so `last_update_at >= first_update_at` is true whether the call ran or not. Deleting
  `pre_update()` and replacing `SET lastupdateat = $2` with `LEAST(lastupdateat, $2)` both survived.
  The fix is to **backdate the column to `1` and the struct with it**, so only the code under test
  can raise it.
* **`promoted` is a list, not a claim.** Flipping the third positional boolean of
  `UpdateChannelMemberSchemeRoles(channelID, userID, false, true, true)` still reports the user as
  promoted and leaves them a plain member. The reconcile test now reads the member back on both
  servers.
* **The `batch_delay_ms` bounds were mutated at a call site the unit test does not use.** The unit
  test exercises `bounded_query_int` with bounds of its own, so raising the *handler's* minimum from
  0 to 1 was invisible to it. That plan line moved to the `api` suite, where every reconcile request
  carries `batch_delay_ms=0`.
* **A duplicate of an existing member is not a test of deduplication.** `{"members": [me, u, u]}`
  where `u` is already a member diffs to nothing either way. It has to be a duplicate of a
  **non-member**: undeduplicated, the add loop runs twice and the second pass — which finds the
  member already there — appends the id to `added` a second time.

One mutation was also **destructive to the shared fixture**: `town-square-is-leavable-by-a-non-guest`
makes the removal succeed, so the run that catches it leaves the caller out of `town-square` and
every later run of that test fails on an unrelated 404. The test now re-joins on both servers first,
which is idempotent because a self-add to a public channel is a `201` either way.

### And one bug the mutation plan did not find

`add_user_to_channel` forwarded a **shared** channel and a channel with a `default_category_name`
*after* `add_user_to_channel_row` had already committed the membership and its history row. Go then
finds the member present, returns it, and publishes nothing — so the body was right and no
`user_added` event went out from either server. Both checks moved above the write. No test could see
it: this deployment has no shared channel and no channel with a default category, which is exactly
why the two branches forward in the first place.
---

## The channel lifecycle, minus creation (2026-09-10)

Five routes, all served: `PUT /api/v4/channels/{channel_id}`, `/patch`, `/privacy`,
`DELETE /api/v4/channels/{channel_id}`, `POST /api/v4/channels/{channel_id}/restore`.
`POST /api/v4/channels` is *not* among them — it adds the creator as a member, and every
`ChannelMembers` write belongs to another session.

| what | where | status |
|---|---|---|
| `Channel().Update`, `Delete`, `Restore`, `SetDeleteAt`, `upsertPublicChannelT` | `crates/mm-store/src/channel_store.rs` (end of both blocks) | done; `Update` takes `&mut Channel` because Go's `PreUpdate` mutates in place |
| `GetIncomingByChannel`, `GetOutgoingByChannel` | `crates/mm-store/src/webhook_store.rs` | done; their own statements, because Go omits `LIMIT`/`OFFSET` when either is negative and `DeleteChannel` passes `-1` |
| `App.UpdateChannel`, `PatchChannel`, `UpdateChannelPrivacy`, `DeleteChannel`, `RestoreChannel` | `crates/mm-app/src/channel_write.rs` | done; the six system posts and the persistent-notification cleanup landed the next session |
| the five handlers | `crates/mm-api/src/channel_writes.rs` | done; a licensed installation and two patch branches forward ([D-234]) |
| 26 cross-server tests and 13 unit tests | `crates/mm-api/tests/parity/channel_writes.rs`, and `#[cfg(test)]` in both new modules | — |

### `updateChannel` and `patchChannel` differ in five ways, and every one is on the wire

Measured field by field against the running server rather than read off the source, because four of
the five are the sort of thing a happy-path test agrees with either way:

| | `PUT /channels/{id}` | `PUT /channels/{id}/patch` |
|---|---|---|
| fields honoured | `header`, `purpose`, `display_name`, `name`, `group_constrained` and **nothing else** — a submitted `create_at`, `total_msg_count`, `scheme_id`, `discoverable`, `autotranslation` or `default_category_name` is silently discarded | everything `Channel::patch` applies |
| an empty string | `header: ""` clears, `display_name: ""` **leaves the old value** | `null` leaves, `""` clears, for all four |
| archived channel | 400 `api.channel.update_channel.deleted.app_error`, from the handler | 400 `app.channel.update.bad_id`, from the **store** — `patchChannel` has no guard of its own |
| `props` in the answer | absent | `FillInChannelProps` runs, so a `~mention` in the header comes back as `channel_mentions` |
| a type change | 400 `typechange` — `/privacy` is the only way to convert a channel | not expressible |

The third row is the one worth remembering: the archived-channel guard for `/patch` is
`updateChannelT`'s `DeleteAt != 0` becoming a `store.ErrInvalidInput`, which is why the two routes
answer different ids for the same request.

### Only `town-square` is special

`off-topic` is created by the same team bootstrap and has **no** guard anywhere: it renames,
archives and converts to private, all measured. Four guards mention `model.DefaultChannelName` and
all four test that one name.

### The websocket addressing is team-for-public and channel-for-private on two of the four events

`channel_updated` goes to the channel, `channel_converted` to the team, and `channel_deleted` and
`channel_restored` to the **team for a public channel and the channel for a private one**.
Inverting that last branch broadcasts a private channel's archive to everyone on the team, which no
assertion about a response body can see. None of the five routes carries an `omit_connection_id`:
Go passes an empty string, so unlike `upsertDraft` the client's `Connection-Id` header does not stop
its own tab being told. `mm_app::channel_write`'s module docs carry the table; the parity suite
asserts each event with `common::SocketProbe`.

### Four fields `patchChannel` accepts and refuses, and one it accepts and ignores

On this unlicensed deployment, measured: `autotranslation` → 403
`api.channel.patch_update_channel.feature_not_available.app_error`; `discoverable` → 400
`api.channel.discoverable_join_request.feature_disabled.app_error`; `banner_info` → 403
`license_error.feature_unavailable.specific`. `managed_category_name` is the silent one — it counts
towards "is this patch empty", gets past the gate, and then does nothing at all, so a patch of
nothing but that field is a 200 whose body differs only in `update_at`.

`canEditChannelBanner` has a shape worth naming: its licence branch sets `c.Err` and **does not
return**, falling into a type switch that can overwrite it. So an admin sees the licence error and a
caller without the banner permission sees the permission error, both 403. A port that returned
early would answer the same thing to both.

### The `WebConn` membership cache excludes an archived channel, and it broke a test

A channel-addressed event reaches a connection only if the channel is in that connection's
`allChannelMembers` snapshot, taken from `get_all_channel_members_for_user(user, include_deleted =
false)` and cached for thirty minutes — Go's `WebConn` the same way. That snapshot **omits a
member's archived channels**, so a socket whose snapshot happens to be taken while the channel is
archived never hears its restore, however correct the publish is. A probe opened after the archive
did exactly that and passed in isolation, where the restore's own event populated the snapshot with
`DeleteAt` already zero. The fix is a warm-up event on the live channel, not a longer window: see
the note on `a_private_channels_archive_and_restore_are_addressed_to_the_channel`.

The same suite also found that one probe held open across five sequential exchanges is dropped
under whole-suite load — a `WebConn` whose send queue fills is *disconnected*, not throttled. Four
short tests, each with its own probe, replaced one long one.

### `json.Decoder.Decode` reads one value and stops, and three handlers were stricter than Go

`{"id":"…","header":"h"} trailing` and two concatenated objects are both a **200** on Go, taking
the first value — `json.Decoder` never looks past it. `serde_json::from_slice` rejects the trailing
bytes and would have answered 400. Measured on `PUT /channels/{id}`, `/patch` and `/privacy`; all
three now go through `mm_model::utils::decode_one_from_json`, which the project already had for
exactly this and which also handles the lone-surrogate escape serde refuses.

### The first mutation run's controls were CAUGHT, and the wreckage was why

Both no-op controls failed, which by the standing rule voids a tally. The cause was not a noisy
harness: **every request in the town-square and off-topic tests is expected to fail, and the only
thing stopping it is the guard under test.** So the mutation that removes the archive guard made
the request *succeed* — archiving the shared fixture team's `town-square` for good — and the
rename mutation left its `off-topic` renamed and private. Every later test in the batch then failed
on the wreckage, the controls among them.

Two lessons, and the second is the general one:

* A test whose subject is a **guard** must own a disposable fixture, because a mutation run is
  precisely the case where the guard is gone. Both tests now create their own team.
* **Cleanup that runs only when the test passes is not cleanup.** The off-topic test used to rename
  the shared channel and put it back at the end; the "put it back" never ran on the runs that
  needed it. It now leaves its own team's channel however it likes.

### Mutation testing: 42 run, 40 caught, 2 controls survived

Plan at `scripts/mutations/channel-writes.plan`. The first run reached 40 CAUGHT and then **both
controls CAUGHT**, which voids it — see above for the cause and the fix. The clean re-run landed
after the session that wrote this was cut off by a rate limit: **42 run, 40 caught, both controls
SURVIVED**, so every real mutation in the plan is caught and the harness's verdicts are
trustworthy.

Three mutations had nothing to catch them on the first pass and each named a missing fixture
rather than a shrug: a duplicate channel name, an archived channel's absence from the public
listing, and the banner licence-versus-permission ordering. All three are now asserted, which is
why the re-run caught 40 of 40.

## The post writes, and what a pin actually costs (2026-09-10)

> **On the four counts above.** The sidebar, membership, lifecycle and post-write sections were
> written in parallel worktrees that each branched at **293 of 764**, so each originally counted
> from 293 and the four could not be added up. They are restated here in merge order — sidebar
> 293 → 298, membership 298 → 304, lifecycle 304 → 309, posts 309 → **314** — which is what
> `scripts/routes.py` reports on the merged tree. 21 route+method pairs in one session.

`POST /api/v4/posts/{post_id}/pin`, `.../unpin`, `PUT /api/v4/posts/{post_id}`,
`PUT /api/v4/posts/{post_id}/patch` and `DELETE /api/v4/posts/{post_id}`. 309 → **314 of 764**.
One store primitive, `SqlPostStore.Update`, is behind the first four; `SqlPostStore.Delete` is
behind the fifth. `POST /api/v4/posts` is **not** here — see [D-221], which names what it is
waiting on.

### A pin is an edit, and an edit is two rows

`saveIsPinnedPost` builds a one-field `PostPatch` and goes through `PatchPost` → `UpdatePost` →
`SqlPostStore.Update`, which rewrites the live post **and inserts the old version as a new row**
carrying `OriginalId` and a stamped `DeleteAt`. So pinning a post bumps its `UpdateAt`, drags the
channel's `LastPostAt` to now, and adds an entry to `GET /posts/{id}/edit_history` — four side
effects for a boolean, and the response body is `{"status":"OK"}` either way. The parity suite
asserts all four, because the body asserts almost nothing.
`mm_store::post_store::PostStore::update` says why each of its four statements is where it is, and
that they are deliberately **not** in a transaction.

The two pin routes are not symmetric with each other: `post.IsPinned == isPinned` short-circuits to
a 200 that writes nothing, *before* the edit-time-limit check, so pinning an already-pinned post
cannot 400 on age while pinning an unpinned one can.

### `ParseHashtags` is four ASCII regexes wearing Unicode clothes

`model.ParseHashtags` derives the `Posts.Hashtags` column from the message on every edit that
changes it, and all four of its patterns are transcription hazards: **RE2's `\d` is `[0-9]` and its
`\s` is `[\t\n\f\r ]`, where the `regex` crate's are Unicode.** The 51-case corpus in
`fixtures/behaviour_utils.json` under `parse_hashtags` records `#tag١` — an Arabic-Indic digit that
Go strips as trailing punctuation and a copied pattern would have accepted. Also recorded: `#` is
the one character `puncStart` will not strip and `puncEnd` will, there is no de-duplication
(`#tag #tag` is stored twice), and the 1000-byte cap is measured in **bytes**, cut at 999 and
rolled back to the last space — which is what keeps a split multi-byte rune out of the answer and
what makes a single over-long hashtag come back as the empty string.

### What these routes refuse is a shape, never a route

`mm_app::post_write` has the table. Each entry is a forward, gated on something the request
carries: a `~channel` mention (`FillInPostProps` resolves channels and teams into a prop), an `@`
mention on a licensed installation (the group-mention prop needs the licence's `LDAPGroups` bit), a
change to the file-id set (`processPostFileChanges` attaches and detaches `FileInfo` rows),
`ai_generated_by`, interactive content (`mm_blocks_actions` has to be pruned to the actions the
content still references), a card post, and `ImageProxySettings.Enable`. A parity test asserts that
a `~town-square` in an edited message forwards and the same edit without it does not, so the
refusal is measured rather than assumed.

### Three things the edit path does that a reader would tidy away

* **`null` file ids and `null` props mean "leave them alone"**, and an empty array does not: a
  `PUT` carrying only `id` and `message` keeps a post's attachments, while `"file_ids": []`
  detaches every one of them.
* **The age-limit gate compares file ids *ordered* (`slices.Equal`) and the permission check three
  lines later compares them as a multiset (`SliceEqualUnordered`).** Reordering a post's file ids
  is therefore a change for one and not for the other.
* **An empty patch is a 200 that still writes.** `postPatchChecks` skips the age limit for it and
  `UpdatePost` runs anyway, so `{}` moves `UpdateAt` and adds an edit-history row while changing
  nothing a reader can see.

`props` are replaced wholesale and then the five integration identity markers are re-applied from
the old post, so an edit cannot strip a webhook's `from_webhook` — and `mm_blocks_actions` is
deleted outright on any post without interactive content. All three are asserted in one exchange.

### `deletePost` serves a root and forwards a reply

`SqlPostStore.Delete` is `WHERE Id = $4 OR RootId = $4`: deleting a root soft-deletes the whole
thread, stamps `props.deleteBy` with `jsonb_set`, marks `Threads.ThreadDeleteAt` and soft-deletes
the **replies'** file infos. The root's own file infos, the flagged-post preferences and the thread
drafts are three more cascades Go runs from goroutines; they are inline here, which closes a window
rather than opening one.

Deleting a *reply* is forwarded, because `App.DeletePost` runs `RemoveNotifications` for one and
that is the mention engine — see [D-221]. `?permanent=true` is forwarded too: it selects
`PermanentDeletePost`, a hard delete across seven tables, and Go's own 501 gate answers it.

The route's refusals differ from its neighbours': a post that does not exist is a **404** here,
where the pin and edit routes turn the same lookup failure into a 403.

### The two `post_deleted` events go to two different halves of the channel

`CleanUpAfterPostDeletion` publishes the event **twice**: once tagged `ContainsSanitizedData` and
once tagged `ContainsSensitiveData` with a `delete_by`. `ShouldSendEvent` delivers the first only to
a connection *without* `manage_system` and the second only to one *with* it, so who deleted the post
reaches an admin's client and nobody else's. Two sockets in one parity test, because a port that
published one event would either leak that or lose it. Both payloads carry the post as it was
**before** the delete — `delete_at: 0`, no `deleteBy` — so a client learns the post is gone from the
event type, not from the post in it.

### Mutation testing: 52 run, 50 caught, 2 controls survived

Plan at `scripts/mutations/post-writes.plan`. The session that wrote it was cut off by a rate
limit partway through, and its partial run is not the tally above — that one reported a harness
fault, which voids a run. Re-run from scratch against the **merged** tree.

Two things came out of the void run that the tally would have hidden.

**`store-delete-does-not-mark-the-thread` did not compile.** It mutated the SQL to
`... WHERE postid = $2 AND $1 < 0`, and sqlx types a bind parameter from how the statement uses
it — `$1` as both `SET threaddeleteat = $1` and a comparison operand is E0308. The harness
reported HARNESS FAULT, which reads as though the mutation ran and the server merely failed to
come up. `AND FALSE` says the same thing and compiles.

**Rewritten, it then SURVIVED — and that was a real gap.** Deleting a root post stamps
`Threads.ThreadDeleteAt`, and nothing asserted it. The reason is structural rather than an
oversight: nothing about that stamp reaches the delete response, and every other test here
asserts on the response or on the posts themselves, so the one write that is invisible from the
route performing it was the one nothing checked. It is observable one route over —
`getThreadsForUser` is served — and
`post_writes::deleting_a_root_post_takes_its_thread_out_of_the_list` now asserts it and catches
the mutation.

One caution for the next parallel session: `unreferenced-action-registry-kept` SURVIVED in the
worktree and is CAUGHT against merged `main`. A per-branch mutation verdict is a verdict against
that branch's test binary, which is smaller than the one that ships.

---

## The system posts twelve served routes owed (2026-09-11)

No new routes. Twelve route+method pairs already served — the six on
`/channels/{id}/members…` and the five channel-lifecycle writes, plus `updateChannel`'s
display-name notice — each ended in a `Posts` write Go makes and this server did not. **D-231,
D-232 and D-233 are paid off and deleted**; what is still owed is the narrower [D-235].

| what | where | status |
|---|---|---|
| `SqlPostStore.Save`/`SaveMultiple` for one root post, and `PostPersistentNotification.DeleteByChannel` | `crates/mm-store/src/post_store.rs` (end of both blocks) | done; the insert itself reuses the existing `insert_post` |
| the slice of `App.CreatePost` a server-constructed post reaches, plus the `posted` event | `crates/mm-app/src/post_write.rs` | `create_system_post` propagates, `post_system_message` swallows — Go has both |
| the four membership posts | `crates/mm-app/src/channel_member.rs` | done |
| the six lifecycle posts and the privacy rollback | `crates/mm-app/src/channel_write.rs` | done |
| `updateChannel`'s display-name notice | `crates/mm-api/src/channel_writes.rs` | it lives in Go's handler, not in `App.UpdateChannel` |
| 5 cross-server tests, 7 unit tests | `crates/mm-api/tests/parity/system_posts.rs`, and `#[cfg(test)]` in both app modules | — |

### The message text is English here, and that is a deliberate exception to [D-092]

Every other string this server emits for an `i18n.T` id is the **id**. A post body is not an error
`message` a client ignores — it is stored, and it is what an old client renders — so the twelve
sentences are English literals beside their call sites. This is not an i18n bundle and must not
grow into one. The cost is stated in [D-235]: `DeleteChannel` and `RestoreChannel` interpolate the
**acting user's** locale in Go, so a non-English admin's archive message differs from ours.

### A join post moves the channel without making it unread

`SaveMultiple`'s post-commit `UPDATE Channels` sets `LastPostAt` with `GREATEST` unconditionally
and adds `count` to `TotalMsgCount`, where `count` is zero when
`Post.ExcludesFromChannelMessageCount()`. So a join reorders every member's sidebar and leaves the
unread count alone. **`system_guest_join_channel` and `system_add_guest_to_chan` are not in
`IsJoinLeaveMessage`**, so a guest's join *does* count — the asymmetry is Go's, and it is the one
line here a reader is most likely to "fix".

### Which of the four membership posts is written is decided by who asked

`POST /members` with your own `user_id` writes `system_join_channel`; with somebody else's, it
writes `system_add_to_channel` with four props instead of one. Two of the four are inline in Go and
two run on `a.Srv().Go`, so **a failed join post fails `POST /members`** and a failed add post does
not. `ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages` does not reach any of
these six routes: it gates `JoinDefaultChannels` and `App.LeaveChannel`, which api4's member routes
do not call.

### The privacy post makes a rollback reachable that never was

`UpdateChannelPrivacy` flips the type back, restores the `discoverable` flag and re-updates when
its post fails, then answers the post's error. Nothing could fail while there was no post, so the
branch was recorded as unreachable; it is ported now. It is also the only one of the six lifecycle
posts whose error id can reach a response body.

### `updateChannel` posts against the body, not against what it wrote

Go's condition is `oldChannelDisplayName != channel.DisplayName` where `channel` is the **submitted**
body. A body that omits `display_name` changes nothing and still posts, with an empty new value.
Pinned by `system_posts::update_channel_posts_the_display_name_it_was_sent`.

### Two things the parity fixtures taught on the first run

**Creating a channel already writes a system post.** `CreateChannelWithUser` runs
`postJoinChannelMessage` for the creator, so a fresh fixture channel's timeline is not empty; every
test counts the baseline and slices it off rather than assuming zero.

**Posting as a user whose membership mm-api wrote is a 403 from Go** — [D-190] arriving on
schedule, in a helper that posts to Go by construction.
## The authentication writes, and the counter that is the lockout (2026-09-11)

`POST /api/v4/users/logout`, `PUT /api/v4/users/{user_id}/password`,
`POST /api/v4/users/password/reset`, `POST /api/v4/users/email/verify` and
`POST /api/v4/users/{user_id}/reset_failed_attempts`. 314 → **319 of 764** from this worktree's
branch point. Two new store surfaces underneath: `mm_store::token_store` — the `Tokens` table,
which is **not** `user_access_token_store` — and five auth writes appended to `SqlUserStore`.

`POST /api/v4/users/login` is **not** here, and the reason is not effort: `DoLogin` writes
`Session.Props` from `uasurfer.Parse(r.UserAgent())` — platform, OS and browser strings that
`GET /users/{id}/sessions` returns verbatim — so byte parity needs a port of a user-agent parser,
which is the "measurable only by reimplementing the package" case the standing decision says to
forward. Everything else login needs now exists.

### A password change writes six columns and only one of them was asked for

`SqlUserStore.UpdatePassword` is

```sql
UPDATE Users SET Password = ?, LastPasswordUpdate = ?, UpdateAt = ?,
                 AuthData = NULL, AuthService = '', FailedAttempts = 0 WHERE Id = ?
```

so setting a password **converts the account to e-mail auth and clears the lockout**. That is what
makes `resetPassword` unlock an account that failed its way to the cap, and what lets an admin move
a SAML user back to a password. Every route above it answers `{"status":"OK"}` either way, which is
why the assertions live in `crates/mm-store/tests/db_auth_writes.rs` rather than the parity suite —
`AuthData` goes to SQL `NULL` and `AuthService` to `''`, two different spellings of "none" in two
columns, and a port that wrote only `Password` is invisible on the wire.

### The claim is taken before the password is read, and refunded selectively

`DoubleCheckPassword` increments `FailedAttempts` **conditionally on it being below
`MaximumLoginAttempts`**, refuses if the claim failed, *then* checks the password, and refunds the
slot for every failure except a credential mismatch. Two consequences a reader would lose by
reordering: a correct password cannot unlock an account already at the cap (a 401 with the lockout
id, not the 400 a wrong password gets), and a backend fault or an over-long password cannot lock
anybody out. `mm_app::App::double_check_password` has the order; the parity suite has both
consequences.

The store predicate is strictly `<`, so `MaximumLoginAttempts` is the number of attempts
*allowed*, and `DecrementFailedPasswordAttempts` floors at zero in the `WHERE` clause rather than
in arithmetic — a `-1` would silently grant an extra attempt and nothing reports the column.

### Three of the five routes require no session at all

`logout`, `resetPassword` and `verifyUserEmail` are `APIHandler`, not `APISessionRequired`:
somebody following a reset link cannot log in by definition. `auth_writes::OptionalSession` ports
the `RequireSession: false` path — no token is not an error, a bad token is not either, a **500**
from the session store still is, and a valid non-OAuth session presented in `?access_token=` is a
401. `logout` therefore answers 200 to a caller holding nothing, and clears the cookie
unconditionally, before the revoke and on the error path too.

An **OAuth** session at logout is forwarded: Go's `RevokeAccessToken` also deletes the
`OAuthAccessData` row, and removing only the session would leave a replayable token behind.

### The error ids are uniform on purpose

An unknown account and a wrong password are one id. A token that never existed and a token of the
wrong type are one id. **Every** error out of `VerifyEmailFromToken` — including a 500 from the
store — leaves as one 400 `api.user.verify_email.bad_link.app_error`, because the handler wraps
rather than returns. Each is an enumeration oracle if split, and `mm_app::auth`'s module docs say
so at the top so that the next reader does not "improve" one.

Two that are *not* uniform and look like they should be: `already_hashed=true` without the
permission is a **401** for yourself and a **403** for anybody else, and `resetPasswordFailedAttempts`
raises a hand-built 403 with its own id for the first permission check and the generic
`SetPermissionError` for the second.

### `Token.Extra` is Go-cased JSON

`{"UserId":"…","Email":"…"}` — an anonymous struct with no tags, so `encoding/json` uses the field
identifiers. A port that used the wire casing would parse every live token to the zero value and
404 them all. Confirmed against a row Go minted rather than read off the source.

Tokens are consumed **only on success**, and the delete's failure is logged and swallowed: the
password has already committed by then. A refused reset leaves the row, so the link stays
retryable — asserted for both the expired case and the wrong-type case.

### What the parity suite had to be taught about Go's caches

Five of its fifteen tests failed on the first run, all for the same reason and none of them a port
bug: Go answers `login`, `GetUser` and `GetSession` from in-process caches that this server's
writes do not reach. Three tests now call `invalidate_go_caches` explicitly, one reads
`Users.EmailVerified` out of the table because `SanitizeProfile` strips it from every API response,
and one asserts the staleness **on purpose** — a session revoked here is still accepted by Go, which
is [D-237] and is the first time [D-190]'s class has had a credential consequence.

Three more, recorded rather than fixed: [D-238] (no e-mail service, so four send-only routes stay
with Go and two writes lose a notification), [D-236] (CSRF is checked on no migrated route, which
predates this work and is written down here for the first time), and [D-237] above.
---

## Channel creation (2026-09-11)

Three routes, all served: `POST /api/v4/channels`, `POST /api/v4/channels/direct`,
`POST /api/v4/channels/group`. `POST /channels` was deferred from the lifecycle session because it
adds the creator as a member; `Channel().SaveMember` landed there, and this is the first of the
routes it unblocks.

| what | where | status |
|---|---|---|
| `Channel().Save`, `SaveDirectChannel`, `saveChannelT`, `GetTeamChannels`' count | `crates/mm-store/src/channel_store.rs` (end of both blocks) | done; `Save` mutates the channel it is handed, because `PreSave` mints the id and the timestamps in place |
| `StoreError::LimitExceeded` | `crates/mm-store/src/error.rs` | done; a store-enforced quota the app layer answers **400** to, where every other write failure is a 500 |
| `App.CreateChannelWithUser`, `CreateChannel`, `GetOrCreateDirectChannel`, `createDirectChannel`, `CreateGroupChannel`, `addChannelToDefaultCategory` | `crates/mm-app/src/channel_create.rs` | done, minus the join system post ([D-231]) |
| `TeamSettings.MaxChannelsPerTeam` | `crates/mm-app/src/config.rs` | done; Go default 2000, and a negative value disables the store's half of the check only |
| `model.NonSortedArrayFromJSON` | `crates/mm-model/src/utils.rs`, corpus in `reference/dump/behaviour.go` | done; the DM route reads its list positionally, so the order is wire surface |
| the three handlers | `crates/mm-api/src/channel_creates.rs` | done; a licensed installation forwards `POST /channels` only ([D-234]'s reading), and two branches of the message routes forward ([D-235], [D-236]) |
| 16 cross-server tests, 6 store tests, 1 corpus test | `crates/mm-api/tests/parity/channel_creates.rs`, `crates/mm-store/tests/db_channel_creates.rs`, `mm_model::utils::go_parity` | — |

### The same store outcome is a 400 on one route and a 201 on the other two

`saveChannelT`'s insert is `ON CONFLICT (TeamId, Name) DO NOTHING`, and on a miss it re-selects the
row that holds the name and returns it **alongside** `ErrConflict`. Three callers read that pair
differently: `CreateChannel` reports `store.sql_channel.save_channel.exists.app_error` at 400 and
throws the channel away, while `GetOrCreateDirectChannel` and `CreateGroupChannel` swallow the error
and answer the existing channel with a **201**. A `Result` cannot carry a meaningful value on its
error arm, so the conflict is a value here — `ChannelSave::Existing` — and the branch stays where Go
put it, above the store.

The re-select has **no `DeleteAt` filter**, which is why an archived channel still holds its name:
archive `town-square-clone` and re-create it and you get the 400, not a fresh channel. Measured on
both servers.

### The per-team limit is checked twice against two different counts

`CreateChannelWithUser` compares `GetNumberOfChannelsOnTeam() + 1` to `MaxChannelsPerTeam`, and
`saveChannelT` then compares a second count to the same setting. They do not count the same rows:

| | types | archived |
|---|---|---|
| `GetNumberOfChannelsOnTeam` (app) | `O`, `P`, `G` | **counted** |
| `saveChannelT` (store) | `O`, `P` | not counted |

So a team can be refused by the first with `api.channel.create_channel.max_channel_limit.app_error`
and accepted by the second with no error at all. Both are ported. Go's app-layer count also comes
from a **list** method that answers `ErrNotFound` for zero rows, so an entirely empty team is a
**404** rather than a count of zero; `count_team_channels` returns the number and
`get_number_of_channels_on_team` raises the 404, so the status stays where the status belongs.

### Three creates, three different event addressings

| route | event | addressed to | published |
|---|---|---|---|
| `POST /channels` | `channel_created` | the **user** (`channel_id` and `team_id` on the broadcast are empty) | once |
| `/channels/direct` | `direct_added` | the **channel** | once, and only when the DM did not already exist |
| `/channels/group` | `group_added` | each **member** individually | once per member |

Getting the first one wrong announces a new private channel to everyone; getting the third wrong
sends one frame where Go sends N. Both are asserted with `SocketProbe`.

`direct_added`'s `creator_id` is `userIds[0]` **from the request body**, not the session's user —
the handler passes the two ids positionally. The parity test sends the pair largest-id-first so that
a sorted parse and a body-ordered parse give different answers; without that the assertion would
pass half the time by luck.

`group_added`'s `teammate_ids` is sorted, and the sort is not obvious from the Rust: Go's
`GetGroupNameFromUserIds` sorts the caller's slice **in place**, and `CreateGroupChannel` then
marshals that same slice into every event. The Rust helper does not mutate its argument, so the sort
is explicit at the publish site — without it the field would carry the request's order.

### `addChannelToDefaultCategory` is ported for a new channel only

Go's function also *moves* a channel out of the category it is already in. That half is dead for a
create — nothing can reference an id the database learned about a millisecond ago — so only
find-or-create is ported, and the doc comment on `App::add_channel_to_default_category` is the
record of why. The match is case-insensitive and against `custom` categories only, so a
`default_category_name` of `"channels"` makes a *second* category rather than filing into the
built-in one. The whole thing is fire-and-forget: Go logs every failure and returns nothing.

### What forwards, and why each one

* **A licensed installation, on `POST /channels` only.** `PrivacySettings.UseAnonymousURLs` behind
  `MinimumEnterpriseAdvancedLicense` would *replace the client's channel name with a fresh id*, and
  managed channel categories read the licence again. Neither message route consults the licence, so
  neither is gated.
* **`RestrictDirectMessage = "team"`** — [D-239]. Needs a store method this port does not have and a
  plugin decision it cannot make.
* **A view-restricted caller** — [D-240], the same wall `GET /users/by_auth_data` already hits. Both
  forwards are returned *before* anything is written.

### Two fixture-generator diffs that are not mine

`TZ=Asia/Kolkata go run .` in `reference/dump` also rewrites `behaviour_filestore.json` (a random
multipart boundary) and `behaviour_scheduled_post{,_recurrence}.json` (this machine's tzdata rejects
`america/new_york` in lower case where the committed fixture accepted it). Both are environmental
rather than caused by any change here, and both were reverted — only `behaviour_utils.json` is
committed, and only as an addition. Worth knowing before the next session reads a dirty `git status`
as a signal.

### Mutation testing: 38 run, 36 caught, 2 controls survived

Plan at `scripts/mutations/channel-creates.plan`. Run it with
`MUTATE_STORE_TARGETS='--test db_channel_creates'` — without it the `store` lines build every
mm-store test binary per mutation and let an unrelated one decide the verdict.

Every one of the 38 was compiled before the batch ran. That is a step this project has paid for
twice now: a plan line that does not compile is reported as a HARNESS FAULT that reads as though
the mutation ran, and a fault voids the whole run. Applying each mutation, running
`cargo check --workspace`, and reverting takes about nine seconds a line against the batch's
fifty, and it caught nothing this time — which is the point of a cheap pre-flight.

**One mutation survived, and the fixture was the reason.** `create-channel-private-gate-is-the-\
public-permission` swaps `create_private_channel` for `create_public_channel` in the handler's type
switch. It survived because on a stock installation `team_user` grants **both** and `system_user`
grants **neither**, so every ordinary member passes either check and the two ids are
indistinguishable from outside. `the_private_create_is_gated_on_its_own_permission` now plants a
role with only `create_public_channel`, hangs it off a throwaway team's scheme as
`DefaultTeamUserRole`, and asserts that a member of that team gets a 201 for `O` and a 403 for `P`
on both servers. Re-run: CAUGHT.

The two controls — a renamed binding in `mm_app::channel_create` and two reordered independent
predicates in `count_team_channels` — both SURVIVED, so the verdicts above mean what they say.

Three mutations were considered and **not** written because they are genuinely unobservable rather
than untested, and it is cheaper to say so than to rediscover it:

* **Swapping the `team_id` and `display_name` emptiness checks.** Both answer
  `api.context.invalid_body_param.app_error`, and `AppError`'s `params` map is not serialised — so
  the two 400s are byte-identical and only the (wiped) `detailed_error` differs.
* **Inverting `createChannel`'s discoverable *type* check.** The feature flag is false and fires
  first, so nothing downstream of it is reachable.
* **`sorted_array_from_json` in place of the non-sorted one on the GM route.** The GM name is a
  hash of the sorted ids, `teammate_ids` is re-sorted at the publish site, and both parsers
  de-duplicate — so the two are observationally identical *there*. They are not on the DM route,
  which is why that mutation is in the plan and is caught.
## The team membership writes (2026-09-11)

`POST /api/v4/teams/{team_id}/members`, `POST …/members/batch`,
`PUT …/members/{user_id}/roles` and `PUT …/members/{user_id}/schemeRoles`. **314 → 318 of 764**
(counted on this branch; the four parallel worktrees this round each branched at 314, so the
merged number is the one `scripts/routes.py` reports on `main`). Behind them:
`TeamStore::update_member` and `TeamStore::save_member`,
`GroupStore::admin_role_groups_for_team_member`, and `mm_app::team_member` — the whole of
`JoinUserToTeam` bar two writes. `DELETE …/members/{user_id}` is **not** here; see the dependency
note at the end.

### Two of the four error ids say `api.channel.` while describing a team

`changing_guest_role` and `scheme_role` (app/team.go:432 and :467) sit in the
`api.channel.update_team_member_roles.*` family; their three siblings on the same function say
`api.team.`. Go's copy-paste, on the wire, and asserted against the running server rather than
inferred. `mm_app::App::update_team_member_roles` carries the note.

### `serde` deserializes a struct from a sequence, and Go does not

`from_slice::<TeamMember>(b"[]")` succeeds — a derived `Deserialize` accepts the sequence form and
`#[serde(default)]` fills the missing tail — so the first version of `addTeamMember` answered
`api.context.invalid_body_param.app_error` where Go answers the route's own
`api.team.add_team_member.invalid_body.app_error`. Both add routes now screen the `serde_json::Value`
before converting; see `decode_team_member`. This is a whole class, not one route: any handler that
decodes a Go struct straight from the body has it.

### One add publishes `added_to_team` twice

`App.JoinUserToTeam` publishes it (team.go:891) and `App.AddTeamMember` publishes it again on top
(team.go:1158). `AddTeamMembers` does the same per successful user. A port that sent one event
passes every response-body test and every count assertion a reader would think to write.

### The add path's two framings, and its two response *types*

`addTeamMember` is `json.NewEncoder(w).Encode` (trailing newline); `addTeamMembers` is
`w.Write(json.Marshal(...))` (none). And `?graceful=` — any **non-empty first value**, `0`
included — swaps a bare list of `TeamMember` for a list of `{user_id, member, error}` in which the
unset half is `null` rather than absent.

### The check order differs between the two add routes

`addTeamMember` checks the permission and *then* reads the team for the group-constrained branch;
`addTeamMembers` reads the team and takes that branch **before** the permission check. So on a
group-constrained team a caller holding nothing gets the group refusal from one route and a 403
from the other.

### `IsTeamEmailAllowed` is an AND across two restriction lists

`[team.AllowedDomains, TeamSettings.RestrictCreationToDomains]`, every non-empty entry of which
must accept the address — so a team allowing `example.com` on a server restricted to
`corp.example.com` admits nobody. An empty entry is skipped rather than matched, which is why the
stock configuration admits everybody. 29 cases in `fixtures/behaviour_team_email.json`, generated
from the Go function.

### The stock `team_user` role holds `add_user_to_team`

A plain member can add a third party. What they lack is `manage_team_roles`, so the answer runs
through `SanitizeRoleData` and reaches the client with `delete_at: **-1**` on a membership that
was just created with `delete_at = 0`. Measured: this suite asserted a 403 first and Go answered
201.

### `UpdateMember` reports no miss, so a vanished membership is a 200

Go `Exec`s the UPDATE and never looks at the rows affected, then computes the answer from the
**in-memory** member. `PUT …/roles` for a user who is not on the team therefore answers 200 having
written nothing — unlike the channel twin, whose re-select turns that into a 404.

### The `/following` validator order has no wire-visible oracle

`RequireUserId().RequireThreadId().RequireTeamId()` — **thread before team**, the opposite of the
read route on the same prefix. Which parameter Go names reaches a client only through the
translated `message`, and we send the raw error id there ([D-092]), so every ordering of the
three produces byte-identical output from us and a mutation swapping two of them survived the
parity suite. `mm_api::thread_writes::first_invalid_following_param` is that branch extracted so
a unit test can be the oracle; the mutation is caught there instead.

### Mutation testing: see the tally in the session report

Plan committed at `scripts/mutations/team-member-writes.plan`, which also records the four
branches it deliberately does **not** mutate and why each is unreachable from this stack
(`MaxUsersPerTeam`, the phase-2 migration gate, the group-constrained forward, and any guest
member).

### What `DELETE …/members/{user_id}` is waiting on

`RemoveUserFromTeam` → `LeaveTeam` needs three store methods this session did not own:
`ChannelStore::GetTeamSpaceChannelsForUser` and `ChannelStore::ClearSidebarOnTeamLeave`
(`channel_store.rs`, a sibling worktree's this round) and `UserStore::UpdateUpdateAt`
(`user_store.rs`, likewise). The websocket events, the soft-delete of the membership and the
preference cleanup are all straightforward once those exist. See [D-242] for the `UpdateUpdateAt`
gap, which the *add* path shares.

## Two cross-test races, and a safety argument with one counterexample (2026-09-11)

A baseline run of the full suite on the merge stack, on unchanged code, failed one test per run
and a *different* test each run. Neither was a port bug; both are now fixed and the suite is
green at 3373 across 56 targets. No route changed.

The first: `user_by_email::a_plain_caller_reads_an_admin_address` is the only byte comparison in
that file whose subject is the **shared admin** rather than a plain fixture user, and
`custom_status_writes` writes that row. `fetch_both_raw` reads Go then Rust, so a concurrent
`Users.UpdateAt` bump lands between them — 39ms, `update_at` alone. It uses `fetch_both_stable`
now; the test's doc comment says why its siblings must not.

The second is the more interesting one, because the harness asserted its own safety.
`common::invalidate_go_caches` carried a written argument that it could not break anything:
invalidation only makes Go **fresher**, and every staleness assertion in the suite is one-sided
in that direction. That is true of all of them but one —
`auth_writes::a_session_revoked_here_is_gone_here_but_lingers_in_gos_cache` asserts Go is
**stale**, and it is the tripwire on [D-237]. `POST /caches/invalidate` is global with six
concurrent callers, so a firing from `system_usage` turns that tripwire into a false "D-237 can
be closed". See `common::GO_CACHE`, which the helper takes itself so a future caller inherits it.
## Three of the five thread writes (2026-09-11)

`PUT /users/{user_id}/teams/{team_id}/threads/read`, and `PUT`/`DELETE` on
`…/threads/{thread_id}/following`. Handlers in `crates/mm-api/src/thread_writes.rs`, app layer in
`crates/mm-app/src/thread.rs`, store in `crates/mm-store/src/thread_store.rs`
(`mark_all_as_read_by_team`, `maintain_membership`, `get`). Suite:
`crates/mm-api/tests/parity/thread_writes.rs`, 13 tests.

The other two — `PUT …/read/{timestamp}` and `POST …/set_unread/{post_id}` — are **deferred**,
not skipped: both write `ThreadMemberships.UnreadMentions` from `countThreadMentions`, which needs
three `GroupStore` methods, `PostStore::GetPostsByThread` and the Markdown mention parser. See
[D-250].

### `UpdateViewedTimestamp` is `state`, not `true`

The one line in `UpdateThreadFollowForUser`'s options a reader reconstructs wrongly: a **follow**
also marks the thread read — `LastViewed` to now, `UnreadMentions` to zero — and an unfollow
touches neither while still moving `LastUpdated`. Consequences documented on
`mm_app::App::update_thread_follow_for_user`: a redundant follow is not idempotent, and any
fixture that follows a thread loses the read mark it planted.

### `MarkAllAsReadByTeam` carries four predicates fewer than the threads list

No `Following`, no channel-membership `EXISTS`, no `ThreadDeleteAt = 0`, no
`LastReplyAt > LastViewed` — so it marks read what the list would never show, and it compares
`ThreadTeamId` **without** `COALESCE`, unlike every read query in the same file. Both facts are on
`mm_store::thread_store::SqlThreadStore::mark_all_as_read_by_team`.

### Unfollowing a thread you have no row for creates one

The insert branch of `maintainMembershipTx` is unguarded and takes `Following` from
`opts.Following` regardless of `UpdateFollowing`. The row it leaves behind changes which 404 the
read route answers, from `app.user.get_thread_membership_for_user.not_found` to
`app.user.get_threads_for_user.not_found` — asserted in the suite rather than inferred.

### `/threads/read` is a static sibling of `{thread_id}`, and matchit has no method dimension

So the static route wins for **every** method, including the `GET` gorilla falls through to
`getThreadForUser` with `read` as the thread id. The method-router fallback forwards it and Go
answers its own 400; `parity::thread_writes::a_get_on_the_read_path_is_still_gos_404_shaped_400`
is what would notice if that stopped being true. Note that test compares the two bodies directly
rather than through `assert_error_bodies_match_except_known_gaps`, whose "our message is the raw
id" pin is false of a proxied answer.

### Mutation testing: see the tally in the session report

Plan committed at `scripts/mutations/thread-writes.plan`. Its header records why the verdicts
depend on the fixture *planting* non-zero, mutually distinct `LastViewed`, `LastUpdated` and
`UnreadMentions`: with three zeroes, "left alone", "rewritten to the same value" and "zeroed" are
the same observation and most of the plan would survive while proving nothing.
## The slash-command writes (2026-09-11)

`POST /api/v4/commands`, `PUT|DELETE /api/v4/commands/{command_id}`, and the two sub-routes
`PUT …/move` and `PUT …/regen_token` — five routes, closing the write half of `api4/command.go`.
`executeCommand` and the two autocomplete routes remain forwarded. New:
`crates/mm-api/tests/parity/command_writes.rs`, `scripts/mutations/command-writes.plan`; the rest
extends `command_store.rs`, `mm-app/src/command.rs` and `mm-api/src/commands.rs`.

### The built-in registry is 33 strings, and the parity suite is its oracle

`validateCommandTriggerUniqueness` asks ~35 provider objects for a `*model.Command` and reads
`.Trigger` and nothing else, so the whole of what the check needs is a list. Two providers return
`nil` on a stock server, which *frees* their trigger: `/test` needs `EnableTesting` and
`/exportlink` a feature flag this port does not model ([D-260]). Because a transcribed list is
exactly the thing that is quietly wrong, `parity::command_writes` posts every entry to both
servers and demands the same refusal, and posts the two free ones and demands the same
acceptance. See `BUILT_IN_COMMAND_TRIGGERS` in `crates/mm-app/src/command.rs`.

### The 404-for-a-403 rule is not uniform across the family

Rung one of the ownership ladder — no `manage_own_slash_commands` on the command's team — is a
404 in all five writes and in `getCommand`. Rung two — not the creator and no `manage_others` —
is a plain **403** in the writes and a 404 in `getCommand`. So `PUT /commands/{id}` tells two
refusals apart that `GET /commands/{id}` deliberately does not. `moveCommand` is further out
still: its `manage_own` check is on the *destination* team and runs before the command is
fetched, so a caller who fails rung one is refused there with a 403. Measured — the ladder test
expected 404 and Go answered 403.

### `UpdateAt` is stamped in the store, and that is load-bearing

`App.MoveCommand` and `App.RegenCommandToken` never mention the field; `SqlCommandStore.Update`
assigns it before validating. Moving that assignment up a layer — the shape the webhook store
uses — leaves both routes writing a stale `UpdateAt`. `command_store.rs` says so at the trait.

### Go's `SqlCommandStore.Delete` cannot fail

`if err != nil { errors.Wrapf(err, …) }` with the result discarded, so it returns `nil`
unconditionally and `app.command.deletecommand.internal_error` is dead code. This port returns
the driver error, which diverges only on a database that is already down. Recorded in the doc
comment on `CommandStore::delete`, not as debt.

### Every JSON body in this module was missing Go's HTML escaping

`encoding/json` escapes `&`, `<` and `>`; `serde_json::to_string` does not, and a slash command's
`url` is the field that makes that reachable — `?a=1&b=2` is an ordinary callback. `encoded` now
goes through `go_json_marshal`, which also fixes the already-shipped `getCommand` and
`listCommands`. `regenCommandToken` is the one body here written with `w.Write` rather than the
encoder, so it alone carries no trailing newline and only the token.

### `POST` is deliberately unregistered on `/commands/{command_id}`

`/api/v4/commands/execute` matches that pattern and this router does not carry it, so the method
fallback is the only thing still forwarding it. `create_command` lives on `/api/v4/commands`;
registering it on the parameterised path as well would swallow `executeCommand` silently.
`parity::command_writes::the_execute_route_is_still_forwarded` is the guard.

### Mutation testing, and the survivor that was a bug in the test

**39 run, 36 caught, 3 survived, 0 harness faults** (`scripts/mutations/command-writes.plan`).
Two survivors are the required no-op controls. The third, `app-update-takes-the-bodys-team`, is a
documented equivalent mutant: `updateCommand` refuses unless the body's `team_id` already equals
the old command's, so the copy below it cannot be observed through any request. `plugin_id` — the
same assignment with no handler guard in front of it — is mutated in its place and is caught.

Two more survived the first run and were **findings about the test**, not equivalents: the move
test searched for trigger `mmrscolg` while writing `mmrscol{tag}` with `tag` already `colg`, so
`occupied()` found nothing, took its `else { return; }` meant for a machine with no database, and
the test passed having run none of its move assertions — including the `team_id` read-back — and
never reaching its own `sweep` (227 rows had leaked). `occupied` now **panics** on a missing row
and returns `None` only for a missing `DATABASE_URL`, and the trigger is one binding used both to
create and to look up. Both mutations are caught since.
## The personal-access-token writes (2026-09-11)

`POST /api/v4/users/{user_id}/tokens` and `/users/tokens/{revoke,disable,enable,rotate,search}`
and `/users/tokens/non_compliant/revoke` — seven route+method pairs, the whole write half of the
family whose reads landed on 2026-09-08. Touched: `crates/mm-store/src/user_access_token_store.rs`,
`crates/mm-app/src/user_access_token.rs`, `crates/mm-api/src/tokens.rs`, the router and one
config field (`enable_user_access_tokens`). New: `crates/mm-api/tests/parity/token_writes.rs`,
`scripts/mutations/token-writes.plan`. No new model type: `UserAccessTokenSearch` was already
ported into `mm-model/src/search_requests.rs`, where Go's own separate file put it — this session
wrote a second copy beside `UserAccessToken` before noticing, which is exactly the silent fork of
a wire type that grouping was meant to prevent.

### The secret is on the wire exactly twice

Creation and rotation return the token with `Token` populated; every other route blanks it, and
`omitempty` turns the cleared string into an absent key. A port that sanitised these two for
symmetry would hand clients a credential they can never learn. See
[`App::create_user_access_token`](crates/mm-app/src/user_access_token.rs).

### Revoke, disable and rotate delete the session the token minted — in the store

The join is `Sessions.Token = UserAccessTokens.Token`, on the **secret**, so on rotate the DELETE
must precede the UPDATE or every session the old secret minted is orphaned: still valid, still
authenticating, no longer reachable from the row that would revoke it. `delete_sessions_for_token`
in the store is the one copy of that statement.

### The search term is an equality, not a pattern

`sanitizeSearchTerm` escapes `%` and `_` and nothing wraps the term, so `seed` does not find
`seed-bot` and `%` finds nothing at all. Measured against the running server before it was
written; a port that "fixed" it into `%…%` returns rows Go does not.

### An empty `token_id` is a 404, not the 400 the code appears to set

Revoke, disable and enable all do `if tokenId == "" { c.SetInvalidParam("token_id") }` **without
returning**, so that error is overwritten by the 404 from looking up the empty id. Rotate's
identical-looking three lines *do* return, so its 400 is real. Both confirmed against Go.

### `json.Decoder.Decode` is not `serde_json::from_slice`

An array is an error in Go and is not in serde (which fills a struct positionally); a `null` is
*not* an error in Go (the struct keeps its zero value); trailing bytes after the first value are
ignored. Each difference changes which parameter the 400 names, so
[`decode_go_struct`](crates/mm-api/src/tokens.rs) ports all three.

### `Store.Delete` reports success when its transaction failed

Go guards the commit with `if err := …; err == nil` and then returns `nil` regardless, so a failed
revoke answers `{"status":"OK"}` having deleted nothing. Reproduced — it is on the wire — and
logged at error level, because nothing else would record it. `UpdateTokenDisable`, in the same
file, propagates instead.

### `enable` is gated on *create*, `disable` on *revoke*

Not a symmetry: if enable took the revoke permission, a caller whose only power is to withdraw
credentials could re-arm every one they had disabled. An admin holds both, so
`parity::token_writes::enable_and_disable_are_gated_on_opposite_permissions` plants a role holding
exactly one.

### Our 400s cannot be told apart, because the parameter name is message-only

`NewInvalidParamError` puts the parameter in the AppError's **params**, which Go never serialises;
a client learns whether it was `token_id` or `rotate_user_access_token` only from the translated
`message`, and ours is the raw id until i18n lands ([D-092]). So over HTTP those two 400s are the
same document, and no parity assertion on **our** body can separate them. A mutation run proved
it: making `decode_go_struct` reject a JSON `null` changed which branch every route took and the
suite did not notice, because the assertion pinned *Go's* message, which the mutation cannot move.
The null branch is now asserted in `mm_api::tokens`'s unit tests, where it is visible; the array
branch stayed in the parity suite because it has an observable form — serde fills a struct from a
sequence **positionally**, so a six-element array would mint a real token (200) where Go answers
400.

### `detailed_error` is empty on every error body here, and everywhere else

`MakePermissionError` fills it with `userId=…, permission=…` and `handleContextError` then wipes it
unless `ServiceSettings.EnableDeveloper` (web/handlers.go:436). So the appended
", attempted access by oauth app" the five OAuth refusals build is reproduced for the log and for
developer mode, and reaches no client on a stock server.

### What the writes are still missing

The `Audits` rows every one of these handlers writes ([D-270], the first entry for a gap every
migrated write shares), and the create/rotate notification e-mails ([D-238], whose "two writes are
silent" is now four). Neither changes a response byte.

### A fixture sweep keyed on a column a mutation can change is not a sweep

`token_writes::sweep` deleted by the `mmrs-write` description every body here posts — and the
`store-save-token-and-description-swapped` mutation writes the *secret* into that column, so two
rows outlived it and the **reads** suite's `an_empty_page_is_an_empty_array` failed hours later on
debris from next door. It sweeps by owner now (`TOKEN_BOT` owns nothing else).

### Mutation testing: 35 run, 33 caught, 2 controls survived

Two passes. The first was 30 caught, 2 real survivors and one harness fault (a replacement whose
`$2 = $2` sqlx could not type-check); the second re-ran those three after the fixes above and
caught all three, with both no-op controls surviving as they must.

Plan at `scripts/mutations/token-writes.plan`, which also records the three things it deliberately
does **not** mutate — the session join (dropping either predicate wipes every session on the
installation), `DeleteNonCompliantExpiry` (unreachable while no lifetime policy is set) and the
remote-user and system-admin-target gates (no reachable false side on this stack).
## The bot writes (2026-09-11)

Five of the six writes in `channels/api4/bot.go`: `POST /bots`, `PUT /bots/{bot_user_id}`,
`POST /bots/{bot_user_id}/{disable,enable}` and `POST /bots/{bot_user_id}/assign/{user_id}`.
`convertBotToUser` is deliberately left in Go. Behind them: `SqlBotStore::Save`/`Update`,
`UserStore::save`/`permanent_delete`, and `App.CreateBot`/`PatchBot`/`UpdateBotActive`/
`UpdateBotOwner`. `SessionHasPermissionToManageBot` was already ported for
`SessionHasPermissionToUserOrBot` and is reused unchanged.

Tests: 7 parity (`parity::bot_writes`), 12 store DB (`db_bot_store`), 5 unit. Full workspace run
56 targets, 3392 passed. Mutations: 35 run, 30 caught, 2 controls survived; the three non-control
survivors are the two below and `assignBot`'s id-check order, all three invisible to a client.

### There is no `enabled` column, and only one of the two rows it writes is idempotent

Disabling a bot soft-deletes `Users` *and* `Bots`. `UpdateBotActive` guards the `Bots` write with
Go's `changed` flag while `UpdateActive` above it runs unconditionally — so a second
`POST /disable` answers with the **first** disable's `update_at` while still bumping
`Users.UpdateAt`. Measured against the running server. See `mm_app::App::update_bot_active`.

### `Bot().Update` answers with the row it re-read, which is why `PatchBot` writes `Users` first

The store copies five fields onto the stored join and returns *that*, so `username` and
`display_name` in the answer come from `Users` rather than from the caller's bot. `App.PatchBot`
writes the user row first for exactly that reason; reversing the two answers with the old username
while having stored the new one. See `mm_store::bot_store::BotStore::update`.

### Renaming a bot rewrites its email, and nothing on the wire says so

`UserFromBot` regenerates the address as `<username>@localhost`, and `PatchBot` copies it onto the
user row along with `Id`, `Username` and `FirstName`. A `model.Bot` shows none of that, so the
parity suite reads both tables — `common::bot_and_user_rows`.

### The database picks the field a duplicate username is reported under, not Go's order of checks

A bot's email derives from its username, so a clash violates both unique indexes, and Go tests its
email list first — which reads as "email wins". It does not: Postgres names one constraint per
error and `IsUniqueConstraintError` is a substring test over that text. The answer is `username`
on both servers. `db_bot_store.rs` asserted the intuitive reading and failed; the finding is on
`mm_store::user_store::UserStore::save`.

### Three orderings, each invisible to a single-gate test

`createBot` decodes the body before checking the permission and checks the permission before the
feature flag, so three callers get three different answers and only an admin ever learns bot
creation is disabled. `patchBot` likewise decodes before the manage gate, so a caller who may not
know the id is a bot still gets a 400 for a malformed body. `assignBot` validates `user_id` before
`bot_user_id`. All three are in `parity::bot_writes`; the third is **not** catchable, below.

### What a client cannot see, and therefore no parity test can pin

`model.AppError`'s parameter map and `MakePermissionError`'s detail both carry `json:"-"`. So
*which* parameter a 400 named and *which* permission a 403 named are absent from the wire —
`detailed_error` is empty on both servers. Two mutations exercise this and are expected to survive;
`scripts/mutations/bot-writes.plan` lists them under their own heading rather than among the
controls.

### `POST /bots` answers 403 on this deployment and always will

`ServiceSettings.EnableBotAccountCreation` defaults false and the stack leaves it there on purpose.
The refusal is what the parity suite compares; the success path is covered by `db_bot_store.rs` and
a unit test, and the gap is [D-280]. Three more divergences on that path and the one below it are
[D-281] (no owner DM), [D-282] (`userDeactivated`'s cascade) and [D-283] (the OAuth arm of session
revocation).

### One survivor nothing can catch: `DeleteAt = UpdateAt` is one clock read

Reading the clock twice instead would put the two columns a millisecond apart on an unlucky run,
and both servers would drift the same way — so the right answer and the wrong answer coincide, and
a test that could tell them apart would fail intermittently on correct code. Recorded on
`mm_app::App::update_active_for_bot` and in the plan's "invisible on the wire" section rather than
papered over.

### A mutation that makes a write *succeed* needs a fixture that can undo it

`api-create-flag-is-inverted` inverts the `EnableBotAccountCreation` guard, so the mutated server
actually created the bot — and `the_create_gates_fire_in_gos_order` asserted that row's absence
**without removing it**. Every later api mutation in the batch then failed that test for a reason
that had nothing to do with it, both no-op controls included: 23 void verdicts. `count_users_named`
became `common::remove_users_named`, which deletes what it counts so a failing assertion cleans up
after itself, and the api half was re-run. The rule generalises past this route.

### `db_bot_store.rs` stopped sweeping `mmrsbot%`

The parity binary plants under that prefix from a **different process**, which no mutex in either
can serialise; this file now sweeps only `mmrsbotstore%`/`mmrsbotowner%` plus the usernames its own
creates mint. Within the parity binary, `common::BOT_FIXTURES` serialises the reads suite against
the writes suite for the same reason.

### The next route in this family

`convertBotToUser` (`POST /bots/{bot_user_id}/convert_to_user`). Its gate is the simplest in the
file — a bare `manage_system` — and three of the five things `App.ConvertBotToUser`
(app/user.go:2942) does are already here: `User().Get`, `User::patch` and `App.UpdateUser`. Two are
not: `App.UpdateUserRoles`, reached only when `?set_system_admin=true` and the user is not already
an admin, and `BotStore::permanent_delete` — one `DELETE FROM Bots`, which is the step that makes
the conversion irreversible. `App.update_password` exists in `mm_app::auth` but under a different
name from Go's `UpdatePassword`; check which of the four variants matches before calling one.

## A second flake pass, and an assertion that was wrong about Go (2026-09-11)

Twelve full runs on the merge stack, deliberately under load, hunting [D-284]. It never
reproduced — but three *other* failures did, none of them port bugs, and one of them was a test
asserting something untrue of the Go server.

`user_get`'s etag pair read the **shared admin**, whose `Users.UpdateAt` a sibling suite bumps;
when that lands between the unconditional GET and the `If-None-Match` GET the etag has genuinely
changed and **200 is correct**. The pair retries now, as `fetch_both_stable` does for bytes.

The one worth reading: `bot_writes` asserted `Users.DeleteAt == Users.UpdateAt` exactly, under the
comment "UpdateActive reads the clock once". It reads it **twice** — `UpdateActive` stamps both
columns, then `SqlUserStore.Update` calls `PreUpdate`, which re-stamps `UpdateAt` unconditionally
(`model/user.go:563`). The columns are equal only when two `GetMillis()` calls share a
millisecond; measured at `470` vs `471`. Our port makes the same two reads in the same order, so
the parity was exact and only the assertion was false.

Its sibling `bots` failure was the *same* fault: panicking at that assertion skipped the disable
test's `unplant_bot` cleanup, leaving two disabled bots, which broke a `with.len() == without.len()
+ 1` count elsewhere. One bug, two red tests, and a lock that could not have helped — residue
outlives it. That count is a subset assertion now.

[D-284] itself now carries the negative result and the one environmental difference worth
checking: two worktrees shared stack 1 during the round that produced all three reports.
`scripts/worktree.sh` refuses that configuration as of this session.
## Custom profile attributes: seven routes whose gate is five different answers (2026-09-11)

`listCPAFields`, `createCPAField`, `patchCPAField`, `deleteCPAField`, `patchCPAValues`,
`listCPAValues` and `patchCPAValuesForUser` — the rest of
`api4/custom_profile_attributes.go`, whose eighth route (`/group`) was already served in
`mm_api::gated_reads`. New: `mm_store::property_store`, `mm_app::custom_profile_attributes`,
`mm_api::custom_profile_attributes`, `parity::custom_profile_attributes` (12 tests).
`scripts/mutations/cpa-routes.plan`: MUTATION_TALLY_PLACEHOLDER.

**No model work was needed.** `property_field.rs`, `property_value.rs`, `property_group.rs`,
`property_access.rs`, `property_field_attrs_validation.rs` and `custom_profile_attributes.rs` were
all already in `mm-model` with generated fixtures, so `reference/dump/main.go` is untouched. This
is the first session where the breadth-first model port paid for itself outright.

### The licence gate is real, and it is not one answer

The `access_control` property group carries a `LicenseCheckHook` registered first, so it runs
before access control and attribute validation (app/server.go:322). Unlicensed it gives 403
`app.property.license_error` — but only from the arms that fire. `PreCreatePropertyField` has no
escape, so `POST /fields` is always a 403. `PostGetPropertyField` runs after the row is found, so
an unknown field is a **404** first. `PostGetPropertyFields` and `PostGetPropertyValues` return
`nil` for an empty slice, so an empty group answers a genuine `200 []` and a user with no values a
genuine `200 {}`.

So this deployment reaches five distinct answers across the seven routes and *which* one depends on
a row in `PropertyFields` or `PropertyValues`. A port that refused everything would be wrong on
five of the seven — which is why these are real database reads and not a constant. The pin is in
`mm_app::custom_profile_attributes`' module docs.

### What the empty group cannot test, and what planting one row showed

Nothing on this stack can create a CPA field without a licence, so the parity suite writes the rows
directly. Four findings only visible that way, each verified against the Go server:

- A soft-deleted `user` field and a live `channel` field are both skipped by the list — and both
  are still **found by id**: `PropertyFieldStore.Get` has no `DeleteAt` filter, `GetMany` no
  object-type filter, and both handlers' own `ObjectType != user` check sits *after* the licence
  hook, so it is unreachable while unlicensed.
- A `session_attributes` field id is a 404 through `/custom_profile_attributes/fields/{id}`, which
  is the only evidence the group scope on the by-id reads is real.
- A value planted for one user leaves every other target's read at `200 {}`.
- The batch cap is `>`, so exactly fifty ids get through to the miss and fifty-one do not.

### Two decode paths, because Go has two

`Decode` into a `*Struct` refuses a JSON array; **serde does not**, because a struct deserialises
happily from a sequence of its fields — so `PATCH …/fields/{id}` with a body of `[]` would have
produced an all-default patch and reached a write where Go answers 400. Caught by a unit test while
writing one, pinned by `an_array_is_not_an_object`. `Decode` into a `map` takes `null` as a nil map
**without** an error, so the same four bytes give `invalid_body_param` on the field routes and
`empty_body` on the value routes. Both in `mm_api::custom_profile_attributes::{decode_struct,
decode_map}`.

### Two orderings a tidier port would lose

`listCPAValues` checks target access **before** reading the group; `cpaPatchValues`, shared by both
PATCH-values routes, reads the group **first**. And `patchCPAField` trims the name before
validating it and clears `target_id` before validating it — so `{"name":"   "}` is a 400 and a
300-character `target_id` is a 404, from the same body shape.

### What is deferred

[D-300] — the licensed half of all seven routes forwards, so three hooks, the group field limit and
four websocket events have never run here. [D-301] — the two store searches implement the
predicates these routes set and refuse the rest rather than ignoring them.

### The next route in this family

`api4/properties.go`, nine routes on the same store: `getPropertyFields`, `searchPropertyFields`,
`getPropertyValues`, `getSystemPropertyValues` and the five writes. The four reads need exactly
what [D-301] lists — cursors, `since`, `ObjectTypes` and the team/channel scope switch — plus
`PropertyGroup::is_psav2`, which is already ported, because `getV2Group` refuses a v1 group before
anything else. **They are registered behind a five-way feature-flag `if` (properties.go:23)**:
`IntegratedBoards || ManagedChannelCategories || ClassificationMarkings || SessionAttributes ||
PostAttributes`. Establish which of those five are on at the pinned SHA before writing a handler —
if all five are false the routes are 404s from the mux and that, not the licence, is the contract
to port.

## Four families, a rate limit, and what the mutation runs were worth (2026-09-12)

**346 → 372 of 764.** `view.go` entire, seven custom-profile-attribute routes, the three config
reads, and the **first local-mode routes this project has ever served** — the unix socket, its
unrestricted session, and six pairs, against a denominator that had been 171-to-0.

All four sessions were terminated mid-work by a session rate limit. Their branches were preserved
as labelled WIP commits, then verified here rather than trusted: all four compiled, passed clippy
and fmt, and went green on their own stacks before merging. `wt/config` had reached **zero**
commits when it died — its ~2,800 lines existed only in a working tree — and it merged green.

### The merge that was green for the wrong reason

`wt/view` failed 36 tests across `token_writes`, `command_writes`, `bot_writes`, `file_bytes` and
`emoji_get` — every one a route that branch predated, all reporting "was forwarded to Go". None of
it was the branch. A process's command line is fixed at `exec` time and does **not** follow a
`git worktree move`, so renaming worktrees between rounds left an mm-api whose cmdline still named
`…/threads/…` holding stack 1's port, and `parity.sh`'s path-scoped `pkill` could not match it.
The suite ran against the previous round's server. `parity.sh` frees the port now — the port is the
owner key, the path is not — and the same shape could as easily have produced a false *pass*.

### The mutation tallies, and the one that is void

| plan | run | caught | controls | verdict |
|---|---|---|---|---|
| `cpa-routes` | 35 | 33 | both survived | valid, no real survivors |
| `local-mode` | 26 | 22 | both survived | valid, two real survivors |
| `view-routes` | 33 | — | **both CAUGHT** | **void** |

The view plan first scored 5 of 33 because nothing on the merge stack starts `go-boards.sh` and
`start_boards` skipped with an `eprintln!` cargo hides — twelve tests passing while asserting
nothing. That is fixed three ways (`stack.sh` starts the oracle, the skip is now a panic, and the
purge sweeps the `Views` table it had never heard of, 754 leaked rows). It still scores nothing:
both controls fail, naming `include_total_count_and_pagination_agree`. See [D-330]. A run whose
controls fail has no verdicts, so no number from that plan is quoted here.

## The view "wire-order bug" did not exist (2026-09-12)

The previous entry closed by reporting that the view round "shipped a wire-order bug that only
mutation testing's controls exposed". **That was wrong and this retracts it.** The port was correct
at every layer; the suite was measuring a stale server.

`SecondServer::start` judged success by polling `/system/ping`. With a stale mm-api already on the
port, the child it spawned failed to bind and died, the ping was answered by the *old* process, and
`start` handed back a dead child — so `parity_views` compared Go against a binary built hours
earlier. That is the second time in one session a stale server produced a confident wrong answer:
the first, through `scripts/parity.sh`, produced 36 false failures after a `git worktree move`
stranded a server whose cmdline no `pkill` pattern could match. **Identify a server by its port.**
Both call sites now free the port, and `SecondServer` additionally requires its own child to be
alive, because a ping cannot tell whose server answered.

Along the way the view store acquired the DB-backed test it never had
(`crates/mm-store/tests/db_view_store.rs`), built so that ordering by `sortorder`, `createat` and
`id` each give a different answer — without that, the assertion is vacuous. `view-routes.plan` then
had its **first valid run: 33 run, 28 caught, both controls survived.** Three earlier attempts were
void and none of their numbers mean anything. The two remaining survivors are recorded in the plan
header: no caller the suite has can tell the write gate from the read gate, and `skip_fetch_threads`
is invisible while no fixture channel contains a reply.

## The four generic property reads, and a flag that is on when four others are off (2026-09-12)

**372 → 376 of 764.** `getPropertyFields`, `searchPropertyFields`, `getPropertyValues` and
`getSystemPropertyValues` — the *generic* PSAv2 property API, of which the seven CPA routes
already served are one group's worth pinned to one object type. Store, app, handler and the mux
charset:

- `crates/mm-store/src/property_store.rs` — both searches carry the whole predicate set now;
  closes [D-301]
- `crates/mm-app/src/properties.rs` — new; the group read and the two searches, with the licence
  hook reproduced for the one group that has one
- `crates/mm-api/src/properties.rs` — new; the four handlers, the scope resolver and
  `hasTargetAccess`
- `crates/mm-api/tests/parity/properties.rs` — new; 16 tests
- `crates/mm-app/src/config.rs` — the four `FeatureFlags` the registration `if` needs
- `crates/mm-api/src/views.rs` — now reads `IntegratedBoards` from that config rather than its own
  `OnceLock`, which is what the module's own doc comment said should happen when a second consumer
  appeared

### The previous session's open question, answered: the routes are registered

`InitProperties` is a five-way `if` over `IntegratedBoards || ManagedChannelCategories ||
ClassificationMarkings || SessionAttributes || PostAttributes`. Four of the five default to
`false`. **`ClassificationMarkings` defaults to `true`** (feature_flags.go:185), so the family is
live on a stock server and sampling any of the other four would have concluded the opposite.

### Why this family is comparable where CPA was not

The licence hook is constructed with **one group id** (app/server.go:325). `RegisterBuiltinGroups`
writes five rows unconditionally beside it, and two of them — `boards` and `post_attributes` — are
PSAv2 with no hook on the read path at all. So on Team Edition these routes return real rows, and
the whole predicate set is measurable against Go: cursors in both modes, the inclusive `since`
boundary, delta mode's automatic tombstones, the three-way channel hierarchy and its two-way DM
form. A sixth row, `managed_channel_categories`, is **version 3** and is a 404 like the v1
`content_flagging` group; `IsPSAv2` is `Version == 2` exactly.

### Three answers a port would smooth over

- **An empty field list is `[]` and an empty value list is `null`.** The two sibling stores differ
  by one line — `fields := []*model.PropertyField{}` against `var values []*model.PropertyValue`
  — and nothing downstream normalises either.
- **`per_page=0` is a 500 on the GET route and a 60-row page on the POST one.** `ParamsFromRequest`
  clamps negatives and the maximum but not zero; the store's `PerPage < 1` guard is what the GET
  reaches, and `searchPropertyFields` clamps `<= 0` itself before the store sees it.
- **A malformed cursor has two different error ids.** The field route calls `cur.IsValid()` and
  answers `invalid_body_param`; the value route does not, and `opts.IsValid()` inside the core
  answers `api.property_value.get.invalid_opts.app_error` instead.

### A NULL `jsonb` column is `{}` in Go, and the port had it inverted

`PropertyField.Attrs` is a Go map, and sqlx's `reflectx.FieldByIndexes` allocates a nil map before
scanning into it — so a SQL `NULL`, whose `Scan` returns early, marshals as **`{}`**, while a
jsonb `null` reaches `json.Unmarshal`, which zeroes the map, and marshals as **`null`**. The store
had them the other way round, with a doc comment asserting the wrong one. It was unreachable until
now: the CPA reads that first used `search_fields` can only ever return an empty page. Fixed for
`PropertyFields`; the same question for the dozen other `jsonb` columns this crate reads is
[D-331].

### The next route in this family

The five **writes** of `api4/properties.go`: `createPropertyField`, `patchPropertyField`,
`deletePropertyField`, `patchPropertyValues` and `patchSystemPropertyValues`. They are more
reachable than [D-300] assumes, and for the same reason the reads were: on `boards` and
`post_attributes` there is **no hook chain at all**, so a write there is a plain
insert/update/delete plus the websocket events `App.CreatePropertyField` and friends publish. What
they need behind them is the write half of `mm_store::property_store` — `Create`, `Update`,
`Delete`, `Upsert` for both fields and values — which is genuinely unported, and
`app.DefaultPropertyFieldPermissionLevel` plus `CanonicalizeSystemObjectField`, which the create
handler calls before any permission check.

`access_control` stays forwarded when licensed, exactly as the reads do.

### Mutation tally

`scripts/mutations/properties-routes.plan`: **44 run, 41 caught, 2 controls survived, 0 harness
faults**, plus the one real survivor re-run and caught after its fixture gap was closed. Two
earlier runs were void and no number from either is quoted here.

What the void runs were worth is the point. The first said `store-fields-always-order-by-create-at`
**survived**: the fixture's `update_at` values ascended in the same sequence as its `create_at`
values, so delta mode and directory mode returned the same rows in the same order and the whole
`CASE WHEN $1` in the field query was untested. Two timestamps were swapped. The same run said
`store-values-the-target-filter-is-dropped` survived, because every planted value sat on the one
channel the tests asked about; two values were added, on a DM and at the system target. And the
valid run's survivor said the `template` guard on the value route could be swapped for `user`
without anything noticing, because `hasTargetAccess` answers the same refusal one layer down —
the suite had no `user/values` probe at all.

None of those three would have been found by a green suite, and each was a real hole in the
evidence for a predicate that decides which rows a client sees.

## Three families on the socket, and the forward leg that has to be a socket too (2026-09-12)

**+12 local-mode pairs.** `bot_local.go` (six of seven), `status_local.go` (both) and
`role_local.go` (four of five), on the unix socket, against a denominator that stood at 6 of 171
after the local router landed. Registration and wiring only — every handler is the **HTTP one**,
called with `model.Session{Local: true}`:

- `crates/mm-api/src/local.rs` — the twelve registrations, their wrappers, and
  `local_mux_segments_or_forward`
- `crates/mm-api/tests/parity/local_mode.rs` — six new tests

### Why the handlers could be shared and the middleware could not

Every gate on these three families is a `SessionHasPermissionTo*`, which short-circuits on
`Session.Local` (app/authorization.go:19) — so the local answer *is* the HTTP handler's with every
check passing, and reimplementing them would only create two things to keep in step.

`mux_segments_or_forward` is the exception, and the reason is the one thing that would have been
silently wrong: it forwards through `proxy::forward_to_go`, which dials the Go server's **port**.
A local request answered over the port reaches `APISessionRequired` rather than `APILocal`, so a
malformed id would come back **401** where Go's local mux answers **404**. The local middleware
forwards over the socket, and
`a_segment_outside_the_mux_charset_is_forwarded_over_the_socket` asserts the status, not just the
body, because that is where the difference shows.

It also carries `role_name` (`[a-z0-9_]+`), which the TCP table does not: on that side
`roles::get_role_by_name` does its own charset check and its own forward. Handling it in the
middleware makes that branch dead on this router, which is deliberate — a handler that forwards
over the port must never run here.

### `me` is nobody, and that is the whole local authentication model

`RequireUserId` rewrites `me` to the session's `UserId`, which is the **empty string** on this
transport — so `GET /users/me/status` and `POST /bots/{id}/assign/me` are both 400s naming
`user_id` over the socket, where the same paths over TCP resolve to the caller. No local-only
branch was needed for it: the handlers already do the rewrite from the session they are handed.

### Mutation tally

`scripts/mutations/local-families.plan`: **11 run, 9 caught, 2 controls survived, 0 harness
faults.** Two earlier runs were void and neither number is quoted: the first had three equivalent
mutants and a line that did not compile, the second had *both controls* broken — a rename that
missed its use, and a "reorder" written as a prepend, which axum rejects at startup with
"Overlapping method route". A control that does not build proves nothing about the harness, which
is the whole reason the rule is that a fault voids the run.

One survivor in that first run was a real fixture gap and is closed: the only `PUT …/status` the
suite made carried a body Go rejects, so it never reached `session_has_permission_to_user` and a
handler handed an ordinary session would have passed. The test now also sets a planted bot's
status through each socket, which is a 200 only because the session is `Local`.


## The first property write, and the refusal a caller can act on (2026-09-12)

**388 → 389 of 764.** `DELETE /api/v4/properties/groups/{group_name}/{object_type}/fields/{field_id}`
— one route, and the first *write* in `api4/properties.go`. It is here rather than
`createPropertyField` because it is the only one of the five whose whole path is reachable: on
`boards` and `post_attributes` a delete's sole pre-hook is the licence check, which does not manage
those groups, so nothing unported sits between the handler and the `UPDATE … SET DeleteAt`.

- `crates/mm-store/src/property_store.rs` — `count_linked_fields`, `delete_values_for_field`,
  `delete_field`; the module writes now
- `crates/mm-app/src/properties.rs` — `get_property_field`, `delete_property_field`, the
  `property_field_deleted` broadcast, and the **permission ladder** the other four writes will
  reuse
- `crates/mm-api/src/properties.rs` — the handler
- `crates/mm-api/tests/parity/properties.rs` — 4 more tests, 21 in the module

### Two deletes, one line apart, with opposite conventions

`deletePropertyField` cascades the field's values and then deletes the field. `DeleteForField`
ignores `RowsAffected` entirely, so a field with no values is not an error; `Delete` treats zero
rows as `store.NewErrNotFound` and the app layer turns it into a 404. Same function, two
statements, and reading one and assuming the other is how a group boundary stops being a boundary
— the `GroupID` predicate on the field delete is what makes deleting a real id through the wrong
group a 404 rather than a silent success.

### Three 404s and two 403s, and none of them are the obvious one

A missing group, a field not in the group, and an **object type that does not match the URL** are
all 404, with three different ids; Go's comment says the third is a 404 rather than a 400 so that
fields can be bucketed by URL without leaking cross-bucket existence. Both 403s answer
`api.property_field.delete.no_permission.app_error` — the handler's own id, not the
`api.context.permissions.app_error` every other route in this file uses — and the second of them
fires for a field whose `PermissionField` is `NULL`, which is a legacy row with no permission model
rather than a permission that was denied.

**`DeletePropertyField`'s own protected 403 is unreachable from this route.**
`SessionHasPermissionToEditPropertyField` refuses a protected field before the app layer is
reached, so the handler's 403 always wins. Measured on both servers; the app-layer check is still
ported because it is the contract for the plugin and internal callers Go has.

### The one refusal that names a state the caller can change

A field with live linked dependents is **409** `app.property_field.delete.has_linked_dependents`.
`CountLinkedFields` counts only `DeleteAt = 0`, so deleting the dependent lifts the refusal — and
a parity test walks exactly that sequence, because counting every row instead would make the
conflict permanent and no single-request test could tell.

### The suite was wiping its own fixture, and a mutation said so

The most useful thing this round produced is not in the port. `plant_delete_fixture` wrote its
rows and *then* called `go_minted_token` — inside whose `OnceCell` `purge_api_fixtures` runs, by
design, so that no fixture is built before the sweep. That sweep deletes every `mmrsdel%` row, so
whichever properties test needed a token first **deleted its own fixture mid-test**. It never
failed the suite, because the tests re-plant; it failed a mutation run, twice, by reporting
`delstore-zero-rows-is-not-a-not-found` as CAUGHT on an assertion about **Go's** status — an answer
no change to this port can produce, which is the signature of a false catch. The token is minted
before the first `INSERT` now and the mutation honestly survives.

The same ordering trap applies to every fixture in the file, so `plant` takes the token explicitly
too rather than reaching it by accident through `fixture_team_and_channel`.

### Three guards no route can reach, and why they stay

Mutation established that three store-level guards on the delete path are unreachable through
HTTP, each for a different reason:

| guard | why it cannot fire |
|---|---|
| `delete_field`'s `GroupID` predicate | the handler already read the field **with the group** |
| `delete_field`'s `RowsAffected() == 0` | same read — the row is known to exist |
| `delete_values_for_field`'s `GroupID` predicate | `PropertyFields.id` is the primary key, so a field id belongs to exactly one group |

A fourth is unreachable for a reason of its own: `SessionHasPermissionToEditPropertyField` tests
`IsUnrestricted` *after* the protected check, and reordering the two is invisible because `api4`
registers **no** properties routes on the local router — an HTTP session is never `Local`, so this
family can never see an unrestricted one. All four stay, correctly ordered, because the functions
are shared with callers Go has and this server does not; they are recorded in the code and in the
plan header rather than chased with a fixture.

### Mutation tally

`scripts/mutations/property-field-delete.plan`: **20 run, 17 caught, 3 survived, 0 harness
faults** — the three being the two controls and the one equivalent mutant above. Three earlier
runs were void and no number from them is quoted: one found the self-wiping fixture, one found
three survivors that were all the same missing bystander row, and two lines faulted on Postgres
parameter typing rather than on anything about the port. The lesson from the last of those is in
the plan header: **a new mutation is worth `cargo check`ing by hand before it costs an hour of
machine time.**


## The join-request family, and a third Go server to see it at all (2026-09-12)

**389 → 396 of 764.** All seven routes of `api4/channel_join_request.go` — the queue behind a
*discoverable* private channel: request to join, withdraw, read mine, list the channel's, count the
pending, and review one.

- `crates/mm-store/src/channel_join_request_store.rs` — the whole `ChannelJoinRequestStore`
  interface, seven methods
- `crates/mm-store/tests/db_channel_join_request_store.rs` — 5 tests, on a fixture built so the two
  sort keys disagree
- `crates/mm-app/src/channel_join_request.rs` — the whole of `app/channel_join_request.go`,
  both websocket broadcasts included
- `crates/mm-app/src/config.rs` — `feature_flag_discoverable_channels`
- `crates/mm-api/src/channel_join_requests.rs` — the seven handlers
- `crates/mm-api/tests/parity/channel_join_requests.rs` — 7 tests
- `crates/mm-model/src/channel_join_request.rs` — a `go_parity` module; the model was already
  ported with fixture round-trips and **no** behavioural oracle, so `IsValid`'s eleven refusals were
  asserted from a reading of the Go source
- `reference/dump/behaviour_channel_join_request.go`, `fixtures/behaviour_channel_join_request.json`
- `scripts/go-discoverable.sh`, started by `scripts/stack.sh up`

### The routes do not exist on the server this one fronts

`initChannelJoinRequestRoutes` returns before its first `Handle` when
`FeatureFlags.DiscoverableChannels` is off, which it is at the pinned SHA. gorilla/mux has
therefore never heard of `/channels/{id}/join_request`, and the answer is its own
`api.context.404.app_error` — whose `detailed_error` interpolates the request URL. So the flag is
the first statement of every handler and a dark request is **forwarded**: the same shape
`api4/view.go` already has, for the same reason. [D-153] is the pin.

Seeing the lit shape needed a third pinned Go process. `scripts/go-discoverable.sh` runs one on
`MMRS_GO_PORT + 31` with the flag on, sharing the database, the configuration document and the
`Sessions` table. Turning the flag on in `go-server.sh` would move `getChannel`, `createChannel`
and `patchChannel` under suites that already assert against them — the argument `go-boards.sh`
makes, one flag over.

### A conflicting save is a 201, not a 409

The partial unique index `(ChannelId, UserId) WHERE Status = 'pending'` refuses a second pending
row. Go catches that one constraint **by name**, re-reads the caller's existing request and returns
*it*, still at 201 — so POSTing twice hands back the original `id`, `create_at` and `message`, and
the second body's message is discarded. Measured. The store therefore has no pre-read: checking
first and inserting second would add a race Go does not have.

### Two paths drop the requester's free text, and `IsValid` is asymmetric about reviewers

`WithdrawChannelJoinRequest` and `UpdateChannelJoinRequest` both set `Message = ""` before writing,
so the body a client gets back from `DELETE` and `PATCH` carries an empty message even though the
row it started from had one. And `IsValid` demands `ReviewedBy` *and* `ReviewedAt` for `approved`
and `denied` and **neither** for `withdrawn`, because the requester withdraws their own request —
a port that treated the three terminal states alike refuses every withdrawal.

### The review's allowlist is not the model's allowlist

`IsValidChannelJoinRequestStatus` accepts four values; the patch accepts **two**. `pending` and
`withdrawn` are both 400 `api.channel.discoverable_join_request.invalid_patch.app_error` — which is
what the `app-review-accepts-any-valid-status` mutation exists to catch, and the reason the handler
does not reach for the model's predicate.

### `getMyChannelJoinRequest`'s miss is a bodiless 404

`w.WriteHeader(http.StatusNotFound)` and `return` — no `AppError`, no `Content-Type`, zero bytes.
Go's comment says why: a client has to be able to tell "no pending request" from "service down".
Every other 404 in the family carries a body, and the withdraw route's 404 for the same state does.

### `edit_other_users` is reported and never checked

`getMyChannelJoinRequests` gates on `c.Params.UserId != session.UserId` — a string comparison — and
refuses with `SetPermissionError(PermissionEditOtherUsers)`. A system admin holding that permission
is refused a colleague's list all the same.

### The generator's own drift, which was not this session's

`cd reference/dump && TZ=Asia/Kolkata go run .` rewrote three fixtures beyond the new one, and all
three are environment-dependent rather than code-dependent: `behaviour_filestore.json` embeds a
**random** multipart boundary, and `behaviour_scheduled_post{,_recurrence}.json` record whether
`time.LoadLocation` accepts `america/new_york` in lower case — which depends on whether Go finds
the system zoneinfo directory or its embedded copy. All three were reverted and none is committed.
A generator whose output is claimed to be deterministic has at least two rows that are not.

### What is deferred

[D-340] — `useOnlyChannelAdminsHook` narrows both join-request broadcasts to the channel's admins,
and this server strips hooks without running them ([D-183]). So a plain member would be told who
asked to join, and with what status. Latent while the flag is off; a disclosure bug the moment it
is on, and the missing half is entirely in `mm-ws`.

### The next route in this family

The rest of the discoverable surface, which [D-153] has been holding: `serveDiscoverableNonMember`
in `getChannel` (api4/channel.go:886) plus `IsDiscoverableJoinAllowed` and
`sanitizeDiscoverableChannel`, and the `discoverable` arms of `createChannel` and `patchChannel`
that `channel_creates.rs` and `channel_writes.rs` currently refuse with a locally-minted 400. The
blocker D-153 named — "a feature-flag/config surface, which this server does not have at all" — is
gone: `Config::feature_flag_discoverable_channels` reads it the way the other five are read, and
`scripts/go-discoverable.sh` is the oracle those three routes need.

### Mutation tally

`scripts/mutations/channel-join-requests.plan`: **41 run, 38 caught, 3 survived, 0 harness
faults**, both controls SURVIVED. Two of the three survivors are those controls. The third is an
equivalent mutant — clearing `DenialReason` before the review re-sets it cannot be observed,
because the row reaching that line is always `pending` and `IsValid` refuses a non-empty reason on
any status but `denied`.

Two lines survived the first run and are counted only after the fixtures were fixed and each was
re-run alone. The more interesting one is the ordering: dropping `Id DESC` from the list was
invisible because the tied pair was **planted high id first**, the query is a seq scan and a sort,
and Postgres's sort is stable at that size — so the tied rows came back in insertion order, which
was the same answer the tiebreak gives. Planting `mmm` before `zzz` separates them. That is the
third time in this project a mutation has found a fixture where the right answer and the wrong
answer coincided, and the first where the coincidence was the database's sort stability rather than
the data.

## The seven group writes, and the corpus that was owed for them (2026-09-12)

**+7 of 764** — `POST /groups`, `POST /groups/names`, `PUT /groups/{id}/patch`,
`DELETE /groups/{id}`, `POST /groups/{id}/restore`, `POST /groups/{id}/members`,
`DELETE /groups/{id}/members`. With the ten reads already served, `api4/group.go` is now
seventeen of its twenty routes; the three left are the syncable link/unlink/patch, which need
team and channel member writes.

- `crates/mm-api/src/groups.rs` — seven handlers plus two that re-claim `/groups/names`
- `crates/mm-api/src/lib.rs` — the registrations
- `crates/mm-api/tests/parity/group_writes.rs` — 7 tests; `parity/groups.rs` — one updated
- `reference/dump/behaviour_group.go` → `fixtures/behaviour_group.json` — 103 rows
- `crates/mm-model/src/group.rs`, `group_member.rs` — `go_parity` modules, no logic changed
- `docs/TECH_DEBT.md` — [D-360]
- `scripts/mutations/group-writes.plan`

### For a write, "the gate is the first statement" means it precedes the body

`requireLicense` opens all seven exactly as it opens the ten reads, so on an unlicensed server a
`POST /groups` carrying `{` is the same 501 as one carrying a valid group — **Go never reaches its
decoder**. A port that parsed first would answer 400 to a request Go does not parse, and would do
it with four different parameter names across the four routes that take a body. Five malformed
bodies × five routes are compared for exactly that.

### A static route shadows its parameterised sibling for *every* method

`names` matches `{group_id:[A-Za-z0-9]+}`, so in Go the method picks the handler at
`/api/v4/groups/names`: `POST` is `getGroupsByNames`, `GET` is `getGroup` and `DELETE` is
`deleteGroup`, each with `group_id = "names"`. Registering the literal for `POST` alone silently
un-served the `GET` this server already answered — axum prefers the static segment and does not
backtrack across method routers, so the literal's fallback swallowed every other method. The wire
did not move (Go re-derives the same 501), only `x-mmrs-served-by` did, which is why every
assertion in this suite checks it. `get_group_named_names` and `delete_group_named_names` re-claim
the two; `PUT` and the rest stay forwarded, where gorilla leaves them too.

### The validators were ported; the oracle for them was not

`model.Group`'s three validators, `Patch`, `IsSyncable` and `GroupMember.IsValid` were already in
`mm-model` with fixture round-trips and **no behavioural test at all** — the branchy half of the
file was unpinned. `reference/dump/behaviour_group.go` now drives them from Go across 103 rows,
including inputs that violate two rules at once so the *order* of each refusal chain is asserted
rather than read. It passed on the first run, which CLAUDE.md is right to say is not evidence; the
mutation batch is.

Four facts it pins that a reader gets wrong: name lengths are **bytes** and the length check runs
**before** the charset check, so 33 two-byte characters is a length error and 10 of the same
character is a charset error; the reserved-name branch's `where` is `IsValidName` without the
`Group.` prefix its neighbours carry; `GroupSourceMaxLength` is declared and never enforced, so a
207-character `plugin_…` source is valid; and the remote-id refusal is one `||` with two halves, so
a `custom` group that needs no remote id is still refused for an over-long one.

`AppError.params` is **unexported in Go** (utils.go:240), so the three `Group*MaxLength`
interpolation params cannot be read from outside the model package and are not pinned — transcribed
from the source, and never on the wire because `Message` is the untranslated id.

### What is not here

Everything behind the gate: the `GroupStore` write surface, the five custom-group permissions,
`licensedAndConfiguredForGroupBySource` and `patchGroup`'s name derivation. Go loads its licence at
startup and re-reads it only on a save, so `set_active_licence_id` moves our answer and not Go's
and the licensed side has no oracle on this stack. [D-360] records what is owed and the three
branch-level facts no test here can reach — chief among them that **`restoreGroup`'s non-custom
refusal is a 501 where every sibling's is a 400**.

### The next route in this family

`linkGroupSyncable`, `unlinkGroupSyncable` and `patchGroupSyncable` — the last three of
`api4/group.go`. They need `Group.TeamMembersToAdd`/`ChannelMembersToAdd` and the team and channel
member writes, so they sequence behind that family rather than behind this one.


## The team write family, and three Go behaviours that read backwards (2026-09-12)

**+6, and 413 of 764 served once this merged** (the branch measured 396 → 402 against a base
that predated the group writes; the merged tree is the number that counts). The six writes of
`api4/team.go`: `createTeam`, `updateTeamPrivacy`, `deleteTeam` (archive arm), `removeTeamMember`,
`searchTeams` and `invalidateAllEmailInvites`.

- `crates/mm-store/src/team_store.rs` — `Save`, `SearchAll`, its `count(*)` twin, `SearchOpen`
  and `SearchPrivate`
- `crates/mm-store/tests/db_team_search.rs` — 10 tests, on a fixture carrying a NULL
  `allowopeninvite`, a group-constrained team and a retention-policy row, none of which the REST
  API can create here
- `crates/mm-store/src/{user,channel,sidebar_category,preference,token,system,job,post}_store.rs` —
  one method each for the removal cascade and the invite purge
- `crates/mm-app/src/team.rs` — `CreateTeam`, `CreateTeamWithUser`, `createDefaultChannels`,
  `UpdateTeamPrivacy`, `SoftDeleteTeam`, `SearchAll/Public/PrivateTeams`,
  `InvalidateAllEmailInvites`
- `crates/mm-app/src/team_member.rs` — `RemoveUserFromTeam`, `LeaveTeam`, `RemoveTeamMember`,
  `postProcessTeamMemberLeave`
- `crates/mm-app/src/config.rs` — `ExperimentalEnableDefaultChannelLeaveJoinMessages` (default
  **true**) and `EnableAPITeamDeletion` (default false)
- `crates/mm-api/src/{teams,team_member_writes}.rs` — the six handlers
- `crates/mm-api/tests/parity/team_write_family.rs` — 17 tests
- `fixtures/behaviour_team_privacy.json`, `reference/dump/behaviour_team_privacy.go` — the
  invite-id predicate over all sixteen input combinations, **transcribed** from app/team.go:237
  rather than driven, and the generator says so

### Three things measured against Go that contradict the obvious reading

Each was written the other way first and the parity suite refused it.

1. **`getTeamMember` has no `DeleteAt` predicate**, so `LeaveTeam` finds a soft-deleted membership
   and runs the whole cascade again: removing an already-removed member is a **200**. The 400
   (`api.team.remove_user_from_team.missing.app_error`) needs someone who was *never* a member.
2. **`RemoveTeamMember`'s `Roles = ""` never reaches the database.** `UpdateMember` writes
   `ExplicitRoles` into the `Roles` column, so the assignment touches only the struct, and a
   departed member still reads back as `team_user`. Asserted in that direction so a "fix" fails.
3. **`createTeam` keeps a submitted `delete_at`** — `Save` writes the column straight from the
   struct — where `updateTeam` discards it, because the update path copies seven named fields onto
   the stored row instead. A client can create a team that is already archived.

### The permission gate for leaving a team is inside the `if`

`if session.UserId != params.UserId { … }` — so a self-removal is checked against nothing at all,
not `remove_user_from_team`, not membership, not that the team exists. Hoisting the check out of
that `if` refuses every ordinary member trying to leave, and no fixture using an admin token can
see it.

### `searchTeams`' pagination refusal and its response shape disagree, deliberately

The **501** on the single-permission arms fires when `page` **or** `per_page` is present; the
`{"teams": …, "total_count": N}` shape needs **both**. So `{"page": 0}` is a 501 for a caller
holding only `list_public_teams` and a 200 carrying a bare array for one holding both.

### What is still Go's

[D-370] — `?permanent=true` with `EnableAPITeamDeletion` **on**. The flag defaults to false and is
unset on the stack, so the cascade it guards (ten store methods across five stores) has no
reachable test; writing it blind is what the parity oracle exists to prevent, and the channel twin
forwards its permanent arm for the same reason. [D-371] — a **licensed** installation forwards
`deleteTeam` whole, for `cleanupTeamAccessControlPolicy`.

Also absent, both inherited from `join_user_to_team` and already recorded: [D-242] no
`Users.UpdateAt` bump on a join, [D-243] no join system post. The *leave* system post **is**
written, in both its "left the team" and "removed from the team" forms.

### Mutation tally

`scripts/mutations/team-write-family.plan`: **40 run, 38 caught, 2 survived, 0 harness faults**,
both survivors the controls.

One line survived the first run and is counted only after its fixture was fixed. Moving the
empty-term guard onto the *sanitised* term drops the `ILIKE` clause for a term of nothing but
escape characters — and the fixture could not see it, because with the clause the query is
`ILIKE '%%'` (every row) and without it there is no predicate (every row). The discriminator is a
team with **NULL `name` and NULL `displayname`**: `NULL ILIKE '%%'` is NULL rather than true, so a
built clause drops that row and a skipped one keeps it. That is the fourth time in this project a
mutation has found a fixture where the right answer and the wrong answer coincided.

A separate harness finding, recorded in the plan: `MUTATE_FILTER` filters test **names**, not
targets, so `db_team_search` — a file name — matched no test function, ran zero tests and reported
SURVIVED. The eight store lines now each name the single test that must catch them, and the batch
is run with `MUTATE_STORE_TARGETS='--test db_team_search'`. `mutate.sh`'s own header warns about
this; the warning was read and the trap still hit on the first attempt.


## The three group syncable writes, and `api4/group.go` finished (2026-09-12)

**+3, and 416 of 764 served in this tree** (measured with `scripts/routes.py` on `wt/syncables`,
whose base is `main` at ab0a424 — 413 there; a sibling branch merging moves the absolute number,
not the delta). `POST` and `DELETE /groups/{id}/{type}/{sid}/link` and
`PUT /groups/{id}/{type}/{sid}/patch`. **`api4/group.go` is now 20/20** — no handler in the file
is forwarded on its own account. The two `group_local.go` pairs are a different file and remain.

- `crates/mm-api/src/groups.rs` — three handlers; `lib.rs` — the two registrations
- `crates/mm-api/tests/parity/group_syncables.rs` — 7 tests
- `docs/TECH_DEBT.md` — [D-390]
- `scripts/mutations/group-syncables.plan`

### What a reader would otherwise get wrong

**The licence gate precedes four things here, not one.** `requireLicense` is the first statement
of all three handlers, above `RequireGroupId`, `RequireSyncableId`, `RequireSyncableType` *and*
`io.ReadAll(r.Body)`. So an unlicensed server answers one 501 to every combination of a bad id and
an unparseable body, and there are four places a helpful port could answer early — all four wrong.

**`RequireSyncableType` is dead code for every HTTP caller.** The route pattern
`{syncable_type:teams|channels}` is an alternation of two literals, so gorilla refuses a third
value before any handler runs, and `params.go:269` maps only those two strings onto
`GroupSyncableType`. A third value is therefore **forwarded** for Go's own mux 404 rather than
answered with the licence error or a 400 — two paths that look like one route, three possible
answers.

### The literal that could have un-served something, measured rather than argued

`/groups/names` cost this project a served route once: axum prefers a static segment over
`{group_id}` and does not backtrack across method routers. `link` and `patch` sit two parameters
deeper, where nothing was registered, so they *should* shadow nothing — but that is an argument
about matchit, not evidence. `every_group_route_this_server_answered_still_answers` re-asks all
**twenty-two** rust-served pairs in the family (the twenty handlers plus the two methods re-claimed
at the `/names` literal) and fails naming any pair that started being forwarded.

### What is not here

Everything behind the gate: the `GroupSyncable` CRUD surface, `verifyLinkUnlinkPermission` with
its parent-team question, `verifySchemeAdminAssignmentPermission`, and `SyncRolesAndMembership`.
[D-390] records what is owed and four branch-level facts no test on this stack can reach — chief
among them that **a re-link of a soft-deleted syncable deliberately starts from a zero value**
rather than patching the old row, so `SchemeAdmin` is not resurrected by an unlink/relink cycle.

The team and channel member writes this family was sequenced behind are already ported
(`mm_app::team_member`, `mm_app::channel_member`) and were **not** needed: the reconciliation loop
in `app/syncables.go`, not the membership primitives, is what the licensed half actually blocks on.

### Mutation tally

`scripts/mutations/group-syncables.plan`: **15 run, 12 caught, 3 survived, 0 harness faults**,
two of the survivors the controls.

The third survivor is real and **cannot** be caught from this side. Swapping which handler is wired
to `POST /link` and which to `DELETE /link` changes nothing observable: both stop at the same
`requireLicense` 501, so the two responses are byte-identical. They diverge only behind the licence
— `link` is a 201 carrying the syncable, `unlink` a 200 carrying `{"status":"OK"}` — so the line is
kept in the plan as the cheapest check that [D-390] was really closed, rather than deleted for
being inconvenient.


## `POST /api/v4/emoji`, `DELETE /api/v4/emoji/{emoji_id}`, `POST /api/v4/terms_of_service`, `POST /api/v4/users/{user_id}/terms_of_service` (2026-09-12)

| layer | file | status |
|---|---|---|
| model | `crates/mm-model/src/user_terms_of_service.rs` — `is_valid`, `pre_save` | DONE |
| store | `crates/mm-store/src/emoji_store.rs` — `save`, `delete` | DONE |
| store | `crates/mm-store/src/reaction_store.rs` — `delete_all_with_emoji_name` | DONE |
| store | `crates/mm-store/src/terms_of_service_store.rs` — `get`, `save` | DONE |
| store | `crates/mm-store/src/user_terms_of_service_store.rs` — `save`, `delete` | DONE |
| app | `crates/mm-app/src/emoji.rs` — `create_emoji`, `upload_emoji_image`, `delete_emoji` | PARTIAL (see below) |
| app | `crates/mm-app/src/imaging.rs` — `decode_config`, `filename_is_certainly_png` | PARTIAL (PNG only) |
| app | `crates/mm-app/src/filestore.rs`, `file.rs` — `move_file`, `write_file` | DONE |
| app | `crates/mm-app/src/terms_of_service.rs` — `create_terms_of_service`, `get_terms_of_service` | DONE |
| app | `crates/mm-app/src/user_terms_of_service.rs` — `save_user_terms_of_service` | DONE |
| api | `crates/mm-api/src/multipart.rs` — `multipart/form-data` as `ParseMultipartForm` reads it | DONE |
| api | `crates/mm-api/src/emoji.rs` — `create_emoji`, `delete_emoji` | PARTIAL (see below) |
| api | `crates/mm-api/src/terms_of_service.rs` — `create_terms_of_service` | DONE |
| api | `crates/mm-api/src/users.rs` — `save_user_terms_of_service` | DONE |

Three routes are served whole. `createEmoji` serves **every refusal** and the write-through image
path; three image cases forward to Go and are recorded in [D-380].

### What a reader would otherwise get wrong

1. **The image branch is picked by the *filename*, not by the bytes.** `isGIF :=
   model.NewInfo(filename).MimeType == "image/gif"` (app/emoji.go:129), so a PNG uploaded as
   `x.gif` is walked by `CountGIFFrames` and fails, and an animated GIF uploaded as `x.png` skips
   the 70-frame cap entirely. Go's `mime` package additionally reads the host's `/etc/mime.types`,
   so the mapping is a property of the machine — which is why `imaging::filename_is_certainly_png`
   tests for `.png` rather than for "not `.gif`".
2. **Two size checks, and neither is the other.** `r.ContentLength > 512 KiB` is a **413** before
   the body is read; the same 512 KiB as a `MaxBytesReader` cap makes an over-long body with no
   `Content-Length` a **400** parse error instead. The third check, on the image part's own size,
   cannot fire.
3. **`createEmoji` validates before it checks for a duplicate**, so a name that is both illegal and
   taken is the model's error, not `api.emoji.create.duplicate.app_error`.
4. **`deleteEmoji` has no `EnableCustomEmoji` check in its handler**, unlike `getEmoji` beside it —
   so the same setting answers 501 on the read and 403 on the delete.
5. **`createTermsOfService`'s licence refusal is a 400**, not the 501 the content-flagging and
   channel-bookmark families give, and `manage_system` is checked *first* — so a non-admin never
   learns the feature is licensed. The empty-text error's `where` is Go's own paste,
   **`Config.IsValid`**.
6. **`saveUserTermsOfService` ignores `{user_id}`** and acts on the session's user, the same as the
   `GET` beside it; and `accepted: false` is a `DELETE` matched on user *and* revision, so
   rejecting revision A leaves an acceptance of revision B standing.

### What is not here

`crates/mm-app/src/imaging.rs` measures **PNG** headers and hands every other format to Go, and the
resize-and-re-encode path is Go's because its output bytes are not reproducible by a second
implementation. Both forwards happen before the file backend is touched. [D-380] records the gap,
[D-381] the RFC 2231 parameter form the multipart port does not decode, [D-382] the licensed half
of `createTermsOfService`, and [D-383] the reaction cache a deleted emoji does not invalidate on
Go.

### The next route in this family

`POST /api/v4/brand/image` and `POST /api/v4/users/{user_id}/image` — the other two multipart
image uploads, which can now reuse `mm_api::multipart` and `App::write_file`. Both need
`imaging`'s resize question answered the same way this one answered it, and the profile-image one
additionally needs `SetProfileImage`'s `UpdateAt` bump and its websocket event.


## Ledger additions — api4/post.go `createPost` / `createEphemeralPost` (appended 2026-09-12, branch `wt/createpost`)

| Go file | Rust file | Status | Tests | Notes |
|---|---|---|---|---|
| app/post.go (`CreatePostAsUserWithFlags`, `CreatePost`, `deduplicateCreatePost`, `SendEphemeralPost`, `PostBurnOnReadCheckWithApp`) | `mm-app/src/post_create.rs` | PARTIAL | 6 unit + parity | One shape is served — a plain root-level message in an open or private channel — and `App::refuse_create_post_shapes` names every other shape and forwards it **before** the pending-post id is claimed and before `Post().Save`. The forwards are not documented here; they are the doc comments on that function, one arm per Go branch. |
| api4/post.go (`createPost`, `createEphemeralPost`, `createPostChecks`, `postPriorityCheck`, `postCardTypeCheck`) | `mm-api/src/post_writes.rs` | PARTIAL | 6 unit + 21 parity | `POST /api/v4/posts` and `POST /api/v4/posts/ephemeral`. Both answer **201**, not 200. The one thing a reader gets wrong: `?set_online=bogus` is swallowed and stays `true` while `?silent=bogus` is a 400 — two `strconv.ParseBool` calls a few lines apart with opposite error handling. |
| store/sqlstore/post_store.go (member mention keys) | `mm-store/src/post_store.rs` | PARTIAL | parity | `PostStore::channel_has_keyword_mention_recipients` is not a port of a Go query: it is the sound test for "would `getExplicitMentions` find a mention in a message with no `@` in it", and it is what makes the notification fan-out a *detectable* forward condition rather than an assumed-absent one. |

## Notes — api4/post.go (`createPost`)

1. **`POST /api/v4/posts/ephemeral` is a literal beside `{post_id}`, and registering it took three
   methods away until `invalid_post_id_param` put them back.** axum prefers a static segment and
   does not backtrack across method routers, so the `GET`, `PUT` and `DELETE` that
   `/api/v4/posts/{post_id}` answered on that exact path had to be re-registered by hand. Both
   halves of the proof are committed: `mm_api::post_writes`'s
   `registering_the_two_create_routes_un_serves_nothing` runs in-process against a dead stack, and
   `parity::post_creates::the_ephemeral_literal_did_not_un_serve_its_parameterised_sibling`
   re-asks the same three methods against Go and compares the bodies.
2. **A forward that happens after a partial write is a correctness bug, and the response cannot
   see it.** A forwarded 201 and a served 201 are indistinguishable to a client, so
   `parity::post_creates::every_forward_condition_forwards_and_leaves_exactly_one_row` counts the
   *rows in the channel* afterwards. Two rows would mean we wrote one and then proxied.
3. **The deduplication cache is in-process and therefore per-server.** Go's
   `seenPendingPostIdsCache` and ours are independent while both servers run, so a post Go created
   is not deduplicated here — [D-400]. The claim/remove/overwrite sequence is Go's `defer`
   unrolled, and unrolling it is what makes a *forward* safe: the forward is a failure as far as
   `App::create_post` is concerned, so the entry is removed and Go's own cache deduplicates the
   retry.
4. **`api.post.create_post.channel_root_id.app_error` is built at 500 and lowered to 400.**
   `CreatePostAsUserWithFlags` tests `err.Id` against two ids after `CreatePost` returns and
   rewrites the status for both. Returning the constructed 500 would diverge on a body that is
   otherwise identical.
5. **`GetSenderName` and `GetChannelName` are both called with the literal `model.ShowUsername`.**
   Neither the `TeammateNameDisplay` setting nor the caller's `name_format` preference is read for
   the `posted` event, so this port needs no preference lookup there — a conclusion reached by
   reading the call site rather than the function.

### Mutation testing: 33 run, 31 caught, 2 controls survived, 0 harness faults

Plan at `scripts/mutations/post-creates.plan`. The first pass scored 27 caught with four real
survivors; each was a fixture gap rather than an equivalent mutant, and each is now caught by a
test written for it. The finding they share is worth stating once: **every one of the four was an
input the suite never sent**, so the right answer and the wrong answer coincided.

1. **The keyword-recipient query is a disjunction and only one arm was exercised.**
   `a_member_with_mention_keys_forwards_the_whole_channel` sets `mention_keys` and explicitly
   turns `first_name` *off*, which pins the first arm and leaves the second free — deleting
   `OR u.notifyprops ->> 'first_name' = 'true'` changed no answer anywhere in the suite.
   `a_member_notified_on_their_first_name_forwards_the_whole_channel` is the mirror.
2. **Every dedup test retried after a success.** The claim-release arm only runs after a
   *failure*, and the refusals that forward are all decided before the claim is taken. The
   discriminator is `?silent=true` from a session that is neither a bot nor an OAuth app: a 403
   raised inside the claimed section. With the claim leaked, the retry answers 500
   `api.post.deduplicate_create_post.pending` instead of creating the post.
3. **The ephemeral route's `create_at` was blanked before comparison.** Correct for comparing the
   rest of the shape, and exactly why "assign unconditionally" and "assign only when zero" were
   indistinguishable. Note the two routes *disagree*: `createEphemeralPost` overwrites a submitted
   `create_at` (api4/post.go:235) where the create route lets an admin's backdated one survive —
   a reader would plausibly "fix" this in the wrong direction.
4. **`len(post.FileIds) > 0` cannot be pinned at route level here.** Separating its arms needs a
   caller holding `create_post` but not `upload_file`, and no stock role on an unlicensed stack
   grants one without the other (`channel_user` has both). Patching the role would have been a
   write to shared state that every concurrent suite reads, which is the shape that has produced
   failures in suites that touched nothing. Extracted as `post_carries_file_ids` and pinned by
   `only_a_non_empty_file_id_list_requires_upload_file`; the plan line moved to the `unit` suite.


## `POST`/`DELETE /api/v4/users/{user_id}/image`, `GET /api/v4/users/{user_id}/image/default`, `POST /api/v4/brand/image` (2026-09-13)

| layer | file | status |
|---|---|---|
| app | `crates/mm-app/src/config.rs` — `file_max_file_size`, `ldap_picture_attribute`, `saml_enable_sync_with_ldap`, `lock_profile_fields_for_email_users` | DONE |
| app | `crates/mm-app/src/user.rs` — `is_profile_image_locked_for_user` | PARTIAL (licensed forwards, [D-413]) |
| app | `crates/mm-app/src/brand.rs` — `save_brand_image` | PARTIAL (the 501 only, [D-411]) |
| api | `crates/mm-api/src/images.rs` — `set_profile_image`, `set_default_profile_image`, `get_default_profile_image`, `upload_brand_image` | PARTIAL (refusals only, [D-411]) |
| test | `crates/mm-api/tests/parity/image_writes.rs` — 13 tests | DONE |
| test | `crates/mm-app/tests/db_profile_image_lock.rs` — 3 tests | DONE |

*34 mutations, 32 caught, 2 controls survived, 0 harness faults.*

**No success on any of the four is served.** Go re-encodes every accepted upload with its own PNG
encoder and generates the default avatar through freetype, so the write path forwards ([D-411]) —
every refusal is answered here and byte-compared against Go, and each hand-over is proved to
happen before the file backend is touched. `scripts/mutations/image-writes.plan`.

### What a reader would otherwise get wrong

1. **The same failed `GetUser` is a 400 on the POST and a 404 on the DELETE.** `setProfileImage`
   discards the error for `SetInvalidURLParam("user_id")` (api4/user.go:668);
   `setDefaultProfileImage` propagates `app.user.missing_account.const` (api4/user.go:719). One
   call, two answers, and a shared helper would give one answer twice.
2. **`uploadBrandImage` checks `edit_brand` fourth**, after the body is read, parsed and found to
   contain an `image` part (api4/brand.go:69) — unlike every other write in the family. A caller
   with no permission and a malformed body gets the 400.
3. **The storage 501 is in a different place on each of the three writes**, and it is
   `SaveBrandImage`'s own error id on the brand route and the *upload's* id under
   `setDefaultProfileImage`'s `where` on the DELETE.
4. **Two size limits, 512 bytes apart, and the over-long body is a 413 on one route and a 400 on
   the other.** `r.ContentLength > MaxFileSize` is the handler's; `MaxFileSize + bytes.MinRead` is
   `web.Handler.ServeHTTP`'s `MaxBytesReader` (web/handlers.go:217-225). Only `setProfileImage`
   wraps its parse error, so only there can `handleContextError` find the `MaxBytesError` inside
   it and rewrite it to `api.context.request_body_too_large.app_error`.
5. **`setProfileImage`'s parse failure is a 500**, where the identical body is a 400 on
   `createEmoji` and on `uploadBrandImage`.
6. **`uploadBrandImage` answers 201**, not 200 — `w.WriteHeader(StatusCreated)` followed by
   `ReturnStatusOK(w)`, so it is a 201 that still carries `{"status":"OK"}`.
7. **The DELETE is a write.** `SetDefaultProfileImage` generates the initials avatar and stores
   it; nothing is removed. And `"name_and_username"` — the middle of the three legal values of
   `LockProfileFieldsForEmailUsers` — does **not** lock the picture.

### What is not here

Every write. [D-411] records the three forwards and what each would need;
[D-412] the three refusal families this stack has no Go oracle for (the 501s, both size
boundaries and the LDAP 409, each measured against a second mm-api with the setting changed);
[D-413] the licensed profile-field lock. [D-381] is **CLOSED** — RFC 2231 landed in `86a802c`,
which was the condition it set for this route shipping — and [D-410] is what remains of it.

One branch is observable **nowhere over HTTP**, which the mutation run is what found:
`is_profile_image_locked_for_user`'s unlicensed `Ok(false)` and its licensed forward reach the
same place, because both callers spell the check `== Some(true)` and a hand-over follows either
way. `crates/mm-app/tests/db_profile_image_lock.rs` covers it against a live store, where the
function returns its own value.

### The next route in this family

`GET /api/v4/users/{user_id}/image` already serves its stored-bytes branch; what is unported
beside these four is `PUT /api/v4/users/{user_id}/patch` and the rest of the `users.go` write
surface, none of which needs multipart.


## Ledger additions — api4/post.go, the four per-user post writes (appended 2026-09-13, branch `wt/postacks`)

| Go source | Rust | Status | Tests | The one thing a reader would otherwise get wrong |
|---|---|---|---|---|
| api4/post.go (`acknowledgePost`, `unacknowledgePost`) | `mm-api/src/licensed_features.rs` | DONE | 2 unit + 2 parity | Both are `MinimumProfessionalLicense` refusals taken **before** `RequirePostId` and both permission gates — and the two halves carry **different** error ids four lines apart: `<untranslated>` on the POST, `license_error.feature_unavailable` on the DELETE. See [D-422]. |
| api4/post.go (`setPostUnread`) | `mm-api/src/post_writes.rs` | PARTIAL | 5 parity | `model.MapBoolFromJSON` **discards its decode error**, so a malformed body is not a 400 here — it is a 200 with `collapsed_threads_supported: false`, unlike every neighbouring route in the file. |
| app/channel.go (`MarkChannelAsUnreadFromPost`, `markChannelAsUnreadFromPostCRTUnsupported`, `countMentionsFromPost`, `sendWebSocketPostUnreadEvent`) | `mm-app/src/post_unread.rs` | PARTIAL | 3 unit + parity | Three Go arms, and the reply-without-CRT one passes `(mentions, 0, 0, false)` where the other two pass `(mentions, mentionsRoot, urgent, true)` — so the same request answers a different `mention_count_root`, `urgent_mention_count` and `msg_count_root` depending on a flag in the body. Forwarded shapes in [D-421]. |
| store/sqlstore/channel_store.go (`CountPostsAfter`, `CountUrgentPostsAfter`, `UpdateLastViewedAtPost`) | `mm-store/src/channel_store.rs` | DONE | parity | Two different excluded-user arguments a few lines apart: `update_last_viewed_at_post` passes `""` so the reader's own posts raise the message counts, while `count_mentions_from_post` passes the reader's id so they do not raise the mention counts. |
| store/sqlstore/post_store.go (`SetPostReminder`, `GetPostReminderMetadata`) | `mm-store/src/post_store.rs` | DONE | offline build; no route reaches them yet | `PostReminders.TargetTime` is Unix **seconds**, the only non-millisecond timestamp in the migrated surface. The route that would call these forwards — [D-420]. |
| api4/post.go (`setPostReminder`) | — | FORWARDED | 1 parity pinning the forward | Its ephemeral confirmation always contains a permalink, so it always needs the permalink-embed path. [D-420]. |

### Notes

**The fixture oracle for `ChannelUnreadAt` was not discriminating.** `mention_count` and
`mention_count_root` both generated as `54` — a hash collision mod 100 in `reference/dump` — so
the round-trip test could not tell the two `serde(rename)`s apart, on precisely the pair the
collapsed-threads branch changes. `reference/dump/main.go` now pins
`channelunreadat.mentioncountroot` to `71`, which rewrites one key of
`fixtures/channel_unread_at.json`; no Rust test asserted the old value.
`channel_member::tests::the_channel_unread_at_fixture_gives_every_counter_a_different_number` now
fails if a future regeneration re-collides any two of the five counters.

**A parity fixture tag is a global name.** `create_plain_user(tag)` becomes the username
`mmrsplain{tag}`, unique across the whole test binary. `parity/post_acks` first used `unreadbody`,
which `parity/channel_unread` already had: both passed alone and one failed
`app.user.save.username_exists.app_error` in every concurrent run. Every tag in the new module is
prefixed `pa`.

**`a_team_and_channel_the_user_is_in` does not return a public channel.** On this stack the
caller's first channel is a `D`, so a "set_unread in an open channel forwards" test built on it
was answered by the DM branch and passed for the wrong reason. The module creates its own open
channel.

### Notes — the second pass (mutation findings)

**A wire-format bug the first pass shipped, and the test that could not see it.**
`model.MapBoolFromJSON` decodes into a `map[string]bool`, and `encoding/json` treats a wrong-typed
value as a `saveError` — it records the `UnmarshalTypeError` and **keeps walking** — so every key
whose value really is a boolean survives and the map comes back non-nil. Decoding straight into a
`HashMap<String, bool>` fails the whole object, so `{"collapsed_threads_supported":true,"x":"nope"}`
was `true` on Go and `false` here. Fixed by decoding to `HashMap<String, serde_json::Value>`.

Two things hid it. It is invisible on a **root** post, where both flags answer the same body; and
a two-server body comparison cannot see it at all, because with the flag `false` we *forward* — Go
supplies both halves and the comparison is green while the divergence is live. The oracle had to
be Go's own answer for an explicit `true` against an explicit `false`, plus an assertion about
which server answered. `parity::post_acks::a_good_flag_survives_a_bad_value_beside_it`.

**The order of the two id validations is not observable on our wire.** `RequirePostId` before
`RequireUserId` — same error id, same 400, and the parameter name lives only in `AppError.params`,
which is `json:"-"` ([D-384]); our `message` is the untranslated id rather than Go's interpolated
sentence ([D-092]). Swapping them survives the whole parity suite. `require_post_id_then_user_id`
exists so a unit test can read what a client cannot.

**`urgent_mention_count` is not verified against Go.** `POST /api/v4/posts` carrying a
`metadata.priority` is a **403** on this stack — measured, in a DM and in a public channel — so no
urgent post exists, `count_urgent_posts_after` is only ever compared at `0`, and both arms of the
`ServiceSettings.PostPriority` gate answer the same body. The SQL is schema-checked and the
`'urgent'` literal is pinned to `model.PostPriorityUrgent` by a unit test; the *count* is not.


## `POST /api/v4/users/login`, `POST /api/v4/users/login/type` (2026-09-13, branch `wt/login`)

**DONE.** The route every client calls first. `login` is served for the local-password path;
`login/type` answers its **404 with an empty body**, which is the whole route on any server
without guest magic links. 14 parity tests, 3 store-backed tests and 32 unit tests across the
four new modules. Mutation run: **49 run, 46 caught, 1 survived, 2 controls survived**;
`scripts/mutations/login.plan` carries the five fixture gaps the first pass found and the one
survivor that stays one.

| File | What |
|---|---|
| `crates/mm-api/src/login.rs` | both handlers, the error mask, the three cookies |
| `crates/mm-app/src/login.rs` | `GetUserForLogin`, `AuthenticateUserForLogin`, `CheckPasswordAndAllCriteria`, `CreateSession`, `DoLogin` |
| `crates/mm-app/src/user_agent.rs` | `channels/app/user_agent.go` **and** the OS/browser half of `github.com/avct/uasurfer` |
| `crates/mm-store/src/user_store.rs` | `GetForLogin`, `UpdateLastLogin` |
| `crates/mm-store/src/session_store.rs` | `Save`, `GetLRUSessions` |

### What a reader would otherwise get wrong

1. **The error id is chosen by the configuration, not by the failure.** A deferred mask
   (api4/user.go:2127) rewrites all but twelve ids into one of four `invalid_credentials_*`
   strings picked by five SSO flags and two sign-in toggles, always at 401 — so a 500 from a
   broken database and a wrong password are the same response. Clients branch on those four.
   `mm_api::login::mask_login_error`.
2. **A user-agent parser is on the wire.** `DoLogin` writes `platform`, `os` and `browser` into
   `Sessions.Props`, which `GET /users/{id}/sessions` echoes — so `uasurfer` had to be ported, not
   approximated. `fixtures/behaviour_user_agent.json` is the real `uasurfer.Parse` over a
   46-string corpus; the four Mattermost mapping functions beside it are **transcribed** into the
   oracle because they are unexported, and the parity test that logs into both servers with three
   different agents is what covers that seam.
3. **Everything this port cannot serve is detected before the failed-attempt counter moves.**
   Go checks MFA *after* claiming a slot, so a forward taken there would have Go claim a second
   slot for the same attempt. `App::login_needs_mfa` asks the same question one `SELECT` earlier.
   Forwarded: `magic_link_token` present, `LdapSettings.Enable`, any licence, MFA.
4. **`UpdateLastLogin` writes two different instants.** `LastLogin` is the *session's* `CreateAt`,
   minted inside the store's `PreSave`; `UpdateAt` is a fresh clock read in the same statement.
5. **The cookies take the web session length even for a mobile session**, because
   `AttachSessionCookies` reads `SessionLengthWebInHours` unconditionally while `DoLogin` may have
   used the mobile one. And only `MMAUTHTOKEN` is `HttpOnly` — the webapp reads the other two.
6. **No cookies at all without `X-Requested-With: XMLHttpRequest`**, compared for equality on the
   whole header value. `curl` gets the `Token` header alone.
7. **The guest, magic-link and remote refusals run *after* a successful password check**, so an
   account that is refused for one of them still has its lockout counter cleared.

### What is not here

[D-430] the rate limit Go puts on this route (5/s, burst 10) and nothing in this port implements.
[D-431] `SessionLengthSSOInHours` and the SSO arm of `DoLogin`, unreachable from this route.
`/login/sso/code-exchange`, `/login/desktop_token`, `/login/switch` and `/login/cws` are
deliberately unregistered, which is what keeps them forwarded.

One divergence is in the code and not in the register: Go writes the `Token` header **inside**
`DoLogin`, before the terms-of-service read, so a 500 from that read still carries a live
credential. This port returns the error without the header. `mm_api::login::login` says so.

Three things this stack cannot show, and none of them has a live oracle:

1. **MFA end to end.** `EnableMultifactorAuthentication` is off for every suite in the binary and
   `Users.MfaActive` cannot be set through the API, so the forward is proved by
   `crates/mm-app/tests/db_login_mfa_probe.rs` — which also asserts the probe writes nothing, the
   property the ordering depends on — and never compared against Go.
2. **Three of the four masked ids.** `invalid_credentials_sso`, `…_username` and `…_email` need
   configurations the shared server cannot have; they are unit-tested against a `Config`, with
   the arms transcribed from api4/user.go:2163-2185 rather than measured.
3. **The mobile-versus-web session length in `DoLogin`.** Both are 4320 hours on this stack, so a
   swapped read is invisible; no mutation for it is in the plan for that reason. What *is*
   covered is the `isMobile` prop the same branch reads, which is on the wire.

### The next route in this family

`POST /api/v4/users/login/desktop_token` needs `ConsumeTokenOnce` and the OAuth/SAML user check;
`POST /api/v4/users/login/switch` needs the whole `switchAccountType` matrix. Neither needs
anything this session did not build except [D-431].

## `POST /api/v4/users`, `/users/email/verify/send`, `/users/password/reset/send`, `/users/{user_id}/email/verify/member` (2026-09-13, branch `wt/createuser`)

**PARTIAL, and deliberately so on three of the four.** `verifyUserEmailWithoutToken` serves whole.
`createUser` serves the system-admin and anonymous-signup branches and forwards the two the query
string selects. Both `/send` routes serve exactly the prefix that precedes `Token().Save` and
forward from there — because Go mints the one-shot row *before* it tries to send, and a forward
taken after that would leave a live credential behind for a request Go then handled from scratch.
14 parity tests, 11 unit tests across the three new modules. Mutation run: first pass **24 run,
21 caught, 1 survived, 2 controls survived, 0 harness faults**; after the repair below and a
three-line re-run, **24 run, 22 caught, 2 controls survived, 0 faults**.
`scripts/mutations/user-creates.plan`.

The survivor was a fixture, not a shrug. `app-locale-default-reads-the-wrong-setting` substituted
`TeamSettings.RestrictCreationToDomains`, which is `""` on this stack — and `User::pre_save`
repairs an empty locale to `DefaultLocale`, which is *also* `"en"`. The right answer and the wrong
answer coincided, so a suite that was in fact watching the field could not see the change.
Replaced with a different **valid** locale (`"de"`), which is caught. What no mutation on this
stack can show is that the reset reads `DefaultClientLocale` rather than a literal `"en"`: the
setting is `"en"` here, so Go and this port agree either way.

| File | What |
|---|---|
| `crates/mm-api/src/user_creates.rs` | all four handlers, the forwarding table, `decode_user` |
| `crates/mm-app/src/user_create.rs` | `CreateUserFromSignup`, `CreateUserAsAdmin`, `CreateUser`/`createUserOrGuest`, `IsUserSignUpAllowed`, `IsFirstUserAccount`, `CheckEmailDomain` |
| `crates/mm-app/src/i18n.rs` | `i18n.supportedLocales` and nothing else in that package |
| `crates/mm-store/src/user_store.rs` | `IsEmpty` |
| `crates/mm-app/src/config.rs` | `EnableUserCreation`, `EnableSignUpWithEmail`, `DefaultClientLocale` |

### What a reader would otherwise get wrong

1. **The query parameters are `t` and `iid`.** Not `token` and `invite_id` — those are labels the
   audit record puts on a log line (api4/user.go:253-254). A port looking for `?token=` would take
   the anonymous branch for every invitation, creating unverified accounts on a closed server.
   `t` wins over `iid`, and the admin probe is consulted only when neither is present, so a system
   admin following an invitation link takes the invitation branch.
2. **`serde` accepts a JSON array where Go's decoder does not.** `model.User` carries
   `#[serde(default)]`, and the derived `deserialize_struct` takes a *sequence* as well as a map —
   so `[]` produced a zero user here and `api.context.invalid_body_param.app_error` on Go. The
   parity suite found it as a password-length error compared against a decode error.
   `mm_api::user_creates::decode_user` now admits only an object or `null`, and ignores trailing
   bytes the way `Decoder.Decode` does.
3. **The locale reset is a membership test, not a validation.** `users.CreateUser` replaces a
   locale that is not one of the **23 locales the server ships translations for**
   (`i18n.supportedLocales`, i18n.go:73) with `DefaultClientLocale`. `zz` passes
   `model.IsValidLocale` and is still replaced; the list is case- and region-sensitive, so
   `en-AU` is supported and `en-au` is not. `mm_app::i18n`.
4. **`IsFirstUserAccount` counts with `IncludeDeleted: true`** and `UserStore::is_empty` excludes
   bots. Two adjacent "is this server fresh" questions with two different rules, and both decide
   whether the account being created gets `"system_admin system_user"`. Dropping the first flag
   would hand the admin role to the next signup on a server whose only account was deactivated.
5. **The refusal order inside `createUserOrGuest` is observable.** User limit → group-name
   collision → accepted domain → password → store. Every one is a 400, so a body violating several
   reports only the first, and reordering them changes the id a client sees.
6. **`CheckEmailDomain`'s empty list allows everything.** That is the stock configuration, so the
   function is a no-op on a default server — and an inverted empty case refuses every signup.
   The match is a suffix test against `"@" + domain`, so a list of `example.com` does not admit
   `bob@evil-example.com`.
7. **`verifyUserEmailWithoutToken` looks the user up *before* checking the permission.** An
   unprivileged caller therefore gets a **404** for an id that names nobody and a 403 for one that
   resolves — the route tells them whether an account exists. Reversing it is not a fix, it is a
   behaviour change. Its reply is also the copy fetched *before* the write, so it never reports
   the change it just made.
8. **`ExperimentalEnableHardenedMode` rewrites `sendPasswordReset`'s three 400s into a 200**, but
   not its missing-`email` 400, which the handler raises before `SendPasswordReset` is called.

### What is not here

[D-450] no welcome e-mail on any served branch. [D-451] the `t` and `iid` branches, which need
`JoinUserToTeam` and `AddDirectChannels`. [D-452] the token-minting half of both `/send` routes.
[D-453] `UpdateViewedProductNoticesForNewUser` and the `UserHasBeenCreated` plugin hook. [D-454]
the config fixture script's residual key drift, which this session grew by three keys (67 → 70)
rather than closed.

A licensed installation forwards `POST /users` whole: `CreateGuest`, the guest-invitation licence
gates and the licensed user-limit id all live behind a licence and none is ported.

Three things the stack cannot show. `EnableUserCreation` and `EnableSignUpWithEmail` are both
`true` on it, so `IsUserSignUpAllowed`'s 501 is unit-reasoning rather than a measurement, and no
mutation for it is in the plan. `MM_TEAMSETTINGS_ENABLEOPENSERVER=true` is an environment override
on both servers, so the `no_open_server` 403 is likewise never produced — the open-server mutation
in the plan asserts the *gate*, by flipping `&&` to `||`, rather than the refusal. And
`isAtUserLimit` cannot fire below 250 active users.

### The next route in this family

`POST /api/v4/users/{user_id}/email/verify/member` closed the last unforwarded route on
`api4/user.go`'s verification path. The cheapest next one is `POST /api/v4/teams/{team_id}/invite/email`
— it needs [D-452]'s token minting, which is the same work the two `/send` routes are waiting on,
and it would then unblock `CreateUserWithToken` behind it.

## `PUT /api/v4/users/{user_id}`, `/patch`, `/active`, `/roles` (2026-09-13, branch `wt/userupdate`)

**Three of the four serve whole; `/active` serves activation and forwards deactivation.** 15
parity tests, 12 unit tests across the two new modules, plus a router guard in `lib.rs`.
Mutation run, first pass: **28 run, 25 caught, 1 survived, 2 controls survived, 0 harness
faults**; after the repair below, a five-line re-run: **5 run, 3 caught, 2 controls survived, 0
faults**. Every one of the 28 real mutations in `scripts/mutations/user-update.plan` is now
caught.

The survivor was a finding about the *code*, not the tests. `api-patch-keeps-the-remote-id`
removed `patch.RemoteId = nil` from `patchUser` and nothing noticed, because
`SqlUserStore.Update` copies `RemoteId` off the stored row **unconditionally** — trusted path
included — so the handler's nil-ing is defence in depth and no REST request can tell the two
apart. Deleted rather than papered over, and replaced by one that removes the store's copy-back:
on `PUT /users/{id}` that is the *only* protection, since `updateUser` has neither a
`SanitizeInput` call nor a nil-ing of its own. `the_body_cannot_grant_itself_roles_or_undelete_itself`
grew two `remoteid` assertions in the same change; without them the replacement would have
survived too.

| File | What |
|---|---|
| `crates/mm-api/src/user_updates.rs` | all four handlers, `MapFromJSON`/`StringInterfaceFromJSON`, the forwarding table |
| `crates/mm-app/src/user_update.rs` | `CheckProviderAttributes`, `CheckLockedProfileFields`, `PatchUser`, `UpdateUserAsUser`, `UpdateUserRoles(WithUser)`, `CheckRolesExist`, `UpdateActive`'s activation half, `SetAutoResponderStatus`, `isAtUserLimit` |
| `crates/mm-store/src/session_store.rs` | `SqlSessionStore.UpdateRoles` |
| `crates/mm-store/src/user_store.rs` | `Count` under `UpdateUserRolesWithUser`'s options, as `count_system_admins` |
| `reference/dump/behaviour_user_update.go` | the oracle: both map decoders, `UserPatch` decoding, `User.Patch`/`ToPatch`/`SanitizeInput`, the locked-field scan, `tryingToChange`, `NewSystemRoleIDs`, the auto-responder transitions |

The one thing a reader would otherwise get wrong: **`updateUser` does not call `SanitizeInput`** —
`createUser` is the only api4 handler that does, and the protection on this route is
`SqlUserStore.Update`'s copy-back of thirteen columns plus, at `trustedUpdateData = false`,
`Roles` and `DeleteAt`. See the module doc on `mm_app::user_update`; `parity::user_updates::the_body_cannot_grant_itself_roles_or_undelete_itself`
sends a body claiming all of them and checks the stored row, not only the response.

### Five things that are not symmetric between the pair, and are on the wire

1. **`updateUser` replaces, `patchUser` merges.** An absent `position` in a `PUT /users/{id}` body
   is written as `""`; in a patch it is left alone. An explicit `null` in a patch is
   indistinguishable from an absent key; `""` is not.
2. **A wrong password on an e-mail change is a 400 on `/users/{id}` and a 401 on `/patch`.**
   `updateUser` flattens every `DoubleCheckPassword` failure — including the account-lockout
   401 — to `SetInvalidParam("password")`; `patchUser` propagates it.
3. **An unknown but well-formed user id is a 404 on `/users/{id}` and a 400 on `/patch`**, and the
   400 names a *body* parameter for a value that came from the URL.
4. **`patch.RemoteId = nil` is unconditional**, on the second line of the handler, with no admin
   bypass. `User.ToPatch` also omits `RemoteId`, so no conflict scan driven from a `model.User`
   can ever see one.
5. **`updateUserRoles` answers `{"status":"OK"}`, not the user**, and its licence check runs
   *before* the permission check — so an unprivileged caller naming `system_manager` is told about
   the licence rather than about the permission.

### What the parity stack cannot show

`CheckLockedProfileFields` and `CheckProviderAttributes`'s LDAP/SAML arms need an Enterprise
licence, which no second process can see. Both are structured so only a request Go would
*actually refuse* reaches the licence question: the field scan runs first and a patch that
conflicts with nothing is served regardless. The scan itself is asserted against a generated
corpus (25 rows, four of them adjacent pairs that exist only to make the *order* of the five
returns observable). [D-462] records the two branches with no test at all — the last-administrator
guard and the 250-seat activation limit.

### What is not here

[D-460] `User`/`UserPatch` decode case-sensitively where Go folds — fail-safe, and the divergence
is asserted rather than hidden. [D-461] deactivation — **closed by the session below**. [D-462]
the two untested branches.

### The next route in this family

`DELETE /api/v4/users/{user_id}` is the cheapest: it is `UpdateActive(false)` plus the permanent
variant, so it needs exactly [D-461]'s list and nothing else. `PUT /users/{user_id}/mfa` and
`/auth` are the other two writes left on the `{user_id}` subtree; `/auth` is system-admin-only and
needs `UpdateAuthData`, which the store already has.


## `GET /api/v4/channels`, `POST /api/v4/channels/search`, `POST /api/v4/channels/group/search` (2026-09-13, branch `wt/channelsearch`)

| layer | file | status |
|---|---|---|
| store | `crates/mm-store/src/channel_store.rs` — `get_all_channels`, `get_all_channels_count`, `search_all_channels`, `search_group_channels`, `autocomplete`, `autocomplete_in_team_filtered` | DONE |
| app | `crates/mm-app/src/channel.rs` — `get_all_channels`, `get_all_channels_count`, `search_all_channels`, `search_group_channels`, `autocomplete_channels`, `autocomplete_channels_for_team_filtered` | DONE |
| api | `crates/mm-api/src/channels.rs` — `get_all_channels`, `search_all_channels`, `search_group_channels`, `sanitize_all_channels_response` | DONE |
| test | `crates/mm-api/tests/parity/channel_search_all.rs` — 18 tests | DONE |
| test | `crates/mm-api/src/lib.rs` — `the_channel_routes_are_all_still_answered_here` (52 route+method pairs) | DONE |

The fourth route of the family, `GET /api/v4/teams/{team_id}/channels/managed_categories`, is
**not registered by Go on this stack** and stays forwarded — see [D-440].

*Mutations: 45 run, 43 caught, 2 controls survived, 0 harness faults.*
`scripts/mutations/channel-search-all.plan` (the unsuffixed `channel-search.plan` is
`searchChannelsForTeam`'s). Two passes were needed and both taught something:

- **Five lines of the first pass were harness faults, all one cause.** A mutation that deletes a
  predicate deletes a bound parameter's last reference, and `sqlx::query!` then cannot type it —
  `could not determine data type of parameter $2`. That is a compile failure, so no test runs and
  the verdict is void. Neutralise instead of deleting: swap `LIMIT $1 OFFSET $2`, wrap a guard as
  `($9 OR NOT $9)`, append `OR TRUE`. The plan's header carries the rule; each rewritten line was
  compiled on its own before the second pass.
- **Two survivors, and neither was a shrug.** `app-search-all-does-not-trim` survived a test that
  *did* send a padded term: `build_fulltext_term` splits on whitespace, so padding never reaches
  the tsquery, and `mmrscastx:*` matches the hyphen-split lexeme in every fixture channel's
  `Name` either way. The term had to become a mid-word substring (`castx`), which the fulltext arm
  cannot match at all — verified in Postgres before the fixture changed.
  `gac-exclude-acp-inverted` survived because the only request sending that flag sent it to the
  search route, not to the list.
- **One equivalent mutant, retired with its reason.** `c.type IN ('P','O')` → `('P','O','G')` in
  the *list* query cannot be caught: that query inner-joins `Teams`, and every `G` and `D` row in
  the database carries `teamid = ''` (577 and 2 rows, none joining a team), so the join already
  excludes them. The count query has no join, the same mistake there moves `total_count` by 577,
  and that line is caught.

### What a reader would otherwise get wrong

1. **`include_total_count` changes the response's top-level type**, from a bare array to
   `{"channels":…,"total_count":…}` — and on `searchAllChannels` the same switch is driven by
   `page` *and* `per_page` both being present **in the body**, not by a query flag.
2. **`GetAllChannelsCount` is not the size of `GetAllChannels`, for two independent reasons.**
   Its store options drop `AccessControlPolicyEnforced` and
   `ExcludeAccessControlPolicyEnforced` (app/channel.go:2471-2479), and its query omits the
   `Teams` join. So `?include_total_count=true&exclude_access_control_policy_enforced=true`
   returns a filtered list beside an unfiltered count. Pinned by
   `the_retention_and_access_control_filters_need_planted_rows`, which plants the
   `AccessControlPolicies` row the stack otherwise has none of.
3. **The same 403 is two different bodies.** `getAllChannels` passes all three sysconsole
   permissions to `SetPermissionError`; `searchAllChannels` passes only
   `sysconsole_read_user_management_channels`.
4. **`?system_console` defaults to true and is true when empty**, and only a non-empty
   unparseable value is a 400. False selects the *autocomplete* queries, which split again on
   `team_ids`: exactly one valid id is team-scoped and gated on `view_team`; none, two, or one
   malformed id is cross-team and gated on **nothing**.
5. **`ORDER BY c.DisplayName, t.DisplayName` has no third key**, so ties are the planner's on
   both servers. Adding `c.Id` would make this port more deterministic than Go — the parity
   comparator therefore asserts the *sequence of sort keys* and the *contents of each tie group*
   rather than bytes, and refuses to invent an order Go does not have.
6. **`searchGroupChannels` matches aggregated member usernames, not channel names**, every word
   must match, and squirrel's empty `And{}` renders as `(1=1)` — so a term of a single space
   returns the caller's group messages unfiltered while the empty term short-circuits to `[]` in
   the app and never reaches the store.
7. **Its LIKE escapes with `\`, not `*`**, and carries no `ESCAPE` clause; every other channel
   search in the file uses `*`.

### What is not here

**`search_all_channels`' unpaginated total is `len(channels)`, not `0`** — Go's `else` branch at
channel_store.go:3802, which is easy to read past. No test can see it (the handler writes the
count only when paginated, which is the same condition that runs the real `count(*)`), so it is
pinned by the doc comment rather than by an assertion.

`channelSearchQuery`'s `PolicyID` branch — an inner join narrowing to one retention policy — has
no caller in api4 and is not ported; `ChannelSearch` has no json tag for it. [D-441] records the
`%q`-versus-JSON quoting of `parent_access_control_policy_id`, which is the same bytes for every
id shape that field can hold.

## api4/channel.go — the channel administration routes (2026-09-13)

| File | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| api4/channel.go (`updateChannelScheme`, `patchChannelModerations`, `updateChannelMemberAutotranslation`) | `mm-api/src/channel_admin.rs` | PARTIAL | 10 pass + 7 parity | Three gates on an unlicensed server. **Only `updateChannelScheme` has anything in front of its gate**: `RequireChannelId` then the body, so `{"scheme_id":"nope"}` is a 400 and a well-formed one is the 403 — the other two answer their gate for every id, including one that does not exist. Licensed installations forward. `IsValidId` is 26 bytes of letters-or-numbers, **not** base32; see the doc comment. Mutations for both rows: 31 run over two passes, 23 caught, 2 controls survived, 0 harness faults. |
| api4/channel.go (`channelMembersMinusGroupMembers`), app/group.go, sqlstore/group_store.go | `mm-api/src/channel_admin.rs`, `mm-app/src/group.rs`, `mm-store/src/group_store.rs` | PARTIAL | 5 pass + 13 parity | The **one route in this file that reads the group tables with no licence gate** — 200 with real rows unlicensed, measured before porting. `group_ids` is validated twice against two different strings (length on the regex-stripped copy, `IsValidId` on the raw split). The outer join onto `GroupMembers` has **no `DeleteAt` filter** while the exclusion subquery does, so a deleted membership still appears in `groups` and does not exclude the member. `count(DISTINCT Users.Id)` against the page's `GROUP BY`. No trailing newline. Four first-pass survivors, all fixture gaps, all fixed and re-run — see note 4. |

### Notes

1. **`updateChannelMemberAutotranslation`'s gate is not a licence field.** It is
   `AutoTranslation() == nil || !IsFeatureAvailable()` — an enterprise interface registered by
   `RegisterAutoTranslationInterface`, nil in the compared build. `IsFeatureAvailable` is not
   visible from this tree, so the licensed branch forwards rather than reproducing the 403.
   The route had been left unregistered on the note that it "needs the AutoTranslation store"; it
   does not, because the gate precedes every store call.
2. **Go interpolates the group ids into the SQL** (`IN ('%s')`, group_store.go:1752) where this
   port binds `= ANY($2)`. Identical for every input the handler admits — which is every input,
   since `IsValidId` runs first — and documented where it is not.
3. `sanitize_profile(&mut user.user, false)` on a sysconsole route: `as_admin` false, so
   `notify_props` is emptied and `auth_data` blanked. A mutation to `true` is caught by the byte
   comparison, because `notify_props` is `omitempty` and an emptied map disappears from the body.
4. **The four survivors were four fixture gaps, and one of them was a test that could not fail.**
   The bot the `Bots.UserId IS NULL` predicate exists for was created with `POST /bots`, which is
   refused on this deployment, and the assertion about it sat inside an `if let Some(bot)` — so
   there was no bot and nothing said so. It is planted now, the way `scripts/stack.sh` plants its
   own. The other three: paging only ever asked for `per_page=1`, where `page * per_page` and
   `page * 1` agree; the `group_ids` rows had no input where stripping changes the split, which
   takes a `!` **inside** an otherwise valid id; and the `total_count` mutation's plan line used a
   `|` in a filter that the `unit` suite passes to libtest as one substring, so nothing ran.

### What is not here

`moveChannel` and `convertGroupMessageToChannel` — [D-480] and [D-481]. Both are unblocked,
neither is small: five new store methods between them, plus an i18n string each for the system
message they post.


## `DELETE /api/v4/users/{user_id}` and the deactivation half of `/active` (2026-09-13, branch `wt/userdelete`)

**The soft delete serves; `?permanent=true`'s refusal serves; the deactivation forward [D-461]
opened is closed.** 10 parity tests in `parity/user_deletes.rs`, one unit test, one rewritten
parity test in `parity/user_updates.rs`. Mutation run: **26 run, 24 caught, 2 controls survived, 0 harness faults** — in two passes,
because the first pass aborted at line 14 on a harness fault of my own making (see below) and was
re-run from there after the repair.

`439 → 440 of 764 route+method pairs`, measured with `scripts/routes.py` in this worktree against
base `7bc3555`. `api4/user.go` goes 60/78 → 61/78.

| File | What |
|---|---|
| `crates/mm-api/src/user_deletes.rs` | `deleteUser`, both registrations (`{user_id}` and the literal `me`), `strconv.ParseBool` |
| `crates/mm-app/src/user_delete.rs` | `UpdateActive`'s deactivation half, `userDeactivated`, `App::owns_bots` |
| `crates/mm-store/src/oauth_store.rs` | `RemoveAuthDataByUserId`, `PermanentDeleteAuthDataByUser` |
| `crates/mm-app/src/config.rs` | `ServiceSettings.EnableAPIUserDeletion`, `TeamSettings.EnableUserDeactivation` |

The one thing a reader would otherwise get wrong: **`PermanentDeleteAuthDataByUser` deletes
`OAuthAccessData`, not `OAuthAuthData`.** The name says one table and the statement says the
other (oauth_store.go:327), and `userDeactivated` calls it beside `RemoveAuthDataByUserId`, which
*does* touch `OAuthAuthData`. Reading the names instead of the statements swaps the two and leaves
every live OAuth token of a deactivated account working. Both directions are mutated, and
`a_soft_delete_clears_both_oauth_tables_and_only_for_that_user` plants a row in each table plus a
bystander's pair, so the swap, the dropped statement and an unpredicated `DELETE` are three
distinct failures.

### How a deactivation became servable when [D-461] said no prefix of it could be

[D-461] was right that everything `UpdateActive(false)` does runs after the `UPDATE`, so there is
no *prefix* to serve. What it missed is that two of those steps —
`notifySysadminsBotOwnerDeactivated` and `disableUserBots` — return immediately when the account
owns no bots (app/bot.go:568 and the empty first `GetBots` page), and that is a fact a `SELECT`
can establish **before** the write. `App::owns_bots` asks it; `true` forwards the whole request,
`false` serves it. Everything else in the tail is ported: `RevokeAllSessions`, `SetStatusOffline`,
`sendUpdatedUserEvent`, and now the two OAuth deletes.

What still forwards, and on exactly what condition:

| request | condition | why |
|---|---|---|
| `DELETE /users/{id}` | the target owns a non-deleted bot | the sysadmin DM needs an i18n template ([D-472]) |
| `DELETE /users/{id}?permanent=true` | `EnableAPIUserDeletion` is on | 17 unported store methods ([D-470]); the flag is **off** here, so the served 401 is what a client gets |
| `PUT /users/{id}/active` `{"active":false}` | the target owns a bot | as above |
| `PUT /users/{id}/active` `{"active":false}` | it is a self-deactivation **and** `EnableUserDeactivation` is on | `SendDeactivateAccountEmail` ([D-238]); the flag is off here, so the served 401 is what a client gets |
| `PUT /users/{id}/active` `{"active":true}` | a licence is installed | unchanged from last session |

Every one of those is decided from the configuration and `SELECT`s alone. Four of the five are
provably before the write, because the served path answers with `x-mmrs-served-by: rust` and the
forwarded one does not while the row is untouched by us. The bot-owner case is **not** provable
that way and is not claimed to be — see [D-475].

### The harness fault, because it cost a run and the next session will hit it too

`mutate-batch.sh` parses each plan line with `IFS=$'\t' read`. Tab is an IFS **whitespace**
character, so a run of two tabs collapses into one — an **empty `to` field shifts `suite` into the
replacement text and `filter` into `suite`**. The suite is then unrecognised, `mutate.sh` exits 2,
`set -e` aborts the batch, and — because the abort happens after the substitution — the tree is
left with the literal string `api` where a block of Rust used to be. Four lines in this plan meant
"delete this block"; all four were empty-`to`. `scripts/preflight-plans.sh` does not catch it: it
checks anchors, not field counts. The four now replace the block with an inert `let _ = …;`
instead, which is a deletion for every purpose that matters.

Every line was compile-checked before launching (`broken: 0`, both passes). That check is what
makes the *other* documented trap harmless: dropping a predicate from a `sqlx::query!` leaves a
bound parameter unreferenced and the macro refuses to compile, so the two "loses its predicate"
mutations are written as `WHERE length($1) >= 0`.

### Two findings that were flaky assertions first

**`DeleteAt` and `UpdateAt` are not equal.** `UpdateActive` writes `UpdateAt = GetMillis()` and
then `DeleteAt = UpdateAt`, which reads as one clock feeding both — and `SqlUserStore.Update`
calls `PreUpdate`, whose `u.UpdateAt = GetMillis()` (model/user.go:563) overwrites it. So the
assignment's only lasting effect is the value `DeleteAt` copied off it, and the row satisfies
`DeleteAt <= UpdateAt`. A full parity run produced a row 1 ms apart and failed an assertion that
said "one clock read". The assertion now pins the *relation*.

**`permanent` is `strconv.ParseBool` with the error discarded** (web/params.go:232). Six spellings
are true; `?permanent=yes` is silently false and soft-deletes the account while answering the same
`{"status":"OK"}` a permanent delete would. `?permanent=tRue` is false too — Go's parser is not
case-insensitive beyond the exact set.

### What the parity stack cannot show

`EnableAPIUserDeletion` and `EnableUserDeactivation` are both **off** in the live document, which
is Go's own default for each. That makes the two refusals the reachable arms and the two writes
they guard unreachable — recorded, not worked around ([D-470], [D-238]). The permanent refusal's
*non-admin* wording is unreachable for a different reason: a caller who is not a system admin can
only reach the `permanent` fork for themselves, and the self-delete guard refuses that first. The
LDAP asymmetry between the two routes and the `manage_system` self-delete escape are [D-473].

### The next route in this family

`PUT /api/v4/users/{user_id}/auth` — system-admin-only, and `UserStore::update_auth_data` is
already the second-most-wanted unserved store method by `scripts/deps.py`. `PUT
/users/{user_id}/mfa` is the other write left on the `{user_id}` subtree and needs the MFA
secret path [D-462]'s sibling probe already exercises.


## api4/team.go — the team administration routes (2026-09-13)

| File | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| api4/team.go (`teamMembersMinusGroupMembers`), app/group.go, sqlstore/group_store.go | `mm-api/src/team_admin.rs`, `mm-app/src/group.rs`, `mm-store/src/group_store.rs` | PARTIAL | 10 parity | The channel route's twin, sharing its `group_ids` parser (Go shares the regex, and it is declared in *this* file). Three differences on the wire: the permission is `sysconsole_read_user_management_groups`, there is no space-channel guard, and the store adds **`TeamMembers.DeleteAt = 0`** — a predicate with no channel counterpart, and one no api4 route can write, so the parity fixture plants it by SQL. |
| api4/team.go (`updateTeamScheme`) | `mm-api/src/team_admin.rs` | PARTIAL | 3 pass + 4 parity | **`{"scheme_id":""}` is a 501 here and a 400 on the channel twin**: Go's team condition carries `&& *p.SchemeID != ""`, which is how a client detaches a team from its scheme. The licence refusal is **501** `api.team.update_team_scheme.license.error` against the channel handler's 403. Gate order is id, body, licence; licensed forwards. |
| api4/team.go (`inviteUsersToTeam`) | `mm-api/src/team_admin.rs`, `mm-model/src/member_invite.rs` | PARTIAL | 3 pass + 4 parity | Six refusals served, **every request that would send mail forwarded** — [D-490]. Both permission arms report `invite_user`, including the one testing `add_user_to_team`. `MemberInvite` gained null-tolerance and `profiles: Vec<Option<_>>`; see note 3. |
| api4/team.go (`inviteGuestsToChannels`) | `mm-api/src/team_admin.rs` | PARTIAL | 2 parity | The licence check is the handler's **first statement**, ahead of `RequireTeamId`, so a bogus team id, a malformed body and a caller with no permission all get the same 501 — measured. Licensed forwards. |
| api4/team.go (`importTeam`) | `mm-api/src/team_admin.rs` | PARTIAL | 2 parity | Seven refusals served, `importFrom=slack` forwarded — [D-491]. The not-multipart failure is a **500**, where `createEmoji` answers 400 for the identical failure. |

### Notes

1. **Measured, not read.** `updateTeamScheme` unlicensed is 501 and `{"scheme_id":""}` reaches it;
   `inviteGuestsToChannels` answers 501 for `/teams/zzz/invite-guests/email` with `{}`;
   `importTeam` with a JSON body is a 500 and with `importFrom=bogus` a 400. All four against the
   pinned Go server on stack 4 before anything was registered.
2. **The forward precedes every write, and the test says so by exhaustion.**
   `parity::team_admin::every_answer_this_server_gives_on_the_two_write_routes_is_a_refusal` shows
   that no input reaches a 2xx from Rust on `invite/email` or `import` — a handler with no success
   of its own cannot have written anything — and that the two requests which *would* write come
   back `x-mmrs-served-by: go`.
3. **`null` is never a decode error in Go, and `MemberInvite` was refusing six bodies Go accepts.**
   `{"emails":null}`, `{"emails":[null]}`, `{"message":null}`, `{"profiles":[null]}`,
   `{"profiles":[{"email":null}]}` and a body that is exactly `null` all decode; `[null]` in a
   `[]string` is `[""]`, and a `null` in `[]*MemberInviteProfile` is a nil pointer that still
   counts toward `len(Profiles)` — which is the difference between a `profiles_graceful` 400 and
   an `invalid_body` 400 on the wire. `profiles` is now `Vec<Option<_>>`, which also makes
   `IsValid`'s `profile_nil` branch reachable for the first time. Pinned by
   `fixtures/behaviour_member_invite.json`, 32 bodies through `json.Unmarshal`.
4. **`sqlx::query_as!` was tried for the two near-identical store queries and reverted.** It binds
   columns to fields by *position*, which turns "reorder two independent SELECT columns" — the
   no-op control both mutation plans depend on — into a silent value swap. The shared mapping is a
   `macro_rules!` over the anonymous, name-addressed `query!` row instead; the reasoning is in
   `mm-store/src/group_store.rs` so the next reader does not repeat the experiment.
5. **No stock role separates `sysconsole_read_user_management_groups` from `…_channels`.**
   `system_admin`, `system_manager`, `system_read_only_admin` and `system_user_manager` all hold
   both, so the mutation swapping the team route's gate for the channel route's survives against
   every session the suite otherwise has.
   `parity::team_admin::the_permission_is_the_groups_one_and_not_the_channels_one` plants two
   roles of one permission each and drives both routes with both tokens.
6. **Five anchors in `channel-admin.plan` went ambiguous** the moment a second
   `…MinusGroupMembers` query landed in `group_store.rs`, and an ambiguous anchor mutates the
   wrong copy silently. They now carry the `cm.`/`channelmembers` alias; `team-admin.plan` is the
   twin and carries `tm.`/`teammembers`.

7. **Mutations: 40 run over two passes, 34 caught, 2 controls survived, 0 harness faults.**
   The first pass reported one of the two controls CAUGHT — by
   `every_answer_this_server_gives_on_the_two_write_routes_is_a_refusal`, which the control cannot
   affect. That test was missing `ACTIVE_LICENCE_ROW.read()`, and `import`'s first gate is the
   licence: a sibling test planting a licence row turns three of its refusals into forwards. The
   lock is there now, and both controls survive.

   **One survivor is not a fixture gap and never can be**: `invite-reports-add-user-to-team`
   changes which permission the 403 *names*, and the name reaches only `detailed_error`, which
   `WipeDetailed` blanks on both servers when `EnableDeveloper` is false. The two bodies are
   byte-identical. The plan line stays as the record that the question was asked; the observable
   half — that `detailed_error` is empty on both — is asserted.

### What is not here

The cloud refusal at the top of `importTeam` (`License().IsCloud()` → 403
`api.restricted_system_admin`) and its `len(fileInfoArray) <= 0` branch are both unreachable — the
first because a cloud installation is licensed and a licensed server is forwarded whole, the second
because `ParseMultipartForm` never builds an empty list under a present key. Both are recorded in
[D-491] rather than asserted.


## Account conversion and the guest pair (2026-09-13)

| File | Rust | Status | Tests | Notes |
|---|---|---|---|---|
| api4/user.go (`convertUserToBot`), app/bot.go | `mm-api/src/user_convert.rs`, `mm-app/src/user_convert.rs` | PARTIAL | 2 pass + 5 parity | The user is fetched **before** `manage_system`, so a caller with neither gets 404 — the opposite of `promote` fifty lines away. Converting an account that is already a bot is a **500** `app.bot.createbot.internal_error` from the primary key, not a 400. The body has **no trailing newline** (`w.Write`). An account carrying an `AuthService` forwards, because `UserStore::update_auth_data` is not ported — [D-510]. |
| api4/bot.go (`convertBotToUser`), app/user.go | `mm-api/src/user_convert.rs`, `mm-app/src/user_convert.rs`, `mm-store/src/bot_store.rs` | DONE | 2 pass + 5 parity | Bot, then body, then permission. Four bodies reach one 400; a password that decodes but is too short fails **after** the patch is written, leaving the account patched and still a bot — reproduced, see note 2. The answer says `is_bot: true` and carries a stale `last_password_update`. One trailing newline, unlike its sibling. |
| api4/user.go (`promoteGuestToUser`), app/user.go, sqlstore/user_store.go | `mm-api/src/user_convert.rs`, `mm-app/src/user_convert.rs`, `mm-store/src/user_store.rs` | DONE | 5 parity | **No licence gate and no config gate** — the only half of the guest pair any server can run. Permission first, then the fetch, then two different 501s. The store's three statements are one transaction; `JoinDefaultChannels` runs outside it and re-adds the account as a **guest**, see note 3. |
| api4/user.go (`demoteUserToGuest`) | `mm-api/src/user_convert.rs` | PARTIAL | 2 parity | The licence is the second statement, ahead of the permission and the fetch, so on an unlicensed server every demote is the same 501 whoever asks and whatever id they name — measured. Only `RequireUserId` precedes it. Licensed forwards, before any read past the licence; the body behind it is [D-511]. |

### Notes

1. **Measured, not read.** Every gate order in the table came from a request to the running Go
   server, because all four differ and none is guessable: the same "no permission, no such user"
   request is a 404 on `convert_to_bot`, a 403 on `promote` and a 501 on `demote`.
2. **`ConvertBotToUser` has five writes and no transaction**, and one intermediate state is
   reachable from a client: roles, patch, password, bot-delete, in that order.
   `parity::user_convert::a_short_password_is_refused_after_the_patch_is_already_written` is the
   only test that can tell the port apart from one that validated the password up front.
3. **A promoted guest rejoins a default channel as a guest.** `JoinDefaultChannels` is handed the
   `*model.User` the handler read *before* the promotion, so the new `ChannelMembers` row is
   written with `SchemeGuest = true` after the transaction cleared every other one. Go's own
   staleness; the parity test compares the two servers rather than asserting zero.
4. **No stock role separates `promote_guest` from `manage_system`**, so a plain user's 403 pins
   nothing about *which* permission a route names.
   `parity::user_convert::each_route_names_its_own_permission` plants a role holding exactly
   `promote_guest` and drives all three permissioned routes with it.
5. **`parity::user_access_tokens` was sweeping `mmrsbot%` without `BOT_FIXTURES`.** It calls
   `unplant_bots`, the whole-prefix sweep, and a full-workspace run deleted a `user_convert`
   fixture mid-test — a failure naming a route that file never touches. The lock is there now.
