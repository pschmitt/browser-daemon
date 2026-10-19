#!/usr/bin/env python3
"""Build Firefox and Chromium extensions without third-party build dependencies.

Like rbw's browser/build.py: shared MV3 source, engine-specific manifests,
and a version shared by both browsers. ZIP timestamps and ordering are reproducible.
"""
import json
from pathlib import Path
import shutil
import zipfile

ROOT = Path(__file__).resolve().parent
VERSION = json.loads((ROOT / "version.json").read_text())["version"]


def main():
    for target in ("chromium", "firefox"):
        destination = ROOT / "dist" / target
        if destination.exists():
            shutil.rmtree(destination)
        shutil.copytree(ROOT / "extension", destination)
        shutil.copyfile(ROOT.parent / "icon-192.png", destination / "icon.png")
        manifest = {
            "manifest_version": 3,
            "name": "Browser Daemon Node",
            "version": VERSION,
            "description": "Temporarily share selected tabs with your browser daemon, with optional remote control.",
            "permissions": ["tabs", "storage", "scripting", "alarms"],
            "optional_host_permissions": ["http://*/*", "https://*/*"],
            "action": {"default_popup": "popup.html", "default_title": "Browser node", "default_icon": "icon.png"},
            "icons": {"192": "icon.png"},
            "commands": {"stop-sharing": {
                "suggested_key": {"default": "Ctrl+Shift+Period"},
                "description": "Immediately disconnect and stop sharing all tabs",
            }},
            "content_security_policy": {"extension_pages": "script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"},
            "incognito": "not_allowed",
        }
        if target == "chromium":
            manifest["minimum_chrome_version"] = "116"
            # Chrome excludes debugger from optional permissions. Installation
            # permits the API; actual tab/control grants remain explicit.
            manifest["permissions"].append("debugger")
            manifest["background"] = {"service_worker": "background.js"}
        else:
            manifest["optional_permissions"] = ["nativeMessaging"]
            manifest["optional_host_permissions"] = ["<all_urls>"]
            manifest["background"] = {"scripts": ["policy.js", "background.js"]}
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
