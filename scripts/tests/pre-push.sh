#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
scratch=$(mktemp -d)
finished=0
trap 'rc=$?; rm -rf "$scratch"; if [ "$rc" -eq 0 ] && [ "$finished" -ne 1 ]; then exit 1; fi' EXIT
git init -q --bare "$scratch/remote.git"
git init -q -b main "$scratch/local"
cd "$scratch/local"
git config user.name Test
git config user.email test@example.invalid
git config core.hooksPath .githooks
git remote add origin "$scratch/remote.git"
mkdir -p .githooks scripts src
cp "$root/.githooks/pre-push" .githooks/pre-push
cp "$root/scripts/no-outside-path-deps.sh" scripts/no-outside-path-deps.sh
printf '[package]\nname = "hook-test"\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
printf 'pub fn f() -> u8 {\n    1\n}\n' > src/lib.rs
cargo generate-lockfile --offline
git add . && git commit -qm formatted

git switch -qc unformatted
printf 'pub fn f( )->u8{1}\n' > src/lib.rs
git commit -qam unformatted

failures=0
expect_refusal() {
  local name=$1 out
  shift
  if out=$(git push origin "$@" 2>&1); then
    printf '%s: unformatted commit was pushed\n' "$name" >&2
    failures=$((failures + 1))
  elif [[ "$out" != *"pre-push refused: formatting"* ]]; then
    printf '%s: missing formatting refusal: %s\n' "$name" "$out" >&2
    failures=$((failures + 1))
  else
    printf 'Passed: %s\n' "$name"
  fi
}

expect_refusal checked_out_unformatted_commit_is_refused unformatted:refs/heads/control
git switch -q main
expect_refusal non_checked_out_unformatted_commit_is_refused unformatted:refs/heads/other
expect_refusal multi_ref_push_checks_each_commit main:refs/heads/mixed-good unformatted:refs/heads/mixed-bad
git switch -q unformatted
cargo fmt --all
expect_refusal uncommitted_formatting_does_not_validate_commit unformatted:refs/heads/dirty

git push origin main:refs/heads/clean
git push origin :refs/heads/clean
printf 'Passed: a formatted commit can be pushed and the ref can be deleted\n'
[ "$failures" -eq 0 ]
finished=1
