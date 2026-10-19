#!/usr/bin/env python3
"""Install the optional Linux Firefox native host for the current user."""
import argparse
import json
from pathlib import Path
import shutil


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-directory", type=Path, default=Path.home() / ".mozilla/native-messaging-hosts")
    parser.add_argument("--lib-directory", type=Path, default=Path.home() / ".local/lib/browser-daemon-node")
    args = parser.parse_args()
    for directory in (args.native_directory, args.lib_directory):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    host = args.lib_directory.resolve() / "companion.py"
    shutil.copyfile(Path(__file__).with_name("companion.py"), host)
    host.chmod(0o700)
    manifest = args.native_directory / "dev.pschmitt.browser_daemon.json"
    manifest.write_text(json.dumps({
        "name": "dev.pschmitt.browser_daemon",
        "description": "Selected-tab Firefox control for Browser Daemon Node",
        "path": str(host), "type": "stdio",
        "allowed_extensions": ["browser-daemon-node@pschmitt.dev"],
    }, indent=2) + "\n")
    manifest.chmod(0o600)
    print(f"Installed {manifest}. Restart Firefox with MOZ_REMOTE_ALLOW_SYSTEM_ACCESS=1 firefox --marionette, then enable the companion in the popup.")


if __name__ == "__main__":
    main()
