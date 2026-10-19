#!/usr/bin/env python3
"""Exercise the companion against a real isolated Firefox, with trusted input.

Run on a scratch host: python3 browser/build.py, then this script with Firefox
on PATH. No existing profile or browser is used.
"""
import argparse
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from companion import Bidi, Companion


class FixtureDriver(Bidi):
    """Errors are safe to inspect only in this disposable fixture browser."""
    def call(self, method, params=None):
        self.sequence += 1
        self.ws.send(json.dumps({"id": self.sequence, "method": method, "params": params or {}}))
        while True:
            reply = json.loads(self.ws.recv())
            if reply.get("id") != self.sequence:
                continue
            if reply.get("type") != "success":
                raise RuntimeError(f"Fixture BiDi error in {method}: {reply.get('error')}: {reply.get('message')}")
            return reply["result"]


class Page(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(b'<!doctype html><title>Companion test</title><input id="field"><button id="button" onclick="window.trustedClick=event.isTrusted">Click</button><script>document.querySelector("input").oninput=e=>window.trustedInput=e.isTrusted</script>')

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--firefox", default="firefox")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="browser-node-firefox-") as temporary:
        directory = Path(temporary)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        profile = directory / "profile"
        profile.mkdir()
        (profile / "user.js").write_text('user_pref("browser.shell.checkDefaultBrowser", false);\nuser_pref("browser.aboutwelcome.enabled", false);\n')
        server = ThreadingHTTPServer(("127.0.0.1", 0), Page)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = {**os.environ, "MOZ_REMOTE_ALLOW_SYSTEM_ACCESS": "1"}
        with (directory / "firefox.log").open("wb") as output:
            process = subprocess.Popen([args.firefox, "--headless", "--no-remote", "--profile", str(profile), "--remote-debugging-port", str(port)], env=environment, stdout=output, stderr=output)
        driver = None
        try:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                try:
                    driver = FixtureDriver(port, {process.pid})
                    break
                except (ConnectionError, OSError, ValueError, EOFError):
                    time.sleep(0.2)
            assert driver is not None, "Firefox did not start its Remote Agent"
            package, = (root / "dist").glob("*-firefox.zip")
            driver.call("webExtension.install", {"extensionData": {"type": "archivePath", "path": str(package)}})
            origin = f"http://127.0.0.1:{server.server_port}"
            first = driver.call("browsingContext.getTree", {"maxDepth": 0})["contexts"][0]["context"]
            driver.call("browsingContext.navigate", {"context": first, "url": origin, "wait": "complete"})
            tab = driver.function("() => { const {ExtensionParent} = ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs'); return ExtensionParent.apiManager.global.tabTracker.getId(gBrowser.selectedTab); }",
                                  [], {"context": driver.chrome_context()})
            companion = Companion(driver)
            grant = {"tab": tab, "origin": origin, "control": False, "expires_at": int(time.time()) + 600}
            companion.update([grant])
            assert companion.execute({"tab": tab, "action": "snapshot"})["title"] == "Companion test"
            image = companion.execute({"tab": tab, "action": "screenshot"})
            assert image["format"] == "jpeg" and image["data"].startswith("/9j")
            try:
                companion.execute({"tab": tab, "action": "type", "params": {"text": "refuse"}})
            except ValueError:
                pass
            else:
                raise AssertionError("View grant allowed typing")
            companion.update([{**grant, "control": True}])
            companion.execute({"tab": tab, "action": "click", "params": {"selector": "#button"}})
            companion.execute({"tab": tab, "action": "type", "params": {"selector": "#field", "text": "hello"}})
            result = companion.execute({"tab": tab, "action": "evaluate", "params": {"script": "return {value:document.querySelector('#field').value, click:window.wrappedJSObject.trustedClick,input:window.wrappedJSObject.trustedInput};"}})
            assert result == {"value": "hello", "click": True, "input": True}, result
            safe_scope = companion.execute({"tab": tab, "action": "evaluate", "params": {"script": "return [typeof globalThis.ChromeUtils,typeof Components.utils];"}})
            assert safe_scope == ["undefined", "undefined"], safe_scope
            companion.update([])
            try:
                companion.execute({"tab": tab, "action": "snapshot"})
            except ValueError:
                pass
            else:
                raise AssertionError("Revoked grant remained readable")
            print("Firefox (WebDriver BiDi): real tab mapping, screenshot, snapshot, trusted click/type, sandboxed evaluation, view-only enforcement and revocation passed")
        finally:
            if driver:
                with contextlib.suppress(OSError, ValueError, EOFError):
                    driver.close()
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            server.shutdown()


if __name__ == "__main__":
    main()
