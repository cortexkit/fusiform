# A serialization change recorded itself as 17,455 upstream price changes

> **RESOLVED 2026-08-17. Fixed at two levels, and the damage is permanent by
> design.**
>
> The diff no longer compares stored bytes, and storage no longer carries the
> annotation that caused it. The 17,455 eras remain in the live store: an
> append-only history does not rewrite itself, and they are what fusiform
> believed when it wrote them.
>
> Kept because the defect is invisible in every artifact it touched. The
> values are correct, the store is consistent, health stayed green, and no
> test failed. Only the meaning of a boundary was wrong.

## What happened

Fusiform serves rates in USD under a named policy, because `models.dev`
publishes no currency field at all. `UnitProvenance` exists so a consumer can
tell an assumed currency from a published one — its own doc says serving USD
without saying why would launder a convention into a fact.

On 2026-08-16 that field was added to the **stored** rate representation.

The ingest diff compared serialized JSON strings. So every stored rate
differed textually from every freshly normalized one, and the first poll after
the binary was placed wrote one era per priced rate:

```
observation 203, 2026-08-16 07:54:20Z, duration 196ms
  17,455 eras, all rate.*
```

Neighbouring polls wrote between 0 and 340.

Each era carries `boundary_kind = observed` and an observation window bounded
by the 07:24 and 07:54 polls — a bounded claim that the provider moved its
price inside those thirty minutes.

```
anthropic/claude-sonnet-4-5  rate.input
  2026-08-12 10:07:29  seed      units 3000000000
  2026-08-16 07:54:20  observed  units 3000000000
```

Identical values. The only difference between the two rows is the field
fusiform added to its own encoding.

`normalized_hash` moved with the write, which is the signal consumers watch to
decide whether anything changed.

## Why nothing caught it

Every guard that could have fired was aimed somewhere else.

The **shrink guard** refuses implausible collapses in model count. This poll
changed no model count at all.

The **served vocabulary fence** asserts that stored facts use declared keys.
These used declared keys.

The **digest agreement test** (`the_digest_moves_exactly_when_the_diff_does`)
pins the signal to the era set. It reserializes the *upstream document* —
different bytes, same facts — so it covers the upstream reformatting case and
not the producer reformatting its own stored value.

**Health** reported `ok` throughout, correctly: it reads atomic signals and
never touches the store, which is what makes it survive a wedged database.

The module's own doc comment warned about exactly this hazard — "comparing
bytes would make a reformatted payload look like a change" — eleven lines
above the code that compared bytes. The warning was about the upstream
payload; the mistake was on the stored value.

## How it was found

By accident, three days later, through an unrelated symptom.

`catalog.status` began timing out on the IPC channel while `ck health` stayed
green. Chasing the timeout produced three false readings before the real one:

1. `ps` reported the daemon at 99.2% CPU. That is a **lifetime average** on
   macOS. A stack sample showed every thread parked in a kernel wait, and
   `top` reported 0.0% — the process was sleeping.
2. A missing index on `era.observation_id` looked like the cause. The query
   plan did say `SCAN`. But the exact query, measured on a copy of the live
   store, ran in **0.067s** — the diagnosis was falsified before it could be
   committed.
3. The real query, including a correlated `NOT EXISTS` that the simplified
   version had dropped, took **97 seconds** for ten polls. That led to the
   per-poll era counts, and one poll with 17,455 of them.

The status route derives what each poll changed by counting the eras that poll
wrote. It was the only surface that made the anomaly visible, and it made it
visible as a performance problem.

## The fix

**An era boundary is a claim about the source.** Fields fusiform adds describe
fusiform's own handling and must never open one.

Two levels, because the first alone left a conditional gap:

1. `states_the_same_upstream_claim` compares parsed values with producer
   annotations stripped, so neither a serialization change nor a key-order
   change can open a boundary.
2. Storage no longer carries the annotation at all. The serve path already
   attached it to any rate lacking it, so the storage write was **a second
   path to an outcome the first path already produced** — and a second path is
   not redundancy, it is a second thing that can be wrong.

The digest and the diff now share one definition, `upstream_claim_of`. Fixing
only the comparison would have left them able to disagree: a future stored
annotation would move the digest and wake every consumer while the diff
correctly wrote nothing, which is the "woken for a change that produced none"
failure the digest's own doc names as its reason for existing.

## What a consumer needs to know

**2026-08-16 07:54:20Z is an artifact instant for rate facts.**

- Values are correct on both sides of it. A comparison of values is unaffected.
- A count of *change events* crossing it is inflated by one per priced model.
- `catalog.history` shows a boundary there for essentially every rate.
- The observation window on those eras is a bounded claim about a change that
  did not occur.

Any window of `catalog.status --polls N` that spans that observation is also
slow — about 19 seconds — because `facts_changed` examines every era the poll
wrote. That cost is inherent rather than a missing index: an alternative SQL
formulation measured 86s against 86s with identical answers, so it is row
count and not query shape. It recedes as polls accumulate.

## What generalises

**A producer's representation change becomes a consumer's event** unless the
diff distinguishes what the source said from how the producer wrote it down.

This is the sibling of a rule already in force here — a producer's inference
becomes a consumer's fact unless the marker crosses with the value — and it is
the worse of the two. An inference at least concerns the subject matter, so a
suspicious consumer might question it. A serialization change has nothing to
do with rates, so there is no question a consumer could think to ask.

The layering that follows: **storage holds what the upstream said, and
producer annotations are attached at serve time.**
