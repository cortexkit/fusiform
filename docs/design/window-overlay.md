# Measured window truth: the overlay schema

**Status: PINNED 2026-08-13.** Interim format, designed to become the served
shape unchanged. Agreed in `#model-window-truth` with MC and SUBC.

MC merges this file plugin-side today. When fusiform serves it over subc, the
cell shape does not change — only the transport.

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

## 3. Cell shape

```json
{
  "provider_id": "openai-chatgpt-oauth",
  "model_id": "gpt-5.6",
  "fact": "window.enforced",
  "value": { "kind": "bracket", "at_least": 350000, "below": 372001 },
  "units": "provider_counted",
  "provenance": "measured_overflow",
  "source_ref": "mc-report:2026-08-13T09:14:22Z:a91f",
  "observed_at_ms": 1786600000000,
  "note": "400 at 372,001; largest success 350,000"
}
```

Every field is required except `note`.

### 3.1 `value` — a bracket, not a number

Three kinds, and the distinction is the point:

```json
{ "kind": "stated",  "value": 272000 }
{ "kind": "bracket", "at_least": 350000, "below": 372001 }
{ "kind": "unknown", "why": "placeholder_output_equals_context" }
```

**`stated`** — an authority named the number. A doc page, or an error body
carrying the provider's own limit.

**`bracket`** — witnessed, never stated. `at_least` is the largest attempt
known to succeed; `below` is the smallest known to fail. Either may be absent
when there is no witness on that side.

A 400 at 372,001 proves the ceiling is **below 372,001**. It does not prove the
ceiling *is* 372,000. Serializing that as a point is the placeholder class in a
subtler costume — precision the source never established. A consumer needing one
number takes `at_least`, which is conservative in the direction that never 400s.

A bisecting canary narrows a bracket over time with no schema change.

**`unknown`** — nobody has said, or what was said is a placeholder. `why` is a
closed vocabulary:

| `why`                                | meaning                              |
| ------------------------------------ | ------------------------------------ |
| `placeholder_output_equals_context`   | Grok 500k/500k class                 |
| `placeholder_zero`                    | `output: 0`, the upstream's "unstated" |
| `never_measured`                      | no evidence either way               |
| `retracted`                           | a cell was withdrawn; see `note`     |

**A detected placeholder becomes an explicit unknown, never a served bound.**
Per FIELD, not per row: 90 models mix real and placeholder limits in the same
row, so a row-level predicate discards real data.

### 3.2 `units` — and the asymmetry that makes it load-bearing

```
provider_counted     the provider's own tokenizer said this
consumer_estimated   a client's tokenizer estimated this
```

**A bound in one unit must never be compared to a point in the other**, and the
error is not symmetric. If a client sends what it estimates as N tokens and the
provider counts M:

- **Client undercounts (N < M).** The true ceiling C satisfies `C < M`, and
  recording `C < N` claims something narrower than the evidence supports. **The
  recorded bound may be false** — C could sit between N and M. It errs toward
  reserving too much, which is safe but is still a claim the evidence does not
  carry.
- **Client overcounts (N > M).** `C < N` is true and loose. Harmless.

So a `consumer_estimated` bracket is **safe-but-possibly-false in the `below`
direction**, and that is different from being true. Recorded here so a future
reader does not treat the two units as interchangeable because both are
integers.

### 3.3 `provenance` — five grades, strongest first

| grade                        | what happened                                          |
| ---------------------------- | ------------------------------------------------------ |
| `provider_asserted_runtime`  | the enforcing system named the limit in-band, in a 400 body |
| `measured_overflow`          | a client hit a wall and recorded the attempt            |
| `provider_asserted_doc`      | a first-party doc page states it                        |
| `catalog_claim`              | models.dev says it                                      |
| `unknown`                    | nobody has said                                          |

`provider_asserted_runtime` is MC's contribution and it outranks a doc page:
the system doing the enforcing is speaking, dated to the second. A doc describes
intent; a 400 body describes behaviour.

`measured_overflow` sits second because it is one client's observation at one
moment and could be quota, a transient, or a provider incident. **A measured
overflow is evidence, not a fact** — see §5.

### 3.4 `source_ref`

An opaque, resolvable string naming where the cell came from: a doc URL for
`provider_asserted_doc`, a report id for the measured grades. **Required on
every cell**, so a future reader can distinguish a cell someone decided from a
cell a rule produced. Unmarked cells become the generated-versus-authored
ambiguity that cost BROCA a morning to eliminate in their pins, and the seed
corpus is where it is cheapest to prevent.

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

## 6. File shape

```json
{
  "schema_version": 1,
  "minted_at_ms": 1786600000000,
  "minted_provider_ids": ["openai-chatgpt-oauth"],
  "cells": [ ... ]
}
```

`schema_version` moves on any change to cell shape. A consumer that does not
recognise the version **must refuse the file rather than merge what it
understands** — a partially-understood overlay silently drops the cells that
matter most, since new cells are added for facts the old schema could not carry.

## 7. What this dataset will never carry

No renderer selection and no endpoint. Access-path **identity** is a key and is
fine; an access path's base URL, auth shaping, or SDK adapter is not.

If the window dataset starts wanting those, it has crossed into BROCA's
territory and belongs there. This is the same fence as the catalog's, restated
for a new dataset rather than rediscovered.
