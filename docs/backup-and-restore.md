# Backup and restore

Fusiform's store is one SQLite file. It describes the world rather than the
machine, so it can be backed up as a whole and restored anywhere. This document
covers enrolling the store with the CortexKit backup module, `engram`, and what
a restore does to each kind of state.

## 1. The enrollment descriptor

The descriptor is `crates/fusiform-module/data/engram-catalog.json`:

```json
{
  "schema_version": 1,
  "module_id": "fusiform",
  "entries": [
    {
      "entry_id": "fusiform/store",
      "class": "portable",
      "mechanism": "whole-db",
      "path": "store.db",
      "writer_interaction": "backup-api-live"
    }
  ]
}
```

| Value | Why |
| --- | --- |
| `class: portable` | every row is derived from the upstream and fusiform's own observations; nothing in it is specific to the device |
| `mechanism: whole-db` | one SQLite file, captured whole |
| `writer_interaction: backup-api-live` | SQLite's online backup API reads a consistent snapshot while the poll loop keeps writing, so polling never pauses for a backup |

Not declared, deliberately:

- The `-wal` and `-shm` files. The backup API resolves committed WAL frames
  into the snapshot; copying the files separately could capture a torn pair.
- The lease file. It names a writer on this machine; a restored store takes a
  fresh lease when the module opens it.

Engram reads the descriptor from the module's data directory, not from the
repository. A data directory without one is not captured.

## 2. How it gets there

The module writes it. Every time `ck-fusiform` opens its store, it writes the
descriptor (compiled into the binary from the repository file, byte for byte)
as `engram-catalog.json` in the directory holding `store.db`. The directory is
taken from the store path the daemon handed over, never from environment
variables, so the descriptor's relative `store.db` names the database in use.
There is nothing to install by hand.

On each start:

1. If an identical descriptor is already there, only its mode is set to `0600`.
2. Otherwise the bytes are written to a temporary file in the same directory,
   flushed to disk, set to `0600`, renamed over `engram-catalog.json`, and read
   back and compared. A failed write never leaves a partial descriptor, because
   the entry is `portable` and an unreadable one fails engram's whole capture
   run, not just fusiform's entry.
3. A live descriptor that differs from the binary's is overwritten: the
   repository's bytes are authoritative. The module logs one line with the old
   and new sizes. To change the descriptor, change the repository file and
   release.

If the write fails, the module keeps serving. It logs the path and the error on
stderr, and health reports `Degraded` with a detail naming
`engram-catalog.json` (and the metric `process_backup_catalog_write_failed` is
`true`) until a restart writes the file.

The module does not run engram's own parser. A descriptor engram rejects shows
up in engram's own capture report, not here.

Verify: the file beside `store.db` is identical to the repository's, and
engram's next published capture includes the `fusiform/store` entry. A
descriptor on disk is not evidence of a backup; a published capture that
includes the entry is.

Reverse: there is no per-machine opt-out. Removing `engram-catalog.json` from
the data directory stops capture only until the module next starts and writes
it again.

Moving or renaming `store.db` without updating the descriptor makes a
`portable` entry point at a missing file, which fails the capture. The module
refuses to write the descriptor beside a store not named `store.db`, and
reports that as the same `Degraded` state.

## 3. What a restore does

A restore replaces the database file with an older copy. Each piece of state
behaves differently:

| State | After a restore | Recovers |
| --- | --- | --- |
| `era` rows | recent facts revert to older values | at the next poll, which writes ordinary observed eras for whatever moved; up to one poll interval (30 minutes) |
| `observation` rows | recent polls are missing | the next poll's window opens against an older prior observation, so it is wider than the truth: a weaker claim, never a false one |
| `plan_price_era` rows | revert | at start: the curated file is re-applied on every boot, before the first read is served |
| `catalog_version` | the stored counter rewinds | immediately: the next value is `max(now_ms, current + 1)`, so it still moves forward (`store_durability::a_restore_rewinds_the_row_counts_but_not_the_catalog_version`) |
| writer fence epoch | rewinds | the next fenced write claims a new epoch (`store_durability::a_restore_rewinds_the_fence_and_the_catalog_version_recovers`) |
| stored ETag | an older validator is sent | models.dev has been observed to answer an unrecognised validator with a full `200`, so the next poll reads the current document. This depends on the upstream's behaviour, not on fusiform's code |
| health signals | not stored | adopted from the restored store at start |

So after a restore, subscription prices are current as soon as the module
answers, while the model catalog serves the restored copy's values until the
first poll after start completes and records what has changed since.

A consumer can tell a restored store from a historical read: after a restore,
`resolved_at_ms` is close to now while the identity set has shrunk, and
`catalog_version` has still gone up. See
[`wire-contract.md`](wire-contract.md) §3.

Backup coverage is not what keeps the catalog version monotonic; the
derivation from the wall clock is (see [`data-model.md`](data-model.md) §4).
