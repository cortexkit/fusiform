# Billing planes

The same model can bill differently depending on how a caller authenticates:
an API key usually pays list prices per token, while a subscription consumes a
plan's quota (even if it authenticates with an API key). A billing plane is fusiform's statement of how one provider bills
one login method. Planes are curated in
`crates/fusiform-module/data/billing-planes.json`, compiled into the binary,
and applied only when a read is served.

Unit tests named below are in `crates/fusiform-module/src/route.rs`
(`billing_plane_tests::…`) or `crates/fusiform-module/src/billing_planes.rs`;
integration tests are libtest paths in the module crate's `--test it` target.

## 1. Selecting a plane

A `catalog.get` request selects a plane with `auth_method`:

1. `auth_method` is one of `apikey`, `chatgpt`, `oauth`, `antigravity`. Any
   other value is `bad_request`
   (`billing_plane_tests::auth_method_accepts_exactly_the_four_pinned_strings`).
   The set is closed so a new login method is refused until someone reviews how
   it bills.
2. `auth_method` requires `provider_id`; without it the read is `bad_request`
   (`billing_plane_tests::auth_method_requires_a_provider`).
3. Plane views are current-only. `auth_method` with `at_ms` is `no_coverage`
   (`billing_plane_tests::plane_views_refuse_every_point_in_time_read`).
4. A `(provider_id, auth_method)` pair with no declaration is `no_coverage`,
   except `apikey`. A subscription login never falls back to the API card,
   because API prices are a confident wrong answer for a subscription
   (`billing_plane_tests::undeclared_subscription_planes_never_fall_back_to_api_rows`,
   `golden_payload::plane_refusal_cases_never_fall_back_to_api_rows`).
5. An undeclared `apikey` plane serves the provider's published rows
   unchanged, with no provenance (`billing_plane_tests::an_undeclared_api_plane_serves_the_unmodified_published_card_without_provenance`).
6. A read without `auth_method` is the ordinary catalog, byte for byte
   (`billing_plane_tests::a_read_without_auth_method_keeps_the_pre_plane_wire_bytes`).

## 2. Plane kinds

| `kind` | Meaning | Allowed on |
| --- | --- | --- |
| `upstream` | the provider's published rows are the bill; rules may fill classes the provider did not publish | any login method |
| `quota_proxy` | a subscription billed as a flat fee against usage limits: API prices are relative quota stand-ins, not amounts billed or an established quota conversion formula | every method; `apikey` requires an explicit `rates_from_provider` |

On a `quota_proxy` plane:

1. Every rate that is not already borrowed is marked `inherited_from` with
   basis `quota_proxy` and the row's real `model.family`
   (`billing_plane_tests::quota_proxies_mark_each_unmarked_rate_with_its_real_family`).
   A consumer must not treat a `quota_proxy` price as an amount billed.
2. A rate already borrowed from another row (by open-weight inheritance or
   through an alias, see [`wire-contract.md`](wire-contract.md) §4.4) keeps its
   original marker (`billing_plane_tests::quota_proxies_preserve_the_first_hop_of_inherited_rates`,
   `billing_plane_tests::quota_proxies_do_not_erase_alias_markers`).
3. Mode rates are dropped on every quota proxy, including borrowed API cards,
   because a subscription's access to a priced API mode is not established
   (`billing_plane_tests::quota_proxies_drop_all_mode_rates`,
   `billing_plane_tests::kimi_quota_proxy_drops_borrowed_mode_rates`).

A `quota_proxy` declaration on `apikey` without an explicit price provider is rejected when the file loads
(`billing_planes.rs`: `a_quota_proxy_on_apikey_without_a_price_provider_is_rejected_with_position`).

### Cross-provider quota cards: Kimi Code

The `(kimi-code-plan-global, apikey)` plane declares
`rates_from_provider: "moonshotai"`. Routing derives `apikey` from the vault
credential `apikey:kimi-for-coding`; this provider's API key consumes a
subscription quota rather than buying Moonshot API tokens.

Only an exact `model.canonical_id` of `moonshotai/<id>` with a present
`(moonshotai, <id>)` row in the current catalog establishes a target. The plan's
entire rate card is replaced by the published non-mode `rate.*` facts on that
target, including base and context-tier keys. Each carries `inherited_from`
with `basis: "quota_proxy"`, the target provider, model id and family. API mode
prices are stripped because they cannot establish subscription mode access.
Plan-only keys are removed, not mixed into the API card.

`InheritedFrom.family` is a required string in the existing wire type. As with
exact-row aliases, an absent or null target family uses the existing empty-string
sentinel; it is not a stated family name. The canonical id establishes the target
independently of family, so missing family does not block borrowing
(`billing_plane_tests::kimi_plan_borrows_only_exact_canonical_api_cards_without_changing_storage`).

If the canonical id is absent, names another provider, or has no present API
row, the plan's rate keys and the five base token classes read
`{"state":"unpriced","reason":"no_api_card"}` without a borrowed marker.
No similar-name or family match, alias, inherited API price, or plan zero is a
fallback. A target that exists but publishes no rate leaves those keys absent.
The caller's filter is applied last, on named and provider-wide reads
(`billing_plane_tests::kimi_plan_borrows_only_exact_canonical_api_cards_without_changing_storage`,
`golden_payload::kimi_plane_cases_pin_borrowed_cards_and_missing_api_coverage`).

The current canonical mappings send `k3` and `k3-256k` to
`moonshotai/kimi-k3`, and `kimi-for-coding-highspeed` to
`moonshotai/kimi-k2.7-code-highspeed`. `kimi-for-coding` names
`moonshotai/kimi-k2.8-preview`, for which the current catalog has no API row.

The [official membership guide](https://www.kimi.com/code/docs/en/kimi-code/membership.html)
states: “Kimi Code is the developer-focused AI coding service within Kimi
membership benefits, provided together with a Kimi membership subscription
and sharing the same quota. Requests from the CLI, VS Code, Desktop, and
third-party tools all count toward that quota.” It also states: “Both groups
share the Kimi membership monthly total quota: if the monthly total is reached,
Kimi Code quota is frozen until the monthly quota resets or you upgrade.”
The [credit rules](https://www.kimi.com/en/help/membership/membership-update-rules)
say usage is token-based and varies with model, context, tools and plan, but
these pages establish no numeric per-token-class conversion. Moonshot prices
are therefore explicitly a stand-in, not a measured subscription rate.
Established 2026-10-19; review due 2026-12-18 (60 days).

## 3. Rules

A plane may carry rules, each stating how one token class bills on that plane.

| `state` | Served value | Meaning |
| --- | --- | --- |
| `billed_as` | `{"state":"billed_as","key":"rate.<as_class>…"}` | tokens of this class are billed at another class's rate on the same row; a cache write with no surcharge is billed as input, not free |
| `stated_zero` | `{"state":"stated_zero"}` | the plane's source states this class is free |
| `not_established` | `{"state":"unpriced","reason":"not_established"}` | nobody has established what this class costs on this plane |

1. Rules are applied per context tier: a rule for `cache_write` also fills
   `rate.cache_write.above_context.<n>` for every tier the row's base or target
   class publishes (`billing_plane_tests::every_rule_state_uses_class_tiers_and_exact_model_scopes`).
2. On an `upstream` plane a rule never overwrites a key the provider
   published, including a stated zero or a tombstone; it fills only missing
   keys (`billing_plane_tests::upstream_rules_never_overwrite_a_published_key_but_fill_each_missing_tier`).
   On a `quota_proxy` plane a rule replaces the proxied price
   (`billing_plane_tests::quota_rules_replace_published_prices_and_narrow_disclosures_last`).
3. A `billed_as` rule whose target key is absent on the row serves
   `not_established` instead, and the disclosure still names the target class
   (`billing_plane_tests::missing_billed_as_targets_are_disclosed_as_not_established`).
4. Targets are resolved against the row before any rule applies, so rule order
   cannot create chains (`billing_plane_tests::billed_as_targets_are_resolved_before_any_rule_outputs`).
5. Non-rate facts are untouched (`billing_plane_tests::plane_views_preserve_non_rate_facts`).
6. Nothing a plane does is stored, and a plane read does not move
   `catalog_version` (`billing_plane_tests::plane_views_never_change_stored_rows_or_catalog_version`).

## 4. What the response carries

- `plane`: `provider_id`, `auth_method`, `kind`, and for a declared plane its
  `source_ref` and `review_by_ms`. An undeclared `apikey` plane has `kind`
  `upstream` and no provenance.
- `billing_rules`: one entry per `(model, fact_key)` a rule set and the read
  kept, with `state`, `as_class` when the rule names one, and the rule's
  `source_ref`, `established_by`, `established_at_ms` and `review_by_ms`.
  The caller's `fact_prefixes` filter applies after the plane, and rule
  disclosures for facts the filter removed are dropped
  (`golden_payload::chatgpt_named_and_bulk_cases_apply_the_plane_before_each_filter`).
- Curated model rows whose `auth_methods` do not include the selected method
  are not served (see [`curated-data.md`](curated-data.md) §4).

The plane fields decode with the protocol crate
(`wire_types_decode_the_wire::plane_variants_and_both_disclosures_decode_from_real_served_bytes`).

## 5. The file

`billing-planes.json` has schema `billing-planes/v1` and a `planes` array. Each
plane:

| Field | Required | Meaning |
| --- | --- | --- |
| `provider_id` | yes | a models.dev provider slug |
| `auth_method` | yes | one of the four methods |
| `kind` | yes | `upstream` or `quota_proxy` |
| `rates_from_provider` | no | non-empty provider slug; only valid on `quota_proxy`; replace rates with the exact canonical row's non-mode rate card |
| `source_ref` | yes | the source that states how this login method bills |
| `established_by`, `established_at_ms` | yes | who established the claim and when |
| `review_by_ms` | yes | when it must be re-verified |
| `rules` | no | array of rules |

Each rule: `class`, `state`, `as_class` (required for `billed_as` and
different from `class`), optional `model_ids` (absent means every model on the
plane; a list means exactly those ids), and its own `source_ref`,
`established_by`, `established_at_ms`, `review_by_ms`.

Load-time checks, each rejecting the offending row with its position in the
document (tests in `billing_planes.rs`):

- unknown `auth_method`, `kind`, `class` or `state`
  (`an_unknown_auth_method_is_rejected_with_position`,
   `unknown_classes_and_states_are_rejected_with_position`);
- `rates_from_provider` on a non-`quota_proxy` plane or an empty price provider
  (`a_price_provider_is_only_allowed_nonempty_on_a_quota_proxy`);
- a duplicate `(provider_id, auth_method)`
  (`duplicate_planes_name_the_original_document_position`);
- missing or empty provenance on a plane or a rule
  (`every_plane_provenance_field_is_required_with_position`,
  `every_rule_provenance_field_is_required_with_position`,
  `empty_provenance_is_rejected_on_planes_and_rules`);
- a `billed_as` rule with no `as_class`, or with `as_class` equal to `class`
  (`billed_as_without_a_distinct_target_is_rejected_with_position`);
- rules for the same class whose model scopes overlap
  (`overlapping_rule_scopes_are_rejected_with_position`);
- a `billed_as` rule whose target class has a rule of its own, or a chain or
  cycle of `billed_as` rules
  (`a_billed_as_target_with_any_overlapping_rule_is_rejected_in_either_order`,
  `plane_wide_chains_and_cycles_are_refused_before_removing_candidates`).

The shipped file has no rejects (`the_shipped_file_has_no_rejects`). Review
dates are a time-triggered gate: the suite fails when any plane or rule is past
its `review_by_ms`, including a plane with no rules
(`no_plane_or_rule_is_past_its_review_date`,
`an_overdue_ruleless_plane_fails_the_review_gate`). The fix is to re-read
each overdue claim's source and record a new review date; see
[`curated-data.md`](curated-data.md) §6.

## 6. What a plane does not claim

- Whether reasoning tokens are counted inside or outside a provider's reported
  output count. That is a usage-reporting fact for the consumer's usage
  normalizer, not a rate.
- A per-token cost for a subscription. `quota_proxy` prices are relative
  weights; scaling them to money is the consumer's policy.
- History. Plane views are current-only, and a point-in-time read serves what
  the upstream published.
