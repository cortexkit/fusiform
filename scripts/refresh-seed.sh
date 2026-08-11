#!/usr/bin/env bash
#
# Refresh the embedded bootstrap snapshot.
#
# This is the ONLY supported way to update the seed. It writes two files that
# must always move together, so they cannot drift:
#
#   crates/fusiform-module/data/models-dev-seed.json  — the snapshot
#   crates/fusiform-module/data/models-dev-seed.meta.json — its provenance
#
# The meta file's `fetched_at_ms` is load-bearing, not documentation. It becomes
# the instant of the `seeded` observation the store records, which is the left
# edge of the observation window for the first real fetch that disagrees with
# the snapshot. Get it wrong and every era opened by that first fetch claims a
# window that never happened.
#
# Never fetched at build or run time: a build that reaches the network produces
# a different binary depending on when it ran.
#
# Usage: scripts/refresh-seed.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="$repo_root/crates/fusiform-module/data"
snapshot="$data_dir/models-dev-seed.json"
meta="$data_dir/models-dev-seed.meta.json"
source_url="https://models.dev/api.json"

command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }
mkdir -p "$data_dir"

echo "→ fetching $source_url"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# The fetch instant is captured around the request rather than after the file is
# written, so a slow download cannot push the recorded instant later than the
# content it describes. Taken BEFORE the request: the upstream's content is
# known to be current as of the moment the request was made, and an instant
# recorded after a 30-second transfer would claim knowledge fusiform did not
# have at that time.
fetched_at_ms="$(python3 -c 'import time; print(int(time.time() * 1000))')"
curl -fsSL --max-time 120 "$source_url" -o "$tmp"

python3 - "$tmp" "$snapshot" "$meta" "$fetched_at_ms" "$source_url" <<'PY'
import hashlib, json, sys, datetime

raw_path, snapshot_path, meta_path, fetched_at_ms, source_url = sys.argv[1:6]
raw = open(raw_path, 'rb').read()

# Parse before writing: a snapshot that does not parse is worse than no
# snapshot, because it fails at install time on a machine with no network,
# which is the exact situation the seed exists for.
doc = json.loads(raw)
providers = len(doc)
models = sum(len(p.get('models') or {}) for p in doc.values())
if models < 1000:
    raise SystemExit(f"refusing a snapshot with only {models} models; the upstream looks wrong")

open(snapshot_path, 'wb').write(raw)

fetched_at_ms = int(fetched_at_ms)
meta = {
    "fetched_at_ms": fetched_at_ms,
    "fetched_at": datetime.datetime.fromtimestamp(
        fetched_at_ms / 1000, datetime.timezone.utc
    ).strftime('%Y-%m-%dT%H:%M:%SZ'),
    "source_url": source_url,
    "sha256": hashlib.sha256(raw).hexdigest(),
    "provider_count": providers,
    "model_count": models,
    "note": (
        "Bootstrap snapshot for a fresh install with no network. "
        "fetched_at_ms is load-bearing: it is the instant of the recorded "
        "`seeded` observation and the left edge of the first real fetch's "
        "observation window. Refresh via scripts/refresh-seed.sh."
    ),
}
open(meta_path, 'w').write(json.dumps(meta, indent=2, sort_keys=True) + "\n")

print(f"  {models} models across {providers} providers")
print(f"  {len(raw) / 1048576:.2f} MB")
print(f"  sha256 {meta['sha256']}")
print(f"  fetched_at {meta['fetched_at']}")
PY

echo "→ wrote $snapshot"
echo "→ wrote $meta"
echo
echo "Review the diff before committing. A seed refresh changes what a fresh"
echo "install believes before its first poll."
