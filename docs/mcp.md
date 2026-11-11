# MCP server for agents

`browser-daemon mcp` is an MCP server (stdio) that gives an agent a browser on any node of the mesh with a single entry in its MCP configuration. The browser tools are [Playwright MCP](https://github.com/microsoft/playwright-mcp)'s, run as a child on a CDP endpoint of a node; Playwright MCP cannot change that endpoint while it runs, so the server swaps the child when the agent asks for something else. These tools are added:

- `browser_status`: the browser in use, the profiles the agent may open (with their labels), the nodes (with their labels) and their load.
- `browser_use`: switch to a saved profile (`profile`) or a throwaway browser. Optional `node` and `label` select its destination (`group` is the old name of `label`); an idle profile moves there automatically. A label is a strict restriction, not a preference: only nodes carrying it qualify. Active profiles that need moving are refused. Set `ephemeral: true` to provision an ephemeral node of its own for a throwaway browser. The previous browser is closed after placement succeeds. See [profile placement](profiles.md#automatic-placement-and-node-groups) and [ephemeral nodes](profiles.md#ephemeral-nodes).
- `browser_fill_secret` (only with `--allow-secret`): type a field of an [rbw](https://github.com/doy/rbw) vault entry (`password`, `username`, `totp` or a custom field) into a page field without the value reaching the agent. See below.
- `browser_fill_card` (only with `--allow-card`): fill an RBW Card field into an authorized payment iframe without submitting the form.
- `browser_passkey` (only with `--allow-passkey`): sign in with a passkey from the vault. See [Passkeys](#passkeys-browser_passkey).
- `browser_ask_human` (only with `--ask-human-cmd`): ask the user to do something only a person may do, in the live view of the agent's own page. See [Asking the user](#asking-the-user-browser_ask_human).
- `browser_export_auth` and `browser_fetch` (only with `--allow-export`): hand the login of a page (cookies, storage values) to the agent's other tools, or use it for one HTTP request, without the agent seeing it. See [Logins](#logins-browser_export_auth-and-browser_fetch).
- `browser_network_har`: record the traffic of the browser and save it as a HAR file. See [Network recording](#network-recording-browser_network_har).
- `browser_flight_recorder`: keep a rolling video of the last seconds of the browser and save it as a recording. See [Flight recorder](#flight-recorder-browser_flight_recorder).
- `browser_screenshot_censored`: a screenshot with passwords, card fields, one-time codes, filled inputs and filled secrets blacked out. See [Censored screenshots](#censored-screenshots-browser_screenshot_censored).
- `browser_set_topic`: label this conversation's browser sessions on the dashboard. See [Session label](#session-label-browser_set_topic).
- `browser_live_link`: the address of the live view of the agent's current page, the same one `browser_ask_human` sends. Read-only.
- `browser_recording`: record the agent's own session (`action`: `start`, `stop`, `status`); the answer gives the recording id. See [Recording and sharing](#recording-and-sharing).
- `browser_share` and `browser_unshare` (only with `--allow-share`): make a public link to the agent's session or to a recording, and end it. See [Recording and sharing](#recording-and-sharing).

Other options: `--censor-screenshots` (the child's `browser_take_screenshot` is answered by the censored screenshot, so an agent cannot take an uncensored one), `--challenge-wait SECS` (default 15) sets how long to wait for a [bot wall](#bot-walls) to clear, `--human-input` asks the node for [human-like input](#human-like-input).

Without a choice the agent gets a throwaway browser on the node with the most room. Profiles keep whatever they are signed in to, so agents may only open the ones named with `--allow-profile` (repeatable; `*` allows every profile); anything else is refused. `--playwright-cmd` names the program that runs Playwright MCP, called as `PROGRAM ENDPOINT OUTPUT_DIR`; `--url`, `--secret-file` as for the other commands.

```json
{ "command": "browser-daemon", "args": ["mcp", "--url", "https://browser.fnuc.example", "--allow-profile", "work"] }
```

## Secrets: `browser_fill_secret`

Typing a password with Playwright MCP's own tools puts it in the tool call, and Playwright MCP echoes every script it runs into its answer and logs. `browser_fill_secret` keeps the value out of both:

1. The entry must be named with `--allow-secret` (repeatable; exact rbw entry names, or `*` for every entry of the vault).
2. The value is read from the RBW agent API with a client certificate and an explicit item/field grant. An empty field is an error; it never falls back to another field.
3. The page must match one of the entry's granted URIs, using its saved match type. The address comes from Playwright's page, not from page script.
4. The field is focused with a Playwright click on the agent's `target`, then the node types the value over CDP (`POST /api/pages/{page_id}/insert-text`, with the entry's URIs and the session the page is in: a page id that is not in that session, or that several sessions show, is refused; refused unless the page, and the frame with focus, are on a site the entry is saved for, checked on the same connection right before typing; the text is never logged).
5. Every value filled in the session is masked as `•••` in the answers and error messages of all later tool calls, also as JSON-escaped, percent-encoded, HTML-escaped and base64 (standard and URL-safe alphabet, at all three byte alignments, so a copy inside a longer encoded string is caught too), and the `user:password` pair of the entry's username (HTTP Basic auth) the same way. Short values can also mask unrelated text.
6. The answer of `browser_fill_secret` only says what was typed where (`Typed the password of 'entry' into <element> on <host>`), never the value. Before typing, the field is marked `data-bd-secret` and given a style (from an isolated world, as a constructed stylesheet so a page's CSP does not block it) that shows its value as dots whatever its `type` becomes (`-webkit-text-security: disc`), so a page's "show password" toggle does not put the value into a screenshot. The vault button's autofill does the same for every field it fills (username, password, TOTP), in Firefox with transparent text instead, as Firefox has no text-security. What this does not protect: a page that replaces the element (a re-render drops the mark), a field in a frame nested deeper than the focused frame, text the page copies elsewhere (a "your password is ..." line), canvas or custom-drawn inputs, and the page's own script, which can read the value or remove the mark. Playwright's snapshot reads the field's value whatever the screen shows, so the MCP server masks the filled values in what it returns and, as Playwright MCP keeps snapshots and logs in files its answers only link, in the text files under `--output-dir` written during each call.

The MCP server connects to the RBW agent API directly; it does not use the [secret agent](rbw.md) of the live views. This guards against accidents, not against an agent bent on reading the value back in another form (e.g. encoded by a script): only allow entries for agents you trust with them.

### RBW agent API

To use secret or card fills, configure all four options: `--rbw-api-url https://vault.example:PORT`, `--rbw-api-ca /path/ca.pem`, `--rbw-api-cert /path/client.pem`, and `--rbw-api-key /path/client-key.pem`. The key must be private to its owner. Provision a separate client certificate and grant it only the item IDs and fields needed using RBW's remote API policy. RBW scopes each client to one account; for several accounts, repeat `--rbw-api-cert` and `--rbw-api-key` in matching order. The client verifies the server against the supplied CA, presents its certificate and disables redirects. Startup fails if fills are enabled without API configuration. Login fills require an RBW agent with `uri_match/N` and `uri_all` API support.

Login fills require the requested field and at least one URI grant for the entry. The RBW API also returns each granted URI's match type so the node can enforce the saved policy immediately before typing. Use an item ID or the `user` selector to disambiguate names; `user` requires a username grant. Grant `username` as well as the requested field to retain HTTP Basic credential redaction.

To retain an existing `--allow-secret '*'` configuration, an administrator may grant `username`, `password`, `totp`, `uri_all` and `custom_all` to each account's client with `rbw api grant CLIENT --account ACCOUNT --item-id @all --field FIELD ...`. This is broad access to login and custom fields throughout that account, including future items. Card numbers and security codes still need individual card grants and `--allow-card` rules.

Card fills use the same `--allow-card` rules as the local backend. Grant `card_number`, `card_code`, `card_exp_month` and/or `card_exp_year` as needed; `exp_date` needs both expiry fields. RBW's API may refuse access while the vault is locked. This backend covers MCP fills only; live-view login search and passkeys still use the secret agent.

Default tabs are not opened for Playwright connections (it ignores tabs it did not open); a profile's login is what an agent gets from it.

## Cards: `browser_fill_card`

Allow each card and merchant/payment-frame pair explicitly, for example `--allow-card 'My card|https://shop.example|https://payments.example'`. Repeat the flag for other pairs. Origins must be exact HTTPS origins, including any non-default port; paths, wildcards and credentials are not accepted. The tool accepts `number`, `exp_month`, `exp_year`, `exp_date` and `cvv` (RBW's `code`) and a snapshot target. Use `exp_date` for a single `MM/YY` field: it combines and validates the RBW month and year. It only reads Card entries, requires the authorized merchant page and payment iframe, checks them again before insertion, and never submits the form. For a cross-process iframe Chromium omits from the page's frame tree, the daemon checks that its target is a direct child of the merchant and verifies its origin before protecting the field. A frame-check error may identify an unauthorized origin, without URL paths or queries. The same redaction limitations described above apply; a short card field can mask unrelated text in later results. Chromium sessions only.

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

## Logins: `browser_export_auth` and `browser_fetch`

An agent that is signed in through a browser sometimes needs that login elsewhere: a site's API, `curl`, a CLI. Reading cookies or tokens with Playwright's tools would put them into the tool call's answer and the logs. These tools keep them out:

- `browser_fetch` makes one HTTP request (`url`, `method`, `headers`, `body`) with the cookies the browser would send to that address and, with `bearer_storage_key`, `Authorization: Bearer` with a token from the page's local or session storage (the value, or its `secret`/`access_token` field when it is JSON, as MSAL keeps them). The request is made by the MCP server, not the browser. The answer (status, headers without `Set-Cookie`, the first 64 KiB of the body) has the login's values masked.
- `browser_export_auth` writes the login to a new file only the user can read, in `$XDG_RUNTIME_DIR/browser-daemon-mcp/` (memory), deleted after `minutes` (default 15) or when the server exits: a `Cookie:` header line (`format: header`, the default, for `curl -H @FILE`), a Netscape cookie jar (`cookies`; curl 8.22 drops every cookie of a jar file it reads, "domain '[file]' must not set cookies", other tools take it), or JSON with the cookies and the `storage_keys` asked for (`json`). The answer gives the path, the cookie names and all of the page's storage keys (`storage_filter` lists only those containing a word, e.g. `token`), never a value.

Either works on the agent's own page, or on a shared browser's tab (`browser`, `tab`, `daemon` from `browser_shared_nodes`) that its owner shares **with control**. The page, and every address the cookies are for, must be on a site named with `--allow-export` (repeatable; a site covers its subdomains, `*` allows any). The cookies are those the browser sends to the addresses, HttpOnly ones included. The values (cookies of 8 characters or more, and the storage values used) are masked in all later results, as filled secrets are.

The node serves this as `POST /api/pages/{page}/auth` (a session's page; Chromium over CDP, Firefox over BiDi) and `POST /api/extension/nodes/{id}/tabs/{tab}/auth` (a shared tab with control; the browser extension reads them, Firefox with its `cookies` permission), with `{urls, storage}`; both need the cluster secret and log nothing of the values. The shared-browser command route never takes the extension's `authExport` action, so `browser_shared_command` cannot read them.

What this does not protect: a file the agent's shell can read, it can also print. An exported session cookie is the account, often without a second factor: allow only sites agents need, keep the files short-lived, and treat one that reaches a transcript as leaked (sign that session out).

## Bot walls

After `browser_navigate`, `browser_navigate_back`, `browser_click` and `browser_press_key` the server checks whether the page is a bot wall: Cloudflare, Akamai, DataDome, PerimeterX (HUMAN), Imperva (Incapsula), AWS WAF, a page that is only a reCAPTCHA or hCaptcha, or a generic "Access denied" or 403 page. The check is conservative: a form that embeds a CAPTCHA or a challenge script is not a wall; a strong marker (or a weak one on a page with almost no text) is. It runs a small script in the page and says nothing when the script fails.

When a wall is found, the server polls for up to `--challenge-wait` seconds (default 15; `0` only reports) for it to clear by itself, as many non-interactive checks do. The answer then ends with a note that the check cleared, or with a note that names the vendor, tells the agent not to try to solve it and, if `--ask-human-cmd` is set, to call `browser_ask_human`. The call never blocks longer than the wait.

## Network recording: `browser_network_har`

`browser_network_har` takes `action`: `start`, `stop`, `status` or `cancel`. `start` records the requests of all tabs of the browser; `stop` writes a HAR 1.2 file to `har/<timestamp>-<id>.har` in the output directory (`--output-dir`) and returns its path and the number of requests.

- The recording runs in Playwright MCP: listeners on the browser context keep the entries on the context object, which stays the same between `browser_run_code_unsafe` calls. Switching browsers (`browser_use`) loses a recording.
- Entries have timings where Playwright knows them. Text response bodies (text, JSON, XML, JavaScript, form data) are kept up to 256 KiB each and 16 MiB in all; at most 2000 requests are kept, the rest are counted. Posted bodies are not recorded.
- Credentials are replaced with `[redacted]`: the values of `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, and any header whose name contains `token`, `secret`, `auth`, `api-key`, `csrf`, `xsrf` or `session`; the values of cookies; query parameters whose names contain `token`, `code`, `pass`, `key`, `secret`, `sig`, `auth`, `session`, `jwt`, `otp` or `credential`; secret-looking fields of JSON bodies. JSON that cannot be parsed (cut off at the cap) is left out. HTTP/2 pseudo-headers are dropped. Values filled with `browser_fill_secret` are masked like everywhere else. Other bodies (HTML, scripts) are kept as they are: treat the file as sensitive.
- The file and its directory are readable by the user only (0600, 0700).

## Flight recorder: `browser_flight_recorder`

`browser_flight_recorder` takes `action`: `start` (optional `seconds` to keep, default 60), `stop`, `status` or `save`. It calls `POST /api/sessions/{id}/flight-recorder` on the node of the agent's page, which buffers the session's video. `save` stores the buffered seconds as a recording and the answer gives its id and a link to it in the dashboard (`<node>/#recordings/<id>`), so the user can watch what just went wrong.

## Human-like input

`--human-input` adds `human_input=1` to the CDP address of the node's `/chromium` endpoint. The node's CDP proxy then makes the agent's mouse and keyboard input human-like.

## Session label: `browser_set_topic`

The dashboard shows what each agent connection is doing. `browser_set_topic` (`topic`, a few words; empty clears it) sets that label for every connection of the agent's conversation, so agents need no shell script. The conversation id is read once at startup from `CLAUDE_CODE_SESSION_ID`, else `CODEX_THREAD_ID`, else `CODEX_SESSION_ID`; without one the tool says so. The label goes to `POST /api/agent-topic` on every online node (no secret is sent) and the answer counts the connections labelled. A label only sticks to connected clients: when no browser is connected yet, the topic is kept and applied after the first browser tool call, and again after each browser switch.

## Recording and sharing

`browser_recording` starts and stops an ordinary recording of the agent's own session (`POST /api/sessions/{id}/record`, `.../stop-record`) and says whether one is running; the answer names the recording id and the dashboard link (`<node>/#recordings/<id>`). Recordings cannot be named.

`browser_share` and `browser_unshare` exist only with `--allow-share`, as a public link needs the user's consent in advance. `browser_share` makes a [public link](sharing.md) (`POST /api/shares`) to the agent's session, view-only unless `control: true` (Chromium), or to a recording (`recording_id`). The link works for anyone without signing in, so the description says so. `expires` is a duration (`30m`, `1h`, `2d`; default 1 hour, at least a minute, at most 7 days): links that never expire cannot be made here. `browser_unshare` (`share_id`) ends a link; it only ends links made by this server, never the user's or another client's.

## Tool definitions

Every tool of the server has a title and the hints `readOnlyHint`, `destructiveHint`, `idempotentHint` and `openWorldHint`, so clients can decide what needs a confirmation (for example `browser_use` and `browser_fetch` are marked destructive, `browser_status` and `browser_live_link` read-only). Input schemas list `required` fields and refuse unknown ones. Failures of a tool, including a browser that did not answer, come back as results with `isError` and a message to act on; only unknown tools and malformed arguments are protocol errors.

## Censored screenshots: `browser_screenshot_censored`

The visual counterpart of the redacted HAR. Arguments: `fullPage`, `target` (an element reference from the snapshot or a CSS selector; `element` describes it), `type` (`png`, the default, or `jpeg`) and `censor`, a list of categories (default: all but `emails`):

| Category | What is blacked out |
| --- | --- |
| `passwords` | `input[type=password]` |
| `cards` | `autocomplete=cc-*`, inputs whose name or id says card, cc number, cvc, cvv, expiry or security code, and every cross-origin iframe (payment frames) |
| `otp` | `autocomplete=one-time-code`, inputs named otp, totp, 2fa, mfa, one-time or `code` |
| `secrets` | every element whose text or value holds a value filled in this session with `browser_fill_secret`, `browser_fill_card` or used by `browser_export_auth` (also as the encodings the other masks know) |
| `inputs` | text inputs, textareas and editable elements that have a value |
| `emails` | elements whose text, or inputs whose value, hold an email address (opt-in) |

The server marks the elements in the page and every same-origin and cross-origin frame, takes the screenshot with Playwright's `mask` option (`maskColor` black) and removes the marks again. The answer is an image, as Playwright MCP's own screenshot, and a line saying how many regions were blacked out. The secrets are not put into any script, as Playwright MCP echoes scripts: a first script returns the page's texts, the server looks for the secrets in them, and the second script only gets the numbers of the elements to mask.

It fails closed: when the mask cannot be built (a script error, an unreadable frame) the answer is an error and no screenshot. What it does not cover: Shadow DOM, canvas and images with text, text the page shows in other forms than it was filled in, and secrets the agent typed with `browser_type`. With `--censor-screenshots` the agent can still reach the page with other tools, so this limits accidents, not an agent bent on reading a secret.

### Shared browsers

`browser_shared_command` with `action: screenshot` takes the same `censor` list (also as `params.censor`). The extension of the user's browser blacks out the content with overlays; the server sends it the categories and, for `secrets`, the values filled in this session (`params.secrets`: they go to the user's own browser, which has them typed in already). With `--censor-screenshots` the default categories are always added to a shared screenshot, whatever the agent asks for. If the extension's reply is not marked `censored: true` (an old extension), the image is discarded and the agent gets an error that tells the user to update the extension. The extension fails closed as well.
