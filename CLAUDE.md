# CLAUDE.md — mattermost-rs

## Project goal

Incremental migration of the Mattermost core server from Go to Rust. The Go source is a
**read-only reference** at `reference/mattermost/` pinned to a fixed commit. We deploy behind a
Strangler Fig proxy: `mm-api` fronts traffic and forwards any not-yet-migrated route to the
still-running Go server. Both processes share one Postgres database and one `Sessions` table.

**Wire compatibility with existing Mattermost clients is the hard requirement.** JSON field
names, casing, null-vs-omitted, and numeric types must match the Go output exactly. When
correctness and idiomatic Rust conflict on the wire format, wire format wins.

## Tech stack

| Concern | Crate | Notes |
|---|---|---|
| Runtime | `tokio` | full features; no `block_on` in request paths |
| HTTP | `axum` | `Router`, extractors, `tower` middleware |
| DB | `sqlx` | Postgres only, `runtime-tokio-rustls`, compile-time checked macros |
| Serde | `serde`, `serde_json` | derive; explicit `rename` on every field |
| Errors | `thiserror` | libraries; `anyhow` permitted **only** in `main.rs`/tests |
| Logging | `tracing` | `#[instrument]` on public async fns; never `println!` |
| Time | `chrono` | Go stores epoch **milliseconds** as `i64` — keep `i64` on the wire |
| IDs | `String` | Mattermost IDs are 26-char base32, NOT UUIDs. Do not use the `uuid` type. |

## The unit of work is a route, not a file

**Revised 2026-08-20 on evidence.** The ledger records 141 files DONE and **6 of 764 api4 routes
served**. Thirty-five of `mm-model`'s 71 modules — about 20,000 lines — are not reachable from any
route this server answers; the rest are *imported* by something served, which is an upper bound on
being exercised, not a claim of it. `MIGRATION_STRATEGY.md:69` predicted precisely this ("months
of unverifiable work") and the vertical slice was built early to prevent it. Breadth-first model
porting then resumed anyway, and MIGRATION.md's own guidance now says of the remaining small files
that "its yield has been falling" and that none "has produced a new tech-debt entry from its wire
format alone."

So:

1. **Pick a route. Port what that route needs, when it needs it.** A session's natural scope is
   one handler plus the app and store functions behind it — three files across three crates is
   normal and correct, not scope creep.
2. **Do not port a model file speculatively.** If you port one, name the route it unblocks.
3. **Legitimate exception:** a route blocked on a decision (i18n, licensing, a missing crate).
   Say which route is blocked and on what, then pick a different route rather than falling back to
   breadth.
4. **Prefer routes a real client actually calls.** A route no client hits is worth less than its
   test suite costs.

## Reading the Go tree

Context is no longer scarce; **round trips are.** Prefer one search that answers the question to
five that circle it. What still holds:

- **Never read `reference/mattermost/server/public/model/client4.go`** (8,526 lines, Go REST
  client, out of scope) or any `*_test.go` file in the Go tree. Never read `target/`,
  `Cargo.lock`, `node_modules/`, or `reference/mattermost/webapp/`. These are scope and safety,
  not budget — they hold unconditionally.
- **Read whatever you need to avoid guessing.** Guessing at a field name, a `json:` tag or a
  validation branch is the failure this project exists to prevent. Reading a neighbouring file to
  get a type right needs no justification.
- **Use ranged reads for files over ~600 lines** (`sed -n '448,520p'`). `store/store.go` is 1,471
  lines — one interface at a time. This is about signal, not tokens: the whole file drowns the
  part you need.
- **Don't wander.** Reading "for background" is still waste. Read for a fact you can name.

## Testing — required, not optional

Every translated file ships with `#[cfg(test)]` tests in the same file. Minimum bar:

- **Serialization parity.** Deserialize the golden fixture at `fixtures/<type>.json` (produced by
  Go), re-serialize, and assert the `serde_json::Value` graphs are equal. Required for every type
  with `json:` tags.
- **Behavioural parity.** Any Go method with logic (validation, `IsValid()`, `PreSave()`,
  sanitization, slug/username rules) gets a test per branch, including the error branches.
- **Edge cases the Go code encodes:** empty strings vs `None`, zero timestamps, `omitempty`
  fields, and any field Go explicitly sanitizes out.

### Fixtures — the parity oracle

`fixtures/*.json` is **generated** by the Go program at `reference/dump/`. Never write or edit a
fixture by hand — a hand-written fixture asserts what you already believe and cannot detect drift.

- **Every field must carry a distinctive non-zero value.** A fixture marshalled from a
  zero-valued Go struct silently omits every `omitempty` field, so the round-trip test passes
  while proving nothing about precisely the fields most likely to drift. Before trusting a
  fixture, check it for missing or zero-valued keys; if any are missing, say so and treat the
  parity test as provisional rather than evidence.
- **Extending the oracle is part of translating a type.** If the file you are translating
  declares a type with `json:` tags and no fixture exists, append it to the registry in
  `reference/dump/main.go` — one line, fully populated.
- **Run the generator** (`cd reference/dump && go run .`) under `TZ=Asia/Kolkata`, then show the
  fixture diff in your report. Output is deterministic, so a clean run touches only the new files;
  anything else in `git status` is a signal.
- **Changing an `overrides` entry rewrites a committed fixture. Call that out separately** — it is
  the one edit that can move a value the Rust tests already assert against. Say which key changed
  and why.
- **Behavioural oracles get the same treatment.** Anything with branching logic gets a corpus in
  `reference/dump/behaviour*.go` and a `go_parity` test module asserting against it. Reading a Go
  branch and reasoning about it is what produces confident, wrong translations.
- If a fixture genuinely cannot be generated, say so and write the test against values
  transcribed from the Go source — do not invent a fixture file. Record in `MIGRATION.md` that
  the test is provisional until the fixture lands.

### Mutation testing — promoted from folklore, 2026-08-20

**A suite that passes on its first run is not evidence.** This is the single highest-yield practice
in the project and it lived only in MIGRATION.md prose until now. Every session that ships logic
runs `scripts/mutate.sh` against the finished work.

- Mutate the decisions a reader could plausibly get wrong: predicate direction, check *order*,
  which column, which constant, off-by-one, a dropped `COALESCE`.
- **A mutation that survives is a finding about the tests, not a shrug.** In the
  `getChannelUnread` session three survivors each exposed a fixture where the right answer and the
  wrong answer coincided — a reader who had never viewed the channel made a subtraction
  indistinguishable from a copy; two counters happened to hold equal values; nothing reachable
  through the REST API produces the NULL a `COALESCE` exists for. Fix the fixture, then re-run.
- **Run two no-op controls** (rename a binding, reorder two independent SELECT columns). If a
  control fails, the harness is noisy and its verdicts mean nothing.
- Report the tally: *N run, N caught, N controls survived*.

## Rust practice

- **No `unwrap()` / `expect()` / `panic!` in library code.** Tests may `unwrap()` freely. In
  `main.rs`, `expect("<why this is impossible>")` is acceptable at startup only.
- **No `.clone()` to silence the borrow checker.** Prefer `&str`/`&[T]`, `Cow<'_, str>`, or
  restructuring. If a clone is genuinely required, add a comment saying why.
- **Errors:** one `thiserror` enum per module, `#[from]` for conversion, `?` for propagation.
  Never stringify an error you could type. Never swallow one.
- **No `async_trait` where native async-in-trait works.** Store traits use native RPITIT.
- **Go pointer fields (`*string`, `*int64`) map to `Option<T>`** with
  `#[serde(skip_serializing_if = "Option::is_none")]` only when Go had `omitempty`. Go's
  non-pointer + `omitempty` on a string means "omit when empty" — that is *not* `Option`; model
  it as `String` plus a skip predicate, or the wire format drifts.
- **Go `map[string]any` → `serde_json::Value`**, not a typed struct, unless the Go code proves
  the shape.
- Public items get doc comments citing the Go origin: `/// Port of `model.User` (user.go:41).`
- Formatting/lint: `cargo fmt` and `cargo clippy -- -D warnings` must both be clean.

## Write the finding once

**Revised 2026-08-20.** The ledger is 8,839 lines of markdown against 74,000 lines of Rust, and
the same discovery routinely appears three times: in a doc comment, in a MIGRATION.md row, and in
a TECH_DEBT entry. That is the largest non-test cost per session and most of it is duplication.

- **A finding lives in the code**, in the doc comment on the thing it constrains. That is where
  someone changing the code will actually see it.
- **A MIGRATION.md row is at most two sentences** plus a pointer to the code. Status, test count,
  and the one thing a reader would otherwise get wrong.
- **`docs/TECH_DEBT.md` is a backlog, not a diary.** It holds work that is *owed*: OPEN entries
  only. Today 71 of its 151 entries are ACCEPTED — permanent decisions and divergences that will
  never be acted on, inflating a to-do list by half. New permanent findings go in the code and in
  MIGRATION.md; open an entry only when something is genuinely deferred.
- Long-form narrative is welcome in a session **report**, which is read once and costs nothing to
  store.

## Definition of done for a session

```
cargo fmt && cargo check --workspace && cargo clippy --all-targets -- -D warnings && cargo test --workspace
```

- **Keep the suite fast. A test that waits is a bug.** The full workspace run is ~50s. It was 145s
  until six tests were found sitting on sqlx's 30-second default `acquire_timeout` against a
  deliberately unreachable database. Cap timeouts in test fixtures; if a suite crosses ~90s,
  find out why before adding to it.
- **Tooling belongs in `scripts/`, committed.** A harness rebuilt from scratch each session is
  paid for every session and improved in none.
- Update `MIGRATION.md` (see *Write the finding once*). Open a `docs/TECH_DEBT.md` entry only for
  work genuinely deferred.
- Then stop. Do not start the next route. Do not commit unless asked — but **say plainly how much
  uncommitted work is in the tree**, since it survives no accident.

## Reporting

At the end of a session, report in this order and keep it short:
1. Files created/modified (paths only)
2. Test results (`N passed`) and the mutation tally (`N run, N caught, N controls survived`)
3. **Parity risks** — anything you were unsure matched Go, stated plainly
4. The next route to migrate, and what it needs behind it

Never claim parity you did not verify with a test. If you guessed at a JSON tag, say you guessed.
