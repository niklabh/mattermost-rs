#!/usr/bin/env python3
"""Inventory every api4 route Go registers, and say which ones this server answers.

CLAUDE.md's denominator rule ("762 api4 route+method pairs ... progress reported against a
subset is progress reported against the wrong number") needs a number nobody has to count by
hand. This produces it from the two sources of truth:

  * `reference/mattermost/server/channels/api4/*.go` — gorilla registrations, resolved through
    the `BaseRoutes` prefix table in `api.go`. Both routers are covered: `Init` (the HTTP
    server) and `InitLocal` (the unix-socket admin API), which register onto *different*
    routers, so the same path+method can legitimately appear in both.
  * `crates/mm-api/src/lib.rs` — the axum chain, parsed by matching parentheses rather than by
    line, since a `.route(...)` call spans as many lines as its comment needs.
  * `crates/mm-api/src/local.rs` — the **second** axum chain. The local-mode routes land on a
    different router bound to a unix socket, so a local route served there is invisible to a
    parse of `lib.rs` alone; before 2026-09-11 this script hardcoded every local pair as
    unserved, which would have reported a whole migrated router as no progress at all.

Usage:
    scripts/routes.py                 summary by base + a served/total tally
    scripts/routes.py --todo          every unserved route, ordered by base
    scripts/routes.py --todo --local  include the local-mode (unix socket) routes
    scripts/routes.py --served        every route we answer, with its Go handler name
    scripts/routes.py --tsv           the whole inventory as TSV

A route counts as served only when the axum router registers that *method* on the matching
path. `partially_migrated` forwards unregistered methods to Go, so a path with a GET and no
DELETE is one served pair and one unserved pair — not a served path.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
API4 = ROOT / "reference/mattermost/server/channels/api4"
LIBRS = ROOT / "crates/mm-api/src/lib.rs"
LOCALRS = ROOT / "crates/mm-api/src/local.rs"

# `{name:[A-Za-z0-9]+}` -> `{name}`. Go escapes some classes (`[A-Za-z0-9\\_\\-\\.]`), and the
# import/export names carry a `.zip` suffix inside the braces, so the regex half is matched
# lazily up to the closing brace that ends the parameter.
PARAM = re.compile(r"\{([a-z_0-9]+):[^}]*\}")

# A gorilla parameter whose pattern is a *literal* is not a parameter at all. The websocket route
# is registered as `{websocket:websocket(?:\/)?}` — the braces exist only to attach the optional
# trailing slash — and reading it as `{websocket}` made the axum route `/api/v4/websocket`, which
# is the literal path every client uses, fail to match its own inventory row. It reported an
# unserved route this server has been answering.
#
# The optional-trailing-slash suffix is stripped first, then the remainder is a literal if it
# holds no regex metacharacter.
TRAILING_SLASH_SUFFIX = re.compile(r"(?:\(\?:)?\\?/(?:\)?)?\?$")
REGEX_METACHARACTERS = set("[]().*+?|^$\\{}")


def literal_param(name: str, pattern: str) -> str | None:
    """`{name:pattern}` as a literal segment, or None when the pattern really is a pattern."""
    body = TRAILING_SLASH_SUFFIX.sub("", pattern)
    if body and not (set(body) & REGEX_METACHARACTERS):
        return body
    return None


def normalise(path: str) -> str:
    def replace(match: "re.Match[str]") -> str:
        name = match.group(1)
        pattern = match.group(0)[len(name) + 2 : -1]
        return literal_param(name, pattern) or "{%s}" % name

    path = PARAM.sub(replace, path)
    return path if path == "/" else path.rstrip("/")


def base_prefixes(text: str, start: int, end: int) -> dict:
    """Resolve `api.BaseRoutes.X = api.BaseRoutes.Y.PathPrefix("/z")` chains to full paths."""
    prefixes = {}
    assign = re.compile(
        r'api\.BaseRoutes\.(\w+) = '
        r'(?:api\.BaseRoutes\.(\w+)|srv\.\w+)'
        r'(?:\.PathPrefix\((?:"((?:[^"\\]|\\.)*)"|model\.(\w+))\))?'
    )
    for line in text[start:end].splitlines():
        m = assign.search(line)
        if not m:
            continue
        name, parent, literal, const = m.groups()
        prefix = prefixes.get(parent, "") if parent else ""
        if literal is not None:
            prefix += literal.replace("\\\\", "\\")
        elif const in ("APIURLSuffix", "APIURLSuffixV4"):
            prefix += "/api/v4"
        elif const == "APIURLSuffixV5":
            prefix += "/api/v5"
        prefixes[name] = prefix
    return prefixes


def collect():
    api_go = (API4 / "api.go").read_text()
    init_at = api_go.index("func Init(srv *app.Server)")
    local_at = api_go.index("func InitLocal(srv *app.Server)")
    remote_prefixes = base_prefixes(api_go, init_at, local_at)
    local_prefixes = base_prefixes(api_go, local_at, len(api_go))

    # gorilla registrations are matched by scanning for `.Handle("` and then walking
    # parentheses, not by a single regex: four shapes exist and half of them span lines —
    # a wrapper taking `handlerParamFileAPI` as a second argument, a
    # `contentFlaggingRequired(...)` middleware wrapping the handler, `RateLimitedHandler`
    # taking a whole `model.RateLimitSettings{...}` literal, and group.go's multi-line
    # registrations. A line-oriented regex silently dropped 39 routes, which is exactly the
    # kind of undercount the denominator rule exists to prevent.
    opener = re.compile(r'api\.BaseRoutes\.(\w+)\.Handle\(\s*"((?:[^"\\]|\\.)*)"\s*,')
    # The handler is whichever argument is a bare lowercase identifier rather than a call: the
    # wrappers are all `UpperCamel` and are always followed by `(`.
    bare_arg = re.compile(r'[(,]\s*(?:\w+\.)?([a-zA-Z]\w*)\s*[,)]')
    methods_re = re.compile(r'\A\s*\.Methods\(([^)]*)\)')
    func_start = re.compile(r'^func \(api \*API\) (\w+)\(', re.M)
    # `preference_local.go` writes bare `.Methods("GET")` where every other file writes
    # `http.MethodGet`. Both spellings are gorilla's; matching only the constant lost five
    # local-mode routes.
    method_const = re.compile(r'http\.Method(\w+)|"([A-Z]+)"')

    routes = []
    for path in sorted(API4.glob("*.go")):
        if path.name.endswith("_test.go"):
            continue
        text = path.read_text()
        # Which Init function encloses a registration decides which router it lands on, and so
        # which prefix table resolves it. Local ones are named `Init<Thing>Local`.
        spans = [(m.start(), m.group(1)) for m in func_start.finditer(text)]
        for m in opener.finditer(text):
            depth, i = 1, m.end()
            while depth and i < len(text):
                if text[i] == "(":
                    depth += 1
                elif text[i] == ")":
                    depth -= 1
                i += 1
            arg = text[m.end():i - 1]
            tail = methods_re.match(text[i:])
            if not tail:
                continue
            names = [n for n in bare_arg.findall("(" + arg + ")")
                     if n not in ("handlerParamFileAPI",)]
            if not names:
                print(f"warning: no handler in {path.name}: {arg[:60]}", file=sys.stderr)
                continue
            enclosing = ""
            for at, name in spans:
                if at < m.start():
                    enclosing = name
                else:
                    break
            base, suffix = m.group(1), m.group(2)
            local = enclosing.endswith("Local") or "APILocal" in arg
            table = local_prefixes if local else remote_prefixes
            if base not in table:
                print(f"warning: unresolved base {base} in {path.name}", file=sys.stderr)
                continue
            full = normalise(table[base] + suffix.replace("\\\\", "\\"))
            for const, literal in method_const.findall(tail.group(1)):
                routes.append({
                    "method": (const or literal).upper(),
                    "path": full,
                    "handler": names[0],
                    "wrapper": "",
                    "file": path.name,
                    "base": base,
                    "local": local,
                })
    return routes


# Paths where the axum spelling deliberately differs from gorilla's, so the diff stays honest.
#
# `{category_id}` is renamed to `{category}` on purpose: `parameter_is_id_shaped` in
# `mm-api/src/lib.rs` keys off the `_id` suffix and would impose `[A-Za-z0-9]+`, but Go's class
# for a category is `[A-Za-z0-9_-]+`. Keeping Go's name would forward every category id
# containing `_` or `-` to Go's 404. The rest are literal siblings axum must register because it
# prefers a literal over a parameter where gorilla goes by registration order; they answer as
# the `{...}` route Go would have chosen, so they add no pair to the numerator.
ALIASES = {
    "/api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category}":
        "/api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}",
    "/api/v4/users/me": "/api/v4/users/{user_id}",
    "/api/v4/users/me/preferences": "/api/v4/users/{user_id}/preferences",
    "/api/v4/users/me/teams/members": "/api/v4/users/{user_id}/teams/members",
}


def served(source=LIBRS):
    """Every (method, path) an axum router registers, by matching parens on `.route(`.

    Takes the file so the same parser reads both routers: `lib.rs` for the HTTP one and
    `local.rs` for the unix-socket one.
    """
    text = source.read_text()
    out = set()
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', text):
        depth, i = 1, m.end()
        while depth and i < len(text):
            if text[i] == "(":
                depth += 1
            elif text[i] == ")":
                depth -= 1
            i += 1
        body = text[m.end():i]
        # Strip comments so a verb named in prose is not counted as a registration.
        body = re.sub(r'//[^\n]*', '', body)
        path = m.group(1).replace("{*", "{")
        # `get(system::get_system_ping)` in lib.rs, but `get(local_get_system_ping)` in local.rs:
        # the local router's handlers are module-private and therefore unqualified. Requiring the
        # `::` matched neither an unqualified handler nor anything else — it silently reported a
        # whole migrated router as unserved. The trailing `[),]` is what keeps this from matching
        # an extractor or a nested call: a handler is the last thing before the closing paren.
        for verb in re.findall(
                r'\b(get|post|put|delete|patch)\s*\(\s*(?:[a-z_0-9]+::)*[a-z_0-9]+\s*[),]',
                body):
            here = normalise(path)
            out.add((verb.upper(), ALIASES.get(here, here)))
            # axum's `get` answers HEAD as well, dispatching it to the GET handler with the body
            # removed (`MethodRouter::call_with_state` tries `head` and then falls through to
            # `get`; axum 0.8's own `get_accepts_head` test pins it). Go registers HEAD
            # explicitly on the file routes, so without this the inventory would under-count what
            # this server actually answers.
            if verb == "get":
                out.add(("HEAD", ALIASES.get(here, here)))
    return out


def main():
    args = set(sys.argv[1:])
    routes = collect()
    have = served()
    have_local = served(LOCALRS) if LOCALRS.exists() else set()
    want_local = "--local" in args

    for r in routes:
        # Two routers, two registries. A path+method can legitimately be served on one and not
        # the other — `/api/v4/server_busy` was migrated on both at once, but `/system/timezones`
        # exists only on the HTTP side — so the local flag selects which registry to ask.
        r["served"] = (r["method"], r["path"]) in (have_local if r["local"] else have)

    if "--tsv" in args:
        for r in routes:
            print("\t".join([
                "SERVED" if r["served"] else "-", r["method"], r["path"],
                r["handler"], "local" if r["local"] else "http", r["file"]]))
        return

    if "--served" in args:
        for r in sorted(routes, key=lambda r: (r["path"], r["method"])):
            if r["served"]:
                print(f'{r["method"]:7} {r["path"]:70} {r["handler"]} ({r["file"]})')
        return

    if "--todo" in args:
        for r in sorted(routes, key=lambda r: (r["file"], r["path"], r["method"])):
            if r["served"] or (r["local"] and not want_local):
                continue
            tag = "local " if r["local"] else ""
            print(f'{r["method"]:7} {r["path"]:70} {tag}{r["handler"]} ({r["file"]})')
        return

    by_file = {}
    for r in routes:
        slot = by_file.setdefault(r["file"], [0, 0, 0])
        slot[1] += 1
        slot[0] += r["served"]
        slot[2] += r["local"]
    for name in sorted(by_file, key=lambda n: (-by_file[n][0], n)):
        done, total, local = by_file[name]
        bar = f"{done}/{total}"
        print(f'{name:34} {bar:>9}  {"" if not local else f"({local} local-mode)"}')
    total = len(routes)
    done = sum(r["served"] for r in routes)
    local = sum(r["local"] for r in routes)
    print(f'\n{done}/{total} route+method pairs served '
          f'({total - local} on the HTTP router, {local} local-mode).')
    http_left = sum(1 for r in routes if not r["served"] and not r["local"])
    local_left = sum(1 for r in routes if not r["served"] and r["local"])
    print(f'{http_left} HTTP pairs and {local_left} local-mode pairs remain.')


if __name__ == "__main__":
    main()
