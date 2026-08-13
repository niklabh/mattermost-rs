# `claude code` Execution Prompts

Copy-pasteable, one phase at a time. All Go paths are relative to your Rust project root and
assume `reference/mattermost/` is a pinned shallow clone.

**Universal rules for using these:**
- Run `/clear` between every file. Context does not need to carry across files — `MIGRATION.md`
  and the compiler carry it instead.
- Every prompt ends at a stopping point. Review the diff, then run the next one.
- If a session dies mid-file, use the **Resume** prompt at the bottom. It works in every phase.

---

## Phase 0 — Setup (run once)

### 0.1 Scaffold

```
Create a Cargo workspace for this project. Do not read any Go source.

Root Cargo.toml with workspace members: crates/mm-model, crates/mm-store, crates/mm-app,
crates/mm-api, crates/mm-ws. Create each crate as a library (mm-api and mm-ws also get a
src/main.rs stub that does nothing yet).

Workspace dependencies, pinned in [workspace.dependencies]:
tokio (features full), axum, sqlx (features runtime-tokio-rustls, postgres, chrono, json,
macros, migrate), serde (derive), serde_json, thiserror, anyhow, tracing,
tracing-subscriber (env-filter), chrono.

Dependency direction is strict: mm-model has no internal deps; mm-store depends on mm-model;
mm-app on mm-store; mm-api and mm-ws on mm-app.

MIGRATION.md ALREADY EXISTS and holds the pinned Go SHA, verified line ranges, and the
out-of-scope list. Read it, but do NOT overwrite or restructure it. Your only edit is to set
"Current phase:" to "1 — Core Types" and "Next file:" to
server/public/model/utils.go once the workspace compiles.

Then run `cargo check --workspace` and report the result. Stop.
```

### 0.2 Build the parity oracle

```
Do not read any Go source files.

Write reference/dump/main.go — a standalone Go program in package main that imports
github.com/mattermost/mattermost/server/public/model.

Structure it as a HARNESS PLUS A REGISTRY, not as a flat sequence of marshal calls:

  var registry = map[string]any{
      "user": model.User{ ... },
      ...
  }

and a main() that loops the registry, marshals each value to indented JSON, and writes it to
fixtures/<key>.json. Adding a type later must be a ONE-LINE change to the registry — later
sessions will append to it as they translate each type, so the loop must be fully generic.

Seed the registry with exactly these nine keys:

  user, team, channel, channel_member, post, session, team_member, status, preference

"Fully populated" means every field set to a distinctive non-zero value — no zero values, no
empty strings, no nil maps — so that omitempty behaviour and field naming are both visible in
the output. Use realistic 26-character IDs and epoch-millisecond timestamps. A zero-valued
field is omitted from the fixture and its parity test then proves nothing; this is the single
most important property of the file.

Also write reference/dump/README.md with the exact command to run it, and a one-paragraph note
telling future sessions to append their type to the registry and re-run.

Do not run it — I will run it myself and commit the fixtures. Stop when the file is written.
```

Then you run: `cd reference/dump && go mod init dump && go mod tidy && go run .`

Commit `fixtures/`. These files are now the contract.

---

## Phase 1 — Core Types (`model/` → `mm-model`)

299 Go files. Do not attempt them all. Migrate in dependency order and stop when the types the
next phase needs are covered.

### 1.1 First file (establishes the pattern — review this one carefully)

```
Phase 1, file 1 of the model migration.

Read ONLY reference/mattermost/server/public/model/utils.go. Do not read any other file.

Translate it to crates/mm-model/src/utils.rs. Scope: ID generation, the string/slice helpers,
and the timestamp helpers. Skip anything that is Go-runtime-specific (reflection helpers,
http.Request helpers, io wrappers) — list what you skipped and why, rather than translating it.

Critical semantics to preserve exactly:
- NewId() is base32 encoding of a random UUID producing a 26-character string. It is NOT a
  UUID string. Reproduce the exact alphabet and length.
- Timestamps are epoch MILLISECONDS as i64.

Write #[cfg(test)] unit tests in the same file covering: NewId length and character set,
NewId uniqueness across 1000 calls, and every helper's edge cases (empty input, boundary
lengths).

Then run `cargo test -p mm-model` and `cargo clippy -p mm-model -- -D warnings`.

Update MIGRATION.md: set Current phase to "1 — Core Types", mark this row DONE with the test
count, and add a Note for anything non-obvious you found. Set "Next file:" to
server/public/model/user.go.

Report: files written, test results, anything you were unsure about, next file. Then stop.
```

### 1.2 The repeatable loop prompt

Run `/clear`, then paste this with the two paths swapped each time:

```
Continue Phase 1. Read MIGRATION.md first for context and prior Notes.

Read ONLY reference/mattermost/server/public/model/user.go. No other file.

Translate the structs and their methods to crates/mm-model/src/user.rs:
- Every Go struct with `json:` tags becomes a serde struct. Map every tag with an explicit
  #[serde(rename = "...")]. Do not rely on rename_all.
- Go pointer fields -> Option<T>. Go `omitempty` on a non-pointer -> skip_serializing_if
  predicate on the concrete type, NOT Option.
- Translate IsValid(), PreSave(), PreUpdate(), Sanitize(), and any Etag/slug logic. Skip
  anything that touches a database, an http.Request, or another package.
- If a method needs a type from another Go file, do NOT open that file — stub the dependency,
  note it in MIGRATION.md, and tell me.

Tests, in the same file:
1. Parity: deserialize fixtures/user.json, re-serialize, assert the serde_json::Value graphs
   are equal. This must pass before you consider the file done.
2. One test per branch of every validation/sanitization method, error branches included.

If this file declares a type with `json:` tags that has NO fixture yet, append it to the
registry in reference/dump/main.go — one line, every field a distinctive non-zero value. Do
not run the generator; tell me it needs re-running. Fall back to values transcribed from the
Go source for this session's test, and note in MIGRATION.md that the parity test is
provisional until the fixture lands.

Run `cargo test -p mm-model` and `cargo clippy -p mm-model -- -D warnings`.
Update MIGRATION.md (status, test count, Notes, Next file).
Report and stop. Do not start the next file.
```

**Suggested order for Phase 1** — ascending dependency count:
`utils.go` → `user.go` → `team.go` → `session.go` → `channel.go` → `channel_member.go` →
`post.go` → `team_member.go` → `status.go` → `preference.go` → `websocket_message.go` →
`emoji.go` → `file_info.go`

### 1.3 The two files that need special handling

`permission.go` (2,789 lines) — do not translate by hand:

```
Do not read reference/mattermost/server/public/model/permission.go — it is 2,789 lines.

Instead write a one-off Go program at reference/dump/permissions.go that imports the model
package, iterates model.AllPermissions, and emits crates/mm-model/src/permission_generated.rs
containing a `pub const` &str per permission ID plus a static slice of all of them.

Then write crates/mm-model/src/permission.rs by hand containing only the Permission struct and
any behavioural methods, with `include!` or a `mod` declaration pulling in the generated file.

Do not run the generator — I will. Stop when both files are written.
```

`config.go` (5,795 lines) — translate lazily:

```
Read ONLY lines 1-260 of reference/mattermost/server/public/model/config.go using
`sed -n '1,260p'`. Do not Read the whole file — it is 5,795 lines.

Translate only the ServiceSettings struct and its SetDefaults method to
crates/mm-model/src/config/service_settings.rs. Ignore every other config section for now.

Same serde and test rules as prior files. Parity test against fixtures/ if a config fixture
exists; if not, say so and test against defaults transcribed from the Go source.

Update MIGRATION.md with a Note recording which line range of config.go is now covered, so the
next session knows where to resume. Report and stop.
```

### Phase 1 gate

```
Phase 1 review. Do not read any Go source.

Run `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and
`cargo fmt --check`.

Then read MIGRATION.md and produce a gap report:
- Which fixtures in fixtures/ have no corresponding parity test
- Which rows in the ledger are marked DONE but have a Note flagging a stub or uncertainty
- Which types mm-store will need in Phase 2 that are not yet migrated

Output the report as a markdown table. Change no code. Stop.
```

---

## Phase 2 — Database Layer

Two sub-phases: traits first (from the Go interfaces), then SQLx implementations. Do not
interleave them — the traits must be stable before anything implements them.

### 2.1 Traits from Go interfaces (ranged reads, one interface per session)

```
Phase 2a. Read MIGRATION.md first.

reference/mattermost/server/public/../channels/store/store.go is 1,471 lines. Do NOT Read it.
Read ONLY lines 448-550 using `sed -n '448,550p'
reference/mattermost/server/channels/store/store.go` — this is the UserStore interface.

Translate it to a Rust trait in crates/mm-store/src/traits/user.rs:
- Native async fns in trait (RPITIT). Do not use #[async_trait].
- Every method returns Result<T, StoreError>.
- Go's `rctx request.CTX` first parameter maps to `&RequestContext` — if that type does not
  exist yet, define a minimal placeholder struct in crates/mm-store/src/context.rs and note it.
- Methods whose Go signature references a model type we have not migrated: comment the method
  out with a `// BLOCKED: needs model::X` marker and list them in your report. Do not stub the
  model type.

Also create crates/mm-store/src/error.rs with a StoreError thiserror enum modelled on the error
kinds in reference/mattermost/server/channels/store/errors.go — read that file, it is small.

No SQL and no implementation in this session. Traits and errors only.

Run `cargo check -p mm-store`. Update MIGRATION.md, noting the line range consumed and every
BLOCKED method. Report and stop.
```

Line ranges you'll need (verified against the pinned source):

| Interface | Range |
|---|---|
| `TeamStore` | `sed -n '135,199p'` |
| `ChannelStore` | `sed -n '200,386p'` |
| `PostStore` | `sed -n '387,447p'` |
| `UserStore` | `sed -n '448,550p'` |
| `SessionStore` | `sed -n '551,600p'` |

Re-verify with `grep -n "^type .*Store interface" reference/mattermost/server/channels/store/store.go`
if your pinned SHA differs.

### 2.2 SQLx implementation (one store per session)

```
Phase 2b. Read MIGRATION.md first.

Read ONLY these two files:
1. crates/mm-store/src/traits/user.rs (the trait you are implementing)
2. reference/mattermost/server/channels/store/sqlstore/user_store.go

Implement PgUserStore in crates/mm-store/src/pg/user.rs for these methods ONLY:
  Save, Update, Get, GetByEmail, GetByUsername, GetAllProfiles, PermanentDelete
Leave every other trait method as `todo!()` with a `// PHASE2b-PENDING` comment.

Rules:
- sqlx::query_as! / query! macros with compile-time verification where the query is static.
  Use QueryBuilder for the genuinely dynamic ones. Never format!() SQL — no string
  interpolation of user input anywhere, ever.
- Copy the Go SQL semantics exactly: same columns, same WHERE clauses, same ORDER BY, same
  LIMIT handling. If the Go code uses a builder, reconstruct the emitted SQL and put it in a
  comment above the Rust query.
- Postgres only. Ignore any MySQL branches in the Go code.
- Map unique-violation and not-found to the correct StoreError variants — match Go's behaviour
  on duplicate email and duplicate username specifically.

Tests: #[sqlx::test] integration tests against a real Postgres, one per implemented method,
including the duplicate-key and not-found paths. Assume DATABASE_URL is set and the schema was
migrated by the Go server.

Run `cargo check -p mm-store`. If DATABASE_URL is unset, say so and skip running tests rather
than mocking the database.

Update MIGRATION.md. Report and stop.
```

### Phase 2 gate

```
Phase 2 review. Read MIGRATION.md, then:

1. `grep -rn "PHASE2b-PENDING" crates/mm-store/src | wc -l` — report the count.
2. `grep -rn "todo!()" crates/mm-store/src --include=*.rs -l` — report which files.
3. `grep -rn "format!" crates/mm-store/src --include=*.rs` — any hit near SQL is a security
   finding; report it prominently.
4. Run `cargo test -p mm-store` if DATABASE_URL is set.

Produce a table of trait methods implemented vs pending, per store. Change no code. Stop.
```

---

## Phase 3 — Business Logic (`app/` → `mm-app`)

No HTTP in this phase. Services take typed inputs and a store handle, and return typed outputs.

### 3.1 Session validation first (the Strangler Fig prerequisite)

```
Phase 3, file 1. Read MIGRATION.md first.

Read ONLY reference/mattermost/server/channels/app/session.go. No other file.

Translate the session *validation* path to crates/mm-app/src/session.rs:
  GetSession, validate token, expiry check, and the "is this session still valid" logic.
Skip session creation, OAuth, MFA, and anything touching a cluster or cache for now — list
what you skipped.

This is the highest-risk file in the migration: the Rust and Go servers must accept the exact
same MMAUTHTOKEN against the shared Sessions table. Any divergence in expiry arithmetic or
token comparison is an auth bug. Call out every place you were not certain.

Define the service as a struct generic over the store trait, constructor-injected — no globals,
no lazy_static, no OnceCell.

Tests: unit tests with a hand-written mock store (a plain struct implementing the trait — do
not add a mocking crate). Cover: valid session, expired session, unknown token, revoked
session, and the exact boundary millisecond of expiry.

Run `cargo test -p mm-app`. Update MIGRATION.md. Report and stop.
```

### 3.2 The repeatable loop prompt

```
Continue Phase 3. Read MIGRATION.md first.

Read ONLY reference/mattermost/server/channels/app/user.go, lines 1-450, using
`sed -n '1,450p'`. Do not read the rest of the file yet and do not read any other file.

Translate only these functions to crates/mm-app/src/user.rs:
  CreateUser, GetUser, GetUserByEmail, GetUserByUsername, UpdateUser

For each one:
- Preserve the validation order exactly as Go has it. The specific error returned for a given
  bad input is part of the API contract — clients depend on the error id strings.
- Where Go calls another app service we have not migrated, take it as an injected trait
  dependency and define the minimal trait here. Do not read that service's Go file.
- Where Go emits a websocket event or a cluster message, insert a `// PHASE5: emit <event>`
  comment. Do not implement it.

Tests: mock store, one test per validation branch including every error path, plus a happy
path per function.

Run `cargo test -p mm-app`. Update MIGRATION.md with the line range consumed and every
injected-trait stub you created. Report and stop.
```

### Phase 3 gate

```
Phase 3 review. Do not read Go source.

`grep -rn "PHASE5:" crates/mm-app/src` — list every deferred websocket event.
`grep -rn "todo!()\|unimplemented!()" crates/mm-app/src` — list every hole.
Run `cargo test -p mm-app` and report pass/fail counts per module.

Then read MIGRATION.md and list which app services are complete enough to expose over HTTP in
Phase 4. Change no code. Stop.
```

---

## Phase 4 — API Layer (Axum + the Strangler Fig proxy)

### 4.1 The proxy shell first — before any route

```
Phase 4, file 1. Do not read any Go source.

Build the Strangler Fig shell in crates/mm-api:

src/main.rs — tokio main, tracing-subscriber with EnvFilter, build AppState (PgPool + the
mm-app services), bind 0.0.0.0:8065.

src/proxy.rs — an Axum fallback handler that forwards ANY unmatched request to the Go server at
$MM_GO_UPSTREAM (default http://localhost:8066) using hyper. It must preserve method, path,
query string, all headers (Cookie and Authorization especially), and the body, and stream the
response back unmodified including status and headers. This is the load-bearing piece: if it
mangles a header, every unmigrated route breaks.

src/routes.rs — an empty Router with the fallback wired in, plus a read of the MM_RS_ROUTES env
var (comma-separated) so individual route groups can be enabled and disabled without a rebuild.

Tests: a test that the proxy forwards headers and status faithfully, using a local wiremock or
a hand-rolled hyper test server.

Run `cargo test -p mm-api`. Update MIGRATION.md. Report and stop.
```

**Human checkpoint:** start the Go server on 8066, start `mm-api` on 8065, open the Mattermost
webapp against 8065. Everything must work exactly as before — you have migrated zero routes.
This proves the proxy before it can hide a bug.

### 4.2 First real route group

```
Phase 4. Read MIGRATION.md first.

Read ONLY reference/mattermost/server/channels/api4/user.go, lines 1-300, using
`sed -n '1,300p'`. Do not read any other file.

Migrate these handlers to crates/mm-api/src/handlers/user.rs:
  GET /api/v4/users/{user_id}
  GET /api/v4/users/username/{username}
  GET /api/v4/users/email/{email}

Requirements:
- Axum handlers taking State<AppState> and typed path extractors.
- An auth extractor that validates MMAUTHTOKEN via the mm-app session service (Phase 3.1).
- Response JSON must be byte-identical to Go's, including field order-insensitivity and the
  sanitization Go applies before returning a User to a non-admin caller. Getting sanitization
  wrong here leaks email addresses and auth data — verify it against the Go source explicitly
  and say what you verified.
- Error responses must match Go's AppError JSON shape exactly: id, message, detailed_error,
  request_id, status_code. Clients switch on the `id` string.
- Register these routes behind the MM_RS_ROUTES gate so they can be turned off at runtime.

Tests: axum integration tests per route — 200, 401 unauthenticated, 404 unknown user, and one
asserting a non-admin caller does not receive sanitized-out fields.

Run `cargo test -p mm-api`. Update MIGRATION.md. Report and stop.
```

**Human checkpoint after every route group:** enable the group, exercise it from the real
webapp, then diff Rust vs Go responses for the same request:

```bash
diff <(curl -s -H "$AUTH" localhost:8065/api/v4/users/me | jq -S .) \
     <(curl -s -H "$AUTH" localhost:8066/api/v4/users/me | jq -S .)
```

An empty diff is the only acceptable result. Make this a script and run it for every route.

---

## Phase 5 — WebSocket / Concurrency

### 5.1 Read the hub before touching it

```
Phase 5, session 1. Analysis only — write no Rust.

Read ONLY reference/mattermost/server/channels/app/platform/web_hub.go.

Produce docs/ws-hub-design.md describing:
- How Go shards connections across hubs and why
- The exact lifecycle of a WebConn: register, broadcast, unregister, and how a slow/dead client
  is detected and dropped
- Every channel in the Go hub, its buffer size, and what happens when it fills
- Where the Go implementation relies on goroutine-per-connection semantics that do not map
  cleanly to Tokio tasks

Then propose the Tokio design: which Go channels become tokio::sync::broadcast vs mpsc vs
watch, and how backpressure is handled. Be specific about the failure mode when a client cannot
keep up — this is where the Go code has real, hard-won behaviour, and a naive translation drops
messages or leaks memory.

Write the doc. Do not write Rust. Stop.
```

**Human checkpoint: read this document properly before approving.** The hub is where a naive
translation produces a system that passes every test and falls over at 500 concurrent
connections. Everything before this phase was mechanical; this part is a design decision.

### 5.2 Implement

```
Phase 5, session 2. Read docs/ws-hub-design.md (which we agreed) and MIGRATION.md.
Do not read any Go source this session.

Implement the hub in crates/mm-ws/src/hub.rs per the approved design:
- A Hub struct owning connection registration and fan-out via tokio::sync::broadcast
- Per-connection tokio tasks with an mpsc send queue and the agreed bounded-queue policy
- Graceful shutdown via CancellationToken
- No Mutex held across an .await, anywhere. Flag it if you think you need one.

Tests: registration/unregistration, broadcast reaches all subscribers, a slow consumer is
dropped per the agreed policy without stalling other consumers, and shutdown drains cleanly.
Include a test with 1,000 simulated connections asserting no task leak.

Run `cargo test -p mm-ws`. Update MIGRATION.md. Report and stop.
```

### 5.3 Wire it up

```
Phase 5, session 3. Read MIGRATION.md and crates/mm-ws/src/hub.rs.
Read ONLY reference/mattermost/server/channels/app/platform/websocket_router.go for the
message envelope format.

Add the /api/v4/websocket upgrade handler in crates/mm-ws/src/handler.rs using axum's
WebSocketUpgrade, authenticating via the same session extractor as Phase 4.

Then resolve the deferred events: `grep -rn "PHASE5:" crates/mm-app/src` and implement each one
as a hub publish. Report the list before you start implementing so I can confirm the scope.

The event JSON envelope must match Go exactly — existing mobile and desktop clients parse it and
will silently misbehave on drift.

Tests: end-to-end test with two connected clients where one's action produces the other's event.

Run `cargo test --workspace`. Update MIGRATION.md. Report and stop.
```

---

## Universal Resume Prompt

Use this whenever a session was interrupted, or when you return after a break and don't
remember where you were. It is deliberately cheap.

```
Session resume. Do this and nothing else:

1. Read MIGRATION.md.
2. Run `git status --short` and `git log --oneline -5`.
3. Run `cargo check --workspace 2>&1 | tail -30`.

Report:
- Current phase and the file the ledger says is next
- Any uncommitted work, and whether it compiles
- Whether the in-flight file (if any) has tests, and whether they pass

Do NOT read any Go source. Do NOT start or continue any translation. Recommend the single next
action and stop.
```

## Universal Recovery Prompt (when a translation is wrong)

```
Do not read any Go source yet.

The Rust file crates/<path>.rs fails parity: <paste the failing assertion or the curl diff>.

Read ONLY that Rust file and fixtures/<type>.json. Diagnose the mismatch from those two alone
and propose a fix. If — and only if — you cannot determine the correct behaviour from those two
files, tell me exactly which Go line range you need and stop; I will paste it.
```

---

## Session hygiene checklist

Between every file:

```bash
cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace
git add -A && git commit -m "migrate(model): user.go -> user.rs"   # one file per commit
```

Then `/clear`.

One file per commit is what keeps review to five minutes and makes `git revert` a precise tool
when a translation turns out subtly wrong three phases later.
