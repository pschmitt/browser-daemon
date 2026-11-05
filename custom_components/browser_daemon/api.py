"""API Client for browser-daemon."""

from __future__ import annotations

import base64
import logging
import time
from typing import Any
from urllib.parse import quote

import aiohttp
from homeassistant.exceptions import HomeAssistantError

_LOGGER = logging.getLogger(__name__)


class BrowserDaemonError(HomeAssistantError):
    """Base exception for Browser Daemon."""


class BrowserDaemonAuthError(BrowserDaemonError):
    """Authentication failed."""


class BrowserDaemonConnectionError(BrowserDaemonError):
    """Connection or network error."""


class BrowserDaemonNodeOfflineError(BrowserDaemonError):
    """Specified node is offline or unreachable."""


class BrowserDaemonClient:
    """HTTP Client for browser-daemon mesh."""

    def __init__(
        self,
        session: aiohttp.ClientSession,
        base_url: str,
        cluster_secret: str | None = None,
        verify_ssl: bool = True,
    ) -> None:
        """Initialize the client."""
        self.session = session
        self.base_url = base_url.rstrip("/")
        self.cluster_secret = cluster_secret.strip() if cluster_secret else None
        self.verify_ssl = verify_ssl

        self._cached_token: str | None = None
        self._token_expires_at: float = 0.0
        self._local_node_name: str | None = None

    @property
    def local_node_name(self) -> str | None:
        """Return the detected local node name."""
        return self._local_node_name

    def _get_peer_path(self, path: str, node: str | None = None) -> str:
        """Build peer forwarding path if node is specified and remote."""
        clean_path = path.lstrip("/")
        if not node or node == self._local_node_name:
            return f"/{clean_path}"
        return f"/api/peers/{quote(node)}/{clean_path}"

    async def _async_ensure_token(self) -> str | None:
        """Ensure a valid bd_auth token is available if cluster_secret is not used."""
        if self.cluster_secret:
            return None

        now = time.time()
        if self._cached_token and now < self._token_expires_at - 60:
            return self._cached_token

        # Fetch dashboard root to acquire bd_auth cookie
        url = f"{self.base_url}/"
        try:
            async with self.session.get(
                url, ssl=self.verify_ssl, timeout=10, allow_redirects=True
            ) as resp:
                # Extract bd_auth from cookies
                cookie = resp.cookies.get("bd_auth")
                if cookie:
                    self._cached_token = cookie.value
                    self._token_expires_at = now + 86400  # Default 24h caching
                    return self._cached_token

                # Fallback: check set-cookie header directly
                raw_cookies = resp.headers.getall("Set-Cookie", [])
                for raw in raw_cookies:
                    if "bd_auth=" in raw:
                        val = raw.split("bd_auth=")[1].split(";")[0].strip()
                        if val:
                            self._cached_token = val
                            self._token_expires_at = now + 86400
                            return self._cached_token

        except aiohttp.ClientError as err:
            _LOGGER.debug("Could not fetch auth cookie token from %s: %s", url, err)

        return None

    async def _async_request(
        self,
        method: str,
        path: str,
        *,
        node: str | None = None,
        json: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
        params: dict[str, Any] | None = None,
        timeout: int = 15,
        retry_auth: bool = True,
    ) -> aiohttp.ClientResponse:
        """Send authenticated HTTP request to the daemon."""
        full_path = self._get_peer_path(path, node=node)
        url = f"{self.base_url}{full_path}"

        req_headers = dict(headers or {})
        if self.cluster_secret:
            req_headers["X-Browser-Cluster-Secret"] = self.cluster_secret
        else:
            token = await self._async_ensure_token()
            if token:
                req_headers["Authorization"] = f"Bearer {token}"
                req_headers["Cookie"] = f"bd_auth={token}"

        try:
            resp = await self.session.request(
                method,
                url,
                json=json,
                headers=req_headers,
                params=params,
                ssl=self.verify_ssl,
                timeout=aiohttp.ClientTimeout(total=timeout),
            )

            if resp.status == 401 and retry_auth and not self.cluster_secret:
                # Invalidate cached token and retry once
                self._cached_token = None
                return await self._async_request(
                    method,
                    path,
                    node=node,
                    json=json,
                    headers=headers,
                    params=params,
                    timeout=timeout,
                    retry_auth=False,
                )

            return resp

        except aiohttp.ClientConnectorError as err:
            raise BrowserDaemonConnectionError(
                f"Cannot connect to browser-daemon at {url}: {err}"
            ) from err
        except aiohttp.ClientError as err:
            raise BrowserDaemonConnectionError(
                f"HTTP communication error with {url}: {err}"
            ) from err

    async def async_get_health(self) -> dict[str, Any]:
        """Fetch daemon health status."""
        resp = await self._async_request("GET", "/health", timeout=5)
        if resp.status != 200:
            raise BrowserDaemonConnectionError(
                f"Health check failed: HTTP {resp.status}"
            )
        return await resp.json()

    async def async_get_overview(self) -> dict[str, Any]:
        """Fetch complete mesh overview."""
        resp = await self._async_request("GET", "/api/mesh/overview", timeout=20)
        if resp.status == 401:
            raise BrowserDaemonAuthError(
                "Unauthorized: Invalid cluster secret or token"
            )
        if resp.status != 200:
            text = await resp.text()
            raise BrowserDaemonError(
                f"Failed to fetch overview ({resp.status}): {text}"
            )

        data = await resp.json()
        local_node = data.get("local_node") or {}
        if "name" in local_node:
            self._local_node_name = local_node["name"]
        return data

    async def async_list_extension_browsers(self) -> list[dict[str, Any]]:
        """List extension-backed (shared) browsers registered on the local node."""
        resp = await self._async_request("GET", "/api/extension/nodes", timeout=15)
        if resp.status != 200:
            raise BrowserDaemonError(
                f"Failed to list extension browsers: HTTP {resp.status}"
            )
        data = await resp.json()
        if isinstance(data, dict):
            data = data.get("nodes") or []
        return [b for b in data if isinstance(b, dict) and b.get("id")]

    async def async_extension_screenshot(
        self, browser_id: str, tab: int
    ) -> tuple[bytes, str] | None:
        """Screenshot a shared tab of an extension browser: (bytes, mime type)."""
        resp = await self._async_request(
            "POST",
            f"/api/extension/nodes/{quote(browser_id)}/command",
            json={"tab": tab, "action": "screenshot", "params": {}},
            timeout=20,
        )
        if resp.status != 200:
            return None
        body = await resp.json()
        result = body.get("result") if isinstance(body, dict) else None
        data = result.get("data") if isinstance(result, dict) else None
        if not isinstance(data, str):
            return None
        mime = "image/png" if result.get("format") == "png" else "image/jpeg"
        return base64.b64decode(data), mime

    async def async_list_sessions(
        self, node: str | None = None
    ) -> list[dict[str, Any]]:
        """List active sessions (local or on peer)."""
        resp = await self._async_request("GET", "/api/sessions", node=node)
        if resp.status != 200:
            raise BrowserDaemonError(f"Failed to list sessions: HTTP {resp.status}")
        return await resp.json()

    async def async_create_session(
        self,
        *,
        profile: str | None = None,
        engine: str | None = None,
        node: str | None = None,
        name: str | None = None,
        screen: str | None = None,
        idle_timeout_minutes: int | None = None,
        record: bool | None = None,
        adblock: bool | None = None,
        default_tabs: bool | None = None,
    ) -> dict[str, Any]:
        """Create a new browser session."""
        payload: dict[str, Any] = {
            "persistent": True,
        }
        if profile:
            payload["profile"] = profile
        if engine:
            payload["engine"] = engine
        if name:
            payload["name"] = name
        if screen:
            payload["screen"] = screen
        if idle_timeout_minutes is not None:
            payload["idle_timeout_minutes"] = idle_timeout_minutes
        if record is not None:
            payload["record"] = record
        if adblock is not None:
            payload["adblock"] = adblock
        if default_tabs is not None:
            payload["default_tabs"] = default_tabs

        resp = await self._async_request(
            "POST", "/api/sessions", node=node, json=payload, timeout=30
        )
        if resp.status not in (200, 201):
            text = await resp.text()
            raise BrowserDaemonError(
                f"Failed to create session ({resp.status}): {text}"
            )

        return await resp.json()

    async def async_get_session(
        self, session_id: str, node: str | None = None
    ) -> dict[str, Any]:
        """Retrieve details of an active session."""
        resp = await self._async_request(
            "GET", f"/api/sessions/{session_id}", node=node
        )
        if resp.status == 404:
            raise BrowserDaemonError(f"Session {session_id} not found")
        if resp.status != 200:
            raise BrowserDaemonError(f"Failed to get session: HTTP {resp.status}")
        return await resp.json()

    async def async_set_idle_timeout(
        self, session_id: str, minutes: int, node: str | None = None
    ) -> dict[str, Any]:
        """Update idle timeout for an active session."""
        resp = await self._async_request(
            "POST",
            f"/api/sessions/{session_id}/idle-timeout",
            node=node,
            params={"minutes": minutes},
        )
        if resp.status not in (200, 204):
            text = await resp.text()
            raise BrowserDaemonError(
                f"Failed to set idle timeout ({resp.status}): {text}"
            )
        if resp.status == 204:
            return {
                "success": True,
                "session_id": session_id,
                "idle_timeout_minutes": minutes,
            }
        if resp.content_length == 0 or resp.content_type != "application/json":
            return {
                "success": True,
                "session_id": session_id,
                "idle_timeout_minutes": minutes,
            }
        return await resp.json()

    async def async_login(
        self,
        session_id: str,
        *,
        tab: str,
        username: str,
        password: str,
        totp_secret: str | None = None,
        totp_code: str | None = None,
        allowed_origins: list[str] | None = None,
        success_selector: str | None = None,
        timeout_seconds: int = 30,
        node: str | None = None,
    ) -> dict[str, Any]:
        """Fill and submit a login form in an existing Chromium session tab.

        This endpoint requires the cluster secret. Credential values are sent
        only in the request body and are not logged or retained by this client.
        """
        if not self.cluster_secret:
            raise BrowserDaemonError("Login requires a configured cluster secret")

        payload: dict[str, Any] = {
            "tab": tab,
            "username": username,
            "password": password,
            "timeout_seconds": timeout_seconds,
        }
        for key, value in (
            ("totp_secret", totp_secret),
            ("totp_code", totp_code),
            ("success_selector", success_selector),
        ):
            if value is not None:
                payload[key] = value
        if allowed_origins:
            payload["allowed_origins"] = allowed_origins

        resp = await self._async_request(
            "POST",
            f"/api/sessions/{quote(session_id)}/login",
            node=node,
            json=payload,
            timeout=timeout_seconds + 5,
        )
        if resp.status != 200:
            raise BrowserDaemonError(f"Login failed: HTTP {resp.status}")
        return await resp.json()

    async def async_delete_session(
        self, session_id: str, node: str | None = None
    ) -> bool:
        """Stop/delete an active session."""
        resp = await self._async_request(
            "DELETE", f"/api/sessions/{session_id}", node=node
        )
        if resp.status in (200, 204):
            return True
        if resp.status == 404:
            return False
        text = await resp.text()
        raise BrowserDaemonError(f"Failed to delete session ({resp.status}): {text}")

    async def async_screenshot(self, session_id: str, node: str | None = None) -> bytes:
        """Capture screenshot JPEG bytes for a session."""
        resp = await self._async_request(
            "GET", f"/api/sessions/{session_id}/screenshot", node=node, timeout=20
        )
        if resp.status != 200:
            raise BrowserDaemonError(
                f"Failed to capture screenshot: HTTP {resp.status}"
            )
        return await resp.read()

    async def async_list_profiles(
        self, node: str | None = None
    ) -> list[dict[str, Any]]:
        """List profiles."""
        resp = await self._async_request("GET", "/api/profiles", node=node)
        if resp.status != 200:
            raise BrowserDaemonError(f"Failed to list profiles: HTTP {resp.status}")
        return await resp.json()

    async def async_get_profile(
        self, name: str, node: str | None = None
    ) -> dict[str, Any]:
        """Get profile details."""
        resp = await self._async_request(
            "GET", f"/api/profiles/{quote(name)}", node=node
        )
        if resp.status != 200:
            raise BrowserDaemonError(
                f"Failed to get profile {name}: HTTP {resp.status}"
            )
        return await resp.json()

    async def async_clear_profile(self, name: str, node: str | None = None) -> bool:
        """Reset profile data and cookies."""
        resp = await self._async_request(
            "POST", f"/api/profiles/{quote(name)}/clear", node=node, timeout=20
        )
        if resp.status in (200, 204):
            return True
        text = await resp.text()
        raise BrowserDaemonError(
            f"Failed to clear profile {name} ({resp.status}): {text}"
        )

    async def async_run(
        self, payload: dict[str, Any], *, node: str | None = None
    ) -> dict[str, Any]:
        """Run a Playwright script or steps on the mesh (`POST /api/run`).

        Returns the run's answer, also for a failed or timed-out run (`ok` false).
        Runs need the cluster secret; a viewer token is refused."""
        timeout = int(payload.get("timeout_seconds") or 60) + 45
        resp = await self._async_request(
            "POST", "/api/run", node=node, json=payload, timeout=timeout
        )
        if resp.status in (200, 422, 504):
            return await resp.json()
        if resp.status in (401, 403):
            raise BrowserDaemonAuthError(
                "Runs need the cluster secret (reconfigure the integration)"
            )
        try:
            message = (await resp.json()).get("error")
        except (aiohttp.ContentTypeError, ValueError):
            message = None
        raise BrowserDaemonError(
            f"Run refused ({resp.status}): {message or 'unknown error'}"
        )

    async def async_export_auth(
        self,
        page_id: str,
        *,
        session_id: str | None = None,
        urls: list[str] | None = None,
        storage: bool = True,
        node: str | None = None,
    ) -> dict[str, Any]:
        """Export authentication state (cookies and localStorage) for a page."""
        payload: dict[str, Any] = {
            "storage": storage,
        }
        if session_id:
            payload["session_id"] = session_id
        if urls:
            payload["urls"] = urls

        resp = await self._async_request(
            "POST",
            f"/api/pages/{quote(page_id)}/auth",
            node=node,
            json=payload,
            timeout=20,
        )
        if resp.status != 200:
            text = await resp.text()
            raise BrowserDaemonError(f"Failed to export auth ({resp.status}): {text}")
        return await resp.json()

    async def async_get_tabs(self, node: str | None = None) -> list[dict[str, Any]]:
        """Get open tabs (Chromium /json/list)."""
        resp = await self._async_request("GET", "/json/list", node=node)
        if resp.status != 200:
            raise BrowserDaemonError(f"Failed to get tabs: HTTP {resp.status}")
        return await resp.json()

    async def async_open_tab(
        self,
        url: str,
        *,
        session_id: str | None = None,
        node: str | None = None,
    ) -> dict[str, Any]:
        """Open a new tab."""
        params: dict[str, str] = {"url": url}
        if session_id:
            params["session_id"] = session_id
        resp = await self._async_request(
            "PUT", "/json/new", node=node, params=params, timeout=15
        )
        if resp.status not in (200, 201):
            text = await resp.text()
            raise BrowserDaemonError(f"Failed to open tab ({resp.status}): {text}")
        return await resp.json()

    async def async_close_tab(self, tab_id: str, node: str | None = None) -> bool:
        """Close an open tab."""
        resp = await self._async_request(
            "PUT", f"/json/close/{quote(tab_id)}", node=node
        )
        return resp.status in (200, 204)

    async def async_vacuum_recordings(self, node: str | None = None) -> dict[str, Any]:
        """Trigger vacuum of stale recordings."""
        resp = await self._async_request("POST", "/api/recordings/vacuum", node=node)
        if resp.status != 200:
            raise BrowserDaemonError(f"Vacuum failed: HTTP {resp.status}")
        return await resp.json()
