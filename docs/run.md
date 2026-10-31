# Runs: Playwright over HTTP

`POST /api/run` runs a Playwright script, or a list of steps, in a browser of the node and answers with the result. Playwright runs on the node, so a client needs nothing but HTTP: a Home Assistant integration (where the Playwright Python package does not install), a shell script with `curl`, n8n, or another language.

```sh
curl -s https://browser.example.org/api/run \
  -H "X-Browser-Cluster-Secret: $(cat /run/secrets/browser-daemon/cluster-secret)" \
  -H 'Content-Type: application/json' \
  -d '{
    "profile": "work",
    "url": "https://example.org/account",
    "steps": [
      { "action": "text", "selector": "#balance", "as": "balance" },
      { "action": "screenshot", "as": "picture" }
    ]
  }'
```

```json
{ "ok": true, "result": { "balance": "-1:30", "picture": "iVBORw0..." },
  "steps": [{ "action": "text", "ms": 12 }, { "action": "screenshot", "ms": 140 }],
  "url": "https://example.org/account", "logs": [], "ms": 2210 }
```

Runs are off unless the node is started with `--run-node` and `--playwright-core` (NixOS: `services.browser-daemon.run.enable`), and they need a cluster secret on the node.

## The browser

| Field | |
| --- | --- |
| `profile` | Run in this profile's browser: its session when it runs, else a new one. Asked of any node, the run goes to the node that has the profile. |
| `session_id` | Run in this running session (of this node). |
| neither | A throwaway browser, which ends with the run. |
| `tab` | A tab of that session (its page id, as in `/json/list`) to run in. Without it the run opens a new tab and closes it at the end. |
| `keep_open` | Leave the run's tab open; its page id is in the answer's `page`, for a later run's `tab`. Implies `persistent`. |
| `persistent` | A session the run starts stays when the run ends, until its idle timeout (`idle_timeout_minutes`). Without it, a session the run started ends with the run; a session that was already running is never stopped. |
| `url` | Open this address before the script or steps. |
| `client` | Who runs it, shown on the dashboard (default `Run API`). |

Runs work in Chromium sessions only. A profile's session is shared with the live view and other clients: a run that opens its own tab leaves their tabs alone.

To run on a given node, send the run to that node, or through any node as `POST /api/peers/{node}/run`.

## Scripts

`script` is a JavaScript function expression. It gets Playwright's `page` (the run's tab), `context`, `browser`, the request's `args` and `log(text)`, and its return value is the answer's `result`:

```json
{
  "profile": "work",
  "args": { "month": "2026-10" },
  "script": "async ({ page, args, log }) => { await page.goto(`https://hr.example.org/time?m=${args.month}`); await page.getByRole('button', { name: 'Show' }).click(); log('loaded'); return { hours: await page.locator('.total').innerText() }; }"
}
```

The result must be JSON; buffers (screenshots) become base64 strings. The script has the whole [Playwright API](https://playwright.dev/docs/api/class-page) of the node's playwright-core.

## Steps

`steps` is a list of actions, run in order, for jobs that need no code. Every step that has `as` puts its value into the answer's `result` under that name. A step with `optional: true` that fails is skipped (its value is `null`); any other failure ends the run.

`selector` is a [Playwright selector](https://playwright.dev/docs/other-locators): CSS (`#id`, `.class`), `text=Sign in`, `role=button[name="Sign in"]`, `xpath=...`, chained with `>>`. The first match is used, or the `nth` (from 0).

| Action | Fields | Value |
| --- | --- | --- |
| `goto` | `url`, `wait_until` (`load`, `domcontentloaded`, `networkidle`, `commit`) | the address |
| `reload`, `back` | `wait_until` | the address |
| `click`, `dblclick`, `hover` | `selector`, `button`, `count` | |
| `fill` | `selector`, `value` or `value_arg` | |
| `type` | `selector`, `value` or `value_arg`, `delay` (ms between keys) | |
| `press` | `key` (`Enter`, `Control+A`), `selector` (optional) | |
| `check` | `selector`, `checked` (default true) | |
| `select` | `selector`, `value` or `value_arg` (an option's value or a list) | |
| `wait_for` | `selector` with `state` (`visible`, `hidden`, `attached`, `detached`); or `url` (a glob, or `/regex/`); or `load_state`; or `ms` (at most 60000) | |
| `text` | `selector`, `all` | the element's text (`all`: a list, of every match) |
| `html` | `selector` (optional) | the element's inner HTML, or the page's |
| `value` | `selector` | an input's value |
| `attribute` | `selector`, `name` | the attribute |
| `count` | `selector` | how many elements match |
| `visible` | `selector` | true or false |
| `url`, `title` | | the page's address, title |
| `evaluate` | `expression`, `arg` | the expression's value in the page; a function is called with `arg` |
| `screenshot` | `selector` (optional), `full_page` | PNG, base64 |

Every step also takes `timeout` (ms), instead of `action_timeout_seconds`.

`value_arg` takes the value from the request's `args`, so a password stays out of the steps (and out of error messages, which may name a step's fields).

## Timeouts

| Field | Default | |
| --- | --- | --- |
| `timeout_seconds` | 60 | the whole run, at most 600 |
| `action_timeout_seconds` | 15 | each Playwright action (click, fill, wait) |

## The answer

| Field | |
| --- | --- |
| `ok` | whether the script or all steps succeeded |
| `result` | the script's return value, or the steps' values by `as` |
| `steps` | the steps done, with their time (`skipped` for optional ones that failed) |
| `error` | why the run failed (the first line of Playwright's message) |
| `timed_out` | the run hit `timeout_seconds` |
| `screenshot` | the tab when the run failed (base64 PNG; `screenshot_on_error: false` turns it off) |
| `url` | the tab's address at the end |
| `page` | the tab's page id, with `keep_open` |
| `logs` | the page's console messages and the script's `log()` lines (at most 200) |
| `ms` | how long the run took |

The status is 200 when the run succeeded, 422 when the script or a step failed, 504 when it timed out, 400 for a request that is not valid, 404 for an unknown profile or session, 501 when the node does not serve runs.

## Security

A run executes code on the node, so:

- Only the **cluster secret** may start one (not a viewer token, which a page in a session can obtain), also through a peer; and a node without a cluster secret refuses runs.
- Each run is a Node.js process of its own under Node's [permission model](https://nodejs.org/api/permissions.html): it may read only playwright-core and its own directory and write only its own temporary directory, and it cannot start processes, worker threads or native add-ons. It reaches the browser through the daemon's own CDP endpoint with a token that the script does not see. Its environment holds nothing else.
- The run's directory is removed when it ends; the process is killed at the deadline. At most four runs go at once; more wait.
- The daemon logs that a run happened (profile, number of steps, outcome, time), never the script, the steps or `args`. The runner's error output is discarded, as it may quote them.

What this does not protect: the script drives the browser, so it can do whatever the profile is signed in to, and read what its pages show. It can reach the network as the node does. Give the cluster secret only to clients you would let use the browser itself.

## Clients

Any HTTP client works. With Home Assistant's `aiohttp` session:

```python
async with session.post(
    f"{url}/api/run",
    headers={"X-Browser-Cluster-Secret": secret},
    json={"profile": "work", "url": "https://example.org", "steps": [
        {"action": "fill", "selector": "#user", "value_arg": "user"},
        {"action": "fill", "selector": "#password", "value_arg": "password"},
        {"action": "click", "selector": "role=button[name='Sign in']"},
        {"action": "wait_for", "selector": ".dashboard"},
        {"action": "text", "selector": ".balance", "as": "balance"},
    ], "args": {"user": user, "password": password}},
    timeout=aiohttp.ClientTimeout(total=90),
) as res:
    answer = await res.json()
```

## Plans

- A small Python client (aiohttp only) for Home Assistant integrations: profiles, sessions, a profile's login (`/api/pages/{id}/auth`) and runs, so integrations stop carrying their own copies.
- Vault helpers for scripts and steps (`fill_secret` with an rbw entry, as `browser_fill_secret` does for agents), so a client need not hold the password at all.
- Firefox sessions.
