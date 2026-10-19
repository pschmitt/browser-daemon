# Browser Daemon Node

Temporarily register your own running browser with the daemon, share selected
tabs, and optionally let trusted daemon clients control them. Chromium uses
`chrome.debugger`. Firefox supports DOM control without a helper, or trusted
pointer/keyboard input and content evaluation through the optional Linux native
companion.

The build and release layout follows the rbw fork: shared Manifest V3 source,
engine-specific manifests, a Python build without npm dependencies, the version
from `Cargo.toml`, and AMO/CRX signing in CI.

## Install and pair

1. Build with `python3 browser/build.py` (or download the CI artifacts).
   Chromium: load `browser/dist/chromium` unpacked at `chrome://extensions`.
   Firefox development: load `browser/dist/firefox/manifest.json` temporarily
   at `about:debugging#/runtime/this-firefox`. Normal Firefox installation
   requires the AMO-signed XPI from the release workflow.
2. Run a daemon built from this source. It must have a cluster secret for pairing.
   The extension needs an HTTPS daemon URL; loopback HTTP is allowed for local
   development. The endpoint must be reachable from the browser. Existing
   reverse-proxy login pages must not intercept `/api/extension/connect`: this
   one endpoint authenticates its first WebSocket message instead. Keep the
   rest of the API behind the usual authentication.
3. Open the extension. Copy its **exact extension origin** into the command
   shown there, using your daemon URL and secret file:

   ```text
   browser-daemon --url https://YOUR-DAEMON --secret-file /PATH/TO/SECRET extension pair --origin ORIGIN-FROM-POPUP
   ```

   Copy the returned `code` directly into the extension. Treat it as a secret;
   do not paste it into an agent conversation. It is single use and expires
   after five minutes. `--lifetime-secs` sets the registration lifetime
   (default 3600, minimum 60, maximum 28800 seconds).
4. Enter the daemon URL and a browser name, then connect. Select tabs and click
   **Apply tab sharing**. **Allow remote control** is off by default.
5. On the daemon dashboard, open **Nodes → Temporary browsers**, or visit
   `/extension/`. This page discovers extensions on this daemon and online
   peers, offers live screenshots, page text, input, navigation, and revocation.

No tabs are shared merely by connecting. Private tabs, browser/extension pages,
the connected daemon's own pages, and localhost pages cannot be selected. Tab
navigation, including reload and SPA URL changes, ends that tab's grant; select
it again to continue. New tabs are not automatically shared.

**Stop sharing**, **Ctrl+Shift+Period**, expiry, socket loss, browser restart,
remote disconnect, and Chromium's **Cancel debugging** bar all revoke access.
Reconnecting requires another pairing code. Grants and codes are never restored
from persistent storage.

## Firefox trusted input

Install the companion for the current Linux user:

```text
python3 browser/install.py
```

This copies the standard-library Python host to
`~/.local/lib/browser-daemon-node/companion.py` and registers its native messaging
manifest in `~/.mozilla/native-messaging-hosts/`. It does not edit a Firefox
profile. Python 3 must be on the browser's inherited PATH. Re-run the installer
when updating the extension/companion together.

Quit Firefox normally and restart it with the automation interface explicitly
enabled:

```text
MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 firefox --marionette
```

Pair the extension, then click **Enable installed companion** before sharing
tabs. Firefox asks for the optional native messaging permission. The popup and
remote node list report whether native control is connected.

This startup setting enables a powerful local Firefox automation interface.
Keep Marionette on loopback; do not forward its port or add permissive remote
host/origin flags. Its default port is 2828. For multiple Firefox processes,
give each profile a distinct `marionette.port` preference and launch it with the
matching `BROWSER_DAEMON_FIREFOX_PORT` environment variable. The companion checks
the reported Firefox PID against its own process ancestry and refuses to act on
another instance.

One fixed privileged script resolves extension tab IDs to Firefox's own
WebDriver handles. No matching by URL or active tab is used. Actual page
evaluation uses a second fixed wrapper that creates a fresh sandbox with the
page's own principal and no privileged automation globals. Script and arguments
are data until evaluated in that sandbox; results are cloned out before the
sandbox is destroyed. The caller cannot choose a principal, chrome context or
system sandbox. Document identity,
origin, expiry, and the independent companion grant table are checked too.
Closing the native pipe detaches the automation session; it does not quit Firefox.

Without the companion, Firefox uses DOM clicks and form editing. Those events
are not trusted keyboard/pointer input, so some web apps will ignore them.
Screenshots in this mode require Firefox's `<all_urls>` permission. With the
companion, screenshot capture does not need that broad host grant.

Chromium requires `debugger` permission at installation because Chrome does not
allow it as an optional permission. That installation permission does not share
any tab; the extension attaches a debugger only when an allowed shared-tab action
needs it. Site access permissions are requested from the popup when needed.

## CLI and agents

```text
browser-daemon extension list
browser-daemon extension command BROWSER_UUID TAB_ID snapshot
browser-daemon extension command BROWSER_UUID TAB_ID click '{"selector":"button.submit"}'
browser-daemon extension command BROWSER_UUID TAB_ID type '{"text":"hello"}'
browser-daemon extension revoke BROWSER_UUID
```

`browser-daemon mcp` exposes `browser_shared_nodes` and `browser_shared_command`.
The first discovers temporary nodes across the mesh, with their hosting daemon
and shared tab IDs. The second takes `browser`, `tab`, `action`, optional
`params`, and an optional hosting `daemon`. Screenshot results are MCP images.
These tools work alongside the existing managed-browser Playwright tools;
`browser_use` keeps its existing meaning. Restart the MCP process to discover
the new tools.

Actions: `screenshot`, `snapshot`, `click`, `type`, `key`, `scroll`, `navigate`,
`reload`, `back`, `forward`, `focus`, `close`. With a control grant, Chromium also
supports `cdp {method, params}` for explicitly allowed Runtime, DOM, Input,
Accessibility and page-inspection methods. Firefox's companion adds
`evaluate {script, args?}`; `script` is a JavaScript function body, e.g.
`return document.title;`.

API: `POST /api/extension/pair`, `GET /api/extension/nodes`,
`DELETE /api/extension/nodes/<browser>`, and
`POST /api/extension/nodes/<browser>/command {tab, action, params}`.
The existing peer proxy forwards listing, commands and revocation, but **never
pairing**. Pair directly with the hosting daemon using its cluster credential.

## Security boundary

- Pairing needs the cluster secret, not a viewer cookie. The extension receives
  only an origin-bound single-use code, sent in its first WebSocket message,
  never a URL, cookie, or cluster-wide credential. The daemon stores a digest of
  unused codes. Host validation and existing origin protections remain intact;
  only the exact extension connection endpoint admits an extension origin.
- Temporary browsers are untrusted registrations, not cluster peers. They cannot
  spawn sessions, receive peer requests, or obtain other nodes' credentials.
- The daemon and extension both enforce shared-tab membership and separate
  control grants. The native host has its own grant table. The popup verifies
  that the selected tab still has the URL it displayed before approving it.
- Viewing allows fixed screenshot/text operations, never caller-provided script
  or input. Control includes the shared page's signed-in authority and its data.
  It is appropriate only for daemon clients you trust with those accounts.
  Arbitrary content evaluation can act with that site's authority, including
  requests and same-origin windows reachable by the page.
- Browser-wide CDP domains, target attachment, cookie/storage APIs, file-system
  upload commands, and persistent injected scripts are blocked. Browser chrome,
  private windows, unshared/new tabs, desktop control, native file pickers and
  browser permission dialogs are outside the grant. Native automation is
  currently Linux-only.
- Limits: 32 extension sockets (including pending handshakes), 128 shared tabs,
  bounded command queues, a five-second authentication deadline, heartbeat
  liveness checks, and an eight-MiB response limit. Registrations and grants are
  memory-only. Sensitive responses have `Cache-Control: no-store`.
- The extension has no content-script bridge, externally-connectable interface,
  remote-loaded code, analytics, or background tab discovery sent to a server.
  Only server URL and display name are saved locally. Screenshots and commands
  are not recorded by this feature. Agent tool arguments/results may be logged:
  do not type credentials through these tools.

## CI and releases

CI builds both ZIPs, lints the Firefox manifest/source, checks JavaScript and
Python permission/framing boundaries, runs Rust format/Clippy/tests and Nix
checks, then exercises real isolated Chromium and Firefox browsers on a scratch
runner. The Firefox round-trip fixture pregrants native messaging and injects a
test-only popup driver; production background and companion code are unchanged.

On a matching `v<Cargo.toml version>` tag, signing waits for all those checks on
that source. Configure repository secrets `BROWSER_NODE_AMO_API_KEY`,
`BROWSER_NODE_AMO_API_SECRET`, and `BROWSER_NODE_CHROMIUM_SIGNING_KEY` before using
it. The Chromium key must remain stable. Signed packages in `browser/packages` are checked against the source in CI.
The workflow reuses matching packages; otherwise it signs Firefox unlisted
through AMO and creates a CRX3 with Chrome. It compares payloads to the source,
produces checksums and provenance, preserves the artifacts, and uploads them to
the tag's release (creating a draft if absent). No signing credentials are
needed for development or normal CI. AMO signs a version only once: bump the
shared version for extension releases and reuse completed signing artifacts
when retrying a later failed step.

Development checks: `just browser-build`, `just browser-check`. Run
`just browser-smoke` with the debug daemon binary, Node 22+, Chromium, and
Firefox on PATH, on a scratch host. Smoke checks use no real browser profiles,
accounts, or infrastructure credentials.

Browser API references: [Chrome optional permission constraints](https://developer.chrome.com/docs/extensions/reference/api/permissions),
[Firefox screenshot permissions](https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/tabs/captureTab),
[Mozilla remote protocol security](https://firefox-source-docs.mozilla.org/remote/Security.html).

## Nix installation

The flake exports `firefox-addon`, `chromium-addon` (with `extensionId`), and
`browser-native-host`. Install the signed Firefox XPI through
`programs.firefox.policies.ExtensionSettings` with a `file://` install URL; add
the native host to Home Manager's `programs.firefox.nativeMessagingHosts`.
Chromium supports the CRX through `programs.chromium.extensions` with its ID,
`crxPath` and package version, as in the rbw fork. Signing keys stay in the
vault and GitHub repository secrets, outside Nix and the source tree.

For trusted Firefox control, fully exit Firefox, then launch it with
`MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 firefox --marionette` using the same profile.
Launching against an already-running ordinary Firefox reuses that process and
does not enable Marionette. Installation does not connect or share any tabs:
pair the extension, enable the companion, then explicitly select tabs and
whether to allow control.
