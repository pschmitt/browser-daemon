"""Diagnostics support for Browser Daemon."""

from __future__ import annotations

from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant

from .const import CONF_CLUSTER_SECRET, DOMAIN
from .coordinator import BrowserDaemonCoordinator


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ConfigEntry
) -> dict[str, Any]:
    """Return diagnostics for a config entry."""
    coordinator: BrowserDaemonCoordinator | None = hass.data.get(DOMAIN, {}).get(
        entry.entry_id
    )

    entry_data = dict(entry.data)
    if CONF_CLUSTER_SECRET in entry_data:
        entry_data[CONF_CLUSTER_SECRET] = "**REDACTED**"

    coord_data = {}
    if coordinator and coordinator.data:
        coord_data = {
            "total_nodes": coordinator.data.get("total_nodes"),
            "online_nodes": coordinator.data.get("online_nodes"),
            "total_sessions": coordinator.data.get("total_sessions"),
            "total_profiles": coordinator.data.get("total_profiles"),
            "total_storage_bytes": coordinator.data.get("total_storage_bytes"),
            "local_node": coordinator.data.get("local_node", {}).get("name"),
            "peers": [
                {
                    "name": p.get("name"),
                    "status": p.get("status"),
                    "latency_ms": p.get("latency_ms"),
                }
                for p in coordinator.data.get("peers", [])
            ],
            "sessions": [
                {
                    "id": s.get("id"),
                    "name": s.get("name"),
                    "profile": s.get("profile"),
                    "engine": s.get("engine"),
                    "node_name": s.get("node_name"),
                }
                for s in coordinator.data.get("sessions", [])
            ],
            "profiles": [
                {
                    "name": p.get("name"),
                    "engine": p.get("engine"),
                    "node_name": p.get("node_name"),
                    "is_active": p.get("is_active"),
                }
                for p in coordinator.data.get("profiles", [])
            ],
        }

    return {
        "entry": {
            "entry_id": entry.entry_id,
            "version": entry.version,
            "title": entry.title,
            "data": entry_data,
            "options": dict(entry.options),
        },
        "coordinator": coord_data,
    }
