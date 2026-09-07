#!/usr/bin/env python3
"""Ask the running Go server what every unmigrated route answers, and print the map.

`scripts/routes.py --todo` says what is left; this says what each of those routes *does* on this
deployment, which is what decides whether porting it is an afternoon or a fortnight. A whole
family answering one licence refusal is a different job from a family of real reads, and reading
seventeen handlers to find that out costs more than asking.

Usage:
    scripts/probe-routes.py                 every safe unmigrated HTTP route, grouped by answer
    scripts/probe-routes.py --file ldap.go  one Go file
    scripts/probe-routes.py --raw           one line per route, for grepping

# What it will not send

Only requests that cannot create or destroy anything:

  * every path parameter is filled with an id that does not exist, so a `DELETE` deletes nothing
    and a `PATCH` patches nothing;
  * a `POST`/`PUT`/`PATCH` whose path has **no** id parameter is **skipped** — `POST /api/v4/teams`
    would create a team, and the parity fixtures count teams;
  * anything under `/uploads`, `/imports`, `/exports`, `/plugins`, `/brand`, `/image` or `/files`
    is skipped: they take multipart bodies or stream files, and a JSON probe tells you nothing.

Skipped routes are listed too, with the reason, so the map has no silent holes.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GO = os.environ.get("MMRS_GO_URL", "http://localhost:8065")
LOGIN_ID = os.environ.get("MMRS_LOGIN_ID", "slice@example.com")
PASSWORD = os.environ.get("MMRS_PASSWORD", "Slice-Test-1234")

# An id of the right shape that is no object. 26 characters, Go's `IsValidId` charset.
NOWHERE = "zzzzzzzzzzzzzzzzzzzzzzzzzz"
UNSAFE_PREFIXES = (
    "/api/v4/uploads",
    "/api/v4/imports",
    "/api/v4/exports",
    "/api/v4/plugins",
    "/api/v4/brand",
    "/api/v4/image",
    "/api/v4/files",
)


def token() -> str:
    body = json.dumps({"login_id": LOGIN_ID, "password": PASSWORD}).encode()
    request = urllib.request.Request(
        f"{GO}/api/v4/users/login", data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(request) as response:
        header = response.headers.get("Token")
    if not header:
        raise SystemExit("the Go server did not mint a token — is the stack up?")
    return header


def fill(path: str) -> str:
    """Every `{name}` becomes an id that does not exist; the odd non-id parameter gets a word."""
    def one(match):
        name = match.group(1)
        if name in ("import_name", "export_name"):
            return "mmrs-nowhere.zip"
        if name.endswith("_name") or name in ("username", "term", "job_type", "category"):
            return "mmrs-nowhere"
        if name == "email":
            return "mmrs-nowhere@example.invalid"
        return NOWHERE
    return re.sub(r"\{([a-z_0-9]+)\}", one, path)


def skip_reason(method: str, path: str) -> str | None:
    if path.startswith(UNSAFE_PREFIXES):
        return "multipart or file-streaming"
    if method in ("POST", "PUT", "PATCH") and "{" not in path:
        return "would create an object"
    return None


def probe(method: str, path: str, auth: str) -> tuple[int, str]:
    body = b"{}" if method in ("POST", "PUT", "PATCH") else None
    request = urllib.request.Request(
        f"{GO}{path}",
        data=body,
        method=method,
        headers={"Authorization": f"Bearer {auth}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, ""
    except urllib.error.HTTPError as err:
        raw = err.read()
        try:
            return err.code, json.loads(raw).get("id", "")
        except (ValueError, AttributeError):
            return err.code, ""
    except urllib.error.URLError as err:
        return 0, f"unreachable: {err.reason}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--file", help="only routes registered in this api4 file")
    parser.add_argument("--raw", action="store_true", help="one line per route")
    args = parser.parse_args()

    todo = subprocess.run(
        [sys.executable, str(ROOT / "scripts/routes.py"), "--todo"],
        capture_output=True, text=True, check=True,
    ).stdout.splitlines()

    auth = token()
    rows = []
    for line in todo:
        parts = line.split()
        if len(parts) < 3:
            continue
        method, path = parts[0], parts[1]
        origin = parts[-1].strip("()")
        if args.file and origin != args.file:
            continue
        reason = skip_reason(method, path)
        if reason:
            rows.append((method, path, origin, None, reason))
            continue
        status, error_id = probe(method, fill(path), auth)
        rows.append((method, path, origin, status, error_id))

    if args.raw:
        for method, path, origin, status, error_id in rows:
            print(f"{status if status else 'skip':>4} {error_id:<48} {method:<7} {path} ({origin})")
        return

    groups: dict[tuple, list] = {}
    for method, path, origin, status, error_id in rows:
        groups.setdefault((status, error_id), []).append((method, path, origin))
    for (status, error_id), members in sorted(
        groups.items(), key=lambda kv: (-len(kv[1]), str(kv[0]))
    ):
        head = f"{status} {error_id}".strip() if status else f"skipped — {error_id}"
        print(f"\n=== {head}  ({len(members)}) ===")
        for method, path, origin in sorted(members, key=lambda m: (m[2], m[1], m[0])):
            print(f"  {method:<7} {path:<64} {origin}")


if __name__ == "__main__":
    main()
