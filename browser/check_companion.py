#!/usr/bin/env python3
"""Native framing and the companion's independent permission boundary."""
import io
import json
import struct
import time
import unittest

from companion import Companion, MAX_MESSAGE, read_native, response


class Driver:
    def __init__(self):
        self.document = "document-1"
        self.executed = []

    def resolve(self, tab, origin):
        return {"handle": str(tab), "document": self.document}

    select = resolve

    def script(self, script, args=None):
        self.executed.append((script, args))
        return {"title": "Test"}


class NativeTests(unittest.TestCase):
    def setUp(self):
        self.driver = Driver()
        self.host = Companion(self.driver)
        self.grant = {"tab": 1, "origin": "https://example.org", "control": False, "expires_at": int(time.time()) + 600}
        self.host.update([self.grant])

    def test_view_grant_cannot_execute_code_or_send_input(self):
        for action in ("evaluate", "click", "type", "key", "scroll", "cdp", "anything"):
            with self.assertRaises(ValueError):
                self.host.execute({"tab": 1, "action": action, "params": {"script": "return 1;"}})
        self.assertEqual(self.driver.executed, [])

    def test_document_replacement_revokes_the_grant(self):
        self.driver.document = "different-document"
        with self.assertRaises(ValueError):
            self.host.execute({"tab": 1, "action": "snapshot"})
        self.assertNotIn(1, self.host.grants)

    def test_unshared_and_expired_tabs_are_refused(self):
        for tab in (0, 2, 100):
            with self.assertRaises(ValueError):
                self.host.execute({"tab": tab, "action": "snapshot"})
        self.host.grants[1]["expires_at"] = 0
        with self.assertRaises(ValueError):
            self.host.execute({"tab": 1, "action": "snapshot"})

    def test_revocation_replaces_the_entire_grant_set(self):
        self.host.update([])
        with self.assertRaises(ValueError):
            self.host.execute({"tab": 1, "action": "snapshot"})

    def test_native_frames_are_bounded_and_chunked(self):
        with self.assertRaises(ValueError):
            read_native(io.BytesIO(struct.pack("<I", MAX_MESSAGE + 1)))
        for data in (b"", b"\x04\x00\x00\x00ab"):
            with self.assertRaises(EOFError):
                read_native(io.BytesIO(data))
        payload = {"data": "x" * 500000}
        stream = io.BytesIO()
        response(stream, "request", payload)
        stream.seek(0)
        chunks = []
        while stream.tell() < len(stream.getvalue()):
            size, = struct.unpack("<I", stream.read(4))
            self.assertLess(size, 1024 * 1024)
            chunks.append(json.loads(stream.read(size)))
        self.assertEqual(json.loads("".join(c["chunk"] for c in chunks)), payload)
        self.assertTrue(chunks[-1]["done"])


if __name__ == "__main__":
    unittest.main()
