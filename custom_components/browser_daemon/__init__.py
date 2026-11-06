"""The Browser Daemon integration."""

from __future__ import annotations

import logging
from typing import TypeVar

from browser_daemon_client import BrowserDaemon
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import (
    BrowserDaemonAuthError,
    BrowserDaemonClient,
    BrowserDaemonConnectionError,
    BrowserDaemonError,
    BrowserDaemonNodeOfflineError,
    BrowserDaemonNotConfiguredError,
)
from .const import (
    CONF_CLUSTER_SECRET,
    CONF_URL,
    CONF_VERIFY_SSL,
    DOMAIN,
    PLATFORMS,
)
from .coordinator import BrowserDaemonCoordinator
from .services import async_setup_services, async_unload_services

_LOGGER = logging.getLogger(__name__)

CONFIG_SCHEMA = cv.empty_config_schema(DOMAIN)

DaemonT = TypeVar("DaemonT", bound=BrowserDaemon)

__all__ = [
    "DOMAIN",
    "BrowserDaemonAuthError",
    "BrowserDaemonClient",
    "BrowserDaemonConnectionError",
    "BrowserDaemonCoordinator",
    "BrowserDaemonError",
    "BrowserDaemonNodeOfflineError",
    "BrowserDaemonNotConfiguredError",
    "async_get_client",
    "async_get_coordinator",
    "async_get_daemon",
]


async def async_setup(hass: HomeAssistant, config: dict) -> bool:
    """Set up Browser Daemon integration (YAML / services)."""
    hass.data.setdefault(DOMAIN, {})
    await async_setup_services(hass)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Set up Browser Daemon from a config entry."""
    hass.data.setdefault(DOMAIN, {})
    session = async_get_clientsession(hass)

    client = BrowserDaemonClient(
        session=session,
        base_url=entry.data[CONF_URL],
        cluster_secret=entry.data.get(CONF_CLUSTER_SECRET),
        verify_ssl=entry.data.get(CONF_VERIFY_SSL, True),
    )

    coordinator = BrowserDaemonCoordinator(hass, entry, client)
    hass.data[DOMAIN][entry.entry_id] = coordinator

    # Fetch initial data so all mesh nodes are known before platform setup
    await coordinator.async_config_entry_first_refresh()

    # Forward to sensor, binary_sensor, button
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)

    entry.async_on_unload(entry.add_update_listener(async_reload_entry))
    return True


async def async_reload_entry(hass: HomeAssistant, entry: ConfigEntry) -> None:
    """Reload config entry."""
    await hass.config_entries.async_reload(entry.entry_id)


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Unload a config entry."""
    unload_ok = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unload_ok:
        hass.data[DOMAIN].pop(entry.entry_id, None)

    # Check if any remaining coordinators
    has_active_entries = any(
        isinstance(v, BrowserDaemonCoordinator) for v in hass.data[DOMAIN].values()
    )
    if not has_active_entries:
        await async_unload_services(hass)

    return unload_ok


# =========================================================================
# Public Python API for consumption by other integrations (hrworks, msteams)
# =========================================================================


def async_get_coordinator(
    hass: HomeAssistant, entry_id: str | None = None
) -> BrowserDaemonCoordinator | None:
    """Retrieve the coordinator for an entry or the default coordinator."""
    domain_data = hass.data.get(DOMAIN, {})
    if entry_id:
        coord = domain_data.get(entry_id)
        return coord if isinstance(coord, BrowserDaemonCoordinator) else None
    for val in domain_data.values():
        if isinstance(val, BrowserDaemonCoordinator):
            return val
    return None


def async_get_client(
    hass: HomeAssistant, entry_id: str | None = None
) -> BrowserDaemonClient | None:
    """Retrieve the BrowserDaemonClient instance."""
    coordinator = async_get_coordinator(hass, entry_id)
    return coordinator.client if coordinator else None


def async_get_daemon(
    hass: HomeAssistant,
    *,
    client: str,
    cls: type[DaemonT] = BrowserDaemon,
    entry_id: str | None = None,
) -> DaemonT:
    """Return a browser-daemon client configured from a config entry.

    Other integrations use this instead of keeping a URL and cluster secret of
    their own. Without ``entry_id`` the first enabled entry is used. It also
    works while the entry retries its setup, so a daemon that is down reports
    itself on the first request. ``client`` is the name shown on the dashboard.
    """
    entries = [
        entry
        for entry in hass.config_entries.async_entries(DOMAIN)
        if not entry.disabled_by and (not entry_id or entry.entry_id == entry_id)
    ]
    if not entries:
        raise BrowserDaemonNotConfiguredError(
            f"Browser Daemon entry {entry_id} is not available"
            if entry_id
            else "The Browser Daemon integration is not set up"
        )
    data = entries[0].data
    return cls(
        async_get_clientsession(hass, verify_ssl=data.get(CONF_VERIFY_SSL, True)),
        data[CONF_URL],
        (data.get(CONF_CLUSTER_SECRET) or "").strip() or None,
        client=client,
    )
