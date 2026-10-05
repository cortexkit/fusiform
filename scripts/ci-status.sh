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
# Two consecutive scheduled failures (2026-09-17 and 2026-09-18) once went
# unnoticed for two days, found by accident while checking an unrelated claim.
# Same shape as a review date with no reader: a signal that fails in the QUIET
# direction, where the absence of a reaction is indistinguishable from the
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
#
# THIS NUMBER CANNOT PRODUCE A FALSE "SCHEDULE IS DEAD", AND THE REASON IS
# WORTH STATING BECAUSE A REASONABLE EDIT WOULD BREAK IT.
#
# SUBC's rule, after we measured that the declared cron fires 3h50m to 5h19m
# late: A CHECK THAT CONCLUDES "IT STOPPED" MUST SIZE ITS WINDOW FROM THE
# OBSERVED DELIVERY DISTRIBUTION, NOT FROM THE DECLARED SCHEDULE. A daily cron
# read through a 24-hour window, queried between the declared time and the real
# delivery, finds nothing and reports a dead schedule that is merely late.
#
# It does not apply here, which I established by driving `CI_STATUS_DAYS=1` and
# getting the correct verdict rather than by reading the code and agreeing with
# myself. The NONE branch fires on an EMPTY gh RESULT, and that query has no
# time bound at all -- it is server-filtered by event and capped by --limit. So
# this window scopes only the older-failures context, exactly as the comment
# beside the verdict says.
#
# WHAT WOULD INTRODUCE THE DEFECT: making NONE mean "no run RECENTLY" by
# applying `cut` to the emptiness test. That is a natural change -- it makes the
# verdict answer "is the schedule alive now" instead of "has it ever run" -- and
# it silently acquires a ~30h floor (one cron period plus the measured maximum
# delivery delay). Fourteen days would clear it; a day or two would not, and
# tightening this for noise reasons is equally natural.
#
# I wrote the floor above as a live hazard first. It was not one, and my own
# `CI_STATUS_DAYS=1` run had already shown it.
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
    --json status,conclusion,headSha,createdAt 2>/dev/null) || {
    echo "ci-status: could not read scheduled runs for $workflow" >&2
    exit 1
}

# THE VERDICT IS THE NEWEST RUN; THE HISTORY IS CONTEXT.
#
# This first exited 1 whenever ANY run in the window had failed, and that is a
# gate which fails for two weeks after its cause is fixed — the precise thing I
# wrote into its own commit message ("still reporting failures whose cause was
# fixed weeks ago, which trains its reader to skim") and then shipped.
#
# The question a reader has is "is master red NOW". A run from nine days ago
# whose cause landed today answers a different one. So the exit code follows the
# newest scheduled run, and older failures print as context BELOW it — visible,
# because a pattern of reds is worth seeing, and not alarming, because they are
# answered.
#
# The date window now only scopes that context. It is load-bearing for neither
# correctness nor the verdict, which is the right place for a number nobody
# measured.
verdict=$(printf '%s' "$runs" | CI_STATUS_DAYS="$days" python3 -c '
import datetime, json, os, sys
runs = json.load(sys.stdin)
if not runs:
    print("NONE")
    sys.exit(0)

days = int(os.environ["CI_STATUS_DAYS"])
cut = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)
def when(r):
    return datetime.datetime.fromisoformat(r["createdAt"].replace("Z", "+00:00"))
def bad(r):
    return r["conclusion"] not in ("success", None, "")

# The verdict comes from the newest COMPLETED run. A queued or running run has
# no conclusion yet, and bad() reads an empty conclusion as not-bad, so taking
# the newest run of any status printed PASSED for a run that had not started.
# A newer unfinished run is reported beside the verdict instead.
completed = [r for r in runs if r.get("status") == "completed"]
if not completed:
    print("UNFINISHED")
    sys.exit(0)

# gh returns newest first. Taken by DATE rather than by position, because a
# trust in list ordering is the kind of assumption that holds until it does not.
newest = max(completed, key=when)
pending = [r for r in runs
           if r.get("status") != "completed" and when(r) > when(newest)]
later = max(pending, key=when) if pending else None
window = [r for r in completed if when(r) >= cut]
older = [r for r in window if bad(r) and r is not newest]

state = "RED" if bad(newest) else "GREEN"
print("%s|%s|%s|%s|%d|%s" % (
    state, newest["createdAt"][:16], newest["conclusion"], newest["headSha"][:7],
    len(older),
    "%s %s %s" % (later["status"], later["createdAt"][:16], later["headSha"][:7])
    if later else ""))
for r in older[:8]:
    print("  %s  %-10s %s" % (r["createdAt"][:16], r["conclusion"], r["headSha"][:7]))
')

case "$verdict" in
    NONE)
        echo "ci-status: NO scheduled runs at all."
        echo
        echo "  This query filters server-side, so a busy repository cannot"
        echo "  hide them — an empty result means the schedule is not firing."
        echo "  A gate that never runs reports the same silence as one that"
        echo "  passes, which is the deadlock QTA hit in September."
        exit 1
        ;;
    UNFINISHED)
        echo "ci-status: scheduled runs exist but none has completed yet."
        echo
        echo "  There is no verdict to report; run this again once one finishes."
        exit 1
        ;;
esac

head=$(printf '%s' "$verdict" | head -1)
rest=$(printf '%s' "$verdict" | tail -n +2)

state=$(printf '%s' "$head" | cut -d'|' -f1)
stamp=$(printf '%s' "$head" | cut -d'|' -f2)
concl=$(printf '%s' "$head" | cut -d'|' -f3)
sha=$(printf '%s' "$head" | cut -d'|' -f4)
older=$(printf '%s' "$head" | cut -d'|' -f5)
later=$(printf '%s' "$head" | cut -d'|' -f6)

# Printed before the verdict so it cannot be read as part of it: the verdict
# below is about the newest FINISHED run, not this one.
if [ -n "$later" ]; then
    echo "ci-status: a newer scheduled run is not finished yet: $later"
fi

if [ "$state" = "GREEN" ]; then
    echo "ci-status: newest scheduled run PASSED  ($stamp, $sha)"
    if [ "$older" -gt 0 ]; then
        echo
        echo "  $older earlier failure(s) in the last $days day(s), shown as context"
        echo "  — the newest run is green, so these are answered:"
        printf '%s\n' "$rest"
    fi
    exit 0
fi

echo "ci-status: newest scheduled run FAILED  ($stamp, $concl, $sha)" >&2
echo >&2
# Every dependency is a crates.io version pinned in Cargo.lock, so nothing can
# move underneath a commit that is sitting still. A scheduled red on an
# unchanged commit therefore means something that depends on the DATE fired —
# the curated plan-price and window-overlay review gates, or the seed freshness
# gate — or the world outside the repository changed: the Rust toolchain,
# which CI takes as `stable`, or GitHub itself. Each of those needs the log;
# none is cleared by touching the lock.
echo "  It ran with nobody attending. With every dependency pinned, the" >&2
echo "  likely causes are a date-triggered gate (a curated review date or" >&2
echo "  seed freshness) or a new stable toolchain. Read the log:" >&2
echo >&2
echo "      gh run view <id> --log-failed" >&2
if [ "$older" -gt 0 ]; then
    echo >&2
    echo "  $older earlier failure(s) in the last $days day(s):" >&2
    printf '%s\n' "$rest" >&2
fi
exit 1
