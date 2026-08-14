#!/usr/bin/env bash
# Prove a guard can fail: break the code it guards, and check it reddens.
#
# WHY THIS IS A FILE RATHER THAN A HABIT
#
# Every mutation run in this repository was an ad-hoc script written from
# scratch, so every run could reinvent the same defects — and did, twice in one
# night on 2026-08-13:
#
#   1. The compile check matched `error[`, but a syntax error prints `error:`.
#      A mutation that did not build was reported as SURVIVED, which reads as
#      "your test has a gap" when the truth is "your mutation never ran".
#
#   2. Build and test were folded into one `cargo test` call. That prints
#      `error: test failed` when a test fails, so the compile check matched a
#      CAUGHT mutant and reported it as unbuildable.
#
# Both were fixed at the time, in the script of the moment, and neither fix
# survived to the next run. That is the instance-versus-shape failure: a repair
# recorded where it happened rather than as a property the tool must have.
# Fixing the instance feels complete, which is what makes it recur.
#
# THE THREE OUTCOMES THIS DISTINGUISHES, which is the whole point:
#
#   ANCHOR MISSING  the pattern is not in the file; nothing was tested
#   DID NOT COMPILE  the mutation is not valid code; nothing was tested
#   NO TESTS RAN     the suite executed zero tests; nothing was tested
#   SURVIVED         the mutation built, ran, and no test objected
#   caught by <test> the mutation built, ran, and that test failed
#
# Only the last two are results. The first three are the tool reporting that it
# failed to ask the question, and they must never be silently readable as
# either result.
#
# The third is ASTRO'\''s control generalised. They caught it by including a
# mutation known to be caught, so a run where everything survives is
# distinguishable from a run where nothing executed. Checking it here means the
# control is not something anyone has to remember to add.
#
# Usage:
#   scripts/mutate.sh <file> <old> <new> [cargo test args...]
#
# Example:
#   scripts/mutate.sh crates/fusiform-store/src/serve.rs \
#     'boundary_at_ms <= ?2' 'boundary_at_ms < ?2' \
#     -p fusiform-store --test serve
set -uo pipefail

cd "$(dirname "$0")/.."

if [ "$#" -lt 3 ]; then
  sed -n '2,40p' "$0" >&2
  exit 2
fi

FILE="$1"; OLD="$2"; NEW="$3"; shift 3
TEST_ARGS=("$@")
[ "${#TEST_ARGS[@]}" -eq 0 ] && TEST_ARGS=(--workspace)

[ -f "$FILE" ] || { echo "no such file: $FILE" >&2; exit 2; }

BACKUP="$(mktemp)"
cp "$FILE" "$BACKUP"
# Restore on any exit path, including interrupt. A mutation left applied is
# worse than no mutation run: the next thing anyone does is against modified
# source they do not know is modified.
#
# SEEING A MUTATED FILE DOES NOT MEAN THIS TRAP FAILED. A run takes tens of
# seconds, and for almost all of that the mutation IS applied — that is the
# point. An agent whose tool call times out at 25s sees a modified file and the
# available reading is "the script died before restoring", which is wrong: the
# process outlives the wait and restores normally.
#
# The discriminator costs nothing and is the one nobody runs: IS THE PROCESS
# STILL ALIVE. Measured 2026-08-13 — mutated mid-run with the script alive,
# clean after it exited, git tree clean.
#
# It matters because the intuitive repair is `git checkout` on the file, and
# doing that WHILE a run is in flight removes the mutation before the test
# executes. The suite then passes and the harness reports SURVIVED — a false
# clean bill produced by the act of tidying up. That is the same defect class as
# the seven this harness has already had: an action that makes a non-result look
# like a result.
trap 'cp "$BACKUP" "$FILE"; rm -f "$BACKUP"' EXIT INT TERM

if ! grep -qF -- "$OLD" "$FILE"; then
  echo "ANCHOR MISSING: $OLD"
  echo "  Nothing was tested. The pattern is not in $FILE — check whitespace"
  echo "  and formatting against the real source rather than what you expect."
  exit 1
fi

# Replace the first occurrence only. A pattern matching several sites mutates
# them together, and a test reddening tells you nothing about which one.
python3 - "$FILE" "$OLD" "$NEW" <<'PY'
import sys, pathlib
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
p = pathlib.Path(path)
p.write_text(p.read_text().replace(old, new, 1))
PY

# BUILD, as its own step, judged by its EXIT CODE.
#
# Not by grepping output for an error string: that is defect 1 and defect 2
# above, in both directions. The exit code is the compiler's own answer.
# Args after `--` are for the test binary, not for cargo, so the build step
# must not receive them. Passing them through made a valid mutation report
# DID NOT COMPILE — the harness failing in a fourth new way while being written
# to stop it failing in three.
BUILD_ARGS=()
for arg in "${TEST_ARGS[@]}"; do
  [ "$arg" = "--" ] && break
  BUILD_ARGS+=("$arg")
done

if ! cargo build --tests "${BUILD_ARGS[@]}" >/dev/null 2>&1; then
  echo "DID NOT COMPILE"
  echo "  Nothing was tested. The mutation is not valid code, so this run says"
  echo "  nothing about the guard. Try a mutation that type-checks."
  exit 1
fi

# TEST, as its own step.
OUT="$(cargo test "${TEST_ARGS[@]}" 2>&1)"

# Did any test actually RUN?
#
# ASTRO's control, generalised: a run where everything survives and a run where
# nothing executed produce identical output. They caught this by including a
# mutation known to be caught; the harness can check it directly, and then the
# control is not something anyone has to remember to add.
#
# Reached by a filter matching no tests, a target that builds nothing, or a
# `--test` name that does not exist — all of which print a clean, cheerful zero.
# PASSED PLUS FAILED, not passed alone.
#
# The seventh way this harness has reported a non-result as a result: `$4` is
# the PASSED count, so a mutation caught by EVERY test in its target summarises
# as `test result: FAILED. 0 passed; 2 failed` and totalled zero — reported as
# NO TESTS RAN, which reads as "check your filter" when the truth is "your guard
# worked perfectly". The two readings demand opposite responses, which is the
# same reason ANCHOR MISSING and SURVIVED had to be distinguished.
#
# Fields are located by the WORD that follows them rather than by position, so a
# change to libtest's summary wording fails loudly here instead of silently
# returning zero.
RAN="$(echo "$OUT" | awk '
  /^test result:/ {
    for (i = 1; i <= NF; i++) {
      if ($i ~ /^passed;?$/ || $i ~ /^failed;?$/) {
        n = $(i - 1); gsub(/[^0-9]/, "", n); total += n
      }
    }
  }
  END { print total + 0 }')"
if [ "$RAN" -eq 0 ]; then
  echo "NO TESTS RAN"
  echo "  Nothing was tested. The suite executed zero tests, so SURVIVED would"
  echo "  be meaningless here — check the filter and the --test name."
  exit 1
fi
# `$2` on a `test <name> ... FAILED` line is the test name. The SUMMARY line
# `test result: FAILED. 1 passed; 1 failed` also matches that pattern and
# yields "result:", so it is excluded explicitly.
#
# Cosmetic, and worth fixing anyway: every mutation run tonight printed a
# phantom test named `result:` beside the real one, and a control whose output
# contains noise nobody can explain is a control people stop reading.
FAILED="$(echo "$OUT" | awk '/^test .*FAILED/ && $2 != "result:" {print $2}' | paste -sd', ' -)"

if [ -n "$FAILED" ]; then
  echo "caught by: $FAILED"
  exit 0
fi

echo "*** SURVIVED ***"
echo "  The mutation built, the suite ran, and nothing objected. Either the"
echo "  guard has a gap, or the mutant is equivalent — if equivalent, say so in"
echo "  a comment at the site so nobody chases it again."
exit 1
