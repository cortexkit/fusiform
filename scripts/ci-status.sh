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
# DAYS, not a run count, and the distinction is the first defect this script
# shipped with.
#
# It asked for the last 60 RUNS. The question is about the last few DAYS, and
# those diverge exactly when the repository is busy: twenty trains in one night
# flushed both failing scheduled runs out of a 60-run window, and the script
# reported "no scheduled runs — the schedule may not be firing", which is a
# false alarm with a credible story. A count window is a proxy for time that
# fails under load, and load is when you most want the check.
days="${CI_STATUS_DAYS:-7}"

# Fetched generously and filtered by DATE below. The limit is a ceiling on the
# fetch rather than the window itself.
runs=$(gh run list --workflow="$workflow" --limit 200 \
    --json event,conclusion,headSha,createdAt 2>/dev/null) || {
    echo "ci-status: could not read run history for $workflow" >&2
    exit 1
}

# Non-push runs only. A push run's result is read by whoever pushed; a
# scheduled or dispatched one has no such reader by construction.
unwatched=$(printf '%s' "$runs" | CI_STATUS_DAYS="$days" python3 -c '
import datetime, json, os, sys
runs = json.load(sys.stdin)
days = int(os.environ["CI_STATUS_DAYS"])
cut = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)
def when(r):
    return datetime.datetime.fromisoformat(r["createdAt"].replace("Z", "+00:00"))
runs = [r for r in runs if when(r) >= cut]
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
        echo "ci-status: no scheduled or dispatched runs in the last $days day(s) —"
        echo "  the schedule may not be firing at all, which is its own defect:"
        echo "  a gate that never runs reports the same silence as one that passes."
        exit 1
        ;;
    *" 0 failed")
        echo "ci-status: $unwatched (last $days day(s))"
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
