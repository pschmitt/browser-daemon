# rbw vault

browser-daemon fills logins and passkeys from an [rbw](https://github.com/doy/rbw) vault without the vault ever being on a node. Three parts talk to rbw:

| Part | Runs | What it does with rbw |
| --- | --- | --- |
| Secret agent (`browser-daemon secret-agent`) | as the vault's user, anywhere that reaches the nodes | serves the live views: passkeys, the vault button's logins and fills |
| MCP server (`browser-daemon mcp`) | as the agent's user, next to the AI agent | `browser_fill_secret` types a vault field into a page (see [MCP](mcp.md#secrets-browser_fill_secret)) |
| `rbw` flake input | at build time only | rbw's live-view extension, the vault's UI (see [Styling](#styling)) |

browser-daemon only runs the `rbw` command (`rbw get`, `rbw code`, `rbw list`, `rbw search`, `rbw passkey ...`); rbw talks to its own `rbw-agent` as usual. Nothing reads rbw's database or the agent's socket directly.

## Secret agent

```
live view ──HTTP──▶ node ◀──long poll── secret agent ──▶ rbw ──▶ rbw-agent
                     ▲                        │
                     └── answer / autofill ◀──┘
```

The agent connects out to each node it serves (`--node URL`, repeatable) and long-polls `GET /api/secret-agent/next` with the cluster secret (`--cluster-secret-file`); nodes never connect to it, so it can run on a machine behind NAT. The live view never calls rbw itself: it queues a request on its node (`POST /api/sessions/<id>/passkey`, `.../autofill`, or the shared-browser equivalents under `/api/extension/nodes/...`) and polls `GET /api/passkey/<request>` until the agent has answered it (`POST /api/secret-agent/<request>`). Meanwhile the agent may post a note shown in the view ("confirm the unlock on your phone", `POST /api/secret-agent/<request>/note`). A request the agent does not take within its time limit fails with "The secret agent did not answer"; a node that has not seen an agent poll for 90 seconds refuses new requests with "No secret agent is running for this node", and the live view then leaves its vault button alone for five minutes.

Requests and what the agent runs for them:

| Request | From | rbw |
| --- | --- | --- |
| `logins` | opening the vault button, typing a search | `rbw search --url https://<site>/ --json [-- <search>]` |
| `fill` | picking a login | `rbw search --url` (is it saved for the site?), `rbw get --raw <id>`, `rbw code <id>` for the one-time code |
| `copy` | a login's copy button or menu | `rbw get --raw <id>`, `rbw code <id>` for a one-time code |
| `find` | the passkey dialog: which entry would sign in | `rbw passkey find --rp-id <site>` (one batched decrypt, no sync: rbw 2.30 and later) |
| `assert` | approving a passkey (any session or shared browser) | `rbw passkey assert --rp-id <site> --origin <origin> --challenge <challenge>` |
| `inject` | (API only; the live view no longer asks for it) | `rbw passkey inject --rp-id <site> --cdp-url <node> --page-id <page>` |

### Finding logins

rbw finds them: `rbw search --url <page>` (rbw 2.30 and later; the node passes the page's address without query and fragment, so scheme, port and path count) lists the logins saved for the page by rbw's URI matching, the way its browser extension offers them (its `defaultUriMatch`, the match types, an HTTPS entry never on an HTTP page), or with a search the logins whose name, user, folder or URIs contain each word, the site's first. It decrypts that metadata in one batch in rbw-agent and answers in milliseconds; it never matches or prints notes, custom fields or secrets. With several rbw accounts it covers the unlocked ones; the agent remembers each entry's account for the `rbw get` and `rbw code` of a fill.

### Locked vault

Lookups (`logins`, `find`) never unlock: on a locked vault the view says so and offers to unlock. A request that needs the vault (a fill, an approved passkey, the view's **Unlock from your phone**) checks `rbw unlocked` and, when locked, posts `{"host": "<--unlock-host>", "agent": "Login for <site>"}` (or `"Passkey for <site>"`) to `--unlock-webhook` and waits up to a few minutes for the vault to open. What the webhook does is up to you; for example a Home Assistant automation that asks your phone and unlocks rbw on that host. Without a webhook the request fails with "The vault is locked".

### Logins

The vault button lists the entries for the page's site, as rbw matches them (see [Finding logins](#finding-logins)), and searches the whole vault. Picking one fills it ([live view](live-view.md#vault-logins)). The site always comes from the browser's own page list, not from the page or the view. The agent fills only entries saved for that site; an entry saved for another site takes a second click in the view, and the agent then posts `{"message": "..."}` (entry, site, fields; never a value) to `--notify-webhook`. The values reach the page only through the node's `POST /api/pages/<id>/autofill`, which needs the cluster secret and a `fill` request the agent took for that page, once. The node fills them from an isolated world (Chromium) or sandbox (Firefox), only into frames on the entry's site; they are never logged and never reach the view.

Copying (a login's copy button: the password, else the one-time code, else the username; or the menu's **Copy username/password/TOTP**) is the one way a value reaches the view, and only the browser you view on: the agent answers the `copy` request, the node hands that answer out once and forgets it, and the live view writes it to this device's clipboard. The extension's frame never sees it, and nothing logs it (the agent logs which field was copied).

### Passkeys

A page's passkey sign-in (`navigator.credentials.get()`) is held while the live view asks: the dialog shows the site and which vault entry would sign in (`find`; with several, you pick one). Approving it has rbw sign the page's challenge, which the page's hook answers with, in every browser; the browser's own authenticator is never touched, so this works whether or not rbw keeps passkey counters (`passkeyCounter`). **Use another authenticator** lets the page ask the browser's own way instead. Only a sign-in that follows a click or tap in the view within 15 seconds is held, so an AI agent driving the page through its own connection is not held up.

### Safety

- The agent takes each request id once and checks every field before it reaches rbw: page and entry ids, host names, origins, challenges and field names must be plain values, so nothing a node sends becomes an rbw option or a path.
- The secret-agent routes and `/api/pages/<id>/autofill` need the cluster secret; a viewer's token is not enough (a page in a session can obtain one). See [access control](access-control.md).
- rbw's errors name the problem (no passkey, several entries), never a secret.

### Running it

```
browser-daemon secret-agent --node https://browser.example.org --cluster-secret-file /run/secrets/browser-daemon \
  [--rbw rbw] [--unlock-webhook URL] [--unlock-host HOST] [--notify-webhook URL]
```

It must run as the vault's user, with rbw configured and able to reach that user's rbw-agent (in `$XDG_RUNTIME_DIR`). On NixOS use the module's `services.browser-daemon.secretAgent` ([NixOS](nixos.md#secret-agent)). One agent can serve several nodes; without an agent a node has no vault (the vault button and passkey dialog say so). `RUST_LOG` sets its log level (default `browser_daemon=info`).

## Styling

The live view's vault popover and passkey prompt are rbw's [live-view extension](extensions.md), from rbw's repository (`browser/browser-daemon`), which shares the rbw browser extension's theme, icons and logo. Nodes deployed from this flake load it from the `rbw` flake input (`packages.rbw-extension`); after `nix flake update rbw` they get rbw's current look. Elsewhere pass `--extension-dir` (see [Live-view extensions](extensions.md#rbws)).
