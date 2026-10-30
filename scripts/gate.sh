#!/usr/bin/env bash
#
# Run exactly what CI runs (.github/workflows/ci.yml), so CI confirms a push
# instead of discovering what is wrong with it. Run it before every push; a
# worker delivery cites its final line.
#
# Phase 1 runs on this machine in seconds and compiles nothing: lock parity,
# formatting, lock resolution, the path-dependency check, the wire-crate
# version check and the importer's Python tests.
# Phase 2 runs the compile-heavy CI steps (clippy and the full test suite). On
# a Mac it sends them as one job to the remote Linux build server (`ck motor
# run`), never compiling here. Anywhere else it runs them in place, which is
# how a worker gates: it runs this whole script as one remote Linux job.
#
# Every step's exit code is captured on its own and printed, so one step's
# failure can never hide behind another's success.
#
# Usage: scripts/gate.sh [--fast] [--base REF]
#   --fast      phase 1 only
#   --base REF  the ref the wire-crate version check compares against
#               (default origin/master, as CI uses). A checkout without that
#               ref, such as a remote job's snapshot, passes the commit its
#               work started from.
set -uo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

fast_only=0
base=origin/master
while [ $# -gt 0 ]; do
    case "$1" in
        --fast) fast_only=1 ;;
        --base)
            [ $# -ge 2 ] || { echo "usage: scripts/gate.sh [--fast] [--base REF]" >&2; exit 2; }
            base="$2"
            shift
            ;;
        *)
            echo "usage: scripts/gate.sh [--fast] [--base REF]" >&2
            exit 2
            ;;
    esac
    shift
done

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
    # The CI step compares against origin/master, so that is the default.
    # Refresh it first when the network allows.
    if [ "$base" = origin/master ]; then
        git fetch -q origin master 2> /dev/null || true
    fi
    # A missing base is a failure, never a skip: skipping would let a wire
    # change through without its version bump.
    if ! git rev-parse --verify --quiet "$base^{commit}" > /dev/null; then
        echo "base ref '$base' is not in this checkout; pass --base <commit>"
        return 1
    fi
    ./scripts/check-wire-crate-version.sh "$base"
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
    # Prints each exit code and fails if either failed.
    compile_steps='
        cargo clippy --locked --workspace --all-targets -- -D warnings; c=$?
        cargo test --locked --workspace --all-targets; t=$?
        echo "clippy=$c test=$t"
        [ "$c" -eq 0 ] && [ "$t" -eq 0 ]'
    if [ "$(uname -s)" = Darwin ]; then
        echo "phase 2 (remote Linux build server)"
        # One job, so both steps share one snapshot and one warm build folder.
        # 8 CPUs: a full workspace build and test keeps that many busy, and a
        # job holds every CPU it requests for its whole run.
        step clippy-and-tests ck motor run --vcpus 8 --timeout 3600 -- "$compile_steps"
    else
        echo "phase 2 (in place)"
        step clippy-and-tests bash -c "$compile_steps"
    fi
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
