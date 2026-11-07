"""Async client for browser-daemon's HTTP API, with nothing but aiohttp.

Made for Home Assistant integrations and other code that cannot carry
Playwright: :meth:`BrowserDaemon.run` runs a Playwright script or a list of
steps on the node (see docs/run.md), and the other methods cover sessions,
tabs and a page's login.

    async with aiohttp.ClientSession() as http:
        bd = BrowserDaemon(http, "https://browser.example.org", secret)
        res = await bd.run(
            profile="work",
            url="https://example.org/login",
            steps=[
                step.fill("#user", arg="user"),
                step.fill("#password", arg="password"),
                step.click("role=button[name='Sign in']"),
                step.text(".balance", as_="balance"),
            ],
            args={"user": user, "password": password},
        )
        print(res.result["balance"])
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from urllib.parse import quote

import aiohttp

__all__ = [
    "BrowserDaemon",
    "BrowserDaemonError",
    "LoginResult",
    "RunError",
    "RunResult",
    "RunTimeout",
    "read_secret",
    "step",
]

SECRET_HEADER = "X-Browser-Cluster-Secret"
DEFAULT_SECRET_FILE = "/run/secrets/browser-daemon/cluster-secret"


class BrowserDaemonError(Exception):
    """The daemon refused a request or could not be reached."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.status = status


@dataclass
class RunResult:
    """The answer of a run (see docs/run.md, "The answer")."""

    ok: bool
    result: Any = None
    error: str | None = None
    timed_out: bool = False
    steps: list[dict[str, Any]] = field(default_factory=list)
    logs: list[dict[str, Any]] = field(default_factory=list)
    url: str | None = None
    page: str | None = None
    #: The tab when the run failed: a base64 PNG.
    screenshot: str | None = None
    ms: int | None = None

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> RunResult:
        return cls(
            ok=bool(data.get("ok")),
            result=data.get("result"),
            error=data.get("error"),
            timed_out=bool(data.get("timed_out")),
            steps=data.get("steps") or [],
            logs=data.get("logs") or [],
            url=data.get("url"),
            page=data.get("page"),
            screenshot=data.get("screenshot"),
            ms=data.get("ms"),
        )


@dataclass
class LoginResult:
    """Outcome of a generic login attempt in an existing session tab."""

    outcome: str

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> LoginResult:
        return cls(outcome=str(data.get("outcome", "unknown")))


class RunError(BrowserDaemonError):
    """The script or a step failed; :attr:`run` holds the whole answer."""

    def __init__(self, run: RunResult, status: int | None = None) -> None:
        super().__init__(run.error or "The run failed", status)
        self.run = run


class RunTimeout(RunError):
    """The run hit its timeout."""


def read_secret(path: str = DEFAULT_SECRET_FILE) -> str | None:
    """The cluster secret from a file, or None when it cannot be read."""
    try:
        with open(path, encoding="utf-8") as f:
            return f.read().strip() or None
    except OSError:
        return None


class BrowserDaemon:
    """A node of the mesh. Requests for other nodes go through it (`node=`)."""

    def __init__(
        self,
        session: aiohttp.ClientSession,
        url: str,
        secret: str | None = None,
        *,
        client: str | None = None,
        timeout: float = 30,
    ) -> None:
        self.session = session
        self.url = url.rstrip("/")
        self.secret = secret
        #: Shown on the dashboard for sessions and runs ("Home Assistant · HR WORKS").
        self.client = client
        self.timeout = timeout
        self.local_node: str | None = None

    async def _request(
        self,
        method: str,
        path: str,
        *,
        node: str | None = None,
        timeout: float | None = None,
        accept: tuple[int, ...] = (),
        **kwargs: Any,
    ) -> tuple[int, Any]:
        if node and node != self.local_node:
            path = f"/api/peers/{quote(node, safe='')}/{path.removeprefix('/api/').lstrip('/')}"
        headers = {SECRET_HEADER: self.secret} if self.secret else {}
        try:
            async with self.session.request(
                method,
                self.url + path,
                headers=headers,
                timeout=aiohttp.ClientTimeout(total=timeout or self.timeout),
                **kwargs,
            ) as res:
                data = await res.json(content_type=None) if res.status != 204 else None
                if res.status < 300 or res.status in accept:
                    return res.status, data
                message = data.get("error") if isinstance(data, dict) else None
                raise BrowserDaemonError(
                    message or f"browser-daemon returned HTTP {res.status}", res.status
                )
        except (aiohttp.ClientError, TimeoutError) as err:
            raise BrowserDaemonError(f"browser-daemon is unreachable: {err!r}") from err
        except ValueError as err:
            raise BrowserDaemonError("browser-daemon sent an unreadable answer") from err

    async def _json(self, method: str, path: str, **kwargs: Any) -> Any:
        return (await self._request(method, path, **kwargs))[1]

    # The mesh

    async def overview(self) -> dict[str, Any]:
        """Nodes, sessions, profiles and tabs of the whole mesh."""
        data = await self._json("GET", "/api/mesh/overview")
        self.local_node = (data.get("local_node") or {}).get("name")
        return data

    async def find_profile(self, name: str) -> dict[str, Any] | None:
        """A profile of the mesh by name, with its `node_name` and `session_id`."""
        overview = await self.overview()
        return next((p for p in overview.get("profiles", []) if p.get("name") == name), None)

    # Runs

    async def run(
        self,
        *,
        script: str | None = None,
        steps: list[dict[str, Any]] | None = None,
        args: dict[str, Any] | None = None,
        profile: str | None = None,
        session_id: str | None = None,
        tab: str | None = None,
        url: str | None = None,
        keep_open: bool = False,
        persistent: bool = False,
        timeout: float = 60,
        action_timeout: float | None = None,
        screenshot_on_error: bool = True,
        idle_timeout_minutes: int | None = None,
        node: str | None = None,
        check: bool = True,
    ) -> RunResult:
        """Run a script or steps in a browser of the mesh (see docs/run.md).

        Raises :class:`RunError` (:class:`RunTimeout`) when the run fails,
        unless `check` is false, and :class:`BrowserDaemonError` when it could
        not start.
        """
        body: dict[str, Any] = {
            "script": script,
            "steps": steps,
            "args": args,
            "profile": profile,
            "session_id": session_id,
            "tab": tab,
            "url": url,
            "keep_open": keep_open,
            "persistent": persistent,
            "timeout_seconds": max(1, round(timeout)),
            "action_timeout_seconds": round(action_timeout) if action_timeout else None,
            "screenshot_on_error": screenshot_on_error,
            "idle_timeout_minutes": idle_timeout_minutes,
            "client": self.client,
        }
        status, data = await self._request(
            "POST",
            "/api/run",
            node=node,
            json={k: v for k, v in body.items() if v is not None},
            timeout=timeout + 45,
            accept=(422, 504),
        )
        res = RunResult.from_json(data or {})
        if check and not res.ok:
            raise (RunTimeout if res.timed_out else RunError)(res, status)
        return res

    # Sessions and tabs

    async def sessions(self, node: str | None = None) -> list[dict[str, Any]]:
        return await self._json("GET", "/api/sessions", node=node)

    async def session(self, session_id: str, node: str | None = None) -> dict[str, Any]:
        """The session and its `targets` (tabs)."""
        return await self._json("GET", f"/api/sessions/{quote(session_id, safe='')}", node=node)

    async def login(
        self,
        session_id: str,
        tab: str,
        username: str,
        password: str,
        *,
        totp_secret: str | None = None,
        totp_code: str | None = None,
        allowed_origins: list[str] | None = None,
        success_selector: str | None = None,
        timeout: float = 30,
        node: str | None = None,
    ) -> LoginResult:
        """Log in in place, without creating a new browser session or tab.

        ``authenticated`` is returned only when ``success_selector`` is visible.
        Otherwise inspect ``outcome`` (``needs_totp``, ``needs_interaction``,
        ``failed`` or ``unknown``) and handle the page accordingly.
        """
        if totp_secret is not None and totp_code is not None:
            raise ValueError("give totp_secret or totp_code, not both")
        timeout = max(1, min(90, timeout))
        body = {
            "tab": tab,
            "username": username,
            "password": password,
            "totp_secret": totp_secret,
            "totp_code": totp_code,
            "allowed_origins": allowed_origins,
            "success_selector": success_selector,
            "timeout_seconds": max(1, round(timeout)),
        }
        data = await self._json(
            "POST",
            f"/api/sessions/{quote(session_id, safe='')}/login",
            node=node,
            json={key: value for key, value in body.items() if value is not None},
            timeout=timeout + 5,
        )
        return LoginResult.from_json(data or {})

    async def place_browser(
        self,
        *,
        profile: str | None = None,
        node: str | None = None,
        group: str | None = None,
        engine: str | None = None,
        create: bool = False,
    ) -> dict[str, Any]:
        """Resolve placement; `create` allows a missing profile for a subsequent session."""
        if self.local_node is None:
            await self.overview()
        return await self._json(
            "POST",
            "/api/mesh/placement",
            json={
                k: v
                for k, v in {
                    "profile": profile,
                    "node": node,
                    "group": group,
                    "engine": engine,
                    "create": create,
                }.items()
                if v is not None
            },
            timeout=180,
        )

    async def create_session(
        self,
        *,
        profile: str | None = None,
        node: str | None = None,
        group: str | None = None,
        ephemeral_node: bool = False,
        **options: Any,
    ) -> dict[str, Any]:
        """Start a session (or attach to the profile's); options as `POST /api/sessions`."""
        if ephemeral_node:
            if profile is not None or node is not None or group is not None:
                raise ValueError("ephemeral_node cannot be combined with profile, node or group")
            body = {**options, "persistent": False}
            if self.client and "name" not in body:
                body["name"] = self.client
            result = await self._json("POST", "/api/ephemeral/sessions", json=body, timeout=1800)
            return {**result["session"], "node_url": result["node_url"]}
        placed = None
        if group is not None or (profile is not None and node is not None):
            placed = await self.place_browser(
                profile=profile, node=node, group=group, engine=options.get("engine"), create=True
            )
            node = placed["node"]
        body = {"persistent": True, "profile": profile, **options}
        if self.client and "name" not in body:
            body["name"] = self.client
        result = await self._json(
            "POST",
            "/api/sessions",
            node=node,
            json={k: v for k, v in body.items() if v is not None},
        )
        if placed is not None:
            result["node_name"] = node
        return result

    async def delete_session(self, session_id: str, node: str | None = None) -> None:
        await self._request("DELETE", f"/api/sessions/{quote(session_id, safe='')}", node=node)

    async def set_idle_timeout(
        self, session_id: str, minutes: int, node: str | None = None
    ) -> None:
        await self._request(
            "POST",
            f"/api/sessions/{quote(session_id, safe='')}/idle-timeout",
            node=node,
            params={"minutes": minutes},
        )

    async def tabs(self, node: str | None = None) -> list[dict[str, Any]]:
        """The pages of all sessions of the node (`/json/list`)."""
        return await self._json("GET", "/json/list", node=node)

    async def open_tab(
        self, url: str, *, session_id: str, node: str | None = None
    ) -> dict[str, Any]:
        return await self._json(
            "PUT", "/json/new", node=node, params={"url": url, "session_id": session_id}
        )

    async def close_tab(self, page_id: str, node: str | None = None) -> None:
        await self._request("PUT", f"/json/close/{quote(page_id, safe='')}", node=node)

    async def export_auth(
        self,
        page_id: str,
        *,
        session_id: str | None = None,
        urls: list[str] | None = None,
        storage: bool = False,
        node: str | None = None,
    ) -> dict[str, Any]:
        """A page's cookies (and local/session storage): its login, for an API client.

        The answer is credentials: never log it.
        """
        body: dict[str, Any] = {"urls": urls or [], "storage": storage}
        if session_id:
            body["session_id"] = session_id
        return await self._json(
            "POST", f"/api/pages/{quote(page_id, safe='')}/auth", node=node, json=body
        )


class step:
    """Builders for run steps (docs/run.md, "Steps"). `as_` names the value."""

    @staticmethod
    def _make(action: str, as_: str | None = None, **fields: Any) -> dict[str, Any]:
        out = {"action": action, **{k: v for k, v in fields.items() if v is not None}}
        if as_:
            out["as"] = as_
        return out

    @staticmethod
    def goto(url: str, *, wait_until: str | None = None, **kw: Any) -> dict[str, Any]:
        return step._make("goto", url=url, wait_until=wait_until, **kw)

    @staticmethod
    def click(selector: str, **kw: Any) -> dict[str, Any]:
        return step._make("click", selector=selector, **kw)

    @staticmethod
    def fill(
        selector: str, value: str | None = None, *, arg: str | None = None, **kw: Any
    ) -> dict[str, Any]:
        """Fill an input with `value`, or with `args[arg]` (keeps secrets out of the steps)."""
        return step._make("fill", selector=selector, value=value, value_arg=arg, **kw)

    @staticmethod
    def type(
        selector: str, value: str | None = None, *, arg: str | None = None, **kw: Any
    ) -> dict[str, Any]:
        return step._make("type", selector=selector, value=value, value_arg=arg, **kw)

    @staticmethod
    def press(key: str, selector: str | None = None, **kw: Any) -> dict[str, Any]:
        return step._make("press", key=key, selector=selector, **kw)

    @staticmethod
    def select(
        selector: str, value: Any = None, *, arg: str | None = None, **kw: Any
    ) -> dict[str, Any]:
        return step._make("select", selector=selector, value=value, value_arg=arg, **kw)

    @staticmethod
    def check(selector: str, checked: bool = True, **kw: Any) -> dict[str, Any]:
        return step._make("check", selector=selector, checked=checked, **kw)

    @staticmethod
    def wait_for(
        selector: str | None = None,
        *,
        state: str | None = None,
        url: str | None = None,
        load_state: str | None = None,
        ms: int | None = None,
        **kw: Any,
    ) -> dict[str, Any]:
        return step._make(
            "wait_for", selector=selector, state=state, url=url, load_state=load_state, ms=ms, **kw
        )

    @staticmethod
    def text(selector: str, *, all: bool | None = None, **kw: Any) -> dict[str, Any]:
        return step._make("text", selector=selector, all=all, **kw)

    @staticmethod
    def value(selector: str, **kw: Any) -> dict[str, Any]:
        return step._make("value", selector=selector, **kw)

    @staticmethod
    def attribute(selector: str, name: str, **kw: Any) -> dict[str, Any]:
        return step._make("attribute", selector=selector, name=name, **kw)

    @staticmethod
    def count(selector: str, **kw: Any) -> dict[str, Any]:
        return step._make("count", selector=selector, **kw)

    @staticmethod
    def visible(selector: str, **kw: Any) -> dict[str, Any]:
        return step._make("visible", selector=selector, **kw)

    @staticmethod
    def evaluate(expression: str, arg: Any = None, **kw: Any) -> dict[str, Any]:
        return step._make("evaluate", expression=expression, arg=arg, **kw)

    @staticmethod
    def screenshot(
        selector: str | None = None, *, full_page: bool | None = None, **kw: Any
    ) -> dict[str, Any]:
        return step._make("screenshot", selector=selector, full_page=full_page, **kw)

    @staticmethod
    def url(**kw: Any) -> dict[str, Any]:
        return step._make("url", **kw)

    @staticmethod
    def title(**kw: Any) -> dict[str, Any]:
        return step._make("title", **kw)
