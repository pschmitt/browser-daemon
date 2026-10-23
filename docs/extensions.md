# Live-view extensions

The live view's vault (the toolbar button, its popover and the passkey prompt) is an extension: a bundle of pages, scripts, styles and icons that the view runs in sandboxed frames and talks to by message. browser-daemon has none of its own: it loads them from `--extension-dir DIR` (repeatable; NixOS: `services.browser-daemon.liveViewExtensions`, which defaults to rbw's). So an extension lives with the project it belongs to and changes without a browser-daemon release; rbw's is in [rbw's repository](https://github.com/pschmitt/rbw/tree/main/browser/browser-daemon), next to the browser extension whose look it shares. Without one, the live view has no vault button and passkeys go the browser's own way.

The extension only does the UI. Everything that reaches the vault (the secret agent's requests, the checks on each field, filling the page, signing a passkey) stays in browser-daemon and works as in [rbw vault](rbw.md); the extension asks the view, and the view decides which page and site that is for.

## Bundle

A directory with a `manifest.json`:

```json
{
  "api": 1,
  "name": "rbw",
  "title": "rbw",
  "version": "2.28.0",
  "icon": "icons/rbw-48.png",
  "permissions": ["vault"],
  "toolbar": { "entry": "popup.html", "label": "Fill a login from the vault (rbw)" },
  "passkey": { "entry": "passkey.html" }
}
```

| Field | |
| --- | --- |
| `api` | the contract's version; this browser-daemon speaks `1` and refuses to start with another |
| `name` | lowercase letters, digits and `-`, unique; its files are served at `/handoff/ext/<name>/` |
| `title`, `version` | shown to people; `version` is optional |
| `icon` | the toolbar button's image |
| `permissions` | what it may ask for: `vault` (logins, fills, passkey lookups) |
| `toolbar` | a button left of the session menu; `entry` opens in a popover under it, `label` is its tooltip |
| `passkey` | `entry` opens in a dialog when a page started from the view wants a passkey; needs `vault`. The first extension with one gets the passkeys |

Paths are relative to the bundle and stay inside it. The node serves the files as they are on disk, so changes to a directory show on the next load of the live view (a changed manifest needs a restart). `GET /handoff/extensions` lists what is loaded.

## Sandbox

Every page of an extension runs in an `<iframe sandbox="allow-scripts">` and is served with `Content-Security-Policy: sandbox allow-scripts; default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'none'; frame-ancestors 'self'`. So it is an opaque origin: it has no cookie and no storage of the node's, cannot reach the live view's document, its sign-in or the remote page, cannot fetch or connect anywhere, and loads only its own files. Its only way out is `postMessage` to the view, which answers only what its manifest and slot allow.

The vault keeps its promise in here too: an extension sees entry names, users and folders, never a password, code or key.

## Messages

Both ways, messages are objects `{"bd": "ext", "type": ..., ...}`; the frame posts to `parent`, the view checks they come from that frame. rbw's `browser/browser-daemon/bridge.js` wraps them (`bd.request(kind, args, {onNote, signal})`, `bd.on(type, fn)`, ...) and can be copied.

From the frame:

| `type` | Fields | |
| --- | --- | --- |
| `ready` | | loaded: the view answers with `init` |
| `request` | `id`, `kind`, ... | see below; answered with `reply` |
| `cancel` | `id` | the answer is not wanted any more |
| `size` | `height` | the content's height in pixels; the view sizes the frame to it (within its limits) |
| `badge` | `text` | the toolbar button's badge (up to 3 characters), `null` hides it |
| `close` | `focus` | close the popover; focus goes back to the `"button"` or to the `"view"` (after a fill) |
| `toast` | `text`, `ms` | a note at the bottom of the view; `ms: 0` keeps it until the next |
| `passkey.answer` | `id`, `use`, `entry` | the prompt's answer: use the vault (with the entry id picked, or `null`) or not |

To the frame:

| `type` | Fields | |
| --- | --- | --- |
| `init` | `api`, `slot`, `theme` | `slot` is `toolbar` or `passkey`; `theme` is `light`, `dark` or `null` (follow the system) |
| `context` | `site`, `ready` | toolbar: after each page load and when opened. `site` is the page's host name (`""` off the web), `ready` whether the vault can fill it |
| `open` | `warning` | the popover is shown |
| `closed` | | it is hidden again |
| `passkey` | `id`, `site`, `warning` | a passkey prompt for `site` |
| `passkey.end` | `id` | the view closed that prompt (Escape) |
| `reply` | `id`, `result` or `error` (`aborted`) | a request's answer |
| `note` | `id`, `note` | progress of a request ("confirm the unlock on your phone") |

`warning` is `null` or `{"text", "watch"}`: the session is shared publicly, and `text` says what that means for a fill or a passkey (`watch`: visitors only look). Show it.

Requests (`vault` permission):

| `kind` | From | Fields | Result |
| --- | --- | --- | --- |
| `logins` | toolbar | `query`, `unlock` | the site's logins (then, with a `query`, the rest of the vault's matches, `"site": false`), or `{"locked": true}`; `unlock: true` asks the phone first |
| `fill` | toolbar | `entry`, `fields` (`username`, `password`, `totp`), `any_site` | `{"filled": [...]}`; an entry saved for another site needs `any_site` (ask twice first) |
| `passkey.find` | passkey, while its prompt is open | | the entries with a passkey for the prompt's site, or `{"locked": true}` |

Entries are `{"id", "name", "user", "folder", "totp", "site"}` (`passkey_user` for passkeys). After `passkey.answer` with `use: true` the view gets the passkey from the secret agent and signs the page in; the extension is done.

## rbw's

rbw's repository holds it in `browser/browser-daemon`: the popover and the prompt, using the browser extension's own `theme.css`, `icons.js` and logo. `python3 browser/build.py` there writes the bundle to `browser/dist/browser-daemon`, and rbw's flake exports it as `browser-daemon-extension` (here: `packages.rbw-extension`, from the locked `rbw` input). To work on it, point a node at the checkout and rebuild there; reloading the live view shows the changes:

```
browser-daemon run --extension-dir ~/src/rbw/browser/dist/browser-daemon ...
```

After `nix flake update rbw` here, nodes deployed from this flake get rbw's latest one.
