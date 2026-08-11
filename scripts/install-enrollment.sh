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
# Refuses in one other case: when there is no store to protect. Enrolling a
# module that is not deployed here puts an entry in a live fleet walk pointing
# at a database that does not exist, which is a change to someone else's system
# with no benefit to this one.
#
# --force overrides both refusals.
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

# Refuse to enroll a module that is not deployed here.
#
# Engram walks the data home and captures what each descriptor declares. A
# descriptor naming a store.db that does not exist adds an entry to a live
# fleet walk pointing at nothing — and engram's whole-db capture opens the
# source read-only with no prior existence check, so the outcome depends on
# code paths that cannot be read from this repository.
#
# The point of installing is to protect a store. If there is no store, there is
# nothing to protect and the only effect is on someone else's system. Install
# alongside the deployment, not before it.
if [ ! -f "$target_dir/store.db" ] && [ "$force" != "--force" ]; then
    echo "error: no store at $target_dir/store.db" >&2
    echo >&2
    echo "Fusiform does not appear to be deployed on this machine. Enrolling now" >&2
    echo "would add an entry to engram's fleet walk pointing at a database that" >&2
    echo "does not exist. Install this after the module is running, or pass" >&2
    echo "--force if you know the store is about to appear." >&2
    exit 1
fi

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
