#!/bin/sh
# Report CI runs that nobody was watching.
#
# WHY THIS EXISTS
#
# A push gate answers "is this change good" and you read it because you just
# pushed. A SCHEDULED run answers "is what already landed still good against the
# world as it is now" — and nothing makes anyone read it. Its whole value is
# that it fires when you are not looking, which is exactly why its result
# arrives when you are not looking.
#
# MEASURED, and this is not hypothetical: two consecutive scheduled runs failed
# on 2026-09-17 and 2026-09-18 and went unnoticed for two days. Both were a
# stale Cargo.lock against sibling REMOTES — the failure mode the scheduled run
# was added to catch, working perfectly, reported to nobody. Found by accident
# while checking an unrelated claim.
#
# Same shape as a review date with no reader, which this repo fixed twice today
# for curated data and had not fixed for its own CI: a signal that fails in the
# QUIET direction, where the absence of a reaction is indistinguishable from the
# absence of a problem.
#
# WHAT IT DOES NOT DO
#
# It does not judge whether a failure matters, and it does not fetch a log. It
# answers one question — has a run that nobody had a reason to read gone red —
# and prints enough to decide. A tool that summarised the failure would be
# guessing at a cause, and a wrong cause is worse than a bare fact.
set -eu

command -v gh > /dev/null 2>&1 || {
    echo "ci-status: gh is not on PATH; cannot read run history" >&2
    exit 1
}

workflow="${CI_STATUS_WORKFLOW:-ci.yml}"
window="${CI_STATUS_WINDOW:-60}"

runs=$(gh run list --workflow="$workflow" --limit "$window" \
    --json event,conclusion,headSha,createdAt 2>/dev/null) || {
    echo "ci-status: could not read run history for $workflow" >&2
    exit 1
}

# Non-push runs only. A push run's result is read by whoever pushed; a
# scheduled or dispatched one has no such reader by construction.
unwatched=$(printf '%s' "$runs" | python3 -c '
import json, sys
runs = json.load(sys.stdin)
rows = [r for r in runs if r["event"] != "push"]
if not rows:
    print("NONE")
    sys.exit(0)
bad = [r for r in rows if r["conclusion"] not in ("success", None, "")]
print(f"{len(rows)} unwatched run(s) in window, {len(bad)} failed")
for r in bad[:10]:
    when = r["createdAt"][:16]
    ev = r["event"]
    concl = r["conclusion"]
    sha = r["headSha"][:7]
    print("  %s  %-10s %-10s %s" % (when, ev, concl, sha))
')

case "$unwatched" in
    NONE)
        echo "ci-status: no scheduled or dispatched runs in the last $window — "
        echo "  the schedule may not be firing at all, which is its own defect:"
        echo "  a gate that never runs reports the same silence as one that passes."
        exit 1
        ;;
    *" 0 failed")
        echo "ci-status: $unwatched"
        ;;
    *)
        echo "ci-status: $unwatched" >&2
        echo >&2
        echo "  These ran when nobody had a reason to look. A scheduled red" >&2
        echo "  usually means master broke with nothing of yours moving —" >&2
        echo "  most often a sibling published and the committed lock is" >&2
        echo "  behind, since CI resolves path deps against REMOTES." >&2
        echo >&2
        echo "  Check the lock first:  ./scripts/lock-vs-published.sh" >&2
        echo "  Then the log:          gh run view <id> --log-failed" >&2
        exit 1
        ;;
esac
