# Plan price plane

Status: **design, agreed with QTA 2026-09-18, not built.**

A curated table of subscription list prices, keyed `(provider_id, tier)`, with
era history so a past window can be priced at the price in force then.

Requested by QTA for a subscription-to-API cost multiplier. This note records
the design and, more importantly, the reasoning that survived being wrong twice.

## What it is

    (anthropic, max_20x)  ->  200 USD / month
    (openai,    pro_200)  ->  200 USD / month

US list prices, deliberately. Prices vary by country and the output is a
multiplier rather than an accounting figure, so a consistent denominator matters
more than a locally exact one. **That choice is recorded in the plane**, not
assumed by readers.

## Why fusiform, after I refused it once

I declined this on two grounds and both were wrong. Recorded because the
corrections are the useful part.

**"The key uses a foreign provider vocabulary."** I measured insula's `provider`
field (`codex`, `claude` — zero models in this catalog) and reasoned about the
key as though that were it. The field the key actually uses is `apiProvider`,
published since August, carrying models.dev slugs. Verified on the live wire:
22 of 22 rows carry one, and every slug resolves here — including
`alibaba -> alibaba-coding-plan`, which picks the plan plane over the API twin.

**"Window-overlay's machinery needs a model dimension."** It carries
`model_id: "*"` cells already. The half of that objection which survives is
narrower and worth keeping: the schema accepts a wildcard and the *serving* path
skips it, so provider-level facts exist as a data shape and not as a queryable
one. A tier price needs a new serving path either way.

**The argument that decides it** is one I had already made to QTA as a hazard
and then failed to apply: a subscription price is a **dated fact that rots with
no version**. A model reprice appears here as an era with a bounding window; a
plan reprice leaves no trace anywhere — the old number stays plausible and the
page simply reads differently one day. A compiled-in constant answers "what the
binary was built with"; the question is "what was in force then".

`BoundaryKind::Asserted` has existed since day one for exactly this and had
**never been used** — 73,032 observed and 68,026 seed eras, zero asserted —
because every source until now publishes by being fetched. This is the first
fact that arrives with a date and no fetch behind it.

## Why a git-tracked file rather than an operator verb

**Fusiform's store is per-host.** The path arrives in HELLO_ACK from the
daemon's own policy; there is no shared store. So an operator verb would put a
fleet-wide public fact into host-local state, and the same price would differ
between machines with nothing able to detect it.

QTA's asymmetry is what makes the release cost acceptable: **a release delays
availability, not correctness.** Because the boundary is the source's effective
date, a price committed on the 20th for a reprice on the 1st lands with a
boundary of the 1st, and every past window computes correctly the moment it
exists. The late-committed era is *indistinguishable* from a timely one, because
neither carries fusiform's observation instant — so there is no residue to
explain away, which is the property that usually makes "we will backfill it" a
lie.

And a commit is the review. The characteristic failure of a hand-typed price is
an order-of-magnitude typo that looks completely healthy: `$20` where `$200`
belongs produces a multiplier wrong by 10x with nothing anomalous about it. A
verb records provenance; a diff invites someone to look at the number before it
lands.

## Schema sketch

A separate table, **not** the era table. `era.model_id` is `NOT NULL`, so a tier
row would need a sentinel, and a sentinel in a key column is where ambiguity
starts. Worse, plan rows keyed into the model space would surface as
pseudo-models on `catalog.get` for every consumer, to serve one.

```sql
CREATE TABLE plan_price_era (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id     TEXT    NOT NULL,   -- a models.dev slug, same vocabulary as era
    tier            TEXT    NOT NULL,   -- the provider's literal string, unnormalised
    -- NULL means "tier observed, no published price": a positive refusal.
    minor_units     INTEGER,
    exponent        INTEGER,
    currency        TEXT,
    period          TEXT    NOT NULL,   -- "month"; stated rather than assumed
    -- Always 'asserted' here. The source states an effective date and there is
    -- no fetch behind it, so degrading it to a polling cadence would be a lie
    -- in the direction this whole plane exists to avoid.
    boundary_kind   TEXT    NOT NULL CHECK (boundary_kind = 'asserted'),
    -- The vendor's effective date. There is deliberately NO column for when
    -- fusiform committed the row, and that absence is load-bearing: it makes a
    -- price committed late INDISTINGUISHABLE from one committed on time, so a
    -- backfilled reprice leaves no residue to explain away and every past
    -- window computes correctly the moment the row lands.
    --
    -- Adding an observation timestamp "for completeness" would destroy that
    -- property without looking like it changed anything.
    boundary_at_ms  INTEGER NOT NULL,
    established_by  TEXT    NOT NULL,   -- who sourced it
    established_at_ms INTEGER NOT NULL, -- when they looked
    -- Per ROW, never per file. See the derivation below: this is a ceiling on
    -- how long a wrong number may circulate, NOT a sampling rate.
    review_by_ms    INTEGER NOT NULL,
    source_ref      TEXT    NOT NULL,   -- the page the number came from
    refusal_reason  TEXT,

    -- Stated as a constraint rather than left to a comment, for the same reason
    -- as the boundary_kind CHECK above: it makes the two incoherent states
    -- UNREPRESENTABLE rather than merely unwritten. A priced row carrying a
    -- refusal, and a refused row explaining nothing, are both things a careful
    -- writer would not produce and a careless one would.
    CHECK ((minor_units IS NULL) = (refusal_reason IS NOT NULL))
);
```

Money as minor units with an explicit exponent, matching `Amount` — a price is
money and must not become a float here for the same reason it must not there.

## Quota value: a curated fallback, not a price era

`quota_value` on a priced `(provider_id, tier)` row measures **list-price USD of
usage per USD of subscription capacity consumed**. Routing measures this live
on installs with astrocyte; elsewhere this optional value is a static fallback.
Absence means not curated, hence unknown, **never zero**. `not_established` is a
positive refusal with a reason and review date, not an omitted measurement.

Only two sources are allowed: a **dated astrocyte measurement** or a **figure
the operator states**. Fusiform never estimates one. A measured value records
its basis, token mix, establisher, source, as-of date and review date. The mix
matters: agentic coding with heavy cache reads need not buy the same list-price
usage as chat. A range preserves observed variation rather than pretending one
account or day established a universal constant. A consumer needing one number
takes **`low`**, because undervaluing the subscription is the conservative
failure direction. Endpoints are exact integer decimals, not floating point.

The compiled file is joined onto current store rows **per request**, like the
billing planes. Quota values are not stored in `plan_price_era`, nor included in
the claim `append_plan_prices` diffs: changing only a multiplier must write zero
price eras. A retired key has no multiplier, even if the compiled file still
names it; without a subscription price there is no dollar to divide by.

Measured values have a **30-day review** policy: capacity, usage mix and the
measurement series can drift without a price change. The shipped first review
is the next month's same date (2026-10-09 to 2026-11-09). The time-triggered gate
checks quota-value deadlines as well as price deadlines, including refusals,
and names `quota_value` when it fires. A refusal's review asks whether a
measurement or operator statement has now become available.

## Rules this plane inherits, and one it adds

- **Row-level review-by, never file-level.** A file date gets bumped wholesale,
  so it goes stale as a unit and stops answering "when did anyone last look at
  *this* price" — which is the question when one vendor reprices and the others
  do not.

  **The interval is derived from tolerance, not from frequency, and the
  derivation must travel with the number.** The instinct is to set it from how
  often plan prices change — rarely, so annually. That is wrong, because
  *nothing reveals a reprice*: every other fact in this store has a fetch that
  would eventually contradict a stale value, and this one has the review and
  nothing else. So the interval is a **ceiling on how long a wrong number may
  circulate**.

  And the damage is retroactive rather than current: the multiplier for *every*
  window in the stale period was computed against the wrong price, so a
  $200 → $250 reprice missed for a year silently corrupts a year of history by
  25%, all of it looking healthy. **Quarterly**, held loosely on the number and
  firmly on the derivation — because the first person to find the review tedious
  will reason from frequency and lengthen it, and they will be right about the
  frequency.
- **Establisher and date from row one.** Retrofitting never happens, because the
  first ten rows are obviously true when you write them.
- **Refusal cells name why**, and sourcing the first four rows found **three**
  kinds where the design anticipated two:

      tier not observed                      nobody has looked yet
      tier observed, no published price      looked; the vendor publishes none
      tier string does not resolve to a       the price IS published, and the
        published price tier                  JOIN from the API's string is not

  Only the last two are completed work, and the third was unanticipated. It is
  the sharpest of them: OpenAI runs two Pro tiers and names them *by price* on
  its own help page, while the API reports `plan_type: "pro"` for both. The
  figure exists; nothing upstream says which tier that string means.

  Recording either figure would be wrong by 2x for half of all accounts, in the
  **flattering** direction, with no error and no anomaly — a wrong join is a
  number identical in shape to a right one. Refusing fails loudly instead: no
  row, no multiplier, someone asks why. A third party's display table maps it,
  which is their inference rather than an upstream statement, and is the same
  class of unpublished join this catalog already refuses for model renames.

  Collapsing any of the three produces a backlog indistinguishable from a
  completed one.
- **Zero derivable cells.** Nothing here is computable from anything, so the
  discipline costs nothing to maintain — and shipping one derivable cell would
  destroy the meaning of absence for every other.
- **A changed tier string must not resolve to the old price.** Exact match only.
  A renamed tier *may be a repriced plan*, so failing into "tier observed, no
  published price" converts an unannounced vendor change into a prompt. This is
  the same refusal made for model renames upstream, where `kimi-for-coding/k3`
  is not joined to `kimi-code-plan-global/k3` despite identical ids and one poll
  between them. In both cases the thing that looks like helpfulness is a
  fabricated continuity.

## The review date needs a reader, or it is a comment with a timestamp in it

`review_by_ms` is the only defence this plane has: a plan reprice leaves no
trace anywhere, so nothing can ever contradict a stale row. A date nobody reads
fails in the quiet direction — the rows keep serving, the multipliers keep
computing, and staleness accumulates behind a field that *looks* like it is
managing the problem.

**So an overdue row fails the test suite.** Not a health metric: a metric
requires someone to look, and the failure mode being guarded against is nobody
looking.

The mechanism that makes this work already exists and was built for this exact
class. CI runs on push *and* on a 06:00 schedule, and the scheduled run's own
comment states why it is there: *"a push gate asks is this change good; only a
scheduled run asks is what already landed still good against the world as it is
now."* A review date passing is precisely that — **the world changes while the
repository does not.** A push-only gate would never fire on a row that went
overdue during a quiet week, which is when review dates actually lapse.

This is the repo's first *time-triggered* gate. Every other fence here fails
because data or code changed; this one fails because a date passed with nothing
touched. That is not a defect in the design, it is the point: the facts in this
plane rot without anyone touching them, so the gate that guards them must fire
without anyone touching them either.

The fix when it fires is cheap and is the intended work: open the source page,
confirm or correct the number, and commit a new establisher and date. Extending
the date without looking is possible, as with any gate — and it leaves a diff
with a name on it, which is the difference between a lapse and a decision.

### Its failure message is load-bearing, because the first red will look broken

A time-triggered gate fails on a morning when nobody changed anything. That is
the least expected failure a suite can produce, and the natural reading is
**"CI is broken"** rather than "a fact expired". A reader who arrives at that
conclusion reaches for a skip, and the gate will have trained exactly the
behaviour it exists to prevent.

So the message is part of the design rather than a detail of it, and it must
carry four things:

- **That this is not a build failure.** Lead with it. Nothing changed; a date
  passed. A reader who learns that in the first line stops debugging.
- **Which row**, by `(provider_id, tier)`, and how long overdue. A gate that
  says "a row is stale" sends someone hunting through a file.
- **The `source_ref`**, so the check is a click rather than a search. The whole
  fix is: open that page, read the number.
- **What to commit**: the value if it moved, and a new `established_by`,
  `established_at_ms`, `review_by_ms` in any case. Confirming a price unchanged
  is a real result and must look like one, or "nothing changed" feels like
  wasted work and the next reader skips it.

The message should also say what it *cannot* tell you: that a price is still
correct. The gate only knows nobody has looked recently. Claiming more would
make it the kind of instrument this repo keeps finding — one that answers a
question adjacent to the one asked.

## What sourcing the first three rows proved, on day zero

Both of these appeared on the **first rows anyone tried to source**, and both
would have been invisible in a table populated from memory. They are recorded as
rows rather than as principles, because the rows are the evidence.

**Billing basis is a hidden key dimension, and it is 15% wide.**
anthropic.com/pricing publishes Pro at *$17 per month with annual subscription*
and *$20 if billed monthly* — one tier, two published prices. From memory this
looks like one number. The plane records the **monthly-billed** figure, chosen
for its failure direction rather than for tidiness: a higher denominator
produces a lower multiplier, so monthly-billed makes a subscription look *worse*
than it is for an annual subscriber, and every other residual in the consuming
calculation biases toward "keep paying". The one conservative term is the one
worth keeping. An annual subscriber's actual rate is an operator fact and
belongs on their credential record, not in a public list.

**A source's silence is evidence about that SOURCE, not about the fact.** This
one was recorded wrongly first, and the mistake is the more useful half.

anthropic.com/pricing says Max is *"From $100 per month"* and *"Choose 5x or 20x
more usage than Pro"* — it publishes the family floor and the multiplier choice,
and never a tier price. Correct reading. I then shipped **two refusal cells**
concluding the figures were unsourceable, and told the consumer their primary
provider might not be computable.

Both are on the vendor's own help centre, stated outright: *"Max 5x: $100 per
month"*, *"Max 20x: $200 per month"*. One more surface from the same publisher.

A refusal cell asserts **nobody publishes this**, which is far stronger than the
evidence *this page does not*. So: exhaust a publisher's own surfaces — pricing
page, help centre, billing docs — before recording one. A refusal is a positive
claim and carries a positive claim's burden.

What made it stick for an hour is that a well-argued negative is the easiest
thing to accept. The consumer took my conclusion, propagated it, and the cost of
checking was a single search.

**And the same page carried two more basis axes.** *"The Max plan is currently
available as a monthly subscription only"* — so the monthly-billed convention is
a **fact** for those rows rather than a choice, and a note implying a decision
would be wrong. *"These prices are for web subscriptions only. Mobile pricing may
vary depending on your app platform"* — an app-store markup is a third axis
beyond country and billing term, invisible to the plane, and it belongs in the
policy note rather than being discovered by someone whose multiplier is quietly
wrong.

Neither axis is visible from memory. Both came from reading the page.

## Settled, after being open

**Its own tool, not `catalog.*`.** The shape argument (non-model rows in a model
response) is the weaker one. The stronger: *the consumer of a plan price is not
the consumer of model prices.* Model rates are read per request by routers on a
hot path; a plan price is read rarely by whatever computes a multiplier. No
caller wants both in one response, so serving them together grows every existing
consumer's response to serve one that does not exist yet — and hands all of them
a field they must **learn to ignore**, which is a field someone eventually
misreads in a way that looks like a reasonable interpretation rather than a bug.

**Compiled in.** Runtime editing reintroduces exactly the per-host drift that
ruled out the operator verb. The cost is real and stated plainly: a price
correction requires a fusiform release. It is acceptable because **the release
is the review** — for a value whose errors are invisible downstream, the thing
that would make a correction cheap is the same thing that would make it
dangerous.
