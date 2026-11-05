"""Screenshot images of extension-backed browsers."""

from __future__ import annotations

import logging
import time
from datetime import datetime, timezone

from homeassistant.components.image import ImageEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import BrowserDaemonBrowserEntity

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

    def add_new_browsers() -> None:
        current = set((coordinator.data or {}).get("browsers", {}))
        new = current - known
        if not new:
            return
        known.update(new)
        async_add_entities(
            BrowserDaemonScreenshot(coordinator, b) for b in sorted(new)
        )

    entry.async_on_unload(coordinator.async_add_listener(add_new_browsers))


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
        self._attr_image_last_updated = datetime.now(timezone.utc)
        self.async_write_ha_state()
        return self._cached
