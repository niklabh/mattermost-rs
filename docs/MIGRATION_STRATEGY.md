# Mattermost Go → Rust: Migration Strategy

**Target stack:** Tokio · Axum · SQLx (Postgres) · Serde · Tracing · thiserror
**Method:** Bottom-Up (Leaf-Node First) translation, delivered behind a Strangler Fig proxy.

---

## 0. Repository layout (set this up once)

```
mattermost-rs/                  # your Rust project, the working directory for claude code
├── CLAUDE.md                   # persistent agent context (artifact #2)
├── MIGRATION.md                # THE LEDGER — the only state that survives sessions
├── Cargo.toml                  # workspace
├── crates/
│   ├── mm-model/               # Phase 1  (leaf: zero internal deps)
│   ├── mm-store/               # Phase 2  (depends on mm-model)
│   ├── mm-app/                 # Phase 3  (depends on mm-store)
│   ├── mm-api/                 # Phase 4  (depends on mm-app)
│   └── mm-ws/                  # Phase 5  (depends on mm-app)
├── fixtures/                   # golden JSON emitted by Go — the parity oracle
├── docs/ledger/                # per-phase archived ledger rows (keeps MIGRATION.md small)
└── reference/mattermost/       # shallow clone of the Go repo, PINNED to one commit
```

Pin the Go source. Mattermost's `master` moves daily; a moving reference silently invalidates
already-translated files.

```bash
git clone --depth 1 https://github.com/mattermost/mattermost.git reference/mattermost
cd reference/mattermost && git rev-parse HEAD   # record this SHA in MIGRATION.md
echo "reference/" >> ../../.gitignore           # don't vendor 1M+ lines into your history
```

---

## 1. Why Bottom-Up (Leaf-Node First)

The dependency graph of the Go server is roughly:

```
api4/ ──► app/ ──► store/sqlstore ──► store/ (interfaces) ──► public/model/
 (web/)                                                        ▲
                                                          zero inbound deps
```

`public/model/` is the **leaf**: it imports almost nothing from the rest of the server. Every
layer above imports it. Translating leaves first means:

1. **Every session compiles.** You never have a half-defined type stubbed with `todo!()` that
   the agent must "remember" across a context boundary. The compiler holds the state, not the
   context window.
2. **Zero speculative reads.** When translating `user.go`, the agent needs `user.go` and
   nothing else, because `user.go` needs nothing else. That is the whole token argument.
3. **Type errors surface immediately.** If `Post::props` is the wrong type, `cargo check` at
   Phase 2 fails loudly instead of producing a plausible-looking wrong API response at Phase 4.

The inverse (top-down from `api4/`) forces the agent to read the entire transitive closure of
handler → service → store → model on the very first file. That is the context blowup you are
trying to avoid.

**Ordering rule inside a phase:** translate in ascending order of inbound dependency count.
In `model/`: `utils.go` → `user.go` → `team.go` → `channel.go` → `post.go` → the rest.

---

## 2. Why Strangler Fig on top of it

Bottom-up alone gives you no running system until Phase 4 completes — months of unverifiable
work. The Strangler Fig gives you a deployable, revertible system from the first migrated route.

**Topology:**

```
          ┌───────────────────────────────────────────┐
client ──►│  mm-api (Axum) :8065                      │
          │                                           │
          │  migrated route?  ──yes──► Rust handler   │
          │        │                                  │
          │        └──no──► reverse_proxy ──────────► │──► Go mattermost :8066
          └───────────────────────────────────────────┘
                          │                                      │
                          └──────────► same Postgres ◄───────────┘
```

Rust sits in **front**, not behind. A single Axum fallback handler proxies everything unmigrated
to the Go binary via `hyper`. Route-by-route, you move handlers left across the boundary.

Three constraints make this work, and they are non-negotiable:

- **Shared database, shared schema.** The Rust side runs *no* migrations of its own during the
  migration. `sqlx::migrate!` points at the Go repo's `db/migrations/postgres/` directory,
  read-only. The Go server remains the schema owner until the last route flips.
- **Shared sessions.** Both read the `Sessions` table and validate the same `MMAUTHTOKEN`
  cookie / `Authorization: Bearer` header. Auth is therefore a **Phase 3** item, not Phase 4 —
  you cannot serve a single authenticated route until session validation has parity.
- **Cluster events cross the boundary.** Until Phase 5, the Rust side publishes WebSocket
  events by writing to the same cluster bus the Go hub reads. Simplest correct version: Rust
  handlers `POST` events to a local Go endpoint; the Go hub fans out. Do not attempt dual hubs.

**Kill switch:** every migrated route is gated by an env var (`MM_RS_ROUTES=users,posts`).
A bad translation is a config rollback, not a redeploy.

---

## 3. Token discipline — the actual mechanics

The context window is the scarce resource. Five rules, enforced by `CLAUDE.md` and by how you
write prompts:

### 3.1 One file in, one file out
A session translates **one** Go file to **one** Rust file. The prompt names both absolute paths.
`CLAUDE.md` forbids opening anything not named. This caps input tokens at roughly the size of
the source file, which for `model/` averages ~200 lines.

### 3.2 Ranged reads for the monsters
Several files are unreadable whole. Never `Read` these; always read a line range:

| File | Lines | How to read |
|---|---|---|
| `store/store.go` | 1,471 | `sed -n '448,520p'` — one interface at a time |
| `model/config.go` | 5,795 | one config section at a time |
| `model/post.go` | 1,640 | `sed -n '1,400p'` then continue |
| `model/utils.go` | 938 | `sed -n '1,300p'` then continue — over the 600-line threshold |
| `model/client4.go` | 8,526 | **never** — it's the Go client, not server code |
| `model/permission.go` | 2,789 | mechanical data; generate, don't translate |

Give the agent the line range in the prompt. `grep -n "type UserStore interface" file` costs
~20 tokens and tells you the range to hand over.

### 3.3 Never read Go `_test.go` files
They are frequently larger than the source (`post_test.go` is 1,644 lines) and encode Go test
harness structure you will not reproduce. You write **Rust** tests against the golden fixtures
instead. This roughly halves the readable surface of the repo.

### 3.4 The golden-fixture parity oracle
This is the highest-leverage trick in the whole plan. Rather than having the agent reason about
whether the Rust JSON matches the Go JSON, make Go *tell* you.

Build it as a **harness plus a registry**, not as a one-shot dump of every type. `model/` holds
198 non-test files; populating each type up front would mean reading all of them before writing
a line of Rust, which is precisely the token blowup this plan exists to avoid.

```go
// reference/dump/main.go — the harness. Write this ONCE, ~40 lines. Zero type knowledge.
var registry = map[string]any{
    "user": model.User{ /* every field, non-zero */ },
    "team": model.Team{ /* ... */ },
    // one line appended per type, in the session that translates that type
}
// for name, v := range registry { → fixtures/<name>.json, indented }
```

Seed the registry with the nine types Phase 1 opens with (see prompt 0.2). After that, **each
translation session appends its own type** and the generator is re-run. The marginal cost is a
few lines in a session that already has the Go struct open — versus a prohibitive up-front cost
if you try to cover all 198 files before starting.

Now every Rust test is a mechanical assertion that costs no reasoning:

```rust
#[test]
fn user_matches_go_serialization() {
    let go = include_str!("../../fixtures/user.json");
    let u: User = serde_json::from_str(go).expect("deserializes Go output");
    let round: serde_json::Value = serde_json::to_value(&u).unwrap();
    assert_eq!(round, serde_json::from_str::<serde_json::Value>(go).unwrap());
}
```

This catches the single most common and most damaging class of migration bug: a `serde(rename)`
that doesn't match Go's `json:"..."` tag, or a `null` vs `omitempty` mismatch. Wire-format drift
between Rust server and existing Mattermost mobile/desktop clients is invisible in code review
and obvious in a fixture diff.

**The one rule that makes or breaks the oracle: every field in the Go instance must carry a
distinctive non-zero value.** Marshal a zero-valued struct and every `omitempty` field vanishes
from the fixture — the round-trip then passes trivially while proving nothing about exactly the
fields most likely to drift. A green test that cannot fail is worse than no test, because it
buys false confidence at a phase gate. Spot-check each new fixture for missing keys before
committing it.

### 3.5 No repo-wide grep
`grep -r` over Mattermost returns tens of thousands of lines. `CLAUDE.md` restricts the agent to
`grep -rl` (names only), `grep -c` (counts), or `grep -n ... | head -20`. When you need to know
where something lives, *you* run the grep in your shell and paste the one path into the prompt.

---

## 4. Checkpoints, review, and resumption

### 4.1 `MIGRATION.md` is the only state
Sessions do not share memory. `MIGRATION.md` is a small, machine-updatable ledger that the agent
reads at the start of every session and appends to at the end. It is the entire resumption
mechanism.

```markdown
# Migration Ledger
Go source pinned at: mattermost@<SHA>
Current phase: 1 — Core Types
Next file: server/public/model/team.go

| Go source | Rust target | Status | Tests | Notes |
|---|---|---|---|---|
| model/utils.go | mm-model/src/utils.rs | DONE | 12 pass | NewId = 26-char base32 of UUIDv4, not raw uuid |
| model/user.go  | mm-model/src/user.rs  | DONE | 18 pass | Props is map<String,String>, never null on wire |
| model/team.go  | mm-model/src/team.rs  | TODO | — | — |
```

Keep it under ~150 lines. When a phase closes, move its rows to `docs/ledger/phase-N.md` and
leave a one-line summary. A ledger that grows to 800 rows becomes the context problem it was
meant to solve.

The **Notes** column is where hard-won knowledge lives — non-obvious semantics the next session
would otherwise rediscover by re-reading Go. `NewId()` being base32-of-UUID rather than a plain
UUID is exactly the kind of fact worth one ledger line and worth nothing as re-derived reasoning.

### 4.2 The checkpoint ritual (end of every session)

1. `cargo check --workspace` — must be clean
2. `cargo test -p <crate>` — must be green
3. `cargo clippy -- -D warnings` — must be clean
4. Agent updates `MIGRATION.md`
5. `git commit -m "migrate(model): user.go -> user.rs"` — **one file per commit**
6. **You** review the diff. One file, one commit — it's a 5-minute read.
7. `/clear` before the next file

One file per commit is what makes review tractable and makes `git revert` a precise instrument
when a translation turns out subtly wrong three phases later.

### 4.3 Phase gates (human review, harder stop)
A phase does not close until:

- **Phase 1:** every fixture round-trips; `cargo test -p mm-model` green
- **Phase 2:** integration tests pass against a real Postgres seeded by the *Go* server's
  migrations (use `testcontainers` or a local `docker compose`)
- **Phase 3:** service functions produce byte-identical results to Go for the same DB state
- **Phase 4:** the existing Mattermost **webapp** logs in and loads through the Rust proxy
- **Phase 5:** two browser clients exchange messages in real time through the Rust hub

### 4.4 Interrupted mid-session
The recovery prompt is always the same shape and costs almost nothing:

> Read `MIGRATION.md`. Run `git status` and `cargo check`. Report which file was in flight and
> whether it compiles. Do not read any Go source yet.

Because the ledger records intent (`Next file:`) and git records completion, the difference
between the two *is* the in-flight work. No archaeology required.

---

## 5. Scope decisions to make before you start

Not everything should be migrated. Decide these now, record them in `CLAUDE.md`, and stop the
agent from wandering into them:

| Area | Recommendation |
|---|---|
| `model/client4.go` (8.5k lines) | **Skip.** It's the Go SDK for external callers. |
| Enterprise / licensed code (`enterprise/`) | **Skip.** Different license; proxy to Go permanently. |
| Plugin system (hashicorp/go-plugin, RPC) | **Skip initially.** Keep the Go plugin host alive behind the proxy. |
| `model/permission.go` (2.8k lines) | **Generate**, don't translate — it's a static table. Write a one-off script. |
| `model/config.go` (5.8k lines) | Translate **lazily**, section by section, only as consuming code needs it. |
| Search (Bleve / Elasticsearch) | Defer past Phase 5. |

The plugin system is the one that will bite. Mattermost plugins are Go binaries speaking net/rpc
over a hashicorp/go-plugin handshake. Reimplementing that host in Rust is a project of its own,
and it is the single strongest argument for keeping a Go process alive indefinitely rather than
targeting full strangulation.

---

## 6. Realistic expectations

`server/public/model/` alone is 299 files and >40,000 lines in its 15 largest files. The full
server is on the order of a million lines including tests. At a sustainable pace of 3–8 files
per session with review, Phase 1 is weeks, not days.

The plan is built so that partial completion is still valuable: at the end of Phase 2 you have a
tested, standalone Rust data-access layer usable for tooling and exports, even if you never
finish Phase 5. Design every phase boundary so that stopping there leaves something shippable.
