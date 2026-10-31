# Browser Daemon Custom Integration for Home Assistant

A state-of-the-art Home Assistant custom component for managing, observing, and controlling a [browser-daemon](https://github.com/pschmitt/browser-daemon) mesh across your infrastructure.

## Features

- **Mesh Topology & Telemetry**: Automatically discovers the local node and all interconnected peer nodes (e.g. `fnuc`, `rofl-13`, `rofl-14`, `rofl-15`, `rofl-16`).
- **Rich Entity Model**:
  - **Hub Device**: `Browser Daemon Mesh (<Node>)` representing cluster-wide metrics.
  - **Node Devices**: Per-node devices linked via `via_device`, reporting latency, uptime, active session count, storage used, and online/offline status.
  - **Sensors**: Active sessions, online nodes, total nodes, saved profiles, storage consumption, storage limits, and node ping latencies.
  - **Binary Sensors**: Mesh connectivity, node reachability, and active session indicators.
  - **Buttons**: Mesh refresh and recording vacuuming.
- **HA Services / Actions**:
  - `browser_daemon.create_session`: Launch ad-hoc or profile-backed browser sessions (with Chromium/Firefox, adblock, recording, phone/desktop screen emulation, and idle timeouts).
  - `browser_daemon.stop_session`: Terminate running sessions by session UUID.
  - `browser_daemon.open_tab`: Open a tab on a node or session.
  - `browser_daemon.close_tab`: Close an open Chromium target/tab.
  - `browser_daemon.export_auth`: Export cookies and `localStorage` (Playwright `storageState` format) for automated token extraction.
  - `browser_daemon.take_screenshot`: Capture live JPEG screenshots from active browser sessions.
  - `browser_daemon.clear_profile`: Reset stored cookies and state for named profiles.
  - `browser_daemon.vacuum_recordings`: Clean up expired session recordings across the cluster.
  - `browser_daemon.run`: Run a Playwright script or a list of steps in a browser of the mesh and return the result (see [Runs](#runs-browser_daemonrun)).
- **Event Bus Integration**: Fires `browser_daemon_session_started` and `browser_daemon_session_stopped` events when sessions start or stop.
- **Zero-Friction Authentication**: Automatically fetches and caches the `bd_auth` token from the dashboard, or accepts an explicit cluster secret (`X-Browser-Cluster-Secret`).
- **Python API for Dependent Integrations**: Designed from the ground up as a foundation for other integrations (e.g., `hrworks`, `msteams`).

---

## Cross-Integration Usage (Python API)

Other Home Assistant integrations can interact directly with `browser_daemon` without re-implementing browser connections, tunnels, or auth tokens.

### 1. Declare Dependency
In your integration's `manifest.json`:

```json
{
  "after_dependencies": ["browser_daemon"]
}
```

### 2. Calling the Python API

```python
from custom_components.browser_daemon import (
    async_get_client,
    async_get_coordinator,
    BrowserDaemonError,
)

# Retrieve the client instance
client = async_get_client(hass)
if client:
    # Launch a session for a profile
    session = await client.async_create_session(
        profile="hrworks",
        engine="chromium",
    )
    session_id = session["id"]

    # Export auth/storage state for a page
    auth_state = await client.async_export_auth(
        page_id=...,
        storage=True,
    )

    # Close the session when done
    await client.async_delete_session(session_id)
```

You can also inspect the coordinator:

```python
coordinator = async_get_coordinator(hass)
if coordinator:
    # Check if a profile is currently in use
    active_session = coordinator.get_active_session_for_profile("hrworks")

    # Check online node availability
    online_nodes = coordinator.get_online_nodes()
```

---

## Runs (`browser_daemon.run`)

Runs a Playwright script or a list of steps on the node that has the browser
(see [docs/run.md](../../docs/run.md)) and returns the run's answer: `ok`,
`result`, `error`, `url`, `steps`, `logs`. Runs need the **cluster secret** in
the integration (Settings → Devices & services → Browser Daemon → Reconfigure);
a viewer token is not enough, since a run executes code on the node.

```yaml
action: browser_daemon.run
data:
  profile: work
  url: https://example.org/account
  steps:
    - action: fill
      selector: "#password"
      value_arg: password
    - action: click
      selector: "role=button[name='Sign in']"
    - action: text
      selector: .balance
      as: balance
  args:
    password: !secret example_password
response_variable: run
```

`run.result.balance` then holds the text. A failed run makes the action fail
(`fail_on_error: false` returns the answer with `ok: false` instead), and a
screenshot of the tab comes with it only with `screenshot_on_error: true`.
Values in `args` are never logged by the daemon; a `script` gets them as `args`,
steps take them with `value_arg`.

## Installation

Through HACS: add `https://github.com/pschmitt/browser-daemon` as a custom
repository of type *Integration*, or copy `custom_components/browser_daemon` to
your configuration. The integration is released with the daemon and has its
version.

## Configuration

Set up via Home Assistant UI:
**Settings $\to$ Devices & Services $\to$ Add Integration $\to$ Browser Daemon**

- **URL**: Default is `https://browser.fnuc.ts.brkn.lol`
- **Cluster Secret**: Optional. If omitted, the integration automatically negotiates session tokens.
- **Verify SSL**: Enabled by default.
