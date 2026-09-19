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
# FOURTEEN because the cron is DAILY, so this is a sample count rather than a
# duration: fourteen runs is enough for a pattern to be visible and short enough
# that a fixed-and-green-since failure ages out. A week would show three reds in
# a bad stretch and read as noise; a month would still be reporting failures
# whose cause was fixed weeks ago, which trains its reader to skim.
#
# Derived from the schedule's own period rather than chosen, because "why is
# this number what it is" is answerable in seconds and I had never asked it of
# my own tools until SUBC put the question tonight.
days="${CI_STATUS_DAYS:-14}"

# `--event schedule` IS FILTERED SERVER-SIDE, and that removes a failure mode
# rather than bounding it.
#
# This script first asked for the last N RUNS and filtered locally. Twenty
# trains in one night pushed every scheduled run out of a 60-run window, and it
# reported "the schedule may not be firing at all" — a false alarm with a
# credible story. A count is a proxy for time and it degrades precisely when
# there is most to look at, which is the anti-correlation that makes it vicious.
#
# Measured here after SUBC measured it on their repo, because a claim about a
# tool's behaviour is checkable in one command:
#
#     gh run list --workflow=ci.yml --limit 30                    -> 0 scheduled
#     gh run list --workflow=ci.yml --event schedule --limit 30   -> 12 scheduled
#
# If the filter were applied after fetching 30, the second would also be 0. So
# the limit applies to the FILTERED set and a push flood cannot empty it.
#
# WHY SCHEDULE RATHER THAN "NOT PUSH", which is what this asked for first: a
# `workflow_dispatch` run has a reader by construction — the person who
# dispatched it is waiting for it. Only the schedule fires with nobody
# attending, so only the schedule has the silence this script exists to break.
runs=$(gh run list --workflow="$workflow" --event schedule --limit 40 \
    --json conclusion,headSha,createdAt 2>/dev/null) || {
    echo "ci-status: could not read scheduled runs for $workflow" >&2
    exit 1
}

# The date window still scopes "recently enough to matter". It is no longer
# load-bearing for CORRECTNESS — the fetch above cannot be flushed — so a bad
# choice here costs relevance rather than truth.
unwatched=$(printf '%s' "$runs" | CI_STATUS_DAYS="$days" python3 -c '
import datetime, json, os, sys
runs = json.load(sys.stdin)
days = int(os.environ["CI_STATUS_DAYS"])
cut = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)
def when(r):
    return datetime.datetime.fromisoformat(r["createdAt"].replace("Z", "+00:00"))
rows = [r for r in runs if when(r) >= cut]
if not rows:
    print("NONE")
    sys.exit(0)
bad = [r for r in rows if r["conclusion"] not in ("success", None, "")]
print(f"{len(rows)} scheduled run(s) in window, {len(bad)} failed")
for r in bad[:10]:
    stamp = r["createdAt"][:16]
    concl = r["conclusion"]
    sha = r["headSha"][:7]
    print("  %s  %-10s %s" % (stamp, concl, sha))
')

case "$unwatched" in
    NONE)
        echo "ci-status: NO scheduled runs in the last $days day(s)."
        echo
        echo "  This query filters server-side, so a busy repository cannot"
        echo "  hide them — an empty result means the schedule is not firing."
        echo "  A gate that never runs reports the same silence as one that"
        echo "  passes, which is the deadlock QTA hit in September."
        exit 1
        ;;
    *" 0 failed")
        echo "ci-status: $unwatched (last $days day(s))"
        ;;
    *)
        echo "ci-status: $unwatched" >&2
        echo >&2
        echo "  These ran with nobody attending. A scheduled red usually" >&2
        echo "  means master broke with nothing of yours moving — most" >&2
        echo "  often a sibling published and the committed lock is behind," >&2
        echo "  since CI resolves path deps against REMOTES." >&2
        echo >&2
        echo "  Check the lock first:  ./scripts/lock-vs-published.sh" >&2
        echo "  Then the log:          gh run view <id> --log-failed" >&2
        exit 1
        ;;
esac
