"""Screenshot images of extension-backed browsers."""

from __future__ import annotations

import logging
import time
from datetime import UTC, datetime

from homeassistant.components.image import ImageEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import BrowserDaemonBrowserEntity, BrowserDaemonNodeEntity

_LOGGER = logging.getLogger(__name__)

# A new screenshot costs the browser a capture: reuse the last one briefly.
CACHE_SECONDS = 30


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up screenshot images for extension browsers."""
    coordinator: BrowserDaemonCoordinator = hass.data[DOMAIN][entry.entry_id]
    known = set((coordinator.data or {}).get("browsers", {}))
    async_add_entities(BrowserDaemonScreenshot(coordinator, b) for b in sorted(known))

    known_nodes = set((coordinator.data or {}).get("nodes", {}))
    async_add_entities(
        BrowserDaemonNodeScreenshot(coordinator, n) for n in sorted(known_nodes)
    )

    def add_new_images() -> None:
        """Add images for browsers and nodes first seen after setup."""
        data = coordinator.data or {}
        new_browsers = set(data.get("browsers", {})) - known
        known.update(new_browsers)
        new_nodes = set(data.get("nodes", {})) - known_nodes
        known_nodes.update(new_nodes)
        async_add_entities(
            [
                *(
                    BrowserDaemonScreenshot(coordinator, b)
                    for b in sorted(new_browsers)
                ),
                *(
                    BrowserDaemonNodeScreenshot(coordinator, n)
                    for n in sorted(new_nodes)
                ),
            ]
        )

    entry.async_on_unload(coordinator.async_add_listener(add_new_images))


class BrowserDaemonScreenshot(BrowserDaemonBrowserEntity, ImageEntity):
    """Screenshot of the active shared tab of an extension browser."""

    _attr_translation_key = "browser_screenshot"
    _attr_icon = "mdi:monitor-screenshot"

    def __init__(self, coordinator: BrowserDaemonCoordinator, browser_id: str) -> None:
        """Initialize the screenshot image."""
        BrowserDaemonBrowserEntity.__init__(self, coordinator, browser_id, "screenshot")
        ImageEntity.__init__(self, coordinator.hass)
        self._cached: bytes | None = None
        self._cached_at = 0.0

    @property
    def available(self) -> bool:
        """The browser has to be connected."""
        return super().available and bool(self.browser_data.get("online"))

    @property
    def extra_state_attributes(self) -> dict[str, str | None]:
        """Tell which tab is shown."""
        tab = self._active_tab()
        return {"tab_title": tab.get("title"), "tab_url": tab.get("url")}

    def _active_tab(self) -> dict:
        tabs = self.browser_data.get("tabs") or []
        return next((t for t in tabs if t.get("active")), tabs[0] if tabs else {})

    async def async_image(self) -> bytes | None:
        """Return a screenshot of the active shared tab."""
        if self._cached and time.monotonic() - self._cached_at < CACHE_SECONDS:
            return self._cached
        tab = self._active_tab()
        if not tab.get("id"):
            return self._cached
        try:
            shot = await self.coordinator.client.async_extension_screenshot(
                self._browser_id, tab["id"]
            )
        except Exception as err:  # noqa: BLE001
            _LOGGER.debug("Screenshot of %s failed: %s", self._browser_id, err)
            return self._cached
        if shot is None:
            return self._cached
        self._cached, self._attr_content_type = shot[0], shot[1]
        self._cached_at = time.monotonic()
        self._attr_image_last_updated = datetime.now(UTC)
        self.async_write_ha_state()
        return self._cached


class BrowserDaemonNodeScreenshot(BrowserDaemonNodeEntity, ImageEntity):
    """Screenshot of the latest session of a node.

    A node can run several sessions; the one shown is the one agents or live
    views are using, else the most recently active.
    """

    _attr_translation_key = "node_screenshot"
    _attr_icon = "mdi:monitor-screenshot"

    def __init__(self, coordinator: BrowserDaemonCoordinator, node_name: str) -> None:
        """Initialize the screenshot image."""
        BrowserDaemonNodeEntity.__init__(self, coordinator, node_name, "screenshot")
        ImageEntity.__init__(self, coordinator.hass)
        self._attr_content_type = "image/jpeg"
        self._cached: bytes | None = None
        self._cached_session: str | None = None
        self._cached_at = 0.0

    def _session(self) -> dict | None:
        local = ((self.coordinator.data or {}).get("local_node") or {}).get("name")
        mine = [
            s
            for s in (self.coordinator.data or {}).get("sessions") or []
            if (s.get("node_name") or local) == self._node_name
        ]
        return max(
            mine,
            key=lambda s: (
                (s.get("client_count") or 0) + (s.get("viewer_count") or 0) > 0,
                -(s.get("idle_seconds") or 0),
            ),
            default=None,
        )

    @property
    def available(self) -> bool:
        """The node has to be online and running a session."""
        return (
            super().available
            and self.node_data.get("status") == "online"
            and self._session() is not None
        )

    @property
    def extra_state_attributes(self) -> dict[str, str | None]:
        """Tell which session is shown."""
        session = self._session() or {}
        return {
            "session_id": session.get("id"),
            "session_name": session.get("name"),
            "profile": session.get("profile"),
            "engine": session.get("engine"),
        }

    async def async_image(self) -> bytes | None:
        """Return a screenshot of the node's latest session."""
        session = self._session()
        if not session or not session.get("id"):
            return None
        sid = session["id"]
        if (
            self._cached
            and self._cached_session == sid
            and time.monotonic() - self._cached_at < CACHE_SECONDS
        ):
            return self._cached
        try:
            data = await self.coordinator.client.async_screenshot(
                sid, node=self._node_name
            )
        except Exception as err:  # noqa: BLE001
            _LOGGER.debug("Screenshot of session %s failed: %s", sid, err)
            return self._cached if self._cached_session == sid else None
        self._cached, self._cached_session = data, sid
        self._cached_at = time.monotonic()
        self._attr_image_last_updated = datetime.now(UTC)
        self.async_write_ha_state()
        return data
