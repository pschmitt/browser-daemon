#!/usr/bin/env python3
"""Firefox native messaging companion. Standard library only; Linux desktop.

The extension owns the grants. This process accepts only its native pipe and
connects only to the loopback WebDriver BiDi endpoint of the *calling Firefox
process* (its Remote Agent). Remote callers cannot choose the endpoint,
browser, chrome script, sandbox or BiDi method.
"""
import base64
import hashlib
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
# Page scripts run here: a realm of the page's own principal, apart from the
# page's globals, without privileged automation helpers (BiDi sandbox).
SANDBOX = "browser-daemon-node"

# This is the only chrome script. Browser-owned tab IDs resolve to browser-owned
# BiDi contexts; no matching by URL, focus, DOM marker or page-owned data.
RESOLVE_TAB = """function(addon, tabId, origin) {
const {ExtensionParent} = ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs');
const {NavigableManager} = ChromeUtils.importESModule('chrome://remote/content/shared/NavigableManager.sys.mjs');
const extension = ExtensionParent.GlobalManager.getExtension(addon);
if (!extension) throw new Error('Extension not installed');
const tab = extension.tabManager.get(tabId);
const info = tab.convert();
if (info.incognito) throw new Error('Private tab');
const browser = tab.nativeTab.linkedBrowser;
const uri = browser.currentURI;
if (!['http', 'https'].includes(uri.scheme) || uri.prePath !== origin) throw new Error('Origin changed');
return {handle: NavigableManager.getIdForBrowser(browser), document: String(browser.browsingContext.currentWindowGlobal.innerWindowId)};
}"""


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


class WebSocket:
    """A minimal RFC 6455 client: text frames, ping/pong, close."""

    def __init__(self, port, path):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.socket.settimeout(15)
        key = base64.b64encode(os.urandom(16)).decode()
        # No Origin header: the Remote Agent refuses requests from web pages.
        self.socket.sendall((f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n"
                             f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = bytearray()
        while not head.endswith(b"\r\n\r\n"):
            part = self.socket.recv(1)
            if not part or len(head) > 8192:
                raise ValueError("Invalid WebSocket handshake")
            head.extend(part)
        lines = head.decode("latin-1").split("\r\n")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        headers = {k.strip().lower(): v.strip() for k, _, v in (line.partition(":") for line in lines[1:] if line)}
        if not lines[0].startswith("HTTP/1.1 101") or headers.get("sec-websocket-accept") != accept:
            raise ValueError("WebSocket upgrade refused")

    def _exact(self, size):
        data = bytearray()
        while len(data) < size:
            part = self.socket.recv(size - len(data))
            if not part:
                raise EOFError
            data.extend(part)
        return bytes(data)

    def _frame(self, opcode, payload):
        header = bytearray([0x80 | opcode])
        if len(payload) < 126:
            header.append(0x80 | len(payload))
        elif len(payload) < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", len(payload))
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", len(payload))
        mask = os.urandom(4)
        self.socket.sendall(bytes(header) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, text):
        self._frame(1, text.encode())

    def recv(self):
        message = bytearray()
        while True:
            first, second = self._exact(2)
            size = second & 0x7F
            if size == 126:
                size, = struct.unpack(">H", self._exact(2))
            elif size == 127:
                size, = struct.unpack(">Q", self._exact(8))
            if second & 0x80 or size + len(message) > MAX_RESPONSE:
                raise ValueError("Invalid WebSocket frame")
            payload = self._exact(size)
            opcode = first & 0x0F
            if opcode == 9:
                self._frame(10, payload)
            elif opcode == 8:
                raise EOFError
            elif opcode in (0, 1):
                message.extend(payload)
                if first & 0x80:
                    return message.decode()
            elif opcode != 10:
                raise ValueError("Unexpected WebSocket frame")

    def close(self):
        try:
            self._frame(8, b"")
        except OSError:
            pass
        self.socket.close()


def local(value):
    """A BiDi LocalValue: data only, never a remote reference or a function."""
    if value is None:
        return {"type": "null"}
    if isinstance(value, bool):
        return {"type": "boolean", "value": value}
    if isinstance(value, (int, float)):
        if not math.isfinite(value):
            raise ValueError("Expected a finite number")
        return {"type": "number", "value": value}
    if isinstance(value, str):
        return {"type": "string", "value": value}
    if isinstance(value, list):
        return {"type": "array", "value": [local(v) for v in value]}
    if isinstance(value, dict):
        return {"type": "object", "value": [[str(k), local(v)] for k, v in value.items()]}
    raise ValueError("Unsupported argument")


def remote(value):
    """Plain JSON from a serialized BiDi RemoteValue."""
    kind = value.get("type")
    if kind in ("undefined", "null"):
        return None
    if kind in ("string", "boolean"):
        return value["value"]
    if kind == "number":
        return value["value"] if isinstance(value["value"], (int, float)) else None
    if kind == "array":
        return [remote(v) for v in value.get("value", [])]
    if kind == "object":
        return {k if isinstance(k, str) else str(remote(k)): remote(v) for k, v in value.get("value", [])}
    return None


class Bidi:
    def __init__(self, port, allowed_pids):
        self.ws = WebSocket(port, "/session")
        self.sequence = 0
        session = self.call("session.new", {"capabilities": {"alwaysMatch": {"acceptInsecureCerts": False, "unhandledPromptBehavior": {"default": "ignore"}}}})
        pid = session.get("capabilities", {}).get("moz:processID")
        if pid not in allowed_pids:
            self.close()
            raise ValueError("The Remote Agent must belong to the Firefox that started this companion")
        self.chrome = None

    def call(self, method, params=None):
        self.sequence += 1
        self.ws.send(json.dumps({"id": self.sequence, "method": method, "params": params or {}}, separators=(",", ":")))
        while True:
            reply = json.loads(self.ws.recv())
            # Events are never subscribed to; anything else is not ours.
            if reply.get("id") != self.sequence:
                continue
            if reply.get("type") != "success":
                # Firefox error text may embed field values or script source.
                raise ValueError("Firefox rejected the operation")
            return reply["result"]

    def function(self, declaration, args, target):
        result = self.call("script.callFunction", {
            "functionDeclaration": declaration, "arguments": [local(a) for a in args],
            "target": target, "awaitPromise": True, "resultOwnership": "none",
            "serializationOptions": {"maxObjectDepth": 8, "maxDomDepth": 0},
        })
        if result.get("type") != "success":
            raise ValueError("Script failed")
        return remote(result["result"])

    def chrome_context(self):
        if self.chrome is None:
            tree = self.call("browsingContext.getTree", {"moz:scope": "chrome", "maxDepth": 0})
            contexts = tree.get("contexts", [])
            if not contexts:
                raise ValueError("No chrome context (is MOZ_REMOTE_ALLOW_SYSTEM_ACCESS set?)")
            self.chrome = contexts[0]["context"]
        return self.chrome

    def resolve(self, tab, origin):
        try:
            return self.function(RESOLVE_TAB, [ADDON, tab, origin], {"context": self.chrome_context()})
        except ValueError:
            # The window may have closed: find a chrome context again, once.
            self.chrome = None
            return self.function(RESOLVE_TAB, [ADDON, tab, origin], {"context": self.chrome_context()})

    def page(self, context, declaration, args=()):
        return self.function(declaration, list(args), {"context": context, "sandbox": SANDBOX})

    def actions(self, context, source):
        try:
            return self.call("input.performActions", {"context": context, "actions": [source]})
        finally:
            self.call("input.releaseActions", {"context": context})

    def close(self):
        # session.end detaches; browser.close would quit the user's browser.
        try:
            self.call("session.end")
        except (OSError, ValueError, EOFError):
            pass
        self.ws.close()


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
        identity = self.driver.resolve(tab, grant["origin"])
        if identity["document"] != grant["document"]:
            # The extension keeps the grant across navigations and sends the
            # new origin; a new document of the granted origin is accepted.
            grant["document"] = identity["document"]
        result = self.perform(identity["handle"], action, params, grant["origin"])
        # A result from a document that navigated mid-command is discarded.
        if identity != self.driver.resolve(tab, grant["origin"]) or grant["expires_at"] <= time.time():
            raise ValueError("Document changed or grant ended")
        return result

    def perform(self, context, action, params, origin):
        d = self.driver
        if action == "screenshot":
            viewport = d.page(context, "() => ({width: innerWidth, height: innerHeight})")
            shot = d.call("browsingContext.captureScreenshot", {"context": context, "origin": "viewport", "format": {"type": "image/jpeg", "quality": 0.7}})
            return {"data": shot["data"], "format": "jpeg", **viewport}
        if action == "snapshot":
            return d.page(context, "() => ({title: document.title, url: location.href, text: (document.body?.innerText || '').slice(0, 100000)})")
        if action == "evaluate":
            script = params.get("script")
            if not isinstance(script, str) or len(script) > 100000:
                raise ValueError("Expected a content script body")
            args = params.get("args", [])
            if not isinstance(args, list):
                raise ValueError("Expected an argument array")
            # The body runs in the sandbox realm, with the page's authority: never
            # with chrome privileges. The origin is checked before it runs.
            declaration = "function(origin, args) { if (location.origin !== origin) throw new Error('Origin changed'); return (function() {\n" + script + "\n}).apply(window, args); }"
            return d.page(context, declaration, [origin, args])
        if params.get("selector"):
            # The selector is an argument, never interpolated into code.
            point = d.page(context, "(selector) => { const e = document.querySelector(selector); if (!e) throw Error('Not found'); e.scrollIntoView({block: 'center'}); e.focus(); const r = e.getBoundingClientRect(); return {x: r.x + r.width / 2, y: r.y + r.height / 2}; }", [params["selector"]])
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
            d.actions(context, {"type": "pointer", "id": "browser-daemon", "parameters": {"pointerType": "mouse"}, "actions": actions})
            return {"ok": True}
        if action == "scroll":
            d.actions(context, {"type": "wheel", "id": "browser-daemon-wheel", "actions": [{"type": "scroll", "duration": 100, "origin": "viewport", "x": 0, "y": 0, "deltaX": round(finite(params["x"])), "deltaY": round(finite(params["y"]))}]})
            return {"ok": True}
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
            d.actions(context, {"type": "key", "id": "browser-daemon-keys", "actions": [{"type": "keyDown", "value": value} for value in held] + actions + [{"type": "keyUp", "value": value} for value in reversed(held)]})
        return {"ok": True}


def main():
    # Firefox supplies manifest path and add-on ID; no caller-supplied endpoint.
    if len(sys.argv) != 3 or sys.argv[2] != ADDON or not sys.platform.startswith("linux"):
        raise ValueError("Only the Firefox extension's native pipe is supported")
    port = int(os.environ.get("BROWSER_DAEMON_FIREFOX_PORT", "9222"))
    if not 0 < port < 65536:
        raise ValueError("Invalid Remote Agent port")
    driver = Bidi(port, ancestors())
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
