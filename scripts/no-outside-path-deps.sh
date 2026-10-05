#!/bin/sh
# Refuse any dependency that cargo resolves from a local path outside this
# repository, including `[patch]` entries.
#
# WHY: a path dependency on a sibling checkout (`../subconscious/...`) builds
# against whatever that working tree holds, and its version is recorded in
# Cargo.lock exactly. Every sibling release then leaves this repo's lock stale,
# and a `--locked` gate fails with nothing in this repo having changed. Fusiform
# moved every dependency to crates.io in September; this check keeps it there.
#
# HOW: `cargo metadata` lists every resolved package. One with no `source` was
# taken from a local path, and its `manifest_path` says where. Anything outside
# the workspace root fails. Reading cargo's own resolution, rather than grepping
# Cargo.toml for `path =`, covers a `[patch]` that is applied, workspace
# inheritance and any spelling a text search would miss. A `[patch]` cargo
# ignores is never resolved, so it is not reported; it also builds nothing.
#
# Usage: scripts/no-outside-path-deps.sh [--manifest-path <Cargo.toml>]
# With no argument it checks this repository. The argument exists so the
# self-test below can point it at a planted violation.
#
#   scripts/no-outside-path-deps.sh --self-test
#
# builds a throwaway workspace with a path dependency on a crate beside it and
# requires this script to refuse it, then a clean one it must accept. Without
# that, a check that never fails is indistinguishable from one that works.
set -eu

script="$0"

check() {
    manifest=$1
    root=$(cd "$(dirname "$manifest")" && pwd)
    # The real repo resolves --locked, as CI builds it, and may fetch crates
    # on a fresh runner. The self-test's throwaway workspace has no lock and
    # no registry dependencies, so it runs offline instead.
    metadata=$(cargo metadata ${METADATA_FLAGS:---locked} --format-version 1 \
        --manifest-path "$manifest" 2>/dev/null) || {
        echo "no-outside-path-deps: cargo metadata failed for $manifest" >&2
        return 2
    }
    printf '%s' "$metadata" | ROOT="$root" python3 -c '
import json, os, sys
root = os.path.realpath(os.environ["ROOT"])
outside = []
packages = json.load(sys.stdin)["packages"]
local = [p for p in packages if p.get("source") is None]
for p in local:
    path = os.path.realpath(os.path.dirname(p["manifest_path"]))
    if os.path.commonpath([root, path]) != root:
        outside.append((p["name"], path))
# Every workspace member is itself a path package, so a resolution that found
# no local package at all means the scan read nothing, not that it is clean.
if not local:
    print("no-outside-path-deps: found no local packages; the scan read nothing",
          file=sys.stderr)
    sys.exit(2)
for name, path in outside:
    print("no-outside-path-deps: %s resolves from %s, outside %s" % (name, path, root),
          file=sys.stderr)
sys.exit(1 if outside else 0)
'
}

if [ "${1:-}" = "--self-test" ]; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/repo/src" "$tmp/outside/src"
    printf '[package]\nname = "outside"\nversion = "0.1.0"\nedition = "2021"\n' \
        > "$tmp/outside/Cargo.toml"
    : > "$tmp/outside/src/lib.rs"
    printf '[package]\nname = "planted"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\noutside = { path = "../outside" }\n' \
        > "$tmp/repo/Cargo.toml"
    : > "$tmp/repo/src/lib.rs"
    export METADATA_FLAGS=--offline
    if "$script" --manifest-path "$tmp/repo/Cargo.toml" 2> "$tmp/err"; then
        echo "self-test FAILED: a path dependency outside the repo was accepted" >&2
        exit 1
    fi
    grep -q 'outside resolves from' "$tmp/err" || {
        echo "self-test FAILED: refused, but not for the planted dependency:" >&2
        cat "$tmp/err" >&2
        exit 1
    }
    # Control: the same workspace without the outside dependency must pass, or
    # the refusal above could come from a check that refuses everything.
    printf '[package]\nname = "planted"\nversion = "0.1.0"\nedition = "2021"\n' \
        > "$tmp/repo/Cargo.toml"
    "$script" --manifest-path "$tmp/repo/Cargo.toml" || {
        echo "self-test FAILED: a clean workspace was refused" >&2
        exit 1
    }
    echo "no-outside-path-deps self-test: refuses the planted dependency, accepts the clean control"
    exit 0
fi

if [ "${1:-}" = "--manifest-path" ]; then
    check "$2"
    exit 0
fi

repo=$(git -C "$(dirname "$script")" rev-parse --show-toplevel)
check "$repo/Cargo.toml"
