# Backup enrollment

Fusiform's store is enrolled with `engram`, the fleet's backup module, as a
single portable whole-database entry.

**Installed and captured on this machine 2026-08-12.** ENGRAM reviewed the
descriptor and ruled it correct as written; the first capture afterwards
published as generation 156 carrying `fusiform/store`, taking the fleet's
`entry_count` from 6 to 7.

**How to confirm coverage without asking anyone** (ENGRAM's own suggestion):
`entry_count` on the newest published generation. It should read 7 and stay
there unless a module joins or leaves.

```sh
sqlite3 ~/.local/share/cortexkit/engram/store.db \
  "SELECT device_seq, entry_count, published, \
          datetime(created_at,'unixepoch') AS created \
   FROM generations ORDER BY device_seq DESC LIMIT 5;"
```

**Read `published` and `created`, not just `entry_count`.** The count answers
"is fusiform in the backup set", which is a different question from "is fusiform
backed up", and the two diverge exactly when publication stalls — the case the
check exists to catch.

Measured 2026-08-13 03:33 UTC: generation 166 published at 23:37 with 7 entries,
then 167, 168 and 169 staged with `published=0` and nothing uploaded. Engram had
halted capture on backpressure at three unpublished generations and reported it
as `degraded` with the cause named. **`entry_count` on the newest published
generation still read 7 and was still correct** — correct about a generation four
hours old.

**What `created` means, confirmed by ENGRAM from their source rather than
inferred from the column name**: it is written when capture STARTS the
generation row, before any sealing or upload. `published` flips at head CAS,
which under a stall is hours later.

So the honest exposure sentence is **"the backup is current through the newest
published generation's `created`"** — not its publish instant. If generation 169
was created at 02:55 and publishes at 09:00, the cloud holds the state as of
02:55.

That is conservative in the right direction. A large capture takes minutes to
seal, so bytes are captured at various instants AFTER `created` and never
before: the sentence understates freshness slightly rather than overstating it.
For an exposure statement that is the direction to be wrong in.

So the shape to look for is a run of `published=0` rows above the newest
`published=1`, and the age of that newest one. A count that has not moved is not
evidence that captures are happening; it is evidence about the last capture that
finished.

The descriptor existing on disk is NOT the witness either. It was present for
over an hour before any capture carried it, and during that window fusiform was
correctly enrolled and entirely uncaptured — a state that looks identical, from
the file system, to being backed up.

Both are the same error one step apart: a fact that is true, checked correctly,
and about the wrong object.

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

**`class: portable`.** The store describes the world, not this machine.

> **This choice binds the whole fleet, not just fusiform.** An absent or
> unreadable `store.db` fails the ENTIRE fleet's capture, because engram
> refuses to publish a generation marked complete while promised data is
> missing (ENGRAM, 2026-08-12). Two consequences that outlive this document:
> the installer's refuse-when-no-store behaviour is load-bearing for every
> other module, and moving or renaming this store without updating the
> descriptor takes backups down fleet-wide. "Rename a file in my own repo" is
> not an action anyone checks the blast radius of, which is why it is written
> here and in the charter rather than left in a message thread.
 Every
row is derived from an upstream document plus fusiform's own observation
history, so restoring it onto a different machine is meaningful — there is no
device-local state in it. The lease file beside it is device-local and is
deliberately *not* declared: it is not in the entry list at all, so engram does
not capture it, and a restored store acquires a fresh lease on open.

**`mechanism: whole-db`.** One SQLite file, one class. Page-level capture
(`page-db`) exists for large stores where dedup pays, and it requires
`writer-interaction: read-transaction-live`, which holds a pager read
transaction and blocks WAL checkpoint progress for the duration of a sweep.
Measured on the live store rather than estimated: **19 MB**, with 9 changed
facts over roughly 6 hours. ENGRAM's ruling put numbers on the trade — that is
about 15 CDC chunks, so page-db would add 4 KB page hashing, slab packing and
cross-generation slab resolution to save bytes that are not being spent. It
earns its keep on their large scattered-write stores (broca's WAL tree,
prefrontal's 1.7 GB), not here.

An earlier version of this paragraph said "a few hundred megabytes at most",
which was a guess written before the store existed and wrong by an order of
magnitude.

**`writer_interaction: backup-api-live`.** SQLite's backup API never takes the
writer lock, so capture runs against the live database while the poll loop
keeps writing. `quiesce-required` would ask the module to stop writing, which
for fusiform would mean pausing polling — widening an observation window for a
backup, which is the one thing the cadence exists to keep narrow.

## What is deliberately not declared

**The WAL and SHM files.** Confirmed by ENGRAM from their source rather than
assumed here: engram does not file-copy. `WholeDb` opens through SQLite's
online backup API on a `mode=ro` connection, so the pager resolves committed
WAL frames into a transactionally consistent snapshot — no checkpoint, no
interference with the single writer. Declaring the siblings separately would
capture a torn pair.

There is also a pre-open guard (`engram-core/src/capture_db.rs`): if a header
declares WAL and the `-wal` companion is absent, capture refuses the
generation. The check runs **before any connection opens**, because opening one
creates the companion it looks for.

Why that guard exists is the part worth carrying: a naive file copy of a
WAL-mode database comes back **behind rather than corrupt**, and
`PRAGMA integrity_check` returns `ok` on it. The verification an operator would
naturally reach for — restore it, check it, see `ok` — confirms a database that
is silently missing recent writes.

**The lease file** (`*.lease`). Device-local by construction — it names a
writer on this machine — and a restored store must acquire its own.

## Permissions

The installed descriptor is `0600` and its directory `0700`, matching the
store beside it. `install-enrollment.sh` sets both, because `cp` gives the file
the repository copy's mode and a git working tree is world-readable.

Not a capture concern — engram only reads it — but a catalog descriptor is a
precise map of which files on this machine are worth taking, and it should be
no more exposed than the data it points at. It was world-readable for about an
hour after the first install, until ENGRAM noticed that every other enrolled
module keeps both at `0600`/`0700` and fusiform did not.

## Two planes recover at different speeds

Worth stating because it is one store with one backup entry and two different
expected-recovery states, and an operator reading "restored" will assume one
number.

| table | rewinds to | heals |
| --- | --- | --- |
| `era` | the backup | on the NEXT POLL — 30-minute cadence, so up to 30 minutes stale |
| `plan_price_era` | the backup | AT BOOT, before the module answers its first read |

The difference is where each plane's authority lives. The catalog's authority is
UPSTREAM, so it cannot heal without the network. The curated plane's authority
is the BINARY — the plan-price file is compiled in and re-applied on every start
— so a rewound table is indistinguishable from an incomplete one and the boot
diff fills both.

So after a restore, fusiform's subscription pricing is correct immediately while
its model catalog is stale until the first poll lands.

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
