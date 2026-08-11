# Fusiform — schema and store, design note 1

Status: **draft for review** by SUBC, BROCA, ASTRO. No code exists. Every
number cited here was measured, not recalled; the measurements live in
`docs/upstream-models-dev-measured.md` and are reproducible.

BROCA's consumed field set, envelope shape, ordering rule, bootstrap posture,
and payload-boundary decision are settled (2026-08-11) and folded in below.
ASTRO's pricing contract is settled the same day. What remains open is in §11.

## 0. The rule the rest follows from — and its correct boundary

> A consumer's behavior must never be silently changed by an upstream edit.

Four constraints that arrived from four unrelated directions are the same
rule:

- **Wire-family resolution stays with consumers** (charter). An upstream edit
  to a renderer hint must not re-route request bytes.
- **Never convert currency** (ASTRO). A producer that converts stamps an
  invented rate with producer authority.
- **Never imply a charge basis** (ASTRO). Calling video-seconds "tokens" makes
  a consumer's arithmetic depend on a convention the producer did not state.
- **Key order, whitespace, and float rendering are upstream edits** (measured),
  so change detection must be immune to them.

### Where fusiform's authority stops — corrected by BROCA

An earlier draft of this note claimed: *"if my payload can change a consumer's
request bytes, my design is wrong."* That is false, and believing it would
have hidden the real risk rather than removed it.

BROCA's `resolve_frozen` (`broca-provider/src/families/mod.rs:227`) gates the
reasoning policy on the model's capability bit:

```rust
reasoning: if model.capabilities.reasoning {
    provider.quirks.reasoning.clone()
} else {
    ReasoningPolicy::None
},
```

The comment reads: *"Provider quirks describe the family's reasoning wire
shape, but the selected model remains the authority on whether that shape is
legal at all."* So a boolean fusiform serves decides whether a `thinking`
block appears in an outbound request body. Three served fields have byte
consequences, not zero.

The correct, narrower invariant — BROCA's formulation, adopted verbatim:

> **Fusiform may never pick the RENDERER — family, endpoint, auth shaping,
> quirks. Fusiform may describe MODEL CAPACITY, and capacity legitimately
> shapes bytes.**

This is testable where the slogan was not, and it makes the quarantine set
(§6) a classification rather than an intuition.

Everything below is an application of these rules. Where a design choice is
arbitrary, it is marked as such.

## 1. Identity

### Model identity is the pair, never the id

Measured: 6,253 model rows carry only 2,957 distinct ids.
`openai/gpt-oss-120b` appears under 28 different providers, `glm-5.2` under
26 — at different prices. Anything keyed on a bare model id fuses distinct
offerings.

```
model identity  = (source, provider_id, model_id)
```

`source` is in the key because a second upstream describing the same model is
a different claim, not an overwrite. v1 has one source; the key does not
change when the second arrives.

### Source identity is declared, not free text

```rust
/// A declared upstream. Adding one is a code change, deliberately:
/// a source's identity carries policy weight (does it publish real
/// effective dates? what currency does it state?) and must not be
/// mintable from data.
pub enum SourceId {
    ModelsDev,
    /// The compile-time bootstrap snapshot. Never a fetch; see §7.
    Seed,
}
```

## 2. Observation vs. era — the distinction the whole store rests on

Two different facts, two different tables. Conflating them is how a catalog
starts lying about freshness.

- An **observation** is "I looked at source S at time T and this is what
  happened." Every poll produces one, including failures and 304s.
- An **era** is "source S asserted value V for fact F, starting at boundary
  B." Only a *change* produces one.

### Why every poll is recorded, including the ones that changed nothing

ASTRO's requirement is point-in-time rate lookup. Mine is that an era boundary
derived from polling is an **observation boundary**, not a provider
announcement — models.dev publishes no effective dates, so the change happened
somewhere between my previous look and this one.

A consumer can only bound that skew if it knows both edges. So:

```
observation(seq, source, started_at, outcome, ...)
    outcome ∈ { Changed(snapshot_seq), Unchanged, NotModified304, Failed(class) }
```

A **failed** poll observed nothing and must never narrow a window. A 304 and a
same-hash 200 both *confirm* the current values and legitimately narrow it.
That is absent / empty / unknown as three states, applied to polling: a poll
that failed is not a poll that returned nothing.

Freshness is therefore a property of the **source's observation stream**, not
a column on an era row. It is joined at read time and cannot drift from the
row it describes.

### The invariant that makes that join sound

Freshness-by-source-stream only works if a successful observation confirms
*every* live fact from that source. So absence must be a written fact:

> **Invariant T (tombstone completeness).** For every successful observation
> of source S, every model previously live under S either appears in that
> observation, or has a tombstone era opened at it.

A model vanishing from the upstream writes an era whose value is
`Absent { reason }`, not a deleted row. This is the 319-retired-models case
represented in the data plane instead of inferred from a gap.

Test shape: construct two observations where a model disappears in the second,
assert a tombstone era exists at the second, and assert a point-in-time query
before it still returns the live value. A store that merely dropped the row
passes neither half.

## 3. Eras are pure append, never mutated

No `valid_to` column. Closing an interval means editing a historical row, and
a store whose history can be edited is a store whose history can be edited by
a bug.

```
the fact in force at T = the newest era with boundary_at <= T
```

Identical in shape to astrocyte's `select_effective`
(`astrocyte-core/src/pricing.rs:204-212`), deliberately — the semantics a
consumer already implements should not need a second mental model.

An era row:

| Column | Meaning |
| --- | --- |
| `source` | which upstream asserted this |
| `boundary_at` | when this value became the one in force |
| `boundary_kind` | how that instant was determined — see below |
| `prior_observation_at` | the other edge of the window; NULL when `boundary_kind` is not `Observed` |
| `observation_seq` | the observation that opened it (provenance, joins to hashes) |
| … | the fact itself |

```rust
pub enum BoundaryKind {
    /// The value differed between two successive observations. The change
    /// happened somewhere in (prior_observation_at, boundary_at].
    Observed,
    /// The source stated an effective date. boundary_at is the source's
    /// claim, not ours. No source does this today.
    Asserted,
    /// Bootstrapped from the embedded seed (§7). Not evidence of anything
    /// about the upstream's timeline.
    Seed,
    /// Fusiform misread the upstream and is correcting its own record. The
    /// UPSTREAM DID NOT MOVE. See §3.1.
    Corrected(Correction),
}
```

### 3.1 `Corrected`: a correction is not a change

An append-only store cannot express *"what I recorded then was wrong"* without
a mechanism for it, and without one a correction is indistinguishable from an
upstream change. A new era saying `min_context: 200000` reads as "the
threshold moved on 11 August" when the truth is "the threshold was always
200000 and fusiform recorded it wrong." The history survives with its shape
intact and its meaning corrupted — and it passes every "does the current value
match" check, which is what makes it dangerous.

This is not hypothetical. The commons tier defect
(`docs/findings/2026-08-11-commons-tier-threshold.md`) left 251 durably wrong
rows in a store that is append-only by design; fixing the parser stops the
bleeding and heals nothing.

A `Corrected` boundary must serve two different consumers of the same fact:

- **Fusiform's reader** needs it to say *the upstream did not move* — so a
  correction never feeds skew arithmetic and never generates a change
  notification.
- **ASTRO's ledger** needs something harsher and narrower: *charges priced
  against the pre-fix era were computed from a value that cannot be defended.*
  A correction partitions their ledger into charges they can stand behind and
  charges they can only explain.

That second use requires **extent, not just kind**. A bare annotation is a
note; an auditable correction says which fields were wrong over which prior
interval, so a consumer can mechanically select every fact it derived from the
bad region instead of eyeballing dates:

```rust
pub struct Correction {
    /// Which facts the correction touches.
    pub fields: Vec<FieldId>,
    /// The prior interval whose recorded values were wrong. A consumer
    /// queries its own facts over this window to find what it derived
    /// from the bad region.
    pub affected_from: Timestamp,
    pub affected_until: Timestamp,
    /// Why the record was wrong. Free text is not enough for an audit to
    /// be repeatable, so this names the defect record.
    pub reason: CorrectionReason,
}
```

The first real use of this on ASTRO's side is empty today — tier data was
never read on their pricing path, so the partition is currently zero charges.
Designing it now, while the stakes are zero, is the point.

A correction never narrows an observation window and never rings a doorbell.
It is fusiform admitting a defect, and admitting a defect is not news about
the upstream.

**`prior_observation_at` is the field that makes the window arithmetic rather
than merely disclosed.** An era saying "observed at T, previously observed at
T−24h" lets a consumer bound its exposure by subtraction. A bare timestamp
asks them to trust a cadence they cannot see.

Stated as a rule, because both directions are lies:

> A source that publishes real effective dates must not be degraded to
> fusiform's observation cadence, and a source polled blind must never be
> dressed up as if it published dates.

### An unchanged value does not open an era

If the first real fetch agrees with the seed, no era is written — appending
one would assert a change that did not happen. The era keeps
`boundary_kind: Seed`, which reads correctly: *this value has never been
observed to change since bootstrap*. Freshness comes from the observation
stream, so nothing is stale about it.

## 4. Rates

### A rate is identified by what it charges for

```
rate identity = (model identity, currency, basis_key)
```

`basis_key` is a canonical, deterministic serialization of the charge basis
and its conditions. Two equal bases must produce equal keys and two different
bases must not — a property with a test that fails if the canonicalizer drifts.

```rust
pub enum ChargeBasis {
    PerMillionTokens { class: TokenClass },
    PerImage { width: u32, height: u32, quality: QualityTier },
    PerSecond,
    PerMinute,
    PerCharacter,
    PerRequest,
}

pub enum TokenClass { Input, Output, CacheRead, CacheWrite, Reasoning }
```

Closed enums, not strings. A consumer must be able to **fail** on a basis it
does not understand rather than guess — and the failure already has a name in
ASTRO's vocabulary: `unknown charge basis`. That variant existed before either
of us knew it would carry non-LLM billing.

The five token classes are the LLM special case, not the general shape. Video
bills per second, audio per minute, images per image at a size and quality.
Fusing those into "tokens" is the same failure as fusing currencies.

### Conditions are part of the key, and measurement forced this

320 models price by prompt size. The rate depends on how large the request
was, so a flat `(model, token_class)` key cannot resolve a rate:

```json
{ "input": 2, "output": 12,
  "tiers": [{ "input": 4, "output": 18,
              "tier": { "type": "context", "size": 200000 } }] }
```

```rust
pub enum RateCondition {
    Always,
    MinContextTokens(u64),
}
```

The tier discriminator is **verified, never assumed**. `tier.tier.type` is
`"context"` on 335/335 rows today, and a parser that reads `tier.tier.size`
without checking `type` silently reads a hypothetical `{"type": "images",
"size": 1000}` tier as "context ≥ 1000 tokens" — confirmed by probing the
fixed commons parser, which accepts exactly that. With 164 image-output, 75
audio and 64 video models already in this upstream, a non-token tier type is a
plausible near-term addition rather than a thought experiment. An unrecognized
tier type is unpriced with `unknown charge basis`, never coerced.

This settles an open question with ASTRO: a descriptive property participates
in rate selection **already**, not hypothetically. It also means a pricing
consumer must never have to join capability rows to price rows to select a
rate — every discriminator a rate needs travels **in the rate's own key**. If
ASTRO ever has to read a capability field to price something, this schema
failed.

Measured shapes: 305 models carry one tier, 15 carry two, thresholds ∈
{32k, 128k, 200k, 256k, 262144, 272k, 512k}, `tier.type == "context"` on all
335. A tier whose discriminator is missing or unrecognized is **not** given a
default threshold — see the defect in
`docs/findings/2026-08-11-commons-tier-threshold.md`, which is exactly this
mistake shipped.

`context_over_200k` (288 models) is an older second encoding of the same fact.
Fusiform normalizes to the general tier form and never serves both. It
currently only ever co-occurs with `tiers`; that is an upstream convention,
not a guarantee, so a `context_over_200k` without a matching tier is a
normalizer error, not a silent drop.

### Money is integers, and the unit is stated

```rust
pub struct Amount {
    /// Integer count of minor units. Never a float, anywhere, ever.
    pub units: i64,
    /// units × 10^(-exponent) of `currency`. Stated, never assumed.
    pub exponent: u8,
    pub currency: CurrencyCode,
}
```

Floats exist only at the JSON parse boundary where the upstream forces them,
and never survive normalization. Measured justification: the same cost key
arrives as both JSON int and float (`input` is an int on 1,620 rows and a
float on 4,213 — a parser typed to one fails on 26% of rows), and 151 numbers
carry ≥6 decimal places including confirmed IEEE-754 artifacts
(`0.024999999999999998`, `0.39999999999999997`, `1.5999999999999999`).

Parse rules, adopted wholesale from `cortexkit-model-catalog`'s money
doctrine because it encodes real incidents: scale via decimal string, round
half-even at the money resolution, reject a nonzero value that would round to
zero, reject negatives, checked arithmetic throughout. Those rules are right.
Their tier parsing is not, and fusiform does not inherit it.

### Currency: stated, assumed by named policy, or unpriced

models.dev has **no currency field anywhere**. USD is convention. Since a
producer must not launder an assumption as a fact:

```rust
pub enum UnitProvenance {
    /// The source stated it.
    Stated,
    /// Fusiform applied a named, auditable policy. The policy id is
    /// carried so "why does the catalog say USD" has an answer.
    AssumedByPolicy(PolicyId),
    /// No statement and no policy covers it. The rate is unpriced.
    Unknown,
}
```

So models.dev rates are served as USD with
`AssumedByPolicy("models-dev-usd-v1")`, never as a stated fact. When QTA's
CNY-denominated providers reach a catalog, `Unknown` is what stops them being
silently priced in dollars.

The same mechanism covers the audio keys. `input_audio` sits in the same flat
namespace as `input` with no unit distinguishing it:

```json
{ "input": 1.5, "output": 9, "cache_read": 0.15, "input_audio": 1.5 }
```

Per audio token? Per second? Per minute? Unstated. Absent a per-provider
policy, that rate is served **unpriced with `unknown charge basis`** rather
than guessed into a token rate.

### Absent, zero, unknown — and retired

Absent, zero, and unknown are all statements about a model the upstream **still
describes**. A model the upstream *stopped* describing is a fourth fact on a
different axis, and it must never be inferred from a gap.

Measured: BROCA's vendored snapshot (2026-07-24) held 5,823 models; today's
live payload holds 6,253. **+821 added, −391 removed** — including
`anthropic/claude-opus-4-1`, `azure/gpt-4`, `azure/deepseek-v3.1`. That is the
319-retired-models event again, larger, sitting in the upstream now.

The two consumers lose different things to a retirement. BROCA's loss is loud:
a vanished model breaks a live run, which is what their swap guard exists to
catch. ASTRO's is quiet: they hold priced facts for models that no longer
exist upstream, and those facts stay *correct* — the usage happened, the rate
was real at the time — but become permanently unverifiable against a current
catalog. A tombstone with a boundary is what keeps that from reading as a
coverage gap when it is actually a retirement.

Hence Invariant T (§2): absence is written down with a reason and a boundary,
never left as silence. A point-in-time query before the tombstone returns the
live value; after it, a stated absence. Neither is inferred.

### Absent, zero, and unknown are three states

```rust
pub enum RateValue {
    Priced(Amount),
    /// The source stated exactly zero. Not "free" — a stated zero. Whether
    /// a zero is a real price is the consumer's policy, not ours.
    StatedZero,
    Unpriced(UnpricedReason),
}

/// ASTRO's enum, adopted rather than paralleled. Their producer-side-
/// irrelevant variants (degraded pricing time, arithmetic out of range)
/// are structurally not ours: a producer has no pricing instant and no
/// ledger arithmetic.
pub enum UnpricedReason { MissingRate, NoCatalogCoverage, UnknownChargeBasis }
```

Measured: 420 models carry no `cost` object at all, and 1,423 cost entries are
exactly `0` (including whole provider families). Some zeros are genuinely
free; some are certainly "not published". The upstream cannot distinguish them
and neither can fusiform from the payload, so fusiform reports what was said
and surfaces the counts loudly on the operator surface rather than resolving
them.

## 5. Modality is carried from day one

Schema, not content: the schema carries modality so non-LLM rows land without
a migration, and v1 ships no non-LLM source.

Measured vocabulary — and note these are not hypothetical:

- input: `text` 6,223, `image` 3,350, `pdf` 1,320, `video` 797, `audio` 468
- output: `text` 6,067, `image` 164, `audio` 75, `video` 64, `pdf` 2

303 models in this LLM-shaped upstream already emit non-text, and their
pricing cannot express what they bill. `stabilityai/stablediffusionxl` carries
`"limit": {"context": 200, "output": 0}` — a token limit of zero on a model
that does not emit tokens. That is the charter's domain argument as a measured
fact: a second source is required for real non-LLM coverage, and the schema
must carry charge basis explicitly before it arrives.

Unknown modality values from a future upstream are preserved and served as
`Other(String)`, never dropped and never coerced into a known variant.

## 5.1 The byte-affecting three

BROCA's consumed field set, read from their source rather than described.
Three fields shape request bytes:

| Field | Consequence in BROCA |
| --- | --- |
| `limits.context` | the transform's pressure signal |
| `limits.output` | rendered into the request body as `max_output_tokens` |
| `capabilities.reasoning` | gates `ReasoningPolicy` (`families/mod.rs:227`) |

Everything else BROCA reads is **advisory**: `id`, `display_name`, `family`,
`release_date`, `status`. Their `raw` passthrough is ingestion-only and never
touched on the render path.

The three get a different class of treatment, because their failure modes are
silent rather than loud. `reasoning: false` on a reasoning model strips
thinking from every request. A `context` limit that is too large lets the
transform overfill. Neither errors; both quietly produce worse output.

1. **Provenance per field, not per row.** Each carries whether its value came
   from the upstream, from a human override, or is absent — and absent stays
   distinguishable from zero. A field that fails silently must be able to say
   where it came from.
2. **A change to one is a distinct event in the diff**, surfaced more loudly
   than a display-name edit. A consumer's guard should not have to grep a flat
   change list to find the three that matter.
3. **No defaults, ever.** Absent is representable and the type forces the
   caller to handle it. The commons defect is what `unwrap_or(0)` does to a
   pricing field; a context limit deserves the same refusal.

### What fusiform must never claim

Whether a model *serves* is not a catalog fact. BROCA's
`in_use_models_still_serve.rs` composes three inputs — catalog lookup with
suffix-stripping and the overlay's `remove` verb, the auth method, and
`family_override` derived from it — and a raw presence check is not a
resolution verdict. Reproduced: screening BROCA's 39 in-use pairs by presence
reports six false positives (`openai/gpt-5.6-luna-fast`,
`google/antigravity-gemini-3.5-flash`, and four more), the same six their doc
comment records, because those ids resolve through suffix-stripping and family
override rather than catalog presence.

> Fusiform's diff may say **"this pair left the upstream."** It may never say
> "this model is served" or "this model was un-served." That is a serve-layer
> composition and it belongs to the consumer.

## 6. Quarantine: what fusiform parses and refuses to serve

Two fields in the measured payload determine how a request is *rendered*.

`provider.npm`, overridden per model on 229 rows — under provider
`opencode-go`, model `qwen3.7-plus` declares `{"npm": "@ai-sdk/anthropic"}`.
That is renderer selection. And `experimental` on 38 rows carries literal
request bytes:

```json
"provider": { "body": { "speed": "fast" },
              "headers": { "anthropic-beta": "fast-mode-2026-02-01" } }
```

Headers and body parameters, editable by an upstream contributor, that would
land verbatim in an outbound provider request.

> **Rule Q.** Fusiform parses these, records them in raw provenance, and never
> serves them as catalog facts. They are evidence about the upstream, not
> instructions to a consumer.

Enforced by a **negative** test over the real payload: the served type
contains no key from the quarantine set, and the test fails if a future field
is added to the served shape without classification. A test that only checks
the fields we do serve cannot see this.

`experimental.modes.*.cost` embeds a second pricing plane inside a
wire-affecting field. v1 serves a model at its base rate and records modes in
raw provenance only.

If BROCA ever wants to reconcile their hand-tabled `family.rs` against the
upstream's claim, that is a **report for a human**, never an input to a swap.
Fusiform can produce it; it must never be wired to anything automatic.

## 7. Bootstrap, and what a seed is allowed to claim

The carriage bar: a fresh install with no network comes up healthy.

BROCA keeps their own embedded seed permanently, and their reason is stronger
than the freshness argument: **they are a durability engine.** A session's
WAL, lease, and replay must come up on a machine with no network and no other
module running, or an operator cannot restart them to recover in-flight
sessions. Coupling their liveness to fusiform's would put fusiform's
availability inside their durability story — not a tradeoff for freshness, a
category error. Their embed stays, demoted to seed, and they log loudly when
serving from it so "fusiform has been down a week" is visible rather than
silent.

Fusiform's own seed follows the same logic for the same reason. The seed is an
embedded snapshot, and its eras are stamped `boundary_kind: Seed` — which says *this is where the store started*, not
*this is when the upstream changed*. A seed boundary must never feed skew
arithmetic and must never generate a change notification, because nothing
changed; a store came into existence.

"Demoted to seed-only the moment the first fetch lands" means: the seed stops
being a source of current facts, and remains as the recorded history of how
the store bootstrapped. Erasing it would erase the explanation for every era
that has not moved since.

## 8. Change detection: two hashes with different audiences

```
normalized_hash  — over the normalized, integer-valued document.
                   Drives diffs, eras, and every consumer notification.
raw_hash         — over the fetched bytes. Provenance and drift only.
                   Never reaches a consumer.
```

Split by **audience**, not by mechanism. A convention about which field to use
erodes; two fields with different destinations cannot be accidentally wired
together.

`raw_hash` moving while `normalized_hash` holds means the upstream changed
something fusiform does not model — genuinely valuable, since that is how
fusiform learns a field was added *before* a consumer needs it. It goes to the
operator CLI and nowhere else.

Measured support: two fetches 21 seconds apart were byte-identical
(`sha256 4fb6410c…`), the ETag matched across both, and a conditional GET
returned **304 with zero bytes**. So polling is nearly free: an unchanged poll
costs one round trip and no body, and a changed one costs ~355 KB gzipped.
That matters because poll interval *is* the width of every observation window
fusiform records — cadence is not a performance knob here, it is the precision
of the history.

## 9. Store

Managed SQLite via `cortexkit-store`, opened **after** daemon connection from
the HELLO_ACK descriptor (never self-keyed — astrocyte's live store is at
`astrocyte/cortexkit/astrocyte/store.db` because it self-keyed early, and the
path at `astrocyte/store.db` is a 0-byte decoy that misleads every audit,
including mine an hour ago).

Tables: `observation`, `snapshot`, `provider_era`, `model_era`, `rate_era`,
`raw_document`. Snapshot history as rows, not content-addressed blobs.

Sizing: 3.6 MB per fetch uncompressed, but eras only grow on change, so steady
state is one raw document per changed fetch plus a few thousand era rows.

### Ingest is one transaction

From astrocyte's gated background-loop spec: facts, cursor, and outbound work
commit **together**. If ingestion commits facts and advances its cursor before
recording what it owes downstream, a crash in between leaves a hole no later
poll can find, because the next poll starts after the committed range. That
defect started their whole campaign; inheriting the fix is free.

### Backup posture is load-bearing for audit, not for operation

Settled with ASTRO: losing fusiform's history must cost them *verifiability*,
never *explainability*. Their ledger records the rate it charged and enough
provenance to name the era it came from, so it stands alone.

The specific failure that would be invisible: **a restore that silently
collapses two eras into one.** Every current value still matches, every charge
still explains itself, and the audit is quietly wrong.

Restore test shape — a negative assertion against a plausible-looking success:
build a store with two distinct eras for one rate, capture, restore, and
assert (a) the boundary survived with its `boundary_kind` and
`prior_observation_at`, and (b) a point-in-time query *inside the earlier era*
returns the earlier value. A collapsed restore passes "the current rate
matches" and fails this.

Engram: whole-db capture, `restore-with-monotonic-fence` declared for the
observation sequence so a stale restore cannot rewind it.

## 10. Serve and push

### Two consumers, two envelope shapes, on purpose

| | BROCA | ASTRO |
| --- | --- | --- |
| Failure mode | a live run dies on an unknown model string | a charge is priced against a wrong rate |
| Worse | late | wrong |
| Envelope | **full snapshot + content hash** | **doorbell**: change identity only |
| Diff | carried, **advisory only** | the doorbell's content |
| Authority | consumer's swap guard | pull |
| Surface | capability plane | pricing plane, separate cadence |

BROCA takes the full normalized catalog on every push, for the reason a
diff-only push makes a missed push **corrupting rather than merely late**.
Fusiform computes the diff (it holds both snapshots; the consumer holds one)
and carries it as an advisory field — but BROCA's guard recomputes what it
needs from the full payload and does not trust fusiform's account for a
refusal decision. That is correct and fusiform should not argue: **a guard
that depends on the pusher's own story about what changed can be defeated by a
pusher bug**, which is the class of thing guards exist to survive.

### Pricing is a separate surface, settled from both ends

BROCA reads `cost` into a `CostSchedule` and nothing on their render or
admission path consumes it; their billing lane exports raw token counts with a
charge-basis label and deliberately ships zero pricing. Capability data
changes when a provider ships a model; pricing changes on a different clock
and carries effective-dated eras capability data does not have. Coupling them
would make BROCA a consumer of pricing churn they have no use for — and every
push they receive is a swap they must validate.

ASTRO reached the same split from the pricing side. Settled from both ends.

ASTRO's push carries which models changed, the catalog version, and a hash —
**nothing they parse into their ledger**. A value-carrying push would create a
second ingestion path, and a second path exercised rarely is a path whose bugs
surface during an incident. Push tells them to pull; one ingestion path, one
set of bugs, exercised every time.

> This asymmetry is deliberate and must not be tidied away. A future reader
> unifying a doorbell and a payload into one envelope would be reintroducing
> the second ingestion path.

### Ordering

BROCA has no catalog version or generation to adopt, so fusiform defines one
and BROCA enforces it: **a push whose version is `<=` the loaded one is
refused.** Their control surface has no dedup, so duplicates are assumed — the
version check makes a duplicate delivery structurally a no-op rather than
defended by luck.

### Payload boundary: shared representation, fusiform-owned envelope

Settled with BROCA: `cortexkit-model-catalog` owns the **representation**,
fusiform owns the **envelope**. Extending the shared crate for modality and
provenance would couple fusiform's schema evolution to every consumer's
release cycle — a field added for one consumer would move every other
consumer's crate.

Fusiform takes the crate's money doctrine, because it encodes real incidents:
decimal-string scaling, half-even rounding at the money resolution,
reject-nonzero-rounding-to-zero, the negative-rate guard, checked arithmetic
throughout. Fusiform does **not** take its tier parsing.

The boundary is pinned with a vendored golden fixture from day one — and, in
BROCA's sharper form, **a golden that a deliberate mutation must break.** A
shape assertion nobody has broken on purpose is just a differently-worded
value assertion.

Note for the record: the crate's header describes itself as the shared
representation "both consumers parse." Measured, no BROCA `Cargo.toml`
depends on it; only astrocyte does. The claim becomes true through this
contract, not before it.

### Multi-source precedence is deferred, not defaulted

With one source the question does not arise; every served row carries its
`source`, so the shape does not change when a second arrives. **Which source
wins for a model both describe is policy, and fusiform holds no policy.** The
decision is deferred explicitly and named as an open item rather than settled
by whichever normalizer happens to run last.

## 11. Open — not settled by this note

1. **Multi-source precedence** (§10). Which source wins for a model two
   upstreams describe is policy, and fusiform holds no policy. Deferred
   explicitly rather than settled by whichever normalizer runs last.
2. **Per-provider unit policies** for the audio rates and any future
   convention-implied unit. Each one is a named, auditable policy or the rate
   stays unpriced.
3. **Correction extent granularity** (§3.1). `FieldId` needs a concrete
   vocabulary that is stable enough for ASTRO to query against and narrow
   enough to be useful. Settled with ASTRO before the first correction, not
   during one.
4. **Cadence.** Poll interval is the width of every observation window, so it
   is a precision decision, not a performance one. A conditional GET costs one
   round trip and zero body bytes (measured), so the floor is politeness to
   the upstream rather than cost.

### Settled since the first draft

- BROCA's consumed field set (§5.1), envelope (full snapshot + advisory diff),
  ordering rule (monotonic version, `<=` refused), bootstrap posture
  (permanent embed, §7), and payload boundary (shared representation,
  fusiform envelope, §10).
- Pricing as a separate surface — agreed independently from both consumer
  sides.
- `Corrected` as a boundary kind carrying extent (§3.1), requested by ASTRO.

## 12. Verification stance

Adopted from the fleet's hunting-loop method, applied to this design:

- **Produced-output fixtures must be minted by the real producer** (the
  fleet's standing form; stronger than "excerpted from a measured payload"
  because it also covers another module's response envelope, not just an
  upstream document). A hand-written fixture encodes its author's
  misunderstanding. The commons defect is that rule's cleanest specimen:
  fixture and parser authored from one wrong belief in one commit, so a
  non-vacuous assertion certified the bug.

  Quantified by SUBC's mutation run: the old `tiers_parse_sorted` would have
  stayed green **even after its fixture was corrected**, because a correct
  fixture parses correctly under both the broken and fixed parsers. The test
  was not weak — it was incapable of distinguishing the two implementations in
  either fixture state.

  Corollary, from ASTRO: a fixture and the code it exercises must not have the
  same author in the same commit without a measured payload in between.
- Every check ships with a proof it can fail. The restore test, Rule Q's
  negative test, and tombstone completeness are all shaped so that the
  plausible-looking wrong implementation fails them.
- Absent, empty, and unknown stay three states — in polling outcomes, in
  rates, in modality, in tier discriminators. Every place this schema could
  substitute a default for a missing value is a place a bug hides.
- The first instrument is the least trustworthy thing in the module. The
  commons defect was found by *executing* a parser against real bytes, one
  nesting level below where anyone was reading — and it fell out of an
  unrelated question (how the parser handled the 26% int/float split). The
  discipline that found it was "run it on real data," not "read it
  carefully." The plan of record had been to read the crate.

- **A residual finding from probing the fix itself:** the corrected commons
  parser reads `tier.tier.size` without checking `tier.tier.type`, so a
  hypothetical `{"type": "images", "size": 1000}` tier is accepted as "context
  ≥ 1000 tokens." Confirmed by probe, reported to SUBC. Fusiform verifies the
  discriminator (§4). The lesson generalizes: **a fix is an instrument too,
  and it earns the same probing as the thing it replaced.**
