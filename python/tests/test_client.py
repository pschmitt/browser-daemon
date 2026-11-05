"""The client against a fake daemon: what it sends and how it reads the answers."""

from __future__ import annotations

import json

import aiohttp
import pytest
from aiohttp import web
from aiohttp.test_utils import TestServer

from browser_daemon_client import (
    BrowserDaemon,
    BrowserDaemonError,
    LoginResult,
    RunError,
    RunTimeout,
    read_secret,
    step,
)


@pytest.fixture
async def daemon():
    """A fake node: records every request and answers from `answers`."""
    seen: list[dict] = []
    answers: dict[tuple[str, str], tuple[int, object]] = {}

    async def handle(request: web.Request) -> web.Response:
        body = await request.text()
        seen.append(
            {
                "method": request.method,
                "path": request.rel_url.raw_path,
                "query": dict(request.query),
                "secret": request.headers.get("X-Browser-Cluster-Secret"),
                "json": json.loads(body) if body else None,
            }
        )
        status, data = answers.get(
            (request.method, request.rel_url.raw_path), (404, {"error": "nope"})
        )
        return web.json_response(data, status=status)

    app = web.Application()
    app.router.add_route("*", "/{tail:.*}", handle)
    server = TestServer(app)
    await server.start_server()
    async with aiohttp.ClientSession() as http:
        bd = BrowserDaemon(http, str(server.make_url("/")), "s3cret", client="tests")
        yield bd, seen, answers
    await server.close()


async def test_a_run_sends_only_what_is_set(daemon):
    bd, seen, answers = daemon
    answers[("POST", "/api/run")] = (200, {"ok": True, "result": {"t": "x"}, "ms": 5})
    res = await bd.run(
        profile="work",
        steps=[step.fill("#pw", arg="password"), step.text("h1", as_="t")],
        args={"password": "hunter2"},
        timeout=30,
    )
    assert res.ok and res.result == {"t": "x"} and res.ms == 5
    sent = seen[0]
    assert sent["secret"] == "s3cret"
    assert sent["json"] == {
        "profile": "work",
        "steps": [
            {"action": "fill", "selector": "#pw", "value_arg": "password"},
            {"action": "text", "selector": "h1", "as": "t"},
        ],
        "args": {"password": "hunter2"},
        "keep_open": False,
        "persistent": False,
        "timeout_seconds": 30,
        "screenshot_on_error": True,
        "client": "tests",
    }


async def test_a_failed_run_raises_with_its_answer(daemon):
    bd, _, answers = daemon
    answers[("POST", "/api/run")] = (
        422,
        {
            "ok": False,
            "error": "step 0 (click): timeout",
            "screenshot": "iVBOR",
            "url": "https://x/",
        },
    )
    with pytest.raises(RunError) as err:
        await bd.run(script="async () => 1")
    assert not isinstance(err.value, RunTimeout)
    assert str(err.value) == "step 0 (click): timeout"
    assert err.value.status == 422
    assert err.value.run.screenshot == "iVBOR"
    assert err.value.run.url == "https://x/"
    res = await bd.run(script="async () => 1", check=False)
    assert not res.ok


async def test_a_timed_out_run_raises_run_timeout(daemon):
    bd, _, answers = daemon
    answers[("POST", "/api/run")] = (504, {"ok": False, "timed_out": True, "error": "timed out"})
    with pytest.raises(RunTimeout):
        await bd.run(script="async () => 1")


async def test_a_refused_request_carries_the_daemons_message(daemon):
    bd, _, answers = daemon
    answers[("POST", "/api/run")] = (501, {"error": "Runs are not enabled on this node"})
    with pytest.raises(BrowserDaemonError) as err:
        await bd.run(script="async () => 1")
    assert not isinstance(err.value, RunError)
    assert err.value.status == 501
    assert "not enabled" in str(err.value)


async def test_requests_for_other_nodes_go_through_the_peer_route(daemon):
    bd, seen, answers = daemon
    answers[("GET", "/api/mesh/overview")] = (
        200,
        {"local_node": {"name": "here"}, "profiles": [{"name": "work", "node_name": "there"}]},
    )
    answers[("GET", "/api/peers/there/json/list")] = (200, [])
    answers[("GET", "/api/sessions")] = (200, [])
    answers[("POST", "/api/peers/there/sessions/a%2Fb/idle-timeout")] = (204, None)
    assert (await bd.find_profile("work"))["node_name"] == "there"
    assert await bd.tabs(node="there") == []
    assert await bd.sessions(node="here") == []
    await bd.set_idle_timeout("a/b", 5, node="there")
    assert [s["path"] for s in seen[1:]] == [
        "/api/peers/there/json/list",
        "/api/sessions",
        "/api/peers/there/sessions/a%2Fb/idle-timeout",
    ]
    assert seen[-1]["query"] == {"minutes": "5"}


@pytest.mark.parametrize("selected", ["here", "worker"])
async def test_group_session_routes_to_placement_and_allows_new_profiles(daemon, selected):
    bd, seen, answers = daemon
    answers[("GET", "/api/mesh/overview")] = (200, {"local_node": {"name": "here"}})
    answers[("POST", "/api/mesh/placement")] = (200, {"node": selected})
    path = "/api/sessions" if selected == "here" else "/api/peers/worker/sessions"
    answers[("POST", path)] = (201, {"id": "new-session"})
    result = await bd.create_session(profile="new-profile", group="cloud", engine="chromium")
    assert result == {"id": "new-session", "node_name": selected}
    assert seen[1]["json"] == {
        "profile": "new-profile",
        "group": "cloud",
        "engine": "chromium",
        "create": True,
    }
    assert seen[2]["path"] == path
    assert "group" not in seen[2]["json"]


async def test_refused_placement_does_not_create_a_session(daemon):
    bd, seen, answers = daemon
    bd.local_node = "here"
    answers[("POST", "/api/mesh/placement")] = (409, {"error": "profile is busy"})
    with pytest.raises(BrowserDaemonError):
        await bd.create_session(profile="work", node="worker")
    assert [item["path"] for item in seen] == ["/api/mesh/placement"]


async def test_a_session_is_named_after_the_client(daemon):
    bd, seen, answers = daemon
    answers[("POST", "/api/sessions")] = (201, {"id": "abc"})
    assert (await bd.create_session(profile="work", default_tabs=False))["id"] == "abc"
    assert seen[0]["json"] == {
        "persistent": True,
        "profile": "work",
        "default_tabs": False,
        "name": "tests",
    }


async def test_login_uses_existing_session_and_tab_without_logging_secrets(daemon):
    bd, seen, answers = daemon
    answers[("POST", "/api/sessions/session%2Fid/login")] = (
        200,
        {"outcome": "needs_totp"},
    )
    result = await bd.login(
        "session/id",
        "tab-id",
        "alice",
        "not-for-logs",
        totp_secret="JBSWY3DPEHPK3PXP",
        allowed_origins=["https://login.example.test"],
        success_selector=".account-home",
    )
    assert result == LoginResult("needs_totp")
    assert seen[0]["json"] == {
        "tab": "tab-id",
        "username": "alice",
        "password": "not-for-logs",
        "totp_secret": "JBSWY3DPEHPK3PXP",
        "allowed_origins": ["https://login.example.test"],
        "success_selector": ".account-home",
        "timeout_seconds": 30,
    }


async def test_login_refuses_both_totp_seed_and_code(daemon):
    bd, _, _ = daemon
    with pytest.raises(ValueError, match="not both"):
        await bd.login(
            "session",
            "tab",
            "alice",
            "password",
            totp_secret="seed",
            totp_code="123456",
        )


async def test_an_unreachable_daemon_is_a_browser_daemon_error():
    async with aiohttp.ClientSession() as http:
        bd = BrowserDaemon(http, "http://127.0.0.1:9", timeout=2)
        with pytest.raises(BrowserDaemonError):
            await bd.overview()


def test_the_secret_is_read_from_its_file(tmp_path):
    path = tmp_path / "secret"
    path.write_text("abc\n")
    assert read_secret(str(path)) == "abc"
    assert read_secret(str(tmp_path / "missing")) is None
