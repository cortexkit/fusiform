# Fusiform — schema and store, design note 1

Status: **implemented in part**, last reconciled 2026-08-11 against commit
`1ff33f9`. This note was written before any code existed and settled the data
model first; most of what it specifies now runs. **Read §0.1 before citing any
sentence here as a fact about the system** — the same sentence is a measured
fact in one section and an unbuilt intention in another, and nothing in the
prose distinguishes them.

### Who the parties are

Fusiform is a supervised module of the CortexKit fleet (see `docs/charter.md`
for its mission, v1 scope, and the constraints settled at chartering). Three
other modules appear throughout; each is an independent binary in its own
repository with its own owner:

| Name | Module | Relationship to fusiform |
| --- | --- | --- |
| **BROCA** | `broca` — the LLM run engine | first consumer; receives the capability catalog |
| **ASTRO** | `astrocyte` — AI spend metering | second consumer; receives the pricing plane |
| **SUBC** | `subc` — the daemon and supervisor | owns the module contract and the `commons` shared crates |

Where this note says a thing is "settled with" one of them, it means that
party is the owner of the constraint and agreed the design meets it. Their
repositories are the durable record; the reasoning is reproduced here so this
note stands alone.

### What is settled

BROCA's consumed field set (§5.1), envelope shape, ordering rule, bootstrap
posture, and the payload-boundary decision are settled. ASTRO's pricing
contract is settled. What remains open is in §11.

### Evidence

Upstream figures cited here were measured on 2026-08-11 and are recorded with
their reproduction method in `docs/upstream-models-dev-measured.md`, which is
the authoritative record for anything about the upstream's shape. Figures
about other repositories (BROCA's snapshot and source, astrocyte's store) name
their file and line inline so they can be re-checked in those repositories;
they cannot be verified from inside fusiform alone. All are dated observations
of systems that change, not constants.

## 0.1 What is built, and what is still specification

**This section replaces the note's original tense convention, which expired.**

The note opened by stating that fusiform had no code, making every sentence in
it unambiguously a specification. That convention held for about six hours.
Seventeen commits later the note predicted its own failure mode accurately
enough that the prediction is quoted below, so this is the announced
re-dating rather than a retrofit.

Verified against the working tree at `1ff33f9`, section by section:

| Section | State |
| --- | --- |
| §1 Identity | **built** — `ModelKey` is the (source, provider, model) triple |
| §2 Observation vs era | **built** — both tables, with the confirming-outcome split |
| §3 Eras are pure append | **built** — no `valid_to`, no `is_current`, no update path |
| §3.1 `Corrected` | **read side built, write side not.** A correction written by hand is honoured: `value_at` refuses inside its extent and names it, and `read_catalog` omits the fact. What has no producer is the code that DECIDES to write one. The earlier status here read "type built, never written", which was one claim covering two states — the type round-tripped AND every point-in-time read silently ignored corrections. Only the first half had been checked. |
| §4 Rates | **partially built** — `PerMillionTokens` and the tiered conditions are produced from real documents. `PerImage`, `PerSecond`, `PerMinute`, `PerCharacter` are declared types with no producer, because models.dev publishes no unit for them. |
| §5 Modality | **built** — carried, unknown values preserved |
| §5.1 Byte-affecting fields | **built** — the zero-limit rule and the quarantine |
| §6 Quarantine | **built** — parsed, flagged, never served; guarded by the fact-set test |
| §7 Bootstrap | **built** — embedded snapshot, `scripts/refresh-seed.sh`, verified on a fresh install |
| §8 Two hashes | **built** — split by audience, both recorded per observation |
| §9 Store | **built** — managed SQLite from the HELLO_ACK descriptor |
| §9 Engram enrollment | **built** — `crates/fusiform-module/data/engram-catalog.json`, validated with engram's own parser, installed by `scripts/install-enrollment.sh`. See `docs/backup-enrollment.md`. Not load-bearing for the version counter: the §9 text once claimed a `restore-with-monotonic-fence` here, that mechanism does not exist, and the counter is restore-invariant by construction instead (§10). |
| §10 Serve | **built** — `catalog.get`, `catalog.history`, `catalog.status` |
| §10 Push | **not built, and not buildable as specified** — see `docs/findings/2026-08-11-push-has-no-acknowledgement.md`. A push frame carries `corr: 0` and a module has no request frame, so the discriminated acknowledgement has no transport; and both consumer clients discard `FrameType::Push` outright. Envelope shapes, ordering and high-water re-sync remain specification; the acknowledgement needs a different design or a transport change that is not fusiform's to make. |
| §10 Payload boundary | **built.** The served types live in `fusiform-protocol` (this repo, not commons — settled 2026-08-12, see §10). Dependency tree pinned to serde-only by test; version discipline enforced in CI; golden fixtures of four real served payloads. What is NOT done is astrocyte consuming it, which is their switch on their schedule. |

Two entries deserve emphasis because they are the ones most likely to be cited
as facts: **no defect-detection path writes a `Corrected` era, and no push
exists.** A consumer
reasoning about how fusiform acknowledges a push is reasoning about a design.

### Why the original convention expired, in its own words

Retained because the mechanism it describes is what this section exists to
prevent, and it was written before the failure it predicts:

> That convention is unambiguous only while there is no implementation. Once
> code exists, "fusiform normalizes X" acquires **two truth conditions where it
> had one**, and nothing in the sentence says which was meant. The dangerous
> period is not when they disagree — it is the stretch beforehand when they
> agree, because that is when the habit of not checking forms, at no cost, and
> is in place by the time it matters.

This is the observed mechanism behind the two-versions-of-a-rule failure
described in §4: nobody chooses the second version. A rule is stated once when
it is true of an intent, the code later settles somewhere narrower, and the
sentence keeps being repeated because **a sentence has no way to notice.**

The cheap defence costs one word, and it belongs in messages across seams as
much as in documents: say **specified** or **measured** when it matters. A
reader holding a claim cannot see which file the convention lives in, and
"fusiform does X" is true under both readings while meaning different things.
The same ambiguity produced a retraction elsewhere in this note — a statement
about what a module *emits*, read as a statement about what the value *means*.
True sentence, truth condition in a different place than the reader assumed.

**Two words, not one — the marker needs a tense.** A third state hides between
"specified" and "measured": a claim that is accurately specified, accurately
remembered, and describes code not yet written. Nothing about it is false
except the tense, and **tense is exactly what a seam conversation strips.**
So: *specified-and-built* or *specified-and-pending*.

This is not hypothetical, and fusiform is downstream of an instance. A
consumer described a completion signal as its deletion authority — accurate to
their design, and their shipped code deletes unconditionally for any provider
appearing in a response, because the slice implementing it has not landed.
Both halves coherent, contradiction on a schedule rather than on a page.

The consequence for a producer is concrete: reasoning about how a consumer
handles a signal it does not yet honor produces analysis of failure modes that
cannot occur, while missing the one that can. **A design a consumer intends to
build is not a property of the system fusiform is integrating with.**

And it stays not-a-property **after** they build it, until they report it
landed. An adversarial review can narrow, defer, or restructure a specified
behavior between the description and the merge — the consumer above has had
four rounds reshape things they had described as settled. So:

> A consumer's specified behavior is a signal about direction, never a
> dependency fusiform may take. What converts it is **the consumer reporting a
> merge**, not fusiform reading their specification.

Same rule as the era-zero seed (§11), from the other direction: each party
commits only to what it can verify, and a specification is not something its
author can verify on a reader's behalf.

### Mark tense at the moment of speaking

The remedy is not a retroactive sweep. Fusiform's own case makes a sweep look
viable — everything it has said is uniformly `specified-and-pending`, so one
announcement at first implementation would re-date the lot. That uniformity is
an artifact of having no code, and it is temporary.

> **What actually happened, recorded for the next reader.** The sweep above
> (§0.1) *was* taken, at first implementation, exactly as this paragraph
> describes — and the paragraph is still right that it does not generalise. It
> worked here because the whole note had one tense and one author, and because
> the state of each section was established by running `grep` and `ls` against
> the working tree rather than by re-reading the prose.
>
> The finding worth carrying: two things this note describes in the present
> tense have no producer at all — no code decides to write a `Corrected` era
> (the read side that honours one is now built), and no push
> code exists. Both read as built to anyone skimming, because the surrounding
> sections describe behaviour that is.
>
> A first draft of this paragraph claimed four corrections and named the
> crate-version sequencing item as one of them. There were three changes, and
> that item was never examined. The claim was written to give a sentence a
> second example — which is the failure this whole section is about, committed
> inside the correction for it, in the same edit. Caught by checking rather
> than by re-reading, which is the only thing that catches it.
>
> The window closes now. From the next slice on, §0.1's table is what goes
> stale, one row at a time, with nothing announcing it.

A module with both shipped code and a pending specification has no such
uniformity: adjacent sentences in one message can be a measured fact, behavior
running since July, and behavior specified but unbuilt — same voice, same
tense, and **no reader can separate them, including the author later.** Such
ambiguity resolves per-claim and slice by slice, never all at once, so no
release history dates it.

Hence: mark at the moment of speaking, because **that is the only moment the
distinction is free.** Afterwards it costs a source read per claim, against
claims already scattered across threads — and a sweep done badly is worse than
an admitted gap.

## 0. The rule the rest follows from — and its correct boundary

> A consumer's behavior must never be silently changed by an upstream edit.

Four constraints that arrived from four unrelated directions are the same
rule:

- **Wire-family resolution stays with consumers** (`docs/charter.md`). An
  upstream edit to a renderer hint must not re-route request bytes. A *wire
  family* is the request-rendering dialect a provider speaks (Anthropic
  Messages, OpenAI Chat Completions, OpenAI Responses, and so on); picking one
  determines the exact bytes on the wire.
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
block appears in an outbound request body. Served fields have byte
consequences, not zero — two of them, enumerated in §5.1.

The correct, narrower invariant — BROCA's formulation, since the boundary
belongs to whoever owns the renderer:

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
  B." A *change* is the only thing that produces one **from polling** — an
  unchanged fetch appends nothing (§3).

Two boundary kinds are written by something other than a change, and both are
deliberate exceptions rather than an inconsistency in the rule above:
`Seed`, written once at bootstrap because a store coming into existence is not
an upstream event; and `Corrected`, written when fusiform is repairing its own
misreading, where the upstream demonstrably did *not* move. Both are defined
in §3. The rule that holds without exception is narrower and is the one that
matters: **an era is never written to record that fusiform looked and saw the
same thing.**

### Why every poll is recorded, including the ones that changed nothing

ASTRO's requirement is point-in-time rate lookup. Mine is that an era boundary
derived from polling is an **observation boundary**, not a provider
announcement — models.dev publishes no effective dates, so the change happened
somewhere between my previous look and this one.

A consumer can only bound that skew if it knows both edges. So:

```
observation(seq, source, started_at, outcome, ...)
    outcome ∈ { Changed(snapshot_seq), Unchanged, NotModified304, Failed(class) }
           snapshot_seq — the snapshot row this observation produced
           class        — the failure taxonomy (network, HTTP status, parse)
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
`Absent { reason }`, not a deleted row.

This represents in the data plane what would otherwise be inferred from a gap.
The motivating incident is recorded in `docs/charter.md`: one models.dev
refresh silently removed 319 retired models from BROCA's catalog and nothing
failed, which was noticed only because unrelated test fixtures happened to pin
ids that no longer existed. A deletion that leaves no trace is
indistinguishable from a model that was never there.

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

One wrinkle the plain selector does not express, and it is load-bearing:
**"what did the store say at T" and "what was true at T" are different
queries**, and the selector above answers only the first.

A `Corrected` era says the value in its `affected_from..affected_until`
interval was *recorded* wrong. Selecting by `boundary_at` alone returns the
corrected value for instants after the correction and the original wrong value
for instants inside the bad interval — correct as history, wrong as fact, with
nothing in the query distinguishing them.

So the read surface must not conflate them. A point-in-time lookup inside a
corrected interval **refuses** rather than returning a confident wrong answer:
the honest response is "the store recorded V here and that record is known
bad," never V.

This is the primary reason `Correction` carries an interval rather than a
timestamp — stronger than the reason it was originally requested for. It was
asked for so a consumer could *find* affected facts. It is also the only thing
that lets the read surface **refuse**, and without it a corrected era is
indistinguishable from a later change with the bad interval unmarked. An audit
aid is useful; a mechanism that stops a query answering the wrong question
confidently is structural.

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

`Timestamp` throughout this note is a millisecond instant in UTC;
`CorrectionReason` names the defect record that explains the correction (a
finding document under `docs/findings/`, so an audit is repeatable rather than
dependent on prose).

```rust
pub struct Correction {
    /// Which facts the correction touches. See FieldId below.
    pub fields: Vec<FieldId>,
    /// The prior interval whose recorded values were wrong. A consumer
    /// queries its own facts over this window to find what it derived
    /// from the bad region. affected_from is a LOWER BOUND — see below.
    pub affected_from: Timestamp,
    pub affected_until: Timestamp,
    /// Why the record was wrong. Free text is not enough for an audit to
    /// be repeatable, so this names the defect record.
    pub reason: CorrectionReason,
}
```

The first real use of this is empty today — the tier defect never reached a
pricing path, so its partition is zero charges. Designing it now, while the
stakes are zero, is the point.

A correction never narrows an observation window and never rings a doorbell.
It is fusiform admitting a defect, and admitting a defect is not news about
the upstream.

#### `affected_from` is a lower bound, and unknown means earliest

> **Rule.** `affected_from` is a lower bound. When the true start of the bad
> region is not known precisely, it goes to the **earliest plausible instant**,
> never to the best guess.

The asymmetry is not a matter of taste. An over-inclusive partition costs a
consumer review time. An under-inclusive one leaves bad facts outside a
partition that asserts they are fine — which is worse than no partition at
all, because it converts an unknown into a false clean bill.

In practice `affected_until` is known exactly (it is when the fix deployed and
fusiform stopped recording the bad value) while `affected_from` may only be
bounded (for a parse defect it is when the bad parser shipped, which may
predate careful records).

#### `FieldId` vocabulary

The vocabulary is **the served contract's field identity, never fusiform's
storage identity.** A name bound to internal representation changes for
reasons that are not facts about the world; the served contract is the only
surface where a rename is already a breaking change both sides would notice.

Granularity is set by a single test:

> Two corrections with genuinely different affected sets must not be able to
> share a `FieldId`. If they can, the vocabulary is too coarse.

Which yields five categories, at the granularity of what a charge is computed
from. The rate category is instantiated **per token class**, so the concrete
id set is larger than five — five is the number of distinct kinds of thing
that can be independently wrong:

| `FieldId` | Why it is separate |
| --- | --- |
| rate, **per token class** | `input` and `cache_read` moving are different facts, and a correction to one need not partition the same set of charges as a correction to the other. How much each class actually matters to a given ledger is that ledger's business and is not measured here. One id per class, never one for "rates". |
| charge unit | A misread unit is not a misread number. "This was per-image, not per-thousand-images" invalidates every derived charge even where the numeric value was right. |
| tier threshold | Correcting a threshold partitions only charges where a request crossed the boundary. |
| tier rates | Distinct from the threshold: if they shared an id, "the threshold was wrong" and "the over-threshold rate was wrong" could not be told apart, and they have different affected sets. This is exactly the shape of the tier defect. |
| existence | So a tombstone correction — *this model was recorded as present after it was actually retired* — is expressible without claiming a rate was wrong. |

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

/// A provider's named quality/fidelity level for a generated artifact
/// ("standard", "hd", "draft"). Left as a source-declared string rather
/// than a fusiform enum: normalizing quality names across providers would
/// mean asserting two providers' tiers are equivalent, which is a claim
/// fusiform has no basis to make.
pub struct QualityTier(String);

pub enum TokenClass { Input, Output, CacheRead, CacheWrite, Reasoning }
```

Closed enums, not strings. A consumer must be able to **fail** on a basis it
does not understand rather than guess — and the failure already has a name in
astrocyte's existing unpriced-reason enum
(`astrocyte-core/src/pricing.rs`): `unknown charge basis`. That variant was
written for LLM metering, before non-LLM billing was in scope, and it turns
out to be exactly the right refusal for a charge basis a consumer cannot
interpret.

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
without checking `type` reads a hypothetical `{"type": "images", "size":
1000}` tier as "context ≥ 1000 tokens". That is not speculative: the first
commons fix (`ffdd06a`) had exactly this gap and a probe confirmed it accepted
such a tier. It was closed in `528b680` after the probe was reported — see
`docs/findings/2026-08-11-commons-tier-threshold.md`.

With 164 image-output, 75 audio and 64 video models already in this upstream,
a non-token tier type is a plausible near-term addition rather than a thought
experiment. An unrecognized tier type is unpriced with `unknown charge basis`,
never coerced.

This settles a question left open in the pricing contract: a descriptive
property participates
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

`context_over_200k` (288 models) reads as an older second encoding of the same
fact — an inference from co-occurrence and matching values, not something the
upstream documents (`docs/upstream-models-dev-measured.md`).
Fusiform normalizes to the general tier form and never serves both. It
currently only ever co-occurs with `tiers`; that is an upstream convention,
not a guarantee, so a `context_over_200k` the tier rows do not corroborate is a
normalizer error, not a silent drop.

**Corroborated on RATES, never on the threshold.** The key's name is not its
threshold. Measured across the 288 models carrying it: the tier size is exactly
200,000 on 126, and 272,000 / 256,000 / 262,144 / 512,000 on the other 162; the
whole document also carries 32,000 and 128,000 thresholds, both *below* the
number the key names. What holds on all 288 with zero exceptions is that the
legacy block's rate set equals some tier row's rate set. So the check is set
equality on rates, and the legacy block contributes no rate of its own.

This paragraph is more specific than it was, because the looser phrasing —
"without a matching tier" — was implemented as *matching threshold* and
rejected 162 of 288 real models. The threshold list four paragraphs up was
already correct and was not consulted while writing the check. A design note
sentence that admits two readings will be implemented as whichever one the
implementer already believed, so the ambiguity is the defect, not the
implementation.

### Money is integers, and the unit is stated

```rust
pub struct Amount {
    /// Integer count of minor units. Never a float, anywhere, ever.
    pub units: i64,
    /// units × 10^(-exponent) of `currency`. Stated, never assumed.
    pub exponent: u8,
    /// ISO 4217 alphabetic code. A code, not a symbol, and never
    /// defaulted — see UnitProvenance below for how it is established.
    pub currency: CurrencyCode,
}
```

Floats exist only at the JSON parse boundary where the upstream forces them,
and never survive normalization. Measured justification: the same cost key
arrives as both JSON int and float (`input` is an int on 1,620 rows and a
float on 4,213 — an `f64`-only parser rejects 27.8% of them, an integer-only
parser rejects 72.2%), and 151 numbers
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
    /// PolicyId names a policy declared in fusiform's source and versioned
    /// with it (e.g. "models-dev-usd-v1"), so "why does the catalog say USD"
    /// resolves to a specific auditable rule rather than a habit.
    AssumedByPolicy(PolicyId),
    /// No statement and no policy covers it. The rate is unpriced.
    Unknown,
}
```

So models.dev rates are served as USD with
`AssumedByPolicy("models-dev-usd-v1")`, never as a stated fact.

> **The cost of recording provenance is constant; the cost of not recording it
> increases monotonically, with a discontinuity at an event nobody controls.**
> A store whose rows carry no unit provenance can still say truthfully, in one
> migration, that everything written so far assumed USD by policy. After the
> first genuinely non-USD rate arrives, that sentence stops being true of
> everything before a line and becomes a claim to defend per row. The decision
> shape is therefore not "wait until it matters" — the last moment to act
> cheaply is strictly before the event that makes it matter.

That argument is why this provenance is in the schema on day one rather than
added when a second currency appears. That policy
is a declaration in fusiform's source, versioned with it, saying: *rates from
the models.dev source with no stated currency are treated as USD.* Changing
it is a code change with a version bump, and every rate carrying it can be
found by its id.

This matters because non-USD provider pricing already exists in the fleet: the
quota-tracking module serves CNY balances from deepseek today. When such a
provider reaches a catalog with no currency stated and no policy covering it,
`Unknown` is what stops it being silently priced in dollars.

The same mechanism covers the audio keys. `input_audio` sits in the same flat
namespace as `input` with no unit distinguishing it:

```json
{ "input": 1.5, "output": 9, "cache_read": 0.15, "input_audio": 1.5 }
```

Per audio token? Per second? Per minute? Unstated. Absent a per-provider
policy, that rate is served **unpriced with `unknown charge basis`** rather
than guessed into a token rate.

### Absent, zero, and unknown are three states

```rust
pub enum RateValue {
    Priced(Amount),
    /// The source stated exactly zero. Not "free" — a stated zero. Whether
    /// a zero is a real price is the consumer's policy, not ours.
    StatedZero,
    Unpriced(UnpricedReason),
}

/// Adopted from astrocyte's existing unpriced-reason enum rather than
/// paralleled, so no mapping happens at the boundary. Their remaining
/// variants (degraded pricing time, arithmetic out of range) are
/// structurally not a producer's: fusiform has no pricing instant and no
/// ledger arithmetic.
pub enum UnpricedReason { MissingRate, NoCatalogCoverage, UnknownChargeBasis }
```

Measured: 420 models carry no `cost` object at all, and 1,423 cost entries are
exactly `0` (including whole provider families). Some zeros are genuinely
free; some are certainly "not published". The upstream cannot distinguish them
and neither can fusiform from the payload, so fusiform reports what was said
and surfaces the counts loudly on the operator surface rather than resolving
them.

### What that costs, stated up front

These rules make fusiform refuse to state a rate in many cases. Measured
against the 2026-08-11 payload, here is how much:

| | Count | Share |
| --- | --- | --- |
| Models with no `cost` object → unpriced, `MissingRate` | 420 | 6.7% of 6,253 |
| Priced models missing a `cache_read` rate | 2,208 | 37.9% of 5,833 |
| Priced models missing a `cache_write` rate | 4,637 | 79.5% |
| Priced models missing a `reasoning` rate | 5,726 | 98.2% |
| Models with audio rates → `UnknownChargeBasis` absent a unit policy | 81 | — |
| Models with `experimental` modes **and** a base rate → base rate only, up to 6.67x low if used in a mode | 33 of 38 | — |

`input` and `output` are present on 100% of priced models; every other class
is sparse. So a consumer metering cache-read traffic will find fusiform
returning `Unpriced(MissingRate)` for 37.9% of priced models — not because
fusiform is incomplete, but because the upstream did not publish a rate and
the alternative is inventing one.

Those are shares of the **catalog**. What they cost a consumer depends on that
consumer's traffic, which fusiform does not measure. One data point, supplied
by the metering module as a measurement over its own 16,407 spend facts as of
2026-08-11 — token volume by class:

| Class | Tokens | Share |
| --- | --- | --- |
| `cache_read` | 462,475,858 | 45.64% |
| `input` | 428,801,363 | 42.32% |
| `output` | 56,600,353 | 5.59% |
| `cache_write` | 33,523,908 | 3.31% |
| `reasoning` | 31,855,428 | 3.14% |

Caveats travel with it: this is one fleet's traffic (long-running agent
sessions against a small model set, so heavily cached), and it is **volume,
not cost** — given that cache-read is typically an order of magnitude cheaper
per token than input, the dollar distribution will look nothing like this.

What that combination implies is more useful than either number alone.
Cache-read is simultaneously the class most likely to be missing a rate and
the one where a missing rate touches the most tokens while distorting cost
least. The 37.9% catalog gap therefore lands on ~46% of that consumer's token
volume and a much smaller share of its dollars: a real gap, and not the
catastrophe the raw percentage suggests.

This is the intended behavior and it is worth stating as a number rather than
a principle, because "absent is not zero" sounds free until you see that it
means refusing to price four in five models' cache writes. The cost of the
rule is real; the cost of the alternative is silently wrong money.

The operator surface reports these counts on every ingest, so an unpriced
population that *grows* is visible as a change rather than as a steady state
nobody looks at. They stay operator-only and never ride a consumer
notification: change identity is a fact about models, a count is a fact about
fusiform's ingest, and putting both on one wire forces a consumer's handler to
branch on which kind of message arrived.

### The fourth state: retired

The three states above are all statements about a model the upstream **still
describes**. A model the upstream *stopped* describing is a fourth fact on a
different axis, and it must never be inferred from a gap.

Measured: BROCA's vendored snapshot (2026-07-24) held 5,823 models; today's
live payload holds 6,253. **+821 added, −391 removed** — including
`anthropic/claude-opus-4-1`, `azure/gpt-4`, `azure/deepseek-v3.1`. That is the
same class of event as the 319 silent removals recorded in `docs/charter.md`,
larger, sitting in the upstream now.

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

### Rate absence and coverage absence are different facts

Only 107 of 5,833 priced models publish a separate `reasoning` rate. A
metering consumer needs to know whether reasoning tokens are billed inside
output or not billed at all — and **an absent rate is consistent with both.**

It would be easy to derive an answer: `capabilities.reasoning == true` plus no
reasoning rate looks like "included in output." Fusiform will not do that. It
would manufacture a billing-relevant claim by combining two unrelated fields,
which is producer authority applied to something the upstream never said.

**This is not a hypothetical temptation.** The metering module reads its own
ingest path and derives an accounting classification from
`capabilities.reasoning` plus an absent rate. Their position is that the ban
is on inferring a *rate*, not on classifying the accounting — a narrower line
than fusiform draws, deliberately drawn, and defensible on its own terms.

What makes it worth recording here is not a rule violation. It is that the
same boundary was drawn in two places and the two disagreed without anyone
noticing: the rule as stated across the seam was broader than the rule
implemented in code. **A contradiction between a rule and its implementation
is findable by reading. A rule that exists in two versions, each internally
consistent, is not** — there is nothing to catch, because neither location is
wrong on its own.

The inference is cheap, plausible, and available to anyone holding both
fields. Refusing it on the producer side does not remove it from the world; it
relocates it to a consumer with the same two fields and less context about why
the correlation is unreliable. That is the argument for the served contract
carrying the cost-object completeness profile (below): **restraint alone
achieves nothing — the useful act is supplying evidence that makes the
inference unnecessary.**

And the downstream shape matters more than the classification. In that module
the derived state is not a label: it decides whether reasoning tokens are
charged, skipped as already-counted, or treated as a contract violation — and
their "no separate rate" state is overloaded, carrying both *this model has no
reasoning* and *I have no information*, which need opposite behavior when
reasoning tokens actually arrive.

Be precise about what that costs a producer, because the loose version argues
for the wrong thing. Fusiform's restraint did **not** create that overload —
it predates fusiform entirely. What the restraint did was make it visible, by
forcing the question of what a consumer does when nobody hands it a judgment.

"Restraint exported a cost" would argue, weakly, for less restraint. The
accurate version argues for the same restraint plus one obligation:

> **When you decline a judgment, know what decision the consumer makes without
> it.** That is an argument for asking, never for deciding on their behalf.
> Had fusiform supplied the inference, the overloaded state would still be
> overloaded and nobody would have looked.

But "presence or absence and nothing more" is too thin, and the reason is a
distinction only the producer can make:

- A provider publishing input, output and cache-read but **no** reasoning line
  is describing a model and declining to price reasoning.
- A provider publishing **no cost object at all** is not describing the model.

Both yield a missing reasoning rate. They are not the same evidence, and a
consumer resolves them differently — the first says something about how that
provider bills, the second says only that nothing is known.

So the served contract carries the rate's presence **and the completeness of
the cost object it sits in**. That is still not an inference; it is a fact
about what fusiform received.

Measured, the shape is real rather than theoretical. 14 distinct cost-object
key sets exist:

| Cost object | Count |
| --- | --- |
| `input`, `output`, `cache_read` | 2,377 |
| `input`, `output` | 2,128 |
| `input`, `output`, `cache_read`, `cache_write` | 1,115 |
| no cost object | 420 |
| … 10 more shapes | 213 |

And among reasoning-capable models with no reasoning rate: **1,009 sit in a
rich cost object carrying both cache rates**, while **2,893 sit in one already
missing a cache rate.** Those two populations are different evidence about the
same absence, and collapsing them would throw away the only signal available.

What accounting state any of it implies stays the consumer's decision, and the
profile does not resolve it. A rich object omitting reasoning is *evidence* of
a deliberate omission, not a statement of one — the provider might bill
reasoning inside output, or not bill it, or simply never have documented it in
a feed not designed to answer the question. The metering consumer's rule
stands: absence means not-reported unless something outside the catalog says
otherwise. The profile lets them distinguish a not-reported that sits on a
deliberate omission from one that sits on an incomplete description — a
defensible unpriced state versus an unknown one.

If a provider ever documents its accounting, that becomes a **named policy**
on fusiform's side (like the USD assumption in §4), auditable and versioned —
never an inference.

> **The rate at which a consumer can act on a signal is not the rate at which
> it should be preserved.** Both sides of this seam nearly failed that rule in
> opposite directions: the producer withholding a fact it held because it had
> just caught itself inferring, the consumer collapsing a distinction because
> it could not act on it yet. Both are the same error — treating "I cannot use
> this yet" as "this should not exist."

## 5. Modality is carried from day one

Schema, not content: the schema carries modality so non-LLM rows land without
a migration, and v1 ships no non-LLM source.

Measured vocabulary — and note these are not hypothetical:

- input: `text` 6,223, `image` 3,350, `pdf` 1,320, `video` 797, `audio` 468
- output: `text` 6,067, `image` 164, `audio` 75, `video` 64, `pdf` 2

299 models in this LLM-shaped upstream already emit something other than text.
164 of them carry a `cost` object, and every rate in it is token-shaped —
there is no field that can express per-image, per-second, or per-minute
billing.

The schema visibly breaks on those rows: **186 models declare
`limit.output == 0`** (70 image, 55 video, 17 audio) and **124 declare
`limit.context == 0`**. `poe/google/veo-3` carries `{"context": 480,
"output": 0}`. A token limit of zero on a model that does not emit tokens is
not a measurement — it is the schema representing a field that does not apply,
and a consumer reading it as a limit concludes the model can produce no
output.

Fusiform normalizes a zero in any limit field to **absent**, recording the raw
value in provenance — unconditionally, because a zero token limit is never a
meaningful limit on any model. §5.1 gives the measurement that rules out the
modality-conditional version of this rule, and explains why the mapping
matters more for `limit.output`, which no consumer reads today, than for a
field under active use.

The design conclusion fusiform draws — that real non-LLM coverage needs a
second source, and the schema must carry charge basis explicitly before one
arrives — is an inference from this evidence, not something the payload
states. It is the argument `docs/charter.md` was chartered on, now with a
measurement under it.

```rust
pub enum Modality {
    Text, Image, Audio, Video, Pdf,
    /// A value this version of fusiform does not recognize, preserved
    /// verbatim. An unknown modality is a fact about the upstream, not a
    /// parse failure, and coercing it into a known variant would invent
    /// a capability claim.
    Other(String),
}
```

Unknown modality values from a future upstream are preserved as `Other`, never
dropped and never coerced.

## 5.1 The byte-affecting fields

BROCA's consumed field set, traced at their source rather than described.
**Two** fields shape request bytes:

| Field | Consequence in BROCA |
| --- | --- |
| `limits.context` | the transform's pressure signal |
| `capabilities.reasoning` | gates `ReasoningPolicy` (`broca-provider/src/families/mod.rs:227`) — when false, forces `ReasoningPolicy::None` regardless of the provider's quirks, so no thinking block is rendered |

Everything else BROCA reads is **advisory**: `id`, `display_name`, `family`,
`release_date`, `status`. Their `raw` passthrough is ingestion-only and never
touched on the render path.

The two get a different class of treatment, because their failure modes are
silent rather than loud. `reasoning: false` on a reasoning model strips
thinking from every request. A `context` limit that is wrong distorts the
transform's pressure signal. Neither errors; both quietly produce worse
output.

**`limits.context` is where the absent/zero distinction is already being lost
downstream**, independent of anything fusiform serves. BROCA's
`broca-core/src/run.rs:2569` reads:

```rust
TransformUsage::new(fill, control.transform_context_limit.unwrap_or(0))
```

Meanwhile their catalog layer deliberately declines to guess: a tier-aliased
id the catalog does not list keeps `None`, because *"resolving it to the base
model's window would be a guess about whether the tier shares that window, and
an absent limit is the honest answer where a wrong one is unrecoverable"*
(`broca-catalog/src/live.rs:481-486`). Three in-use pairs resolve with no
context limit today, carrying 389 spend facts between them.

What is established: **absent and zero are indistinguishable once they cross
that boundary.** One bit of information — *was a limit published at all* — is
destroyed before the value leaves the process. What a downstream consumer then
does with a zero is not established: `context_limit_tokens` is serialized over
subc to another module, and that module's handling has not been read. It might
treat zero as saturation, as unknown, or divide by it. **That question has an
owner and should be asked rather than assumed.**

The defect stands without the answer, because destroying the distinction is
the defect. Two correct local decisions compose into a wrong global one: the
catalog layer's honest `None` becomes a confident `0` one layer down. Each
half reads fine in isolation.

> **Nobody reviews a composition, because a composition is not a place.**

The consequence for fusiform is not that the downstream collapse is fusiform's
to fix — it is that **emitting a meaningless zero would join a collapse that
already exists rather than introduce one.** Absent must stay absent all the way
out.

1. **Provenance per field, not per row.** Each carries whether its value came
   from the upstream, from a human override, or is absent — and absent stays
   distinguishable from zero. A field that fails silently must be able to say
   where it came from.
2. **A change to one is a distinct event in the diff**, surfaced more loudly
   than a display-name edit. A consumer's guard should not have to grep a flat
   change list to find the ones that matter.
3. **No defaults, ever.** Absent is representable and the type forces the
   caller to handle it. The commons defect is what `unwrap_or(0)` does to a
   pricing field; a context limit deserves the same refusal.

### `limits.output`: parsed by everyone, read by no one

An earlier draft listed `limits.output` as a third byte-affecting field. It is
not, and the correction is worth keeping because the true state is more
dangerous than the wrong one.

BROCA's `resolve_frozen` takes `max_output_tokens` as a **caller argument**;
the catalog's `limits.output` parses into their `Limits.max_output` and is
read by nothing outside three test assertions. No clamp, no default, no
fallback. So a zero from the upstream cannot reach a request body today.

That makes it a **parsed-but-unread field, which is exactly where a
plausible-looking zero waits.** The day someone adds a reasonable-looking
clamp — cap the caller's request at the model's declared maximum — a zero
limit becomes `max_output_tokens: 0` on the wire, and the reviewer has no
reason to suspect the value.

Measured: **186 models declare `limit.output == 0`**, **124 declare
`limit.context == 0`**, and **5 declare `limit.input == 0`**. A zero token
limit is never a real limit — a model accepting zero context or emitting zero
output cannot be called — so every one of these means "not applicable" or "not
published", never "zero tokens".

```
openai/gpt-image-1          {"context": 0, "input": 0, "output": 0}
poe/cerebras/qwen3-32b-cs   {"context": 0, "output": 0}       (text output)
greenpt/green-s             {"context": 0, "output": 8192}    (text output)
```

### The predicate, corrected by measurement

An earlier draft normalized a zero limit **on a non-text-output model** to
absent. That predicate is wrong, and the measurement that killed it is worth
keeping because it shows the rule was over-fitted to the examples that
suggested it.

Of the models declaring at least one zero limit field:

| Output modality | Count |
| --- | --- |
| non-text only (image / video / audio) | 148 |
| mixed (text **and** non-text) | 13 |
| **text only** | **39** |

`poe/cerebras/qwen3-32b-cs` declares `{"context": 0, "output": 0}` and emits
text. `greenpt/green-s` declares `{"context": 0, "output": 8192}` — zero on one
field, a real value on another, on a text model. And the mixed set is
genuinely mixed: `poe/google/nano-banana` has a real 65,536 context with a
zero output, while `azure/gpt-image-1.5` zeroes both.

So modality does not predict the zero, and a per-model judgement cannot be
made correctly — 39 text-only models would be excluded by the predicate while
carrying exactly the same meaningless zero.

**The correct rule is simpler and needs no predicate at all:**

> A token limit of zero is never a meaningful limit. A model that accepts zero
> context tokens or emits zero output tokens cannot be called. So a zero in
> any limit field normalizes to **absent**, unconditionally, with the raw
> value kept in provenance.

This is stronger than the modality version and immune to the failure that
version had: it does not depend on classifying the model, so it cannot be
right for 148 rows and wrong for 39.

**And it must be per-field, never per-row.** Of the 200 models declaring at
least one zero limit field, only 110 declare *every* field zero — **90 mix
real values with zeros**:

```
alibaba-token-plan/qwen-image-2.0   {"context": 8192, "output": 0}
xai/grok-imagine-image              {"context": 8000, "output": 0}
greenpt/green-s                     {"context": 0,    "output": 8192}
```

A row-level rule discards a real 8,192-token context window along with an
inapplicable output cap. Eight of those 90 sit under providers a consumer
serves today.

Absent stops a future clamp cold; zero sails through it. The mapping matters
*more* because nothing reads two of these fields yet, not less — an unread
field has no guard around it and no reader who would notice.

### How the wrong rule got there

Worth recording, because the mechanism is more reusable than the rule.

The row-level version came from five OpenAI image models that all zeroed every
field. But that set was not a sample of zero-limit models — it was the
`limit.input == 0` set, selected to answer a different question (whether
`max_input` shared the `max_output` hazard). It was internally consistent
because image models are, and a property of the *selection* was read as a
property of the upstream.

> **A rule inferred from a self-consistent example set will be self-consistent
> and wrong.** The check is not "is this rule true" — that invites
> re-reasoning from the same sample — but "what population produced these
> examples, and was it selected for something else?"

Two seats agreed on the row-level pattern before anyone counted. That felt
like independent confirmation and was not: **two parties agreeing on a pattern
drawn from one sample is one observation wearing two names.** The agreement is
what suppressed the check.

The cheap defence, which neither party ran: before agreeing, ask **"what did
you count?"** rather than "is that right?". The first is answerable and
exposes a selected sample immediately; the second invites re-derivation from
the same data.

And on how the modality predicate was caught: it was flagged as *imprecise*,
and the response was to measure the edge rather than reword the sentence.
**Rewording is what you do when you believe the rule and doubt the sentence;
measuring is what you do when you are willing for the rule to be wrong.**

### What fusiform must never claim

Whether a model *serves* is not a catalog fact. BROCA's
`in_use_models_still_serve.rs` composes three inputs — catalog lookup with
suffix-stripping and the overlay's `remove` verb, the auth method, and
`family_override` derived from it — and a raw presence check is not a
resolution verdict. Reproduced: screening BROCA's 39 in-use pairs by presence
reports six false positives — `openai/gpt-5.6-luna-fast`,
`openai/gpt-5.6-sol-fast`, `openai/gpt-5.6-terra-fast`,
`google/antigravity-gemini-3.5-flash`, `google/antigravity-gemini-3.6-flash`,
`xai/grok-composer-2.5-fast` — the same six the test's doc comment records,
because those ids resolve through suffix-stripping and family override rather
than catalog presence. All six are absent from both the vendored and the live
snapshot while carrying real production traffic.

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

### This is not a hypothetical risk

Measured during the first hour of watching the feed. Two fetches 36 minutes
apart, 6,253 models both times, zero added, zero removed — and exactly one
field changed in the entire 3.6 MB document:

```
model  opencode-go/deepseek-v4-flash
field  provider
09:58  {"npm": "@ai-sdk/anthropic"}
10:34  null
```

Deleting that override drops the model to its provider-level default of
`@ai-sdk/openai-compatible`. A model's renderer moved from the Anthropic wire
family to the OpenAI-compatible one, mid-morning, with nothing else in the
document touched and **`last_updated` still reading `2026-07-31`**.

A consumer sourcing wire-family selection from upstream data would have
changed its outbound request bytes for that model, silently, within a
half-hour window. That is the entire argument for Rule Q, and it took 36
minutes of observation to produce a live specimen.

It also demonstrates the two-hash split (§8) on real data: `raw_hash` moves
for this fetch, `normalized_hash` does not, and no consumer is notified —
which is correct, because nothing a consumer may act on has changed. The
operator surface still learns the upstream moved.

Enforced by a **negative** test over the real payload: the served type
contains no key from the quarantine set, and the test fails if a future field
is added to the served shape without classification. A test that only checks
the fields we do serve cannot see this.

### `experimental` is two planes fused into one field

Measured on the 10:34 payload: 38 models carry `experimental`, holding 42 mode
variants. Every mode carries a `provider` block (42 with `body`, 16 with
`headers`) and 38 carry a `cost` block. So one field contains both literal
request bytes and a second pricing plane:

```json
"fast": {
  "cost": { "input": 30, "output": 150, "cache_read": 3, "cache_write": 37.5 },
  "provider": { "body": { "speed": "fast" },
                "headers": { "anthropic-beta": "fast-mode-2026-02-01" } }
}
```

The pricing half is not a rounding difference. Of 42 modes, **32 carry an
input rate that differs from the model's base rate**, at multipliers of 2.0,
2.5, 6.0 and 6.67 — `gmicloud/anthropic/claude-opus-4.7` bills $4.50/Mtok base
and $30/Mtok in `fast` mode.

Counts to keep straight: 38 models carry `experimental`, all 38 have modes
carrying a `cost` block, and **33 of them also carry a base `cost` object.**
The 33 are where a base rate exists to be wrong; the other 5 have no base rate
at all, so they are already `Unpriced(MissingRate)` and no silent under-charge
is possible.

This is the charge-basis argument in its sharpest form: a mode is a rate
discriminator that lives inside a renderer-selection field. Fusiform cannot
serve the mode's rate without serving the mode, and serving the mode means
serving request bytes.

v1 therefore serves a model at its **base rate only** and records modes in raw
provenance. A consumer metering a request made in a non-base mode would price
it up to 6.67x low — a *stated coverage limit*, not an oversight.

The eventual fix keeps the quarantine intact: a mode becomes a `RateCondition`
(§4) carrying only its discriminating identity, never its `provider` block.
But that fix has a **prerequisite outside fusiform**, and building it earlier
would ship a selector nobody can use: the metering consumer prices from the
billing model identity stamped on a spend segment, and nothing on that fact
distinguishes which mode a run used. The discriminator is lost before pricing
happens, so the run engine must carry the mode onto the fact before a
`RateCondition` for modes is worth serving.

Full record, including the fleet's measured exposure and the condition that
arms it: `docs/findings/2026-08-11-experimental-mode-rates.md`.

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
                   Drives diffs, upstream-derived eras, and every
                   UPSTREAM-CHANGE notification.
raw_hash         — over the fetched bytes. Provenance and drift only.
                   Never reaches a consumer.
```

Split by **audience**, not by mechanism. A convention about which field to use
erodes; two fields with different destinations cannot be accidentally wired
together.

Note the qualifier, because the unqualified version conflicts with §3.1: a
`Corrected` era is written **without** `normalized_hash` moving, since nothing
upstream changed — fusiform's reading of an unchanged payload did. Corrections
therefore ring no doorbell and appear in no change notification, and this hash
is not the mechanism that surfaces them. That is not an exception bolted on;
it follows from what the hash measures. **A hash over the upstream's content
cannot detect a defect in the reader**, which is exactly why a correction
needs its own mechanism rather than riding the change path.

`raw_hash` moving while `normalized_hash` holds means the upstream changed
something fusiform does not model — genuinely valuable, since that is how
fusiform learns a field was added *before* a consumer needs it. It goes to the
operator CLI and nowhere else.

`raw_hash` earned its place the same day it was designed. Two fetches 36
minutes apart differed in exactly one field across 6,253 models, and that
field was a quarantined one (§6). `normalized_hash` correctly held, no
consumer was notified, and only `raw_hash` recorded that the upstream had
moved at all.

A conditional GET returns 304 with zero body bytes
(`docs/upstream-models-dev-measured.md`), so an unchanged poll is one round
trip and a changed one is ~355 KB gzipped. Bandwidth is therefore not what
bounds cadence. What matters is that the poll interval **is** the width of
every observation window fusiform records: cadence is the precision of the
history, not a performance knob. The remaining constraint on it is the
upstream's tolerance for polling, which is a courtesy question rather than a
measured one.

## 9. Store

Managed SQLite via `cortexkit-store`, opened **after** daemon connection from
the storage descriptor the daemon returns in its connection handshake
(`HELLO_ACK`), which names the path, the single-writer lease, and the mode.
Opening the store *before* connecting means guessing that path — "self-keyed"
— and a wrong guess is silent (never self-keyed: astrocyte's live store is at
`astrocyte/cortexkit/astrocyte/store.db` because it self-keyed early, and the
path at `astrocyte/store.db` is a 0-byte decoy that misleads every audit,
including mine an hour ago).

Tables: `observation`, `snapshot`, `provider_era`, `model_era`, `rate_era`,
`raw_document`.

Snapshot history lives as **rows, not content-addressed blob files**. At this
scale a snapshot is single-digit megabytes and eras only grow on change, so a
blob store buys nothing on size while costing a second durability surface: the
backup module captures the database, and files beside it would need their own
consistency story between the two. One store, one capture, one restore.

Sizing: 3.6 MB per fetch uncompressed, but eras only grow on change, so steady
state is one raw document per changed fetch plus a few thousand era rows.

### Ingest is one transaction

Adopted from astrocyte's background-loop design (their repository,
`.cortexkit/alfonso/drafts/2026-08-08-r3-revision-9-close-astrocyte-background-loop-gaps.md`,
merged at `05306a0`; a specification under adversarial review, not shipped
code): facts, cursor, and outbound work
commit **together**. If ingestion commits facts and advances its cursor before
recording what it owes downstream, a crash in between leaves a hole no later
poll can find, because the next poll starts after the committed range. That
defect is what prompted that design effort; inheriting the conclusion costs
nothing.

### Backup posture is load-bearing for audit, not for operation

The requirement, set by astrocyte as the consuming ledger: losing fusiform's
history must cost them *verifiability*, never *explainability*. Their ledger records the rate it charged and enough
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

Backup enrollment with `engram` (the fleet's backup module): whole-db capture.

**Corrected 2026-08-11.** This paragraph previously declared
`restore-with-monotonic-fence` for the observation sequence. No such mechanism
exists. Engram's descriptor vocabulary is `class`, `mechanism`, `path`, `root`,
`cursor_authority`, `writer_interaction`, `export_contract` and
`validate_hook` (`engram-core/src/catalog.rs`); there is no restore policy of
any kind, and `validate_hook` is declared in the catalog type but read by
nothing in the restore path.

The deeper problem is that no enrollment flag could have worked. A whole-db
restore replaces the file, so a watermark stored inside the database is
restored along with it. Engram protects its own sequences with monotonic
watermark tables, but those defend against row deletion inside a live
database — a different failure. **The rewind is unavoidable at the storage
layer**, so the counter has to be immune to it instead: see §10, where the
version is derived as `max(now_ms, current + 1)`.

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

BROCA parses the upstream's `cost` object into their own cost type, and
nothing on their render or
admission path consumes it; their billing lane exports raw token counts with a
charge-basis label and deliberately ships zero pricing. Capability data
changes when a provider ships a model; pricing changes on a different clock
and carries effective-dated eras capability data does not have. Coupling them
would make BROCA a consumer of pricing churn they have no use for — and every
push they receive is a swap they must validate.

Astrocyte did not argue for the split. They stated a requirement —
effective-dated rate eras with point-in-time lookup — and the split falls out
of it: a plane carrying eras and a plane carrying current facts have different
change semantics, not merely different clocks.

Worth stating precisely rather than as "two independent derivations," which
would be the stronger and less accurate claim. One derivation, plus one
requirement that implies it.

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

#### The restore case, where a correct refusal produces a wrong outcome

The version's durability lives in **fusiform's** store, and that store is
backup-class (§9). A restore from backup rewinds the version counter. Every
consumer then correctly refuses every push until fusiform re-crosses the old
high-water — which presents as a total, silent push outage after a recovery,
with every component behaving exactly as designed.

This is the one path where the ordering rule's correctness is the problem, so
the rule is not complete without it. Two closures:

1. **The version is restore-invariant by construction** — `max(now_ms,
   current + 1)`, built and mutation-proven. Wall-clock time does not rewind
   when a file is restored, so the first version issued after a restore already
   exceeds every version issued before it; the `current + 1` term covers the
   opposite failure, an NTP correction moving the clock backwards.

   **This replaces a mechanism that did not exist.** The original text declared
   `restore-with-monotonic-fence` in the engram enrollment and called it a
   fleet rule that "exists for exactly this shape". It does not exist — see §9
   — and no enrollment flag could have supplied it, because a whole-db restore
   replaces the file and takes any in-database watermark back with it.

2. **High-water re-sync on first push after boot** (specification, no code).
   Fusiform asks each consumer for its last-seen version and refuses to serve
   below it. Still needed after closure 1, and for a case closure 1 cannot
   reach: a *consumer* restoring from backup, where fusiform's counter is
   intact and the consumer's high-water moved backwards.

> **How the invented mechanism survived.** It was cited twice, in two sections,
> in a form specific enough to look verified — a kebab-case flag name and the
> phrase "the fleet rule exists for exactly this shape". Both citations came
> from one belief, so cross-referencing them confirmed it. What exposed it was
> going to write the enrollment file and reading engram's descriptor type,
> which is the same instrument that has found every other defect in this
> repository: run the real thing against the real artifact.
>
> The specific hazard worth naming: **a fabricated mechanism attributed to
> another team's system is nearly unfalsifiable from inside your own.** Nothing
> in fusiform can contradict it, the name reads as a quotation, and the
> attribution makes checking feel redundant.

   **Measured, and it holds — with one ordering requirement.** A consumer
   restore rewinds its ingest cursor too, so it reports a last-seen version
   lower than what it actually consumed, and fusiform re-serves eras it
   already holds. The metering module read its own ingest path against this:
   it compares each model's rates against the latest stored row and skips on
   equality, so a re-serve appends nothing. A genuine no-op, not a probable
   one.

   But the comparison is **latest-row-only**, which makes the guarantee
   conditional: *re-served eras must arrive in observation order.* An older
   era arriving after a newer one compares against the newer row, differs, and
   is appended below the top — leaving a history row out of sequence. Current
   pricing survives (selection is by pricing instant), but a point-in-time
   query then returns a value that was never in force at that instant.

   So fusiform's re-serve must be ordered by observation, not merely complete.
   Their ingest gaining its own ordering guard is their fix; emitting in order
   is fusiform's obligation, and "the consumer will cope" is not one this
   design gets to assume.

   Note the shape: the re-sync is correct for fusiform and its consequence
   lands entirely on the other side of the wire, where fusiform cannot see it.
   A fix whose cost is paid by the party that did not choose it is one to name
   out loud rather than ship quietly.

   This is a standing hazard of being a producer, not a one-off: **every
   consumer-side consequence of a fusiform decision is invisible to fusiform
   by construction.** A terminal consumer — one that serves only humans — has
   the opposite property and pays for its own seam mistakes, which means it
   never has to develop the habit. Fusiform serves modules, so it does.

Belt and braces, because the two failures are on different sides of the wire
and neither mechanism sees the other's.

**A refusal that a consumer reports as success is the worse cousin of this,
and it is worth stating even though fusiform does not have it.** The metering
module raised the possibility in its own delivery path on hearing about this
one: its receiver treats a too-low version as an idempotent no-op returning
the installed watermark, and its sender counts any well-formed response as
progress — so the identical restore would produce a *silent false success*
there rather than a silent total outage here.

Subsequently measured by its owner and confirmed: their allocator takes
`MAX(version)` over five tables that all live in one SQLite file, so a restore
rewinds every source atomically and the allocator has no way to know a higher
version was ever issued. Combined with the two ends already read from source,
a restored store reissues versions the consumer has already seen, the consumer
correctly no-ops them, and the sender retires outbox rows nobody applied.

Two details from that measurement generalize past their module:

**A partial solution to a general problem is more dangerous than none**,
because it reads as if the hazard was considered. Their schema already carries
a per-cap `highest_version` specifically so versions are not reused after rows
are deleted — a deliberate high-water mechanism, solving the reuse hazard for
the *deletion* path while leaving it open for the whole-file-rewind path.

**Guards reveal their author's assumption.** The same code checks `checked_add`
and rejects negative versions: it defends against the arithmetic going wrong,
not against the input being stale. Every guard there assumes the maximum is
authoritative, which is exactly the assumption a restore breaks.

Applied independently to the receiver at the other end of the same chain, the
technique found the same gap immediately: every guard in that function
concerns *the request being wrong* — empty bucket, inactive cap, mismatched
identity, non-contiguous range — and **not one concerns the request being
stale-but-well-formed.** Two modules, one blind spot, recovered the same way.

> This is a better audit technique than reading for correctness, because
> reading for correctness re-derives the author's model and then checks the
> code against it. Reading the guards recovers the model and then asks what it
> **omitted** — which is the question the author could not have asked.

Fusiform's push must not acquire this shape. Note also which fix is which: an
echoed-watermark comparison catches the false success *after* the collision; a
durable floor prevents the collision. They are not substitutes — the floor
cannot cover a consumer-side rewind, and the comparison cannot prevent the
reissue.

The receiver half of that chain was subsequently read at source by its owner,
and it is not a sender bug in isolation: **two distinct no-op arms both return
`Applied(current_watermark)`** — one for a stale version, one for an
already-consumed range — and both roll their transaction back. The reply is
not merely shaped like a success; it is a legitimate value, the true current
position. A sender has nothing to distinguish an apply from a
refusal-dressed-as-acknowledgement except comparing the echoed watermark
against what it sent, which requires already suspecting the case.

**And the reason it is built that way is correct for the case it was built
for.** Both arms are idempotency, and idempotency wants exactly this: a
duplicate delivery should be a harmless no-op returning true state. That is
right for *retry*. It is wrong for *restore*, where the resent version is not
a duplicate but a **reissued** one — a different payload wearing a version
number already retired. Idempotency cannot tell them apart, because from the
receiver's side they are byte-identical.

> **A guard built for retry, meeting restore.** The distinction a receiver
> cannot make is not a flaw in the receiver; it is a fact about what the
> protocol says. If "same version" can mean two different things, the wire
> must carry which.

So the rule for fusiform's own push is stronger than "compare the watermark":
**the acknowledgement must be discriminated at the protocol level** — applied
versus no-op-with-reason — so a sender distinguishes progress from
acknowledgement without inferring it. A watermark comparison is a sender-side
workaround for a receiver-side ambiguity, and it only fires for a sender that
already knows to look.

#### Measured 2026-08-11: this acknowledgement has no transport

Everything below about the acknowledgement's shape stands as reasoning and
cannot be built on the current push path. Three facts, read from source:

- `ModuleHandle::push` emits `FrameType::Push` with `corr: 0`, and a module's
  outbound vocabulary is `catalog_update`, `push`, and nothing else. There is
  no module-initiated request, so no reply can come back.
- `subc-client-rs/src/consumer.rs:3053` is `FrameType::Push => {}`. The shared
  Rust consumer client reads push frames and drops them.
- `broca-subc/src/connection.rs:867` and `:951` do the same, deliberately:
  "interim progress — ignore".

The daemon forwards pushes to the consumer socket, so the bytes arrive; both
clients throw them away. What a consumer actually receives is `StreamData` on
a held-open subscription, which the module emits from inside a live request —
a pull-shaped relationship, with the consumer owning the lifecycle.

The error worth naming: `broca-protocol`'s `ApprovalResponse` is real and does
what this section says. A response TYPE existing was read as a response
CHANNEL existing. The section argues at length about which arm names a partial
apply and never says how the reply travels — **a design detailed about a
message's contents and silent about its direction has not been walked end to
end.**

#### The shape already exists in the fleet; do not invent a third

`broca-protocol/src/approval.rs` ships a discriminated acknowledgement today:

```rust
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalResponse {
    Allow         { tool_call_id },
    AllowNarrowed { tool_call_id, input },   // partial apply, named
    Deny          { tool_call_id, reason },  // refusal carries its reason
}
```

Outcome in the tag, reason on the refusing arm, and — the arm worth stealing —
**the partial-apply case gets its own name instead of hiding inside success.**
That is exactly the failure being avoided: an absent or partial apply
indistinguishable from a completed one. Fusiform's push acknowledgement starts
from this shape rather than a new one.

The refusal *vocabulary* has a second precedent with a guard attached
(`broca/docs/error-class-contract.md`): a pinned closed string set, producer
detail in a sibling channel, and an arm-admission test —

> **"Does a consumer branch on it GENERICALLY?"**

If no consumer makes a distinct decision on a reason, it does not earn a wire
arm; it collapses to a broader class and survives in detail. Applied to
fusiform's push refusals, most candidate reasons fail that test, which is the
point: **a rich refusal vocabulary nobody branches on is the same defect as a
rich success shape nobody can read.** Both add structure that carries no
decision.

There is a trap in applying that strictly right now, though: **the test asks
whether a consumer branches generically, and fusiform has no consumer yet.**
Answering it today means predicting a consumer's decisions rather than
observing them.

So the safe direction when unsure is **detail, not an arm.** Promoting a
detail string to an arm later is additive; demoting an arm is a breaking
change to a live acknowledgement — the exact wedge this section exists to
avoid. When a real consumer branches on a detail string, that is the evidence
to promote it, and it will be evidence rather than prediction.

The reason to record this here rather than design it later: retrofitting a
discriminator to a live acknowledgement wedges every existing consumer.
Fusiform has no receiver yet, so it has the one chance to not need that
retrofit.

A no-op that reports like a success converts an ordering fence into an
ordering illusion.

Note what the version must **not** be derived from: the maximum observation
sequence. It rewinds on restore for the same reason the counter does, so it
solves nothing. A restore-invariant derivation needs a wall-clock or hybrid
logical-clock component; the fence plus re-sync is simpler and is what this
design takes.

### Payload boundary: fusiform owns the served schema, in its own crate

> **Settled 2026-08-12 (Ufuk), after checking with SUBC, ASTRO and BROCA.**
> This section previously said the schema would ship as the next major version
> of `cortexkit-model-catalog` in commons. It ships as `fusiform-protocol` in
> this repository instead. The reasoning below is what each seat supplied;
> two of the three corrected something this note had asserted.

The fleet's cross-repo payload rule requires one definition consumed by both
sides of a wire; a same-repo test cannot see a cross-repo boundary.

The rule never required a NEUTRAL home — that was this note's inference from
the fact that the only example it knew of lived in commons. SUBC's ruling:
served schema types are **declarations authored by the producer**, and
producer ownership makes drift unauthorable, because the schema and the crate
move in one commit. A neutral home reintroduces the two-copies problem with
extra steps. The precedent is `subc-protocol` — subconscious owns the daemon
and publishes the wire types from the same repo, and every client compiles
against them. `cortexkit-model-catalog` living in commons was a workaround for
there being no owner module; now there is one.

The dependency tree is part of the contract. ASTRO's condition for depending
on a module-owned crate: it must not drag in a client, a runtime, or a
database driver, because then the location stops mattering and the coupling is
real — they have declined a crate on exactly that ground, mirroring three
types by hand rather than take a dependency that pulled in cryptographic and
network libraries for three struct definitions. `fusiform-protocol` depends on
`serde` and `serde_json`, and `crates/fusiform-protocol/tests/deps.rs` fails
if anything else arrives.

#### Lockstep: the argument this note made was wrong

This section previously argued that a published, semver-versioned crate cannot
create lockstep releases, because a consumer upgrades when it chooses. That is
true of a *published dependency* and says nothing about how this fleet
actually consumes crates.

BROCA supplied the counterexample from their own tree: **four sibling
dependencies are PATH deps**, including one into commons. Nobody consumes the
published version, so semver protected nothing. They already carry the scar —
a prose file recording which sibling commit each release built against,
because `Cargo.lock` cannot pin a path dependency.

Verified in fusiform's own lock rather than taken on report: `subc-protocol`
appears with a version and a dependency list and **no `source` and no
`checksum`**, while a registry dependency like `serde_json` carries both. So
`cargo build --locked` genuinely cannot see a path dep's code move.

That makes the version-discipline check load-bearing rather than hygienic: a
version bump is the only signal that reaches a path-dep consumer at all.
`scripts/check-wire-crate-version.sh` enforces it in CI, following
subconscious's own script including the doc-only exemption — a rule that fires
on prose gets ignored on substance.

The general lesson is worth more than the correction. Two people agreed on a
mechanism's properties and neither asked the adjacent factual question: does
anyone here depend that way? **Mutual agreement reads as independent
confirmation.** A claim held alone still feels like something to check; a
claim two seats settled together feels checked already, and what made it feel
settled was consensus rather than evidence.

#### What is pinned

Both halves of the payload rule, in SUBC's framing — declarations shared,
outputs producer-pinned:

- **Declarations**: `fusiform-protocol`, the crate a consumer compiles against.
- **Outputs**: `crates/fusiform-module/fixtures/served-payloads.json`, a golden
  fixture of four real served payloads, minted by running the serve path over a
  real store. Renaming a wire field, dropping a correction's reason, or
  emitting a list that should be skipped each redden it by name.

The fixture is minted, never hand-written, per this repository's rule: a
hand-written fixture encodes its author's understanding of the format, so a
fixture and the code it exercises sharing one author in one commit can produce
a non-vacuous test that certifies a bug.

Fusiform takes the commons crate's money doctrine, because it encodes real
incidents: decimal-string scaling, half-even rounding at the money resolution,
reject-nonzero-rounding-to-zero, the negative-rate guard, checked arithmetic
throughout. It does **not** take its raw-models.dev shape — that is the role
being retired.

Fusiform takes the crate's money doctrine, because it encodes real incidents:
decimal-string scaling, half-even rounding at the money resolution,
reject-nonzero-rounding-to-zero, the negative-rate guard, checked arithmetic
throughout. Fusiform does **not** take its raw-models.dev shape — that is the
role being retired.

The boundary is pinned with a vendored golden fixture from day one, and
specifically **a golden that a deliberate mutation must break**. A shape
assertion nobody has broken on purpose is just a differently-worded value
assertion.

### Retirement is the tail, and it has one real consumer

The current crate's role — shared mirror of raw models.dev shapes — ends when
its consumers read fusiform-served data instead. That retirement is the *tail*
of the consolidation, never a milestone on its own: nothing may be removed
before the replacement serves.

One correction to how this is often stated. Measured: **no BROCA `Cargo.toml`
depends on `cortexkit-model-catalog`.** BROCA has its own `broca-catalog`,
which parses the vendored snapshot into `broca-provider`'s own spec types.
So BROCA's no-network seed embed does **not** parse with the commons crate's
types and is not coupled to its retirement at all. The current crate has
exactly one dependent: astrocyte.

That matters for sequencing. The gate on retiring the old *role* is
astrocyte's switch, not both consumers'. BROCA's gate is separate and is about
its own embed: it keeps a permanent seed regardless (§7), so what changes for
BROCA is which types the seed parses with, on BROCA's schedule.

**And that switch is smaller than every sentence written about it.** ASTRO
measured their own use, 2026-08-12: the entire dependency is one type,
`CatalogDoc`, at one call site — `ingest_snapshot` in
`astrocyte-core/src/catalog_ingest.rs`, which walks providers and models,
reads five rate fields plus tiers and `capabilities.reasoning`, and converts
to their own row shape. Nothing else in either of their crates touches it.
Their store, era selection, pricing and money arithmetic are unaffected: they
convert at the boundary today and would convert at the boundary after.

So the cutover is a function's input type, not a migration — and the
transition mechanic this section used to name (two semver-incompatible
versions of one crate coexisting under a renamed dependency) does not arise
at all, because the successor is a differently-named crate.

The reason that estimate was wrong for so long is worth keeping, because it
is reusable. **"The retirement of `cortexkit-model-catalog`" sounds like a
migration because retirement is a lifecycle word, and lifecycle words carry an
implied scope** — things that get retired are things that were installed,
integrated, depended upon. The phrase smuggles in a size estimate through the
connotation of its verb. What collapsed it was not care; it was asking a
question with a countable answer (how many call sites) instead of a question
about the change.

The operational form, from ASTRO: when a change is described with a lifecycle
verb — retire, migrate, deprecate, consolidate, cut over — find the countable
thing before estimating. With one addition from this instance: **the countable
thing is often in someone else's repository**, and that is exactly the
position where the verb's connotation is the only information available.
Nothing on fusiform's side could have corrected this; re-reading the charter
ten more times would have kept yielding the same wrong size.

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
3. ~~**Cadence.**~~ **Settled: 30 minutes, conditional GET, not
   configurable.** Poll interval is the width of every observation window, so
   it is a precision decision rather than a performance one. The floor is
   upstream politeness, not cost: an unchanged poll is one round trip and zero
   body bytes (measured). The value comes from the live specimen in §6 — a
   renderer flip visible in a 36-minute window — which 30 minutes catches. Not
   a configuration knob in v1: one number, recorded in every observation row,
   changed by release. A knob here would let an operator silently widen every
   era boundary in the store.
4. **Crate-version sequencing.** The served types ship as
   `cortexkit-model-catalog`'s next major version (§10). Astrocyte's switch is
   what retires the crate's current role; BROCA's adoption is on its own
   schedule, since BROCA does not depend on the crate today. Neither sequence
   is settled here.
5. ~~**The cutover prior edge.**~~ **Settled: era zero is `Seed` with no
   window.** Found by this note contradicting itself — §3 requires
   `prior_observation_at` to be fusiform's own previous observation and NULL
   unless the boundary is `Observed`, while a commitment made across the seam
   assumed that column could hold an instant handed in by a consumer. See
   below for the resolution and why the alternative was worse.

### Settled since the first draft

An index into the sections that carry each decision's reasoning, not a
substitute for them. Nothing here should be cited without its section.

| Decision | Where |
| --- | --- |
| BROCA's consumed field set (two byte-affecting fields, not three) | §5.1 |
| Envelopes differ **per consumer**: full snapshot + hash to the capability consumer, change-identity doorbell to the pricing one — deliberately, not yet unified | §10 |
| Diff carried as advisory only; the consumer's guard recomputes | §10 |
| Ordering: monotonic catalog version, `<=` refused, plus a restore fence **and** a consumer high-water re-sync | §10 |
| Acknowledgement discriminated at the protocol level; detail-when-unsure | §10 |
| Bootstrap: consumer keeps a permanent embed | §7 |
| Payload boundary: fusiform authors the type, commons publishes it | §10 |
| Pricing as a separate surface from capabilities | §10 |
| `Corrected` boundary kind, its extent, and its `FieldId` vocabulary | §3.1 |
| `affected_from` is a lower bound; unknown means earliest | §3.1 |
| Zero limit → absent, per field, unconditionally | §5.1 |
| Cadence: 30 minutes, conditional GET, not configurable | §11 |

One of those rows was itself a defect until this pass: the envelope line read
"full snapshot + content hash" without qualification, collapsing two
deliberately different shapes into one — which is exactly what §10 warns a
future reader against doing. **A summary table is where an asymmetry a
document argues for goes to die**, because summarizing is compression and the
asymmetry is the detail being compressed away.

### The cutover input fusiform cannot derive — and the schema gap it exposes

Fusiform's first push to the pricing consumer cannot supply its own
`prior_observation_at`: era zero has no prior edge on the producer side. The
prior edge must be **handed in** from the consumer's last real observation, or
the resulting boundary silently looks one poll-interval wide instead of
spanning the supply gap it actually spans.

**This contradicts §3 as written, and the contradiction is the useful part.**
§3 defines `prior_observation_at` as *the other edge of the window* —
fusiform's own previous observation — and requires it NULL unless
`boundary_kind` is `Observed`. A handed-in instant is neither: it is another
module's observation of a different artifact, and writing it into that column
would make the column mean two things depending on the row.

Two options were drawn; **the consumer chose the second**, and their reason is
stronger than the one this note originally gave for it.

- **Option A:** a distinct nullable column for an inherited prior edge, with
  its own source attribution, leaving `prior_observation_at` strictly
  fusiform's.
- **Option B (chosen):** era zero carries `boundary_kind: Seed` with no window
  at all. The gap is expressed in the consumer's own record.

The original argument for B was that the gap is a fact about the consumer's
supply rather than the upstream. True, and not the load-bearing part. The
real argument is that **A would move a fact across a seam and give it
fusiform's authority.** The consumer already holds both edges — their last
real observation sits in their own store. Under A, fusiform's store would
carry a three-week window as a value it cannot verify, attributed to a source
it cannot read, in a column used once. A wrong instant handed in would be
recorded with fusiform's authority and nothing on fusiform's side could catch
it.

That is the producer-authority problem arriving through a column instead of an
inference — and refusing it is the same rule as refusing to convert currency
or derive an accounting state.

Under B, era zero is `Seed` with no window, which is exactly true: fusiform
has no prior observation and a seed boundary says so. So the commitment splits
into two, each held by the party that can verify it:

> Fusiform commits that era zero is honestly marked as a seed rather than
> dressed as an observation. The consumer commits that its first ingest
> records the gap against its own prior instant.

Neither is weaker than the single commitment it replaces. **A fact should be
recorded by whoever can check it**, and a guarantee is worth more when its
holder can falsify it.

On the record so it exists in more than one place: astrocyte's last real
observation is `observed_at_ms` 1784494281391, snapshot version tag
`file:1784494256028`, 19 July 2026, 5,297 rows — to be re-measured at cutover
rather than quoted, but available if it cannot be.

## 12. Verification stance

Adopted from the fleet's verification method (subconscious
`docs/hunting-loop-briefing.md`, the accumulated defect-hunting discipline
these modules are reviewed against), applied to this design:

- **Produced-output fixtures must be minted by the real producer.** Stronger
  than "excerpted from a measured payload," because it also covers another
  module's response envelope, not just an upstream document. A hand-written
  fixture encodes its author's misunderstanding.

  The commons tier defect is the specimen: fixture and parser authored from
  one wrong belief in one commit, so a non-vacuous assertion certified the
  bug. Mutation-tested during the fix — restoring the defective line reddens
  the two new tests, while the original `tiers_parse_sorted` stays green **even
  with a corrected fixture**, because a correct fixture parses correctly under
  both the broken and the fixed parser. The old test was not weak; it was
  incapable of distinguishing the two implementations in either fixture state.
  Only a test pinning the *refusal* direction can. Full record in
  `docs/findings/2026-08-11-commons-tier-threshold.md`.

  Corollary: a fixture and the code it exercises must not have the same author
  in the same commit without a measured payload in between.
- Every check ships with a proof it can fail. The restore test, Rule Q's
  negative test, and tombstone completeness are all shaped so that the
  plausible-looking wrong implementation fails them.
- Absent, empty, and unknown stay three states — in polling outcomes, in
  rates, in modality, in tier discriminators. Every place this schema could
  substitute a default for a missing value is a place a bug hides.
- The first instrument is the least trustworthy thing in the module. The
  commons defect was found by *executing* a parser against real bytes, one
  nesting level below where anyone was reading — and it fell out of an
  unrelated question (how the parser handled the int/float split, where the
  same key arrives as both types on different rows). The discipline that found
  it was "run it on real data," not "read it carefully." The plan of record
  had been to read the crate.

- **A sweep is only as good as the case in it whose answer you already know.**
  Every finding on 2026-08-11 that survived turned on a known-positive sitting
  in the output: a rate-limit error reproduced against a pre-change binary, an
  in-use screen that reproduced six known false positives, and a field-usage
  enumeration whose first pass wrongly flagged a field the author had traced
  by hand an hour earlier. That last one is the clearest — **the instrument
  being obviously wrong on a known case is what made it trustworthy on the
  unknown one.** Without it, the same output is seventeen "unread" fields and
  no way to sort them.

- **A claim only becomes checkable when it is stated across a seam.** This is
  stronger than "cross-seam claims have propagated, so check them." A rule
  held privately reconciles with everything, because it commits to nothing;
  saying it to another party is what collapses it into a definite form, and
  the collapsed form is the first version a specification can contradict.

  So the set of claims made across seams is not merely the highest-risk set —
  it is very nearly the only set that *can* be checked. Everything else is
  still vague enough to survive any check run against it.

  Evidence, from both sides of this seam on one day: an author-run consistency
  pass over 1,800 lines was still running when a claim-by-claim check of
  things said out loud found a real contradiction in ten minutes. And on the
  consumer's side, four adversarial rounds found contradictions in their
  specification — every one in a claim they had also stated to another module.
  The natural reading is "seam claims are load-bearing so they get scrutiny."
  The better one is that those were the only parts precise enough to be wrong.

  Practical form: **check what you have said, in the direction claim →
  section.** Reading two sections for agreement invites the author to supply
  the reconciling clause from intent; testing a specific sentence against a
  section does not, because the sentence arrived already fixed.

- **Textual and seam contradictions are disjoint populations, and only one of
  them gets found eventually.** An author-run pass over this document found
  three real contradictions — all textual, two sentences on a page disagreeing,
  which any careful reader would eventually hit. The claim-by-claim check
  found one that **no reader of the document could ever catch**, because the
  contradicting half lived in a message.

  So the argument is not that one method is cheaper per defect. It is that a
  textual defect has a finite discovery time and a document-versus-commitment
  defect has **no discovery mechanism at all** except someone happening to
  hold both halves — which occurs by accident or never.

- **The narrowing is the event that creates the textual defect, so check at
  the narrowing.** All three found here have one shape: a general statement
  written early, a specific rule added later, and the general statement left
  standing. Nobody chose the contradiction. The highest-yield check is
  therefore not "read for contradictions" but **re-read every general
  statement immediately after adding a rule that narrows it** — a moment you
  can notice while it happens, rather than a search you run afterwards.

- **A premise you would state to someone else is load-bearing enough to
  measure.** An unverified assumption supporting *caution* escapes checking,
  because being wrong about it seems only to make you slower — so it feels
  safe to leave unmeasured. It is not: it propagates as a stated fact and
  shapes plans around costs that do not exist. Caution built on unmeasured
  premises is not conservative, it is wrong in a direction that feels
  responsible. The catchable moment is when the premise becomes a reason given
  to another party; at that point it has stopped being a private assumption
  and become a claim.

  **The strong form: "I am about to tell another seat what my system does" is
  a hard stop for a source read.** Stating a rule across a seam *feels* like
  reporting and is actually claiming, which is why it escapes the check that
  any other assertion would get. Three distinct defects in one day would have
  been caught by that single tell — an unmeasured caution premise, an
  unmeasured traffic claim, and a policy stated in a form its own code did not
  implement. Three instances in a day is not a coincidence, it is a rate.

  It is also the deterministic detector for the two-versions-of-a-rule failure
  above. A third party holding both versions catches it reliably but by
  accident; the author noticing they are about to assert outward catches it
  every time, before it propagates, for the cost of one read.

- **Correct attribution does not make a claim measured, and citing a source
  makes it look checked.** Three variants of one family turned up while this
  note was written, in increasing order of subtlety: inventing a fact;
  inferring one by combining unrelated fields; and *laundering an unmeasured
  claim through correct attribution*. All three produce a sentence that reads
  as evidence. The third is the hardest to see because the provenance is
  genuine — nothing feels wrong.

  Specimen: this note carried "cache-read dominates agentic traffic" as a bare
  fact, then was "fixed" by attributing it to the metering consumer. The fix
  made it worse, promoting an unmeasured design argument into a cited
  measurement. It was removed rather than re-attributed.

  The sequel is the useful part. The consumer then *measured* it, and the
  claim survived in a corrected form: cache-read is 45.6% of their token
  volume — a plurality, not a majority, with fresh input close behind at
  42.3%. "Dominates" was wrong; the structural point it was recruited to
  support was right for a different reason. **An unmeasured claim is not
  necessarily a false one, which is exactly why it is dangerous** — it is
  usually close enough to survive scrutiny and wrong in the detail that
  matters.

  Two corollaries, both earned the same day:

  **Laundering needs no dishonesty at either end.** A second instance ran the
  full path: a module owner inferred a semantic from a field's name, stated it
  confidently, this note cited it accurately, and the citation was correct.
  The defect entered at the assertion and became invisible at the citation.

  The rule this yields is *not* "trust owners less" — a claim from the seat
  that owns the code is the strongest evidence normally available, and
  discounting it would cost more than it saves. The precise version is that
  **owning a field tells you what your code does with it, which is a different
  fact from what the field means to whoever reads it.** The retracted claim
  concerned a value with no consumer in its own tree: serialized over the wire
  to another module, whose handling nobody involved had read. The owner could
  say exactly what they emit and could not say what it means downstream — and
  fusiform is in the same position about everything it serves.

  So *"where did you read that"* and *"what happens to it"* are answerable in
  different places, and a seam is exactly where the two get conflated.

  **A sixth variant, and the only one whose defect enters at someone else's
  keyboard: a claim strengthened by another party's generous framing.** This
  note nearly recorded a peer's reasoned hypothesis as something they had
  found by inspection — an upgrade made in good faith, which would have
  arrived in a design document with their name attached and neither party
  having said anything false. They caught it. The author of a claim is the
  only one who knows which link in it was measured, so a restatement is a
  place where confidence is silently added.

  The defence they offered is better than a confidence level: **name which
  link is soft.** "Both ends read from source, the middle reasoned" is
  checkable and tells a reader exactly where to look. "Fairly confident" is
  not, and cannot be corrected by anyone but the author.

  The sequel closes the argument. Having named the soft link, they went and
  measured it — and it confirmed *worse* than the hypothesis (§10). So the
  retraction was not a detour on the way to the answer; **it was what made the
  answer worth having.** An unmeasured claim that happens to be right is
  indistinguishable from one that happens to be wrong, and only the retraction
  turns "I recognize this shape" into a query someone runs.

  **A structural argument that needs a fact was never structural.** If the
  argument survives without the measurement, the measurement was decoration;
  if it does not, it needed a real one. That test disposes of both instances
  without adjudicating them.

- **A measurement is a measurement at an instant, and different quantities
  decay differently.** The consumer's fact count went from 15,170 to 16,407
  within one day, because ingestion never stops — it grows monotonically, so a
  stale figure is a floor. The upstream's model count moves in *both*
  directions (+821 and −391 over 18 days), so a stale figure there is neither
  a floor nor a ceiling and cannot be extrapolated at all. Every count in
  these documents carries its date; the ones that can move downward say so.

- **A number produced to fill a documentary hole is contaminated even when it
  is correctly measured.** It exists because a document needed it, not because
  a question needed answering — and that is how a real measurement becomes
  load-bearing for a claim nobody tested. A cost-by-class breakdown was
  offered to fill exactly such a hole here and was declined; the right time to
  run it is when a decision turns on it, and then it gets to be evidence.

  That completes the family: **inventing a fact, inferring one from adjacent
  fields, laundering an unmeasured claim through correct attribution, and
  producing a genuine measurement to fill a hole.** All four read as evidence.
  The last two are the hard ones, because nothing in them is false.

- **Cross-referencing is the highest-risk moment for propagating a soft
  claim.** The `context_over_200k` "two encodings of the same fact" inference
  was caught once and then reproduced twice while checking the documents
  against each other — because cross-referencing reads one's own prose for
  *consistency* rather than for *truth*, and a claim repeated in three places
  reads as three sources agreeing.

- **Inferring a semantic from a name is the cheapest laundering there is.**
  `context_limit_tokens` with a zero value has an obvious reading, and obvious
  readings get written down without a source line — by the person who owns the
  code, who is the last person anyone would ask for one. Fusiform's own rules
  are exposed to this: `limit.input`, `limit.output` and `cost.cache_read` all
  have obvious readings, and only two of the three were checked against the
  payload before a rule was written on them.

- **Agreement between two parties is not confirmation when both read one
  sample.** Two seats agreed on a row-level zero-limit rule drawn from a
  five-model set that had been selected to answer a different question. It
  read as independent corroboration and was one observation wearing two names
  — and the agreement is precisely what stopped anyone counting. The cheap
  defence is to ask **"what did you count?"** rather than "is that right?":
  the first exposes a selected sample, the second invites re-derivation from
  the same data.

- **Rewording is what you do when you believe the rule and doubt the sentence;
  measuring is what you do when you are willing for the rule to be wrong.** A
  reviewer flagged a normalization predicate as imprecise. Measuring the edge
  destroyed the predicate rather than sharpening it — 39 rows contradicted it
  outright.

  A wording fix would have been worse than no review at all: **a reworded rule
  that passed review is worse than an unreviewed one, because the review
  becomes evidence of correctness.** The unreviewed rule carries its
  uncertainty visibly; the reviewed-and-reworded rule has been laundered by
  the review. The review did happen and the record is accurate — the accuracy
  is what does the damage. Same mechanism as laundering through attribution,
  one level up.

- **A general flag gets reworded; a specific counterexample gets measured. So
  ask for the counterexample.** Reviewing this note produced both kinds, and
  only the second changed anything: the flag that named a specific model whose
  classification could not be predicted led to a measurement; a general
  "imprecise here" would have led to a better sentence.

  Checked against the day's record on both sides of the seam, this held
  without exception — every correction either party made was downstream of
  something specific the other named. That is not a weakness in either
  reviewer. **A specific counterexample is expensive to produce about your own
  work, because producing it requires already suspecting the thing, and cheap
  to produce about someone else's, because you arrive without the belief that
  generated it.**

  Practical form: when a review lands as general doubt, do not defend it and
  do not reword — ask **"which row would this be wrong about?"** That converts
  an unmeasurable flag into a measurable one and puts the work where it is
  cheap.

- **To test a belief, measure something chosen for having no relationship to
  it.** A carefully-reasoned set is reasoned *from a model*, so re-examining
  the set applies the same wrong model harder. Three defects on 2026-08-11
  arrived from adjacent work rather than from scrutiny: a tier defect out of
  an int/float question, a renderer-field misclassification out of counting
  non-LLM rows, and a second unread limit field out of enumerating struct
  fields. Adjacency is not the point — *independence from the belief under
  test* is.

- **A fix is an instrument too, and earns the same probing as the thing it
  replaced.** Probing the first commons fix found two residuals in the fix
  itself: an unchecked tier `type` discriminator, and an error variant that
  named no row in a 3.6 MB payload. Both were closed in a follow-up
  (`528b680`). The general form: the state of mind that writes a fix is not
  the state of mind that finds a defect, so a fix for a wrong-assumption
  defect should sweep for *other* assumed fields in the same parse before it
  ships.

- **When one variant in an error enum lacks the context its siblings carry,
  the omission is usually accident, not design.** A mechanically checkable
  audit form, and how the second residual was noticed.

- **A quarantine is a promise; an unread field is a fact.** BROCA's
  formulation. Fusiform's producer-side rule keeps renderer fields out of the
  served contract (§6); a consumer's structural guarantee is that its renderer
  decision reads a field set that cannot include them. Two independent
  barriers, neither load-bearing alone — the right shape for a failure that is
  silent.

- **An author cannot detect the ambiguity that matters, by construction.**
  This note said a `context_over_200k` "without a matching tier" is an error.
  *Matching* had two readings — matching threshold, matching rates — and the
  first shipped, rejecting 162 of 288 real models.

  The tempting lesson is "write unambiguous sentences", and it is wrong.
  Prose specifications contain ambiguous sentences everywhere and most resolve
  correctly, because a reader hits one that could go two ways and *stops*. This
  one produced no stop. It produced a confident reading, because the sentence
  and the implementation came from one understanding — so there were never two
  readings to choose between, and the ambiguity was invisible to the only
  person positioned to notice it.

  An author is therefore the wrong instrument for this class, the same way an
  author-run consistency pass is the wrong instrument for contradictions. What
  detects it is a second reader with no understanding to supply, or the data.

  **The data was available the entire time.** Seven distinct threshold values
  across 6,253 rows, and the correct list sat four paragraphs above the
  sentence in this document. It was never consulted, because the sentence did
  not feel like it needed consulting — which is the same tell as stating a rule
  across a seam without checking it. **The absence of doubt is not evidence,
  and it is weakest exactly where one belief authored both sides.**

- **The parser written to prevent a defect reproduced it.** Reading
  `tier.size` as a context threshold without checking `tier.type` is the
  commons defect; reading `context_over_200k`'s NAME as its threshold is the
  same act, and it shipped in the module whose purpose is to not do that,
  guarded by a rule stated three times in this note.

  The failure is not in knowing the rule; it is in recognizing the instance.
  Recognition matches against the labelled version, and the label was "reading
  `tier.size` without checking `tier.type`". `context_over_200k` was the same
  shape wearing a different name — and a name is exactly what recognition keys
  on.

  So the durable defence is a check that does not depend on recognition at all.
  Running the parser against 6,253 real rows found this without anyone
  recognizing anything, which is also how the original was found.
