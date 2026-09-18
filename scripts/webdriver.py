"""A minimal W3C WebDriver client over raw HTTP, for scripts/demo_traffic.py.

selenium is not installed on the hosts this runs on, and the protocol needs a dozen calls. Snap
Firefox cannot read /tmp, so the profile lives under $HOME/snap/firefox/common/.
"""

import json
import os
import shutil
import subprocess
import time
import urllib.error
import urllib.request

W3C_ELEMENT = "element-6066-11e4-a52e-4f735466cecc"
KEYS = {"enter": "", "escape": "", "ctrl": "", "up": ""}


class WebDriverError(Exception):
    pass


class Browser:
    @classmethod
    def attach(cls, port, session):
        """Reuse a session another process started (for exploring a page step by step)."""
        self = cls.__new__(cls)
        self.port, self.base, self.session, self.driver = port, f"http://127.0.0.1:{port}", session, None
        return self

    def __init__(self, port=4445, headless=True):
        self.port = port
        self.base = f"http://127.0.0.1:{port}"
        self.work = os.path.expanduser("~/snap/firefox/common/mmrs-demo")
        shutil.rmtree(os.path.join(self.work, "profile"), ignore_errors=True)
        os.makedirs(os.path.join(self.work, "profile"), exist_ok=True)
        self.driver = subprocess.Popen(
            ["/snap/bin/geckodriver", "--port", str(port)],
            stdout=open(os.path.join(self.work, "geckodriver.log"), "w"),
            stderr=subprocess.STDOUT,
        )
        for _ in range(60):
            try:
                self._call("GET", "/status")
                break
            except Exception:
                time.sleep(0.25)
        args = ["-profile", os.path.join(self.work, "profile"), "-width", "1400", "-height", "900"]
        if headless:
            args.insert(0, "-headless")
        caps = {"capabilities": {"alwaysMatch": {"browserName": "firefox",
                                                 "moz:firefoxOptions": {"args": args}}}}
        self.session = self._call("POST", "/session", caps)["sessionId"]

    def _call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read() or b"{}").get("value")
        except urllib.error.HTTPError as err:
            raise WebDriverError(f"{method} {path}: {err.read()[:400]!r}") from None

    def s(self, method, path, body=None):
        return self._call(method, f"/session/{self.session}{path}", body)

    def quit(self):
        try:
            self._call("DELETE", f"/session/{self.session}")
        finally:
            if self.driver:
                self.driver.terminate()

    def go(self, url):
        self.s("POST", "/url", {"url": url})

    def url(self):
        return self.s("GET", "/url")

    def js(self, script, *args):
        return self.s("POST", "/execute/sync", {"script": script, "args": list(args)})

    def find(self, css, timeout=15, visible=True):
        """The first element matching `css`, waiting for it; the element id."""
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                found = self.s("POST", "/elements", {"using": "css selector", "value": css})
                for el in found:
                    eid = next(iter(el.values()))
                    if not visible or self.s("GET", f"/element/{eid}/displayed"):
                        return eid
            except WebDriverError as err:
                last = err
            time.sleep(0.25)
        raise WebDriverError(f"no element {css!r} after {timeout}s ({last})")

    def exists(self, css):
        return bool(self.s("POST", "/elements", {"using": "css selector", "value": css}))

    def click(self, css, timeout=15):
        eid = self.find(css, timeout)
        try:
            self.s("POST", f"/element/{eid}/click", {})
        except WebDriverError as err:
            # Obscured by an overlay mid-animation: a DOM click reaches the same handler. Any
            # other failure (a click that navigated, say) is not retried.
            # By selector: this geckodriver hands an element argument to the script as a plain
            # object, so `arguments[0].click()` is a TypeError.
            if "intercepted" not in str(err) and "not interactable" not in str(err):
                raise
            self.js("document.querySelector(arguments[0]).click()", css)
        return eid

    def type(self, css, text, clear=False, timeout=15):
        eid = self.find(css, timeout)
        if clear:
            self.s("POST", f"/element/{eid}/clear", {})
        self.s("POST", f"/element/{eid}/value", {"text": text})
        return eid

    def keys(self, eid, text):
        self.s("POST", f"/element/{eid}/value", {"text": text})

    def hover(self, eid):
        # Element-origin pointer moves are refused by this geckodriver ("did not match any variant
        # of untagged enum PointerActionItem"), so move to the element's centre in the viewport.
        r = self.s("GET", f"/element/{eid}/rect")
        self.s("POST", "/actions", {"actions": [{
            "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
            # Away first: a pointer already over the element fires no new mouseover.
            "actions": [{"type": "pointerMove", "duration": 20, "origin": "viewport", "x": 1, "y": 1},
                        {"type": "pointerMove", "duration": 50, "origin": "viewport",
                         "x": int(r["x"] + r["width"] / 2), "y": int(r["y"] + r["height"] / 2)}]}]})

    def upload(self, css, path):
        eid = self.find(css, visible=False)
        self.s("POST", f"/element/{eid}/value", {"text": path})

    def source(self):
        return self.s("GET", "/source")

    def screenshot(self, path):
        import base64
        with open(path, "wb") as f:
            f.write(base64.b64decode(self.s("GET", "/screenshot")))
