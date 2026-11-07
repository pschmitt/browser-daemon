# browser-daemon-client

Async Python client for [browser-daemon](https://github.com/pschmitt/browser-daemon),
with nothing but `aiohttp`: made for Home Assistant integrations and other code
that cannot carry Playwright.

```python
import aiohttp
from browser_daemon_client import BrowserDaemon, RunError, step

async with aiohttp.ClientSession() as http:
    bd = BrowserDaemon(
        http, "https://browser.example.org", secret, client="Home Assistant · HR WORKS"
    )
    try:
        res = await bd.run(
            profile="work",
            url="https://hr.example.org/login",
            steps=[
                step.fill("#user", arg="user"),
                step.fill("#password", arg="password"),
                step.click("role=button[name='Sign in']"),
                step.wait_for(".dashboard"),
                step.text(".balance", as_="balance"),
            ],
            args={"user": user, "password": password},
        )
    except RunError as err:
        print(err, err.run.url)  # err.run.screenshot: the tab, as a base64 PNG
    else:
        print(res.result["balance"])
```

- `run()`: a Playwright `script` (a JavaScript function expression) or `steps`,
  in a profile's browser, a session or a throwaway browser. Runs for a profile
  go to the node that has it. See [docs/run.md](../docs/run.md).
- `login(session_id, tab, username, password, ...)`: submit credentials in an
  existing Chromium tab and return a `LoginResult.outcome`. Set
  `success_selector` to visible page evidence before treating the result as
  authenticated; see [docs/run.md](../docs/run.md#log-in-to-an-existing-tab).
- `overview()`, `find_profile()`, `sessions()`, `session()`, `create_session()`,
  `delete_session()`, `set_idle_timeout()`, `tabs()`, `open_tab()`,
  `close_tab()`, `export_auth()` (a page's cookies and storage; never log them).
- `place_browser(profile=..., node=..., group=..., engine=...)` selects a
  matching node and moves an idle existing profile there. `create=True` permits
  a missing profile for a subsequent session or run; it reserves nothing.
- `create_session(profile="work", group="cloud")` places the profile before
  opening it and returns `node_name` for subsequent node-specific calls.
  Supplying both `profile` and `node` also enables automatic placement.
  See [profile placement](../docs/profiles.md#automatic-placement-and-node-labels)
  for transfer restrictions and failure behavior.
- `create_session(ephemeral_node=True)` provisions a dedicated node and returns
  its `node_url` alongside the session. Connect to that node with the session ID;
  the session and node are removed after the last client disconnects.
- `node=` sends a request to another node of the mesh through this one.
- `read_secret()` reads the cluster secret from its file.

Errors are `BrowserDaemonError` (with `status`), and for runs `RunError` and
`RunTimeout` (with the whole answer in `.run`).

## Releasing

The client has the daemon's version and is released with it: `just release-prepare X.Y.Z`
bumps `pyproject.toml` with `Cargo.toml`, and the signed `vX.Y.Z` tag makes the
`python` job of the release workflow publish it to PyPI through trusted
publishing (the `pypi` environment; no token is stored anywhere). `test.yaml`
tests it on every change.
