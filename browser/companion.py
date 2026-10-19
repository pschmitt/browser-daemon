#!/usr/bin/env python3
"""Firefox native messaging companion. Standard library only; Linux desktop.

The extension owns the grants. This process accepts only its native pipe and
connects only to loopback Marionette in the *calling Firefox process*. Remote
callers cannot choose the endpoint, browser, chrome script, or WebDriver method.
"""
import json
import math
import os
from pathlib import Path
import socket
import struct
import sys
import time

ADDON = "browser-daemon-node@pschmitt.dev"
MAX_MESSAGE = 128 * 1024
MAX_RESPONSE = 8 * 1024 * 1024

# This is the only chrome script. Browser-owned tab IDs resolve to browser-owned
# WebDriver handles; no matching by URL, focus, DOM marker or page-owned data.
RESOLVE_TAB = """
const {ExtensionParent} = ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs');
const {NavigableManager} = ChromeUtils.importESModule('chrome://remote/content/shared/NavigableManager.sys.mjs');
const extension = ExtensionParent.GlobalManager.getExtension(arguments[0]);
if (!extension) throw new Error('Extension not installed');
const tab = extension.tabManager.get(arguments[1]);
const info = tab.convert();
if (info.incognito) throw new Error('Private tab');
const browser = tab.nativeTab.linkedBrowser;
const uri = browser.currentURI;
if (!['http', 'https'].includes(uri.scheme) || uri.prePath !== arguments[2]) throw new Error('Origin changed');
return {handle: NavigableManager.getIdForBrowser(browser), document: String(browser.browsingContext.currentWindowGlobal.innerWindowId)};
"""

# Marionette's default content sandbox includes extra automation globals. Never
# execute remote source there: make a fresh realm with the page's own principal,
# no Components/ChromeUtils, and data-only cloned arguments. Only this fixed
# wrapper runs in the system sandbox; remote source stays an argument until the
# content-principal eval. The caller cannot supply the principal or options.
CONTENT_EVALUATE = """
const utils = Components.utils;
const principal = utils.getObjectPrincipal(window);
if (principal.isSystemPrincipal || principal.originNoSuffix !== arguments[2]) throw new Error('Invalid content principal');
const scope = new utils.Sandbox(principal, {sandboxPrototype:window, wantComponents:false, wantXrays:true});
scope.__browserDaemonArguments = utils.cloneInto(arguments[1], scope);
try {
  const result = utils.evalInSandbox('(function(){\\n'+arguments[0]+'\\n}).apply(window,__browserDaemonArguments)', scope);
  return utils.cloneInto(result, globalThis);
} finally {
  utils.nukeSandbox(scope);
}
"""


def exact(stream, size):
    data = bytearray()
    while len(data) < size:
        part = stream.read(size - len(data))
        if not part:
            raise EOFError
        data.extend(part)
    return bytes(data)


def read_native(stream):
    size, = struct.unpack("<I", exact(stream, 4))
    if not 0 < size <= MAX_MESSAGE:
        raise ValueError("Native request is too large")
    return json.loads(exact(stream, size))


def write_native(stream, value):
    data = json.dumps(value, separators=(",", ":")).encode()
    if len(data) > 1024 * 1024:
        raise ValueError("Native message is too large")
    stream.write(struct.pack("<I", len(data)) + data)
    stream.flush()


def response(stream, request, value):
    data = json.dumps(value, separators=(",", ":"), ensure_ascii=True)
    if len(data) > MAX_RESPONSE:
        data = json.dumps({"error": "Response is too large"})
    for start in range(0, len(data), 128 * 1024):
        end = start + 128 * 1024
        write_native(stream, {"id": request, "chunk": data[start:end], "done": end >= len(data)})


def ancestors():
    result = set()
    pid = os.getppid()
    for _ in range(32):
        if pid <= 1 or pid in result:
            break
        result.add(pid)
        status = Path(f"/proc/{pid}/status").read_text()
        pid = int(next(line.split()[1] for line in status.splitlines() if line.startswith("PPid:")))
    return result


class Marionette:
    def __init__(self, port, allowed_pids):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.socket.settimeout(15)
        self.stream = self.socket.makefile("rwb", buffering=0)
        self.sequence = 0
        if self.read().get("marionetteProtocol") != 3:
            raise ValueError("Unsupported Marionette protocol")
        session = self.call("WebDriver:NewSession", {"acceptInsecureCerts": False, "unhandledPromptBehavior": "ignore"})
        pid = session.get("capabilities", {}).get("moz:processID")
        if pid not in allowed_pids:
            self.close()
            raise ValueError("Marionette must belong to the Firefox that started this companion")
        self.call("WebDriver:SetTimeouts", {"script": 10000, "pageLoad": 10000, "implicit": 0})

    def read(self):
        prefix = bytearray()
        while True:
            part = exact(self.stream, 1)
            if part == b":":
                break
            if not part.isdigit() or len(prefix) >= 9:
                raise ValueError("Invalid Marionette frame")
            prefix.extend(part)
        size = int(prefix)
        if not 0 < size <= MAX_RESPONSE:
            raise ValueError("Marionette response too large")
        return json.loads(exact(self.stream, size))

    def call(self, method, params=None):
        self.sequence += 1
        payload = json.dumps([0, self.sequence, method, params or {}], separators=(",", ":")).encode()
        self.socket.sendall(str(len(payload)).encode() + b":" + payload)
        reply = self.read()
        if not isinstance(reply, list) or len(reply) != 4 or reply[:2] != [1, self.sequence] or reply[2] is not None:
            # Firefox error text may embed field values or script source.
            raise ValueError("Firefox rejected the operation")
        return reply[3]

    def script(self, script, args=None):
        result = self.call("WebDriver:ExecuteScript", {"script": script, "args": args or [], "newSandbox": True, "sandbox": "default"})
        return result.get("value") if isinstance(result, dict) and "value" in result else result

    def content_script(self, script, args, origin):
        result = self.call("WebDriver:ExecuteScript", {"script": CONTENT_EVALUATE, "args": [script, args, origin], "newSandbox": True, "sandbox": "system"})
        return result.get("value") if isinstance(result, dict) and "value" in result else result

    def resolve(self, tab, origin):
        self.call("Marionette:SetContext", {"value": "chrome"})
        return self.script(RESOLVE_TAB, [ADDON, tab, origin])

    def select(self, tab, origin):
        identity = self.resolve(tab, origin)
        self.call("Marionette:SetContext", {"value": "content"})
        self.call("WebDriver:SwitchToWindow", {"handle": identity["handle"], "focus": False})
        # Resolve again after switching, so a navigating or replaced document
        # never inherits a command prepared for its predecessor.
        if identity != self.resolve(tab, origin):
            raise ValueError("Tab document changed")
        self.call("Marionette:SetContext", {"value": "content"})
        return identity

    def actions(self, kind, actions):
        source = {"type": kind, "id": "browser-daemon", "actions": actions}
        if kind == "pointer":
            source["parameters"] = {"pointerType": "mouse"}
        try:
            return self.call("WebDriver:PerformActions", {"actions": [source]})
        finally:
            self.call("WebDriver:ReleaseActions")

    def close(self):
        # DeleteSession detaches; Quit would close the user's browser.
        try:
            self.call("WebDriver:DeleteSession")
        except (OSError, ValueError, EOFError):
            pass
        self.stream.close()
        self.socket.close()


def finite(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError("Expected a finite coordinate")
    return value


class Companion:
    def __init__(self, driver):
        self.driver = driver
        self.grants = {}

    def update(self, grants):
        self.grants = {}
        if not isinstance(grants, list) or len(grants) > 128:
            raise ValueError("Invalid grants")
        updated = {}
        for grant in grants:
            if not isinstance(grant["tab"], int) or isinstance(grant["tab"], bool):
                raise ValueError("Invalid tab")
            if not isinstance(grant["origin"], str) or not grant["origin"].startswith(("http://", "https://")):
                raise ValueError("Invalid origin")
            expires = finite(grant["expires_at"])
            if expires <= time.time() or expires > time.time() + 28800:
                raise ValueError("Invalid expiry")
            identity = self.driver.resolve(grant["tab"], grant["origin"])
            updated[grant["tab"]] = {**grant, "document": identity["document"]}
        self.grants = updated
        return {"ok": True}

    def execute(self, command):
        tab = command.get("tab")
        action = command.get("action")
        grant = self.grants.get(tab)
        if not grant or grant["expires_at"] <= time.time():
            raise ValueError("No active tab grant")
        if action not in ("snapshot", "screenshot") and grant.get("control") is not True:
            raise ValueError("Control not granted")
        if action not in ("snapshot", "screenshot", "click", "type", "key", "scroll", "evaluate"):
            raise ValueError("Action not supported")
        params = command.get("params", {})
        if not isinstance(params, dict):
            raise ValueError("Invalid parameters")
        identity = self.driver.select(tab, grant["origin"])
        if identity["document"] != grant["document"]:
            self.grants.pop(tab, None)
            raise ValueError("Document changed; share the tab again")
        result = self.perform(action, params, grant["origin"])
        # A result from a document that navigated mid-command is discarded.
        if identity != self.driver.resolve(tab, grant["origin"]) or grant["expires_at"] <= time.time():
            self.grants.pop(tab, None)
            raise ValueError("Grant ended")
        return result

    def perform(self, action, params, origin):
        d = self.driver
        if action == "screenshot":
            viewport = d.script("return {width: innerWidth, height: innerHeight};")
            data = d.call("WebDriver:TakeScreenshot", {"full": False, "scroll": False})
            if isinstance(data, dict):
                data = data["value"]
            return {"data": data, "format": "png", **viewport}
        if action == "snapshot":
            return d.script("return {title: document.title, url: location.href, text: (document.body?.innerText || '').slice(0,100000)};")
        if action == "evaluate":
            script = params.get("script")
            if not isinstance(script, str) or len(script) > 100000:
                raise ValueError("Expected a content script body")
            args = params.get("args", [])
            if not isinstance(args, list):
                raise ValueError("Expected an argument array")
            return d.content_script(script, args, origin)
        if params.get("selector"):
            # The selector is an argument, never interpolated into code.
            point = d.script("const e=document.querySelector(arguments[0]); if(!e) throw Error('Not found'); e.scrollIntoView({block:'center'}); e.focus(); const r=e.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2};", [params["selector"]])
            params = {**params, **point}
        if action == "click":
            button = params.get("button", 0)
            if type(button) is not int or button not in (0, 1, 2):
                raise ValueError("Invalid pointer button")
            path = params.get("path", [])
            if not isinstance(path, list) or len(path) > 128:
                raise ValueError("Invalid pointer path")
            points = [{"x": round(finite(point["x"])), "y": round(finite(point["y"]))} for point in path]
            actions = [{"type": "pointerMove", "duration": 0, "origin": "viewport", "x": round(finite(params["x"])), "y": round(finite(params["y"]))}, {"type": "pointerDown", "button": button}]
            actions.extend({"type": "pointerMove", "duration": 16, "origin": "viewport", **point} for point in points)
            actions.append({"type": "pointerUp", "button": button})
            return d.actions("pointer", actions)
        if action == "scroll":
            return d.actions("wheel", [{"type": "scroll", "duration": 100, "x": 0, "y": 0, "deltaX": round(finite(params["x"])), "deltaY": round(finite(params["y"]))}])
        keys = {"Enter": "\ue007", "Tab": "\ue004", "Escape": "\ue00c", "Backspace": "\ue003", "Delete": "\ue017", "ArrowLeft": "\ue012", "ArrowRight": "\ue014", "ArrowUp": "\ue013", "ArrowDown": "\ue015", "Home": "\ue011", "End": "\ue010", "PageUp": "\ue00e", "PageDown": "\ue00f"}
        if action == "key":
            key = params.get("key")
            if key not in keys and (not isinstance(key, str) or len(key) != 1):
                raise ValueError("Invalid key")
            text = keys.get(key, key)
        else:
            text = params.get("text")
        if not isinstance(text, str) or len(text) > 4096:
            raise ValueError("Invalid text")
        modifiers = params.get("modifiers", {}) if action == "key" else {}
        if not isinstance(modifiers, dict) or any(k not in ("ctrl", "shift", "alt", "meta") or type(v) is not bool for k, v in modifiers.items()):
            raise ValueError("Invalid key modifiers")
        held = [value for name, value in (("ctrl", "\ue009"), ("shift", "\ue008"), ("alt", "\ue00a"), ("meta", "\ue03d")) if modifiers.get(name)]
        for start in range(0, len(text), 64):
            actions = [event for character in text[start:start + 64] for event in ({"type": "keyDown", "value": character}, {"type": "keyUp", "value": character})]
            d.actions("key", [{"type": "keyDown", "value": value} for value in held] + actions + [{"type": "keyUp", "value": value} for value in reversed(held)])
        return {"ok": True}


def main():
    # Firefox supplies manifest path and add-on ID; no caller-supplied endpoint.
    if len(sys.argv) != 3 or sys.argv[2] != ADDON or not sys.platform.startswith("linux"):
        raise ValueError("Only the Firefox extension's native pipe is supported")
    port = int(os.environ.get("BROWSER_DAEMON_FIREFOX_PORT", "2828"))
    if not 0 < port < 65536:
        raise ValueError("Invalid Marionette port")
    driver = Marionette(port, ancestors())
    companion = Companion(driver)
    write_native(sys.stdout.buffer, {"type": "ready"})
    try:
        while True:
            try:
                message = read_native(sys.stdin.buffer)
            except EOFError:
                break
            try:
                result = companion.update(message["grants"]) if message.get("type") == "grants" else companion.execute(message)
                response(sys.stdout.buffer, message["id"], {"result": result})
            except (ValueError, KeyError, TypeError, OSError, EOFError):
                response(sys.stdout.buffer, message.get("id"), {"error": "Native action refused or failed"})
    finally:
        driver.close()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, EOFError):
        # Never print exception text: it may contain script or page data.
        print("Firefox companion could not connect securely; check native host installation and Firefox startup flags.", file=sys.stderr)
        sys.exit(1)
