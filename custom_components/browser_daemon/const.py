"""Constants for the Browser Daemon integration."""

from __future__ import annotations

from typing import Final
from homeassistant.const import Platform

DOMAIN: Final = "browser_daemon"

# Configuration keys
CONF_URL: Final = "url"
CONF_CLUSTER_SECRET: Final = "cluster_secret"
CONF_VERIFY_SSL: Final = "verify_ssl"
CONF_POLL_INTERVAL: Final = "poll_interval"
CONF_DEFAULT_NODE: Final = "default_node"
CONF_DEFAULT_PROFILE: Final = "default_profile"

# Defaults
DEFAULT_URL: Final = "https://browser.fnuc.ts.brkn.lol"
DEFAULT_POLL_INTERVAL: Final = 30
DEFAULT_TIMEOUT: Final = 15

# Supported platforms
PLATFORMS: Final = [
    Platform.SENSOR,
    Platform.BINARY_SENSOR,
    Platform.BUTTON,
]

# Events
EVENT_SESSION_STARTED: Final = f"{DOMAIN}_session_started"
EVENT_SESSION_STOPPED: Final = f"{DOMAIN}_session_stopped"

# Services
SERVICE_CREATE_SESSION: Final = "create_session"
SERVICE_STOP_SESSION: Final = "stop_session"
SERVICE_OPEN_TAB: Final = "open_tab"
SERVICE_CLOSE_TAB: Final = "close_tab"
SERVICE_EXPORT_AUTH: Final = "export_auth"
SERVICE_TAKE_SCREENSHOT: Final = "take_screenshot"
SERVICE_CLEAR_PROFILE: Final = "clear_profile"
SERVICE_VACUUM_RECORDINGS: Final = "vacuum_recordings"

# Attributes
ATTR_SESSION_ID: Final = "session_id"
ATTR_PROFILE: Final = "profile"
ATTR_NODE: Final = "node"
ATTR_ENGINE: Final = "engine"
ATTR_URL: Final = "url"
ATTR_TAB_ID: Final = "tab_id"
ATTR_PAGE_ID: Final = "page_id"
ATTR_STORAGE: Final = "storage"
ATTR_URLS: Final = "urls"
ATTR_NAME: Final = "name"
ATTR_SCREEN: Final = "screen"
ATTR_IDLE_TIMEOUT: Final = "idle_timeout_minutes"
ATTR_RECORD: Final = "record"
ATTR_ADBLOCK: Final = "adblock"
