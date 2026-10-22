# MCP server for agents

`browser-daemon mcp` is an MCP server (stdio) that gives an agent a browser on any node of the mesh with a single entry in its MCP configuration. The browser tools are [Playwright MCP](https://github.com/microsoft/playwright-mcp)'s, run as a child on a CDP endpoint of a node; Playwright MCP cannot change that endpoint while it runs, so the server swaps the child when the agent asks for something else. These tools are added:

- `browser_status`: the browser in use, the profiles the agent may open, the nodes and their load.
- `browser_use`: switch to a saved profile (`profile`, which also fixes the node) or to a throwaway browser on a chosen node (`node`). The previous browser is closed.
- `browser_fill_secret` (only with `--allow-secret`): type a field of an [rbw](https://github.com/doy/rbw) vault entry (`password`, `username`, `totp` or a custom field) into a page field without the value reaching the agent. See below.

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
