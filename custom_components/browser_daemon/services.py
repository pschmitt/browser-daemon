"""Custom services for Browser Daemon integration."""

from __future__ import annotations

import base64
import logging
from typing import Any

import voluptuous as vol
from homeassistant.core import HomeAssistant, ServiceCall, SupportsResponse
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import config_validation as cv

from .api import BrowserDaemonError
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
    SERVICE_LOGIN,
    SERVICE_OPEN_TAB,
    SERVICE_RUN,
    SERVICE_SET_IDLE_TIMEOUT,
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
    for val in domain_data.values():
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

    async def handle_set_idle_timeout(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            raise HomeAssistantError("No Browser Daemon entry is set up")

        return await coordinator.client.async_set_idle_timeout(
            session_id=call.data[ATTR_SESSION_ID],
            minutes=call.data[ATTR_IDLE_TIMEOUT],
            node=call.data.get(ATTR_NODE),
        )

    async def handle_login(call: ServiceCall) -> dict[str, Any]:
        """Log in to a page using credentials supplied for this call only."""
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            raise HomeAssistantError("No Browser Daemon entry is set up")

        payload = {
            "session_id": call.data[ATTR_SESSION_ID],
            "tab": call.data["tab"],
            "username": call.data["username"],
            "password": call.data["password"],
            "totp_secret": call.data.get("totp_secret"),
            "totp_code": call.data.get("totp_code"),
            "allowed_origins": call.data.get("allowed_origins"),
            "success_selector": call.data.get("success_selector"),
            "timeout_seconds": call.data.get("timeout_seconds", 30),
            "node": call.data.get(ATTR_NODE),
        }
        return await coordinator.client.async_login(
            **{key: value for key, value in payload.items() if value is not None}
        )

    async def handle_open_tab(call: ServiceCall) -> dict[str, Any]:
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            return {"error": "no_coordinator"}

        res = await coordinator.client.async_open_tab(
            url=call.data[ATTR_URL],
            session_id=call.data.get(ATTR_SESSION_ID),
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

    async def handle_run(call: ServiceCall) -> dict[str, Any]:
        """Run a Playwright script or steps (docs/run.md) and answer with the result."""
        coordinator = _get_coordinator(hass, call.data.get("entry_id"))
        if not coordinator:
            raise HomeAssistantError("No Browser Daemon entry is set up")
        if ("script" in call.data) == ("steps" in call.data):
            raise HomeAssistantError("Give a script or steps")
        payload = {
            "script": call.data.get("script"),
            "steps": call.data.get("steps"),
            # Values for the script and `value_arg` of steps: never logged.
            "args": call.data.get("args"),
            "profile": call.data.get(ATTR_PROFILE),
            "session_id": call.data.get(ATTR_SESSION_ID),
            "tab": call.data.get("tab"),
            "url": call.data.get(ATTR_URL),
            "keep_open": call.data.get("keep_open"),
            "persistent": call.data.get("persistent"),
            "timeout_seconds": call.data.get("timeout"),
            "action_timeout_seconds": call.data.get("action_timeout"),
            "screenshot_on_error": call.data.get("screenshot_on_error"),
            "idle_timeout_minutes": call.data.get(ATTR_IDLE_TIMEOUT),
            "client": "Home Assistant",
        }
        try:
            answer = await coordinator.client.async_run(
                {k: v for k, v in payload.items() if v is not None},
                node=call.data.get(ATTR_NODE),
            )
        except BrowserDaemonError as err:
            raise HomeAssistantError(str(err)) from err
        if not answer.get("ok") and call.data.get("fail_on_error"):
            raise HomeAssistantError(
                f"Run failed: {answer.get('error') or 'unknown error'}"
            )
        return answer

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
        SERVICE_SET_IDLE_TIMEOUT,
        handle_set_idle_timeout,
        schema=vol.Schema(
            {
                vol.Required(ATTR_SESSION_ID): cv.string,
                vol.Required(ATTR_IDLE_TIMEOUT): vol.All(
                    vol.Coerce(int), vol.Range(min=0)
                ),
                vol.Optional(ATTR_NODE): cv.string,
                vol.Optional("entry_id"): cv.string,
            }
        ),
        supports_response=SupportsResponse.OPTIONAL,
    )

    hass.services.async_register(
        DOMAIN,
        SERVICE_LOGIN,
        handle_login,
        schema=vol.Schema(
            {
                vol.Required(ATTR_SESSION_ID): cv.string,
                vol.Required("tab"): cv.string,
                vol.Required("username"): cv.string,
                vol.Required("password"): cv.string,
                vol.Exclusive("totp_secret", "totp"): cv.string,
                vol.Exclusive("totp_code", "totp"): cv.string,
                vol.Optional("allowed_origins"): vol.All(cv.ensure_list, [cv.string]),
                vol.Optional("success_selector"): cv.string,
                vol.Optional("timeout_seconds", default=30): vol.All(
                    vol.Coerce(int), vol.Range(min=1, max=90)
                ),
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
                vol.Optional(ATTR_SESSION_ID): cv.string,
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
        SERVICE_RUN,
        handle_run,
        schema=vol.Schema(
            {
                vol.Exclusive("script", "code"): cv.string,
                vol.Exclusive("steps", "code"): vol.All(cv.ensure_list, [dict]),
                vol.Optional("args"): dict,
                vol.Exclusive(ATTR_PROFILE, "browser"): cv.string,
                vol.Exclusive(ATTR_SESSION_ID, "browser"): cv.string,
                vol.Optional("tab"): cv.string,
                vol.Optional(ATTR_URL): cv.string,
                vol.Optional("keep_open"): cv.boolean,
                vol.Optional("persistent"): cv.boolean,
                vol.Optional("timeout", default=60): vol.All(
                    vol.Coerce(int), vol.Range(min=1, max=600)
                ),
                vol.Optional("action_timeout"): vol.All(
                    vol.Coerce(int), vol.Range(min=1, max=600)
                ),
                # A base64 PNG in every failed automation trace is too much by default.
                vol.Optional("screenshot_on_error", default=False): cv.boolean,
                vol.Optional("fail_on_error", default=True): cv.boolean,
                vol.Optional(ATTR_IDLE_TIMEOUT): vol.All(
                    vol.Coerce(int), vol.Range(min=0)
                ),
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
        SERVICE_SET_IDLE_TIMEOUT,
        SERVICE_LOGIN,
        SERVICE_OPEN_TAB,
        SERVICE_CLOSE_TAB,
        SERVICE_EXPORT_AUTH,
        SERVICE_TAKE_SCREENSHOT,
        SERVICE_CLEAR_PROFILE,
        SERVICE_VACUUM_RECORDINGS,
        SERVICE_RUN,
    ]
    for service in services:
        hass.services.async_remove(DOMAIN, service)
