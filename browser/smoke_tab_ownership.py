#!/usr/bin/env python3
"""Exercise tab ownership with two real clients and both browser engines.

Requires the Python websockets package and an already-built browser-daemon.
All profiles and the daemon's temporary files live in a private directory.
"""

import argparse
import asyncio
import json
import os
import socket
import subprocess
import tempfile
import time
import urllib.request
import uuid
import websockets

BASE = ""
PORT = 0
FAKE_MCP = ""


def api(path, payload=None):
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(
        BASE + path, data=data, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.load(response)


class Client:
    def __init__(self, ws):
        self.ws = ws
        self.pending = {}
        self.next = 0
        self.task = asyncio.create_task(self.read())

    async def read(self):
        async for text in self.ws:
            msg = json.loads(text)
            if msg.get("method") == "Target.attachedToTarget":
                # Auto-attached Chromium pages can wait for each debugger. A
                # real Playwright client resumes them before using popups.
                await self.ws.send(
                    json.dumps(
                        {
                            "id": 1000000,
                            "method": "Runtime.runIfWaitingForDebugger",
                            "sessionId": msg["params"]["sessionId"],
                        }
                    )
                )
            if "id" in msg and msg["id"] in self.pending:
                self.pending.pop(msg["id"]).set_result(msg)

    async def call(self, method, params=None, session=None):
        self.next += 1
        msg = {"id": self.next, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        fut = asyncio.get_running_loop().create_future()
        self.pending[self.next] = fut
        await self.ws.send(json.dumps(msg))
        reply = await asyncio.wait_for(fut, 30)
        if "error" in reply:
            raise RuntimeError(reply)
        return reply["result"]

    async def close(self):
        await self.ws.close()
        try:
            await self.task
        except websockets.exceptions.ConnectionClosed:
            pass


async def until(predicate):
    for _ in range(120):
        tabs = api("/json/list")
        if predicate(tabs):
            return tabs
        await asyncio.sleep(0.25)
    raise AssertionError(json.dumps(api("/json/list"), indent=2))


def owned(tabs, tab, name):
    return any(
        t["id"] == tab and (t.get("owner") or {}).get("agent_name") == name
        for t in tabs
    )


async def check(engine):
    api("/api/profiles", {"name": engine, "engine": engine})
    path = "chromium" if engine == "chromium" else "firefox"

    # Select before either browser client connects, as in the profile race.
    mcp = None
    if engine == "chromium":
        env = dict(os.environ)
        env.pop("BROWSER_DAEMON_SECRET_FILE", None)
        mcp = await asyncio.create_subprocess_exec(
            options.binary,
            "mcp",
            "--url",
            BASE,
            "--allow-profile",
            engine,
            "--playwright-cmd",
            FAKE_MCP,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
            env=env,
        )
        await mcp_call(
            mcp,
            1,
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "clientInfo": {"name": "test", "version": "1"},
            },
        )
        selected = await mcp_call(
            mcp,
            2,
            "tools/call",
            {"name": "browser_use", "arguments": {"profile": engine}},
        )
        assert "Warning:" not in selected["result"]["content"][0]["text"]

    async def connect(name):
        ws = await websockets.connect(
            f"ws://127.0.0.1:{PORT}/{path}?profile={engine}&persistent=true&agent={name}",
            user_agent_header="Playwright/1.0",
        )
        return Client(ws)

    a, b = await asyncio.gather(connect("AgentA"), connect("AgentB"))
    if mcp:
        for _ in range(40):
            if any(len(s["clients"]) >= 2 for s in api("/api/sessions")):
                break
            await asyncio.sleep(0.1)
        else:
            raise AssertionError("Both clients did not register")
        result = await mcp_call(
            mcp, 3, "tools/call", {"name": "browser_snapshot", "arguments": {}}
        )
        assert any(
            "Warning:" in part.get("text", "")
            and "AgentA" in part["text"]
            and "AgentB" in part["text"]
            for part in result["result"]["content"]
        ), result
    if engine == "chromium":
        await a.call(
            "Target.setAutoAttach",
            {"autoAttach": True, "waitForDebuggerOnStart": False, "flatten": True},
        )
        await b.call(
            "Target.setAutoAttach",
            {"autoAttach": True, "waitForDebuggerOnStart": False, "flatten": True},
        )
        parent = (await a.call("Target.createTarget", {"url": "about:blank"}))[
            "targetId"
        ]
        other = (await b.call("Target.createTarget", {"url": "about:blank"}))[
            "targetId"
        ]
        session = (
            await a.call("Target.attachToTarget", {"targetId": parent, "flatten": True})
        )["sessionId"]
        await a.ws.send(
            json.dumps(
                {
                    "id": 999,
                    "method": "Runtime.evaluate",
                    "sessionId": session,
                    "params": {
                        "expression": "window.open('about:blank')",
                        "userGesture": True,
                    },
                }
            )
        )
    else:
        parent = (await a.call("browsingContext.create", {"type": "tab"}))["context"]
        other = (await b.call("browsingContext.create", {"type": "tab"}))["context"]
        await a.call(
            "script.evaluate",
            {
                "expression": "window.open('about:blank')",
                "target": {"context": parent},
                "awaitPromise": False,
                "userActivation": True,
            },
        )
    tabs = await until(
        lambda ts: owned(ts, parent, "AgentA")
        and owned(ts, other, "AgentB")
        and any(
            t["id"] not in (parent, other)
            and t.get("owner", {}).get("agent_name") == "AgentA"
            for t in ts
        )
    )
    popup = next(
        t["id"]
        for t in tabs
        if t["id"] not in (parent, other)
        and t.get("owner", {}).get("agent_name") == "AgentA"
    )
    if mcp:
        result = await mcp_call(
            mcp, 4, "tools/call", {"name": "browser_profile_tabs", "arguments": {}}
        )
        reported = json.loads(result["result"]["content"][0]["text"])["tabs"]
        assert (
            owned(reported, parent, "AgentA")
            and owned(reported, other, "AgentB")
            and owned(reported, popup, "AgentA")
        ), reported
        result = await mcp_call(
            mcp, 5, "tools/call", {"name": "browser_tabs", "arguments": {}}
        )
        assert any(
            "Profile tabs and owners" in part.get("text", "")
            and "AgentA" in part["text"]
            and "AgentB" in part["text"]
            for part in result["result"]["content"]
        ), result
    if engine == "chromium":
        popup_session = (
            await a.call("Target.attachToTarget", {"targetId": popup, "flatten": True})
        )["sessionId"]
        await a.ws.send(
            json.dumps(
                {
                    "id": 998,
                    "method": "Runtime.evaluate",
                    "sessionId": popup_session,
                    "params": {
                        "expression": "window.open('about:blank')",
                        "userGesture": True,
                    },
                }
            )
        )
    else:
        await a.call(
            "script.evaluate",
            {
                "expression": "window.open('about:blank')",
                "target": {"context": popup},
                "awaitPromise": False,
                "userActivation": True,
            },
        )
    tabs = await until(
        lambda ts: sum(t.get("owner", {}).get("agent_name") == "AgentA" for t in ts)
        >= 3
    )
    nested = next(
        t["id"]
        for t in tabs
        if t["id"] not in (parent, other, popup)
        and t.get("owner", {}).get("agent_name") == "AgentA"
    )
    old_ids = {t["id"] for t in tabs}
    if engine == "chromium":
        other_session = (
            await b.call("Target.attachToTarget", {"targetId": other, "flatten": True})
        )["sessionId"]
        await b.ws.send(
            json.dumps(
                {
                    "id": 997,
                    "method": "Runtime.evaluate",
                    "sessionId": other_session,
                    "params": {
                        "expression": "window.open('about:blank','_blank','noopener')",
                        "userGesture": True,
                    },
                }
            )
        )
    else:
        await b.call(
            "script.evaluate",
            {
                "expression": "window.open('about:blank','_blank','noopener')",
                "target": {"context": other},
                "awaitPromise": False,
                "userActivation": True,
            },
        )
    tabs = await until(
        lambda ts: any(t["id"] not in old_ids and t["type"] == "page" for t in ts)
    )
    isolated = next(t for t in tabs if t["id"] not in old_ids and t["type"] == "page")
    assert isolated.get("owner", {}).get("agent_name") in (None, "AgentB"), isolated
    if engine == "chromium":
        await a.call("Target.closeTarget", {"targetId": parent})
    else:
        await a.call("browsingContext.close", {"context": parent})
    await until(
        lambda ts: not any(t["id"] == parent for t in ts) and owned(ts, popup, "AgentA")
    )
    if engine == "chromium":
        await a.call("Target.closeTarget", {"targetId": popup})
    else:
        await a.call("browsingContext.close", {"context": popup})
    await until(
        lambda ts: not any(t["id"] == popup for t in ts) and owned(ts, nested, "AgentA")
    )
    await a.close()
    await until(
        lambda ts: any(t["id"] == nested and not t.get("owner") for t in ts)
        and owned(ts, other, "AgentB")
    )
    if mcp:
        result = await mcp_call(
            mcp, 6, "tools/call", {"name": "browser_profile_tabs", "arguments": {}}
        )
        reported = json.loads(result["result"]["content"][0]["text"])["tabs"]
        assert next(t for t in reported if t["id"] == nested)["owner"] is None
        assert owned(reported, other, "AgentB")
        mcp.terminate()
        await mcp.wait()
    await b.close()
    print(
        engine,
        "concurrent clients, nested and noopener popups, opener close, disconnect: OK",
        flush=True,
    )


async def main():
    for engine in ("chromium", "firefox"):
        await check(engine)


async def mcp_call(proc, request_id, method, params):
    proc.stdin.write(
        (
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
            )
            + "\n"
        ).encode()
    )
    await proc.stdin.drain()
    return json.loads(await asyncio.wait_for(proc.stdout.readline(), 30))


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", default="target/debug/browser-daemon")
parser.add_argument("--chromium", default="chromium")
parser.add_argument("--firefox", default="firefox")
parser.add_argument("--xvfb", default="Xvfb")
options = parser.parse_args()
with socket.socket() as probe:
    probe.bind(("127.0.0.1", 0))
    PORT = probe.getsockname()[1]
BASE = f"http://127.0.0.1:{PORT}"
node = "ownership-test-" + uuid.uuid4().hex
with tempfile.TemporaryDirectory(prefix="browser-ownership-") as tmp:
    FAKE_MCP = tmp + "/fake-mcp.py"
    with open(FAKE_MCP, "w") as fake:
        fake.write("""#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg: continue
    if msg["method"] == "tools/list": result = {"tools": []}
    elif msg["method"] == "tools/call": result = {"content": [{"type": "text", "text": "ok"}]}
    else: result = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
""")
    os.chmod(FAKE_MCP, 0o700)
    args = [
        options.binary,
        "run",
        "--bind",
        f"127.0.0.1:{PORT}",
        "--public-url",
        BASE,
        "--node-name",
        node,
        "--profile-dir",
        tmp + "/profiles",
        "--recordings-dir",
        tmp + "/recordings",
        "--record-sessions",
        "false",
        "--adblock-default",
        "false",
        "--display-mode",
        "per-session",
        "--xvfb",
        options.xvfb,
        "--chromium",
        options.chromium,
        "--firefox",
        options.firefox,
    ]
    with open(tmp + "/daemon.log", "w+") as log:
        proc = subprocess.Popen(
            args,
            stdout=log,
            stderr=subprocess.STDOUT,
            env={**os.environ, "TMPDIR": tmp},
        )
        ready = False
        try:
            for _ in range(100):
                try:
                    if api("/api/mesh/node")["name"] == node and proc.poll() is None:
                        ready = True
                        break
                except Exception:
                    if proc.poll() is not None:
                        raise RuntimeError("daemon exited")
                    time.sleep(0.1)
            if not ready:
                raise RuntimeError("test daemon did not start")
            asyncio.run(main())
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read()[-8000:])
            raise
        finally:
            try:
                if ready and proc.poll() is None:
                    for session in api("/api/sessions"):
                        req = urllib.request.Request(
                            BASE + "/api/sessions/" + session["id"], method="DELETE"
                        )
                        with urllib.request.urlopen(req, timeout=60):
                            pass
            finally:
                if proc.poll() is None:
                    proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
