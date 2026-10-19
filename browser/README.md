# Browser Daemon Node

Temporarily register your own running browser with the daemon, share selected
tabs, and optionally let trusted daemon clients control them. Chromium uses
`chrome.debugger`. Firefox supports DOM control without a helper, or trusted
pointer/keyboard input and content evaluation through the optional Linux native
companion.

The build and release layout follows the rbw fork: shared Manifest V3 source,
engine-specific manifests, a Python build without npm dependencies, the version
from the daemon's `Cargo.toml` (one version for both), and AMO/CRX signing in CI.

## Allowed daemon URLs

The extension refuses every daemon until its exact origin appears on an
explicit allowlist. HTTPS is required except for explicitly listed loopback
development origins. Wildcards, credentials, paths, queries and fragments are
not accepted. The allowlist is checked before pairing, dashboard integration,
registration, automatic reconnect and tab commands. Removing an active daemon
from policy ends its connection and sharing.

Administrators set `allowedDaemonOrigins` in `storage.managed`: Chromium uses
the bundled managed-storage schema; Firefox uses a managed-storage manifest.
Managed policy is read-only in the popup. Once managed policy has been seen,
missing or broken policy blocks connections rather than falling back to local
settings. Unmanaged installations can explicitly edit **Allowed daemon URLs**
in the extension's own setup page. No origins are trusted by default.

## Pair from the dashboard

Pick the daemon in the extension and click **Pair from the dashboard** once to
grant access to that site. Then click **Pair browser** under **Nodes → Add a
node** on the signed-in HTTPS dashboard. An extension tab opens for your local
approval; name the browser and click **Connect**, then switch on the tabs to
share. No terminal command or code copying is needed.

Only the configured daemon gets a small content-script pairing bridge. It
accepts discovery and pairing requests from its top-level dashboard pages,
never tab-control commands. Each request still requires approval in the
extension's own UI. Codes stay in memory, expire in five minutes, and are bound
to this installation's extension origin. Dashboard pairing requires a valid
sign-in cookie, the daemon's HTTPS Origin, and a same-origin browser fetch;
bearer viewer tokens alone cannot pair.

URL/name drafts save immediately. Code and tab-selection drafts and the last
12 connection events live only in `storage.session`, across popup closes but
not browser restarts; code drafts expire after five minutes. Active grants are
not restored. **Open setup in a tab** gives a stable page for permission prompts.

## Remembered browsers and key expiry

Select **Remember this browser and reconnect after restart (no key expiry)** on
the dashboard and approve that choice in the extension. The daemon stores a
hash of a random browser-only key in `browser-keys.json` (mode 0600). The key
itself stays in the extension's local storage; it is bound to the extension
origin and browser type and can only register that browser, never act as a peer
or control another node. Its stable node entry remains visible while offline.

The extension reconnects on startup or within a minute after a network failure.
Tab grants are not persisted or automatically restored: choose tabs again after
a restart/reconnection. **Stop sharing** and the stop shortcut pause automatic
reconnect; **Reconnect remembered browser** resumes it. **Forget browser** while
connected or **Forget browser and revoke key** on the dashboard revokes the key.
The dashboard can revoke offline remembered browsers too. Pairing codes still
expire in five minutes. Manual pairing uses `extension pair --remember`; a
memory-only connection with no time limit uses `--lifetime-secs 0` instead.

## Install and pair manually

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
4. Enter the daemon URL and a browser name, then connect. Switch on the tabs to
   share; changes apply at once. **Remote control** is off by default.
5. The browser appears alongside regular nodes in **Nodes**. Its shared tabs
   form one session in **Sessions**, with the same preview and tab strip as a
   managed browser. **Take over** opens the regular `/handoff/` view, which
   speaks CDP to the tab over the daemon's relay socket
   (`/api/extension/nodes/<id>/tabs/<tab>/cdp`): a pushed screencast and input
   that does not wait for answers, as for managed browsers. Chromium answers from
   the tab's debugger; Firefox emulates the subset the view uses (frames captured
   on change, input through the companion or DOM actions). View-only grants get
   frames and page state only. Shared browsers keep their owner's window size,
   user agent and pages: the view neither resizes nor injects anything. Node
   actions disconnect the browser or revoke its remembered key. Old
   `/extension/` links redirect to these views.

No tabs are shared merely by connecting. Private tabs, browser/extension pages,
any allowed daemon's pages, and localhost pages are never shared. Sharing
belongs to the tab and follows it across navigations and sites. While a shared
tab shows a page that must never be shared, it is paused: unlisted, its live
views closed, nothing read from it or sent to it, until it shows a shareable
page again. **Share new tabs** (on by default) shares tabs you open while
connected, with the current **Remote control** choice. The toolbar badge counts
shared tabs: red while any may be controlled, green when all are view only.

**Stop sharing**, **Ctrl+Shift+Period**, expiry, socket loss, browser restart,
remote disconnect, and Chromium's **Cancel debugging** bar all revoke access.
Temporary browsers require another pairing code or dashboard approval to reconnect.
Remembered browsers use their revocable key. Tab grants and pairing codes are
never restored from persistent storage.

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
MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 firefox --remote-debugging-port 9222
```

Pair the extension, then click **Enable installed companion** before sharing
tabs. Firefox asks for the optional native messaging permission. The popup and
remote node list report whether native control is connected.

The companion speaks WebDriver BiDi to Firefox's Remote Agent (standard
library Python, its own minimal WebSocket client). This startup setting enables
a powerful local Firefox automation interface: any local process can reach the
port, and system access lets it run privileged code. Keep it on loopback; do
not forward the port or add permissive `--remote-allow-hosts`/`-origins` flags.
For several Firefox processes, give each its own `--remote-debugging-port` and
set the matching `BROWSER_DAEMON_FIREFOX_PORT` (default 9222) for it. The
companion checks the session's Firefox PID against its own process ancestry and
refuses to act on another instance.

One fixed privileged script, run in a chrome context, resolves extension tab
IDs to Firefox's own BiDi browsing contexts. No matching by URL or active tab
is used. Commands then address that context directly (no window switching):
screenshots as JPEG, trusted input through `input.performActions`. Page code
(snapshots, selectors, evaluation) runs in a named BiDi sandbox: a realm with
the page's own principal, apart from the page's globals and without chrome
privileges. Script and arguments are data until evaluated there; the origin is
checked before the script body runs, and results come back as serialized
values. The caller cannot choose a context, sandbox, chrome scope or BiDi
method. Origin, expiry, and the independent companion grant table are checked
too. Closing the native pipe ends the BiDi session; it does not quit Firefox.

Without the companion, Firefox uses DOM clicks and form editing. Those events
are not trusted keyboard/pointer input, so some web apps will ignore them.
Live views capture frames with Firefox's own `tabs.captureTab` (JPEG at CSS
pixels), which needs the `<all_urls>` permission; the companion's screenshots
are the fallback without it.

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
pairing**. Pair directly with the hosting daemon using the dashboard or its cluster credential.

## Security boundary

- Pairing needs the CLI secret or a valid dashboard cookie with a same-origin
  HTTPS browser fetch. Bare viewer bearer tokens are refused. The extension receives
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
- Browser-wide CDP domains, target attachment, cookie/storage APIs, emulation,
  file-system upload commands, and persistent injected scripts are blocked, in
  the daemon and again in the extension. Browser chrome,
  private windows, unshared tabs, desktop control, native file pickers and
  browser permission dialogs are outside the grant. Native automation is
  currently Linux-only.
- Limits: 32 extension sockets (including pending handshakes), 128 shared tabs,
  bounded command queues, a five-second authentication deadline, heartbeat
  liveness checks, and an eight-MiB response limit. Registrations and grants are
  memory-only; remembered identities and key digests persist. Sensitive responses have `Cache-Control: no-store`.
- The extension has no externally-connectable interface,
  remote-loaded code, analytics, or background tab discovery sent to a server.
  Server URL, display name and preferences are saved persistently; a browser key
  is stored only when you explicitly choose to remember the browser. Screenshots and commands
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
`MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 firefox --remote-debugging-port 9222` using
the same profile. Launching against an already-running ordinary Firefox reuses
that process and does not enable the Remote Agent. Installation does not connect or share any tabs:
pair the extension, enable the companion, then explicitly select tabs and
whether to allow control.
