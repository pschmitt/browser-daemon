# Automation: CDP, BiDi and Playwright

Every incoming connection gets an isolated browser of its own; connections never
interfere with one another.

## Endpoints

- `/chromium` (WebSocket for Playwright, Puppeteer, CDP clients)
- `/firefox` (WebSocket for Firefox WebDriver BiDi clients)
- `/json/version`
- `/json/list` (aggregates active pages across all active sessions)
- `/devtools/page/:page_id` (WebSocket for individual page inspection)
- `/health`

## Engines

Spawns an isolated browser instance per incoming connection (`/chromium` for CDP, `/firefox` for WebDriver BiDi). Connections never interfere with one another.

Runs Chromium with native CDP and Firefox with WebDriver BiDi (`/firefox`). Firefox only allows one WebDriver session per process, so the daemon owns it and multiplexes it: any number of BiDi clients (automation, handoff, recorder) can attach to the same Firefox session (`/firefox?session_id=<id>`).

## Bot detection

Runs headful Chromium inside an X11 virtual display (`Xvfb`) with `--disable-blink-features=AutomationControlled` natively. Cleanly passes Google Botguard, Cloudflare Turnstile, and other bot detection systems.

## Human-like input

`/chromium?human_input=1` makes the node reshape that client's input so it looks
less like a bot (the MCP server has a flag for it; `--human-input-default` or the
NixOS option `humanInput` turn it on for all clients, `human_input=0` opts out):

- A click, or a pointer jump of more than a few pixels, is preceded by a curved
  path of `mouseMoved` events (15 to 40 steps over 150 to 600 ms) from the last
  pointer position of that CDP session.
- `Input.insertText` becomes one insertion per character, 30 to 140 ms apart,
  with an occasional longer pause (very long text goes in chunks, at most 100).
- `Input.dispatchKeyEvent` gets a small random gap.
- A big mouse wheel delta is split into steps of about 100 with short gaps.

The node's own messages use ids that clients never use, and their answers are
not forwarded. The order of the client's messages is kept: later ones wait for a
delayed one. Typing and clicking are slower this way. Chromium only: Firefox
(`/firefox`, BiDi) is not reshaped.

## Without Playwright on the client

[`POST /api/run`](run.md) runs a Playwright script or a list of steps on the
node and answers with the result.

## Tabs over HTTP

`PUT /json/new` opens a tab in the session named by `session_id`, or in the one
showing `page`; with neither it starts a session of its own, and an unknown
session or page is a 404. `/json/new`, `/json/close/{id}` and
`/json/activate/{id}` take PUT only.

## Shared desktop browsers

A desktop browser with the [browser extension](../browser/README.md) is a node of
the mesh, with the tabs its owner shares.

Playwright (and other CDP clients) can drive a shared Chromium browser's tabs
shared with control, through a browser-level endpoint on the daemon:

```js
const browser = await chromium.connectOverCDP(
  'https://browser.example.org/api/extension/nodes/<node-id>',
  { headers: { 'X-Browser-Cluster-Secret': secret } });
const page = browser.contexts()[0].pages()[0];
```

Shared tabs are the pages of the browser's one context; `newPage()` opens a tab
(shared with control) where the owner lets viewers open tabs. Profile-wide and
disk access stay closed (other sites' cookies, downloads, file inputs from the
owner's disk), and what the client set (interception, emulation, injected
scripts) is undone when it disconnects. Firefox browsers serve live views only.
