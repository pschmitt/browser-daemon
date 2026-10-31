"""Custom services for Browser Daemon integration."""

from __future__ import annotations

import base64
import logging
from typing import Any

import voluptuous as vol
from homeassistant.core import HomeAssistant, ServiceCall, SupportsResponse
from homeassistant.helpers import config_validation as cv

from .const import (
    ATTR_ADBLOCK,
    ATTR_ENGINE,
    ATTR_IDLE_TIMEOUT,
    ATTR_NAME,
    ATTR_NODE,
    ATTR_PAGE_ID,
    ATTR_PROFILE,
    ATTR_RECORD,
    ATTR_SCREEN,
    ATTR_SESSION_ID,
    ATTR_STORAGE,
    ATTR_TAB_ID,
    ATTR_URL,
    ATTR_URLS,
    DOMAIN,
    SERVICE_CLEAR_PROFILE,
    SERVICE_CLOSE_TAB,
    SERVICE_CREATE_SESSION,
    SERVICE_EXPORT_AUTH,
    SERVICE_OPEN_TAB,
    SERVICE_STOP_SESSION,
    SERVICE_TAKE_SCREENSHOT,
    SERVICE_VACUUM_RECORDINGS,
)
from .coordinator import BrowserDaemonCoordinator

_LOGGER = logging.getLogger(__name__)


def _get_coordinator(
    hass: HomeAssistant, entry_id: str | None = None
) -> BrowserDaemonCoordinator | None:
    """Retrieve the coordinator for the specified entry_id or the first available."""
    domain_data = hass.data.get(DOMAIN, {})
    if entry_id and entry_id in domain_data:
        return domain_data[entry_id]
    for key, val in domain_data.items():
        if isinstance(val, BrowserDaemonCoordinator):
            return val
    return None


async def async_setup_services(hass: HomeAssistant) -> None:
    """Register Browser Daemon actions/services."""

    async def handle_create_session(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            _LOGGER.error("No Browser Daemon coordinator found")
            return {"error": "no_coordinator"}

        session = await coordinator.client.async_create_session(
            profile=call.data.get(ATTR_PROFILE),
            engine=call.data.get(ATTR_ENGINE),
            node=call.data.get(ATTR_NODE),
            name=call.data.get(ATTR_NAME),
            screen=call.data.get(ATTR_SCREEN),
            idle_timeout_minutes=call.data.get(ATTR_IDLE_TIMEOUT),
            record=call.data.get(ATTR_RECORD),
            adblock=call.data.get(ATTR_ADBLOCK),
            default_tabs=call.data.get("default_tabs"),
        )
        await coordinator.async_request_refresh()
        return session

    async def handle_stop_session(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        success = await coordinator.client.async_delete_session(
            session_id=call.data[ATTR_SESSION_ID],
            node=call.data.get(ATTR_NODE),
        )
        await coordinator.async_request_refresh()
        return {"success": success}

    async def handle_open_tab(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        res = await coordinator.client.async_open_tab(
            url=call.data[ATTR_URL],
            node=call.data.get(ATTR_NODE),
        )
        return res

    async def handle_close_tab(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        success = await coordinator.client.async_close_tab(
            tab_id=call.data[ATTR_TAB_ID],
            node=call.data.get(ATTR_NODE),
        )
        return {"success": success}

    async def handle_export_auth(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        auth = await coordinator.client.async_export_auth(
            page_id=call.data[ATTR_PAGE_ID],
            session_id=call.data.get(ATTR_SESSION_ID),
            urls=call.data.get(ATTR_URLS),
            storage=call.data.get(ATTR_STORAGE, True),
            node=call.data.get(ATTR_NODE),
        )
        return auth

    async def handle_take_screenshot(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        image_bytes = await coordinator.client.async_screenshot(
            session_id=call.data[ATTR_SESSION_ID],
            node=call.data.get(ATTR_NODE),
        )
        encoded = base64.b64encode(image_bytes).decode("ascii")
        return {"image_base64": encoded}

    async def handle_clear_profile(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        success = await coordinator.client.async_clear_profile(
            name=call.data[ATTR_PROFILE],
            node=call.data.get(ATTR_NODE),
        )
        await coordinator.async_request_refresh()
        return {"success": success}

    async def handle_vacuum_recordings(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        res = await coordinator.client.async_vacuum_recordings(
            node=call.data.get(ATTR_NODE)
        )
        await coordinator.async_request_refresh()
        return res

    # Register services
    hass.services.async_register(
        DOMAIN,
        SERVICE_CREATE_SESSION,
        handle_create_session,
        schema=vol.Schema(
            {
                vol.Optional(ATTR_PROFILE): cv.string,
                vol.Optional(ATTR_ENGINE): vol.In(["chromium", "firefox"]),
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional(ATTR_NAME): cv.string,
                vol.Optional(ATTR_SCREEN): vol.In(["desktop", "phone"]),
                vol.Optional(ATTR_IDLE_TIMEOUT): cv.positive_int,
                vol.Optional(ATTR_RECORD): cv.boolean,
                vol.Optional(ATTR_ADBLOCK): cv.boolean,
                vol.Optional("default_tabs"): cv.boolean,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_STOP_SESSION,
        handle_stop_session,
        schema=vol.Schema(
            {
                vol.Required(ATTR_SESSION_ID): cv.string,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_OPEN_TAB,
        handle_open_tab,
        schema=vol.Schema(
            {
                vol.Required(ATTR_URL): cv.string,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_CLOSE_TAB,
        handle_close_tab,
        schema=vol.Schema(
            {
                vol.Required(ATTR_TAB_ID): cv.string,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_EXPORT_AUTH,
        handle_export_auth,
        schema=vol.Schema(
            {
                vol.Required(ATTR_PAGE_ID): cv.string,
                vol.Optional(ATTR_SESSION_ID): cv.string,
                vol.Optional(ATTR_URLS): cv.ensure_list_csv,
                vol.Optional(ATTR_STORAGE, default=True): cv.boolean,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_TAKE_SCREENSHOT,
        handle_take_screenshot,
        schema=vol.Schema(
            {
                vol.Required(ATTR_SESSION_ID): cv.string,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_CLEAR_PROFILE,
        handle_clear_profile,
        schema=vol.Schema(
            {
                vol.Required(ATTR_PROFILE): cv.string,
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_VACUUM_RECORDINGS,
        handle_vacuum_recordings,
        schema=vol.Schema(
            {
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )


async def async_unload_services(hass: HomeAssistant) -> None:
    """Unload Browser Daemon services."""
    services = [
        SERVICE_CREATE_SESSION,
        SERVICE_STOP_SESSION,
        SERVICE_OPEN_TAB,
        SERVICE_CLOSE_TAB,
        SERVICE_EXPORT_AUTH,
        SERVICE_TAKE_SCREENSHOT,
        SERVICE_CLEAR_PROFILE,
        SERVICE_VACUUM_RECORDINGS,
    ]
    for service in services:
        hass.services.async_remove(DOMAIN, service)
