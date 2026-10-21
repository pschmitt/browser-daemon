# Changelog

## [Unreleased]

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
