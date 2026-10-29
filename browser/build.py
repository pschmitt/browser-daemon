#!/usr/bin/env python3
"""Build Firefox and Chromium extensions without third-party build dependencies.

Like rbw's browser/build.py: shared MV3 source, engine-specific manifests,
and the daemon's Cargo.toml version for both browsers. ZIP timestamps and ordering are reproducible.
"""
import json
from pathlib import Path
import shutil
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent
# One version for the daemon and its extension: they speak the same protocol.
VERSION = tomllib.loads((ROOT.parent / "Cargo.toml").read_text())["package"]["version"]


def main():
    for target in ("chromium", "firefox"):
        destination = ROOT / "dist" / target
        if destination.exists():
            shutil.rmtree(destination)
        shutil.copytree(ROOT / "extension", destination)
        # The icons live with the daemon's other logo assets (assets/icons).
        icons = ROOT.parent / "assets" / "icons"
        shutil.copyfile(icons / "icon-192.png", destination / "icon.png")
        for size in (16, 32, 48):
            shutil.copyfile(icons / f"toolbar-{size}.png", destination / f"toolbar-{size}.png")
        manifest = {
            "manifest_version": 3,
            "name": "Browser Daemon Node",
            "version": VERSION,
            "description": "Temporarily share selected tabs with your browser daemon, with optional remote control.",
            "permissions": ["tabs", "storage", "scripting", "alarms", "webNavigation"],
            "host_permissions": ["http://*/*", "https://*/*"],
            "action": {"default_popup": "popup.html", "default_title": "Browser node", "default_icon": {
                str(size): f"toolbar-{size}.png" for size in (16, 32, 48)
            }},
            "icons": {"192": "icon.png"},
            "commands": {"stop-sharing": {
                "suggested_key": {"default": "Ctrl+Shift+Period"},
                "description": "Immediately disconnect and stop sharing all tabs",
            }},
            "content_security_policy": {"extension_pages": "script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"},
            "incognito": "not_allowed",
        }
        if target == "chromium":
            manifest["storage"] = {"managed_schema": "managed-schema.json"}
            manifest["minimum_chrome_version"] = "116"
            manifest["optional_permissions"] = ["history", "bookmarks"]
            # Chrome excludes debugger from optional permissions. Installation
            # permits the API; actual tab/control grants remain explicit.
            manifest["permissions"].append("debugger")
            manifest["background"] = {"service_worker": "background.js"}
        else:
            # These are core node features, so managed installs should not
            # require separate per-permission approvals after installation.
            manifest["permissions"].extend([
                "nativeMessaging", "webRequest", "webRequestBlocking",
                "webRequestFilterResponse", "history", "bookmarks",
                # The daemon's authExport (an agent's tool reads a tab's login).
                "cookies",
            ])
            manifest["host_permissions"] = ["<all_urls>"]
            manifest["background"] = {"scripts": ["policy.js", "audio.js", "files-page.js", "files.js", "favicon.js", "background.js"]}
            manifest["browser_specific_settings"] = {"gecko": {
                "id": "browser-daemon-node@pschmitt.dev",
                "strict_min_version": "142.0",
                "data_collection_permissions": {"required": ["browsingActivity", "websiteContent"]},
            }}
        (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        package = ROOT / "dist" / f"browser-daemon-node-{VERSION}-{target}.zip"
        with zipfile.ZipFile(package, "w", zipfile.ZIP_DEFLATED) as archive:
            for source in sorted(destination.rglob("*")):
                if source.is_file():
                    info = zipfile.ZipInfo(source.relative_to(destination).as_posix(), (2020, 1, 1, 0, 0, 0))
                    info.compress_type = zipfile.ZIP_DEFLATED
                    info.external_attr = 0o100644 << 16
                    archive.writestr(info, source.read_bytes())
        print(package)


if __name__ == "__main__":
    main()
