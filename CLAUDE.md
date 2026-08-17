# CLAUDE.md — mattermost-rs

## Project goal

Incremental migration of the Mattermost core server from Go to Rust. The Go source is a
**read-only reference** at `reference/mattermost/` pinned to a fixed commit. We translate it
bottom-up (leaf nodes first) and deploy behind a Strangler Fig proxy: `mm-api` fronts traffic and
forwards any not-yet-migrated route to the still-running Go server. Both processes share one
Postgres database and one `Sessions` table.

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

## Token rules — strong defaults, not hard constraints

**Quality outranks the token budget.** The goal is the best-quality, best-architected Rust port of
Mattermost; these rules exist to stop context blowup, which is a means to that end, not the end
itself. When following a rule would force you to *guess* — at a field name, a `json:` tag, a
validation branch, a semantic — the rule has stopped doing its job. Read the extra file instead,
and say in your report which rule you broke and what it bought.

What that does **not** license: speculative wandering, reading a file "for background", or
re-reading what you already have. Break a rule for a specific fact you need and can name.

Rules 2 and 7 below are scope and safety decisions, not budget ones — those hold unconditionally.

1. **Prefer to read only what the prompt names.** Don't open unlisted files speculatively. If you
   need another file to avoid guessing, read it and say why — asking first is welcome but not
   required when the need is clear-cut.
2. **Never read `reference/mattermost/server/public/model/client4.go`** (8,526 lines, Go REST
   client, out of scope) or any `*_test.go` file in the Go tree.
3. **Use ranged reads for large files.** For anything over ~600 lines, read a line range only
   (`sed -n '448,520p' <file>`). `store/store.go` is 1,471 lines — one interface at a time.
4. **No repo-wide grep.** Allowed: `grep -rl` (filenames only), `grep -c` (counts),
   `grep -n ... | head -20`. Never emit an unbounded `grep -r` or `find` over `reference/`.
5. **One Go file → one Rust file per session.** Do not "while I'm here" adjacent files. Reading a
   neighbouring file to get a type right is fine; *translating* it in the same session is not.
6. **Don't re-read files you already wrote this session.** Trust the edits.
7. **Never read `target/`, `Cargo.lock`, `node_modules/`, or `reference/mattermost/webapp/`.**

## Testing — required, not optional

Every translated file ships with `#[cfg(test)]` tests in the same file. Minimum bar:

- **Serialization parity.** Deserialize the golden fixture at `fixtures/<type>.json` (produced by
  Go), re-serialize, and assert the `serde_json::Value` graphs are equal. This is the primary
  defence against wire drift and is required for every type with `json:` tags.
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
- **Run the generator** (`cd reference/dump && go run .`), then show the fixture diff in your
  report. Output is deterministic, so a clean run touches only the new files; anything else in
  `git status` is a signal. Decided 2026-08-14 — the earlier rule was "add the line, report that
  it needs re-running", which bought hand-transcribed test values, i.e. exactly the guessing the
  oracle exists to eliminate.
- **Changing an `overrides` entry rewrites a committed fixture. Call that out separately.** It
  is the one edit that can move a value the Rust tests already assert against, so it does not
  belong buried in a list of new files. It is legitimate — a fixture that fails its own type's
  `IsValid` is worth pinning to valid values — but say which key changed and why.
- **Behavioural oracles get the same treatment.** Anything with branching logic gets a corpus in
  `reference/dump/behaviour*.go` and a `go_parity` test module asserting against it. Reading a Go
  branch and reasoning about it is what produces confident, wrong translations.
- If a fixture genuinely cannot be generated, say so and write the test against values
  transcribed from the Go source — do not invent a fixture file. Record in `MIGRATION.md` that
  the test is provisional until the fixture lands.

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

## Definition of done for a session

```
cargo fmt && cargo check --workspace && cargo clippy -- -D warnings && cargo test -p <crate>
```

Then update `MIGRATION.md` (status, test count, and a one-line **Note** for any non-obvious
semantics discovered). Anything you skipped, approximated, or found-but-did-not-fix gets an entry
in `docs/TECH_DEBT.md` — that register is how deferred work survives a `/clear`. Then stop. Do not start the next file. Do not commit unless asked.

## Reporting

At the end of a session, report in this order and keep it short:
1. Files created/modified (paths only)
2. Test results (`N passed`)
3. **Parity risks** — anything you were unsure matched Go, stated plainly
4. The single next file to migrate

Never claim parity you did not verify with a test. If you guessed at a JSON tag, say you guessed.
