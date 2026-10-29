#!/usr/bin/env bash
#
# Run exactly what CI runs (.github/workflows/ci.yml), so CI confirms a push
# instead of discovering what is wrong with it. Run it before every push; a
# worker delivery cites its final line.
#
# Phase 1 runs on this machine in seconds and compiles nothing: lock parity,
# formatting, lock resolution, the path-dependency check, the wire-crate
# version check and the importer's Python tests.
# Phase 2 runs the compile-heavy CI steps (clippy and the full test suite) as
# one job on the remote Linux build server, never on this machine.
#
# Every step's exit code is captured on its own and printed, so one step's
# failure can never hide behind another's success.
#
# Usage: scripts/gate.sh          both phases
#        scripts/gate.sh --fast   phase 1 only
set -uo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

fast_only=0
if [ "${1:-}" = "--fast" ]; then
    fast_only=1
elif [ $# -gt 0 ]; then
    echo "usage: scripts/gate.sh [--fast]" >&2
    exit 2
fi

log_dir="$(mktemp -d "${TMPDIR:-/tmp}/fusiform-gate.XXXXXX")"
failed=()

# Runs one named step, saving its output, and records it as failed when its
# exit code is non-zero.
step() {
    local name="$1"
    shift
    local log="$log_dir/$name.log"
    local started=$SECONDS
    "$@" > "$log" 2>&1
    local rc=$?
    printf '  %-26s exit=%d  %3ds\n' "$name" "$rc" "$((SECONDS - started))"
    if [ "$rc" -ne 0 ]; then
        failed+=("$name")
        sed 's/^/      /' "$log" | tail -n 25
    fi
}

lock_parity() {
    # CI resolves the commit, not this working tree.
    git show HEAD:Cargo.lock | cmp -s - Cargo.lock
}

wire_version() {
    # The CI step compares against origin/master, so compare against the
    # same ref. Refresh it first when the network allows.
    git fetch -q origin master 2> /dev/null || true
    ./scripts/check-wire-crate-version.sh origin/master
}

if ! git diff --quiet HEAD -- || [ -n "$(git ls-files --others --exclude-standard)" ]; then
    echo "note: the tree differs from HEAD, so this gate checks the tree, not the commit." >&2
fi

echo "phase 1 (local, no compile)"
step lock-parity lock_parity
step fmt cargo --locked fmt --all -- --check
step lock-resolves cargo metadata --locked --offline --format-version 1
step path-deps-self-test scripts/no-outside-path-deps.sh --self-test
step path-deps scripts/no-outside-path-deps.sh
step wire-crate-version wire_version
step importer-tests python3 scripts/test_import_quality_scores.py

if [ "$fast_only" -eq 0 ]; then
    echo "phase 2 (remote Linux build server)"
    # One job, so both steps share one snapshot and one warm build folder.
    # The remote command prints each exit code and fails if either failed.
    step clippy-and-tests ck motor run --vcpus 16 --timeout 3600 -- '
        cargo clippy --locked --workspace --all-targets -- -D warnings; c=$?
        cargo test --locked --workspace --all-targets; t=$?
        echo "clippy=$c test=$t"
        [ "$c" -eq 0 ] && [ "$t" -eq 0 ]'
    grep -E '^test result:|^clippy=' "$log_dir/clippy-and-tests.log" |
        awk '/^test result/{p+=$4; f+=$6; next} {print "      " $0} END{printf "      tests passed=%d failed=%d\n", p, f}'
fi

rev="$(git rev-parse --short=12 HEAD)"
if [ ${#failed[@]} -eq 0 ]; then
    if [ "$fast_only" -eq 1 ]; then
        echo "GATE_FAST_OK $rev"
    else
        echo "GATE_OK $rev"
    fi
    rm -rf "$log_dir"
    exit 0
fi
echo "GATE_FAILED $rev: ${failed[*]} (logs in $log_dir)"
exit 1
