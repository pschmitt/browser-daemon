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
- `overview()`, `find_profile()`, `sessions()`, `session()`, `create_session()`,
  `delete_session()`, `set_idle_timeout()`, `tabs()`, `open_tab()`,
  `close_tab()`, `export_auth()` (a page's cookies and storage; never log them).
- `node=` sends a request to another node of the mesh through this one.
- `read_secret()` reads the cluster secret from its file.

Errors are `BrowserDaemonError` (with `status`), and for runs `RunError` and
`RunTimeout` (with the whole answer in `.run`).

## Releasing

The client has the daemon's version and is released with it: `just release-prepare X.Y.Z`
bumps `pyproject.toml` with `Cargo.toml`, and the signed `vX.Y.Z` tag makes the
`python-client` workflow test, build and publish it to PyPI through trusted
publishing (the `pypi` environment; no token is stored anywhere).
