# Changelog

## [Unreleased]

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
