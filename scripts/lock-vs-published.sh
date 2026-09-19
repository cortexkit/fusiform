#!/bin/sh
# Refuse a Cargo.lock that pins a sibling version which is not on that sibling's
# published master.
#
# WHY THIS EXISTS, AND WHY THE EXISTING CHECKS DO NOT COVER IT
#
# `cargo metadata --locked` asks "does this lockfile resolve against the
# siblings ON DISK". That is the wrong subject. CI resolves the eight sibling
# path dependencies against their REMOTES, so the question that decides whether
# a push goes green is "does this lockfile match what those siblings have
# PUBLISHED".
#
# Those agree almost always, and disagree exactly when a sibling has a feature
# branch checked out — at which point `cargo update` picks up a version that
# never landed, every local gate passes forever, and CI goes red against a
# version nobody shipped, with nothing in this repo wrong. That red is
# expensive precisely because it reads as inexplicable.
#
# MEASURED, TWICE, ON ONE EVENING: subc-core 0.18.9 and subc-control 0.13.1 were
# picked up while subconscious had a terminal-history branch checked out; both
# are real work, neither was on master. A second seat hit the same thing the
# same hour with two lock commits that only undid each other, and neither of us
# could see the cause from our own repository.
#
# A CLEAN MANIFEST IS NOT A PUBLISHED ONE. Dirtiness is a fact about a working
# tree; publication is a fact about a ref.
#
# WHAT IT DELIBERATELY DOES NOT DO
#
# It does not fetch. A pre-push hook that makes a network call per sibling is
# slow enough to resent and broken offline, and this check is worth having only
# if it is free. So it reads each sibling's ALREADY-FETCHED origin/master, and
# skips a sibling whose remote ref is missing rather than guessing.
#
# THIS GATE CANNOT PREDICT CARGO, AND THE PAIR IS A DIAGNOSTIC.
#
# They read DIFFERENT SUBJECTS. Cargo resolves a path dep to the sibling's
# WORKING TREE; this gate reads the sibling's PUBLISHED ref. So the two can
# disagree, and the disagreement is informative rather than a bug in either:
#
#     cargo --locked refuses  +  this gate says MATCHES
#       -> the sibling's DISK is ahead of its master. Absorbing would put the
#          lock in the dangerous AHEAD state. Do not absorb; gate with
#          --offline instead and leave the lock where it is.
#
#     cargo --locked refuses  +  this gate says BEHIND
#       -> an ordinary wave. Absorb it.
#
# Measured 2026-09-19: cargo refused while this gate passed, because subc-core
# read 0.18.17 on the sibling's disk and 0.18.16 on its master. Absorbing the
# obvious way would have pinned a version nobody had shipped.
#
# WHICH MEANS A STALE CACHED REF PRODUCES A FALSE REFUSAL, and that is the cost.
#
# I first wrote here that not fetching only meant "it cannot CATCH a sibling
# publishing something this machine has not fetched". That understated it: a
# cached `origin/master` behind the real one reports the lock as AHEAD of
# published when it is merely ahead of my copy, and the refusal blocks a stage
# with an explanation that is wrong in the direction of alarm.
#
# Measured: subc-core 0.18.14 was on the sibling's master, my cached ref said
# 0.18.13, and the gate refused. A `git fetch` in the sibling resolved it.
#
# So the refusal message names the fetch as the FIRST thing to try, ahead of the
# unlanded-branch explanation. A guard whose commonest false positive has a
# one-command fix must say that command, or its reader learns to distrust it —
# and a distrusted guard is one that gets skipped when it is right.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

lock="$root/Cargo.lock"
[ -f "$lock" ] || exit 0

status=0
checked=0
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

# Sibling path deps, read from the workspace manifest rather than listed here:
# a hardcoded list is a borrowed constant that goes stale the moment a
# dependency is added, and it goes stale SILENTLY, which is the same class of
# defect this script exists to catch.
deps=$(awk '/^\[workspace.dependencies\]/{f=1;next} /^\[/{f=0} f' Cargo.toml \
    | sed -n 's/^\([A-Za-z0-9_-]*\)[[:space:]]*=.*path[[:space:]]*=[[:space:]]*"\(\.\.[^"]*\)".*/\1 \2/p')

[ -n "$deps" ] || {
    echo "lock-vs-published: found no sibling path deps in Cargo.toml" >&2
    echo "  Either the manifest changed shape or this parser broke. Refusing" >&2
    echo "  rather than passing, because a check that silently examines" >&2
    echo "  nothing is worse than no check." >&2
    exit 1
}

# Fed by redirection rather than a pipe, DELIBERATELY. A `while` on the right of
# a pipe runs in a SUBSHELL, so every `status=1` set inside it is discarded and
# the script exits 0 while printing a refusal in full.
#
# This script did exactly that on its first run: it correctly detected a real
# divergence, printed the whole explanation, and reported success. A guard that
# detects and cannot refuse is worse than no guard, because its output is
# evidence that it works.
printf '%s\n' "$deps" > "$tmp"
while read -r name path; do
    [ -n "$name" ] || continue

    sibling_repo=$(cd "$root/$path" 2>/dev/null && git rev-parse --show-toplevel 2>/dev/null) || continue
    rel=$(cd "$root/$path" && pwd | sed "s|^$sibling_repo/||")

    published=$(git -C "$sibling_repo" show "origin/master:$rel/Cargo.toml" 2>/dev/null \
        | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -1) || true

    # No fetched remote ref, or a sibling with no origin: skip rather than
    # guess. Reported, so a silent skip cannot masquerade as a pass.
    [ -n "$published" ] || {
        echo "lock-vs-published: $name — no fetched origin/master, skipped" >&2
        continue
    }

    # The PATH-sourced entry specifically, and that qualifier is load-bearing.
    #
    # A name can appear in Cargo.lock more than once: the same package pulled
    # from git at a pinned rev is a DIFFERENT INSTANCE from the one resolved
    # through a path dependency, with its own version. Taking the first match
    # compares a git-pinned transitive against a sibling's published path
    # version — a true reading of the wrong subject, which is what this script
    # did on its first run and reported as a divergence.
    #
    # The discriminator is structural rather than textual: a path dependency has
    # NO `source` line in the lock, while git and registry entries always do.
    #
    # AND THE GIT ENTRIES NEED NO CHECK OF THEIR OWN, which is worth writing
    # down so the next reader does not "extend" this to cover them.
    #
    # A git dependency pins an immutable rev:
    #
    #     source = "git+https://github.com/cortexkit/commons.git?rev=50a70f2d..."
    #
    # CI resolves that exact object, not whatever the branch points at now. So
    # the wave problem this script exists for — a sibling publishes, my pin goes
    # stale, CI resolves the remote and disagrees with my local build — cannot
    # occur: there is no moving target. The lock IS the version.
    #
    # Their failure mode is different and much rarer: a force-push plus GC could
    # orphan the rev, and CI would fail to resolve it at all. That is loud, not
    # silent, and no local check can predict it.
    #
    # SUBC's 2026-09-19 finding is the same distinction from the producer side:
    # a merged bump and a PUBLISHED version are different facts, and a path
    # consumer absorbing the merge reads exactly like distribution working. For
    # fusiform every CortexKit dependency is a path dep, so "published" here
    # means "on the sibling's master" with no registry hop to diverge — which is
    # why this script's subject is correct for this repo and would be the wrong
    # subject for one consuming the same crates from crates.io.
    pinned=$(awk -v n="$name" '
        /^\[\[package\]\]/ { name=""; ver=""; src=""; next }
        $1=="name"    { gsub(/"/,"",$3); name=$3; next }
        $1=="version" { gsub(/"/,"",$3); ver=$3;  next }
        $1=="source"  { src=$3; next }
        # `exit` runs END, so a naive END fallback prints the version TWICE and
        # nothing ever compares equal — a guard that refuses everything, which
        # is worth exactly what one that passes everything is. The flag makes
        # the two branches exclusive.
        /^$/ { if (name==n && src=="" && !done) { print ver; done=1; exit } }
        END  { if (name==n && src=="" && !done) print ver }
    ' "$lock")

    [ -n "$pinned" ] || continue
    checked=$((checked + 1))

    if [ "$pinned" != "$published" ]; then
        echo "lock-vs-published: REFUSING — $name" >&2
        echo "  Cargo.lock pins        $pinned" >&2
        echo "  $name published        $published" >&2
        echo >&2

        # WHICH DIRECTION, because the two have different causes and opposite
        # actions, and printing one text for both misdirects half the time.
        #
        # This printed the AHEAD guidance unconditionally until it fired on a
        # BEHIND case and sent me hunting an unlanded branch for a version that
        # was plainly on master. A wrong hint is worse than none: it moves the
        # reader away and costs a search plus the time it takes to stop trusting
        # the tool — which is the exact defect I had just fixed in this repo's
        # route refusals, reproduced in my own gate hours later.
        #
        # `sort -V` decides it: a version-aware compare, so 0.18.9 is BELOW
        # 0.18.16 rather than above it, which a lexical compare gets backwards
        # on precisely the two-digit patch numbers this fleet reaches weekly.
        newest=$(printf '%s\n%s\n' "$pinned" "$published" | sort -V | tail -1)

        if [ "$newest" = "$published" ]; then
            echo "  BEHIND: a wave landed. The sibling published a version this" >&2
            echo "  lock does not have, so CI — which resolves path deps against" >&2
            echo "  REMOTES — will refuse the build." >&2
            echo >&2
            echo "  Absorb it:" >&2
            echo >&2
            echo "      cargo update -w --offline && ./scripts/lock-vs-published.sh" >&2
            echo >&2
            echo "  Then gate and commit the lock on its own." >&2
        else
            echo "  AHEAD: this lock pins a version that is NOT on the sibling's" >&2
            echo "  master. That is the dangerous direction — every local gate" >&2
            echo "  keeps passing forever while CI goes red against something" >&2
            echo "  nobody shipped, and nothing in this repo explains the red." >&2
            echo >&2
            echo "  CHEAPEST FIRST, regardless of likelihood: this check does not" >&2
            echo "  fetch, so a stale cached ref reports a divergence that does" >&2
            echo "  not exist." >&2
            echo >&2
            echo "      git -C $sibling_repo fetch origin" >&2
            echo >&2
            echo "  If it still refuses, the sibling likely has a branch checked" >&2
            echo "  out. Confirm:" >&2
            echo >&2
            echo "      git -C $sibling_repo log --all -S 'version = \"$pinned\"' -- $rel/Cargo.toml" >&2
            echo >&2
            echo "  If it is on an unlanded branch, restore the lock and wait for" >&2
            echo "  the wave notice." >&2
        fi
        status=1
    fi
done < "$tmp"

rm -f "$tmp"

[ "$checked" -gt 0 ] || {
    echo "lock-vs-published: examined nothing — every sibling was skipped" >&2
    echo "  A pass on zero comparisons is not a pass. Fetch the siblings." >&2
    exit 1
}

exit $status
