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
if ! git diff --quiet -- "$src" "$manifest" 2>/dev/null; then
  echo "  note: $CRATE has UNCOMMITTED changes, which this check does not see." >&2
  echo "        It compares $BASE...HEAD. Commit first for a complete answer." >&2
fi

changed=$(git diff "$BASE"...HEAD -- "$src" \
  | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' || true)

if [ -z "$changed" ]; then
  echo "  $CRATE unchanged against $BASE"
  exit 0
fi

# Strip comment and blank lines. Doc comments, line comments and block-comment
# bodies are prose: they cannot change what a consumer compiles.
substantive=$(printf '%s\n' "$changed" \
  | sed -E 's/^[+-][[:space:]]*//' \
  | grep -vE '^(///|//!|//|/\*|\*|\*/)' \
  | grep -vE '^[[:space:]]*$' || true)

if [ -z "$substantive" ]; then
  echo "  $CRATE changed in comments only against $BASE"
  exit 0
fi

if git diff "$BASE"...HEAD -- "$manifest" | grep -qE '^\+version[[:space:]]*='; then
  new=$(grep -m1 -E '^version[[:space:]]*=' "$manifest" | sed -E 's/.*"(.*)".*/\1/')
  echo "  $CRATE changed and version moved to $new"
  exit 0
fi

cur=$(grep -m1 -E '^version[[:space:]]*=' "$manifest" | sed -E 's/.*"(.*)".*/\1/')
echo "  $CRATE: served schema changed, version still $cur" >&2
echo "      A consumer path-depending on this cannot see the change: Cargo.lock" >&2
echo "      records a path dep with no source and no checksum, so --locked" >&2
echo "      passes over moved code." >&2
echo "      Bump $manifest, or confirm the change is doc-only." >&2
exit 1
