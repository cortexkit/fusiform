# Measured window truth: the overlay schema

**Status: PINNED 2026-08-13.** Interim format, designed to become the served
shape unchanged. Agreed in `#model-window-truth` with MC and SUBC.

MC merges this file plugin-side today. When fusiform serves it over subc, the
cell shape does not change — only the transport.

**The file lives at `crates/fusiform-module/data/window-overlay.json`, and until
fusiform serves it, THAT PATH IS THE DELIVERY MECHANISM.** A consumer in another
repository reads those bytes from there. Moving the file is a breaking change
for them that nothing in their build will report — they get a stale vendored
copy or a missing file, and neither says why.

Held by `the_overlay_is_where_the_consumer_expects_it`, which resolves the path
from the workspace root rather than relative to the test, so it fails on exactly
the move that breaks a consumer. `include_str!` is not sufficient: moving the
file and updating the include in one commit leaves the suite green.

---

## 1. What this dataset is for

models.dev is path-blind by construction: one row per `(provider, model)`, and
the same model reached by a different auth door can have a different window.
Four kinds of truth live outside it:

- **Per-access-path windows.** GPT-5.6 via the platform API is ~1.05M; via the
  ChatGPT/Codex backend the *advertised* window moved 372k → 272k while the
  backend still *admits* 372k.
- **Real output caps.** ollama-cloud enforces a hard 65,536-token output cap
  with an HTTP 400, regardless of the model's native capability.
- **Wall geometry.** Whether a provider truncates the prompt only, rejects the
  combined total up front, or meters input and output separately — which
  decides whether output reservation is a validity requirement or a margin.
- **Placeholders in the catalog.** `output == context` (Grok 500k/500k), and
  `output: 0` on 186 models. Rows that poison any consumer trusting them as
  bounds.

## 2. The two facts, and why one number cannot carry them

`window.advertised` and `window.enforced` are **separate facts with independent
provenance**, not one number with a confidence grade.

The case that forced this: Codex advertises 272k and the backend admits 372k.
Neither is wrong; they answer different questions. A consumer reserving output
needs **enforced**. A consumer displaying a limit to a user needs
**advertised**. A single cell would have to lie in one direction.

The same split applies to output:

| fact                | question it answers                             |
| ------------------- | ----------------------------------------------- |
| `window.advertised` | what does this path *say* its window is          |
| `window.enforced`   | what does the backend actually *admit*           |
| `output.advertised` | what does the path say the output cap is         |
| `output.enforced`   | what output length actually returns a 400        |
| `wall.geometry`     | `prompt_only` \| `combined` \| `separate`        |

Choosing between advertised and enforced is **interpretation and belongs in the
consumer**. MC's reservation math consumes enforced with a fallback to
advertised-minus-margin; that fallback is MC's code, not this dataset's.

## 3. The contract: `fusiform-window-overlay/v1`

MC proposed the envelope and it is ratified with four amendments, each below
with the reason. Field names are final as of this commit.

```json
{
  "schema": "fusiform-window-overlay/v1",
  "generated_at": "2026-08-13T13:02:44Z",
  "minted_provider_ids": ["openai-chatgpt-oauth"],
  "cells": [
    {
      "provider_id": "openai-chatgpt-oauth",
      "model_id": "gpt-5.6-sol",
      "facts": {
        "window.advertised": { "...": "FACT" },
        "window.enforced":   { "...": "FACT" }
      }
    }
  ]
}
```

```json
FACT = {
  "value":       VALUE,
  "grade":       "provider_asserted_runtime" | "measured" | "provider_asserted_doc"
               | "catalog" | "unknown",
  "units":       "provider" | "estimate",
  "boundary":    "Observed" | "Asserted" | "Corrected",
  "source_ref":  "https://... | mc-report:<id> | codex-rs@<sha>:<path>",
  "observed_at": "2026-08-13T09:14:22Z"
}
```

Every FACT field is required. A fact that is absent from `facts` was never
considered; a fact present with `grade: "unknown"` was considered and has no
answer. **Those are different and a consumer may act on the difference.**

### 3.1 AMENDMENT 1 — `value` is a bracket, not a scalar

MC's proposal had `value: number|string|null`. That silently drops the
bracket, which is the room's own decision and the reason it exists:

```json
{ "kind": "stated",  "value": 0 }
{ "kind": "bracket", "at_least": 0, "below": 1 }
{ "kind": "unknown", "why": "placeholder_output_equals_context" }
```

**The numbers above are deliberately absurd.** An earlier version of this
section used plausible ones — `at_least: 350000, below: 372001` — to illustrate
the Codex case, and within the hour a consumer had taken `below: 372001` into a
fixture as though it were measured. It never was: I invented it to show the
shape, and it had the exact form a real bracket takes, in a document whose
subject is provenance.

A plausible example in a schema document is indistinguishable from data once it
leaves the paragraph that framed it. Illustrations here use values that cannot
survive being mistaken for measurements.

A 400 at 372,001 proves the ceiling is **below 372,001**. It does not prove the
ceiling *is* 372,000. A scalar cannot say that, so serializing it as one is
precision the source never established — the placeholder class in a subtler
costume.

`at_least` is the largest attempt known to succeed, `below` the smallest known
to fail; either may be absent when there is no witness on that side. **A
consumer needing one number takes `at_least`**, which is conservative in the
direction that never 400s. A bisecting canary narrows a bracket over time with
no schema change.

`why` on an unknown is a closed vocabulary:

| `why`                               | meaning                                |
| ----------------------------------- | -------------------------------------- |
| `placeholder_output_equals_context` | Grok 500k/500k class                   |
| `placeholder_zero`                  | `output: 0`, the upstream's "unstated"  |
| `never_measured`                    | no evidence either way                 |
| `retracted`                         | a cell was withdrawn; see `source_ref` |

### 3.2 AMENDMENT 2 — five grades, keeping the one MC invented

MC's enum was `measured | asserted | catalog | unknown`, which drops
`provider_asserted_runtime` — MC's own contribution, and the strongest grade in
the set.

| grade                       | what happened                                            |
| --------------------------- | -------------------------------------------------------- |
| `provider_asserted_runtime` | the enforcing system named the limit in-band, in a 400 body |
| `measured`                  | a client hit a wall and recorded the attempt              |
| `provider_asserted_doc`     | a first-party doc page states it                          |
| `catalog`                   | models.dev says it                                        |
| `unknown`                   | nobody has said                                            |

`provider_asserted_runtime` outranks a doc page because **a doc describes
intent and a 400 body describes behaviour** — the system doing the enforcing is
speaking, dated to the second. Collapsing it into `measured` would lose the
distinction between *the provider told me its limit* and *I found the wall by
hitting it*, which have different reliability and different staleness.

### 3.3 AMENDMENT 3 — `model_id: "*"` accepted, and it asserts uniformity

Accepted: ollama-cloud's 65,536 output cap applies to every model regardless of
native capability, and writing it 200 times would be worse in every way.

**But a wildcard asserts that the provider enforces this REGARDLESS of model.
It does not fill a gap.** The tempting second reading — "the value for models I
have not measured yet" — manufactures a claim from absence, which is the defect
this whole dataset exists to remove. If a fact varies by model, it does not get
a wildcard; the models that lack it get no cell.

Resolution is **specific-beats-wildcard**, per fact rather than per cell: a
model with `window.enforced` and no `output.cap` takes its own window and the
provider's wildcard cap.

### 3.4 AMENDMENT 4 — output splits three ways, not two

MC proposed `output.cap` and `output.default`. The second is a real fact I did
not have and it is not a limit at all — it is what you get when you do not ask.
Kimi K3's widely-quoted "131k" is exactly this, misread as a context window.

But `cap` needs the same advertised/enforced split as `window`, for the same
reason: ollama-cloud publishes models claiming 1M output and enforces 64k.
Both are true statements about different systems.

```
window.advertised    what this path SAYS its window is
window.enforced      what the backend actually ADMITS
output.advertised    what this path says the output cap is
output.enforced      what output length actually returns a 400
output.default       what you get when you do not ask   <- not a limit
geometry             shared_upfront | shared_truncating | separate
```

`geometry` takes MC's vocabulary over mine — it names what the wall *does*
rather than what gets counted, which is the question a consumer is asking.

### 3.5 `units`, and the asymmetry that makes it load-bearing

```
provider     the provider's own tokenizer said this
estimate     a client's tokenizer estimated this
```

**A bound in one unit must never be compared to a point in the other**, and the
error is not symmetric. If a client sends what it estimates as N tokens and the
provider counts M:

- **Client undercounts (N < M).** The true ceiling C satisfies `C < M`, so
  recording `C < N` claims something narrower than the evidence supports and
  **the recorded bound may be false** — C could sit between N and M. It errs
  toward reserving too much, which is safe. Safe-and-possibly-false is not the
  same as true.
- **Client overcounts (N > M).** `C < N` is true and merely loose. Harmless.

So an `estimate` bracket is safe-but-possibly-false in the `below` direction
only. Recorded because both units are integers and nothing about their shape
warns that they are not interchangeable.

### 3.6 `boundary` — kept, because it is orthogonal to `grade`

`grade` says how strong the evidence is. `boundary` says **whose clock
`observed_at` is on**:

- `Observed` — fusiform saw this at that instant. The value may have been true
  earlier; this is when it was witnessed.
- `Asserted` — the source stated an effective date and `observed_at` is the
  source's claim, not fusiform's.
- `Corrected` — this cell replaces one that was wrong. A consumer should
  invalidate anything derived from the previous value.

A doc page reading "as of 2026-08-01, 272k" is `provider_asserted_doc` +
`Asserted`. The same page with no date, read today, is `provider_asserted_doc` +
`Observed`. Same grade, different clock, and only the second tells a consumer
that the value might predate its own timestamp by a year.

## 4. Access-path identity

`provider_id` is already an opaque string carrying access-path semantics
upstream — models.dev spells `alibaba`, `alibaba-cn`, `alibaba-token-plan` and
`alibaba-token-plan-cn` as four providers over the same models at different
prices. So this needs **no new key**; it needs a naming rule.

**Mint an id only when a fact DIVERGES between paths.** Where every fact is
identical, the bare provider id covers both. Vocabulary grows only when reality
forks, and a mint is itself the signal that someone measured a divergence.

**Where the upstream already spells a path as a provider id, reuse it.** No
synonyms: `ollama-cloud`, `github-copilot`, `antigravity`, `amazon-bedrock`.

**Spelling: `<upstream-provider-id>-<auth-door>`.** The qualifier names the
auth mechanism, not the product — products get renamed and mechanisms do not, so
`openai-chatgpt-oauth` survives a Codex rebrand and `openai-codex` would not.

**Minted today: `openai-chatgpt-oauth`**, because the bare `openai` id currently
serves both doors and the windows differ 1.05M vs 372k. That is the collapse
this dataset exists to remove.

Reserved, unminted until a fact diverges: `anthropic-claude-oauth`,
`google-gemini-oauth`.

**Minted ids are marked as fusiform-minted** in `minted_provider_ids` at the top
of the file. A future audit against models.dev must be able to tell "this id
should not resolve upstream" from "this row is stale", and without the marker it
reads them all as stale.

## 5. The write path is separate from the serve path

MC submits **reports**; fusiform mints **cells**. A consumer never writes the
dataset it consumes.

The reason is stronger than avoiding a feedback loop: **a measured overflow is
evidence, not a fact.** A 400 is one observation by one client at one moment,
and it could be quota exhaustion, a transient, a provider incident, or a genuine
ceiling. Turning it into a served cell is an adjudication, and an adjudication
needs an owner who can be wrong and be corrected. A misclassified 400 written
directly becomes a false ceiling every other consumer inherits, with nobody
owning the disagreement.

Report shape, from MC's three evidence flavours:

```json
{
  "provider_id": "anthropic",
  "model_id": "claude-sonnet-4-5",
  "access_path": "api",
  "status": 400,
  "matched_pattern": "anthropic_prompt_too_long",
  "extracted_limit": 200000,
  "attempted_tokens": 214311,
  "units": "consumer_estimated",
  "geometry": "prompt_only",
  "observed_at_ms": 1786600000000,
  "reporter": "magic-context@0.4.1"
}
```

`extracted_limit` is `provider_counted` when present; `attempted_tokens` carries
the reporter's units. **Both are recorded** — the pair is what lets a bracket
narrow, and dropping the succeeding attempt is what makes a bracket collapse
into a false point.

## 6. Report shape: `fusiform-window-report/v1`

MC submits these; fusiform mints cells. Pinned so the context.db backlog can
export today.

```json
{
  "schema": "fusiform-window-report/v1",
  "reporter": "magic-context@0.4.1",
  "reports": [
    {
      "report_id": "a91f3c",
      "provider_id": "anthropic",
      "model_id": "claude-sonnet-4-5",
      "status": 400,
      "matched_pattern": "anthropic_prompt_too_long",
      "extracted_limit": 200000,
      "attempted_tokens": 214311,
      "largest_success": 198002,
      "units": "estimate",
      "geometry": "shared_truncating",
      "observed_at": "2026-08-13T09:14:22Z"
    }
  ]
}
```

`extracted_limit` is `provider` units when present regardless of the report's
`units`, which describes `attempted_tokens` and `largest_success`. **Send
`largest_success` whenever you have it** — dropping the succeeding attempt is
what collapses a bracket into a false point, and it is unrecoverable afterwards.

`report_id` is the reporter's, and it becomes the cell's `source_ref` as
`mc-report:<report_id>`.

## 7. File shape and version refusal

```json
{
  "schema": "fusiform-window-overlay/v1",
  "generated_at": "2026-08-13T13:02:44Z",
  "minted_provider_ids": ["openai-chatgpt-oauth"],
  "cells": [ ... ]
}
```

`minted_provider_ids` lists every id fusiform invented rather than received
from the upstream, so an audit against models.dev can tell "this id should not
resolve there" from "this row is stale".

**A consumer that does not recognise `schema` must refuse the file, not merge
what it understands.** A partially-understood overlay silently drops the cells
that matter most, because new cells get added for facts the old schema could
not carry — so the failure lands precisely on the newest and most consequential
data.

### 7.1 Which is exactly why an ADDITIVE reason value does not bump the version

The refusal rule and the closed reason vocabulary are both correct, and put
together they have a trap. Adding one `why` value is additive: a consumer that
does not know it skips that cell and merges the rest correctly. But if the
addition bumps `schema`, the refusal rule fires at the FILE level — and a
consumer loses every cell over one value affecting one of them.

Found 2026-08-13 by bumping to `v1.1` for the
`not_single_valued_at_key` addition and then re-reading this section.
The bump was the tidy-looking move and it would have cost a consumer 8 good
cells to protect them from 1 they could already skip.

**The rule: a value added to a closed vocabulary does not move `schema`.** What
moves it is a change to the CELL SHAPE — a new required field, a renamed field,
a changed meaning — because those are the changes an old consumer cannot skip
past. The distinction is whether the ignorant consumer's degradation is
per-cell or whole-file.

The reason vocabulary being closed is what makes this safe: an unrecognised
value is a REFUSED CELL rather than a merged one, so the addition degrades
loudly in the one place it applies and nowhere else.

### 7.2 `grade` describes the evidence for the cell's ASSERTION, not for a number

An unknown's assertion depends on its reason, and the two kinds are graded
differently:

- **Evidence-absence reasons** (`never_measured`, `placeholder_*`, `retracted`)
  assert *nobody has established this*. There is nothing to grade, so the grade
  must be `unknown` — anything else claims evidence the value denies.
- **`not_single_valued_at_key`** asserts *the key cannot hold one fact*.
  That is a positive claim resting on evidence, so it carries the grade of that
  evidence and a real `source_ref`. Grading it `unknown` would say nobody
  established it, which is what the reason denies.

So a cell may legitimately have `kind: "unknown"` and `grade: "measured"`. The
two fields answer different questions and the first case of their divergence is
OpenRouter's geometry.

## 7. What this dataset will never carry

No renderer selection and no endpoint. Access-path **identity** is a key and is
fine; an access path's base URL, auth shaping, or SDK adapter is not.

If the window dataset starts wanting those, it has crossed into BROCA's
territory and belongs there. This is the same fence as the catalog's, restated
for a new dataset rather than rediscovered.

## 8. Consumer guidance: merge precedence

Recorded here at SUBC's request rather than living in one consumer, because
every consumer of this dataset faces the same ordering question and MC's answer
is right. From strongest to weakest:

```
user config  >  runtime provider hooks  >  overlay  >  models.dev
```

with the **detected-overflow lane capping any of them downward**.

Two things about that shape are worth stating, because a consumer reproducing
the order without them will get the behaviour wrong:

**The overlay outranks models.dev but not a runtime hook.** A hook speaks for
the live connection — it knows which auth door is open and what the server just
said. This dataset is a recorded belief, and a recorded belief must never
override a running system's own report about itself.

**The overflow cap is one-directional and that is deliberate.** A witnessed 400
may lower any layer's number; it may never raise one. Raising on evidence of a
*success* would be inferring a ceiling from a floor — the same
absence-becomes-claim defect as the wildcard's tempting second reading (§3.3).

## 9. Method for the sweep

The research method is MC's, in `#model-window-truth` post 13, in their yield
order. Cited rather than restated so it does not drift from the version its
author maintains:

1. **First-party client source** for OAuth and backend paths. The highest-yield
   lane, and the only one that catches advertised-versus-enforced splits — the
   window is often a client constant or server-delivered there and appears in
   no catalog.
2. **Provider error formats** as enforcement ground truth. MC's
   `overflow-detection.ts` carries ~20 providers' patterns already tagged
   prompt-only versus combined: a free geometry classifier seed.
3. **Official docs** for geometry class and reasoning-token accounting, watching
   version gates — Anthropic's geometry changed at 4.5.
4. **GitHub issues** on serving infrastructure for caps no doc states.
5. **Cross-aggregator disagreement** as a research queue: where models.dev,
   openrouter and bedrock disagree on one model, one of them is a placeholder
   or a path difference, and both are cells worth minting.

Priority is by fleet exposure, delivered in batches rather than held for a
complete file: anthropic (+oauth), openai (+chatgpt-oauth), google
(+antigravity), ollama-cloud, xai, moonshot, deepseek, openrouter, groq,
mistral, github-copilot, amazon-bedrock, then the tail.

**This dataset cannot be complete and must not be delivered as though it were.**
Some windows are knowable only by measuring against a live endpoint. Every model
whose window cannot be sourced gets an explicit `unknown/never_measured` cell
rather than silence, so a consumer can tell "nobody has established this" from
"nobody has looked".

## 10. Standing checks before minting from a bulk source

Two lanes that look like evidence and are not. Both were found the hard way on
2026-08-13, within an hour of each other, and both would have passed a
provenance check.

**Look at the value distribution first. Repeats at a small number of exact
points mean you are reading a configuration, not a measurement.**

MC's usage-reported limit lane held 3,900+ rows for the Codex path — real rows,
right path, right units, honestly recorded by a lane that genuinely observes
traffic. They clustered at exactly 244k / 308k / 372k, which are MC's own
override eras. The lane was echoing resolved harness config back at itself.

Provenance would have passed: every link in that chain is sound. The dataset was
right about what it recorded and wrong about what it was evidence of. What
catches it is shape — external systems produce distributions, configurations
produce repeats — and the shape test needs no knowledge of the override history
that caused it.

**For a number lifted from source, read the enclosing function's PURPOSE, not
just the line.**

`272_000` is really in `codex-rs`, quoted accurately, and it is the fallback
constant for unrecognised model slugs. The tell was one field away:
`used_fallback_model_metadata: true`. A real number in a real source, doing a
different job than the sentence quoting it implies — indistinguishable, on
arrival, from an invented one.

**Do not mint a cell for anything a harness applies to itself.** Codex clamps
its own usable window to 95% and auto-compacts at 90%. Those are real,
server-configurable, and not provider walls. A consumer computing its own
reservation on that path stacks a second clamp on the first, which is worth
knowing when diagnosing early compaction and is not a fact about the provider.

**Known contaminated lanes**, so no future transcriber rediscovers them as a
find: MC's usage-reported limit rows for the Codex path (config echo, above).

## 11. Why placeholder detection does NOT belong in the normalizer

Measured 2026-08-13 against the live document: **1,175 of 6,292 models publish
`output == context`** across 111 providers, plus 186 publishing `output: 0`.
Together 21.6% of the catalog. Of the equal-output rows, **560 are provably
placeholders** — a sibling provider publishes the same model with a distinct
smaller output, so the equality is contradicted by the upstream's own data
rather than merely suspected:

```
glm-5    crof says 202752 == 202752  |  zhipuai says output=131072 of 204800
glm-5.2  digitalocean says 262144 == 262144  |  zhipuai says output=131072 of 1000000
```

Fusiform already collapses `output: 0` to absence in the normalizer, so
extending the same treatment to `output == context` looks like consistency. It
is not, and the reason is decisive.

**A normalizer change would write 1,175 era rows recording a change that did not
happen upstream.** Ingest diffs normalized facts against stored eras; flipping a
value to null is a change, so the next poll after such an edit would tombstone
1,175 facts and stamp them with an observation boundary. Every one of those eras
would assert that models.dev stopped publishing an output limit at that instant.
It did not. **Fusiform's own reading changed, and the history would record it as
an upstream event** — indistinguishable, a month later, from a real withdrawal.

That is worse than the placeholder. A wrong value in a served row is a wrong
value; a wrong era is a falsified observation, and the whole point of the
observation/era split is that those never blur.

The zero rule predates any stored history, so it never had this problem. The
distinction is not which rule is more justified — it is that **one was applied
before there was a history to falsify and the other would not be.**

So placeholder judgement lives here, in the overlay, where it is a stated
correction carrying its own provenance and cannot masquerade as something the
upstream did.

### 11.1 What that implies about which cells are worth minting

The rule `output >= context implies placeholder` is one line, and it is already
in the consumer's own spec. A cell asserting it tells a consumer nothing it
could not compute from the catalog row in front of it.

**The test for whether a cell earns its place: does it carry something the
consumer cannot derive?**

- A placeholder flag: **derivable**. Not worth 1,175 cells.
- A real enforced value behind a placeholder — ollama-cloud's 65,536 against an
  advertised 1,048,576: **not derivable from anything**. Worth a cell on its own.

Shipping the derivable ones would swamp the irreplaceable ones at a ratio of
about 150 to 1, and a dataset whose bulk is recomputable trains its consumer to
skim it. The measurement above is reported so a consumer knows the scale of the
problem; the cells are reserved for what only measurement can supply.

**Settled with MC 2026-08-13: ZERO derivable cells, not few.** Their consumer
applies the equal-output rule itself, per field and both spellings, committed
with mutation tests. Their argument for uniformity is stronger than the
ratio one:

> The moment the dataset contains ONE derivable cell, a consumer cannot know
> whether the absence of a flag means "not a placeholder" or "fusiform did not
> ship the derivable one here."

Shipping zero makes absence mean one thing again — the same absent-versus-unknown
discipline the schema already pays for elsewhere, applied to the dataset's own
contents. Held by `no_cell_states_something_the_consumer_can_derive`, which also
asserts the enforced value BEHIND a harmful advertisement survives, so the rule
cannot quietly delete what it exists to protect.

SUBC's harm-first priority rule is unaffected: what made the ollama-cloud row
lead batch one is the enforced 65,536 behind an advertised 1,048,576, and that
cell stays. **Harm ranking decides which measurements to chase first; it does
not license shipping recomputable flags.**

## 12. `geometry` bundles two dimensions, and the fourth combination is real

Recorded 2026-08-13, NOT acted on. The three-value enum is ratified and a
consumer has implemented against it; this is the boundary condition it does not
express, written down before someone rediscovers it as a bug.

`geometry` answers two independent questions with one value:

| what the wall counts | over-window output   | enum value          |
| -------------------- | -------------------- | ------------------- |
| combined             | rejected up front    | `shared_upfront`    |
| prompt only          | truncated            | `shared_truncating` |
| prompt only          | independent quota    | `separate`          |
| combined             | truncated            | **no enum value**   |

The fourth row is reachable today. The seed corpus records OpenAI's platform API
as `shared_upfront` with the parenthetical "400 with `truncation: disabled`" —
so with truncation enabled the same provider counts the combined total and
truncates rather than refusing. Same wall, different behaviour on hitting it.

**Which makes geometry partly a property of the REQUEST, not only the provider.**
A consumer setting `truncation: auto` gets different behaviour from the same
endpoint, and no cell keyed on `(provider, model)` can express that.

Not proposing a change. The enum covers every combination measured so far, the
fourth is request-controlled rather than a provider fact, and splitting a
ratified contract after a consumer has built against it would cost more than the
gap. What is written here is the trigger: **if a report ever shows a combined
wall that truncates, the enum is the thing that is wrong, not the report.**

This is the same shape as the advertised/enforced split that started this
dataset — two facts wearing one number — caught early enough to be a note rather
than a correction.

### 12.1 Second instance, with a named mechanism

Anthropic's live documentation (read 2026-08-13) separates the two dimensions
explicitly, and the separation is exactly the one the enum collapses:

> If the input alone already exceeds the model's context window, the API returns
> a 400 `invalid_request_error` ("prompt is too long") **on every model**. On
> Claude 4.5 models and newer, if input tokens plus `max_tokens` exceeds the
> context window size, the API accepts the request [and] stops with
> `stop_reason: "model_context_window_exceeded"`. On earlier models, the API
> returns a validation error instead. To opt in to the
> `model_context_window_exceeded` behavior on those models, use the
> `model-context-window-exceeded-2025-08-26` beta header.

The wall is prompt-only on every Anthropic model, always. What varies is the
over-window OUTPUT behaviour — by model version, **and by a request header on
older models**.

So geometry is request-dependent here through a named, documented mechanism
rather than by inference. Two providers now, reached independently: OpenAI via
`truncation`, Anthropic via a beta header. That is no longer a boundary
condition; it is how the space is shaped.

Still not changing the contract. The trigger in §12 stands, and this is the
evidence that it will fire.

### 12.2 No geometry wildcard is justified for any provider

Checked while planning to extend geometry with `model_id: "*"`. Every
documented geometry names a **model**, a **version**, or a **request mode** —
none names a provider:

- **anthropic** — version-gated at 4.5, header-overridable below it.
- **xai** — measured on grok-4.6 alone. One model is not a provider.
- **openai platform** — the `shared_upfront` evidence is specific to
  `truncation: disabled`.
- **google** — contradicted by its own documentation (§11.2 below).

The uniformity of that result is the finding. It suggests geometry may be keyed
at the wrong granularity — `(provider, model)` cannot express a fact that
depends on a header the consumer sets. Recorded, not acted on.

## 13. Pinning a string across a seam

Learned 2026-08-13 by causing the collision this section prevents.

A vocabulary value was ratified three times in one exchange and the three
ratifications disagreed, because each named the authority differently:

- *"the ratified spelling"* — resolved to whichever message the reader had open.
- *"copy from the commit, not the thread"* — correct advice, and it pointed at a
  commit that was replaced while the instruction was in flight.
- *"the executed one"* — re-resolved under the reader.

Two consumers ended up incompatible for several minutes. **Nobody was resolving
a stale value; each party resolved a live one, at a different instant.** That is
what makes the failure invisible — every reader sees something real, current and
correct.

**The rule: a cross-seam string ratification must name an immutable authority —
a specific commit SHA plus the string verbatim, declared terminal.** Anything
softer re-resolves. `HEAD`, "the committed spelling", "my working tree" and "the
executed one" are all pointers, and a pointer is a subject that re-resolves under
whoever reads it.

The corollary is uncomfortable and worth stating: **this collision was caused by
the anti-drift discipline working.** Both parties moved to correct a divergence
they had each correctly identified, and the moves crossed. Care is not the
defence here; naming an immutable authority is.

The verification is mechanical and takes one command — read the string out of
the committed object rather than the working tree:

```
git show <sha>:<path>
```

Not `grep`, not the file on disk, not memory. The working tree is a different
subject from the commit, and on the day this rule was learned they differed.

## 14. Gateways, and why being one does not settle anything

`not_single_valued_at_key` was minted for OpenRouter because it routes to
heterogeneous backends. The obvious generalisation — *find the other gateways
and refuse their cells too* — is wrong, and the reason matters more than the
list.

### 14.1 Detecting a gateway takes several tests, and none is complete

Measured 2026-08-13 against the live document. Two independent tests, each blind
where the other sees:

| test | catches | misses |
| --- | --- | --- |
| vendor-namespaced ids (`anthropic/claude-opus-5`) | openrouter (348 ids, 59 vendors) | bedrock (dots), copilot (bare names) |
| exact-id intersection with first-party catalogs | github-copilot (5 origins), ollama-cloud (3) | openrouter (namespacing defeats exact match) |

Neither catches `amazon-bedrock`, whose 116 ids use a third convention
(`global.anthropic.claude-opus-4-8`).

**So a provider passing every test is not established as first-party** — it may
simply use an id convention no test knows. The tests have one-directional power:
present evidence proves multi-vendor, absent evidence proves nothing. Recorded
that way rather than as a classifier.

### 14.2 The criterion is who OWNS THE WALL, not who owns the models

ollama-cloud is a gateway by the directional test — it carries models from at
least three other first-party catalogs. Applying "gateway implies
not-single-valued" would have refused its cell.

**That cell is the most valuable one in the batch.** Its 65,536 output cap is
ollama's own serving-infrastructure ceiling, applied regardless of whose weights
are behind it, and it stands against an advertised 1,048,576 that would
otherwise have a consumer reserving 16x the real limit.

The distinction:

- **OpenRouter forwards** — the wall that fires belongs to whichever upstream
  served the request, so the key holds no single fact.
- **ollama-cloud imposes** — the wall is theirs, uniform across the models they
  serve, so the key holds one fact and it is measurable.

Both carry other people's models. Only one of them lets other people's walls
through. **The question is never "does this provider originate the model" but
"whose wall fires when the request is too big".**

A wrong answer here is expensive in both directions: refusing ollama-cloud loses
the batch's best cell, and accepting an OpenRouter measurement mints a value
correct for one routing decision and wrong for the next.
