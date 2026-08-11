# Finding: `experimental` fuses request bytes with a second pricing plane

Found 2026-08-11 while measuring models.dev for fusiform's normalizer.
Exposure measured independently by the `broca` seat on their own snapshot.

Severity: **latent across the fleet, with a named arming condition.** No
charge is wrong today. Two independent conditions keep it dormant, and both
are choices rather than structural guarantees.

## The shape

`experimental` is a per-model field in models.dev. Measured on the 2026-08-11
10:34 payload: **38 models carry it, holding 42 mode variants.**

```
keys under a mode:           cost 38,  provider 42
keys under mode.provider:    body 42,  headers 16
```

```json
"fast": {
  "cost": { "input": 30, "output": 150, "cache_read": 3, "cache_write": 37.5 },
  "provider": { "body": { "speed": "fast" },
                "headers": { "anthropic-beta": "fast-mode-2026-02-01" } }
}
```

One field, two planes: literal outbound request bytes, and a rate schedule
that overrides the model's base rate.

## The pricing half is material

**32 of the 42 modes price input differently from the model's base rate.**
Distinct multipliers observed: 2.0, 2.5, 6.0, 6.67.

| Model | Mode | Base | Mode rate | Multiplier |
| --- | --- | --- | --- | --- |
| `gmicloud/anthropic/claude-opus-4.7` | fast | $4.50/Mtok | $30/Mtok | 6.67x |
| `github-copilot/claude-opus-4.7` | fast | $5/Mtok | $30/Mtok | 6.0x |
| `orcarouter/anthropic/claude-opus-4.7` | fast | $5/Mtok | $30/Mtok | 6.0x |

This is not a rounding difference or a promotional tier. A consumer pricing a
`fast`-mode request against the base rate under-charges by up to 6.67x.

Two counts appear in this document and they measure different things: **38
models carry `experimental`** and all 38 have modes carrying a `cost` block,
but only **33 also carry a base `cost` object.** The 33 are the exposed set —
where a base rate exists to be silently wrong. The other 5 have no base rate,
so they resolve to `Unpriced(MissingRate)` and fail loudly instead.

## Why fusiform cannot simply serve the mode rate

Fusiform's quarantine rule (`docs/design/schema-and-store.md` §6) forbids
serving anything that selects a renderer, because an upstream edit to such a
field silently changes a consumer's outbound bytes. A mode's `provider` block
is exactly that: `body` params and `headers` that land verbatim in a request.

The mode's *rate* and the mode's *bytes* are the same object in the upstream's
structure. Serving the rate means serving the mode; serving the mode means
serving the bytes.

So v1 serves **base rates only** and records modes in raw provenance. This is
a **declared coverage limit**, not an oversight: a consumer metering a request
made in a non-base mode prices it up to 6.67x low, and 33 models carry modes.

The honest form matters here. A silently-plausible wrong price is worse than a
refusal, so where a consumer can tell fusiform which mode a request used, the
correct answer is `Unpriced(UnknownChargeBasis)` rather than the base rate.
Where it cannot, the base rate is served with the limit documented.

The eventual fix keeps the quarantine intact: a mode becomes a `RateCondition`
(§4) carrying only its discriminating identity — never its `provider` block —
so the rate is selectable without the bytes being served. Not built
speculatively; built when a consumer needs it.

## Fleet exposure, measured by broca on their own data

Five in-use suffixed pairs. Three carry divergent mode rates, all at exactly
2x base:

```
openai/gpt-5.6-luna-fast     facts= 43   base 0.2 -> mode 0.4
openai/gpt-5.6-terra-fast    facts=121   base 2   -> mode 4
openai/gpt-5.6-sol-fast      facts= 10   base 5   -> mode 10
ollama-cloud/deepseek-v4-pro             no base rate
xai/grok-composer-2.5-fast   facts=145   no base rate
```

Dormant for two **independent** reasons, both verified rather than assumed:

1. Broca's export lane carries token counts plus a charge-basis label and
   ships zero pricing — `priced_nanos` is absent on every suffixed fact.
2. All three divergent models resolve `subscription_included` under
   `chatgpt:openai`, so no per-token rate applies to them at all.

Suffixed facts by charge basis: **223 subscription_included, 147 unknown,
zero metered_api.**

## The discriminator does not survive into the ledger

A second gap sits underneath the first, on a different axis, and neither
producer nor catalog can close it alone.

The metering side prices from the billing model identity stamped on a spend
segment by the run engine. That identity is the base model; **nothing on the
fact distinguishes which mode the run used.** So even if fusiform served a
mode as a `RateCondition`, the metering side could not select on it — the
discriminator is lost before pricing happens.

This matters for how the failure presents. An unpriced segment is loud: it
carries a reason and shows up in the unpriced counts. A base-rate charge on a
mode-mode run is **silent and looks like a normal charge**, and it is wrong by
up to 6.67x.

So "when mode-conditional pricing matters" has a prerequisite outside both the
catalog and the ledger: the run engine must carry the mode onto the fact.
Building a `RateCondition` for modes before that exists would produce a
selector nobody can use. Recorded here so the dependency is visible; not
raised as a requirement on the run engine, because current traffic is
subscription and OAuth and a concrete requirement is worth more than a
hypothetical one.

## The arming condition

> A `-fast` or `-pro` run landing on a **metered_api** `(provider,
> auth_method)` pair.

At that moment the applicable rate lives in a field neither producer nor
consumer serves, and the consumer prices off base — silently low.

Nearest candidate: **`xai/grok-composer-2.5-fast`, 145 facts already,
currently `unknown` basis under `oauth:xai`.** A charge-basis mapping change
for that pair to metered arms this immediately, with real volume behind it.

### The flip is the trigger, not the fix — and the order matters

At the moment that pair flips to metered, the ledger starts pricing those
facts' successors at base rate **silently**, because the mode discriminator
still does not survive into the fact (see above). The flip creates the
concrete case that justifies the work; it does not do the work.

So the safe sequence is: **run engine stamps the mode onto the fact first,
then the charge basis flips.** Reversed, there is a window where real money is
up to 6.67x low and nothing reports it.

And "first" means further apart than it sounds. The requirement is that
**stamped facts exist before the flip**, not that the stamping code is
deployed before it. A stamp deployed at the same moment the basis changes
leaves every in-flight and recently-completed run unstamped — and those are
precisely the facts the metering ingest processes first. The safe gap is one
full export cursor's worth of stamped facts, not one deploy.

Stated because the flip is the visible event and would otherwise read as the
finish line.

Both dormancy conditions are reversible by ordinary decisions — a pricing
posture is a design choice and a subscription coverage is a mapping row.
Neither is a structural guarantee, so this is a thing to watch rather than a
thing that has been closed.

**The arming action is invisible from where it happens.** Editing a
charge-basis row, or adding pricing to an export lane, would not mention modes
in its commit message and would not look like it was touching pricing
correctness. That is what makes the ledger the discovery surface rather than
review: nothing at the point of change points here.

Which is the reason this finding names its arming condition rather than
describing a state. A note that says "currently dormant" ages into
reassurance. A note that says "the `(xai, oauth)` charge-basis row arms this,
with 145 facts behind it" fires when someone touches that row.

## What fusiform takes from this

- A field that fuses a policy-relevant value with a renderer-selection value
  cannot be split by the producer alone. The correct response is a declared
  coverage limit, not a partial serve that looks complete.
- Where a limit produces a plausible-but-wrong number, the honest output is a
  refusal with a named reason. `UnknownChargeBasis` already exists for exactly
  this, from the metering side's vocabulary.
- Latency between "latent" and "live" is a mapping row. Coverage limits should
  name their arming condition, so the transition is watchable rather than
  discovered in a ledger.
- A served discriminator is useless if it does not survive into the consumer's
  fact. Before building a selector, check that the thing being selected on
  reaches the point of selection — otherwise the producer ships a feature that
  cannot be exercised, which is a worse outcome than a declared gap because it
  looks closed.
