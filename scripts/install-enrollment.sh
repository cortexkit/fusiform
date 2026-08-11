#!/usr/bin/env bash
#
# Install fusiform's engram enrollment into its data directory.
#
# Engram discovers enrollments by walking `<data_home>/cortexkit/*/` and reading
# `engram-catalog.json` from each module directory. A module without one is
# reported as NotEnrolled and is not captured — so this file being in the
# repository does nothing until it is copied into place.
#
# Idempotent, and refuses rather than overwrites when the installed copy differs
# from the repository's: a divergence means someone edited the live file by hand,
# and silently replacing it would destroy the only record of what they changed.
#
# Usage: scripts/install-enrollment.sh [--force]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_file="$repo_root/crates/fusiform-module/data/engram-catalog.json"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
target_dir="$data_home/cortexkit/fusiform"
target_file="$target_dir/engram-catalog.json"
force="${1:-}"

[ -f "$source_file" ] || { echo "error: $source_file is missing" >&2; exit 1; }

# Validate before installing. An unparseable descriptor is worse than none:
# engram reports it as Invalid and refuses the whole fleet capture, so a typo
# here takes down backups for every module rather than just this one.
python3 -c "
import json, sys
d = json.load(open('$source_file'))
assert d['schema_version'] == 1, 'unsupported schema version'
assert d['module_id'] == 'fusiform', f\"module_id must match the directory name, got {d['module_id']!r}\"
assert d['entries'], 'an enrollment with no entries captures nothing'
" || { echo "error: descriptor did not validate" >&2; exit 1; }

if [ -f "$target_file" ] && [ "$force" != "--force" ]; then
    if cmp -s "$source_file" "$target_file"; then
        echo "already installed and identical: $target_file"
        exit 0
    fi
    echo "error: installed descriptor differs from the repository's" >&2
    echo "  installed: $target_file" >&2
    echo "  repository: $source_file" >&2
    diff "$target_file" "$source_file" >&2 || true
    echo >&2
    echo "Someone edited the live file. Reconcile it deliberately, then re-run" >&2
    echo "with --force." >&2
    exit 1
fi

mkdir -p "$target_dir"
cp "$source_file" "$target_file"
echo "installed: $target_file"
echo
echo "Engram picks this up on its next fleet walk. Until then fusiform reports"
echo "as NotEnrolled and its store is not captured."
