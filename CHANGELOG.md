# Changelog

## [Unreleased]

### Fixed

* The secret agent refused passkey challenges and credential ids longer than 128 characters ("Bad challenge"), which Microsoft's sign-in sends; it takes up to 1024 now. Values starting with a dash reach `rbw passkey assert` as `--challenge=…`/`--allow=…`, and the entry after `--`.

## [0.7.0] - 2026-10-29

### Added

* `browser-daemon mcp` has `browser_passkey` (with `--allow-passkey`): it arms the agent's Chromium session so the secret agent signs its passkey sign-ins on the allowed sites, also across origins and on page load (`POST /api/sessions/{id}/agent-passkey`).
* `browser-daemon mcp` has `browser_ask_human` (with `--ask-human-cmd`): it runs a program with a link to the live view of the agent's own page, for CAPTCHAs, approvals and other steps only a person may do.
* WebM exports support concise mode (`--concise`, `?mode=concise`, `/export-concise.webm`), capping still periods at 5 seconds and slicing audio to match.
* The dashboard recording and player menus offer both Concise and Raw WebM downloads.

### Fixed

* Concise recording playback in the dashboard and public share views adjusts total playtime and the progress indicator to reflect concise durations, advancing continuously across capped intervals and reaching the end of the timeline.

## [0.6.2] - 2026-10-24

### Changed

* The signed browser extension packages are no longer committed to the
  repository: they are release assets, and the Nix package fetches them by the
  hash pinned in `browser/packages/metadata.json`.
* Updated the `psl` and `uuid` dependencies.

## [0.6.1] - 2026-10-24

### Changed

* The vault in the live view follows rbw 2.31.2: the new ninja crab logo,
  duplicate, archive/trash notices and status pills, and an "All items" view
  that includes archived and trashed items.

## [0.6.0] - 2026-10-24

### Added

* The vault popover copies a login's password (else its one-time code, else
  the username) to the clipboard of the device viewing, and its menu copies the
  username, password or TOTP. The value goes agent → node (handed out once) →
  live view → clipboard; the extension's frame never sees it.
* Extensions can show sites' favicons (the `favicon` request: the node fetches
  them) and know their version (`init`); a passkey prompt can let the browser
  ask its own way ("Use another authenticator").

### Changed

* The secret agent leaves finding logins to rbw: `rbw search --url <page>`
  (rbw 2.30 and later) lists the logins saved for the page by rbw's URI
  matching, the one its browser extension uses, and searches; a fill checks
  the entry with it too. Its own vault index and matcher are gone. The node
  passes the page's address (without query and fragment) along, so scheme,
  port and path count, and with several rbw accounts the agent fills from the
  right one.

### Fixed

* Passkeys in Chromium sessions failed for passkeys without a counter while
  rbw's `passkeyCounter` is off ("inject would start one"): the live view now
  has rbw sign the page's challenge in every browser, as it did in Firefox and
  shared browsers, instead of putting the passkey into Chromium's virtual
  authenticator.
* The vault listed every login of the site's registrable domain as suggested
  (50 for `auth.brkn.lol`), and an index built while rbw was locked made every
  lookup and search come back empty until rbw next synced.

## [0.5.0] - 2026-10-24

### Added

* Live-view extensions: the vault's toolbar button, popover and passkey prompt
  are now a bundle the live view runs in sandboxed frames and talks to by
  message (`docs/extensions.md`), loaded with `--extension-dir` (NixOS:
  `liveViewExtensions`).

### Changed

* The vault's UI comes from rbw (`browser/browser-daemon` there), in the rbw
  extension's current look, through the `rbw` flake input, now a flake
  (`packages.rbw-extension`, the NixOS module's default). browser-daemon keeps
  no rbw styles or icons of its own: `just rbw-theme` and the `rbw-theme` flake
  check are gone. Without an extension the live view has no vault button.
  The `rbw` input follows rbw's `release` branch (2.29.0 brings the bundle; its
  fills confirm in place like the rbw popup's).

## [0.4.3] - 2026-10-23

### Fixed

* The browser extension detaches its debugger before a shared tab navigates to
  a protected page, clearing automation overrides and request interception.
* Profile imports validate archives before extraction and enforce expanded-size,
  entry-count, metadata-size and time limits. Sparse files are refused.

## [0.4.2] - 2026-10-23

### Added

* Live-view tabs close from a hover button and have Reload, Pin and Close menus,
  including managed sessions. Profile pictures appear in the profile menu, top
  bar and session menu, and Escape closes the tab chooser and local dialogs.

* The extension offers **Re-pair** for a remembered browser, reusing its
  configured daemon, and the pairing button is now called **Pair**. Popup
  buttons have icons, and **Forget this browser** stays disabled while
  disconnected.

## [0.4.1] - 2026-10-23

### Added

* Firefox without the native companion supports basic text editing, hover,
  backward focus traversal and input in accessible embedded frames. Live views
  identify DOM control and show capture failures with a retry action.

* Seeking into an unloaded recording fetches a bounded window around that
  timestamp immediately. Dashboard downloads pause for the foreground request
  and resume afterward; new seeks cancel stale requests. Public links support
  the same window fetching with sanitized addresses.

* Recording players can copy links at the current timestamp, preserving playback
  mode and using original recording seconds in both dashboard and public links.

* `GET /api/recordings/vacuum` lists what the retention policy would remove
  now, without removing it; the cleanup's answer names the recordings too
  (`ids`).

### Changed

* Browser sharing excludes additional loopback spellings and daemon aliases.
  Chromium automation uses an explicit method allowlist and refuses executable
  initialization and reload scripts while retaining Playwright utility worlds.
* Firefox viewers share one capture loop per tab. Pairing accurately describes
  automatic tab and sound sharing, and setup works when opened in a browser tab.

* Recording players keep the original timeline and fixed total duration in both
  playback modes, so the duration and seek bar no longer grow as frames load.

* Handover returns to a popup's opener when it closes, keeping the picture visible
  while switching instead of flashing a session-disconnected error.

* Recording playback defaults to Concise in the dashboard and public links: still
  periods are capped at five seconds. Switch to Raw for the original timing.
  The seek bar follows the selected mode; stored recordings and downloads stay raw.

* Dashboard: "Stop all" is now "Prune sessions", and its dialog shows the
  sessions it stops as their cards, which work there as in the grid.
* Dashboard: "Clean up" is now "Prune recordings", and its dialog lists the
  recordings the retention policy removes before it does.
* Dashboard: sessions are listed newest first; agent-driven ones are no
  longer pinned to the top.
* Dashboard: the recordings list selects all with a checkbox in its header
  (the button stays on phones, which have no header).

## [0.4.0] - 2026-10-22

### Added

* `services.browser-daemon.secretAgent` runs the secret agent from the NixOS
  module, as a system service for the vault's `user` (off by default). It
  gets the cluster secret as a systemd credential, so that user need not be
  able to read the file, and serves this host's node unless `nodes` says
  otherwise. It doesn't need the node itself (`enable`).
* `docs/rbw.md`: how the live view, the secret agent, the MCP server and rbw
  work together, what is checked where, and how the vault's styles follow rbw.

### Changed

* `browser-daemon passkey-helper` is now `browser-daemon secret-agent`: it
  serves the whole vault of the live views (logins as well as passkeys). Its
  routes moved from `/api/passkey-helper/*` to `/api/secret-agent/*`, and
  helpers that predate passkey lookups or autofill are no longer handled. Nodes
  and agents must be updated together; there is no alias for the old name.

* Node cards in the Nodes view are compact: "New session" (or "Take over")
  and "Settings" are buttons in the card header, the stats are one inline
  strip with a capacity bar under the sessions, and uptime drops the seconds
  once a node is a minute old. The per-card "Filter" buttons are gone.
* The session count badge on the tab icon is red instead of green.
* The live view's vault popover and passkey dialog follow the rbw
  extension's new look: its violet palette, shield icon, uppercase section
  headings with counts and rounder login cards with key icons. The palette
  and icon now come from rbw's `theme.css` through the new `rbw` flake input:
  `just rbw-theme` restyles the live view after a bump, and `nix flake check`
  fails while `handoff.html` and the locked rbw differ.
* Taking over a session an agent is driving shows who: the agent, its topic,
  machine, project and how long it has been connected. The take-over icon is
  used on every take-over button, toasts included.
* Public sessions stand out with a full border, glow and top bar. The clock,
  shield and control indicators sit next to the session name, and "Control
  allowed" / "View only" is an icon with a tooltip.
* A browser connected through the extension is labelled "Real browser" instead of
  "Shared browser".
* The session menu is shorter: auto-stop and proxy moved into a "Settings…"
  dialog, and "New tab…" is gone.

## [0.3.27] - 2026-10-22

### Changed

* The Nodes view has a single "Add node" button that opens one dialog with
  Browser and Daemon tabs. The pairing step no longer repeats the remote
  control and key expiry choices, which the extension popup asks for. The
  dialog closes once the browser connects, and newly connected browsers are
  announced with a toast and highlighted.
* The offline cleanup button is labelled "Prune nodes" and its confirmation
  lists the nodes it will remove.

### Fixed

* A remembered browser is retried every minute until it reconnects, also after
  a daemon restart or a rejected allowlist check.

## [0.3.26] - 2026-10-22

### Added

* Daemon peers have a separate entry point from browser extension pairing.
* The Nodes view can remove all offline daemon peers and disconnected browsers
  in one confirmed cleanup action.

### Changed

* Follow focus is disabled by default for newly connected browsers.
* Firefox's toolbar icon fills its slot more clearly, and the dashboard shows
  the companion warning as a hoverable icon beside the browser status badges.
* Handoff notifications show a leading icon for clipboard, navigation, files,
  keyboard, passkey, success, and error messages.

### Fixed

* Firefox remote input now forwards hover, click counts, modifiers, and
  pointer-positioned scrolling. Extension-only sessions can select page text,
  select words on double-click, and dispatch page context-menu events when
  trusted input is unavailable. Copying a remote selection also works without
  the Firefox companion.

## [0.3.25] - 2026-10-21

### Changed

* Firefox shows the remembered-connection failure in the popup and distinguishes
  a different extension installation from a missing daemon permission. Errors
  remain visible until dismissed, and failed origin discovery retries.
* The toolbar uses a clear red dot while offline and a separate pulsing client
  indicator while a remote client is connected.

## [0.3.24] - 2026-10-21

### Added

* Managed extensions can discover daemon origins from a configured server,
  with updates pushed over their existing WebSocket connection.

### Changed

* Extension-backed tabs send the browser's exact favicons to handoff, including
  private URLs, in bounded batches.

## [0.3.23] - 2026-10-21

### Changed

* The allowed-daemon list uses themed rows with per-daemon add and remove
  controls instead of a plain text area.
* The Firefox companion status uses the same pill styling as the browser
  connection status.
* Removed the redundant **Open in a tab** button from the popup.

## [0.3.22] - 2026-10-21

### Changed

* Firefox pairs with its native companion automatically, and disconnecting the
  companion no longer stops browser sharing. The popup reconnect flow is clearer
  when a browser was previously paired.
* Handoff settings now group browser, input, view, audio, and picture controls
  under their own headings.

### Fixed

* Firefox extension-only sessions now insert typed characters at the caret or
  replace the selection instead of replacing the entire field value.
* Firefox companion grant updates defer discarded tabs until they have a
  browsing context, keeping the other shared tabs connected.
* Automatic and manual Firefox companion startup share one connection attempt.

## [0.3.21] - 2026-10-21

### Changed

* Firefox installs the node's native messaging, history, bookmarks, and file
  transfer API permissions as required permissions. The popup checks existing
  grants before asking for optional permissions, preserving Chromium behavior.
* The Firefox toolbar draws a pulsing red client mark and a separate offline
  mark in the same top-right icon style as rbw.

## [0.3.20] - 2026-10-21

### Changed

* Handoff and the dashboard warn when a Firefox browser is missing its native
  companion. Firefox's fallback input now dispatches pointer and mouse press
  and release events before activating a target.
* The handoff page and Firefox extension share one Follow in the browser setting.
* The handoff tab strip shows labels for unpinned tabs and favicons only for
  pinned tabs. The tab context menu can reload and close tabs, with an icon for
  each action.

## [0.3.19] - 2026-10-21

### Changed

* Handoff tab strips show the browser's real favicon and no longer crowd each
  tab with a pin icon. Right-click a controllable extension tab to pin or unpin
  it; pinned state is labeled in the Tabs list.

### Fixed

* The connected toolbar indicator is a visible red dot, not a thin badge bar.
* Address suggestions in extension-backed sessions query the browser's history
  and bookmarks instead of the managed-session history endpoint.

## [0.3.18] - 2026-10-21

### Added

* Extension-backed tabs in the handoff view can be reordered by dragging and
  pinned or unpinned. Existing pinned state is shown in the tab strip.
* Handoff notifications can be dismissed and wrap to two lines. Opening the
  Tabs view focuses its search field.

### Changed

* The extension toolbar shows a red dot while a connected client is using the
  browser. The popup tab list has a chevron, and **Sharing settings** is now
  **Settings**.

### Fixed

* Selecting a discarded Firefox tab through remote control wakes and loads it.

## [0.3.17] - 2026-10-21

### Added

* Extension-backed browser sessions can suggest destinations from browser
  history and bookmarks on the handoff page. The owner must enable address
  suggestions and grant the optional browser permissions.

### Changed

* The tab picker is now called **Tabs** and includes a title and URL search.

### Fixed

* Firefox extension-backed sessions can complete passkey requests through the
  live view, including when a page replaces the WebAuthn handler after load.
* Remote clicks reach controls inside nested shadow roots, including complex
  Home Assistant and Argo CD interfaces.

## [0.3.16] - 2026-10-21

### Added

* Vault fills also fill linked custom fields that stand for the login's
  username or password (needs rbw 2.22.13 or later, which reports the link).

## [0.3.15] - 2026-10-21

### Added

* Vault fills also fill the entry's text custom fields, as the rbw extension
  does: an input whose id or name is the field's name, else one whose
  autocomplete, aria-label, placeholder or label matches it. Hidden, boolean and
  linked fields are never filled.

### Changed

* Ctrl+wheel zoom in the handoff view: a mouse wheel notch is a gentle, eased
  step instead of a jump between a few levels (a touchpad pinch still follows
  one to one); Ctrl+plus/minus are eased too.
* The handoff view follows the tab a dropped file opens when the page has no
  drop handler.

## [0.3.14] - 2026-10-20

### Changed

* Extension popup: the connected status control disconnects; tab and sharing
  options start collapsed, Follow focus is renamed, and dashboard navigation
  is clearer.
* Handoff settings now live in a dialog opened from the overflow menu. The
  recorder control starts recording, new blank tabs switch into view at once,
  and extension-backed sessions appear in the session picker.
* File drops preserve the pointer coordinates and dispatch to the selected
  page element.

## [0.3.13] - 2026-10-20

### Added

* Extension handoff views support file uploads, file drag-and-drop and shared
  downloads, including page-generated files. Enable **File transfer** in the
  owner’s extension; a control grant and site access are required.
* File transfers accept up to **1 GiB per file**. Regular handoff uploads use
  the same limit. Extension transfer buffers are capped at 2 GiB in total.

* Extension sessions support public links with owner permission, revocation,
  optional control, URL navigation, and all shared tabs. Live public views play
  session audio unless the owner disables it.
* Desktop viewers support Ctrl+plus/minus and Ctrl+wheel zoom, Ctrl+0 reset,
  and drag-to-pan in view-only mode.

### Fixed

* Shared sound is enabled by default unless the owner saved an off setting.
  Installation/update requests the site access required for media capture.
* Public capture uses the visible viewport without a scaled document clip,
  preventing gray captures and flashing in concurrent handoff views.
* Overflow menus scroll within small windows. Picture quality has a separate
  Auto checkbox; manual quality supports the full range through 100.
* Share actions use an Android-style connected-dots icon.
* A real stereo-media smoke check covers view-only playback and owner revocation.

## [0.3.12] - 2026-10-20

### Added

* Extension live views can play audio/video media sound from the viewed tab,
  including permitted embedded frames. Enable **Share sound** in the owner’s
  extension. View-only clients can listen; capture stops when sharing or the
  last listener ends. Standalone Web Audio and protected media are not captured.
* Extension-backed nodes and sessions carry an **Addon** badge with a puzzle icon.

### Changed

* The live view’s vault and passkey dialogs follow the rbw extension’s neutral
  palette, branded header, search and account cards, with a compact layout
  for phones and clear locked, loading and empty states.

## [0.3.11] - 2026-10-20

### Fixed

* Live-view passkey requests keep reaching the view when rbw or another
  extension replaces the WebAuthn handler after the page loads. The provider
  remains the fallback when the view is absent or chooses the browser.

## [0.3.10] - 2026-10-20

### Added

* Playwright and other CDP clients drive a shared Chromium browser:
  `connectOverCDP("https://<daemon>/api/extension/nodes/<id>")`. Its tabs
  shared with control are the pages; `newPage()` opens a tab. Other sites'
  cookies, downloads and the owner's files stay out of reach, and what a
  client set in a tab is undone when it disconnects.
* Shared browsers: passkeys offered in a login field's suggestions
  (conditional mediation) appear as a "Sign in with a passkey" button in the
  live view, instead of in a dropdown on the owner's screen.
* Live view: "Reopen closed tab" and "Follow in the browser" (per shared
  browser) in the session menu.

### Changed

* The live view never uses the browser's alert/confirm/prompt: its own dialogs,
  also for the page's dialogs ("Leave this page?").
* Dashboard: node, browser and engine cards are laid out for phones; their
  settings (default proxy, removing a peer, disconnecting a browser, engine
  defaults) moved behind a gear; "Show only" is "Filter"; "Add a node" is a
  sheet behind **Add node**.
* CI is split into `build`, `test` and `release` workflows (as in rbw): Rust
  jobs use rust-cache, Nix jobs keep their store between runs.
* The Nix package builds with crane: the dependencies are their own
  derivation, which neither source changes nor version bumps rebuild.
* Releases: `just release-prepare X.Y.Z`, then `just release-tag`. A version
  tag publishes Linux binaries and the signed browser extension, with
  checksums and build attestations.

### Fixed

* The extension no longer disconnects on daemon messages over 256 KB.

## [0.3.8] - 2026-10-20

### Added

* Shared browsers' live views: the vault button fills logins into a tab
  shared with control, only in its frames on the entry's sites.
* Passkeys in shared Chromium tabs with control: while a controlling live view
  watches, the page's request goes to the view and the vault signs it for the
  tab's own site. Unwatched, the browser's own dialog opens as usual.
* Live view zoom: two fingers (or Ctrl+wheel) magnify the picture, with sharper
  frames for shared browsers, which can't be resized.

## [0.3.7] - 2026-10-19

### Added

* Extension: "Follow live view" (on by default): the tab a live view switches
  to comes to the front in the shared browser.
* New browser session: connected browsers are listed with the nodes; for one,
  the dialog opens a tab there. "Public link" creates the share at launch.

### Changed

* Dashboard: "Expires in Nd" for offline browsers; "Clean up" moved to the
  Recordings toolbar; the "Live view" menu entry is gone.

## [0.3.6] - 2026-10-19

### Fixed

* Shared browsers' live views stream a screencast instead of a picture a second.

### Added

* + in a shared browser's live view works as in a regular session.
* Session menu: picture quality slider, remembered per viewer.

## [0.3.5] - 2026-10-19

### Changed

* Faster shared-browser live views: TCP_NODELAY on the daemon's sockets, and
  the extension keeps up to four frames in flight to viewers.
* With no live shared tab, an address typed in the live view opens the
  browser's first tab.

## [0.3.4] - 2026-10-19

### Added

* Shared and unshared tabs reach live views in about 100 ms (long poll).

### Changed

* "Reconnects after restart" reads "Key expiry disabled".

## [0.3.3] - 2026-10-19

### Added

* "Share all tabs" (on by default); a connected browser is listed before
  anything is shared; controllers can open its first tab.

## [0.3.2] - 2026-10-19

### Changed

* The Firefox companion speaks WebDriver BiDi instead of Marionette.

## [0.3.1] - 2026-10-19

### Added

* Shared browsers record like sessions; controllers open tabs remotely; the
  extension updates itself and reports its version.
* Offline remembered browsers are forgotten after 24 hours by default
  (`--browser-forget-after-hours`).

## [0.3.0] - 2026-10-19

### Added

* Shared tabs are relayed over CDP for live views; control is kept across
  navigation; redesigned extension popup and pairing.

## [0.2.0] - 2026-10-16

### Added

* Command line, terminal UI and MCP improvements, profile archives and
  transfers, secret masking, and the code split into modules (no changelog
  was kept before 0.3.0; see the git history).

## [0.1.0] - 2026-10-10

### Added

* First release: managed Chromium and Firefox sessions with CDP multiplexing,
  virtual displays and the mobile live view (handoff).
