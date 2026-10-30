"""Buttons for Browser Daemon."""

from __future__ import annotations

from homeassistant.components.button import (
    ButtonDeviceClass,
    ButtonEntity,
    ButtonEntityDescription,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN
from .coordinator import BrowserDaemonCoordinator
from .entity import BrowserDaemonEntity

BUTTON_DESCRIPTIONS: tuple[ButtonEntityDescription, ...] = (
    ButtonEntityDescription(
        key="refresh_mesh",
        translation_key="refresh_mesh",
        icon="mdi:refresh",
    ),
    ButtonEntityDescription(
        key="vacuum_recordings",
        translation_key="vacuum_recordings",
        icon="mdi:broom",
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up Browser Daemon buttons."""
    coordinator: BrowserDaemonCoordinator = hass.data[DOMAIN][entry.entry_id]

    entities: list[ButtonEntity] = [
        BrowserDaemonButton(coordinator, desc) for desc in BUTTON_DESCRIPTIONS
    ]
    async_add_entities(entities)


class BrowserDaemonButton(BrowserDaemonEntity, ButtonEntity):
    """Button entity for Browser Daemon."""

    entity_description: ButtonEntityDescription

    def __init__(
        self,
        coordinator: BrowserDaemonCoordinator,
        description: ButtonEntityDescription,
    ) -> None:
        """Initialize the button."""
        super().__init__(coordinator, description.key)
        self.entity_description = description

    async def async_press(self) -> None:
        """Handle button press."""
        if self.entity_description.key == "refresh_mesh":
            await self.coordinator.async_request_refresh()
        elif self.entity_description.key == "vacuum_recordings":
            await self.coordinator.client.async_vacuum_recordings()
            await self.coordinator.async_request_refresh()
