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

## Token rules — these are hard constraints

1. **Read only what the prompt names.** Never open a file that was not explicitly listed. If you
   believe you need another file, **stop and ask** — do not read it speculatively.
2. **Never read `reference/mattermost/server/public/model/client4.go`** (8,526 lines, Go REST
   client, out of scope) or any `*_test.go` file in the Go tree.
3. **Use ranged reads for large files.** For anything over ~600 lines, read a line range only
   (`sed -n '448,520p' <file>`). `store/store.go` is 1,471 lines — one interface at a time.
4. **No repo-wide grep.** Allowed: `grep -rl` (filenames only), `grep -c` (counts),
   `grep -n ... | head -20`. Never emit an unbounded `grep -r` or `find` over `reference/`.
5. **One Go file → one Rust file per session.** Do not "while I'm here" adjacent files.
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

If a fixture doesn't exist for a type you're translating, say so and write the test against
values transcribed from the Go source — do not invent a fixture file.

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
semantics discovered) and stop. Do not start the next file. Do not commit unless asked.

## Reporting

At the end of a session, report in this order and keep it short:
1. Files created/modified (paths only)
2. Test results (`N passed`)
3. **Parity risks** — anything you were unsure matched Go, stated plainly
4. The single next file to migrate

Never claim parity you did not verify with a test. If you guessed at a JSON tag, say you guessed.
