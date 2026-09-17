#!/usr/bin/env python3
"""Generate mm-plugin's gob wire types from the Go plugin IDL.

The IDL, `fixtures/plugin/idl.json`, is written by `reference/dump/plugingen` from reflection over
the pinned Go tree (docs/PLUGIN_PLAN.md, D5). This script turns it into committed Rust:

  crates/mm-plugin/src/wire/<package>.rs   one module per Go package: every struct, named type
                                           and wire struct (`Z_*Args`/`Z_*Returns`) the plugin RPC
                                           can send, as `#[derive(Gob)]` types
  crates/mm-plugin/src/wire/mod.rs         the modules, the names `init()` registers, and
                                           `for_each_wire_struct!` and `for_each_registered!`

The Rust types are the **gob** form of the Go types, not `mm-model`'s JSON form: every field gob
sends is here (including the `json:"-"` ones), named exactly as Go names it, typed by its Go kind.
Converting them to and from `mm-model` is the host's business, where a lossy mapping is visible.

Go → Rust:
  int, int64 … uint8      i64, i64 … u8 (int and uint are 64-bit on every Mattermost build)
  string, bool, float64   String, bool, f64
  []byte (any uint8 elem) Vec<u8>
  []T, map[K]V            Vec<T>, HashMap<K, V>   (a pointer element is flattened: gob cannot
                                                  send a nil element, and decodes to non-nil)
  *T                      Option<Box<T>> for a struct, Option<T> otherwise
  interface (any, error)  Option<gobwire::Interface>
  time.Time               gobwire::GoTime
  GobEncoder / Binary…    gobwire::GobBytes / gobwire::BinaryBytes
  named non-struct type   `pub type Name = <underlying>;`

Usage:
    scripts/plugingen.py           regenerate
    scripts/plugingen.py --check   fail if the committed output differs from a fresh generation
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
IDL = ROOT / "fixtures/plugin/idl.json"
OUT = ROOT / "crates/mm-plugin/src/wire"

# Go package → Rust module. Every package the IDL reaches must be listed: an unlisted one is a new
# dependency of the plugin API and deserves a deliberate name.
MODULES = {
    "github.com/mattermost/mattermost/server/public/model": "model",
    "github.com/mattermost/mattermost/server/public/plugin": "plugin",
    "github.com/mattermost/gosaml2": "saml2",
    "github.com/mattermost/gosaml2/types": "saml2_types",
    "github.com/mattermost/logr/v2": "logr",
    "github.com/lib/pq": "pq",
    "github.com/lib/pq/pqerror": "pqerror",
    "crypto/tls": "tls",
    "crypto/x509": "x509",
    "crypto/x509/pkix": "pkix",
    "encoding/asn1": "asn1",
    "encoding/json": "json",
    "encoding/xml": "xml",
    "math/big": "big",
    "mime/multipart": "multipart",
    "net": "net",
    "net/http": "http",
    "net/textproto": "textproto",
    "net/url": "url",
    "time": "time",
    "io": "io",
}

BASIC = {
    "bool": "bool",
    "int": "i64",
    "int8": "i8",
    "int16": "i16",
    "int32": "i32",
    "int64": "i64",
    "uint": "u64",
    "uint8": "u8",
    "uint16": "u16",
    "uint32": "u32",
    "uint64": "u64",
    "uintptr": "u64",
    "float32": "f32",
    "float64": "f64",
    "complex64": "::gobwire::Complex",
    "complex128": "::gobwire::Complex",
    "string": "::std::string::String",
}

RUST_KEYWORDS = {
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where",
    "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "try",
    "typeof", "unsized", "virtual", "yield", "gen",
}
NOT_RAW = {"self", "Self", "super", "crate"}


class GenError(Exception):
    pass


def snake(name: str) -> str:
    """Go identifier → snake_case: `CreateAt` → `create_at`, `IPAddress` → `ip_address`."""
    s = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    s = s.lower()
    if s in NOT_RAW:
        return s + "_"
    if s in RUST_KEYWORDS:
        return "r#" + s
    return s


def screaming(name: str) -> str:
    return snake(name).removeprefix("r#").rstrip("_").upper()


class Generator:
    def __init__(self, idl):
        self.idl = idl
        self.types = idl["types"]
        self.rust_names = {}  # type id → Rust item name, for named types
        self.modules = {}  # module → list of type ids
        self.assign_names()

    def module_of(self, tid):
        pkg = self.types[tid].get("package", "")
        if pkg not in MODULES:
            raise GenError(f"{tid}: Go package {pkg!r} has no Rust module in MODULES")
        return MODULES[pkg]

    def assign_names(self):
        taken = {}
        for tid in sorted(self.types):
            t = self.types[tid]
            if not t.get("name") or not t.get("package"):
                continue
            if t["kind"] == "interface" or tid == "time.Time":
                continue
            go = t["name"]
            base, _, args = go.partition("[")
            rust = base[0].upper() + base[1:]
            if args:
                # Name a generic instantiation after its type arguments' own names.
                for arg in re.findall(r"[\w/.-]+\.(\w+)", args):
                    rust += arg[0].upper() + arg[1:]
            module = self.module_of(tid)
            key = (module, rust)
            if key in taken:
                raise GenError(f"{tid} and {taken[key]} both map to {module}::{rust}")
            taken[key] = tid
            self.rust_names[tid] = rust
            self.modules.setdefault(module, []).append(tid)

    def path(self, tid, here):
        """The Rust path of a named type, relative to module `here`."""
        module = self.module_of(tid)
        name = self.rust_names[tid]
        return name if module == here else f"super::{module}::{name}"

    def rust_type(self, tid, here, flatten_pointer=False):
        t = self.types[tid]
        kind = t["kind"]
        if tid == "time.Time":
            return "::gobwire::GoTime"
        if kind == "interface":
            return "::std::option::Option<::gobwire::Interface>"
        if tid in self.rust_names:
            return self.path(tid, here)
        if kind in BASIC:
            return BASIC[kind]
        if kind == "pointer":
            elem = self.types[t["elem"]]
            inner = self.rust_type(t["elem"], here)
            if flatten_pointer:
                return inner
            if elem["kind"] == "interface":
                raise GenError(f"{tid}: a pointer to an interface cannot be sent by gob")
            if elem["kind"] == "pointer":
                raise GenError(f"{tid}: a pointer to a pointer has no mapping")
            if elem["kind"] == "struct":
                return f"::std::option::Option<::std::boxed::Box<{inner}>>"
            return f"::std::option::Option<{inner}>"
        if kind == "slice":
            elem = self.types[t["elem"]]
            if elem["kind"] == "uint8":
                return "::std::vec::Vec<u8>"
            return f"::std::vec::Vec<{self.rust_type(t['elem'], here, flatten_pointer=True)}>"
        if kind == "array":
            return f"[{self.rust_type(t['elem'], here, flatten_pointer=True)}; {t['len']}]"
        if kind == "map":
            key = self.rust_type(t["key"], here)
            val = self.rust_type(t["elem"], here, flatten_pointer=True)
            return f"::std::collections::HashMap<{key}, {val}>"
        if kind == "gob":
            return "::gobwire::GobBytes"
        if kind == "binary":
            return "::gobwire::BinaryBytes"
        raise GenError(f"{tid}: unmapped kind {kind}")

    def underlying(self, tid, here):
        """The Rust type a named non-struct type aliases."""
        t = dict(self.types[tid])
        kind = t["kind"]
        if kind in BASIC:
            return BASIC[kind]
        if kind == "gob":
            return "::gobwire::GobBytes"
        if kind == "binary":
            return "::gobwire::BinaryBytes"
        # A named slice, map or pointer: the same mapping as the unnamed form.
        saved = self.rust_names.pop(tid)
        try:
            return self.rust_type(tid, here)
        finally:
            self.rust_names[tid] = saved

    def item(self, tid, module):
        t = self.types[tid]
        name = self.rust_names[tid]
        go = f"{t['package']}.{t['name']}"
        if t["kind"] != "struct":
            return (
                f"/// Go `{go}` ({t['kind']}).\n"
                f"pub type {name} = {self.underlying(tid, module)};\n"
            )
        lines = [f"/// Go `{go}`, as gob sends it."]
        if name.startswith("Z_"):
            lines.append("#[allow(non_camel_case_types)]")
        lines.append("#[derive(::gobwire::Gob, Debug, Clone, Default, PartialEq)]")
        lines.append(f'#[gob(name = "{t["name"]}")]')
        lines.append(f"pub struct {name} {{")
        seen = set()
        for f in t.get("fields", []):
            field = snake(f["name"])
            if field in seen:
                raise GenError(f"{tid}: two fields map to {field}")
            seen.add(field)
            rust = self.rust_type(f["type"], module)
            if f.get("embedded"):
                lines.append("    /// Embedded: gob sends it as one field named after its type.")
            lines.append(f'    #[gob(name = "{f["name"]}")]')
            lines.append(f"    pub {field}: {rust},")
        lines.append("}")
        return "\n".join(lines) + "\n"

    def module_file(self, module):
        pkgs = sorted(p for p, m in MODULES.items() if m == module)
        out = [
            "// @generated by scripts/plugingen.py from fixtures/plugin/idl.json. Do not edit.",
            "",
            f"//! Gob wire types of Go package `{pkgs[0]}`.",
            "",
            "#![allow(clippy::doc_markdown)]",
            "",
        ]
        items = sorted(self.modules[module], key=lambda tid: self.rust_names[tid])
        for tid in items:
            out.append(self.item(tid, module))
        return "\n".join(out)

    def mod_file(self):
        out = [
            "// @generated by scripts/plugingen.py from fixtures/plugin/idl.json. Do not edit.",
            "",
            "//! The gob form of every value Mattermost's plugin RPC sends, generated from the Go",
            "//! tree (docs/PLUGIN_PLAN.md, D5).",
            "//!",
            f"//! Generated from Go {self.idl['go_version']}: {len(self.idl['wire'])} wire structs, "
            f"{sum(len(v) for v in self.modules.values())} named types.",
            "",
        ]
        for module in sorted(self.modules):
            out.append(f"pub mod {module};")
        out.append("")
        out.append("/// The names client_rpc.go's `init()` registers with gob, for interface values.")
        out.append("pub mod registered {")
        consts = []
        for r in self.idl["registered"]:
            const = self.registered_const(r)
            consts.append(const)
            out.append(f"    /// Go `{r['type']}`.")
            out.append(f'    pub const {const}: &str = "{r["name"]}";')
        out.append("    /// Every name above.")
        out.append(f"    pub const ALL: &[&str] = &[{', '.join(consts)}];")
        out.append("}")
        out.append("")
        out.append("/// Invoke `$m!` with every registered interface type as `(registered::NAME, Type),`: the")
        out.append("/// type a value sent under that name decodes into.")
        out.append("#[doc(hidden)]")
        out.append("#[macro_export]")
        out.append("macro_rules! for_each_registered {")
        out.append("    ($m:ident) => {")
        out.append("        $m! {")
        for r in self.idl["registered"]:
            tid = r["type"]
            if self.types[tid]["kind"] == "pointer":
                tid = self.types[tid]["elem"]  # gob flattens the pointer
            rust = self.rust_type(tid, "").replace("super::", "$crate::wire::")
            out.append(f"            ($crate::wire::registered::{self.registered_const(r)}, {rust}),")
        out.append("        }")
        out.append("    };")
        out.append("}")
        out.append("")
        out.append("/// Invoke `$m!` with every wire struct as `(\"Z_Name\", path::Z_Name),`.")
        out.append("#[doc(hidden)]")
        out.append("#[macro_export]")
        out.append("macro_rules! for_each_wire_struct {")
        out.append("    ($m:ident) => {")
        out.append("        $m! {")
        for tid in self.idl["wire"]:
            name = self.rust_names[tid]
            out.append(f'            ("{name}", $crate::wire::plugin::{name}),')
        out.append("        }")
        out.append("    };")
        out.append("}")
        return "\n".join(out) + "\n"

    def registered_const(self, r):
        t = r["type"]
        tt = self.types[t]
        if tt["kind"] == "pointer":
            name = self.rust_names[tt["elem"]]
            if name == "Error":
                # `pq.Error`: name the package, not just "ERROR".
                return f"{self.module_of(tt['elem']).upper()}_ERROR"
            return screaming(name)
        if t in self.rust_names:
            return screaming(self.rust_names[t])
        if tt["kind"] == "slice":
            elem = self.types[tt["elem"]]
            if elem["kind"] == "pointer":
                return screaming(self.rust_names[elem["elem"]]) + "_PTR_SLICE"
            if tt["elem"] in self.rust_names:
                return screaming(self.rust_names[tt["elem"]]) + "_SLICE"
            return "ANY_SLICE"
        if tt["kind"] == "map":
            return "STRING_ANY_MAP"
        raise GenError(f"no constant name for registered {t}")

    def files(self):
        out = {OUT / "mod.rs": self.mod_file()}
        for module in self.modules:
            out[OUT / f"{module}.rs"] = self.module_file(module)
        return out


def rustfmt(src: str) -> str:
    r = subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout"],
        input=src,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise GenError("rustfmt failed:\n" + r.stderr[:4000])
    return r.stdout


def main():
    check = "--check" in sys.argv[1:]
    gen = Generator(json.loads(IDL.read_text()))
    files = {path: rustfmt(src) for path, src in gen.files().items()}
    if check:
        stale = [p for p, src in files.items() if not p.exists() or p.read_text() != src]
        extra = [p for p in OUT.glob("*.rs") if p not in files]
        for p in stale + extra:
            print(f"stale: {p.relative_to(ROOT)}", file=sys.stderr)
        sys.exit(1 if stale or extra else 0)
    OUT.mkdir(parents=True, exist_ok=True)
    for p in OUT.glob("*.rs"):
        if p not in files:
            p.unlink()
    for path, src in files.items():
        path.write_text(src)
    print(f"wrote {len(files)} files to {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    try:
        main()
    except GenError as e:
        print(f"plugingen: {e}", file=sys.stderr)
        sys.exit(1)
