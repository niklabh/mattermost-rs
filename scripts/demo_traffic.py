#!/usr/bin/env python3
"""Drive a real browser session through mm-api and report what share of it Rust answered.

    scripts/demo_traffic.py run   LOG BASE      the browser session, then the report
    scripts/demo_traffic.py report LOG [OFFSET] only the report, over LOG from byte OFFSET

Normally reached through `scripts/demo-traffic.sh`, which starts mm-api with
`MM_API_TRAFFIC_LOG=1` and seeds the accounts first. The report reads the `mm_api::traffic`
lines (see `crates/mm-api/src/traffic.rs`) written after the session started, so anything else
hitting the same mm-api at the same time is counted too: run it on an otherwise idle stack.

The session is tester's, on team `playground` with `lounge` as the second team: log in; switch
teams and channels; post, edit, delete; react; reply in a thread and open Threads; search; open a
DM; upload a file; change a display preference; view a profile; log out. Selectors come from the
live DOM, not from the webapp's source (CLAUDE.md puts `webapp/` out of bounds).
"""

import collections
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import webdriver  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ANSI = re.compile(r"\x1b\[[0-9;]*m")
LINE = re.compile(r"mm_api::traffic: method=(\S+) path=(\S+) route=(\S+) status=(\d+) "
                  r"served_by=(\S+) forwarded_at=(\S+)(?: params=(\S+))?")
USER, PASSWORD = "tester", "Tester-Pass-2026"
PEER, PEER_PASSWORD = "tester2", "Tester2-Pass-2026"
ENTER, ESCAPE = webdriver.KEYS["enter"], webdriver.KEYS["escape"]


# ---------------------------------------------------------------- the session

def api(base, method, path, body=None, token=None):
    req = urllib.request.Request(base + "/api/v4" + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json"})
    if token:
        req.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read() or b"null"), resp.headers


def peer_says_hello(base):
    """tester2 writes to tester first, so the DM has someone else's post and profile to open.

    Before the measured window: it is setup, not the browser's traffic."""
    _, headers = api(base, "POST", "/users/login", {"login_id": PEER, "password": PEER_PASSWORD})
    token = headers["Token"]
    me, _ = api(base, "GET", "/users/me", token=token)
    other, _ = api(base, "GET", f"/users/username/{USER}", token=token)
    dm, _ = api(base, "POST", "/channels/direct", [me["id"], other["id"]], token=token)
    api(base, "POST", "/posts", {"channel_id": dm["id"], "message": "hi from tester2"}, token=token)
    api(base, "POST", "/users/logout", {}, token=token)


def last_post_id(b):
    ids = re.findall(r'id="post_([a-z0-9]{26})"', b.source())
    if not ids:
        raise webdriver.WebDriverError("no post in the center channel")
    return ids[-1]


def post_menu(b, pid, button):
    b.hover(b.find(f"#post_{pid}"))
    time.sleep(0.4)
    b.click(f"#CENTER_{button}_{pid}")
    time.sleep(0.6)


def escape(b):
    b.s("POST", "/actions", {"actions": [{"type": "key", "id": "kb", "actions": [
        {"type": "keyDown", "value": ESCAPE}, {"type": "keyUp", "value": ESCAPE}]}]})
    time.sleep(0.4)


def send(b, css, text):
    box = b.find(css)
    b.keys(box, text)
    # Separately: an Enter that arrives with the text can land before the editor has taken it.
    time.sleep(0.5)
    b.keys(box, ENTER)
    time.sleep(1.5)


def session(base, upload_path):
    """The scripted session. Returns the names of the steps that failed."""
    b = webdriver.Browser()
    failed = []

    def step(name, fn):
        print(f"  - {name}", flush=True)
        try:
            fn()
        except Exception as err:  # a failed step is reported, and the session carries on
            import traceback
            here = [f for f in traceback.extract_tb(err.__traceback__) if f.filename == __file__]
            where = f" (demo_traffic.py:{here[-1].lineno})" if here else ""
            shot = os.path.join(b.work, f"failed-{len(failed)}.png")
            try:
                b.screenshot(shot)
                where += f", screenshot {shot}"
            except Exception:
                pass
            print(f"    FAILED{where}: {err}", flush=True)
            failed.append(name)

    def login():
        b.go(base + "/login")
        time.sleep(2)
        if "/landing" in b.url():  # the "View in Browser" screen
            b.click("a.btn-tertiary")
        b.type("#input_loginId", USER)
        b.type("#input_password-input", PASSWORD)
        b.click("#saveSetting")
        b.find("#post_textbox", timeout=30)
        time.sleep(3)

    def switch_teams():
        b.click("#loungeTeamButton")
        time.sleep(2)
        b.click("#playgroundTeamButton")
        time.sleep(2)

    def switch_channels():
        b.click("#sidebarItem_off-topic")
        time.sleep(1.5)
        b.click("#sidebarItem_town-square")
        time.sleep(1.5)

    state = {}

    def post_edit_delete():
        stamp = time.strftime("%H:%M:%S")
        send(b, "#post_textbox", f"demo: a message to edit and delete ({stamp})")
        pid = last_post_id(b)
        post_menu(b, pid, "button")
        b.click(f"#edit_post_{pid}")
        send(b, "#edit_textbox", " (edited)")
        post_menu(b, pid, "button")
        b.click(f"#delete_post_{pid}")
        b.click("#deletePostModalButton")
        time.sleep(1)
        send(b, "#post_textbox", f"demo: a message to keep ({stamp})")
        state["pid"] = last_post_id(b)

    def react():
        pid = state["pid"]
        post_menu(b, pid, "reaction")
        b.keys(b.find("#emojiPickerSearch"), "thumbsup")
        time.sleep(0.6)
        b.keys(b.find("#emojiPickerSearch"), ENTER)
        b.find(f"#postReaction-{pid}-\\+1", timeout=10)

    def reply_and_threads():
        post_menu(b, state["pid"], "commentIcon")
        send(b, "#reply_textbox", "demo: a reply in the thread")
        escape(b)
        b.click("#sidebarItem_threads")
        time.sleep(2.5)
        b.click("#sidebarItem_town-square")
        time.sleep(1.5)

    def search():
        b.click("#searchFormContainer")
        time.sleep(0.8)
        b.keys(b.find('input[aria-label="Search messages"]'), "demo" + ENTER)
        time.sleep(2.5)
        escape(b)

    def direct_message():
        b.click("#newDirectMessageButton")
        time.sleep(1)
        b.keys(b.find("#selectItems input"), PEER)
        time.sleep(1.5)
        b.keys(b.find("#selectItems input"), ENTER)
        time.sleep(0.6)
        b.click("#saveItems")
        time.sleep(2.5)
        send(b, "#post_textbox", "demo: hello over DM")

    def upload():
        b.upload("#fileUploadInput", upload_path)
        time.sleep(2.5)
        send(b, "#post_textbox", "demo: a file")
        time.sleep(1)

    def preference():
        b.click('button[aria-label="Settings"]')
        b.click("#displayButton")
        b.click("#clockEdit")
        # Toggle, so every run is a real write whichever way the last one left it.
        checked = b.js('return document.querySelector("#clockFormatA").checked')
        b.click("#clockFormatB" if checked else "#clockFormatA")
        b.click("#saveSetting")
        time.sleep(1)
        b.js('document.querySelector("#closeButton").click()')
        time.sleep(1)

    def view_profile():
        # The first user-popover in a DM is the earliest post, tester2's greeting.
        b.click("#post-list button.user-popover")
        # Present but mid-transition, so not yet "displayed" to WebDriver.
        b.find("#userPopoverUsername", timeout=10, visible=False)
        time.sleep(1.5)
        escape(b)

    def logout():
        b.click("#userAccountMenuButton")
        time.sleep(0.8)
        b.js('[...document.querySelectorAll("#userAccountMenu li")]'
             '.find(l => l.innerText.trim() === "Log out").click()')
        b.find("#input_loginId", timeout=15)
        time.sleep(1.5)

    try:
        for name, fn in [
            ("log in", login), ("switch teams", switch_teams), ("switch channels", switch_channels),
            ("post, edit, delete", post_edit_delete), ("react", react),
            ("reply in a thread, open Threads", reply_and_threads), ("search", search),
            ("open a DM", direct_message), ("upload a file", upload),
            ("change a preference", preference), ("view a profile", view_profile),
            ("log out", logout),
        ]:
            step(name, fn)
    finally:
        b.quit()
    return failed


# ---------------------------------------------------------------- the report

def go_inventory():
    """Go's HTTP-router templates, as (method, template, regex, literal-segment count)."""
    out = subprocess.run([sys.executable, os.path.join(ROOT, "scripts/routes.py"), "--tsv"],
                         capture_output=True, text=True, check=True).stdout
    routes = []
    for row in out.splitlines():
        cols = row.split("\t")
        if len(cols) < 5 or cols[4] != "http":
            continue
        method, template = cols[1], cols[2]
        pattern = "^" + re.sub(r"\\\{[a-z_0-9]+\\\}", "[^/]+", re.escape(template)) + "/?$"
        literals = sum(1 for seg in template.split("/") if seg and not seg.startswith("{"))
        routes.append((method, template, re.compile(pattern), literals))
    return routes


def template_for(routes, method, path):
    best = None
    for m, template, rx, literals in routes:
        if (m == method or (method == "HEAD" and m == "GET")) and rx.match(path):
            if best is None or literals > best[1]:
                best = (template, literals)
    return best[0] if best else None


def reason_at(site):
    """The comment above a forwarding call site, first sentence, with any D-entry it names."""
    file, _, line = site.rpartition(":")
    # Reached as a function value (`partially_migrated`'s method fallback), so the caller is a
    # closure shim in core or axum rather than a line of ours.
    if not file.startswith("crates/"):
        return "method not registered on this path: partially_migrated's fallback"
    path = file if os.path.isabs(file) else os.path.join(ROOT, file)
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return site
    i = int(line) - 1
    comment = []
    j = i - 1
    while j >= 0 and j > i - 40:
        text = lines[j].strip()
        if text.startswith("//"):
            comment.insert(0, text.lstrip("/! ").strip())
        elif comment or not text or text.endswith(("{", "(", ",")) or text.startswith(("if", "}")):
            if comment:
                break
        j -= 1
    text = " ".join(comment)
    d_entries = sorted(set(re.findall(r"D-\d+", " ".join(lines[max(0, i - 60):i + 1]))))
    first = re.split(r"(?<=[.;])\s", text, maxsplit=1)[0][:160] if text else "(no comment at the call)"
    return first + (f" [{', '.join(d_entries)}]" if d_entries else "")


def report(log, offset=0):
    with open(log, "rb") as f:
        f.seek(offset)
        raw = f.read().decode("utf-8", "replace")
    rows = [m.groups() for m in (LINE.search(ANSI.sub("", l)) for l in raw.splitlines()) if m]
    if not rows:
        print("no mm_api::traffic lines: is mm-api running with MM_API_TRAFFIC_LOG=1?")
        return 1
    routes = go_inventory()
    total = len(rows)
    rust = sum(1 for r in rows if r[4].startswith("rust"))
    api_rows = [r for r in rows if r[1].startswith("/api/")]
    api_rust = sum(1 for r in api_rows if r[4].startswith("rust"))

    go_groups = collections.Counter()
    reasons = {}
    for method, path, route, status, served_by, site, params in rows:
        if served_by.startswith("rust"):
            continue
        if not path.startswith("/api/"):
            kind = "static" if path.startswith(("/static/", "/plugins/")) or "." in path.rsplit("/", 1)[-1] \
                else "webapp page"
            key = (method, "/static/*" if path.startswith("/static/") else path, f"{kind} (Go serves the client)")
        else:
            template = template_for(routes, method, path) or f"{path} (not in Go's api4 inventory)"
            if route == "-":
                why = "unregistered route"
            else:
                reason = reason_at(site)
                why = reason if reason.startswith("method not registered") else \
                    f"forwarded branch at {site}: {reason}"
            if params and params != "-":
                template += "?" + params
            key = (method, template, why)
        go_groups[key] += 1
        reasons[key] = status

    print()
    print(f"requests through mm-api: {total}")
    print(f"answered by Rust:        {rust}  ({100.0 * rust / total:.1f}% of requests)")
    print(f"  of the {len(api_rows)} /api/ requests, Rust answered {api_rust}"
          f" ({100.0 * api_rust / max(1, len(api_rows)):.1f}%)")
    print(f"answered by Go:          {total - rust}")
    print()
    print("Go-served calls, most frequent first:")
    for (method, template, why), n in go_groups.most_common():
        print(f"  {n:4d}  {method:6s} {template}  [{reasons[(method, template, why)]}]")
        print(f"        {why}")
    return 0


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "report":
        return report(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 0)
    if len(sys.argv) != 4 or sys.argv[1] != "run":
        print(__doc__)
        return 2
    log, base = sys.argv[2], sys.argv[3].rstrip("/")
    work = os.path.expanduser("~/snap/firefox/common/mmrs-demo")
    os.makedirs(work, exist_ok=True)
    upload_path = os.path.join(work, "demo-upload.txt")
    with open(upload_path, "w") as f:
        f.write(f"uploaded by scripts/demo-traffic.sh at {time.ctime()}\n")
    peer_says_hello(base)
    time.sleep(0.5)
    offset = os.path.getsize(log)
    print("browser session:")
    failed = session(base, upload_path)
    time.sleep(1.5)
    status = report(log, offset)
    if failed:
        print(f"\n{len(failed)} step(s) failed: {', '.join(failed)}")
        return 1
    return status


if __name__ == "__main__":
    sys.exit(main())
