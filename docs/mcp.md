# MCP server for agents

`browser-daemon mcp` is an MCP server (stdio) that gives an agent a browser on any node of the mesh with a single entry in its MCP configuration. The browser tools are [Playwright MCP](https://github.com/microsoft/playwright-mcp)'s, run as a child on a CDP endpoint of a node; Playwright MCP cannot change that endpoint while it runs, so the server swaps the child when the agent asks for something else. These tools are added:

- `browser_status`: the browser in use, the profiles the agent may open, the nodes and their load.
- `browser_use`: switch to a saved profile (`profile`, which also fixes the node) or to a throwaway browser on a chosen node (`node`). The previous browser is closed.
- `browser_fill_secret` (only with `--allow-secret`): type a field of an [rbw](https://github.com/doy/rbw) vault entry (`password`, `username`, `totp` or a custom field) into a page field without the value reaching the agent. See below.
- `browser_passkey` (only with `--allow-passkey`): sign in with a passkey from the vault. See [Passkeys](#passkeys-browser_passkey).
- `browser_ask_human` (only with `--ask-human-cmd`): ask the user to do something only a person may do, in the live view of the agent's own page. See [Asking the user](#asking-the-user-browser_ask_human).

Without a choice the agent gets a throwaway browser on the node with the most room. Profiles keep whatever they are signed in to, so agents may only open the ones named with `--allow-profile` (repeatable; `*` allows every profile); anything else is refused. `--playwright-cmd` names the program that runs Playwright MCP, called as `PROGRAM ENDPOINT OUTPUT_DIR`; `--url`, `--secret-file` as for the other commands.

```json
{ "command": "browser-daemon", "args": ["mcp", "--url", "https://browser.fnuc.example", "--allow-profile", "work"] }
```

## Secrets: `browser_fill_secret`

Typing a password with Playwright MCP's own tools puts it in the tool call, and Playwright MCP echoes every script it runs into its answer and logs. `browser_fill_secret` keeps the value out of both:

1. The entry must be named with `--allow-secret` (repeatable; exact rbw entry names, or `*` for every entry of the vault).
2. The value is read here with rbw (`--rbw`, default `rbw`; `rbw get --raw`, `rbw code` for `totp`). An empty field is an error; it never falls back to another field (`rbw get` prints the notes of an entry without a password).
3. The page must be on a site the entry is saved for, by the entry's URIs and their match type as Bitwarden's autofill decides (base domain by the Public Suffix List, so `a.github.io` is not `b.github.io`; host, starts-with and exact are honoured; an https entry is not used on http, and a port counts; regex URIs never match). The address comes from Playwright's page, not from page script.
4. The field is focused with a Playwright click on the agent's `target`, then the node types the value over CDP (`POST /api/pages/{page_id}/insert-text`, with the entry's URIs and the session the page is in: a page id that is not in that session, or that several sessions show, is refused; refused unless the page, and the frame with focus, are on a site the entry is saved for, checked on the same connection right before typing; the text is never logged).
5. Every value filled in the session is masked as `•••` in the answers and error messages of all later tool calls, also as JSON-escaped, percent-encoded, HTML-escaped and base64 (standard and URL-safe alphabet, at all three byte alignments, so a copy inside a longer encoded string is caught too), and the `user:password` pair of the entry's username (HTTP Basic auth) the same way (values under four characters only where they are the whole text). Files Playwright MCP writes to its output directory (snapshots) are not rewritten; password fields never show their value there.
6. The answer of `browser_fill_secret` only says what was typed where (`Typed the password of 'entry' into <element> on <host>`), never the value. Before typing, the field is marked `data-bd-secret` and given a style (from an isolated world, as a constructed stylesheet so a page's CSP does not block it) that shows its value as dots whatever its `type` becomes (`-webkit-text-security: disc`), so a page's "show password" toggle does not put the value into a screenshot. The vault button's autofill does the same for every field it fills (username, password, TOTP), in Firefox with transparent text instead, as Firefox has no text-security. What this does not protect: a page that replaces the element (a re-render drops the mark), a field in a frame nested deeper than the focused frame, text the page copies elsewhere (a "your password is ..." line), canvas or custom-drawn inputs, and the page's own script, which can read the value or remove the mark. Playwright's snapshot reads the field's value whatever the screen shows, so the MCP server masks the filled values in what it returns and, as Playwright MCP keeps snapshots and logs in files its answers only link, in the text files under `--output-dir` written during each call.

The MCP server runs rbw itself, as its own user; it does not use the [secret agent](rbw.md) of the live views. This guards against accidents, not against an agent bent on reading the value back in another form (e.g. encoded by a script): only allow entries for agents you trust with them.

Default tabs are not opened for Playwright connections (it ignores tabs it did not open); a profile's login is what an agent gets from it.

## Passkeys: `browser_passkey`

An agent cannot use the live view's passkey dialog, which waits for a person. `browser_passkey` arms the agent's browser instead (`POST /api/sessions/{id}/agent-passkey` on its node): for a few minutes (`minutes`, default 5, at most 30; `0` disarms) every `navigator.credentials.get()` of every page and frame of the session, on one of the sites named with `--allow-passkey` (repeatable; a site covers its subdomains, `*` allows any), is signed by the node's [secret agent](rbw.md#secret-agent) with `rbw passkey assert` and answered in the page. The agent then clicks the site's passkey sign-in as usual.

- Nothing is armed on one page, so sign-ins that move across origins or ask the moment a page loads (Microsoft's `login.microsoft.com/common/bridge/fido`) work.
- The origin and the site come from the browser's target list, never from page script; a page may only ask for its own site or a parent of it, over HTTPS. rbw refuses an origin outside the passkey's relying party as well.
- A locked vault is unlocked the way the secret agent always does it (`--unlock-webhook`, e.g. a phone prompt). A request it cannot answer (not armed for the site, no passkey, refused) goes to the browser's own `get()`. Autofill (conditional) requests are always left to the browser.
- Chromium sessions only; the node needs a running secret agent.
- Every assertion advances the passkey's counter in the vault. A Bitwarden client that also signs in to the site with that passkey, with an older counter, may then be rejected as a clone: allow only sites the agent's passkeys are meant for.

## Asking the user: `browser_ask_human`

Some steps need a person: a CAPTCHA or other bot check, an approval on another device, a password the agent may not fill. `browser_ask_human` runs `--ask-human-cmd PROGRAM` as `PROGRAM MESSAGE`, with the live view of the agent's current page in its environment, so the user can click and type in the very session the agent drives:

| Variable | Value |
| --- | --- |
| `BD_HANDOFF_URL` | the live view of the page on its node (`<node>/handoff/?page=<id>`) |
| `BD_PAGE` | the page's id |
| `BD_NODE`, `BD_NODE_URL` | the node's name and address |
| `BD_TITLE` | the title the agent asked for (may be empty) |
| `BD_AGENT` | the agent's MCP client (`clientInfo.name`, e.g. `claude-code`) |

What the program does with it is up to you (a phone notification, a chat message); its standard output is passed back to the agent, a non-zero exit is an error. It must finish within a minute. The agent is told to leave the page alone until the user is done.
