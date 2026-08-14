#!/usr/bin/env bash
# Build fusiform's binaries with their provenance stamped in.
#
# WHY THIS EXISTS RATHER THAN `cargo build --release`
#
# A binary must be able to say which commit it was built from. Nothing else
# can: CARGO_PKG_VERSION has not moved in this project's lifetime, and LC_UUID
# is path-dependent, so it proves two FILES match without naming a commit.
#
# That gap was not hypothetical here. Fusiform ran for hours on a binary nine
# code commits behind the tree, and what exposed it was noticing that health
# metrics lacked counters added since — no identity probe could have answered
# it.
#
# An ordinary `cargo build` leaves CK_BUILD_REV unset and the binary reports
# "unknown", which is the honest answer for a dev build. Stamping a
# possibly-dirty tree's HEAD would assert a provenance the bytes do not have.
#
# Convention adopted from CKCRED via SUBC.
set -euo pipefail

cd "$(dirname "$0")/.."

REV="$(git rev-parse HEAD)"

# A dirty tree cannot be identified by a commit, so it is not stamped with one.
# This is the same rule as the unset case: an unstamped binary is honest about
# being unidentifiable, and a wrongly-stamped one sends an incident responder to
# read code that was never built.
if [ -n "$(git status --porcelain)" ]; then
  echo "refusing to stamp a dirty tree: the built bytes would not match $REV" >&2
  echo "commit or stash first, or use a plain cargo build for a dev binary" >&2
  exit 1
fi

echo "building at $REV"
CK_BUILD_REV="$REV" cargo build --locked --release \
  -p fusiform-module -p fusiform-cli

echo
for bin in ck-fusiform ck-models; do
  printf '%-12s %s\n' "$bin" "$(./target/release/$bin --version)"
done

echo
echo "sha256:"
# `shasum` is macOS; `sha256sum` is coreutils. This script is otherwise
# portable — it only builds and stamps, with no signing — so an external
# report (issue #1) correctly noted that this one call was all that stopped it
# running on Linux. `stage.sh` is a different case and refuses outright: it
# signs, and `codesign` has no counterpart to fall back to.
if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 target/release/ck-fusiform target/release/ck-models \
    | sed 's|target/release/||'
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum target/release/ck-fusiform target/release/ck-models \
    | sed 's|target/release/||'
else
  # Refuse rather than print nothing: a provenance block that silently omits
  # the hashes is worse than a failure, because the omission is invisible in
  # the output someone copies.
  echo "no sha256 tool found (looked for shasum, sha256sum)" >&2
  exit 1
fi
