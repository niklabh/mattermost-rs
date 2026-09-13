#!/usr/bin/env python3
"""Transcribe Go's `htmlEntities` map (shared/markdown/html_entities.go) into
crates/mm-markdown/src/html_entities.rs as a sorted static slice.

Run from the repository root after regenerating fixtures; it refuses to write if the
table disagrees with what `markdown.CharacterReference` answered in
fixtures/behaviour_markdown.json, so the Rust table can only ever say what Go says.
"""
import json
import re
import sys

SRC = 'reference/mattermost/server/public/shared/markdown/html_entities.go'
FIXTURE = 'fixtures/behaviour_markdown.json'
OUT = 'crates/mm-markdown/src/html_entities.rs'

src = open(SRC, encoding='utf-8').read()
entries = re.findall(r'^\s*"([^"]+)":\s*"((?:[^"\\]|\\.)*)",\s*$', src, re.M)
if len(entries) != 2125:
    sys.exit(f'expected 2125 entries in {SRC}, found {len(entries)}')


def decode(v):
    # The Go table uses only \uXXXX and \UXXXXXXXX escapes.
    if re.search(r'\\(?![uU][0-9A-Fa-f])', v):
        sys.exit(f'unexpected escape in {v!r}')
    return re.sub(r'\\u([0-9A-Fa-f]{4})|\\U([0-9A-Fa-f]{8})',
                  lambda m: chr(int(m.group(1) or m.group(2), 16)), v)


pairs = sorted((k, decode(v)) for k, v in entries)
if len({k for k, _ in pairs}) != len(pairs):
    sys.exit('duplicate entity names')
oracle = json.load(open(FIXTURE, encoding='utf-8'))['entities']
if oracle != dict(pairs):
    bad = [k for k, v in pairs if oracle.get(k) != v]
    sys.exit(f'table disagrees with CharacterReference on {bad[:5]}')


def rs_str(s):
    return '"' + ''.join(
        ('\\u{%X}' % ord(c)) if (ord(c) < 0x20 or ord(c) > 0x7e or c in '"\\') else c
        for c in s) + '"'


out = [
    '//! Port of `html_entities.go`: the 2,125 named character references `CharacterReference`\n',
    '//! (inlines.go:365) resolves. Transcribed from the Go table by `scripts/gen-html-entities.py`,\n',
    '//! which refuses to write a table that disagrees with `markdown.CharacterReference`; the\n',
    '//! `go_parity` suite then asserts every entry again against the fixture.\n',
    '//!\n',
    '//! Sorted by name in byte order, so `binary_search_by_key` applies. Lookups are case-sensitive,\n',
    '//! as Go map lookups are: `AMP` and `amp` are both present, `Amp` is not.\n',
    '\n',
    "/// `(name, replacement)` for every entry of Go's `htmlEntities` map, sorted by name.\n",
    '#[rustfmt::skip]\n',
    'pub static HTML_ENTITIES: &[(&str, &str)] = &[\n',
]
out += ['    (%s, %s),\n' % (rs_str(k), rs_str(v)) for k, v in pairs]
out += [
    '];\n',
    '\n',
    '/// Port of the `htmlEntities[ref]` lookup: the replacement for a named reference (without\n',
    '/// the `&` and `;`), or `None`.\n',
    "pub(crate) fn lookup(name: &str) -> Option<&'static str> {\n",
    '    HTML_ENTITIES\n',
    '        .binary_search_by_key(&name, |(k, _)| k)\n',
    '        .ok()\n',
    '        .map(|i| HTML_ENTITIES[i].1)\n',
    '}\n',
]
open(OUT, 'w', encoding='utf-8').write(''.join(out))
print(f'wrote {OUT} ({len(pairs)} entities)')
