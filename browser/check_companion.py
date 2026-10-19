#!/usr/bin/env python3
"""Native framing and the companion's independent permission boundary."""
import io
import json
import struct
import time
import unittest

from companion import Companion, MAX_MESSAGE, local, read_native, remote, response


class Driver:
    def __init__(self):
        self.document = "document-1"
        self.origin = "https://example.org"
        self.executed = []

    def resolve(self, tab, origin):
        # Like the chrome script: the tab's current origin must be the granted one.
        if origin != self.origin:
            raise ValueError("Origin changed")
        return {"handle": f"context-{tab}", "document": self.document}

    def page(self, context, declaration, args=()):
        self.executed.append((context, declaration, list(args)))
        return {"title": "Test"}

    def call(self, method, params=None):
        self.executed.append((method, params))
        return {}

    def actions(self, context, source):
        self.executed.append((context, source))


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

    def test_grants_follow_new_documents_but_never_another_origin(self):
        self.driver.document = "next-document"
        self.assertEqual(self.host.execute({"tab": 1, "action": "snapshot"}), {"title": "Test"})
        self.assertEqual(self.driver.executed[-1][0], "context-1")
        self.driver.origin = "https://elsewhere.example"
        with self.assertRaises(ValueError):
            self.host.execute({"tab": 1, "action": "snapshot"})

    def test_page_code_runs_in_the_sandbox_after_an_origin_check(self):
        self.host.update([{**self.grant, "control": True}])
        self.host.execute({"tab": 1, "action": "evaluate", "params": {"script": "return 1;", "args": [2]}})
        context, declaration, args = self.driver.executed[-1]
        self.assertEqual(context, "context-1")
        self.assertTrue(declaration.startswith("function(origin, args) { if (location.origin !== origin) throw"))
        self.assertEqual(args, ["https://example.org", [2]])

    def test_bidi_values_are_data_only(self):
        value = {"a": [1, "x", True, None], "b": {"c": 2.5}}
        self.assertEqual(local(value)["type"], "object")
        for bad in (float("nan"), float("inf"), object(), print):
            with self.assertRaises(ValueError):
                local(bad)
        serialized = {"type": "object", "value": [["a", {"type": "array", "value": [{"type": "number", "value": 1}, {"type": "string", "value": "x"}]}],
                                                    ["n", {"type": "node", "sharedId": "x"}], ["u", {"type": "undefined"}]]}
        self.assertEqual(remote(serialized), {"a": [1, "x"], "n": None, "u": None})

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
