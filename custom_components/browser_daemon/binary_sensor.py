"""Binary sensors for Browser Daemon."""

from __future__ import annotations

from typing import Any

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
    BinarySensorEntityDescription,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import (
    BrowserDaemonBrowserEntity,
    BrowserDaemonEntity,
    BrowserDaemonNodeEntity,
)

MESH_BINARY_DESCRIPTIONS: tuple[BinarySensorEntityDescription, ...] = (
    BinarySensorEntityDescription(
        key="mesh_online",
        translation_key="mesh_online",
        device_class=BinarySensorDeviceClass.CONNECTIVITY,
        icon="mdi:check-network",
    ),
    BinarySensorEntityDescription(
        key="active_sessions",
        translation_key="active_sessions",
        device_class=BinarySensorDeviceClass.RUNNING,
        icon="mdi:play-circle-outline",
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up Browser Daemon binary sensors."""
    coordinator: BrowserDaemonCoordinator = hass.data[DOMAIN][entry.entry_id]

    entities: list[BinarySensorEntity] = []

    # Mesh-wide binary sensors
    for desc in MESH_BINARY_DESCRIPTIONS:
        entities.append(BrowserDaemonMeshBinarySensor(coordinator, desc))

    # Per-node online binary sensor
    nodes = (coordinator.data or {}).get("nodes", {})
    known_nodes = set(nodes)
    for node_name in nodes:
        desc = BinarySensorEntityDescription(
            key="node_online",
            translation_key="node_online",
            device_class=BinarySensorDeviceClass.CONNECTIVITY,
            icon="mdi:server-network",
        )
        entities.append(BrowserDaemonNodeBinarySensor(coordinator, node_name, desc))

    known_browsers = set((coordinator.data or {}).get("browsers", {}))
    entities.extend(
        BrowserDaemonBrowserBinarySensor(coordinator, bid) for bid in known_browsers
    )
    known_signals = _signal_keys(coordinator)
    entities.extend(
        BrowserDaemonBrowserSignalBinarySensor(coordinator, bid, name)
        for bid, name in sorted(known_signals)
    )

    async_add_entities(entities)

    def add_new_signal_entities() -> None:
        """Add a sensor for each signal a browser reports for the first time."""
        new = _signal_keys(coordinator) - known_signals
        if not new:
            return
        known_signals.update(new)
        async_add_entities(
            [
                BrowserDaemonBrowserSignalBinarySensor(coordinator, bid, name)
                for bid, name in sorted(new)
            ]
        )

    def add_new_browser_entities() -> None:
        """Add entities for extension browsers first seen after setup."""
        current = set((coordinator.data or {}).get("browsers", {}))
        new = current - known_browsers
        if not new:
            return
        known_browsers.update(new)
        async_add_entities(
            [BrowserDaemonBrowserBinarySensor(coordinator, b) for b in sorted(new)]
        )

    def add_new_node_entities() -> None:
        """Add online sensors for nodes first seen after initial setup."""
        add_new_browser_entities()
        add_new_signal_entities()
        current_nodes = set((coordinator.data or {}).get("nodes", {}))
        new_nodes = current_nodes - known_nodes
        if not new_nodes:
            return
        known_nodes.update(new_nodes)
        async_add_entities(
            [
                BrowserDaemonNodeBinarySensor(
                    coordinator,
                    node_name,
                    BinarySensorEntityDescription(
                        key="node_online",
                        translation_key="node_online",
                        device_class=BinarySensorDeviceClass.CONNECTIVITY,
                        icon="mdi:server-network",
                    ),
                )
                for node_name in sorted(new_nodes)
            ]
        )

    entry.async_on_unload(coordinator.async_add_listener(add_new_node_entities))


class BrowserDaemonMeshBinarySensor(BrowserDaemonEntity, BinarySensorEntity):
    """Mesh-level binary sensor."""

    entity_description: BinarySensorEntityDescription

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        description: BinarySensorEntityDescription,
    ) -> None:
        """Initialize binary sensor."""
        super().__init__(coordinator, description.key)
        self.entity_description = description

    @property
    def is_on(self) -> bool:
        """Return True if condition is met."""
        if not self.coordinator.data or not self.coordinator.last_update_success:
            return False

        if self.entity_description.key == "mesh_online":
            return self.coordinator.data.get("online_nodes", 0) > 0

        if self.entity_description.key == "active_sessions":
            return self.coordinator.data.get("total_sessions", 0) > 0

        return False


class BrowserDaemonNodeBinarySensor(BrowserDaemonNodeEntity, BinarySensorEntity):
    """Node-specific online binary sensor."""

    entity_description: BinarySensorEntityDescription

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        node_name: str,
        description: BinarySensorEntityDescription,
    ) -> None:
        """Initialize node binary sensor."""
        super().__init__(coordinator, node_name, description.key)
        self.entity_description = description

    @property
    def is_on(self) -> bool:
        """Return True if node is online."""
        node_data = self.node_data
        if not node_data:
            return False
        return node_data.get("status") == "online"

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """Describe the node."""
        n = self.node_data
        return {
            k: n.get(k)
            for k in (
                "url",
                "version",
                "revision",
                "supported_engines",
                "max_sessions",
                "profiles_count",
                "recordings_count",
                "is_local",
            )
            if n.get(k) is not None
        }


class BrowserDaemonBrowserBinarySensor(BrowserDaemonBrowserEntity, BinarySensorEntity):
    """Online state of an extension-backed browser."""

    _attr_device_class = BinarySensorDeviceClass.CONNECTIVITY
    _attr_translation_key = "browser_online"
    _attr_icon = "mdi:web"

    def __init__(self, coordinator: BrowserDaemonCoordinator, browser_id: str) -> None:
        """Initialize the browser online sensor."""
        super().__init__(coordinator, browser_id, "online")

    @property
    def is_on(self) -> bool:
        """Return True if the browser's extension is connected."""
        return bool(self.browser_data.get("online"))


def _signal_keys(coordinator: BrowserDaemonCoordinator) -> set[tuple[str, str]]:
    """(browser id, signal name) for every signal a browser reports."""
    browsers = (coordinator.data or {}).get("browsers", {})
    return {
        (bid, name)
        for bid, browser in browsers.items()
        for name in (browser.get("signals") or {})
    }


SIGNAL_ICONS: dict[str, str] = {
    "media": "mdi:music-note",
    "meeting": "mdi:video",
}


class BrowserDaemonBrowserSignalBinarySensor(
    BrowserDaemonBrowserEntity, BinarySensorEntity
):
    """A signal of an extension-backed browser.

    A tab playing sound, a meeting open, or an own rule (the extension's
    signalRules policy), pushed by the daemon as it changes.
    """

    def __init__(
        self, coordinator: BrowserDaemonCoordinator, browser_id: str, name: str
    ) -> None:
        """Initialize the signal sensor."""
        super().__init__(coordinator, browser_id, f"signal_{name}")
        self._signal = name
        self._attr_name = name.replace("_", " ").capitalize()
        self._attr_icon = SIGNAL_ICONS.get(name, "mdi:tab-search")
        if name == "meeting":
            self._attr_device_class = BinarySensorDeviceClass.OCCUPANCY

    @property
    def _signal_data(self) -> dict[str, Any]:
        return (self.browser_data.get("signals") or {}).get(self._signal) or {}

    @property
    def available(self) -> bool:
        """Only while the browser is connected and still reports the signal."""
        return (
            super().available
            and bool(self.browser_data.get("online"))
            and bool(self._signal_data)
        )

    @property
    def is_on(self) -> bool:
        """Return True while the signal is on."""
        return bool(self._signal_data.get("on"))

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """The signal's attributes (title, site, provider, url, count)."""
        return dict(self._signal_data.get("attributes") or {})
