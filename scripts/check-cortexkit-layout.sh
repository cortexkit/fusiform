#!/usr/bin/env bash
# Keep private CortexKit files out of the index even when ignore rules fail.
set -euo pipefail

check_paths() {
    local path refused=0
    while IFS= read -r -d '' path; do
        case "$path" in
            .cortexkit/release-notes/*|.cortexkit/skills/*|.cortexkit/rules/*|.cortexkit/WORKER.md) continue ;;
            # Bash globs match slashes, so reject nesting before matching .jsonc.
            .cortexkit/*/*) ;;
            .cortexkit/?*.jsonc) continue ;;
        esac
        printf 'cortexkit-layout: disallowed tracked path: %q\n' "$path" >&2
        refused=1
    done
    return "$refused"
}

check() {
    {
        git -C "$1" ls-files -z -- .cortexkit || {
            echo 'cortexkit-layout: git ls-files failed' >&2
            return 2
        }
    } | check_paths
}

if [[ ${1:-} == --self-test ]]; then
    tmp=$(mktemp -d)
    # Bash 3.2 can enter the trap with status 0 after an unbound variable.
    finished=0
    on_exit() {
        rc=$?
        rm -rf "$tmp" || true
        if [[ $rc -eq 0 && $finished -ne 1 ]]; then
            echo "self-test FAILED: stopped before its last step" >&2
            rc=1
        fi
        exit "$rc"
    }
    trap on_exit EXIT

    repo=$tmp/repo
    git init -q "$repo"
    mkdir -p "$tmp/outside" "$repo/.cortexkit/"{release-notes,skills,rules,internal-docs,sub}
    touch "$repo/.cortexkit/"{release-notes/one.md,skills/two.md,rules/three.md,WORKER.md,project.jsonc}
    git -C "$repo" add -- .cortexkit
    check "$repo"

    ignored=.cortexkit/internal-docs/x.md
    nested=.cortexkit/sub/a.jsonc
    printf '/.cortexkit/internal-docs/\n' > "$repo/.gitignore"
    touch "$repo/$ignored" "$repo/$nested"
    git -C "$repo" add -f -- "$ignored"
    git -C "$repo" add -- "$nested"
    if output=$(check "$repo" 2>&1); then
        echo "self-test FAILED: disallowed tracked paths were accepted" >&2
        exit 1
    fi
    if [[ $output != *"$ignored"* || $output != *"$nested"* ]]; then
        printf 'self-test FAILED: refusal did not name both paths:\n%s\n' "$output" >&2
        exit 1
    fi

    # Prevent Git from discovering an enclosing repository if TMPDIR is in one.
    if output=$(GIT_CEILING_DIRECTORIES="$tmp" check "$tmp/outside" 2>&1); then
        echo "self-test FAILED: failed git ls-files was accepted" >&2
        exit 1
    fi
    if [[ $output != *"git ls-files failed"* ]]; then
        printf 'self-test FAILED: missing git ls-files diagnostic:\n%s\n' "$output" >&2
        exit 1
    fi
    echo "cortexkit-layout self-test: accepts allowed paths, names disallowed paths, refuses a failed listing"
    finished=1
    exit 0
fi

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
check "$repo"
echo 'cortexkit-layout: all tracked .cortexkit paths are allowed'
