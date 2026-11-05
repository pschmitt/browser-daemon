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
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import BrowserDaemonEntity, BrowserDaemonNodeEntity

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


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up Browser Daemon sensors from a config entry."""
    coordinator: BrowserDaemonCoordinator = hass.data[DOMAIN][entry.entry_id]

    entities: list[SensorEntity] = []

    # Add mesh-wide sensors
    for desc in MESH_SENSOR_DESCRIPTIONS:
        entities.append(BrowserDaemonMeshSensor(coordinator, desc))

    # Add per-node sensors
    nodes = (coordinator.data or {}).get("nodes", {})
    known_nodes = set(nodes)
    for node_name in nodes:
        for desc in NODE_SENSOR_DESCRIPTIONS:
            entities.append(BrowserDaemonNodeSensor(coordinator, node_name, desc))

    async_add_entities(entities)

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
                for desc in NODE_SENSOR_DESCRIPTIONS
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
        return node_data.get(self.entity_description.key)
