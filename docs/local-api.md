# Local tab API

Status: phases 1 (read), 2 (activate, close, open) and the signals of phase 3
are implemented;
see [Rollout](#rollout) for the rest.

A local API on the machine where the node extension runs, so scripts can ask
about and control the tabs of the user's own Firefox and Chromium cheaply,
with a token, and without polling the browser. It replaces
[bruvtab](https://github.com/pschmitt/bruvtab) once it covers bruvtab's
features.

## Setup

1. Register the native messaging host for Firefox and Chromium-based
   browsers: `browser-daemon local install`. With Nix, add the flake's
   `browser-native-host` package to the browsers' native messaging hosts
   instead (Home Manager: `programs.firefox.nativeMessagingHosts`,
   `programs.chromium.nativeMessagingHosts`).
2. Run the server: `browser-daemon local serve`, or with Home Manager import
   the flake's `homeManagerModules.default` and set
   `services.browser-daemon-local.enable = true` (socket activated). Without
   a running server the native host starts one itself.
3. Make a token: `browser-daemon local token create --name me --scope tabs:read`,
   and save it as `~/.config/browser-daemon/local-token` (or pass it in
   `BROWSER_DAEMON_LOCAL_TOKEN`). With Home Manager, `--no-save` prints the
   entry for `services.browser-daemon-local.tokens.<name>`.
4. Turn on **Local tab API** in the extension popup (Advanced), or set the
   managed policy `localApi: true`. Chromium asks for the native messaging
   permission when you switch it on.

## The `tabs` command

`browser-daemon tabs` lists every tab you can reach, from three sources:

| Source | Flag | What |
| --- | --- | --- |
| local | `--local` | all tabs of this machine's browsers (local tab API; no network) |
| shared | `--shared` | tabs of browsers paired with the daemon |
| sessions | `--sessions` | tabs of the daemon's browser sessions, mesh-wide |

Without a flag it reads all three and skips one that does not answer, with a
note on stderr; a source asked for by flag that fails is an error. Scripts
use `--local`: about a millisecond, and no network.

Tab ids are `<client>:<tab>`. A browser's client name is `<host>-<browser>`
(`gk4-firefox`; the name the extension pairs with by default), so a tab of
your own browser that is also shared has the same id from both sources and is
listed once (`shared` in `--json`). Session tabs are `<session name>:<first 8
characters of the target id>`.

```text
$ browser-daemon tabs --audible
gk4-firefox:42	Some video - YouTube	https://www.youtube.com/watch?v=...
$ browser-daemon tabs --local --site meet.jit.si
$ browser-daemon tabs --browser chromium
$ browser-daemon tabs get gk4-firefox:42
```

Browsers are nodes, next to the mesh's daemons: `browser-daemon nodes` lists
both (kind `browser`, with `local`, `shared` or both), and `browser-daemon
nodes --local` only this machine's browsers, without a daemon.

Act on tabs with `tabs activate ID` (alias `focus`; also brings the window
to the front), `tabs close ID...` and `tabs open URL` (`--browser NAME`,
`--background`; prints the new id). A tab is acted on where it lives: local
ones through this API (token scope `tabs:control`), shared ones through the
daemon, session tabs through their node.

Read what a tab shows with `tabs text ID`, `tabs html ID`, `tabs screenshot
ID` and `tabs pdf ID`. Text and HTML go to stdout; a screenshot (PNG) or PDF
is written to `<id>.png`/`<id>.pdf` here (`-f FILE` elsewhere, `-f -` to
stdout) and the path printed. Local tabs need the scopes `tabs:content`
(text, HTML) and `tabs:capture` (screenshot, PDF). Chromium prints a real
PDF; Firefox gives extensions no PDF bytes (`tabs.saveAsPDF` only opens a
Save dialog), so there the PDF is the whole page captured as one image, and
its text cannot be selected. Session tabs need a Chromium session.

Tabs of private windows are listed (`-o wide` shows PRIVATE, `--json` has
`private`) and can be used like any other when the browser lets the
extension into private windows (Firefox: "Run in Private Windows";
Chromium: "Allow in Incognito"). Signals never count them.

Filters: `--audible`, `--active`, `--pinned`, `--site` (URL contains),
`--title`, `--browser` (a client name, a kind such as `firefox`, or a name
prefix), and a free-text query after `list`. `--json` prints every field.
Tab ids and `--browser` complete in bash, zsh and fish (`browser-daemon
completions <shell>` registers dynamic completion).

## Why

- Asking the browser through a CLI that starts an interpreter costs about
  250 ms of CPU per call (`bruvtab list`: ~140 ms of imports, ~30 ms to build
  the argument parser, ~20 ms to load a CA bundle for a plain-HTTP localhost
  call). Sensors that poll every 10 to 15 s add up to several percent of a
  core on a laptop.
- bruvtab's mediator listens on `127.0.0.1:4625` without authentication, and
  its actions are plain `GET`s: any local user, and any web page through a
  blind cross-origin request, can close or activate tabs.
- The node extension already sees every tab change as it happens
  (`tabs.onCreated`, `onUpdated`, `onRemoved`, `onActivated`), so the state
  can be pushed instead of polled.

## Goals

- One API for Firefox and Chromium, all profiles, merged in one view.
- Tokens on every request, scoped, revocable, also on the Unix socket.
- Answers from memory: a read does not reach into the browser.
- Push: a stream of changes locally, and derived signals (media playing,
  meeting open, user rules) to the daemon and Home Assistant.
- Feature parity with bruvtab, then retire it.
- One Rust binary: the native host, the local server and the CLI are
  subcommands of `browser-daemon`.

## Non-goals

- Exposing the full tab list beyond the machine. Only derived signals leave
  it (see [Signals](#signals-and-home-assistant)), and only when enabled.
- Private and incognito windows: only with the browser's own permission for
  the extension (see above), and never in signals.

## Architecture

```text
Firefox ──native messaging──▶ browser-daemon native-host ─┐
Chromium ─native messaging──▶ browser-daemon native-host ─┤ feeder socket
                                                          ▼
                       browser-daemon local serve  ($XDG_RUNTIME_DIR/browser-daemon/)
                         ├─ local.sock  (API, tokens)  ◀── browser-daemon tabs …, scripts
                         └─ state: tabs per browser/profile, signals, search index
```

- **Native host** (`browser-daemon native-host`). The browser starts one per
  browser profile when the extension calls `connectNative`. It is a thin
  relay: it forwards the extension's snapshots to the server and the server's
  commands to the extension. Registered for both browsers:
  - Firefox: `~/.mozilla/native-messaging-hosts/<name>.json`,
    `allowed_extensions: ["browser-daemon-node@pschmitt.dev"]`.
  - Chromium and Chrome: `~/.config/{chromium,google-chrome}/NativeMessagingHosts/<name>.json`,
    `allowed_origins: ["chrome-extension://<id>/"]`.

  An open native port also keeps the Chromium MV3 service worker alive.
- **Server** (`browser-daemon local serve`). A systemd user service, socket
  activated, so the path is stable, tokens load in one place, and a query
  with no browser running gets an empty answer instead of an error. Without
  systemd, the native host starts it on demand. It keeps the state in memory,
  answers reads from it, and forwards control commands to the right
  host and extension.
- **CLI** (`browser-daemon tabs …`). Talks to `local.sock`; a Rust start is a
  few milliseconds. `--json` everywhere.
- **Extension**. On every tab change it sends a debounced snapshot (about
  100 ms) of that window: id, window, index, title, URL, active, audible,
  muted, pinned, discarded, private, favicon URL. Full resync on connect and
  on request. It executes control commands with the tabs and scripting APIs,
  and on Chromium with the debugger it already attaches for sharing.

### Native host name and the Firefox companion

The Firefox companion (`browser/companion.py`, `dev.pschmitt.browser_daemon`)
does trusted input over WebDriver BiDi. A native host name maps to exactly one
program. Start with a new name (`dev.pschmitt.browser_daemon_local`) so the two
can ship independently. Later, fold the BiDi relay into the Rust host
(Firefox only) and drop the Python companion.

## Authentication

Tokens are required on every request, on the Unix socket too.

- The API socket is `0600` in a `0700` directory, so only the user can
  connect at all. Web pages cannot reach a Unix socket.
- Each client has its own token. Format `bdl_<id>_<secret>` (32 random bytes).
  The server stores `id`, a SHA-256 of the secret, the name and the scopes;
  it compares in constant time.
- Scopes:

  | Scope | Allows |
  | --- | --- |
  | `tabs:read` | list, query, active tab, windows, clients |
  | `tabs:control` | activate, open, close, move, navigate, update, new |
  | `media:control` | play, pause, mute, unmute |
  | `tabs:content` | page text, HTML, words, index, search |
  | `tabs:capture` | screenshot, PDF |
  | `tabs:debug` | console and network streams, closing past confirmations |
  | `events:read` | the change stream |

- Management: `browser-daemon local token create --name NAME --scope …`
  prints the token once; `list` and `revoke` by name or id. Declaratively
  (NixOS/Home Manager), tokens are listed with their scopes and the hash
  comes from a file, for example a sops secret, so nothing secret lands in
  the Nix store.
- Clients read their token from `BROWSER_DAEMON_LOCAL_TOKEN` or a file
  (`--token-file`, default `$XDG_CONFIG_HOME/browser-daemon/local-token`).
- Every request is logged with the token name and the peer's pid
  (`SO_PEERCRED`).
- Native hosts do not use tokens: they connect to a separate feeder socket,
  and the server accepts them only when `SO_PEERCRED` shows the same uid.
- If a TCP listener is ever added, it is localhost only and also checks the
  `Host` header against DNS rebinding.

## API

HTTP/1.1 over the Unix socket, JSON, `Authorization: Bearer <token>`.

| Method and path | Scope | |
| --- | --- | --- |
| `GET /v1/tabs` | `tabs:read` | filters: `audible`, `active`, `pinned`, `browser`, `window`, `url` (substring), `title`, `q` (either) |
| `GET /v1/tabs/{id}` | `tabs:read` | ids are `<client>:<tab>`, e.g. `gk4-firefox:42` |
| `GET /v1/windows`, `GET /v1/clients` | `tabs:read` | windows with tab counts; connected browsers and profiles |
| `POST /v1/tabs/{id}/activate` | `tabs:control` | also focuses the window |
| `POST /v1/tabs/open` | `tabs:control` | URLs, target window, background |
| `POST /v1/tabs/close` | `tabs:control` | ids; `force` needs `tabs:debug` (Chromium) |
| `POST /v1/tabs/move`, `/update`, `/navigate` | `tabs:control` | bruvtab's `move`, `update`, `navigate` |
| `POST /v1/tabs/{id}/media` | `media:control` | `play`, `pause`, `mute`, `unmute` |
| `GET /v1/tabs/{id}/content?kind=text\|html` | `tabs:content` | `{kind, text}` |
| `GET /v1/tabs/{id}/content?kind=screenshot\|pdf` | `tabs:capture` | `{kind, format, data}` (base64); Firefox PDFs are a page image (`raster: true`) |
| `POST /v1/index`, `GET /v1/search` | `tabs:content` | SQLite FTS5 index kept by the server |
| `GET /v1/tabs/{id}/console`, `/network` | `tabs:debug` | streams, Chromium |
| `GET /v1/events` | `events:read` | server-sent events: tab created, updated, removed, activated; signal changes |

## Signals and Home Assistant

Signals are small facts the extension derives from all its tabs. They are
off by default; switch **Signals** on in the popup (Settings) or set the
`signals` policy. Then they, and nothing else of the tabs, go to the paired
daemon over the extension's existing connection, and only to daemons that
announce support in their `registered` message (older ones close the
connection on an unknown message). Private windows never count.

- Built in:
  - `media`: a tab plays sound and is not muted (attributes `title`, `site`;
    the active tab first). This also covers sound without an MPRIS player
    (WebAudio, calls).
  - `meeting`: a meeting tab is open (attributes `provider`: `zoom`, `jitsi`,
    `teams` or `meet`, and `url`).
- Own rules, from the `signalRules` policy: `{"name": "...", "url": "regex",
  "title": "regex", "match": "regex"}` (case-insensitive; `match` tests the
  title or the URL; a tab must pass every pattern the rule has).
  The signal is on while a tab matches, with the attribute `count`. Only the
  name and the count leave the browser, so a rule can live in a private
  configuration.

The daemon keeps them per browser (`signals` in `GET /api/extension/nodes`)
and pushes every change on `GET /api/extension/signals/stream` (server-sent
events: `event: signals` with `{"browser", "name", "online", "signals"}`,
first one per connected browser, then one per change; `event: resync` when a
subscriber fell behind). The Home Assistant integration subscribes to that
stream and keeps one binary sensor per signal on each browser's device, so a
change shows up at once instead of with the next poll.

This replaces the go-hass-agent sensors that poll bruvtab today: `pp`,
`online-meeting`, `ms-teams in-a-meeting` and the browser fallback of the
media player sensor.

## bruvtab parity

| bruvtab | Here | Notes |
| --- | --- | --- |
| `list`, `query`, `active`, `windows`, `clients` | `tabs [list]`, `tabs active/get`, `nodes [--local]`, `GET /v1/…` | all sources; local ones from memory |
| `activate`, `close`, `open`, `new`, `navigate`, `update`, `move` | `tabs …`, `POST /v1/tabs/…` | |
| `play`, `pause`, `mute`, `unmute` | `tabs media …` | mute with `tabs.update`; play/pause with a script in the page |
| `text`, `html`, `words` | `tabs text/html/words` | `scripting.executeScript` |
| `index`, `search` | `tabs index/search` | SQLite FTS5 (new dependency: `rusqlite`, bundled) |
| `screenshot`, `pdf` | `tabs screenshot/pdf` | Chromium full page via debugger; Firefox via BiDi |
| close past confirmations | `tabs close --force` | Chromium debugger |
| console and network streams | `tabs console/network` | Chromium debugger |
| `dup` | `tabs dup` | done in the CLI from the list |
| `install` | `browser-daemon local install` | writes the native host manifests for both browsers; NixOS/Home Manager module does the same declaratively |
| `--json`, completions | clap | |
| rofi/fzf scripts (`shell/`) | ported to the new CLI | |

Consumers to move in the user's configuration: go-hass-agent `lib.sh`,
`pp.sh`, `online-meeting.sh`, `playerctl.sh`; `ms-teams in-a-meeting`;
`browser-run-or-raise.sh`; the zsh `browser::`/`bruvtab::` functions;
`cmp-bruvtab` in Neovim (uses words); the rofi and fzf scripts.

## Rollout

1. Server, native host for both browsers, tokens, read API, `tabs
   list/active/windows/clients`, Nix module. Move the read-only consumers.
2. Control and media commands. Move `browser-run-or-raise`, the zsh functions,
   and the rofi/fzf scripts.
3. Events, signals, Home Assistant entities. Drop the polling go-hass-agent
   sensors.
4. Content, search, capture, debug streams. Move `cmp-bruvtab`.
5. Remove the bruvtab extension and mediator from the hosts; archive bruvtab.

## Open questions

- Chromium: `nativeMessaging` is an optional permission, asked for when the
  user switches the API on, so updating the extension does not disable it.
  A policy cannot grant it; the popup switch is needed once.
- The extension version: every extension change needs a new signed release
  (AMO signs each version once), so batch the extension work per phase.
- Search index: keep it in memory only, or persist it under
  `$XDG_STATE_HOME/browser-daemon/`?
