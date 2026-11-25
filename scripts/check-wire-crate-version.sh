#!/usr/bin/env bash
# Refuse a change to fusiform's served schema that does not move its version.
#
# `fusiform-protocol` is what a consumer compiles against. The hazard is the
# same one subconscious found on its own wire crates: a consumer that
# path-depends on a sibling records the dependency in Cargo.lock as a bare
# version string with NO source and NO checksum, so `cargo build --locked` over
# there cannot see that the code moved. Verified in this repo's own lock file —
# subc-protocol has a version and a dependency list and neither field.
#
# Two cases:
#   version moved     -> the consumer's --locked build fails until they take it
#   version unchanged -> the new code is silently compiled in, lock unchanged
#
# The second is the common one and has no signal anywhere.
#
# BROCA's correction is why this matters more than it looks: the fleet's actual
# practice is path deps, not published ones. Four of their sibling dependencies
# are paths, including one to commons, and they keep a prose file recording
# which sibling commit each release built against — because Cargo.lock cannot
# pin a path dep. So "semver publication prevents lockstep" is true only for
# consumers who take the published version, which is not this fleet's default.
# A version bump is the only signal that reaches a path-dep consumer at all.
#
# DOC-ONLY CHANGES ARE EXEMPT DELIBERATELY, following subconscious's check: a
# rule that fires on prose gets ignored on substance. This asks whether the
# OUTPUT can move, not whether a file did.
set -uo pipefail

BASE="${1:-}"
if [ -z "$BASE" ]; then
  echo "usage: $0 <base-ref>   (e.g. origin/master, HEAD~1)" >&2
  exit 2
fi

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "  base ref '$BASE' does not resolve — cannot compare, refusing rather than passing" >&2
  exit 2
fi

CRATE=fusiform-protocol
src="crates/$CRATE/src"
manifest="crates/$CRATE/Cargo.toml"

# A run that examined nothing is not a pass. If the crate is renamed or moved,
# this would otherwise report clean over an empty set — which is the failure the
# whole file exists to prevent.
if [ ! -d "$src" ]; then
  echo "  $src does not exist — the crate moved or was renamed, refusing" >&2
  exit 2
fi

# UNCOMMITTED WORK IS INVISIBLE TO A COMMIT-RANGE DIFF, and silence about that
# is the trap. In CI the head is committed so the range is complete; run locally
# before committing, this would compare an unchanged range and report clean over
# a working tree full of schema edits. Measured on this repository: the served
# schema had gained three types and the script said "unchanged".
#
# Reported rather than folded into the comparison, because a dirty tree is not
# itself a violation — the operator just needs to know the answer does not
# cover it yet.
# `git diff HEAD` RATHER THAN `git diff`, and the difference is the whole
# warning.
#
# `git diff` compares the working tree to the INDEX, so a STAGED change is
# identical on both sides and reports nothing. This warning therefore went
# silent the moment someone ran `git add` — which is the natural order before a
# commit, and so precisely the moment a human runs this check by hand.
#
# Measured, with a wire change planted in the working tree:
#
#     git diff       -- lib.rs   sees NOTHING when staged
#     git diff HEAD  -- lib.rs   sees it
#
# The failure was quiet in the worst direction: the check printed
# "fusiform-protocol unchanged against origin/master" with no note, which reads
# as a clean pass rather than as an answer that does not cover your edit.
#
# Irrelevant in CI, where the checkout is clean — which is exactly why it
# survived. A hand-run path with no CI coverage is where a gate rots unseen.
if ! git diff HEAD --quiet -- "$src" "$manifest" 2>/dev/null; then
  echo "  note: $CRATE has UNCOMMITTED changes (staged or not), which this" >&2
  echo "        check does not see." >&2
  echo "        It compares $BASE...HEAD. Commit first for a complete answer." >&2
fi

diff=$(git diff --no-ext-diff --no-textconv "$BASE"...HEAD -- "$src") || exit 2
merge_base=$(git merge-base "$BASE" HEAD) || exit 2
before=$(git show "$merge_base:$manifest") || exit 2
after=$(git show "HEAD:$manifest") || exit 2
changed=$(printf '%s\n' "$diff" | grep -E '^[+-]' | grep -vE '^(\+\+\+ |--- )' || true)

manifest_tables() {
  awk '
    /^[[:space:]]*\[/ {
      in_table = ($0 ~ /^[[:space:]]*\[(dependencies|features)(\]|\.)/)
    }
    in_table && !/^[[:space:]]*(#|$)/ { print }
  '
}
before_tables=$(manifest_tables <<< "$before") || exit 2
after_tables=$(manifest_tables <<< "$after") || exit 2

if [ -z "$changed" ] && [ "$before_tables" = "$after_tables" ]; then
  echo "  $CRATE unchanged against $BASE"
  exit 0
fi

# A leading `*` could be multiplication or dereferencing, and a block
# comment can end on the same line as code. Ignore only line comments.
substantive=$(printf '%s\n' "$changed" \
  | sed -E 's/^[+-][[:space:]]*//' \
  | grep -vE '^//' \
  | grep -vE '^[[:space:]]*$' || true)

if [ -z "$substantive" ] && [ "$before_tables" = "$after_tables" ]; then
  echo "  $CRATE changed in comments only against $BASE"
  exit 0
fi

package_version() {
  awk '
    /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\]/) }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
      split($0, fields, "\""); print fields[2]; exit
    }
  '
}

# Protocol releases use plain numeric versions. Compare each component as
# decimal, so 0.10.0 sorts after 0.9.0, independent of the host sort command.
version_increased() {
  local pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
  local -a a b
  local i x y LC_ALL=C
  [[ "$1" =~ $pattern ]] || return 1
  a=("${BASH_REMATCH[@]}")
  [[ "$2" =~ $pattern ]] || return 1
  b=("${BASH_REMATCH[@]}")
  for i in 1 2 3; do
    x=${a[$i]} y=${b[$i]}
    if [ "${#x}" -ne "${#y}" ]; then
      [ "${#x}" -gt "${#y}" ] && return 0
      return 1
    fi
    if [ "$x" != "$y" ]; then
      [[ "$x" > "$y" ]] && return 0
      return 1
    fi
  done
  return 1
}

old=$(package_version <<< "$before") || exit 2
new=$(package_version <<< "$after") || exit 2
if version_increased "$new" "$old"; then
  echo "  $CRATE changed and version moved to $new"
  exit 0
fi

echo "  $CRATE: served schema changed, version did not increase ($old -> $new)" >&2
echo "      A consumer path-depending on this cannot see the change: Cargo.lock" >&2
echo "      records a path dep with no source and no checksum, so --locked" >&2
echo "      passes over moved code." >&2
echo "      Bump $manifest, or confirm the change is doc-only." >&2
exit 1
