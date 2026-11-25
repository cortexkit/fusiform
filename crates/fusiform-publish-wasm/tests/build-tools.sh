#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."

# Restrict PATH so the host's optimizer cannot hide a missing or wrong one.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ln -s "$(command -v dirname)" "$work/dirname"
write_tool() {
    printf '#!%s\n%s\n' "$BASH" "$2" > "$work/$1"
    chmod +x "$work/$1"
}
write_tool rustc 'echo "rustc 1.100.0 (test)"'
write_tool wasm-bindgen 'echo "wasm-bindgen 0.2.127"'
write_tool git 'echo test-commit'
write_tool cargo 'echo "cargo build reached"; exit 42'

refuses_before_build() {
    local output status=0
    output=$(PATH="$work" "$BASH" scripts/build-publish-wasm.sh 2>&1) || status=$?
    if [[ "$status" != 1 || "$output" != *"$1"* || "$output" == *"cargo build reached"* ]]; then
        printf 'Expected refusal %s before cargo build, got exit %s: %s\n' "$1" "$status" "$output" >&2
        exit 1
    fi
}

refuses_before_build 'Expected wasm-opt version 120, but it is not on PATH'
write_tool wasm-opt 'echo "wasm-opt version 1200"'
refuses_before_build 'Expected wasm-opt version 120, found wasm-opt version 1200'
write_tool wasm-opt 'echo "wasm-opt version 120 (version_120)"'
status=0
output=$(PATH="$work" "$BASH" scripts/build-publish-wasm.sh 2>&1) || status=$?
if [[ "$status" != 42 || "$output" != *"cargo build reached"* ]]; then
    printf 'Expected pinned tools to reach cargo build, got exit %s: %s\n' "$status" "$output" >&2
    exit 1
fi
printf 'Passed: missing and wrong wasm-opt refused, pinned version reached cargo build\n'
