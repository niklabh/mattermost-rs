# mattermost-rs

An incremental port of the [Mattermost](https://github.com/mattermost/mattermost) server from Go
to Rust, translated bottom-up and deployed behind a Strangler Fig proxy.

**Wire compatibility with existing Mattermost clients is the hard requirement.** JSON field names,
casing, null-vs-omitted and numeric types must match the Go server's output exactly. Where
correctness and idiomatic Rust conflict on the wire format, the wire format wins.

---

## Status: phases 1-4 are live

The model crate, the store, the app layer and the REST API all serve real traffic. `mm-api`
listens on :8066, authenticates against session rows the **Go** server wrote, and forwards
anything it has not migrated — so a client cannot tell which server answered. Three surfaces are
up:

- **the HTTP api4 router** — reads and writes across users, teams, channels, posts, threads,
  files, emoji, webhooks, OAuth, roles, schemes, jobs, and the licence-gated families that answer
  by refusing before they read anything;
- **the local-mode admin API** — the same handlers on a unix socket with no session, which is
  what `mmctl --local` talks to. It is a *second* router with its own proxy leg, so an unmigrated
  local route still reaches the Go server's socket rather than a 404;
- **`GET /api/v4/websocket`** — the upgrade, both pumps, and the fan-out hub in
  `crates/mm-app/src/hub.rs`, `ShouldSendEvent`'s addressing rules included.

**No progress count lives in this file.** A README carrying counts is a README that is quietly
wrong most of the time, and every merged route would otherwise drag an unrelated edit along with
it. Ask the tree instead — `scripts/routes.py` derives the tally from the Go registrations and
*both* axum routers:

```sh
scripts/routes.py                  # served / total route+method pairs, per api4 file
scripts/routes.py --todo           # what is left on the HTTP router
scripts/routes.py --todo --local   # ... including the unix-socket routes
```

The denominator is **every** api4 route+method pair, the local-mode socket API and the
licensed/enterprise handlers included. A route no client calls is deferred, not dropped: the
strangler proxy is test apparatus, and the end state is a Go server that is not running.

**[`MIGRATION.md`](MIGRATION.md) is the authoritative ledger** — per-route status, test counts,
and the non-obvious Go semantics each translation turned up. Progress is tracked there and only
there.

Phase 5 has no crate of its own yet. The hub lives in `mm-app` and the socket in `mm-api`,
mirroring how Go splits `api4/websocket.go` from `app/platform`; `crates/mm-ws/` is an empty stub
kept for the day fan-out is worth its own binary.

**Out of scope**, to be proxied or generated rather than hand-translated: `client4.go`,
`client4_route.go` and `websocket_client.go` (Go's own REST and websocket *clients*, not server
code), and the `*_serial_gen.go` msgpack codecs, which encode a Go cache this server never
populates. `config.go` and `permission.go` *are* ported, but **generated** — 1,300-odd config
fields and 311 permission ids are exactly where a hand-copy mistypes a key in silence. `config.go`
is the wire shape only; `SetDefaults`, `IsValid` and the `Sanitize`/`Clone` family are not
ported.

---

## The parity oracle

This is the part of the repo worth understanding before anything else.

Translating a validator by reading the Go source and reasoning about it produces confident,
wrong code. So we don't. Every branching function is run through a **corpus in Go**, and the
answers are recorded as a fixture that the Rust tests assert against:

```
reference/dump/behaviour_*.go   →   fixtures/behaviour_*.json   →   #[cfg(test)] mod go_parity
```

`fixtures/*.json` is **generated**, never hand-written — a hand-written fixture asserts what you
already believe and cannot detect drift. Serialization fixtures are reflection-populated from
zero-valued structs, so every field carries a distinctive non-zero value and `omitempty` cannot
silently hide a field from its own test.

This is not ceremony. A representative sample of what the oracle caught that a careful reading of
the Go source did not:

- **`strings.ToLower` is not `str::to_lowercase`.** Go uses Unicode's *simple* case mapping;
  Rust uses the *full* mapping plus the Final_Sigma rule. They disagree on `İ` and on a trailing
  sigma — and the wrong one had already shipped in six call sites covering emails, usernames and
  team slugs.
- **`Path::extension` is not `filepath.Ext`.** Rust reads a leading dot as a stem, so `.hidden`
  has no extension in Rust and `hidden` in Go.
- **Go's `encoding/json` base64-encodes `[]byte`**; serde_json emits an array of numbers.
- **`SplitVersion` returns `i64::MAX`, not `0`, on overflow** — it discards `ParseInt`'s error,
  and Go returns the saturated bound *alongside* the error.
- **`ChannelMember.SetChannelMuted` ignores its argument** and toggles instead. Reproduced
  verbatim; "fixing" it would make two servers disagree about a column they both write.

Several upstream Go bugs are reproduced deliberately rather than fixed, and the reachable ones
are pinned by an oracle case — so a future reader who "repairs" one fails a test instead of
silently forking behaviour from a server sharing the same database. They are called out
individually in [`docs/TECH_DEBT.md`](docs/TECH_DEBT.md) and [`MIGRATION.md`](MIGRATION.md).

Where Go's behaviour is genuinely unportable — a table living in an unexported variable, or a
lookup that reads the host's `/etc` — the generator emits Rust source instead of guessing.
`crates/mm-model/src/emoji_generated.rs` is 4,464 emoji names emitted from `model.SystemEmojis`.

---

## A suite that passes on its first run is not evidence

The oracle says our answer matches Go's. It does not say the *test* would have noticed if it
didn't — a fixture where the right answer and the wrong answer coincide passes either way. So
anything that ships logic is mutated afterwards:

```sh
scripts/mutate.sh <name> <file> <from> <to> [suite]    # one mutation, one verdict
scripts/mutate-batch.sh scripts/mutations/<x>.plan     # a committed plan of them
scripts/preflight-plans.sh                             # every plan, checked against the tree
```

A mutation flips a decision a reader could plausibly get wrong — predicate direction, check
*order*, which column, which constant, off-by-one, a dropped `COALESCE` — and the run reports
CAUGHT or SURVIVED. **A survivor is a finding about the tests, not a shrug:** in the
`getChannelUnread` round three survivors each named a fixture where the right answer and the wrong
answer coincided, and fixing the fixture is what closed them.

Every run also carries **two no-op controls** (rename a binding, reorder two independent SELECT
columns). If a control fails, the harness is measuring something other than the mutation and the
whole run is void — which is not hypothetical: one plan scored 5 of 33 because the suite was
talking to a stale server left on the port, and the controls are what said so. Plans keep their
surviving mutations in their own header, so a later reader does not re-litigate them.

---

## Getting started

Requires Rust 1.85+ (edition 2024). Go 1.26+ is needed to regenerate fixtures **and** to build the
forward target — the Go server is compiled from the pinned source, not pulled as an image.

```sh
# The Go source is a read-only reference, pinned to a fixed commit and never vendored.
# Fetch the pinned SHA directly — a plain `clone --depth 1` only gets the current tip,
# which will not contain this commit once upstream moves on.
git init reference/mattermost
git -C reference/mattermost remote add origin https://github.com/mattermost/mattermost.git
git -C reference/mattermost fetch --depth 1 origin 9dfbaeca99f4096388fd1c048a9e6d1d0a86743e
git -C reference/mattermost checkout FETCH_HEAD

cargo test --workspace
```

`.sqlx/` is committed, so the workspace — compile-time checked queries included — builds and
tests with no database and no Go clone at all. The suite runs in well under a minute; the handful
of tests ignored for wall clock run under `scripts/slow-tests.sh`.

### Running the stack

Porting a route means asking both servers the same question and diffing the answers, so unlike the
model-only sessions this repo started with, you want the stack up most of the time. It is there
for three things:

- **the schema** — this repo contains no DDL and never will; the Go server's migrations create
  every table `mm-store` reads
- **the parity oracle** — the only way to claim our bytes match Go's is to ask Go for the same
  request and diff. Nearly every wrong belief this project has corrected was corrected here
- **a forward target** — for exercising the proxy, over the port and over the socket

```sh
docker compose up -d          # postgres :5432 — and postgres only
scripts/go-server.sh start    # the pinned Go server, built from source, on :8065
export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
cargo run -p mm-api           # :8066 — serves what is migrated, forwards the rest
```

Or `scripts/stack.sh up 0`, which does the first two and then seeds the fixture user, team and
channel every parity suite needs. Seeding is idempotent — it does nothing when the login already
works — so `up` and `seed` are safe to re-run. The first user created becomes the system admin,
and no id is hardcoded anywhere, so that user, team and channel are the only fixture state the
suites assume.

**The Go server is no longer a container.** `scripts/go-server.sh` builds it from the pinned
reference SHA and runs it on the host. The published image and `reference/mattermost/` are both
"11.11.0" and are *not* the same code: rc1 answers `GET /api/v4/bots` with a `system_owned` field
the pinned source has never heard of, and 404s routes the pinned source registers. A route whose
live shape disagrees with the source cannot be ported honestly — matching the source produces a
body the proxy's own target does not serve — so building the reference is what keeps "read the Go
source" and "ask the forward target" the same question. That is D-130, closed by this build; the
image remains in `docker-compose.yml` behind the `published-image` profile as a fallback, and the
same entry is why any tag there must stay pinned — `latest` once drifted a minor ahead of the
source, and every parity claim in the repo was measured against the wrong server for a while.
Building natively also retired the qemu tax on arm64.

Two route families are still absent from the forward target, and it is **not** skew:
`api4/view.go`'s seven and `api4/channel_join_request.go`'s seven sit behind feature flags that are
off at the pinned SHA. `scripts/go-boards.sh` runs a second Go server with
`IntegratedBoards` on, on its own port, because turning the flag on in the main one would move the
answers under routes already ported.

**Both servers also share one configuration**, not just one database. `MM_CONFIG` points the Go
server at the shared Postgres, so it keeps `model.Config` in a `Configurations` row instead of a
`config.json` on a volume the Rust process cannot see — and `mm-api` reads that row at startup,
refusing to start and saying so if it is missing. See D-156.

**Local mode is opt-in, and the two sockets must differ.** With
`MM_SERVICESETTINGS_ENABLELOCALMODE=true`, `mm-api` binds its own socket (`MM_API_LOCAL_SOCKET`,
default `/var/tmp/mmrs_local.socket`) and forwards unmigrated local routes to the Go server's
(`MM_SERVICESETTINGS_LOCALMODESOCKETLOCATION`). Go's `startLocalModeServer` opens with
`os.RemoveAll`, so pointing both at one path means whichever starts second unlinks the other's
socket and then proxies to itself; `mm-api` refuses that configuration at startup rather than
letting you discover it later.

The Go server owns the schema and runs the migrations. **Never point a migration tool at this
database from the Rust side** — the two would race, and Go's migrations are the reference.

Every response carries `x-mmrs-served-by: rust` or `: go`, so you can see the cutover:

```sh
curl -si localhost:8066/api/v4/system/ping | grep -i served-by     # -> go
```

## Several stacks at once

Route work parallelises, and a *stack* is what makes that safe: one Postgres, one pinned Go
server and one `mm-api`, sharing nothing with any other stack.

```
stack k    postgres  5432 + k      go  8065 + 100k      mm-api  8066 + 100k
```

Stack **0 is the layout above**, container names and volume included, so everything in this README
keeps working with no flags. Additional stacks exist so several worktrees can run the parity and
mutation suites *at the same time* — `scripts/stack-lock.sh` is per stack, and it used to be one
lock for the machine, which made a twenty-minute mutation batch block every other checkout.

```sh
scripts/stack.sh up 1 2 3        # three more, each seeded with the fixture user and team
scripts/stack.sh status
scripts/worktree.sh add members 1   # a worktree pinned to stack 1, pre-building in the background
scripts/stack.sh down 1
```

A worktree pins itself with a `.mmrs-stack` file, so `scripts/parity.sh` and `scripts/mutate.sh`
inside it need no arguments. The test harness's `common::GO` and `common::RUST` are baked in at
compile time from `MMRS_GO_BASE`/`MMRS_RUST_BASE` (`crates/mm-api/build.rs` registers the
`rerun-if-env-changed`), because they are `&'static str` consts used in about thirteen hundred
inline format captures — a runtime lookup would mean rewriting every one.

The cross-server parity test is the oracle for anything migrated. It needs the stack up and a
user to log in as, and it is skipped unless explicitly enabled, so `cargo test` stays green on a
machine with no Docker:

```sh
MM_PARITY_STACK=1 cargo test -p mm-api --test parity users_me
```

The store tests need only Postgres, not the Go server, and are gated separately. They read the
roles the Go server wrote at startup and insert their own scheme rows, cleaning up after
themselves:

```sh
MM_STORE_DB=1 cargo test -p mm-store --test db_roles_schemes
MM_STORE_DB=1 cargo test -p mm-app --test db_authorization
```

The permission checks are gated the same way. They read the roles and users the Go server created,
so what they assert is what the reference implementation would answer.

In practice neither gate is set by hand: `scripts/parity.sh` rebuilds `mm-api` from the current
checkout, replaces whatever is bound to the stack's port with it, takes the stack lock and runs
the suites with both gates set. **Identify a server by its port, never by its command line** — a
stale server answering `/system/ping` has twice produced confident wrong answers here, once 36
false failures and once a whole mutation plan's worth of false verdicts.

Branches from several worktrees merge one at a time through `scripts/merge-worktrees.sh`, which
re-runs the suite after each: four branches that each passed alone are not four branches that
pass together, because the parity suites share fixture users, teams and channels and two routes
that each sort correctly can still tie on a sort key once both sets exist.

`.sqlx/` is committed, so `SQLX_OFFLINE=true cargo check --workspace` builds the compile-time
checked queries with no database at all. Re-run `cargo sqlx prepare --workspace` after changing
one.

### Regenerating fixtures

The committed fixtures are enough to run the test suite, so the clone is only needed to
**regenerate** them or to translate another file:

```sh
cd reference/dump && go run .    # rewrites fixtures/ and emoji_generated.rs
```

Output is deterministic — no `rand`, no `time.Now` — so a clean run touches only new files.
Anything else appearing in `git status` is a signal worth reading.

### Definition of done for a change

```sh
cargo fmt && cargo check --workspace && cargo clippy --all-targets -- -D warnings && cargo test --workspace
```

Plus `scripts/mutate.sh` over anything with logic in it, and the tally reported — *N run, N
caught, N controls survived*. Plus `gofmt -l reference/dump/` and `go vet ./...` if you touched
the generator.

---

## Layout

```
crates/
  mm-model/      phase 1  wire types; zero internal dependencies
  mm-store/      phase 2  persistence (sqlx, Postgres); depends on mm-model
  mm-app/        phase 3  business logic; depends on mm-store; knows nothing about HTTP
  mm-api/        phase 4  REST + the Strangler Fig proxy; depends on mm-app
  mm-ws/         phase 5  empty stub; the hub is in mm-app and the socket in mm-api for now
fixtures/        generated parity fixtures — never edit by hand
scripts/         the harness: stacks, worktrees, parity runs, mutation plans
reference/
  mattermost/    pinned Go source, read-only, gitignored
  dump/          the fixture generator and behavioural oracles
docs/
  MIGRATION_STRATEGY.md   the plan: phases, sequencing, proxy cutover
  TECH_DEBT.md            what we owe — deferred work and known divergences
  PROMPTS.md              per-phase execution prompts
MIGRATION.md     THE LEDGER: per-route status and hard-won semantics
CLAUDE.md        agent context — read this before contributing
```

The dependency direction is strict and enforced by review:
`mm-model ← mm-store ← mm-app ← {mm-api, mm-ws}`.

### Where state lives

Sessions are short and context does not carry across them. Three files carry it instead:

- **`MIGRATION.md`** — what is translated, and every non-obvious Go semantic discovered while
  doing it. Most entries cost real time to find and are not recoverable by re-reading the source
  casually.
- **`docs/TECH_DEBT.md`** — a backlog, not a diary: numbered entries for work genuinely
  deferred, each `OPEN` (owed), `ACCEPTED` (a deliberate permanent divergence) or `CLOSED` (paid
  off). A permanent finding belongs in a doc comment on the thing it constrains and a two-sentence
  `MIGRATION.md` row; only something actually owed gets an entry here.
- **`CLAUDE.md`** — the rules a contributor (human or agent) is expected to follow.

---

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first. The rules that matter most:

- **The unit of work is a route, not a file.** Pick a route; port the handler, the app function
  and the store function behind it. Porting a model file with no route to exercise it is how this
  project accumulated ~20,000 lines of unreachable code.
- **Never edit `fixtures/` by hand.** Extend the generator and re-run it.
- **Every translated file ships with tests in the same file** — serialization parity against a
  generated fixture, and a behavioural test per branch for anything with logic.
- **Never claim parity you did not verify with a test.** If you guessed at a JSON tag, say so.
- **Mutate what you ship.** A suite that passes on its first run has not been tested yet.
- No `unwrap`/`expect`/`panic!` in library code. No `.clone()` to appease the borrow checker.
- The Go tree under `reference/mattermost/` is **read-only**.

Deferred work belongs in `docs/TECH_DEBT.md`, not in a comment — that register is how it
survives a context reset.

---

## License

**The license is split, the way upstream splits it** — see [`NOTICE`](NOTICE) for the full
statement.

| Path | License | Derived from |
|---|---|---|
| `crates/mm-model/` | **Apache-2.0** ([text](crates/mm-model/LICENSE)) | `server/public/model/` |
| `crates/mm-store/`, `mm-app/`, `mm-api/`, `mm-ws/` | **AGPL-3.0-only** ([text](LICENSE)) | `server/channels/{store,app,api4}/` |
| everything else | **AGPL-3.0-only** | — |

Upstream Mattermost is licensed in two parts: `server/public/`, `server/templates/`,
`server/i18n/` and `webapp/` are Apache-2.0, and the rest of the platform is GNU AGPL v3.0 or a
commercial license from Mattermost, Inc. Collapsing that into a single AGPL root would have been
cheaper, but `mm-model` is the part of this repo another project is most likely to want, and it
owes nothing to the AGPL half — so it keeps the more permissive terms.

`crates/mm-model/LICENSE` is a byte-identical copy of upstream's `server/public/LICENSE.txt`;
the root `LICENSE` is the verbatim GNU AGPL v3.0.

Apache-2.0 is one-way compatible with AGPL-3.0, so the AGPL crates may depend on `mm-model`.
**The reverse must never happen:** `mm-model` cannot take code or a dependency from an
AGPL-licensed crate, or from `server/channels/`. The existing rule that `mm-model` has zero
internal dependencies already enforces this, but it is now a licensing requirement and not only
an architectural one.

The AGPL crates carried `AGPL-3.0-only` from before they held a single AGPL-derived line, and
that was the point — the label is a precondition for the first commit of code derived from
`server/channels/`, not a consequence of it. Resolved as **D-031** in
[`docs/TECH_DEBT.md`](docs/TECH_DEBT.md).

This is a translation, not a copy: no file here is copied from upstream, and a few functions
deliberately diverge. `NOTICE` records that, as Apache-2.0 §4(b) requires.

"Mattermost" is a trademark of Mattermost, Inc. This is an unofficial port, not affiliated with,
endorsed by, or supported by Mattermost, Inc.
