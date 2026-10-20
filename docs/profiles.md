# Profiles, proxies and notes

Optional persistent profiles via `?profile=<name or id>` query parameter, alongside ephemeral auto-cleaned sessions. Each profile lives in `/var/lib/browser-daemon/profiles/<uuid>/` with a `profile.json` holding its free-text name (e.g. `Google (p@example.com)`) and engine; the API reports the directory as `storage_dir`. Name-based directories from older versions are migrated on startup.

## What a profile keeps

cookies (including the ones without an expiry that many logins use), localStorage and IndexedDB, like a desktop browser. A session of a profile is quit properly when it stops, so the browser writes its data first (a signal would lose cookies set in the last seconds), and the profile's saved tabs are not restored: only the pages in its default tabs open. This holds for sessions made by agents through Playwright too: Playwright works in a context of its own, which the daemon maps onto the profile's. Throwaway sessions keep nothing, until **Save as profile** (session menu in the dashboard and handoff, `browser-daemon session save-as-profile`, `POST /api/sessions/<id>/save-as-profile {"name": "..."}`): the browser quits, its data is copied into a new profile, and a session of that profile opens the same pages.

## Moving profiles between nodes

Discovers peer nodes across Tailscale/wireguard meshes and transfers persistent browser profiles between nodes with live progress toasts.

## Upstream proxies

Route a session through a SOCKS or HTTP proxy configured on its node (`services.browser-daemon.proxies`). Pick one per session (`"proxy": "<id>"` when creating it, `?proxy=<id>` on `/chromium` and `/firefox`, `none` for direct) or store a default on a profile (`PATCH /api/profiles/<name> {"proxy": "<id>"}`). A running session switches on the fly (`POST /api/sessions/<id>/proxy {"proxy": "<id>"}`, the session menu in the dashboard and handoff, `p` in the TUI): browsers always talk to a per-session SOCKS5 relay in the daemon, which forwards directly or through the chosen proxy and drops open connections when the route changes. DNS goes through SOCKS5 proxies, and WebRTC is kept from bypassing them. `GET /api/proxies` lists a node's proxies.

## Notes

sessions, profiles and recordings take notes (short texts with their time, in Markdown: emphasis, code, links, lists and task boxes, quotes; rendered without HTML, links only to http(s) and mailto): the dashboard's menus (**Notes…**) and the note badges on cards and rows, the live view's session menu, `browser-daemon session|profile|recording note TARGET [TEXT|-]` (without TEXT: list), `GET|POST /api/notes/<sessions|profiles|recordings>/<id>`, `PUT|DELETE /api/notes/<kind>/<id>/<note>`. A profile's notes live in its directory (`bd-notes.json`: they move, back up and export with it, and clearing its data keeps them), a recording's in its directory, a session's in the state directory until the session ends. The overview carries each item's count and latest note, and the search fields find notes.

![Profiles with free-text names and per-profile proxies](../dashboard-profiles.png)
