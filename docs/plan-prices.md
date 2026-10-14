# Plan prices

`plan.prices` serves curated subscription list prices keyed by
`(provider_id, tier)`, with the source and review date behind each one. It is a
separate plane from the model catalog: model rates come from the upstream and
are read per request, while a plan price is curated by hand and read rarely, by
whatever computes a subscription-versus-API comparison.

The rows are authored in `crates/fusiform-module/data/plan-prices.json`,
compiled into the binary, and applied to the store's `plan_price_era` table on
every start (see [`data-model.md`](data-model.md) §7). Unit tests named below
are in `crates/fusiform-module/src/plan_prices.rs` unless another file is named.

## 1. Request and response

Request: optional `provider_id`, a models.dev provider slug. Absent means every
provider.

Response:

| Field | Meaning |
| --- | --- |
| `prices` | every row in force now, refusals included |
| `resolved_at_ms` | the instant the answer describes; always the current time |
| `tier_vocabulary` | a statement of what the `tier` strings name (§3) |
| `unit_policy` | a statement of what the prices are quoted on (§4) |

Each row in `prices`:

| Field | Present | Meaning |
| --- | --- | --- |
| `provider_id`, `tier` | always | the key |
| `price` | priced rows only | `minor_units`, `exponent`, `currency`, `period`; the amount is `minor_units × 10^-exponent` |
| `refusal_reason` | refusal rows only | why there is no price |
| `quota_value` | when curated, priced rows only | §5 |
| `boundary_at_ms` | always | when the price took effect, as best the source allows |
| `established_by`, `established_at_ms` | always | who read the source and when |
| `review_by_ms` | always | when someone must read the source again |
| `source_ref` | always | the page the figure came from |

Rules:

1. `price` and `refusal_reason` are exclusive, and exactly one is present
   (`a_refusal_carries_no_money_parts_at_all`,
   `incoherent_rows_are_rejected_with_their_position`).
2. A refusal is a row, not an absence. "Tier observed and no published price"
   and "tier never looked at" are different states, and only the second is
   silent (`a_refusal_is_a_row_rather_than_a_silence`).
3. A named provider with no rows is `no_coverage`: fusiform has not curated
   it, which is not a claim that the provider has no plans
   (`route::an_uncurated_provider_is_refused_rather_than_empty`).
4. Rows serve with their provenance
   (`route::plan_prices_serves_rows_with_their_provenance`).
5. Reads are current-only; there is no `at_ms`. The store keeps the history so
   a past price can be established, but the tool does not serve it.

## 2. Matching a tier

The `tier` string is matched exactly. A tier that is renamed does not resolve
to the old price, because a renamed tier may be a repriced plan. Tier strings
are never normalised, because the same word names plans far apart in price at
different vendors.

## 3. Tier vocabulary

`tier` is the vendor's own name for a subscription tier as written on its
pricing page, for example `pro_200` for OpenAI's "Pro 200". It is not the plan
string a vendor's API reports for an account. Those vocabularies do not map one
to one: an API may report the same string for several tiers. Mapping an account
to its tier is the consumer's job. Where an API string could name several
tiers, the file carries a refusal row for that string naming the candidates.
The served `tier_vocabulary` field states this
(`the_tier_vocabulary_is_a_served_statement`).

## 4. Unit policy

Prices are US list prices, monthly-billed, for a web subscription. An account
on an annual term, in another country, or subscribed through an app store pays
something else, and this plane cannot see which. The served `unit_policy` field
states this (`the_served_policy_is_a_statement_rather_than_an_argument`).

## 5. Quota value

`quota_value` is list-price USD of usage per USD of subscription capacity
consumed. It is optional and tagged by `state`:

| `state` | Fields | Meaning |
| --- | --- | --- |
| `measured` | `low`, `high`, `basis`, `token_mix`, `established_by`, `source_ref`, `as_of_ms`, `review_by_ms` | a dated measurement or an operator-stated figure, as a range |
| `not_established` | `refusal_reason`, `review_by_ms` | nobody has established the value |

1. Absent means not curated: unknown, never zero.
2. `low` and `high` are exact decimals, `units × 10^-exponent`. A consumer
   needing one number takes `low`, which undervalues the subscription.
3. Fusiform never estimates a quota value; the only sources are a dated
   measurement and a figure the operator states.
4. Quota values are joined onto served rows at read time and are not part of
   the stored price claim, so changing only a quota value writes no
   `plan_price_era` row (`changing_only_quota_value_writes_no_plan_price_era`).
5. A refusal row carries no quota value, and neither does a tombstoned key
   (`a_price_refusal_cannot_carry_a_quota_value`,
   `route::plan_prices_joins_quota_values_and_omits_them_for_tombstones`).
6. A malformed quota value is rejected without losing the row's price
   (`malformed_quota_is_rejected_without_losing_the_price`).

## 6. The file

`plan-prices.json` has schema `plan-prices/v1`, a `cells` array, and the
`unit_policy` and `tier_vocabulary` statements served above. Each cell:
`provider_id`, `tier`, either `price` or `refusal_reason`, optional
`quota_value`, `boundary_at_ms`, `established_by`, `established_at_ms`,
`review_by_ms`, `source_ref`, and an optional free-text `note` that is not
served.

Load rules (rejected rows are reported with their cell index):

- every provenance field is required;
- a cell with neither a price nor a refusal, or with both, is rejected;
- a price missing any of its four parts is rejected;
- the same `(provider_id, tier, boundary_at_ms)` twice is rejected, because one
  copy would be silently ignored.

The shipped file has no rejects (`the_shipped_file_has_no_rejects`).

Applying the file at start:

- an unchanged file writes nothing
  (`schema_doc::re_applying_the_curated_file_is_a_no_op`, in the store crate);
- a changed price, or a price becoming a refusal, writes a new row;
- a key that disappears from the file is tombstoned with a refusal saying it is
  no longer carried, rather than left serving its old price
  (`schema_doc::a_key_dropped_from_the_file_is_tombstoned`).

To correct a mistyped price, edit the cell and keep its `boundary_at_ms`: the
corrected row is written at the same boundary and supersedes the old one by
insertion order.

## 7. Review dates

Nothing reveals a plan reprice: there is no fetch to contradict a stale value.
The review date on each row is therefore the only thing that can prompt anyone
to re-check a price, and it is a ceiling on how long a wrong value may
circulate. Each row carries its
own date; there is no file-level date.

The suite fails when any price or quota value is past its review date
(`no_curated_price_is_past_its_review_date`). The failure message begins "NOT A
BUILD FAILURE", names each overdue row and its `source_ref`, and says what to
commit (`an_overdue_row_fires_and_the_message_is_actionable`,
`overdue_quota_values_fire_for_both_shapes`). CI's scheduled run exists so this
gate fires on a day nobody pushed. How to clear it is in
[`curated-data.md`](curated-data.md) §6.
