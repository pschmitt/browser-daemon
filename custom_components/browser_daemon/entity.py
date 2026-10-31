"""Base entity definitions for Browser Daemon."""

from __future__ import annotations

from typing import Any

from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator


class BrowserDaemonEntity(CoordinatorEntity[BrowserDaemonCoordinator]):
    """Base entity for mesh-level Browser Daemon attributes."""

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        key: str,
    ) -> None:
        """Initialize the entity."""
        super().__init__(coordinator)
        self._key = key
        data = coordinator.data or {}
        local_name = data.get("local_node", {}).get("name") or "local"
        self._attr_unique_id = f"{coordinator.entry.entry_id}_{key}"

        version = data.get("local_node", {}).get("version")
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, f"{coordinator.entry.entry_id}_mesh")},
            name=f"Browser Daemon Mesh ({local_name})",
            manufacturer="pschmitt",
            model="Browser Daemon Mesh",
            sw_version=version,
            configuration_url=coordinator.client.base_url,
        )


class BrowserDaemonNodeEntity(CoordinatorEntity[BrowserDaemonCoordinator]):
    """Base entity for node-specific Browser Daemon attributes."""

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        node_name: str,
        key: str,
    ) -> None:
        """Initialize the node entity."""
        super().__init__(coordinator)
        self._node_name = node_name
        self._key = key
        self._attr_unique_id = f"{coordinator.entry.entry_id}_{node_name}_{key}"

        data = coordinator.data or {}
        node_info = data.get("nodes", {}).get(node_name, {})
        version = node_info.get("version")
        url = node_info.get("url")

        dev_reg = dr.async_get(coordinator.hass)
        parent_device = dev_reg.async_get_device(
            identifiers={(DOMAIN, f"{coordinator.entry.entry_id}_mesh")}
        )
        via_device_id = parent_device.id if parent_device else None

        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, f"{coordinator.entry.entry_id}_node_{node_name}")},
            name=f"Browser Node {node_name}",
            manufacturer="pschmitt",
            model="Browser Daemon Node",
            sw_version=version,
            via_device_id=via_device_id,
            configuration_url=url or coordinator.client.base_url,
        )

    @property
    def node_data(self) -> dict[str, Any]:
        """Return data for this node."""
        if not self.coordinator.data:
            return {}
        nodes = self.coordinator.data.get("nodes") or {}
        return nodes.get(self._node_name) or {}
