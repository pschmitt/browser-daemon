"""Focused contract tests for the optional Home Assistant integration."""

from __future__ import annotations

import importlib
import importlib.util
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[2]
INTEGRATION = ROOT / "custom_components" / "browser_daemon"


class Marker:
    def __init__(self, key: str, group: str | None = None) -> None:
        self.key = key
        self.group = group


class Schema:
    def __init__(self, fields: dict[Any, Any]) -> None:
        self.fields = fields

    def __call__(self, value: dict[str, Any]) -> dict[str, Any]:
        return value


def _install_home_assistant_stubs() -> None:
    """Provide only the HA contracts needed to import and exercise the platforms."""
    ha = ModuleType("homeassistant")
    ha.__path__ = []
    sys.modules["homeassistant"] = ha

    core = ModuleType("homeassistant.core")
    core.HomeAssistant = type("HomeAssistant", (), {})
    core.ServiceCall = type("ServiceCall", (), {})
    core.SupportsResponse = SimpleNamespace(OPTIONAL="optional")
    sys.modules["homeassistant.core"] = core

    exceptions = ModuleType("homeassistant.exceptions")
    exceptions.HomeAssistantError = type("HomeAssistantError", (Exception,), {})
    sys.modules["homeassistant.exceptions"] = exceptions

    helpers = ModuleType("homeassistant.helpers")
    helpers.__path__ = []
    sys.modules["homeassistant.helpers"] = helpers

    validation = ModuleType("homeassistant.helpers.config_validation")
    validation.string = str
    validation.boolean = bool
    validation.positive_int = int
    validation.ensure_list = lambda value: value if isinstance(value, list) else [value]
    validation.ensure_list_csv = validation.ensure_list
    sys.modules["homeassistant.helpers.config_validation"] = validation

    entity_platform = ModuleType("homeassistant.helpers.entity_platform")
    entity_platform.AddEntitiesCallback = Any
    sys.modules["homeassistant.helpers.entity_platform"] = entity_platform

    config_entries = ModuleType("homeassistant.config_entries")
    config_entries.ConfigEntry = type("ConfigEntry", (), {})
    sys.modules["homeassistant.config_entries"] = config_entries

    const = ModuleType("homeassistant.const")
    const.Platform = SimpleNamespace(
        SENSOR="sensor", BINARY_SENSOR="binary_sensor", BUTTON="button", IMAGE="image"
    )
    const.EntityCategory = SimpleNamespace(DIAGNOSTIC="diagnostic")
    const.UnitOfInformation = SimpleNamespace(BYTES="B")
    const.UnitOfTime = SimpleNamespace(SECONDS="s")
    sys.modules["homeassistant.const"] = const

    class EntityDescription:
        def __init__(self, **kwargs: Any) -> None:
            self.__dict__.update(kwargs)

    sensor = ModuleType("homeassistant.components.sensor")
    sensor.SensorDeviceClass = SimpleNamespace(DATA_SIZE="data_size", DURATION="duration")
    sensor.SensorEntity = type("SensorEntity", (), {})
    sensor.SensorEntityDescription = EntityDescription
    sensor.SensorStateClass = SimpleNamespace(MEASUREMENT="measurement", TOTAL="total")
    sys.modules["homeassistant.components"] = ModuleType("homeassistant.components")
    sys.modules["homeassistant.components"].__path__ = []
    sys.modules["homeassistant.components.sensor"] = sensor

    binary_sensor = ModuleType("homeassistant.components.binary_sensor")
    binary_sensor.BinarySensorDeviceClass = SimpleNamespace(
        CONNECTIVITY="connectivity", RUNNING="running"
    )
    binary_sensor.BinarySensorEntity = type("BinarySensorEntity", (), {})
    binary_sensor.BinarySensorEntityDescription = EntityDescription
    sys.modules["homeassistant.components.binary_sensor"] = binary_sensor

    voluptuous = ModuleType("voluptuous")
    voluptuous.Schema = Schema
    voluptuous.Required = lambda key: Marker(key)
    voluptuous.Optional = lambda key, **kwargs: Marker(key)
    voluptuous.Exclusive = lambda key, group: Marker(key, group)
    voluptuous.In = lambda values: values
    voluptuous.All = lambda *values: values
    voluptuous.Range = lambda **kwargs: SimpleNamespace(**kwargs)
    voluptuous.Coerce = lambda kind: kind
    sys.modules["voluptuous"] = voluptuous

    package = ModuleType("custom_components")
    package.__path__ = [str(ROOT / "custom_components")]
    sys.modules["custom_components"] = package
    integration = ModuleType("custom_components.browser_daemon")
    integration.__path__ = [str(INTEGRATION)]
    sys.modules["custom_components.browser_daemon"] = integration

    api = ModuleType("custom_components.browser_daemon.api")
    api.BrowserDaemonError = Exception
    sys.modules[api.__name__] = api

    coordinator = ModuleType("custom_components.browser_daemon.coordinator")
    coordinator.BrowserDaemonCoordinator = type("BrowserDaemonCoordinator", (), {})
    sys.modules[coordinator.__name__] = coordinator

    entity = ModuleType("custom_components.browser_daemon.entity")

    class MeshEntity:
        def __init__(self, coordinator: Any, key: str) -> None:
            self.coordinator = coordinator
            self._key = key

    class NodeEntity(MeshEntity):
        def __init__(self, coordinator: Any, node_name: str, key: str) -> None:
            super().__init__(coordinator, key)
            self._node_name = node_name

    class BrowserEntity(MeshEntity):
        def __init__(self, coordinator: Any, browser_id: str, key: str) -> None:
            super().__init__(coordinator, key)
            self._browser_id = browser_id

    entity.BrowserDaemonEntity = MeshEntity
    entity.BrowserDaemonBrowserEntity = BrowserEntity
    entity.BrowserDaemonNodeEntity = NodeEntity
    sys.modules[entity.__name__] = entity


_install_home_assistant_stubs()
services = importlib.import_module("custom_components.browser_daemon.services")
sensor_platform = importlib.import_module("custom_components.browser_daemon.sensor")
binary_sensor_platform = importlib.import_module("custom_components.browser_daemon.binary_sensor")
constants = importlib.import_module("custom_components.browser_daemon.const")
api_spec = importlib.util.spec_from_file_location("ha_browser_daemon_api", INTEGRATION / "api.py")
api = importlib.util.module_from_spec(api_spec)
assert api_spec.loader is not None
api_spec.loader.exec_module(api)


class FakeClient:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []

    async def async_set_idle_timeout(self, **kwargs: Any) -> dict[str, Any]:
        self.calls.append(("set_idle_timeout", kwargs))
        return {"success": True, "idle_timeout_minutes": kwargs["minutes"]}

    async def async_open_tab(self, **kwargs: Any) -> dict[str, Any]:
        self.calls.append(("open_tab", kwargs))
        return {"id": "tab"}

    async def async_login(self, **kwargs: Any) -> dict[str, Any]:
        self.calls.append(("login", kwargs))
        return {"outcome": "needs_totp"}


class FakeCoordinator(
    sys.modules["custom_components.browser_daemon.coordinator"].BrowserDaemonCoordinator
):
    def __init__(self, data: dict[str, Any] | None = None) -> None:
        self.data = data or {"nodes": {}}
        self.client = FakeClient()
        self.listeners: list[Any] = []

    def async_add_listener(self, listener: Any) -> Any:
        self.listeners.append(listener)

        def remove() -> None:
            self.listeners.remove(listener)

        return remove


class FakeServices:
    def __init__(self) -> None:
        self.registered: dict[str, dict[str, Any]] = {}

    def async_register(self, domain: str, name: str, handler: Any, **kwargs: Any) -> None:
        self.registered[name] = {"handler": handler, **kwargs}

    def async_remove(self, domain: str, name: str) -> None:
        self.registered.pop(name, None)


class FakeResponse:
    def __init__(
        self,
        status: int,
        body: dict[str, Any] | None = None,
        *,
        content_type: str = "application/json",
    ) -> None:
        self.status = status
        self.body = body
        self.content_type = content_type
        self.content_length = 0 if body is None else 1

    async def json(self) -> dict[str, Any]:
        if self.body is None:
            raise ValueError("response has no JSON body")
        return self.body

    async def text(self) -> str:
        return ""


class FakeHttpSession:
    def __init__(self, response: FakeResponse) -> None:
        self.response = response
        self.requests: list[dict[str, Any]] = []

    async def request(self, method: str, url: str, **kwargs: Any) -> FakeResponse:
        self.requests.append({"method": method, "url": url, **kwargs})
        return self.response


@pytest.fixture
def service_context():
    coordinator = FakeCoordinator()
    hass = SimpleNamespace(data={constants.DOMAIN: {"entry": coordinator}}, services=FakeServices())
    return hass, coordinator


@pytest.mark.asyncio
async def test_idle_timeout_service_accepts_zero_and_open_tab_targets_session(service_context):
    hass, coordinator = service_context
    await services.async_setup_services(hass)

    idle_schema = hass.services.registered[constants.SERVICE_SET_IDLE_TIMEOUT]["schema"]
    assert any(
        marker.key == constants.ATTR_IDLE_TIMEOUT
        and any(getattr(rule, "min", None) == 0 for rule in rules)
        for marker, rules in idle_schema.fields.items()
        if isinstance(marker, Marker)
    )

    result = await hass.services.registered[constants.SERVICE_SET_IDLE_TIMEOUT]["handler"](
        SimpleNamespace(
            data={
                "entry_id": "entry",
                constants.ATTR_SESSION_ID: "session",
                constants.ATTR_IDLE_TIMEOUT: 0,
            }
        )
    )
    assert result == {"success": True, "idle_timeout_minutes": 0}
    assert coordinator.client.calls[-1] == (
        "set_idle_timeout",
        {"session_id": "session", "minutes": 0, "node": None},
    )

    result = await hass.services.registered[constants.SERVICE_OPEN_TAB]["handler"](
        SimpleNamespace(
            data={"entry_id": "entry", "url": "https://example.test", "session_id": "session"}
        )
    )
    assert result == {"id": "tab"}
    assert coordinator.client.calls[-1] == (
        "open_tab",
        {"url": "https://example.test", "session_id": "session", "node": None},
    )

    await services.async_unload_services(hass)
    assert constants.SERVICE_LOGIN not in hass.services.registered
    assert constants.SERVICE_SET_IDLE_TIMEOUT not in hass.services.registered


@pytest.mark.asyncio
async def test_login_service_returns_expected_interaction_state(service_context):
    hass, coordinator = service_context
    await services.async_setup_services(hass)
    login_schema = hass.services.registered[constants.SERVICE_LOGIN]["schema"]
    totp_fields = [
        marker for marker in login_schema.fields if isinstance(marker, Marker) and marker.group
    ]
    assert {marker.key for marker in totp_fields} == {"totp_secret", "totp_code"}
    assert {marker.group for marker in totp_fields} == {"totp"}
    call = SimpleNamespace(
        data={
            "entry_id": "entry",
            "session_id": "session",
            "tab": "tab",
            "username": "user",
            "password": "password-value",
            "totp_code": "123456",
            "success_selector": ".account-home",
        }
    )

    result = await hass.services.registered[constants.SERVICE_LOGIN]["handler"](call)

    assert result == {"outcome": "needs_totp"}
    assert coordinator.client.calls[-1] == (
        "login",
        {
            "session_id": "session",
            "tab": "tab",
            "username": "user",
            "password": "password-value",
            "totp_code": "123456",
            "success_selector": ".account-home",
            "timeout_seconds": 30,
        },
    )


@pytest.mark.asyncio
async def test_api_login_uses_cluster_secret_and_returns_needs_totp():
    http = FakeHttpSession(FakeResponse(200, {"outcome": "needs_totp"}))
    client = api.BrowserDaemonClient(http, "http://daemon.test", cluster_secret="cluster-secret")

    result = await client.async_login(
        "session",
        tab="tab",
        username="user",
        password="pass",
        totp_secret="seed",
        success_selector=".account-home",
    )

    assert result == {"outcome": "needs_totp"}
    request = http.requests[0]
    assert request["method"] == "POST"
    assert request["url"].endswith("/api/sessions/session/login")
    assert request["headers"]["X-Browser-Cluster-Secret"] == "cluster-secret"
    assert request["json"] == {
        "tab": "tab",
        "username": "user",
        "password": "pass",
        "totp_secret": "seed",
        "success_selector": ".account-home",
        "timeout_seconds": 30,
    }


@pytest.mark.asyncio
async def test_api_login_requires_cluster_secret():
    http = FakeHttpSession(FakeResponse(200, {"outcome": "authenticated"}))
    client = api.BrowserDaemonClient(http, "http://daemon.test")

    with pytest.raises(api.BrowserDaemonError, match="cluster secret"):
        await client.async_login("session", tab="tab", username="user", password="pass")

    assert http.requests == []


@pytest.mark.asyncio
async def test_idle_timeout_handles_204_without_json():
    http = FakeHttpSession(FakeResponse(204))
    client = api.BrowserDaemonClient(http, "http://daemon.test", cluster_secret="secret")

    result = await client.async_set_idle_timeout("session", 0)

    assert result == {
        "success": True,
        "session_id": "session",
        "idle_timeout_minutes": 0,
    }


@pytest.mark.asyncio
@pytest.mark.parametrize("platform", [sensor_platform, binary_sensor_platform])
async def test_new_nodes_are_added_once_and_listener_is_removed(platform):
    coordinator = FakeCoordinator({"nodes": {"initial": {}}})
    hass = SimpleNamespace(data={constants.DOMAIN: {"entry": coordinator}})
    unload_callbacks: list[Any] = []
    entry = SimpleNamespace(entry_id="entry", async_on_unload=unload_callbacks.append)
    batches: list[list[Any]] = []

    await platform.async_setup_entry(hass, entry, batches.append)
    assert len(coordinator.listeners) == 1
    coordinator.data["nodes"]["new"] = {}
    coordinator.listeners[0]()
    new_batch = batches[-1]
    assert new_batch
    assert all(entity._node_name == "new" for entity in new_batch)

    coordinator.listeners[0]()
    assert len(batches) == 2
    unload_callbacks[0]()
    assert coordinator.listeners == []


def test_recording_and_storage_sensors_are_gauges():
    descriptions = {
        description.key: description for description in sensor_platform.MESH_SENSOR_DESCRIPTIONS
    }
    assert descriptions["total_recordings"].state_class == "measurement"
    assert descriptions["total_storage_bytes"].state_class == "measurement"
    node_descriptions = {
        description.key: description for description in sensor_platform.NODE_SENSOR_DESCRIPTIONS
    }
    assert node_descriptions["storage_used_bytes"].state_class == "measurement"


def test_counter_attributes_list_profiles_sessions_and_nodes():
    data = {
        "local_node": {"name": "a"},
        "nodes": {
            "a": {"status": "online", "version": "1", "storage_used_bytes": 5},
            "b": {"status": "offline", "node_name": "b"},
        },
        "profiles": [
            {"id": "p1", "name": "work", "engine": "chromium", "size_bytes": 1, "x": 1},
            {"id": "p2", "name": "home", "node_name": "b"},
        ],
        "sessions": [{"id": "s1", "name": "one", "tabs_count": 2, "profile": "work"}],
    }
    profiles = sensor_platform.mesh_attributes("total_profiles", data)["profiles"]
    assert [(p["name"], p["node"]) for p in profiles] == [("work", "a"), ("home", "b")]
    assert "x" not in profiles[0]
    sessions = sensor_platform.mesh_attributes("total_sessions", data)["sessions"]
    assert sessions == [{"id": "s1", "name": "one", "node": "a", "profile": "work", "tabs": 2}]
    online = sensor_platform.mesh_attributes("online_nodes", data)
    assert online == {"online": ["a"], "offline": ["b"]}
    per_node = sensor_platform.node_attributes("storage_used_bytes", "b", data)
    assert [p["name"] for p in per_node["profiles"]] == ["home"]


@pytest.fixture
def integration(monkeypatch):
    """The package's public API, on the real exceptions."""
    session = ModuleType("homeassistant.helpers.aiohttp_client")
    session.async_get_clientsession = lambda hass, verify_ssl=True: ("session", verify_ssl)
    monkeypatch.setitem(sys.modules, session.__name__, session)
    monkeypatch.setitem(sys.modules, "custom_components.browser_daemon.api", api)
    monkeypatch.setattr(
        sys.modules["homeassistant.helpers.config_validation"],
        "empty_config_schema",
        lambda domain: None,
        raising=False,
    )
    spec = importlib.util.spec_from_file_location(
        "custom_components.browser_daemon",
        INTEGRATION / "__init__.py",
        submodule_search_locations=[str(INTEGRATION)],
    )
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def _daemon_entries(*entries: SimpleNamespace) -> SimpleNamespace:
    return SimpleNamespace(
        config_entries=SimpleNamespace(async_entries=lambda domain: list(entries))
    )


def _daemon_entry(entry_id: str, url: str, secret: str | None, *, disabled_by=None):
    return SimpleNamespace(
        entry_id=entry_id,
        disabled_by=disabled_by,
        data={"url": url, "cluster_secret": secret, "verify_ssl": False},
    )


def test_get_daemon_builds_a_client_from_the_selected_entry(integration):
    from browser_daemon_client import BrowserDaemon

    hass = _daemon_entries(
        _daemon_entry("off", "http://off", "x", disabled_by="user"),
        _daemon_entry("a", "http://a/", " secret-a "),
        _daemon_entry("b", "http://b", None),
    )

    class Custom(BrowserDaemon):
        pass

    first = integration.async_get_daemon(hass, client="Test")
    assert (first.url, first.secret, first.client) == ("http://a", "secret-a", "Test")
    assert first.session == ("session", False)

    selected = integration.async_get_daemon(hass, client="Test", cls=Custom, entry_id="b")
    assert isinstance(selected, Custom)
    assert (selected.url, selected.secret) == ("http://b", None)


@pytest.mark.parametrize("entry_id", ["gone", "off"])
def test_get_daemon_never_falls_back_to_another_entry(integration, entry_id):
    hass = _daemon_entries(
        _daemon_entry("off", "http://off", "x", disabled_by="user"),
        _daemon_entry("a", "http://a", "s"),
    )
    with pytest.raises(integration.BrowserDaemonNotConfiguredError):
        integration.async_get_daemon(hass, client="Test", entry_id=entry_id)


def test_get_coordinator_never_falls_back_to_another_entry(integration):
    coordinator = integration.BrowserDaemonCoordinator()
    hass = SimpleNamespace(data={integration.DOMAIN: {"a": coordinator}})
    assert integration.async_get_coordinator(hass) is coordinator
    assert integration.async_get_coordinator(hass, "a") is coordinator
    assert integration.async_get_coordinator(hass, "gone") is None
