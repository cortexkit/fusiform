# Finding: `cortexkit-model-catalog` parses every context-pricing tier threshold as 0

Found 2026-08-11 while measuring the models.dev payload for fusiform's
normalizer. The affected crate lives in the `commons` repository, owned by the
`subc` seat; the affected consumer is `astrocyte`. Both were notified the same
day and the fix shipped in `commons` — see Status below.

Recorded here rather than only in `commons` because the reasoning is reusable:
the defect class, the root cause, and the residuals found in the fix are all
things fusiform's own normalizer must avoid.

Severity: **latent, not active.** No charge is wrong *because of this defect*:
the corrupted value is persisted in astrocyte's store but never read back on
the pricing path. It becomes a live mispricing the moment anyone wires tier
selection.

That is narrower than "no money is wrong." Astrocyte separately under-prices
large-context requests on all 320 tiered models, because it prices from flat
rates and ignores tiers entirely — a real gap, but a different one, and not
caused by this. The two are connected only in that the obvious fix for the gap
is the thing this defect makes dangerous.

**Status: fixed at source the same day** — commons `ffdd06a` (threshold read
from `tier.tier.size`, absent threshold is a loud error) and `528b680` (tier
dimension gated on `tier.type == "context"`, refusal names the row).
`cortexkit-model-catalog` 0.1.0 → 0.2.0. Both verified independently by
re-probe, see "After the fix" below. Astrocyte's 251 stored rows are **not**
healed by the fix; that migration is tracked separately on ASTRO's side, whose
standing rule until then is that no pricing path may read `tiers_json`.

## The defect

`commons/crates/cortexkit-model-catalog/src/lib.rs:276-288` reads a tier's
context threshold from two keys:

```rust
min_context: tier
    .get("context_over")
    .or_else(|| tier.get("min_context"))
    .and_then(Value::as_u64)
    .unwrap_or(0),
```

Neither key exists in models.dev. Measured against the live payload
(2026-08-11, 3,628,293 bytes):

```
"context_over_200k": 288 occurrences   ← a different key, on cost, not on a tier
bare "context_over":   0 occurrences
"min_context":         0 occurrences
```

The real threshold is nested one level deeper, at `tier.tier.size`:

```json
{
  "input": 4, "output": 18, "cache_read": 0.4,
  "tier": { "type": "context", "size": 200000 }
}
```

Measured tier-object key frequency across all 335 tier rows: `tier` 335,
`input` 335, `output` 335, `cache_read` 303, `cache_write` 127,
`input_audio` 4. `tier.type` is `"context"` on all 335; `tier.size` is
present on all 335.

So `unwrap_or(0)` fires on every tier, silently.

## Confirmed by running the shipped parser against live data

Not inferred from reading — executed. A probe binary depending on the crate by
path, parsing today's `api.json`:

```
providers parsed: 183
models with tiers: 320
tier rows with min_context == 0 : 335
tier rows with min_context  > 0 : 0
  302ai/claude-opus-4-7 base_input=Some(5000000000) tiers=[(0, Some(10000000000))]
  302ai/gpt-5.4         base_input=Some(2500000000) tiers=[(0, Some(5000000000))]
  302ai/gpt-5.4-pro     base_input=Some(30000000000) tiers=[(0, Some(60000000000))]
```

Every one of the 335 tier rows carries threshold 0. `claude-opus-4-7`'s base
input rate is $5/Mtok and its over-200k rate is $10/Mtok; parsed, the $10 tier
claims to apply from context 0.

The parse itself succeeds — 183/183 providers, no error — which is what makes
this silent. `tiers.sort_by_key(|t| t.min_context)` then sorts a list of
identical zeros, so even the ordering invariant the code documents is vacuous.

## Confirmed in live consumer state

Astrocyte's store, `~/.local/share/cortexkit/astrocyte/cortexkit/astrocyte/store.db`:

```
price_snapshot: 5297 rows, 251 with tiers_json, 1 distinct observed_at_ms

302ai|claude-opus-4-7|[{"cache_read":1000000000,"cache_write":12500000000,
                        "input":10000000000,"min_context":0,"output":37500000000}]
```

All 251 rows with `tiers_json` contain `"min_context":0`. The corruption is
durable, not transient.

## Why no money is wrong today

Astrocyte writes `tiers_json` in `catalog_ingest.rs:89-108` and never reads it
back. `store_snapshot_candidates` (`astrocyte-core/src/lib.rs:37-42`) selects
only the five flat rate columns, `ResolvedPrice`/`PriceRow`
(`pricing.rs:33-45`) carry no tier fields, and `cost_for_usage`
(`pricing.rs:162-186`) prices exclusively from the flat rates.

So astrocyte currently prices all 320 tiered models at their base rate
regardless of prompt size. That is a *separate* under-pricing gap — real, and
astrocyte's to weigh — but it is not caused by this defect. This defect is the
landmine underneath it: wiring tier selection against the stored data would
apply every model's *higher* over-threshold rate to *every* request, because
each tier claims to start at context 0.

Broca is unaffected. `broca-catalog/src/lib.rs:641-665` reads the threshold
from the correct path and drops tiers that lack it:

```rust
let min_context = t.get("tier").and_then(|ti| ti.get("size")).and_then(Value::as_u64)?;
```

Two independent implementations of the same parse, one correct, one not. The
argument that chartered fusiform (`docs/charter.md`) was that three copies of
the same upstream knowledge already existed in the fleet and each rots on its
own schedule; this is that argument as a concrete defect rather than a
maintenance theory.

## Root cause: a fixture that cannot express the failure

The crate's tier test passes:

```rust
// lib.rs:504-511
#[test]
fn tiers_parse_sorted() {
    let doc = CatalogDoc::parse(snapshot()).unwrap();
    let (_, model) = doc.model("somehost", "tiered").unwrap();
    assert_eq!(model.cost.tiers.len(), 1);
    assert_eq!(model.cost.tiers[0].min_context, 200_000);
```

It passes because its hand-written fixture (`lib.rs:472`) supplies
`{ "context_over": 200000, ... }` — a key shape that has never appeared in
models.dev. The assertion is real and non-vacuous; the input is fiction. The
test proves the parser can read a threshold from a key the upstream does not
emit.

The fleet's verification method (subconscious
`docs/hunting-loop-briefing.md`) holds that **produced-output fixtures must be
minted by the real producer**, because a hand-written fixture encodes its
author's misunderstanding. This is that rule's cleanest specimen: fixture and parser
were authored from one wrong belief in one commit, so a non-vacuous assertion
certified the defect.

Quantified afterwards by mutation, during the fix (`ffdd06a`): with the
defective `unwrap_or(0)` restored, the two newly added tests redden while
`tiers_parse_sorted` stays green **even with a corrected fixture** — because a
correct fixture parses correctly under both the broken and the fixed parser.
The test was not weak; it was structurally incapable of distinguishing the two
implementations in either fixture state. Only a test pinning the *refusal*
direction can. To reproduce: restore the `unwrap_or(0)` line in
`parse_cost` and run the crate's test suite.

Corollary worth carrying: a fixture and the code it exercises must not have
the same author in the same commit without a measured payload in between.

The `context_over` spelling appears nowhere in git history as a real upstream
key; it was introduced with the crate at `eb1ae99` (2026-07-18) alongside the
fixture that validates it. Both the July-19 snapshot in astrocyte's data
directory (258 tier rows) and broca's July-24 vendored snapshot (261 tier
rows) use `tier.tier.size`, so the shape has been consistent for at least the
whole life of the crate. This was never correct.

## Recommended fix (as filed; all four adopted in `ffdd06a` + `528b680`)

1. Read the threshold from `tier.tier.size`, gated on `tier.tier.type ==
   "context"` so a future non-context tier type cannot be misread as one.
2. Make an absent threshold a **loud** outcome, not `0`. Either drop the tier
   (broca's choice) or return a parse error; silently defaulting a pricing
   threshold to zero is the same class of failure as defaulting a missing rate
   to free, which this crate correctly refuses to do everywhere else.
3. Replace the hand-written tier fixture with an excerpt copied verbatim from a
   real snapshot, and add a test that parses a real snapshot and asserts at
   least one tier has a nonzero threshold — a check that fails if the upstream
   shape moves again.
4. Decide what to do with `context_over_200k` (288 models). It is a second,
   older encoding of the same fact and is ignored entirely by both existing
   parsers. All 288 rows carrying it also carry `tiers`, so ignoring it loses
   nothing today — but that co-occurrence is an upstream convention, not a
   guarantee. (Fusiform's own normalizer, when built, will treat a
   `context_over_200k` without a matching tier as an error rather than a
   silent drop; see `docs/design/schema-and-store.md`. That is a fusiform
   decision, not a recommendation for this crate.)

## After the fix

The same probe, re-run against `528b680` on the same live payload:

```
providers parsed: 183
models with tiers: 320
tier rows with min_context == 0 : 0
tier rows with min_context  > 0 : 335
  302ai/claude-opus-4-7 base_input=Some(5000000000) tiers=[(200000, Some(10000000000))]
  302ai/gpt-5.4         base_input=Some(2500000000) tiers=[(272000, Some(5000000000))]
```

Thresholds now match what the payload states.

### Two residual findings, from probing the fix rather than reading it

A fix is an instrument too, and earns the same probing as the thing it
replaced. Both were found against `ffdd06a` and closed in `528b680`.

**The tier `type` discriminator was still unchecked.** `ffdd06a` read
`tier.tier.size` without looking at `tier.tier.type`. On today's payload every
tier is a context tier (`type == "context"` on 335/335 rows), so no live row
was misread — but the parser had no way to tell, and a non-context tier was
silently reinterpreted:

```rust
"tiers": [ { "tier": { "type": "images", "size": 1000 }, "input": 2.5 } ]
```
```
ACCEPTED a non-context tier type: min_context=1000 input=Some(2500000000)
```

A per-image tier read as "context ≥ 1000 tokens" — the same failure class one
level up: a field whose meaning is assumed rather than verified. With 164
image-output, 75 audio and 64 video models already in this upstream, a
non-token tier type is a plausible near-term addition. `min_context` is a
**claim that the dimension is context**, so verifying the dimension is part of
the threshold's meaning, not decoration.

**`MissingTierThreshold` named no row.** Every neighbouring variant carries
`{ provider, model, field, value }`; this one rendered as a bare sentence.
Since the failure is fatal to the whole parse, an operator got a rejected
3.6 MB snapshot and no pointer to the offending row.

The general audit form, worth more than the instance:

> When one variant in an error enum lacks the context its siblings carry, the
> omission is usually accident, not design.

After `528b680`:

```
refused non-context tier: catalog pricing tier on p/m lacks a verifiable
  context threshold (tier.type/tier.size)
```

## What fusiform takes from this

- The normalizer parses `tier.tier.size` and treats a missing or unrecognized
  tier discriminator as unpriced with `unknown charge basis`, never as a
  default threshold.
- Fixtures are excerpted from measured payloads, never hand-written. Every
  parser test carries the provenance of the snapshot it was cut from.
- A parse that silently substitutes a default for a missing pricing field is a
  bug by construction. Absent must be representable, and the type must force
  the caller to handle it.
- Two implementations of one parse drift, and the drift is invisible until
  someone measures. That is the charter's argument, now with a receipt — with
  the counterweight that sharing centralizes *correction* equally: one commons
  fix propagates to every future consumer at once, which the private-parser
  world structurally cannot do. What this indicts is fixture provenance, not
  sharing.
- A fix is an instrument. Probe it the way you probed what it replaced.
