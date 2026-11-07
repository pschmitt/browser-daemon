# Profiles, proxies and notes

Optional persistent profiles via `?profile=<name or id>` query parameter, alongside ephemeral auto-cleaned sessions. Each profile lives in `/var/lib/browser-daemon/profiles/<uuid>/` with a `profile.json` holding its free-text name (e.g. `Google (p@example.com)`) and engine; the API reports the directory as `storage_dir`. Name-based directories from older versions are migrated on startup.

## What a profile keeps

cookies (including the ones without an expiry that many logins use), localStorage and IndexedDB, like a desktop browser. A session of a profile is quit properly when it stops, so the browser writes its data first (a signal would lose cookies set in the last seconds), and the profile's saved tabs are not restored: only the pages in its default tabs open. This holds for sessions made by agents through Playwright too: Playwright works in a context of its own, which the daemon maps onto the profile's. Throwaway sessions keep nothing, until **Save as profile** (session menu in the dashboard and handoff, `browser-daemon session save-as-profile`, `POST /api/sessions/<id>/save-as-profile {"name": "..."}`): the browser quits, its data is copied into a new profile, and a session of that profile opens the same pages.

## Moving profiles between nodes

Discovers peer nodes across Tailscale/wireguard meshes and transfers persistent browser profiles between nodes with live progress toasts.

### Automatic placement and node groups

Advertise labels with repeated `--node-group` arguments or NixOS
`services.browser-daemon.nodeGroups = [ "cloud" ];`. Groups appear in mesh
inventory and MCP `browser_status`. Nodes without groups remain usable when
no group is requested.

Groups can also be added at runtime in a node's settings on the dashboard's
Nodes tab (or `PATCH /api/settings` with `{"groups": [...]}`) and are kept across
restarts. Groups from the startup flags are immutable there: change them in the
flags or Nix option.

MCP `browser_use` accepts `profile`, `node` and `group`. Requesting a saved
profile on another node moves it there before opening it. A group restricts
placement to its members; an explicit node must also belong to that group.
An active profile stays on its node. Otherwise a group request selects the
least-utilized online node with capacity and the required browser engine. For
requests without a group, a matching resident profile is preferred.

The authenticated `POST /api/mesh/placement` endpoint accepts:

```json
{"profile": "work", "group": "cloud", "engine": "chromium"}
```

It returns `node`, `node_url` and `profile`. Call the selected node's session
or run API afterwards. Placement does not reserve capacity or start a browser.
`create: true` allows a missing profile for a subsequent session or run to
create; it does not create or reserve that profile itself. Clients creating a
new profile concurrently must coordinate its name and destination.

An active profile can stay on a matching node, but cannot be moved. Transfers
never overwrite a destination profile. Duplicate names, incomplete profile
inventory and a group without available capacity cause an error rather than
fallback. A node that is down stops only what it may hold. Each
node remembers the profiles its peers had when they last answered: a profile
that a down node had is an error until it is back, as creating it would lose its
logins to a new, empty copy. Any other profile is found or created as usual. A
failed request can leave complete copies on both nodes; resolve
those copies before retrying. If opening a session fails after a successful
move, the profile stays on its destination.

The CLI and dashboard's explicit transfer controls remain available; group
placement is exposed through the API, MCP and Python client.

## Ephemeral nodes

Configure both `--ephemeral-up` and `--ephemeral-down` (NixOS:
`services.browser-daemon.ephemeralUp` and `ephemeralDown`) with executable
paths. The daemon passes a unique lease ID as the only argument to either
executable. The up command must provision a node running a compatible daemon,
wait until it accepts requests, and print a JSON object containing its base
URL, for example `{"url":"https://worker.example.org"}`. The node must
accept the coordinator's cluster secret and be reachable by the client.
The down command must destroy the resource associated with the lease ID and
be idempotent, including when provisioning failed before creating anything.
The provider must retain the lease-to-resource mapping across restarts and
coordinate concurrent up/down calls. Use a provider-side maximum lifetime as
a backstop for failures outside the daemon's control.

Call authenticated `POST /api/ephemeral/sessions` with the usual session
creation options, `browser-daemon session new --ephemeral-node`, or MCP
`browser_use` with `{"ephemeral":true}`. In the dashboard, choose **New session**
and enable **Dedicated short-lived node**. The live view keeps this worker alive;
closing the last live view or automation client starts the disconnect grace period.
The dashboard reaches the worker through its provider, without exposing the
cluster secret to the browser. The
response contains `node_url` and `session`; connect to the node's `/chromium`
or `/firefox` with `session_id` set to that session's ID. The session is always
non-persistent. Saved profiles and node/group selectors are not supported for
ephemeral nodes. A session that has never had a client expires after two
minutes. After the last automation client disconnects, a five-second window
allows reconnection; then the session closes and the coordinator runs the down
command. Failed teardown is retried and leases are recovered after coordinator
restart. A node that is unreachable cannot be confirmed closed, so the
coordinator keeps retrying; the provider's maximum lifetime is the backstop.
Active and starting workers appear in the dashboard's **Nodes** view and in
`browser-daemon node list`. The dashboard can open their live view or stop their
session, which also decommissions the worker.
Requests sent to another mesh node are forwarded to an online provider; nodes
advertise the capability as `ephemeral_provider` in mesh inventory.

The commands run as the daemon's service user under its systemd restrictions.
An rworker adapter can use a dedicated browser-worker image; see the rworker
profile configuration for its startup and controller access. Start a disposable
node with `--ephemeral-node`: it allows one session over its lifetime and forces
that session to be non-persistent, even if a caller requests persistence.

## Upstream proxies

Route a session through a SOCKS or HTTP proxy configured on its node (`services.browser-daemon.proxies`). Pick one per session (`"proxy": "<id>"` when creating it, `?proxy=<id>` on `/chromium` and `/firefox`, `none` for direct) or store a default on a profile (`PATCH /api/profiles/<name> {"proxy": "<id>"}`). A running session switches on the fly (`POST /api/sessions/<id>/proxy {"proxy": "<id>"}`, the session menu in the dashboard and handoff, `p` in the TUI): browsers always talk to a per-session SOCKS5 relay in the daemon, which forwards directly or through the chosen proxy and drops open connections when the route changes. DNS goes through SOCKS5 proxies, and WebRTC is kept from bypassing them. `GET /api/proxies` lists a node's proxies. A proxy URL may carry a login (`socks5://user:password@host:1080`, percent-encoded; HTTP proxies get Basic; keep it in a file with `url_file` instead of `url`); the API never shows it. A node can be such a proxy for the others, and they pick it up by themselves as `node:<name>`: see [SOCKS5 egress](nixos.md#socks5-egress).

## Notes

sessions, profiles and recordings take notes (short texts with their time, in Markdown: emphasis, code, links, lists and task boxes, quotes; rendered without HTML, links only to http(s) and mailto): the dashboard's menus (**Notes…**) and the note badges on cards and rows, the live view's session menu, `browser-daemon session|profile|recording note TARGET [TEXT|-]` (without TEXT: list), `GET|POST /api/notes/<sessions|profiles|recordings>/<id>`, `PUT|DELETE /api/notes/<kind>/<id>/<note>`. A profile's notes live in its directory (`bd-notes.json`: they move, back up and export with it, and clearing its data keeps them), a recording's in its directory, a session's in the state directory until the session ends. The overview carries each item's count and latest note, and the search fields find notes.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../dashboard-profiles-dark.png">
  <img src="../dashboard-profiles.png" alt="Profiles with free-text names and per-profile proxies">
</picture>
