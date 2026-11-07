#!/usr/bin/env python3
"""Refuse a shell script that can report success after it stopped early.

On macOS, /bin/bash (and /bin/sh, which is the same binary) is version 3.2.
There, when `set -e` and `set -u` are both on and an EXIT trap is set, a
reference to an unbound variable stops the script with exit status 0, and `$?`
inside the trap is 0 too. A gate or staging script can then report success
having run nothing. Linux CI runs bash 5, which exits non-zero, so only local
runs on a Mac are exposed and CI cannot catch it.

The fix this check enforces: a script that turns on errexit and sets an EXIT
trap must also set `finished=1` before its intended successful exit, and its
trap must turn a 0 status into a failure when `finished` is not 1. This check
looks only for `finished=1`; the trap body is read in review.

Usage: scripts/check-shell-exit-guard.py [--self-test]
"""

import os
import re
import subprocess
import sys
import tempfile

ERREXIT = re.compile(r"^\s*set\s+(-[A-Za-z]*e[A-Za-z]*\b|.*-o\s+errexit\b)")
EXIT_TRAP = re.compile(r"^\s*trap\s+.*\bEXIT\b")
# `finished=1`, or a prefixed name such as `train_push_finished=1`.
FINISHED = re.compile(r"^\s*(\w+_)?finished=1\s*$")

# Copied from another repository and replaced whole when re-lifted, so a fix
# belongs upstream. Each entry names who owns the fix.
LIFTED = {}


def code_lines(text):
    return [line for line in text.splitlines() if not line.lstrip().startswith("#")]


def unsafe(text):
    lines = code_lines(text)
    has = lambda pattern: any(pattern.search(line) for line in lines)
    return has(ERREXIT) and has(EXIT_TRAP) and not has(FINISHED)


def scripts(root):
    found = []
    for directory in ("scripts", ".githooks"):
        for base, _, names in os.walk(os.path.join(root, directory)):
            for name in names:
                path = os.path.join(base, name)
                with open(path, "rb") as handle:
                    head = handle.read(64)
                if name.endswith(".sh") or head.startswith(b"#!/bin/sh") or b"bash" in head.split(b"\n")[0]:
                    found.append(os.path.relpath(path, root))
    return sorted(found)


def check(root):
    found = scripts(root)
    # The scan must have read something, or an empty result reads as clean.
    if not found:
        print("shell-exit-guard: found no shell scripts; the scan read nothing", file=sys.stderr)
        return 2
    refused = []
    for rel in found:
        if rel in LIFTED:
            continue
        with open(os.path.join(root, rel), encoding="utf-8") as handle:
            if unsafe(handle.read()):
                refused.append(rel)
    for rel in refused:
        print(
            "shell-exit-guard: %s sets errexit and an EXIT trap but never sets finished=1;"
            " on macOS bash 3.2 an unbound variable would exit 0" % rel,
            file=sys.stderr,
        )
    if refused:
        return 1
    print("shell-exit-guard: %d script(s) read, %d lifted and skipped" % (len(found), len(LIFTED)))
    return 0


def self_test():
    unguarded = "#!/usr/bin/env bash\nset -euo pipefail\ntrap 'rm -f x' EXIT\necho done\n"
    guarded = unguarded + "finished=1\n"
    prefixed = unguarded + "train_push_finished=1\n"
    # `unfinished=1` is a different variable, not the guard.
    lookalike = unguarded + "unfinished=1\n"
    # A commented-out `finished=1` is not a guard.
    commented = unguarded + "# finished=1\n"
    no_errexit = "#!/usr/bin/env bash\nset -uo pipefail\ntrap 'rm -f x' EXIT\n"
    cases = [
        (unguarded, 1),
        (commented, 1),
        (lookalike, 1),
        (guarded, 0),
        (prefixed, 0),
        (no_errexit, 0),
    ]
    for body, want in cases:
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, "scripts"))
            with open(os.path.join(root, "scripts", "planted.sh"), "w") as handle:
                handle.write(body)
            got = subprocess.run(
                [sys.executable, __file__, "--root", root],
                capture_output=True,
                text=True,
            ).returncode
            if got != want:
                print("self-test FAILED: expected exit %d, got %d for:\n%s" % (want, got, body), file=sys.stderr)
                return 1
    print(
        "shell-exit-guard self-test: refuses an unguarded, a commented and a look-alike guard;"
        " accepts a guarded, a prefixed-guard and a no-errexit script"
    )
    return 0


if __name__ == "__main__":
    args = sys.argv[1:]
    if args == ["--self-test"]:
        sys.exit(self_test())
    if len(args) == 2 and args[0] == "--root":
        sys.exit(check(args[1]))
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    ).stdout.strip()
    sys.exit(check(root))
