#!/usr/bin/env bash
# Build, sign, and stage fusiform's binaries for placement.
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
echo "staged at $STAGE"
echo "rev       $HEAD_REV (self-reported by both binaries)"
echo
echo "sha256:"
shasum -a 256 "$STAGE"/* | sed "s|$STAGE/||"
echo
echo "LC_UUID:"
for bin in ck-fusiform ck-models; do
  printf '%-14s %s\n' "$bin" "$(dwarfdump --uuid "$STAGE/$bin" | awk '{print $2}')"
done
