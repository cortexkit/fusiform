# Billing planes and rate states

Status: proposed, 2026-10-07. Not built. Ruling needed on the four rate states
(section 3) before anything ships.

## 1. The problem

Routing prices a route by a basket: the route's own tokens per class (fresh
input, output, reasoning, cached reads, cache writes) times that class's rate.
Two things make today's catalog insufficient for that:

- **An absent rate is ambiguous.** On the wire today, an absent `rate.*` key
  means "the upstream published no number". A reader cannot tell which of these
  is true: the class is billed at another class's rate, it is genuinely free,
  or nobody knows.
- **The same model bills differently per access plane.** `openai/gpt-6.1-sol`
  through an API key bills cache writes at their own rate; through a ChatGPT
  login (Codex) the rate card has no cache-write class at all. The catalog
  serves one row per `(provider_id, model_id)`, so it can state only one of
  these.

The decision that this belongs in fusiform was made by the operator: billing
quirks are fusiform's to state, so routing and broca price only from served
facts instead of guessing.

## 2. Planes

A plane is identified by **`(provider_id, auth_method)`**, not by a new
provider id. The same provider id is carried by API-key credentials and by
subscription logins in the vault, so the provider id alone cannot tell the
planes apart. `auth_method` is the vault's own closed vocabulary
(`ListAuthMethod` in claustrum): `apikey`, `chatgpt`, `oauth`, `antigravity`.
Adding a value is a breaking change to `credential.list_scoped`, so it cannot
appear silently.

- A credential with **no** auth method (GitHub Apps, signing keys, cookies) has
  no plane. A reader that cannot find a plane row for its pair **refuses**; it
  never falls back to the API plane, because the API plane's prices are a
  confident wrong answer for a subscription.
- The `apikey` plane is the provider's ordinary models.dev rows, unchanged.
- Planes models.dev already publishes as their own providers
  (`zai-coding-plan`, `kimi-code-plan-global` and 22 more) stay as they are.
- Curated planes, which models.dev does not carry:
  - `(openai, chatgpt)`: Codex under a ChatGPT plan.
  - `(anthropic, oauth)`: Claude Code under a Pro or Max plan.
  - `(google, antigravity)`: the existing Code Assist aliases.

A curated plane row serves the API row's limits, capabilities and reasoning
options through the alias mechanism (disclosed, never stored), and its rates
per class from the plane's billing rules (section 4).

A test pins the four `auth_method` strings and the absent case, so a vault
change that this design depends on reddens here.

## 3. Rate states

Each rate class on each plane is in exactly one of four states:

| State | Meaning | A basket reader does |
| --- | --- | --- |
| `priced(n)` | Published rate, minor units with exponent and currency, as today. | tokens × n |
| `stated_zero` | Explicitly free. Already on the wire. | 0 |
| `billed_as(class)` | Tokens of this class are billed at another class's rate on the same row: a cache write with no surcharge is billed as input. | tokens × that class's rate |
| `unknown` | Fusiform established that no source states how this plane bills this class. | refuses to price if tokens > 0 |

Absent keeps its current meaning: the upstream published nothing, and fusiform
has not curated the class. A reader treats it like `unknown`. The difference is
provenance: `unknown` is a curated claim with a source, absent is silence.

`billed_as` is the state a three-state design (number, not billed, unknown)
gets wrong. "No write charge" usually means "no surcharge", not "free": the
tokens are still billed as input. Pricing them at 0 undercounts by the full
input rate.

`billed_as` differs from `inherited_from`: inheritance borrows a number from
another provider's row, while `billed_as` points at a class on the same row.

### Subscription planes serve a quota proxy, not a bill

A subscription plane bills a flat fee, so its per-token money cost is not a
fact anyone publishes. What a reader needs from it is the *relative* weight of
each class against the plan's limits, which it then scales by its own
subscription multiplier (operator policy, not a catalog fact). Serving `unknown`
there would be true and useless: a reader refuses to price the route, ranks it
at maximum cost, and never picks it.

So a subscription plane serves the API plane's per-class rates as `priced`,
marked `inherited_from` with basis `quota_proxy` and the source that justifies
the proxy. This is a provenance marker on an existing state, not a fifth state,
and the marker is what stops a reader treating it as a bill. It is the same
shape the Code Assist aliases already serve for `(google, antigravity)`, where
the API price is borrowed under basis `alias`.

Where a plane's own source states a rule for a class, that rule wins over the
proxy: Codex's rate card has no cache-write class, so `(openai, chatgpt)` serves
cache writes as `billed_as(input)` and the other classes as the proxy.

Every curated cell carries provenance in the plan-price discipline:
`source_ref`, `established_by`, `established_at_ms` and `review_by_ms`. The
review gate fails the suite when a cell goes past its review date.

## 4. Billing rules, as sourced on 2026-10-07

From each vendor's own documentation. A cell marked "to source" ships only once
its row has a quote and URL.

| Plane | Class | State | Source |
| --- | --- | --- | --- |
| `(openai, apikey)` | cache write | `priced`, 1.25× input for GPT-5.6 and later | "For GPT-5.6 and later, cache writes cost 1.25× the standard, uncached input-token rate." Usage reports `input_tokens_details.cache_write_tokens`. platform.openai.com/docs/guides/prompt-caching |
| `(openai, apikey)` | cache write, models before GPT-5.6 | to source | models.dev publishes a write rate on 10 of 53 OpenAI models |
| `(openai, chatgpt)` | cache write | `billed_as(input)` | Rate card meters "input tokens, cached input tokens, and output tokens"; no write class. help.openai.com/en/articles/20001106. A Codex run's usage reports `cache_write_tokens` 0 |
| `(openai, chatgpt)` | input, cached input, output | `priced`, `quota_proxy` from `(openai, apikey)` | Same rate card: usage "is priced based on API token usage". The card's own credits per class are a candidate source for the subscription multiplier, which stays operator policy |
| `(anthropic, apikey)` | cache write, 5-minute | `priced`, 1.25× input | "5-minute cache write tokens are 1.25 times the base input tokens price." docs.anthropic.com prompt caching |
| `(anthropic, apikey)` | cache write, 1-hour | open | "1-hour cache write tokens are 2 times the base input tokens price." models.dev carries the 5-minute rate only. Core already sends 1-hour TTLs for some calls, so a basket priced at the 5-minute rate underprices them |
| `(anthropic, oauth)` | every class | `priced`, `quota_proxy` from `(anthropic, apikey)` | No numeric rule is published; plan limits are shared and "cached portions count less against your limits", which the API row's discounted cache-read rate reflects in the same direction. support.anthropic.com/en/articles/9797557 |
| `(xai, apikey)` | cache write | `billed_as(input)` | Pricing table has input and cached-input columns, no write column; "Prompt tokens (non-cached) \| Full prompt token price". docs.x.ai prompt caching |
| `(deepseek, apikey)` | cache write | `billed_as(input)` | Prompt tokens split into cache hit and cache miss, no write field: "It equals prompt_cache_hit_tokens + prompt_cache_miss_tokens." api-docs.deepseek.com |
| `(google, apikey)` | cache write, implicit caching | to source | Docs say savings pass on for hits; they do not say how the first request's tokens bill |
| `(google, apikey)` | explicit cache storage | out of scope | Billed per token-hour, which is not a per-token class |
| `(google, antigravity)` | every class | `priced`, borrowed under basis `alias` | Already served by the Code Assist aliases; no change |
| every plane | reasoning | to source | Whether reasoning tokens are already inside output, or billed in addition, decides whether a basket that adds them double-prices |

## 5. Where it lives

- A compiled-in curated file, `crates/fusiform-module/data/billing-rules.json`,
  loaded like the plan-price and creator tables: parsed once, malformed rows
  counted as rejects, reviewed by date.
- Applied at serve time and never written into era history, by the same rule
  that keeps inherited rates out of storage: a point-in-time read must serve
  what the upstream published, not a rule fusiform holds today.
- Served on current reads only. A historical read serves the raw rows.

## 6. Wire

`RateValue` gains `billed_as { class }` and a curated `unknown`, and curated
cells carry a provenance object. This is a breaking wire change. The consumer
reading baskets (routing) switches on all four states before the change lands,
and is told before it ships.

## 7. Open

1. Whether rates use the four states in section 3 (`priced`, `stated_zero`,
   `billed_as`, `unknown`), as opposed to three without `billed_as`. This is the
   operator's decision, and nothing is built until it is made.
2. Reasoning: subset of output or additive, per vendor. Needed before any
   reasoning cell ships.
3. Anthropic's 1-hour cache write: a second write class. Kept open so the
   underpricing of 1-hour writes stays visible.
4. Pre-GPT-5.6 OpenAI models and Google implicit caching: sources.
