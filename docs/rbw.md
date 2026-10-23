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
| `logins` | opening the vault button, typing a search | the vault index (below), else `rbw search` + `rbw get --raw` |
| `fill` | picking a login | `rbw get --raw <id>`, `rbw code <id>` for the one-time code |
| `find` | the passkey dialog: which entry would sign in | `rbw passkey find --rp-id <site>` |
| `inject` | approving a passkey (Chromium session) | `rbw passkey inject --rp-id <site> --cdp-url <node> --page-id <page>` |
| `assert` | approving a passkey (Firefox, shared browsers) | `rbw passkey assert --rp-id <site> --origin <origin> --challenge <challenge>` |

### Vault index

`rbw search` decrypts every entry's names and URIs, and rbw-agent serves one `rbw get` at a time, so a lookup through rbw takes seconds. While rbw is unlocked the agent keeps an index built from `rbw list --raw` (names, users, folders, URIs and whether a password or TOTP is there, never a secret) and rebuilds it when rbw's local database (`~/.cache/rbw/*.json`) changes. Until it is there, lookups go through rbw.

### Locked vault

Lookups (`logins`, `find`) never unlock: on a locked vault the view says so and offers to unlock. A request that needs the vault (a fill, an approved passkey, the view's **Unlock from your phone**) checks `rbw unlocked` and, when locked, posts `{"host": "<--unlock-host>", "agent": "Login for <site>"}` (or `"Passkey for <site>"`) to `--unlock-webhook` and waits up to a few minutes for the vault to open. What the webhook does is up to you; for example a Home Assistant automation that asks your phone and unlocks rbw on that host. Without a webhook the request fails with "The vault is locked".

### Logins

The vault button lists the entries for the page's site, matched by their URIs and match types as Bitwarden's autofill does, and searches the whole vault. Picking one fills it ([live view](live-view.md#vault-logins)). The site always comes from the browser's own page list, not from the page or the view. The agent fills only entries saved for that site; an entry saved for another site takes a second click in the view, and the agent then posts `{"message": "..."}` (entry, site, fields; never a value) to `--notify-webhook`. The values reach the page only through the node's `POST /api/pages/<id>/autofill`, which needs the cluster secret and a `fill` request the agent took for that page, once. The node fills them from an isolated world (Chromium) or sandbox (Firefox), only into frames on the entry's site; they are never logged and never reach the view.

### Passkeys

A page's passkey sign-in (`navigator.credentials.get()`) is held while the live view asks: the dialog shows the site and which vault entry would sign in (`find`; with several, you pick one). Approving it has rbw add the passkey to a virtual authenticator in the page over the node's CDP endpoint, removed again once the page has signed in (Chromium sessions), or has rbw sign the page's challenge, which the page's hook answers with (Firefox and shared browsers; the browser's own authenticator is never touched). Only a sign-in that follows a click or tap in the view within 15 seconds is held, so an AI agent driving the page through its own connection is not held up.

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
