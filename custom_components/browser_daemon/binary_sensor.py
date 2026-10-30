"""Binary sensors for Browser Daemon."""

from __future__ import annotations

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
from .entity import BrowserDaemonEntity, BrowserDaemonNodeEntity

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
    for node_name in nodes:
        desc = BinarySensorEntityDescription(
            key="node_online",
            translation_key="node_online",
            device_class=BinarySensorDeviceClass.CONNECTIVITY,
            icon="mdi:server-network",
        )
        entities.append(BrowserDaemonNodeBinarySensor(coordinator, node_name, desc))

    async_add_entities(entities)


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
