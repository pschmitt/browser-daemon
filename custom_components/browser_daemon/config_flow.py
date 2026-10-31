"""Config flow for Browser Daemon integration."""

from __future__ import annotations

import logging
from typing import Any

import voluptuous as vol
from homeassistant import config_entries
from homeassistant.core import callback
from homeassistant.data_entry_flow import FlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import (
    BrowserDaemonAuthError,
    BrowserDaemonClient,
    BrowserDaemonConnectionError,
)
from .const import (
    CONF_CLUSTER_SECRET,
    CONF_DEFAULT_NODE,
    CONF_DEFAULT_PROFILE,
    CONF_POLL_INTERVAL,
    CONF_URL,
    CONF_VERIFY_SSL,
    DEFAULT_POLL_INTERVAL,
    DEFAULT_URL,
    DOMAIN,
)

_LOGGER = logging.getLogger(__name__)


class BrowserDaemonConfigFlow(config_entries.ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Browser Daemon."""

    VERSION = 1

    async def async_step_user(
        self, user_input: dict[str, Any] | None = None
    ) -> FlowResult:
        """Handle the initial setup step."""
        errors: dict[str, str] = {}

        if user_input is not None:
            url = user_input[CONF_URL].rstrip("/")
            secret = user_input.get(CONF_CLUSTER_SECRET)
            verify_ssl = user_input.get(CONF_VERIFY_SSL, True)

            session = async_get_clientsession(self.hass)
            client = BrowserDaemonClient(
                session=session,
                base_url=url,
                cluster_secret=secret,
                verify_ssl=verify_ssl,
            )

            try:
                overview = await client.async_get_overview()
                local_node = overview.get("local_node") or {}
                node_name = local_node.get("name") or "browser-daemon"

                unique_id = f"browser_daemon_{node_name.lower()}"
                await self.async_set_unique_id(unique_id)
                self._abort_if_unique_id_configured()

                title = f"Browser Daemon ({node_name})"
                return self.async_create_entry(title=title, data=user_input)

            except BrowserDaemonAuthError:
                errors["base"] = "invalid_auth"
            except BrowserDaemonConnectionError:
                errors["base"] = "cannot_connect"
            except Exception as err:
                _LOGGER.exception("Unexpected error setting up browser-daemon: %s", err)
                errors["base"] = "unknown"

        schema = vol.Schema(
            {
                vol.Required(CONF_URL, default=DEFAULT_URL): str,
                vol.Optional(CONF_CLUSTER_SECRET): str,
                vol.Optional(CONF_VERIFY_SSL, default=True): bool,
            }
        )

        return self.async_show_form(
            step_id="user",
            data_schema=schema,
            errors=errors,
        )

    async def async_step_reconfigure(
        self, user_input: dict[str, Any] | None = None
    ) -> FlowResult:
        """Change the URL or cluster secret; a blank secret keeps the saved one."""
        entry = self._get_reconfigure_entry()
        errors: dict[str, str] = {}

        if user_input is not None:
            data = {
                **entry.data,
                CONF_URL: user_input[CONF_URL].rstrip("/"),
                CONF_VERIFY_SSL: user_input.get(CONF_VERIFY_SSL, True),
            }
            if user_input.get(CONF_CLUSTER_SECRET):
                data[CONF_CLUSTER_SECRET] = user_input[CONF_CLUSTER_SECRET].strip()

            client = BrowserDaemonClient(
                session=async_get_clientsession(self.hass),
                base_url=data[CONF_URL],
                cluster_secret=data.get(CONF_CLUSTER_SECRET),
                verify_ssl=data[CONF_VERIFY_SSL],
            )
            try:
                await client.async_get_overview()
            except BrowserDaemonAuthError:
                errors["base"] = "invalid_auth"
            except BrowserDaemonConnectionError:
                errors["base"] = "cannot_connect"
            except Exception:
                _LOGGER.exception("Unexpected error reconfiguring browser-daemon")
                errors["base"] = "unknown"
            else:
                return self.async_update_reload_and_abort(entry, data=data)

        schema = vol.Schema(
            {
                vol.Required(
                    CONF_URL, default=entry.data.get(CONF_URL, DEFAULT_URL)
                ): str,
                vol.Optional(CONF_CLUSTER_SECRET): str,
                vol.Optional(
                    CONF_VERIFY_SSL, default=entry.data.get(CONF_VERIFY_SSL, True)
                ): bool,
            }
        )
        return self.async_show_form(
            step_id="reconfigure", data_schema=schema, errors=errors
        )

    @staticmethod
    @callback
    def async_get_options_flow(
        config_entry: config_entries.ConfigEntry,
    ) -> config_entries.OptionsFlow:
        """Create options flow handler."""
        return BrowserDaemonOptionsFlowHandler(config_entry)


class BrowserDaemonOptionsFlowHandler(config_entries.OptionsFlowWithConfigEntry):
    """Handle options flow for Browser Daemon."""

    async def async_step_init(
        self, user_input: dict[str, Any] | None = None
    ) -> FlowResult:
        """Manage the options."""
        if user_input is not None:
            return self.async_create_entry(title="", data=user_input)

        coordinator = self.hass.data.get(DOMAIN, {}).get(self.config_entry.entry_id)

        node_choices: list[str] = []
        profile_choices: list[str] = []

        if coordinator and coordinator.data:
            node_choices = list(coordinator.data.get("nodes", {}).keys())
            profile_choices = list(coordinator.data.get("profiles_by_name", {}).keys())

        schema_dict: dict[Any, Any] = {
            vol.Optional(
                CONF_POLL_INTERVAL,
                default=self.options.get(
                    CONF_POLL_INTERVAL,
                    self.config_entry.data.get(
                        CONF_POLL_INTERVAL, DEFAULT_POLL_INTERVAL
                    ),
                ),
            ): vol.All(vol.Coerce(int), vol.Range(min=5, max=300)),
        }

        if node_choices:
            schema_dict[
                vol.Optional(
                    CONF_DEFAULT_NODE,
                    default=self.options.get(CONF_DEFAULT_NODE, node_choices[0]),
                )
            ] = vol.In(node_choices)

        if profile_choices:
            schema_dict[
                vol.Optional(
                    CONF_DEFAULT_PROFILE,
                    default=self.options.get(CONF_DEFAULT_PROFILE),
                )
            ] = vol.In(profile_choices)

        return self.async_show_form(
            step_id="init",
            data_schema=vol.Schema(schema_dict),
        )
