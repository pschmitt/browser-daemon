"""Exercise the coordinator and a fake provider with a built daemon binary."""

from __future__ import annotations

import asyncio
import os
import shutil
import socket
import sys
from pathlib import Path

import aiohttp
import pytest
from aiohttp import web
from aiohttp.test_utils import TestServer


@pytest.mark.skipif(not os.environ.get("BD_TEST_BINARY"), reason="requires a built daemon")
async def test_ephemeral_session_survives_coordinator_restart_and_cleans_up(tmp_path):
    session_id = "00000000-0000-0000-0000-000000000001"
    alive = True
    requests = []

    async def fake_node(request):
        requests.append((request.method, request.path))
        if request.method == "POST":
            body = await request.json()
            assert body["persistent"] is False
            return web.json_response({
                "id": session_id, "port": 9222, "ws_path": "devtools/browser/test",
                "profile": None, "persistent": False, "uptime_seconds": 0,
                "client_count": 0, "tabs_count": 0, "is_recording": False,
            })
        if alive:
            return web.json_response({"session": {"client_count": 1}})
        return web.Response(status=404)

    app = web.Application()
    app.router.add_route("*", "/api/sessions{tail:.*}", fake_node)
    node = TestServer(app)
    await node.start_server()
    up = tmp_path / "up"
    node_url = str(node.make_url("/")).rstrip("/")
    up.write_text(f"#!{sys.executable}\nimport json\nprint(json.dumps({{'url': {node_url!r}}}))\n")
    down = tmp_path / "down"
    marker = tmp_path / "destroyed"
    down.write_text(
        f"#!{sys.executable}\nfrom pathlib import Path\nimport sys\n"
        f"Path({str(marker)!r}).write_text(sys.argv[1])\n"
    )
    up.chmod(0o700)
    down.chmod(0o700)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = f"http://127.0.0.1:{port}"

    async def start():
        proc = await asyncio.create_subprocess_exec(
            os.environ["BD_TEST_BINARY"], "run", "--bind", f"127.0.0.1:{port}",
            "--profile-dir", str(tmp_path / "profiles"),
            "--recordings-dir", str(tmp_path / "recordings"),
            "--ephemeral-up", str(up), "--ephemeral-down", str(down),
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        for _ in range(100):
            try:
                async with aiohttp.ClientSession() as client:
                    async with client.get(f"{base}/api/mesh/node"):
                        return proc
            except aiohttp.ClientError:
                await asyncio.sleep(0.1)
        proc.kill()
        await proc.wait()
        raise AssertionError("daemon did not start")

    proc = await start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        proxy_port = sock.getsockname()[1]
    proxy_base = f"http://127.0.0.1:{proxy_port}"
    proxy = await asyncio.create_subprocess_exec(
        os.environ["BD_TEST_BINARY"], "run", "--bind", f"127.0.0.1:{proxy_port}",
        "--profile-dir", str(tmp_path / "proxy" / "profiles"),
        "--recordings-dir", str(tmp_path / "proxy" / "recordings"), "--peer", base,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        async with aiohttp.ClientSession() as client:
            for _ in range(100):
                try:
                    async with client.get(f"{proxy_base}/api/mesh/node"):
                        break
                except aiohttp.ClientError:
                    await asyncio.sleep(0.1)
            async with client.post(
                f"{proxy_base}/api/ephemeral/sessions", json={"persistent": True}
            ) as response:
                assert response.status == 200
                result = await response.json()
                assert result["session"]["id"] == session_id
        proc.terminate()
        await proc.wait()
        proc = await start()
        alive = False
        for _ in range(100):
            if marker.exists():
                break
            await asyncio.sleep(0.1)
        assert marker.exists()
        assert marker.read_text()
        assert ("POST", "/api/sessions") in requests
        marker.unlink()
        up.write_text(f"#!{sys.executable}\nimport sys\nsys.exit(1)\n")
        async with aiohttp.ClientSession() as client:
            async with client.post(f"{base}/api/ephemeral/sessions", json={}) as response:
                assert response.status == 502
        for _ in range(100):
            if marker.exists():
                break
            await asyncio.sleep(0.1)
        assert marker.exists(), "failed provisioning must also invoke teardown"
    finally:
        proxy.terminate()
        await proxy.wait()
        proc.terminate()
        await proc.wait()
        await node.close()


@pytest.mark.skipif(
    not os.environ.get("BD_TEST_BROWSER"), reason="requires Chromium and Xvfb"
)
async def test_last_client_grace_period_allows_reconnect(tmp_path):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    proc = await asyncio.create_subprocess_exec(
        os.environ["BD_TEST_BINARY"], "run", "--bind", f"127.0.0.1:{port}",
        "--profile-dir", str(tmp_path / "profiles"),
        "--recordings-dir", str(tmp_path / "recordings"),
        "--chromium", shutil.which("chromium"), "--display-mode", "per-session",
        "--ephemeral-node",
        "--xvfb", shutil.which("Xvfb"),
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        async with aiohttp.ClientSession() as client:
            for _ in range(100):
                try:
                    async with client.get(f"{base}/api/mesh/node"):
                        break
                except aiohttp.ClientError:
                    await asyncio.sleep(0.1)
            async with client.post(f"{base}/api/sessions", json={"persistent": True}) as response:
                assert response.status == 201
                session = await response.json()
                assert session["persistent"] is False
            async with client.post(f"{base}/api/sessions", json={}) as response:
                assert response.status == 409
            url = f"ws://127.0.0.1:{port}/chromium?session_id={session['id']}"
            ws = await client.ws_connect(url)
            await ws.close()
            await asyncio.sleep(1)
            ws = await client.ws_connect(url)
            await asyncio.sleep(5)
            async with client.get(f"{base}/api/sessions/{session['id']}") as response:
                assert response.status == 200
            await ws.close()
            for _ in range(150):
                await asyncio.sleep(0.1)
                async with client.get(f"{base}/api/sessions/{session['id']}") as response:
                    if response.status == 404:
                        break
            else:
                pytest.fail("session did not close after its final client disconnected")
    finally:
        proc.terminate()
        await proc.wait()
