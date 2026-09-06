#!/usr/bin/env bash
# Build, sign, and stage fusiform's binaries for placement.
#
# PLATFORM: macOS ONLY, and it says so at line 1 rather than failing at line 42.
#
# This script signs with an Apple Developer identity via `codesign`, which
# exists nowhere else. External report (issue #1) suggested a
# `shasum`/`sha256sum` fallback for Linux — a real portability gap, and fixing
# it alone would have moved the failure NINETEEN LINES EARLIER to `codesign`
# rather than removing it. A script that gets further before failing is worse
# than one that refuses immediately: it has already built, and the operator now
# has a half-staged directory and a less obvious reason.
#
# So the refusal is explicit and first. Signing is macOS-bound by the fleet's
# signing topology, not by an accident of tooling, so there is no portable
# version of this script to write.
#
# WHY THIS EXISTS: A REFUSAL NOBODY SEES IS NOT A GATE
#
# The release script refuses to stamp a dirty tree, which is correct. But that
# refusal was lost twice over on 2026-08-13: the message went into a `tail`
# filter, and the pipeline's exit code came from `tail` rather than the script,
# so a STALE binary from a previous build was signed and staged. It was caught
# only because `--version` on the artifact contradicted HEAD, by hand, before
# provenance was published.
#
# The remedy is not "read the output more carefully". That is a promise about
# future attention, and it expires. This script makes a stale artifact
# structurally unstageable: the binary must SELF-REPORT the rev this tree is
# on, checked after signing, on the exact bytes being published. A build that
# did not happen cannot pass, however its output was displayed.
#
# The check is on the artifact rather than on the build, deliberately. A build
# step can be skipped, filtered, or silently reuse a previous target directory;
# the bytes in the staging directory are the thing that gets placed, and asking
# them what they are is the only question whose answer cannot be stale.
set -euo pipefail

# Refuse before building, not after. `codesign` is the binding constraint.
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "stage.sh signs with an Apple Developer identity and runs on macOS only." >&2
  echo "  this host: $(uname -s)" >&2
  echo "  for a dev binary on another platform, use: cargo build --release" >&2
  echo "  (unsigned, unstageable, and fine for local testing)" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

HEAD_REV="$(git rev-parse HEAD)"
STAGE="$HOME/ck-stage/fusiform-$(date -u +%Y%m%dT%H%M%SZ)"
IDENTITY="Apple Development: ISMET UFUK ALTINOK (7UX762GU88)"

# Unfiltered and unpiped: this script's own refusals must reach the terminal
# for the same reason the release script's must.
./scripts/release-build.sh

mkdir -p "$STAGE"
cp target/release/ck-fusiform target/release/ck-models "$STAGE/"

for bin in ck-fusiform ck-models; do
  # Sign under signing-topology v2 with a pinned identifier. Never `--sign -`:
  # an ad-hoc identifier is derived from LC_UUID, which changes on every
  # rebuild and orphans the TCC grants attached to the previous one.
  codesign --force --sign "$IDENTITY" --identifier "$bin" "$STAGE/$bin"
  codesign --verify --strict "$STAGE/$bin"

  # THE GATE. Ask the signed bytes what they are.
  reported="$("$STAGE/$bin" --version | grep -oE '\([0-9a-f]{40}\)' | tr -d '()')"
  if [ "$reported" != "$HEAD_REV" ]; then
    echo >&2
    echo "REFUSING TO STAGE: $bin reports rev $reported, tree is on $HEAD_REV" >&2
    echo "The staged bytes are not this commit. A previous build was reused, or" >&2
    echo "the build did not run. Nothing has been published." >&2
    rm -rf "$STAGE"
    exit 1
  fi
done

echo
echo "rev       $HEAD_REV (self-reported by both binaries)"
echo
# A sidecar per binary, not just a printed digest.
#
# This script used to print the hashes and stop, which reads as sufficient and
# is not: a digest in a terminal is gone when the window is, so the placing seat
# has nothing to verify the bytes against at the moment they copy them. SUBC
# refuses bare-binary staging for exactly that reason and refused this script's
# output once.
#
# Written in `shasum -c` form and hashed FROM THE BYTES AT REST here, so the
# check the placer runs is against the file they are about to move rather than
# against a number this script remembered.
for bin in ck-fusiform ck-models; do
    (cd "$STAGE" && shasum -a 256 "$bin" > "$bin.sha256")
    # Verified here rather than assumed: a sidecar that does not check is worse
    # than none, because it is CITED as evidence.
    (cd "$STAGE" && shasum -c "$bin.sha256" > /dev/null) || {
        echo "the $bin sidecar does not check against the staged bytes" >&2
        exit 1
    }
done

echo "sha256:"
shasum -a 256 "$STAGE"/ck-fusiform "$STAGE"/ck-models | sed "s|$STAGE/||"
echo
echo "LC_UUID:"
for bin in ck-fusiform ck-models; do
  printf '%-14s %s\n' "$bin" "$(dwarfdump --uuid "$STAGE/$bin" | awk '{print $2}')"
done

# THE DIRECTORY PRINTS LAST, and that ordering is the fix for a real error.
#
# It used to print first. A handoff was published naming
# fusiform-20260814T103634Z when the artifact was at 103529Z: the stage output
# had been read through `tail -9`, which cut the locator line while keeping the
# hashes, and the path was then typed from the clock rather than copied. SUBC
# placed from the sha-verified directory instead of refusing, which is the
# resolution-identity rule working — the locator missed and the identity held.
#
# Printing it last means a truncated read loses the HASHES, which are the half
# nobody can reconstruct from memory. A missing locator is recoverable; a
# fabricated one that happens to resolve is not.
echo
echo "staged at $STAGE"
