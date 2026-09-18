#!/usr/bin/env python3
"""Seed the demo accounts and teams through the REST API, idempotently.

    scripts/demo-seed.py [BASE_URL]        default http://127.0.0.1:$MMRS_API_PORT (8066)

Creates what is missing and leaves what exists alone, so it is safe to run on every `demo.sh up`:

  sliceuser  slice@example.com    Slice-Test-1234    system admin (the first account on a new server)
  tester     tester@example.com   Tester-Pass-2026
  tester2    tester2@example.com  Tester2-Pass-2026

and two open teams, `playground` and `lounge`, with all three as members. Nothing else is touched:
on a parity stack the other teams belong to the suites.

Through the API rather than SQL, because a row planted behind both servers' caches is state
neither of them wrote, and the point of the demo is what the servers do.
"""

import json
import os
import sys
import urllib.error
import urllib.request

USERS = [
    ("sliceuser", "slice@example.com", "Slice-Test-1234"),
    ("tester", "tester@example.com", "Tester-Pass-2026"),
    ("tester2", "tester2@example.com", "Tester2-Pass-2026"),
]
TEAMS = [("playground", "Playground"), ("lounge", "Lounge")]


def call(base, method, path, body=None, token=None, ok=(200, 201)):
    req = urllib.request.Request(base + "/api/v4" + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json",
                                          "X-Requested-With": "XMLHttpRequest"})
    if token:
        req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, json.loads(resp.read() or b"null"), resp.headers
    except urllib.error.HTTPError as err:
        payload = err.read()
        try:
            payload = json.loads(payload)
        except ValueError:
            pass
        return err.code, payload, err.headers


def login(base, username, password):
    status, body, headers = call(base, "POST", "/users/login",
                                 {"login_id": username, "password": password})
    return headers.get("Token") if status == 200 else None


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else \
        f"http://127.0.0.1:{os.environ.get('MMRS_API_PORT', '8066')}"
    ids = {}
    for username, email, password in USERS:
        token = login(base, username, password)
        if token:
            print(f"  user {username}: exists")
        else:
            status, body, _ = call(base, "POST", "/users",
                                   {"username": username, "email": email, "password": password})
            if status != 201:
                sys.exit(f"  user {username}: could not create ({status} {body}). If the account "
                         f"exists with another password, reset it or use a fresh database.")
            print(f"  user {username}: created")
            token = login(base, username, password)
        _, me, _ = call(base, "GET", "/users/me", token=token)
        ids[username] = (me["id"], token)

    admin_id, admin = ids["sliceuser"]
    _, me, _ = call(base, "GET", "/users/me", token=admin)
    if "system_admin" not in me.get("roles", ""):
        sys.exit("  sliceuser is not a system admin: it must be the first account on the server.")

    for name, display in TEAMS:
        status, team, _ = call(base, "GET", f"/teams/name/{name}", token=admin)
        if status == 200:
            print(f"  team {name}: exists")
        else:
            status, team, _ = call(base, "POST", "/teams",
                                   {"name": name, "display_name": display, "type": "O"}, token=admin)
            if status != 201:
                sys.exit(f"  team {name}: could not create ({status} {team})")
            print(f"  team {name}: created")
        for username, (user_id, _) in ids.items():
            status, _, _ = call(base, "GET", f"/teams/{team['id']}/members/{user_id}", token=admin)
            if status != 200:
                status, body, _ = call(base, "POST", f"/teams/{team['id']}/members",
                                       {"team_id": team["id"], "user_id": user_id}, token=admin)
                if status != 201:
                    sys.exit(f"  team {name}: could not add {username} ({status} {body})")
                print(f"  team {name}: added {username}")

    # The first-load tutorial and the admin "Start Trial" modal would otherwise greet every login.
    for username, (user_id, token) in ids.items():
        call(base, "PUT", f"/users/{user_id}/preferences", [
            {"user_id": user_id, "category": "tutorial_step", "name": user_id, "value": "999"},
            {"user_id": user_id, "category": "onboarding_task_list",
             "name": "onboarding_task_list_show", "value": "false"},
        ], token=token)
    print("  seed complete")


if __name__ == "__main__":
    main()
