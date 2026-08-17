# mattermost-rs

An incremental port of the [Mattermost](https://github.com/mattermost/mattermost) server from Go
to Rust, translated bottom-up and deployed behind a Strangler Fig proxy.

**Wire compatibility with existing Mattermost clients is the hard requirement.** JSON field names,
casing, null-vs-omitted and numeric types must match the Go server's output exactly. Where
correctness and idiomatic Rust conflict on the wire format, the wire format wins.

---

## Status: early, but it runs.

Phase 1 of 5 — the model crate — with one **vertical slice** through phases 2-4 landed to prove
the architecture end to end. `GET /api/v4/users/me` is served from Rust, authenticated against a
session row the Go server wrote, and returns bytes identical to Go's. Every other route is
forwarded to the Go server and comes back unaltered.

The later crates are ported in dependency order; see [Layout](#layout) for what each one is and
which phase it belongs to.

**[`MIGRATION.md`](MIGRATION.md) is the authoritative ledger** — per-file status, test counts,
and the non-obvious semantics each translation turned up. Progress is tracked there and only
there. It is deliberately not summarised here: a README carrying counts is a README that is
quietly wrong most of the time, and every merged file would otherwise drag an unrelated edit
along with it.

Three model files are explicitly **out of scope** and will be proxied to Go or generated rather
than hand-translated: `client4.go` (a REST client, not server code), `permission.go`, and
`config.go`.

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

## Getting started

Requires Rust 1.85+ (edition 2024) and, to regenerate fixtures, Go 1.26+.

```sh
# The Go source is a read-only reference, pinned to a fixed commit and never vendored.
# Fetch the pinned SHA directly — a plain `clone --depth 1` only gets the current tip,
# which will not contain this commit once upstream moves on.
git init reference/mattermost
git -C reference/mattermost remote add origin https://github.com/mattermost/mattermost.git
git -C reference/mattermost fetch --depth 1 origin 9dfbaeca99f4096388fd1c048a9e6d1d0a86743e
git -C reference/mattermost checkout FETCH_HEAD

cargo test -p mm-model
```

### Running the two servers

The Strangler Fig needs a Go server to forward to and a Postgres both servers share. Both are in
`docker-compose.yml`; the Rust server runs on the host so it can be rebuilt without a container
cycle.

```sh
docker compose up -d          # postgres :5432, the Go server :8065
export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
cargo run -p mm-api           # :8066 — serves what is migrated, forwards the rest
```

The Go server owns the schema and runs the migrations. **Never point a migration tool at this
database from the Rust side** — the two would race, and Go's migrations are the reference.

Every response carries `x-mmrs-served-by: rust` or `: go`, so you can see the cutover:

```sh
curl -si localhost:8066/api/v4/system/ping | grep -i served-by     # -> go
```

The cross-server parity test is the oracle for anything migrated. It needs the stack up and a
user to log in as, and it is skipped unless explicitly enabled, so `cargo test` stays green on a
machine with no Docker:

```sh
MM_PARITY_STACK=1 cargo test -p mm-api --test parity_users_me
```

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
cargo fmt && cargo check --workspace && cargo clippy -- -D warnings && cargo test -p mm-model
```

Plus `gofmt -l reference/dump/` and `go vet ./...` if you touched the generator.

---

## Layout

```
crates/
  mm-model/      phase 1  wire types; zero internal dependencies
  mm-store/      phase 2  persistence (sqlx, Postgres); depends on mm-model
  mm-app/        phase 3  business logic; depends on mm-store; knows nothing about HTTP
  mm-api/        phase 4  REST + the Strangler Fig proxy; depends on mm-app
  mm-ws/         phase 5  WebSocket hub; separate binary so fan-out scales independently
fixtures/        generated parity fixtures — never edit by hand
reference/
  mattermost/    pinned Go source, read-only, gitignored
  dump/          the fixture generator and behavioural oracles
docs/
  MIGRATION_STRATEGY.md   the plan: phases, sequencing, proxy cutover
  TECH_DEBT.md            what we owe — deferred work and known divergences
  PROMPTS.md              per-phase execution prompts
MIGRATION.md     THE LEDGER: per-file status and hard-won semantics
CLAUDE.md        agent context — read this before contributing
```

The dependency direction is strict and enforced by review:
`mm-model ← mm-store ← mm-app ← {mm-api, mm-ws}`.

### Where state lives

Sessions are short and context does not carry across them. Three files carry it instead:

- **`MIGRATION.md`** — what is translated, and every non-obvious Go semantic discovered while
  doing it. Most entries cost real time to find and are not recoverable by re-reading the source
  casually.
- **`docs/TECH_DEBT.md`** — numbered entries, each `OPEN` (owed), `ACCEPTED` (a deliberate
  permanent divergence) or `CLOSED` (paid off). Anything skipped, approximated, or
  found-but-not-fixed gets an entry here.
- **`CLAUDE.md`** — the rules a contributor (human or agent) is expected to follow.

---

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first. The rules that matter most:

- **Never edit `fixtures/` by hand.** Extend the generator and re-run it.
- **Every translated file ships with tests in the same file** — serialization parity against a
  generated fixture, and a behavioural test per branch for anything with logic.
- **Never claim parity you did not verify with a test.** If you guessed at a JSON tag, say so.
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
