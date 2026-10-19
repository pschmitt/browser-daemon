#!/usr/bin/env python3
"""Real Firefox extension -> daemon -> native pipe -> same Firefox round trip.

Uses disposable profiles, a fixture page, and ephemeral test credentials. The
test add-on has an extra popup bootstrap script and nativeMessaging pregranted
at installation; the production background and native host are unchanged.
"""
import argparse
import contextlib
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit
from urllib.request import ProxyHandler, Request, build_opener
import zipfile

from companion import ADDON, Bidi
from smoke_firefox import Page

HTTP = build_opener(ProxyHandler({}))


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(predicate):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (ConnectionError, OSError, KeyError, IndexError):
            pass
        time.sleep(0.1)
    raise AssertionError("Firefox native round-trip check timed out")


class Fixture(Page):
    def do_GET(self):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        if url.path == "/bootstrap":
            status, pairing = self.server.api("/api/extension/pair", "POST", {"origin": query["origin"][0]})
            assert status == 200
            value = {"code": pairing["code"], "server": self.server.daemon_url}
        elif url.path == "/stage":
            self.server.stage = query["value"][0]
            value = {"ok": True}
        elif url.path == "/mode":
            value = {"control": self.server.control}
        else:
            return super().do_GET()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(value).encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--firefox", default="firefox")
    parser.add_argument("--daemon", default="target/debug/browser-daemon")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="browser-node-native-") as temporary:
        directory = Path(temporary)
        home = directory / "home"
        home.mkdir()
        remote_port, daemon_port = port(), port()
        secret = secrets.token_hex(32)
        secret_file = directory / "secret"
        secret_file.write_text(secret)
        secret_file.chmod(0o600)
        base = f"http://127.0.0.1:{daemon_port}"

        def api(path, method="GET", body=None):
            request = Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"X-Browser-Cluster-Secret": secret, "Content-Type": "application/json"})
            try:
                result = HTTP.open(request, timeout=25)
            except HTTPError as error:
                result = error
            with result:
                return result.status, json.load(result)

        subprocess.run(["python3", str(root / "install.py"), "--native-directory", str(home / ".mozilla/native-messaging-hosts"), "--lib-directory", str(home / ".local/lib/browser-daemon-node")], check=True, stdout=subprocess.DEVNULL)
        with (directory / "daemon.log").open("wb") as output:
            daemon = subprocess.Popen([str(Path(args.daemon).resolve()), "run", "--bind", f"127.0.0.1:{daemon_port}", "--public-url", base, "--profile-dir", str(directory / "state/profiles"), "--recordings-dir", str(directory / "state/recordings"), "--cluster-secret-file", str(secret_file), "--record-sessions", "false"], stdout=output, stderr=output)
        firefox = None
        driver = None
        fixture = ThreadingHTTPServer(("127.0.0.2", 0), Fixture)
        fixture.api = api
        fixture.daemon_url = base
        fixture.stage = "starting"
        fixture.control = False
        threading.Thread(target=fixture.serve_forever, daemon=True).start()
        fixture_url = f"http://127.0.0.2:{fixture.server_port}"
        try:
            wait_for(lambda: api("/health")[0] == 200)
            source = directory / "extension"
            shutil.copytree(root / "dist/firefox", source)
            manifest = json.loads((source / "manifest.json").read_text())
            manifest["permissions"].append("nativeMessaging")
            manifest.pop("optional_permissions", None)
            manifest["host_permissions"] = ["http://127.0.0.2/*"]
            (source / "manifest.json").write_text(json.dumps(manifest))
            with (source / "popup.html").open("a") as html:
                html.write('<script src="fixture.js"></script>')
            (source / "fixture.js").write_text("const fixtureURL=" + json.dumps(fixture_url) + ";" + r"""
(async () => {
  const fixtureApi = browser;
  const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
  const report = value => fetch(`${fixtureURL}/stage?value=${encodeURIComponent(value)}`);
  const message = async value => { const r=await fixtureApi.runtime.sendMessage(value); if(r.error) throw Error(r.error); return r; };
  try {
    // The test driver detaches before the native host starts its own session.
    await delay(2500);
    const origin=fixtureApi.runtime.getURL('').replace(/\/$/,'');
    const setup=await fetch(`${fixtureURL}/bootstrap?origin=${encodeURIComponent(origin)}`).then(r=>r.json());
    await message({type:'setDaemonAllowlist',origins:[setup.server]});
    await message({type:'shareAll',enabled:false});
    await message({type:'connect',server:setup.server,code:setup.code,name:'Firefox roundtrip'});
    let state;
    for(let i=0;i<100;i++){state=await message({type:'state'}); if(state.registration)break; await delay(100);}
    if(!state.registration)throw Error('No registration');
    await message({type:'native',enable:true});
    const tab=state.tabs.find(t=>t.url===`${fixtureURL}/page`);
    if(!tab)throw Error('Fixture tab missing');
    let control=false;
    const share=()=>message({type:'share',tabs:[tab.id],urls:{[tab.id]:tab.url},control});
    await share(); await report('view');
    for(let i=0;i<200;i++){
      await delay(200);
      const mode=await fetch(`${fixtureURL}/mode`).then(r=>r.json());
      if(mode.control!==control){control=mode.control; await share(); await report(control?'control':'view');}
    }
  } catch (error) { await report('failed: '+String(error.message).slice(0,160)); }
})();
""")
            package = directory / "fixture.zip"
            with zipfile.ZipFile(package, "w", zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(source.rglob("*")):
                    if path.is_file():
                        archive.write(path, path.relative_to(source))
            profile = directory / "profile"
            profile.mkdir()
            (profile / "user.js").write_text('user_pref("browser.aboutwelcome.enabled", false);\n')
            environment = {**os.environ, "HOME": str(home), "MOZ_REMOTE_ALLOW_SYSTEM_ACCESS": "1", "BROWSER_DAEMON_FIREFOX_PORT": str(remote_port)}
            with (directory / "firefox.log").open("wb") as output:
                firefox = subprocess.Popen([args.firefox, "--headless", "--no-remote", "--profile", str(profile), "--remote-debugging-port", str(remote_port)], env=environment, stdout=output, stderr=output)
            def connect_driver():
                return Bidi(remote_port, {firefox.pid})
            driver = wait_for(connect_driver)
            driver.call("webExtension.install", {"extensionData": {"type": "archivePath", "path": str(package)}})
            first = driver.call("browsingContext.getTree", {"maxDepth": 0})["contexts"][0]["context"]
            driver.call("browsingContext.navigate", {"context": first, "url": fixture_url + "/page", "wait": "complete"})
            popup = driver.function("(addon) => { const {ExtensionParent} = ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs'); return ExtensionParent.GlobalManager.getExtension(addon).baseURI.resolve('popup.html'); }",
                                    [ADDON], {"context": driver.chrome_context()})
            tab = driver.call("browsingContext.create", {"type": "tab"})["context"]
            driver.call("browsingContext.navigate", {"context": tab, "url": popup})
            driver.close()
            driver = None
            wait_for(lambda: fixture.stage == "view" or fixture.stage.startswith("failed"))
            assert fixture.stage == "view", fixture.stage
            node = api("/api/extension/nodes")[1]["nodes"][0]
            tab = node["tabs"][0]["id"]

            def command(action, params=None):
                return api(f'/api/extension/nodes/{node["id"]}/command', "POST", {"tab": tab, "action": action, "params": params or {}})

            status, screenshot = command("screenshot")
            assert status == 200 and screenshot["result"]["format"] == "jpeg" and screenshot["result"]["data"].startswith("/9j"), "Native screenshot failed"
            assert command("type", {"text": "refuse"})[0] == 403
            fixture.control = True
            wait_for(lambda: fixture.stage == "control" or fixture.stage.startswith("failed"))
            assert fixture.stage == "control"
            assert command("click", {"selector": "#button"})[1]["error"] is None
            assert command("type", {"selector": "#field", "text": "hello"})[1]["error"] is None
            status, result = command("evaluate", {"script": "return {value:document.querySelector('#field').value,click:window.wrappedJSObject.trustedClick,input:window.wrappedJSObject.trustedInput,system:typeof globalThis.ChromeUtils,components:typeof Components.utils};"})
            assert status == 200 and result["result"] == {"value": "hello", "click": True, "input": True, "system": "undefined", "components": "undefined"}, (status, result)
            api(f'/api/extension/nodes/{node["id"]}', "DELETE")
            wait_for(lambda: not api("/api/extension/nodes")[1]["nodes"])
            print("Firefox round trip (WebDriver BiDi): real extension registration, native pipe identity, JPEG screenshots, trusted click/type, sandboxed content evaluation, view-only enforcement and revocation passed")
        finally:
            if driver:
                with contextlib.suppress(OSError, ValueError, EOFError):
                    driver.close()
            for process in (firefox, daemon):
                if process:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            fixture.shutdown()


if __name__ == "__main__":
    main()
