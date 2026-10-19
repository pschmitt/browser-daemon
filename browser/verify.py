#!/usr/bin/env python3
"""Check signed extension payloads against the build, as rbw's release.py does."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import zipfile

ROOT = Path(__file__).resolve().parent


def verify(package, source):
    with zipfile.ZipFile(package) as archive:
        if archive.testzip() is not None:
            raise ValueError("Corrupt extension archive")
        expected = {p.relative_to(source).as_posix(): p for p in source.rglob("*")
                    if p.is_file() and p.name not in {".amo-upload-uuid", ".web-extension-id"}}
        actual = {n for n in archive.namelist() if not n.endswith("/") and not n.startswith("META-INF/")}
        if set(expected) != actual:
            raise ValueError("Signed package contains unexpected or missing files")
        for name, path in expected.items():
            a, b = archive.read(name), path.read_bytes()
            matches = json.loads(a) == json.loads(b) if name == "manifest.json" else a == b
            if not matches:
                raise ValueError(f"Signed package differs from source: {name}")
        return archive.namelist()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--signed-directory", type=Path, default=ROOT / "dist/signed")
    signed = parser.parse_args().signed_directory
    firefox, = signed.glob("*.xpi")
    entries = verify(firefox, ROOT / "dist/firefox")
    if not any(n.startswith("META-INF/") and n.endswith((".rsa", "cose.sig")) for n in entries):
        raise ValueError("Missing Mozilla signature")
    chromium = signed / "browser-daemon-node-chromium.crx"
    verify(chromium, ROOT / "dist/chromium")
    with chromium.open("rb") as stream:
        magic, version, size = struct.unpack("<4sII", stream.read(12))
        if magic != b"Cr24" or version != 3:
            raise ValueError("Not a CRX3 package")
        stream.seek(12 + size)
        if stream.read(4) != b"PK\x03\x04":
            raise ValueError("Invalid CRX3 payload")
    checksums = "".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in sorted(signed.iterdir()) if p.suffix in (".crx", ".xpi"))
    (signed / "SHA256SUMS").write_text(checksums)
    print("Signed payloads match the extension source")


if __name__ == "__main__":
    main()
