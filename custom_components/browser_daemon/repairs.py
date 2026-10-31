"""Repairs flow for Browser Daemon."""

from __future__ import annotations

import logging
from typing import Any

import voluptuous as vol
from homeassistant.components.repairs import RepairsFlow
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import BrowserDaemonAuthError, BrowserDaemonClient
from .const import CONF_CLUSTER_SECRET, CONF_URL, CONF_VERIFY_SSL, DOMAIN

_LOGGER = logging.getLogger(__name__)


class BrowserDaemonAuthRepairFlow(RepairsFlow):
    """Handler for an issue fixing flow when authentication fails."""

    def __init__(self, entry_id: str) -> None:
        """Initialize repair flow."""
        self.entry_id = entry_id

    async def async_step_init(
        self, user_input: dict[str, Any] | None = None
    ) -> FlowResult:
        """Handle initial step."""
        errors: dict[str, str] = {}
        entry = self.hass.config_entries.async_get_entry(self.entry_id)
        if not entry:
            return self.async_abort(reason="entry_not_found")

        if user_input is not None:
            secret = user_input.get(CONF_CLUSTER_SECRET)
            session = async_get_clientsession(self.hass)
            client = BrowserDaemonClient(
                session=session,
                base_url=entry.data[CONF_URL],
                cluster_secret=secret,
                verify_ssl=entry.data.get(CONF_VERIFY_SSL, True),
            )

            try:
                await client.async_get_overview()
                self.hass.config_entries.async_update_entry(
                    entry,
                    data={
                        **entry.data,
                        CONF_CLUSTER_SECRET: secret,
                    },
                )
                await self.hass.config_entries.async_reload(entry.entry_id)
                return self.async_create_entry(title="", data={})
            except BrowserDaemonAuthError:
                errors["base"] = "invalid_auth"
            except Exception as err:
                _LOGGER.exception("Unexpected error during repair: %s", err)
                errors["base"] = "cannot_connect"

        schema = vol.Schema(
            {
                vol.Required(
                    CONF_CLUSTER_SECRET,
                    default=entry.data.get(CONF_CLUSTER_SECRET, ""),
                ): str,
            }
        )

        return self.async_show_form(
            step_id="init",
            data_schema=schema,
            errors=errors,
        )


async def async_create_fix_flow(
    hass: HomeAssistant,
    issue_id: str,
    data: dict[str, Any] | None,
) -> RepairsFlow | None:
    """Create a repair flow based on issue."""
    if issue_id.startswith("auth_error_"):
        entry_id = issue_id.removeprefix("auth_error_")
        return BrowserDaemonAuthRepairFlow(entry_id)
    return None
