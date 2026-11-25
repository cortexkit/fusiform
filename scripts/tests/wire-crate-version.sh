#!/usr/bin/env bash
set -euo pipefail

check="$(cd "$(dirname "$0")/.." && pwd)/check-wire-crate-version.sh"
scratch=$(mktemp -d)
finished=0
trap 'rc=$?; rm -rf "$scratch"; if [ "$rc" -eq 0 ] && [ "$finished" -ne 1 ]; then exit 1; fi' EXIT
git init -q -b base "$scratch/full"
cd "$scratch/full"
git config user.name Test
git config user.email test@example.invalid
manifest=crates/fusiform-protocol/Cargo.toml
src=crates/fusiform-protocol/src/lib.rs
mkdir -p "$(dirname "$src")"
printf '[package]\nversion = "0.10.0"\n\n[dependencies]\nserde_json = { features = ["arbitrary_precision"] }\n' > "$manifest"
printf 'pub const PRODUCT: u32 = 2\n    * 1000;\n' > "$src"
git add . && git commit -qm base

failures=0
expect() {
  local name=$1 expected=$2 rc=0 output
  output=$(bash "$check" base 2>&1) || rc=$?
  if [ "$rc" -ne "$expected" ]; then
    printf '%s: expected exit %s, got %s: %s\n' "$name" "$expected" "$rc" "$output" >&2
    failures=$((failures + 1))
  else
    printf 'Passed: %s (exit %s)\n' "$name" "$rc"
  fi
}

git switch -qc product
printf 'pub const PRODUCT: u32 = 2\n    * 1;\n' > "$src"
git commit -qam product
expect star_prefixed_code_needs_bump 1
git clone -q --no-local --depth=1 --no-single-branch . "$scratch/shallow"
(cd "$scratch/shallow" && git branch base origin/base)
cd "$scratch/shallow"
expect missing_merge_base_is_an_error 2
cd "$scratch/full"

git switch -qc dependency base
printf '[package]\nversion = "0.10.0"\n\n[dependencies]\nserde_json = {}\n' > "$manifest"
git commit -qam dependency
expect dependency_features_need_bump 1

git switch -qc feature base
printf '\n[features]\ndefault = []\n' >> "$manifest"
git commit -qam feature
expect feature_table_needs_bump 1

git switch -qc version product
printf '\npub struct AddedWireType;\n' >> "$src"
printf '[package]\nversion = "0.9.0"\n\n[dependencies]\nserde_json = { features = ["arbitrary_precision"] }\n' > "$manifest"
git commit -qam downgrade
expect version_downgrade_is_not_a_bump 1
printf '[package]\nversion = "0.11.0"\n\n[dependencies]\nserde_json = { features = ["arbitrary_precision"] }\n' > "$manifest"
git commit -qam upgrade
expect version_bump_allows_schema_change 0

git switch -qc docs base
printf '// Wire documentation.\n' >> "$src"
git commit -qam docs
expect documentation_only_does_not_need_bump 0

[ "$failures" -eq 0 ]
finished=1
