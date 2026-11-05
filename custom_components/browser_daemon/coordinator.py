"""DataUpdateCoordinator for Browser Daemon."""

from __future__ import annotations

import logging
from datetime import timedelta
from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import (
    BrowserDaemonAuthError,
    BrowserDaemonClient,
    BrowserDaemonConnectionError,
)
from .const import (
    CONF_POLL_INTERVAL,
    DEFAULT_POLL_INTERVAL,
    DOMAIN,
    EVENT_SESSION_STARTED,
    EVENT_SESSION_STOPPED,
)

_LOGGER = logging.getLogger(__name__)


class BrowserDaemonCoordinator(DataUpdateCoordinator[dict[str, Any]]):
    """Manage fetching Browser Daemon mesh data."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: BrowserDaemonClient,
    ) -> None:
        """Initialize the coordinator."""
        self.entry = entry
        self.client = client
        poll_interval = entry.options.get(
            CONF_POLL_INTERVAL,
            entry.data.get(CONF_POLL_INTERVAL, DEFAULT_POLL_INTERVAL),
        )
        super().__init__(
            hass,
            _LOGGER,
            config_entry=entry,
            name=f"Browser Daemon ({entry.title})",
            update_interval=timedelta(seconds=poll_interval),
        )

        self._previous_sessions: dict[str, dict[str, Any]] = {}

    async def _async_update_data(self) -> dict[str, Any]:
        """Fetch latest overview data from browser-daemon."""
        try:
            overview = await self.client.async_get_overview()
        except BrowserDaemonAuthError as err:
            raise UpdateFailed(
                f"Authentication failed with browser-daemon: {err}"
            ) from err
        except BrowserDaemonConnectionError as err:
            raise UpdateFailed(
                f"Connection error reaching browser-daemon: {err}"
            ) from err
        except Exception as err:
            raise UpdateFailed(
                f"Unexpected error fetching browser-daemon overview: {err}"
            ) from err

        # Process and normalize mesh nodes
        local_node = overview.get("local_node") or {}
        local_name = local_node.get("name") or "local"
        peers = overview.get("peers") or []

        all_nodes: dict[str, dict[str, Any]] = {
            local_name: {
                **local_node,
                "is_local": True,
                "status": "online",
                "latency_ms": 0,
            }
        }
        for peer in peers:
            p_name = peer.get("name")
            if p_name:
                all_nodes[p_name] = {
                    **peer,
                    "is_local": False,
                }

        # Extension-backed browsers are not mesh nodes; failures here must not
        # take the rest of the data down (older daemons lack the endpoint).
        browsers: dict[str, dict[str, Any]] = {}
        try:
            for browser in await self.client.async_list_extension_browsers():
                browsers[str(browser["id"])] = {
                    **browser,
                    "tab_count": len(browser.get("tabs") or []),
                }
        except Exception as err:  # noqa: BLE001
            _LOGGER.debug("Could not list extension browsers: %s", err)

        # Process sessions
        sessions = overview.get("sessions") or []
        sessions_by_id = {s["id"]: s for s in sessions if "id" in s}

        # Fire lifecycle events for sessions
        current_ids = set(sessions_by_id.keys())
        previous_ids = set(self._previous_sessions.keys())

        # Detected new sessions
        for sid in current_ids - previous_ids:
            sess_data = sessions_by_id[sid]
            _LOGGER.debug(
                "Browser session started: %s (%s)", sess_data.get("name"), sid
            )
            self.hass.bus.async_fire(
                EVENT_SESSION_STARTED,
                {
                    "session_id": sid,
                    "name": sess_data.get("name"),
                    "profile": sess_data.get("profile"),
                    "engine": sess_data.get("engine"),
                    "node": sess_data.get("node_name") or local_name,
                },
            )

        # Detected stopped sessions
        for sid in previous_ids - current_ids:
            old_sess = self._previous_sessions.get(sid, {})
            _LOGGER.debug("Browser session stopped: %s (%s)", old_sess.get("name"), sid)
            self.hass.bus.async_fire(
                EVENT_SESSION_STOPPED,
                {
                    "session_id": sid,
                    "name": old_sess.get("name"),
                    "profile": old_sess.get("profile"),
                    "engine": old_sess.get("engine"),
                    "node": old_sess.get("node_name") or local_name,
                },
            )

        self._previous_sessions = sessions_by_id

        # Process profiles
        profiles = overview.get("profiles") or []
        profiles_by_name = {p["name"]: p for p in profiles if "name" in p}

        return {
            "overview": overview,
            "local_node": local_node,
            "peers": peers,
            "nodes": all_nodes,
            "browsers": browsers,
            "sessions": sessions,
            "sessions_by_id": sessions_by_id,
            "profiles": profiles,
            "profiles_by_name": profiles_by_name,
            "recordings": overview.get("recordings") or [],
            "total_sessions": overview.get("total_sessions", len(sessions)),
            "total_nodes": overview.get("total_nodes", len(all_nodes)),
            "online_nodes": overview.get(
                "online_nodes",
                len([n for n in all_nodes.values() if n.get("status") == "online"]),
            ),
            "total_profiles": overview.get("total_profiles", len(profiles)),
            "total_recordings": overview.get("total_recordings", 0),
            "total_storage_bytes": overview.get("total_storage_bytes", 0),
            "max_storage_bytes": overview.get("max_storage_bytes", 0),
        }

    def get_active_session_for_profile(
        self, profile_name: str
    ) -> dict[str, Any] | None:
        """Find an active browser session for the specified profile."""
        if not self.data:
            return None
        sessions = self.data.get("sessions") or []
        for session in sessions:
            if session.get("profile") == profile_name:
                return session
        return None

    def get_online_nodes(self) -> list[str]:
        """Return a list of online node names."""
        if not self.data:
            return []
        nodes = self.data.get("nodes") or {}
        return [name for name, info in nodes.items() if info.get("status") == "online"]

    def is_node_online(self, node_name: str) -> bool:
        """Check if a specific node is currently online."""
        if not self.data:
            return False
        nodes = self.data.get("nodes") or {}
        node = nodes.get(node_name)
        return node is not None and node.get("status") == "online"
