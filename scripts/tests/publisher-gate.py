#!/usr/bin/env python3
"""Test the publisher steps without building an artifact or running the full gate."""

from pathlib import Path
import os
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SHARED = "scripts/check-publish-wasm.sh"


class PublisherGate(unittest.TestCase):
    def test_ci_and_gate_use_the_same_publisher_checks(self):
        for path in ["scripts/gate.sh", ".github/workflows/ci.yml"]:
            with self.subTest(path=path):
                self.assertTrue(
                    f"bash {SHARED}" in (ROOT / path).read_text(encoding="utf-8"),
                    f"{path} does not run the shared publisher checks",
                )

    def test_publisher_checks_require_every_step(self):
        expected = ["build", "tool-pins", "node", "checksums"]
        self.assertTrue((ROOT / SHARED).is_file(), "shared publisher checks are missing")
        for fail_at in [None, *expected]:
            with self.subTest(fail_at=fail_at), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "scripts").mkdir()
                (root / "dist/publish-wasm").mkdir(parents=True)
                (root / "crates/fusiform-publish-wasm/tests").mkdir(parents=True)
                (root / SHARED).write_bytes((ROOT / SHARED).read_bytes())
                for path, name in [
                    ("scripts/build-publish-wasm.sh", "build"),
                    ("crates/fusiform-publish-wasm/tests/build-tools.sh", "tool-pins"),
                    ("node", "node"),
                    ("sha256sum", "checksums"),
                ]:
                    stub = root / path
                    stub.write_text(
                        f'#!/bin/sh\necho {name}\n[ "$FAIL_AT" != {name} ]\n', encoding="utf-8"
                    )
                    stub.chmod(0o755)
                result = subprocess.run(
                    ["bash", SHARED], cwd=root, capture_output=True, text=True,
                    env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}", "FAIL_AT": fail_at or ""},
                )
                completed = expected if fail_at is None else expected[:expected.index(fail_at) + 1]
                self.assertEqual(result.stdout.splitlines(), completed, result.stderr)
                self.assertEqual(result.returncode, int(fail_at is not None), result.stderr)


if __name__ == "__main__":
    unittest.main()
