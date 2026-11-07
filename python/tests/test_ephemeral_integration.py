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
    async def fake_ws(request):
        requests.append((request.method, request.path))
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        async for msg in ws:
            if msg.type == aiohttp.WSMsgType.TEXT:
                await ws.send_str(msg.data)
        return ws
    app.router.add_get("/devtools/page/test", fake_ws)
    app.router.add_get("/chromium", fake_ws)
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
                assert result["lease_id"] and result["node_name"]
                relay = f"{base}/api/ephemeral/{result['lease_id']}"
                async with client.get(f"{relay}/proxy/api/sessions/{session_id}") as proxied:
                    assert proxied.status == 200
                async with client.get(f"{relay}/proxy/api/secret-agent/status") as forbidden:
                    assert forbidden.status == 404
                async with client.ws_connect(f"{relay}/ws/devtools/page/test?observer=true") as ws:
                    await ws.send_str("hello")
                    assert (await ws.receive()).data == "hello"
                    assert ("GET", "/chromium") in requests
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
@pytest.mark.parametrize("observer", ["true", "false"])
async def test_last_client_grace_period_allows_reconnect(tmp_path, observer):
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
            async with client.get(f"{base}/json/list") as response:
                pages = await response.json()
            page = next(item for item in pages if item["type"] == "page")
            view = await client.ws_connect(f"ws://127.0.0.1:{port}/devtools/page/{page['id']}?observer={observer}")
            await ws.close()
            await asyncio.sleep(6)
            async with client.get(f"{base}/api/sessions/{session['id']}") as response:
                assert response.status == 200
            await view.close()
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


@pytest.mark.skipif(not os.environ.get("BD_TEST_BROWSER"), reason="requires Chromium and Xvfb")
@pytest.mark.parametrize("engine", ["chromium", "firefox"])
async def test_dashboard_launches_and_closes_dedicated_worker(tmp_path, engine):
    playwright = pytest.importorskip("playwright.async_api")
    if engine == "firefox" and not shutil.which("firefox"):
        pytest.skip("requires Firefox")
    def free_port():
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            return sock.getsockname()[1]
    worker_port, coordinator_port, router_port = free_port(), free_port(), free_port()
    worker_base = f"http://127.0.0.1:{worker_port}"
    base = f"http://127.0.0.1:{coordinator_port}"
    up, down, marker = tmp_path / "up", tmp_path / "down", tmp_path / "destroyed"
    up.write_text(f"#!{sys.executable}\nimport json\nprint(json.dumps({{'url': {worker_base!r}}}))\n")
    down.write_text(f"#!{sys.executable}\nfrom pathlib import Path\nPath({str(marker)!r}).touch()\n")
    up.chmod(0o700)
    down.chmod(0o700)
    async def daemon(port, name, *extra):
        engines = ["--chromium", shutil.which("chromium"), "--xvfb", shutil.which("Xvfb")]
        if shutil.which("firefox"):
            engines += ["--firefox", shutil.which("firefox")]
        proc = await asyncio.create_subprocess_exec(
            os.environ["BD_TEST_BINARY"], "run", "--bind", f"127.0.0.1:{port}",
            "--node-name", name,
            "--profile-dir", str(tmp_path / name / "profiles"),
            "--recordings-dir", str(tmp_path / name / "recordings"),
            *engines,
            "--display-mode", "per-session", *extra,
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        async with aiohttp.ClientSession() as client:
            for _ in range(100):
                try:
                    async with client.get(f"http://127.0.0.1:{port}/api/mesh/node"):
                        return proc
                except aiohttp.ClientError:
                    await asyncio.sleep(0.1)
        proc.kill()
        await proc.wait()
        raise AssertionError("daemon did not start")
    worker = await daemon(worker_port, "worker", "--ephemeral-node")
    coordinator = await daemon(coordinator_port, "coordinator", "--ephemeral-up", str(up), "--ephemeral-down", str(down))
    router = await daemon(router_port, "router", "--peer", base)
    try:
        async with playwright.async_playwright() as pw:
            browser = await pw.chromium.launch(executable_path=shutil.which("chromium"), args=["--no-sandbox"])
            page = await browser.new_page()
            await page.goto(f"http://127.0.0.1:{router_port}")
            await page.wait_for_function("typeof nodes !== 'undefined' && nodes.some(n => n.ephemeral_provider)", timeout=30_000)
            await page.locator("#newSessionAction").click()
            await page.locator("#sessionDedicatedField").click()
            assert await page.locator("#sessionDedicated").is_checked()
            if engine == "firefox":
                await page.locator('#sessionEngines [data-engine="firefox"]').click()
            await page.locator("#sessionUrl").fill("data:text/html,ephemeral-test")
            async with page.expect_response(lambda r: "/proxy/json/new?" in r.url, timeout=120_000) as opened:
                await page.locator("#sessionSubmit").click()
            response = await opened.value
            assert response.ok, f"{response.status}: {await response.text()}"
            await page.wait_for_url("**/handoff/?ephemeral=**", timeout=120_000)
            await page.wait_for_function("document.querySelector('#dot')?.className === 'live'", timeout=60_000)
            dashboard = await browser.new_page()
            await dashboard.goto(f"http://127.0.0.1:{router_port}")
            await dashboard.locator('[data-view="nodes"]').click()
            card = dashboard.locator(".node-card").filter(has_text="Dedicated worker")
            await card.wait_for(timeout=30_000)
            assert "worker" in await card.inner_text()
            assert "ephemeral" in await card.inner_text()
            cli = await asyncio.create_subprocess_exec(
                os.environ["BD_TEST_BINARY"], "node", "list", "--url", f"http://127.0.0.1:{router_port}",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            output, error = await cli.communicate()
            assert cli.returncode == 0, error.decode()
            assert "worker" in output.decode() and "ephemeral" in output.decode()
            if engine == "firefox":
                await card.locator("[data-ephemeral-stop]").click()
            await page.close()
            for _ in range(150):
                if marker.exists():
                    break
                await asyncio.sleep(0.2)
            assert marker.exists(), "closing the live view must decommission the worker"
            await card.wait_for(state="hidden", timeout=30_000)
            await dashboard.close()
            await browser.close()
    finally:
        router.terminate()
        coordinator.terminate()
        worker.terminate()
        await router.wait()
        await coordinator.wait()
        await worker.wait()
