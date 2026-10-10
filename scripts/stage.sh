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

# The signing identity comes from the environment rather than this file, so the
# script carries no one's certificate name. Checked here, before building, for
# the same reason as the platform check above: a build that runs to completion
# and only then finds it cannot sign leaves a half-staged directory behind.
if [[ -z "${FUSIFORM_SIGNING_IDENTITY:-}" ]]; then
  echo "stage.sh needs FUSIFORM_SIGNING_IDENTITY: the codesign identity to sign" >&2
  echo "  the staged binaries with, as listed by 'security find-identity -v -p codesigning'." >&2
  echo "  example: FUSIFORM_SIGNING_IDENTITY=\"Apple Development: <Name> (<TEAMID>)\"" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

HEAD_REV="$(git rev-parse HEAD)"
STAGE="$HOME/ck-stage/fusiform-$(date -u +%Y%m%dT%H%M%SZ)"
IDENTITY="$FUSIFORM_SIGNING_IDENTITY"

# Run a staged binary under a `ckdev-` name.
#
# On this machine a running `ck-` process means a production binary placed in
# the fleet's bin directory (or its staging directory, which ~/ck-stage is
# not), so the live fleet can be told apart from probes in a process list.
# Running a staged `ck-fusiform` directly would show a second one beside the
# live module. A hard link shares the inode, so the probe runs exactly the
# signed bytes it is checking; a copy is the fallback across volumes.
CKDEV_DIR="$(mktemp -d)"
trap 'rm -rf "$CKDEV_DIR"' EXIT
ckdev_run() {
  local built="$1"; shift
  local link="$CKDEV_DIR/ckdev-$(basename "$built" | sed 's/^ck-//')"
  rm -f "$link"
  ln "$built" "$link" 2>/dev/null || cp "$built" "$link"
  "$link" "$@"
}

# Unfiltered and unpiped: this script's own refusals must reach the terminal
# for the same reason the release script's must.
./scripts/release-build.sh

mkdir -p "$STAGE"
cp target/release/ck-fusiform target/release/ck-models "$STAGE/"

for bin in ck-fusiform ck-models; do
  # Sign under signing-topology v2 with a pinned identifier. Never `--sign -`:
  # an ad-hoc identifier is derived from LC_UUID, which changes on every
  # rebuild and orphans the TCC grants attached to the previous one.
  #
  # `-o runtime` enables the hardened runtime, which the daemon's placement
  # gate requires before it stops handing modules their launch nonce through
  # the environment. It leaves the designated requirement unchanged, so the
  # macOS grants tied to that requirement still apply.
  codesign --force --sign "$IDENTITY" --identifier "$bin" -o runtime "$STAGE/$bin"
  codesign --verify --strict "$STAGE/$bin"
  # Captured first, not piped into `grep -q`: under pipefail, `grep -q` exits
  # on its first match, codesign then dies of SIGPIPE, and the pipeline fails
  # exactly when the flag IS present.
  signature="$(codesign -dv "$STAGE/$bin" 2>&1)"
  if [[ "$signature" != *"(runtime)"* ]]; then
    echo "stage.sh: $bin was signed without the hardened runtime" >&2
    rm -rf "$STAGE"
    exit 1
  fi

  # THE GATE. Ask the signed bytes what they are.
  reported="$(ckdev_run "$STAGE/$bin" --version | grep -oE '\([0-9a-f]{40}\)' | tr -d '()')"
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
    # WHAT THIS CAN AND CANNOT CATCH, because the honest bound is narrower than
    # it looks and the loose version of this comment was already here.
    #
    # The hash is computed from the bytes at rest and then checked against those
    # same bytes, so it CANNOT detect a corrupt or truncated binary: a truncated
    # write is hashed as truncated and matches itself. Reading it as proof the
    # staged bytes are right would be a self-confirming claim.
    #
    # It catches the sidecar being unusable: an empty file, a redirect that
    # failed, a name that does not resolve at the placer's `shasum -c`. Measured
    # both: empty refuses, wrong filename refuses.
    #
    # That narrow thing is worth checking because the sidecar is CITED as
    # evidence by the placing seat, and one that silently checks nothing is
    # worse than none at all. The real verification of the bytes happens on
    # their side, after the copy, which is the only place it means anything.
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

# Prune superseded stages, keeping this one and the DEPLOYED revision.
#
# Thirty of these accumulated over a month, 429 MB, every one a signed and
# runnable ck-fusiform of a revision nobody wants any more. That is the same
# hazard I named to SUBC when I deleted a single superseded directory by hand
# -- "a stale stage waiting to be placed is a trap I set for them" -- and then
# left twenty-nine others sitting beside it.
#
# The script only ever created. So the cleanup was a thing I had to REMEMBER,
# which means it was a thing that would eventually not happen, and the growth
# is silent: nothing about a successful stage says the directory before it is
# now a liability.
#
# WHY THE DEPLOYED REVISION IS KEPT RATHER THAN JUST THE NEWEST. It is HALF the
# rollback. Deleting it would mean a rollback needs a rebuild, and a rebuild
# during an incident is the worst time to discover the toolchain moved.
#
# HALF, and the word is load-bearing. After a placement that MIGRATES the store,
# putting this binary back is not a rollback: it meets a schema ahead of what it
# knows and refuses on `store_ahead`, which is correct fail-closed behaviour and
# also means service does not come back. The other half is a store snapshot,
# taken before the swap with SQLite's online backup — `.backup`, never `cp`,
# which would capture a torn .db beside a live -wal and restore to a state that
# never existed.
#
# Keeping the binary and calling it "the rollback target" is a claim that is
# true about the artifact and wrong about recoverability, which is the class
# this repo keeps finding: a guard reporting accurately on a subject adjacent to
# the one that matters.
# Read from the placed binary rather than assumed, so it stays right when
# placement lags several stages behind, which is the normal case here.
#
# Failure is NOT fatal: a stage that built, signed and verified is good even if
# the tidying cannot run. Refusing here would turn housekeeping into a
# deployment blocker.
# `|| true` inside the substitution, and it is load-bearing rather than
# defensive noise.
#
# `set -euo pipefail` is on. When grep matches nothing it returns 1, pipefail
# propagates that, and the ASSIGNMENT then fails and kills the script. Both of
# these read binaries that may legitimately produce no revision -- a module
# that is not placed yet, or a stage directory holding something unreadable --
# so "no match" is an ordinary outcome here, not an error.
DEPLOYED_REV="$("$HOME/.local/share/cortexkit/bin/ck-fusiform" --version 2>/dev/null \
    | grep -oE '[0-9a-f]{40}' | head -1 || true)"

pruned=0
for dir in "$HOME"/ck-stage/fusiform-*; do
    # `if` rather than `[ cond ] && continue`.
    #
    # Under `set -e` that idiom EXITS THE SCRIPT whenever the test is false,
    # because the compound returns 1 -- so the first directory that was not the
    # one just built would end the run silently, after the artifact was already
    # built and signed. It did, on the first real run.
    #
    # Found only because the run was unpiped and the exit code read: the same
    # command through `| tail -5` shows the hashes, looks complete, and reports
    # tail's zero.
    if [ ! -d "$dir" ]; then
        continue
    fi
    if [ "$dir" = "$STAGE" ]; then
        continue
    fi

    # Identify by what the binary REPORTS, never by the directory's timestamp.
    # A name is a label someone typed; the self-reported revision is the thing
    # that decides whether this directory is the rollback target.
    rev="$(ckdev_run "$dir/ck-fusiform" --version 2>/dev/null | grep -oE '[0-9a-f]{40}' | head -1 || true)"
    if [ -n "$DEPLOYED_REV" ] && [ "$rev" = "$DEPLOYED_REV" ]; then
        continue
    fi
    # An unreadable binary is pruned too: it cannot be the rollback target,
    # because nothing can establish what it is.
    if rm -rf "$dir"; then
        pruned=$((pruned + 1))
    fi
done

if [ "$pruned" -gt 0 ]; then
    echo "pruned    $pruned superseded stage(s); kept this one and the deployed revision"
fi

# Warn when the placed ck-models no longer matches what was just built.
#
# The operator reaches the CLI as `ck models`, which runs the copy in the
# CortexKit bin folder. Subcommands are not on PATH as `ck-<name>`, so this looks
# at that file directly rather than through `command -v`, which would find
# nothing and stay silent.
#
# The CLI is placed with the module, by the placing seat, from the stage this
# script writes. That step is the one that goes missing: once a prune deleted
# the stage directory the placed CLI came from, so the operator copy was pinned
# to a revision with no artifact left on disk, and nothing said so. Acceptance
# checks run through that stale CLI were reported as verification of the new
# revision.
#
# A WARNING RATHER THAN A PLACEMENT. Copying into the bin folder from a build
# script would make every stage a deployment, and staging exists precisely to
# separate the two.
#
# Compared by SELF-REPORTED revision rather than by file hash: the two binaries
# differ in signature and timestamp even when built from the same source, so a
# hash comparison would cry on every run.
placed_cli="$HOME/.local/share/cortexkit/bin/ck-models"
if [ -x "$placed_cli" ]; then
    placed_rev="$("$placed_cli" --version 2>/dev/null \
        | grep -oE '[0-9a-f]{40}' | head -1 || true)"
    if [ -n "$placed_rev" ] && [ "$placed_rev" != "$HEAD_REV" ]; then
        echo
        echo "NOTE: $placed_cli reports ${placed_rev:0:7}, this stage is ${HEAD_REV:0:7}"
        echo "      the operator CLI is stale until the card places ck-models with the module"
    fi
fi

# The acceptance card's shape, printed where a card is about to be written.
#
# Two cards in two days were refused or corrected by the placing seat's gate,
# for the same reason each time: a field was a true statement about the WRONG
# ARTIFACT. First a marker quoting CLI output for a module placement; then a
# control doing the same, in a card whose marker I had just fixed. Both times
# the rule existed — I had written it to that seat the night before — and both
# times it was applied to the field I was thinking about.
#
# A third card asserted output composed from memory of a contract table: it
# claimed twelve keys where the wire serves fifteen, and named the wrong first
# entry. Nothing in the card was checkable without running it, and I did not.
#
# So the template prints HERE, at the moment a card gets written, rather than
# living in a message that protected its reader once. The labels are the whole
# mechanism: "which binary" makes CLI-versus-module a field to fill rather than
# a distinction the writer has to remember.
echo
echo "acceptance card — fill from MEASUREMENT, not memory:"
echo "  MARKER    a string the PLACED binary carries (ck-fusiform), absent in the old one"
echo "  CONTROL   from the PLACED binary, same surface as the marker, and with a"
echo "            COUNT THAT DOES NOT MOVE between revisions. In strings prefer"
echo "            models-dev-usd-v1 (exactly 1, held by the test"
echo "            only_one_currency_policy_has_ever_existed) over a string whose"
echo "            mentions drift with the code: models.dev read 11 staged / 12"
echo "            live across one placement, and an unequal control reads as a"
echo "            discriminator to anyone skimming. On a served value use"
echo "            model_count, never catalog_version — max(now_ms, current+1)"
echo "            advances on every restart, so it can never hold still"
echo "  ARM       name WHICH BINARY renders it, and paste output you EXECUTED
  MIGRATES? if this revision adds a schema migration, SAY SO on the card. The
            placer's binary snapshot is only half a rollback for a migrating
            placement: the old binary meets the newer store and refuses on
            store_ahead, so service does not come back without a store restore
            too. A card that omits this reads as freely reversible"

# DECLARE which stage is current, so a placer does not have to infer it.
#
# SUBC's gate printed "INFERRED from mtime (no ck-fusiform.current)". It picked
# the right artifact, and the inference and the declaration disagree in exactly
# one case: when a card has been SUPERSEDED. That is not hypothetical here —
# stage.sh prunes, so a path I handed over an hour ago can be gone, and I have
# already had to send a replacement locator once tonight.
#
# An mtime read cannot see that. Newest-on-disk answers "which directory was
# written last", which is a true answer to a question nobody asked: the question
# is "which one did the producer publish".
#
# Written to a STABLE path so a stale card is recoverable without asking me. It
# carries the sha as well as the directory, so a placer can check that what it
# found is what was declared rather than trusting the pointer.
#
# The placer's gate looks the declaration up per binary: `fusiform.current` for
# the module and `ck-models.current` for the CLI. Both are written here from the
# one stage, with identical contents, so they cannot name different builds.
declaration="stage=$STAGE
revision=$(git rev-parse HEAD)
declared_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
for name in fusiform ck-models; do
    current="$HOME/ck-stage/$name.current"
    printf '%s\n' "$declaration" > "$current.tmp"
    mv -f "$current.tmp" "$current"
done

# Written through a temp file and renamed, because a reader can open this at any
# moment: a truncated declaration naming half a path is worse than a stale one
# naming a whole path that no longer exists. `mv` within one filesystem is
# atomic; a direct redirect is not.

# Printed LAST, and the prune block above deliberately sits before it.
#
# The reasoning is three paragraphs up: a truncated read must lose the hashes
# rather than the locator, because a missing locator is recoverable and a
# fabricated one that happens to resolve is not. Housekeeping output that
# pushed the path off the bottom would undo exactly that.
echo
echo "staged at $STAGE"
echo "declared in $current"
