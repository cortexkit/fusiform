# Backup enrollment

Fusiform's store is enrolled with `engram`, the fleet's backup module, as a
single portable whole-database entry.

The descriptor lives at `crates/fusiform-module/data/engram-catalog.json` in
this repository and must be **installed to the module's data directory** to
take effect:

```
~/.local/share/cortexkit/fusiform/engram-catalog.json
```

Engram discovers enrollments by walking `<data_home>/cortexkit/*/` and reading
`engram-catalog.json` from each module's directory
(`engram-module/src/fleet.rs`). A module directory without one is reported as
`NotEnrolled` and **is not captured** — engram never guesses.

Install it with `scripts/install-enrollment.sh`, **after the module is deployed
and its store exists**:

```sh
scripts/install-enrollment.sh
```

The installer refuses if there is no `store.db` in the data directory. Enrolling
an undeployed module puts an entry in engram's fleet walk pointing at a database
that does not exist — a change to a running system with no store to protect in
exchange. I made exactly that mistake writing this: installed the descriptor on
a machine where fusiform is not deployed, then backed it out. The guard exists
because the mistake is easy and its effects land somewhere I would not see them.

## Why the file is in the repository at all

Engram reads it from the data directory, so the copy that matters is the
installed one. Keeping the source of truth in git is deliberate: the installed
file is untracked, unversioned, and invisible to review, and every other
enrollment on this machine exists only as a hand-placed file that no repository
knows about. A descriptor that lives only in a data directory is one `rm -rf`
away from silently un-enrolling a module, and the symptom is that backups keep
reporting success while covering less.

`the_repository_descriptor_is_valid` parses this file with engram's own
`catalog::plan`, so a typo fails a test here rather than showing up as a
`NotEnrolled` line an operator has to notice.

## Why these values

**`class: portable`.** The store describes the world, not this machine. Every
row is derived from an upstream document plus fusiform's own observation
history, so restoring it onto a different machine is meaningful — there is no
device-local state in it. The lease file beside it is device-local and is
deliberately *not* declared: it is not in the entry list at all, so engram does
not capture it, and a restored store acquires a fresh lease on open.

**`mechanism: whole-db`.** One SQLite file, one class. Page-level capture
(`page-db`) exists for large stores where dedup pays, and it requires
`writer-interaction: read-transaction-live`, which holds a pager read
transaction and blocks WAL checkpoint progress for the duration of a sweep.
The store is a few hundred megabytes at most and the poll loop writes every 30
minutes; the simpler mechanism is the right trade until the size argues
otherwise.

**`writer_interaction: backup-api-live`.** SQLite's backup API never takes the
writer lock, so capture runs against the live database while the poll loop
keeps writing. `quiesce-required` would ask the module to stop writing, which
for fusiform would mean pausing polling — widening an observation window for a
backup, which is the one thing the cadence exists to keep narrow.

## What is deliberately not declared

**The WAL and SHM files.** They are siblings of `store.db` and the backup API
handles them; declaring them separately would capture a torn pair.

**The lease file** (`*.lease`). Device-local by construction — it names a
writer on this machine — and a restored store must acquire its own.

## What enrollment does not protect

Enrollment is backup coverage. It is **not** what keeps the catalog version
monotonic across a restore.

A whole-db restore replaces the file, so anything stored inside the database is
restored with it, including any counter or watermark meant to survive. The
catalog version is therefore derived as `max(now_ms, current + 1)`
(`fusiform-store/src/lib.rs`), which is immune to a restore because wall-clock
time does not rewind when a file is put back.

This is worth stating explicitly because the design note once claimed the
opposite: that a `restore-with-monotonic-fence` declared here protected the
counter. No such mechanism exists in engram's descriptor vocabulary, and no
enrollment flag could have supplied it.
