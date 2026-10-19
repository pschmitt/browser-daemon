#!/usr/bin/env python3
"""Exercise the companion against a real isolated Firefox, with trusted input.

Run on a scratch host: python3 browser/build.py, then this script with Firefox
on PATH. No existing profile or browser is used.
"""
import argparse
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from companion import ADDON, Companion, Marionette


class FixtureDriver(Marionette):
    """Errors are safe to inspect only in this disposable fixture browser."""
    def read(self):
        message = super().read()
        if isinstance(message, list) and len(message) == 4 and message[2] is not None:
            raise RuntimeError(f"Fixture WebDriver error: {message[2].get('message', 'unknown')}")
        return message


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
        (profile / "user.js").write_text(f'user_pref("marionette.port", {port});\nuser_pref("browser.shell.checkDefaultBrowser", false);\nuser_pref("browser.aboutwelcome.enabled", false);\n')
        server = ThreadingHTTPServer(("127.0.0.1", 0), Page)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = {**os.environ, "MOZ_REMOTE_ALLOW_SYSTEM_ACCESS": "1"}
        with (directory / "firefox.log").open("wb") as output:
            process = subprocess.Popen([args.firefox, "--headless", "--no-remote", "--profile", str(profile), "--marionette"], env=environment, stdout=output, stderr=output)
        driver = None
        try:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                try:
                    driver = FixtureDriver(port, {process.pid})
                    break
                except (ConnectionError, OSError):
                    time.sleep(0.2)
            assert driver is not None, "Firefox did not start Marionette"
            package, = (root / "dist").glob("*-firefox.zip")
            driver.call("Addon:Install", {"path": str(package), "temporary": True})
            origin = f"http://127.0.0.1:{server.server_port}"
            driver.call("WebDriver:Navigate", {"url": origin})
            driver.call("Marionette:SetContext", {"value": "chrome"})
            tab = driver.script("const {ExtensionParent}=ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs'); return ExtensionParent.apiManager.global.tabTracker.getId(gBrowser.selectedTab);")
            companion = Companion(driver)
            grant = {"tab": tab, "origin": origin, "control": False, "expires_at": int(time.time()) + 600}
            companion.update([grant])
            assert companion.execute({"tab": tab, "action": "snapshot"})["title"] == "Companion test"
            image = companion.execute({"tab": tab, "action": "screenshot"})
            assert image["format"] == "png" and image["data"].startswith("iVBOR")
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
            print("Firefox: real tab mapping, screenshot, snapshot, trusted click/type, view-only enforcement and revocation passed")
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
