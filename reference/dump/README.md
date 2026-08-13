# `dump` — the parity oracle

Generates `fixtures/*.json`: the golden JSON that every Rust serialization-parity test asserts
against. Go decides what the wire format is; the Rust tests only have to agree with it.

## Run it

```bash
cd reference/dump
go mod tidy      # first time only
go run .         # writes ../../fixtures/*.json
```

Exit code is non-zero if any fixture came out incomplete. Commit `fixtures/` afterwards — those
files are the contract.

## Adding a type

One line in the `registry` map in `main.go`:

```go
"file_info": &model.FileInfo{},
```

Leave it zero-valued. Reflection fills every exported field; you do not need to know, or look up,
what fields the type has. Then re-run and commit the new fixture.

## Three rules this program is built on

**Every field must end up non-zero.** A field left at its zero value is dropped from the JSON by
`omitempty`, and the Rust round-trip test for that field then passes while proving nothing — a
green test that cannot fail, on exactly the fields most likely to drift. This is why population is
reflective rather than hand-written: enumerating ~30 fields per type across 198 types by hand
guarantees some get missed. The program verifies its own output and fails the run if a top-level
key declared by the struct is absent from the JSON.

**Output must be deterministic.** Every value derives from an FNV hash of the field's path, so
re-running produces byte-identical files. Fixtures are committed and the generator is re-run once
per migrated type; a generator seeded from `rand` or `time.Now()` would churn every fixture on
every run and make the diffs unreadable. Do not introduce either.

**The model package resolves from the pinned clone.** The `replace` directive in `go.mod` points
at `../mattermost/server/public`, so fixtures are always generated from the SHA recorded in
`MIGRATION.md`. Without it a proxy fetch would silently pull some other version and the fixtures
would stop describing the code being translated.

## Tuning a value

The generic filler produces values that serialize correctly but are meaningless as domain data
(`"type-8f21ac"` for a channel type). Where a fixture should also be usable for exercising
`IsValid()` on the Rust side, pin the real value in the `overrides` map, keyed by the dotted field
path:

```go
"channel.type": "O",
```

Anything convertible to the field's type works, maps included. Pinning `""` is allowed and is
correct for fields whose valid domain value is empty, but only on fields without `omitempty` —
the built-in key check fails the run otherwise.

## Known limits

- The completeness check covers **top-level** keys only. A zero value nested inside a struct is
  not caught by it; the populator emits a `warning:` line for anything it could not reach
  (reference cycles, depth cap, non-empty interfaces). There are currently no warnings.
- Values are type-correct and shape-correct, not semantically coherent across fields. A fixture
  is a serialization oracle, not a valid domain object, except where `overrides` makes it one.
