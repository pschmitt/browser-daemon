"""Sensors for Browser Daemon."""

from __future__ import annotations

from typing import Any

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
    SensorStateClass,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import UnitOfInformation, UnitOfTime
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import (
    BrowserDaemonBrowserEntity,
    BrowserDaemonEntity,
    BrowserDaemonNodeEntity,
)

MESH_SENSOR_DESCRIPTIONS: tuple[SensorEntityDescription, ...] = (
    SensorEntityDescription(
        key="online_nodes",
        translation_key="online_nodes",
        native_unit_of_measurement="nodes",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:server-network",
    ),
    SensorEntityDescription(
        key="total_nodes",
        translation_key="total_nodes",
        native_unit_of_measurement="nodes",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:server",
    ),
    SensorEntityDescription(
        key="total_sessions",
        translation_key="total_sessions",
        native_unit_of_measurement="sessions",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:web",
    ),
    SensorEntityDescription(
        key="total_profiles",
        translation_key="total_profiles",
        native_unit_of_measurement="profiles",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:account-group",
    ),
    SensorEntityDescription(
        key="total_recordings",
        translation_key="total_recordings",
        native_unit_of_measurement="recordings",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:video-box",
    ),
    SensorEntityDescription(
        key="total_storage_bytes",
        translation_key="total_storage_bytes",
        device_class=SensorDeviceClass.DATA_SIZE,
        native_unit_of_measurement=UnitOfInformation.BYTES,
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:database",
    ),
    SensorEntityDescription(
        key="max_storage_bytes",
        translation_key="max_storage_bytes",
        device_class=SensorDeviceClass.DATA_SIZE,
        native_unit_of_measurement=UnitOfInformation.BYTES,
        icon="mdi:database-arrow-up",
    ),
)

NODE_SENSOR_DESCRIPTIONS: tuple[SensorEntityDescription, ...] = (
    SensorEntityDescription(
        key="active_sessions",
        translation_key="active_sessions",
        native_unit_of_measurement="sessions",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:tab",
    ),
    SensorEntityDescription(
        key="storage_used_bytes",
        translation_key="storage_used_bytes",
        device_class=SensorDeviceClass.DATA_SIZE,
        native_unit_of_measurement=UnitOfInformation.BYTES,
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:harddisk",
    ),
    SensorEntityDescription(
        key="uptime_seconds",
        translation_key="uptime_seconds",
        device_class=SensorDeviceClass.DURATION,
        native_unit_of_measurement=UnitOfTime.SECONDS,
        icon="mdi:timer-outline",
    ),
    SensorEntityDescription(
        key="latency_ms",
        translation_key="latency_ms",
        native_unit_of_measurement="ms",
        state_class=SensorStateClass.MEASUREMENT,
        icon="mdi:speedometer",
    ),
)


# Metadata, shown as diagnostic text sensors on each device.
NODE_METADATA_DESCRIPTIONS: tuple[SensorEntityDescription, ...] = (
    SensorEntityDescription(
        key="version",
        translation_key="version",
        entity_category=EntityCategory.DIAGNOSTIC,
        icon="mdi:tag",
    ),
    SensorEntityDescription(
        key="revision",
        translation_key="revision",
        entity_category=EntityCategory.DIAGNOSTIC,
        icon="mdi:source-commit",
    ),
    SensorEntityDescription(
        key="supported_engines",
        translation_key="supported_engines",
        entity_category=EntityCategory.DIAGNOSTIC,
        icon="mdi:application-cog",
    ),
)

MESH_METADATA_DESCRIPTIONS: tuple[SensorEntityDescription, ...] = (
    SensorEntityDescription(
        key="version",
        translation_key="version",
        entity_category=EntityCategory.DIAGNOSTIC,
        icon="mdi:tag",
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up Browser Daemon sensors from a config entry."""
    coordinator: BrowserDaemonCoordinator = hass.data[DOMAIN][entry.entry_id]

    entities: list[SensorEntity] = []

    # Add mesh-wide sensors
    for desc in (*MESH_SENSOR_DESCRIPTIONS, *MESH_METADATA_DESCRIPTIONS):
        entities.append(BrowserDaemonMeshSensor(coordinator, desc))

    # Add per-node sensors
    nodes = (coordinator.data or {}).get("nodes", {})
    known_nodes = set(nodes)
    for node_name in nodes:
        for desc in (*NODE_SENSOR_DESCRIPTIONS, *NODE_METADATA_DESCRIPTIONS):
            entities.append(BrowserDaemonNodeSensor(coordinator, node_name, desc))

    known_browsers = set((coordinator.data or {}).get("browsers", {}))
    entities.extend(
        sensor
        for bid in known_browsers
        for sensor in _browser_sensors(coordinator, bid)
    )

    async_add_entities(entities)

    def add_new_browser_entities() -> None:
        """Add entities for extension browsers first seen after setup."""
        current = set((coordinator.data or {}).get("browsers", {}))
        new = current - known_browsers
        if not new:
            return
        known_browsers.update(new)
        async_add_entities(
            [s for b in sorted(new) for s in _browser_sensors(coordinator, b)]
        )

    entry.async_on_unload(coordinator.async_add_listener(add_new_browser_entities))

    def add_new_node_entities() -> None:
        """Add entities for nodes first seen after initial setup."""
        current_nodes = set((coordinator.data or {}).get("nodes", {}))
        new_nodes = current_nodes - known_nodes
        if not new_nodes:
            return
        known_nodes.update(new_nodes)
        async_add_entities(
            [
                BrowserDaemonNodeSensor(coordinator, node_name, desc)
                for node_name in sorted(new_nodes)
                for desc in (*NODE_SENSOR_DESCRIPTIONS, *NODE_METADATA_DESCRIPTIONS)
            ]
        )

    entry.async_on_unload(coordinator.async_add_listener(add_new_node_entities))


class BrowserDaemonMeshSensor(BrowserDaemonEntity, SensorEntity):
    """Mesh-level sensor for Browser Daemon."""

    entity_description: SensorEntityDescription

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        description: SensorEntityDescription,
    ) -> None:
        """Initialize the sensor."""
        super().__init__(coordinator, description.key)
        self.entity_description = description

    @property
    def native_value(self) -> Any:
        """Return the state of the sensor."""
        if not self.coordinator.data:
            return None
        if self.entity_description.key == "version":
            return (self.coordinator.data.get("local_node") or {}).get("version")
        return self.coordinator.data.get(self.entity_description.key)


class BrowserDaemonNodeSensor(BrowserDaemonNodeEntity, SensorEntity):
    """Node-specific sensor for Browser Daemon."""

    entity_description: SensorEntityDescription

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        node_name: str,
        description: SensorEntityDescription,
    ) -> None:
        """Initialize the node sensor."""
        super().__init__(coordinator, node_name, description.key)
        self.entity_description = description

    @property
    def native_value(self) -> Any:
        """Return the state of the sensor."""
        node_data = self.node_data
        if not node_data:
            return None
        value = node_data.get(self.entity_description.key)
        if isinstance(value, list):
            return ", ".join(str(v) for v in value)
        return value


class BrowserDaemonBrowserTabsSensor(BrowserDaemonBrowserEntity, SensorEntity):
    """Number of shared tabs of an extension-backed browser."""

    _attr_translation_key = "browser_tabs"
    _attr_native_unit_of_measurement = "tabs"
    _attr_state_class = SensorStateClass.MEASUREMENT
    _attr_icon = "mdi:tab"

    def __init__(self, coordinator: BrowserDaemonCoordinator, browser_id: str) -> None:
        """Initialize the tab count sensor."""
        super().__init__(coordinator, browser_id, "tabs")

    @property
    def native_value(self) -> int | None:
        """Return the number of shared tabs."""
        data = self.browser_data
        return data.get("tab_count") if data else None

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """Describe the browser."""
        data = self.browser_data
        return {
            "browser": data.get("browser"),
            "version": data.get("version"),
            "remembered": data.get("remembered"),
            "native_control": data.get("native_control"),
        }


def _browser_sensors(
    coordinator: BrowserDaemonCoordinator, browser_id: str
) -> list[SensorEntity]:
    """Return all sensors of one extension browser."""
    return [
        BrowserDaemonBrowserTabsSensor(coordinator, browser_id),
        BrowserDaemonBrowserMetadataSensor(coordinator, browser_id, "version"),
        BrowserDaemonBrowserMetadataSensor(coordinator, browser_id, "browser"),
    ]


class BrowserDaemonBrowserMetadataSensor(BrowserDaemonBrowserEntity, SensorEntity):
    """Diagnostic metadata (extension version, browser) of an extension browser."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(
        self, coordinator: BrowserDaemonCoordinator, browser_id: str, key: str
    ) -> None:
        """Initialize the metadata sensor."""
        super().__init__(coordinator, browser_id, key)
        self._attr_translation_key = f"browser_{key}"
        self._attr_icon = "mdi:tag" if key == "version" else "mdi:web"

    @property
    def native_value(self) -> str | None:
        """Return the metadata value."""
        return self.browser_data.get(self._key) or None
