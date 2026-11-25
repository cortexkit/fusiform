#!/usr/bin/env python3
"""Run release-build with fake artifacts and extract stage's shell checks without signing."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ReleaseProvenance(unittest.TestCase):
    def setUp(self):
        self.home = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.repo = self.home / "repo"
        self.tools = self.home / "tools"
        self.stage = self.home / "stage"
        for directory in [self.repo / "scripts", self.repo / "target/release", self.tools]:
            directory.mkdir(parents=True)
        self.put(self.repo / ".gitignore", "/target/\n")
        self.put(self.repo / "scripts/release-build.sh", (ROOT / "scripts/release-build.sh").read_text())
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("add", ".")
        self.git("commit", "-qm", "baseline")
        self.rev = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True, text=True)

    def shell(self, script, **env):
        return subprocess.run(
            ["bash", "-euo", "pipefail", "-c", script], cwd=self.repo, capture_output=True, text=True,
            env={**os.environ, "PATH": f"{self.tools}:{os.environ['PATH']}", **env},
        )

    def put(self, path, body):
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)

    def test_release_rejects_failed_or_misidentified_probes(self):
        self.put(self.tools / "cargo", '''#!/usr/bin/env bash
for bin in ck-fusiform ck-models; do
  printf '#!/bin/sh\n%s\n' "$PROBE" > "target/release/$bin"
  chmod +x "target/release/$bin"
done
''')
        cases = [
            ("crash", "exit 17", "version probe failed"),
            ("unknown", "echo 'test (unknown)'", "does not report revision"),
            ("wrong", f"echo 'test ({'a' * 40})'", "does not report revision"),
        ]
        for name, probe, expected in cases:
            with self.subTest(name=name):
                result = self.shell("bash scripts/release-build.sh", PROBE=probe)
                self.assertNotEqual(result.returncode, 0, f"release accepted {name}: {result.stdout}")
                self.assertIn(expected, result.stderr)
        result = self.shell("bash scripts/release-build.sh", PROBE=f"echo 'test ({self.rev})'")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"test ({self.rev})", result.stdout)
        self.assertIn("sha256:", result.stdout)

    def test_stage_refuses_and_cleans_failed_signing_or_probe(self):
        self.put(self.repo / "scripts/release-build.sh", "#!/bin/sh\nexit 0\n")
        self.git("add", "scripts/release-build.sh")
        self.git("commit", "-qm", "stub build")
        self.rev = self.git("rev-parse", "HEAD").stdout.strip()
        self.put(self.tools / "codesign", '''#!/bin/sh
[ "$SIGN_FAIL" != "$1" ] || { echo "signature check failed" >&2; exit 17; }
[ "$1" != "-dv" ] || echo "(runtime)"
''')
        for name, fail, probe in [
            ("sign", "--force", "exit 0"), ("verify", "--verify", "exit 0"),
            ("inspect", "-dv", "exit 0"), ("launch", "", "exit 18"),
            ("unstamped", "", "echo 'test (unknown)'"),
        ]:
            with self.subTest(name=name):
                for bin in ["ck-fusiform", "ck-models"]:
                    self.put(self.repo / "target/release" / bin, "#!/bin/sh\n" + probe + "\n")
                result = self.stage_checks(SIGN_FAIL=fail)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("REFUSING TO STAGE", result.stderr)
                self.assertFalse(self.stage.exists(), f"{name} left an incomplete stage")

    def test_stage_refuses_a_changed_build_tree(self):
        for mutation in ["git commit --allow-empty -qm moved", "printf x > untracked-change"]:
            with self.subTest(mutation=mutation):
                self.put(self.repo / "scripts/release-build.sh", "#!/bin/sh\n" + mutation + "\n")
                self.git("add", "scripts/release-build.sh")
                self.git("commit", "-qm", "stub build")
                self.rev = self.git("rev-parse", "HEAD").stdout.strip()
                result = self.stage_checks(SIGN_FAIL="")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("changed during the build", result.stderr)
                self.assertFalse(self.stage.exists())
                (self.repo / "untracked-change").unlink(missing_ok=True)

    def test_stage_declaration_uses_the_verified_revision(self):
        self.git("commit", "--allow-empty", "-qm", "moved after validation")
        src = (ROOT / "scripts/stage.sh").read_text(encoding="utf-8")
        declaration = 'declaration="' + src.split('declaration="', 1)[1].split("\nfor name", 1)[0]
        result = self.shell(declaration + '\nprintf "%s\\n" "$declaration"', STAGE=str(self.stage), HEAD_REV=self.rev)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"revision={self.rev}\n", result.stdout)

    def stage_checks(self, **env):
        src = (ROOT / "scripts/stage.sh").read_text(encoding="utf-8")
        checks = src.split("# Unfiltered and unpiped:", 1)[1].split('echo "rev ', 1)[0]
        return self.shell(
            'ckdev_run() { "$@"; }\n# Unfiltered and unpiped:' + checks,
            STAGE=str(self.stage), HEAD_REV=self.rev, IDENTITY="test", **env,
        )


if __name__ == "__main__":
    unittest.main()
