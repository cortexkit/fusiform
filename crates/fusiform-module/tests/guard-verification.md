# Scan-guard and blocking-test verification

The real-input guards retain their extraction floors and known-item checks.
Their rejection rules now accept source text or parsed data independently of
repository I/O. Every synthetic example contains one violation of the check it
calls, and asserts the offending name or diagnostic rather than merely a count.
Some planted tests exercise several examples in separate arms.

## Planted violations

Prefixes below are integration-test modules unless explicitly marked as unit tests.
Each row's planted test was run individually with an exact filter while its
check returned no violations. Every run exited 101, naming that test alone:
`0 passed; 1 failed`; all other tests were filtered out, not reported as passes.
For the nine overlay checks using `require!`, neutralizing that shared collector
also reddened each planted test individually after the final refactor.

| Guard | Shared check | Planted test | Broken-check proof |
| --- | --- | --- | --- |
| store `no_write_escapes_the_fence` | `unfenced_writes` | `every_write_is_fenced::planted_unfenced_write_is_reported` | Empty result; expected line 1 |
| store `the_fence_is_actually_called` | `missing_fence_calls` | `every_write_is_fenced::planted_missing_fence_calls_are_reported` | Empty result; expected `with_conn_fenced` |
| store `the_store_migrates_through_the_handle_it_returns` | `migration_handle_violations` | `every_write_is_fenced::planted_reopened_migration_handle_is_reported` | Empty result; expected `connection count` |
| store `the_design_note_names_the_tables_that_exist` (both directions) | `table_list_violations`, `table_names` | `schema_doc::planted_table_list_drift_is_reported` | Empty result; expected `undocumented table: lost` (also plants phantom `ghost`) |
| store `the_design_note_names_functions_that_exist` | `missing_documented_functions` | `schema_doc::planted_nonexistent_documented_function_is_reported` | Empty result; expected `lost` |
| store both boundary-vocabulary direction tests | `missing_kinds` | `boundary_vocabulary::planted_vocabulary_difference_is_reported` | Empty result; expected `invented`; shared comparison covers both directions |
| core `every_measured_field_is_read_or_declared_unread` | `unaccounted_fields` | `measured_fields_are_read_or_declared::planted_unaccounted_field_is_reported` | Empty result; expected `lost` |
| core `the_document_names_every_field_the_parser_reads` | `undocumented_fields` | `measured_fields_are_read_or_declared::planted_undocumented_field_is_reported` | Empty result; expected `secret` |
| core `every_nested_key_the_document_records_is_read` | `nested_key_violations` | `measured_fields_are_read_or_declared::planted_unread_nested_key_is_reported` | Empty violations with nonzero count; expected `limit.lost` |
| core `every_key_inside_a_mode_is_read_or_declared_unread` | `mode_key_violations` | `measured_fields_are_read_or_declared::planted_unaccounted_mode_key_is_reported` | Empty result; expected `lost` (also plants a read/unread overlap for `provider`) |
| core `a_declared_unread_field_is_actually_unread` | `falsely_unread_fields` | `measured_fields_are_read_or_declared::planted_false_unread_declaration_is_reported` | Empty result; expected `secret` |
| core unit `the_raw_layer_is_read_only` | `serializable_raw_derives` | `normalize::raw::tests::planted_serializable_raw_type_is_reported` | Empty result; expected the Serialize derive line |
| CLI unit `every_plural_goes_through_count` | `plural_caller_violations` | `tests::planted_extra_plural_caller_is_reported` | Empty result; expected `caller count: 2`; tests remain below production code |
| module `every_read_response_field_reaches_the_fixture` | `missing_response_fields` | `golden_payload::planted_missing_response_field_is_reported` | Empty violations with nonzero count; expected `Response.lost` |
| module `every_served_fact_key_reaches_the_fixture` | `missing_served_fact_keys` | `golden_payload::planted_missing_served_fact_key_is_reported` | Empty violations with nonzero count; expected `lost` |
| module `the_failure_signal_is_stamped_before_the_store_write` | `failure_stamp_violations` | `failure_path::planted_late_failure_stamp_is_reported` | Empty result; expected `signals.failed` |
| module `the_design_notes_serve_row_lists_every_served_tool` | `undocumented_tools` | `wire_fact_table::planted_undocumented_tool_is_reported` | Empty result; expected `catalog.lost` |
| module `only_one_currency_policy_has_ever_existed` | `extra_currency_policies`, `currency_policies` | `served_corrections::planted_second_currency_policy_is_reported` | Empty result; expected the second constructor |
| overlay provenance vocabulary and grade | `provenance_violations` | `window_overlay::planted_invalid_provenance_is_reported` | Neutralized collector; expected exactly one diagnostic naming invented provenance |
| overlay wall vocabulary and grade | `wall_vocabulary_violations` | `window_overlay::planted_invalid_wall_ownership_is_reported` | Neutralized collector; expected exactly one diagnostic naming invented ownership |
| overlay wall claim agrees with siblings | `wall_agreement_violations` | `window_overlay::planted_unsupported_wall_claim_is_reported` | Neutralized collector; expected one missing-refusal diagnostic (also tests missing measured value) |
| overlay joins a real model | `join_violations` | `window_overlay::planted_unjoinable_cell_is_reported` | Neutralized collector; expected `example/model` diagnostic |
| overlay completely specified | `specification_violations` | `window_overlay::planted_incomplete_fact_is_reported` | Neutralized collector; expected missing `observed_at` |
| overlay three value kinds | `value_violations` | `window_overlay::planted_invalid_value_is_reported` | Neutralized collector; expected invented-kind diagnostic; separate arms cover missing stated value, boundless bracket, inconsistent unknown grade |
| overlay minted ids | `mint_violations` | `window_overlay::planted_unused_mint_is_reported` | Neutralized collector; expected unused `example-fork` |
| overlay nothing derivable | `derivable_violations` | `window_overlay::planted_derivable_cell_is_reported` | Neutralized collector; expected `example/model output.enforced` |
| overlay key closed to promotion | `promotion_violations` | `window_overlay::planted_promoted_closed_key_is_reported` | Neutralized collector; expected promoted `openrouter/* geometry` |
| overlay self-contradicting Anthropic rows | `anthropic_violations` | `window_overlay::planted_uncorrected_anthropic_row_is_reported` | Empty result; expected model `lost` |
| overlay review dates | `review_violations` | `window_overlay::planted_overdue_fact_is_reported` | Empty violations with nonzero count; expected overdue `example/model output.enforced` |

Mutation commands for every integration row:

```text
cargo test --locked -p <package> --test it <planted-test-full-name> -- --exact
```

The two unit-test commands:

```text
cargo test --locked -p fusiform-core --lib normalize::raw::tests::planted_serializable_raw_type_is_reported -- --exact
cargo test --locked -p fusiform-cli --bin ck-models tests::planted_extra_plural_caller_is_reported -- --exact
```

Before mutation, explicit files were staged and `git diff --stat` was empty.
The first mutation batch changed 11 files, 41 insertions and one deletion.
Each was restored from the index and touched; `git diff --stat` was empty again.
The final overlay collector proof changed `window_overlay.rs` alone (one
insertion, one deletion) and likewise restored to an empty diff. Detailed
per-test evidence is also in the delivery declaration.

No guard from the requested starting set was left unplanted. The existing
`scripts/no-outside-path-deps.sh --self-test` already has its planted violation
and was left unchanged. These helpers preserve the existing source-scan scope
and structural assumptions; this is not a general Rust or Markdown parser.

## Blocking operations

The shared HTTP helper bounds each awaited local poll/tick to 60 seconds. The
stub uses nonblocking accept, a stop signal, 30-second socket reads/writes, and
a 120-second bounded shutdown loop. It joins only an already-finished worker.
Accepted sockets explicitly switch back to blocking mode because inheritance
of a listener's nonblocking mode varies by platform. A worker failure is
reported by the owning test; shutdown avoids double-panic aborts during an
already-failing test.

| Blocking test | Operation | Deadline before | Deadline added |
| --- | --- | --- | --- |
| `conditional_get::a_stored_etag_is_sent_as_if_none_match` | HTTP poll; stub accept/read/write | reqwest whole-request 90 s; detached stub unbounded | poll 60 s; shared bounded stub lifecycle |
| `conditional_get::a_first_poll_sends_no_validator` | HTTP poll; stub I/O | same | same |
| `conditional_get::a_304_carries_the_validator_forward` | HTTP poll; stub I/O | same | same |
| `conditional_get::a_304_without_an_etag_header_does_not_lose_the_validator` | HTTP poll; stub I/O | same | same |
| `conditional_get::the_validator_is_sent_verbatim_including_a_weak_prefix` | HTTP poll; stub I/O | same | same |
| `conditional_get::the_stored_validator_reaches_the_next_request` | two awaited ticks; stub I/O | same per fetch | 60 s per tick; shared bounded stub lifecycle |
| `conditional_get::a_new_process_reads_the_document_before_trusting_a_stored_validator` | three awaited ticks; stub I/O | same per fetch | same |
| `failure_path::a_failure_streak_is_recorded_without_corrupting_history` | finite tick sequence; stub I/O | reqwest 90 s per fetch; detached stub unbounded | 60 s per tick; shared bounded stub lifecycle |
| `failure_path::the_loop_recovers_when_the_upstream_returns` | finite tick sequence; stub I/O | same | same |
| `failure_path::polling_continues_against_a_server_without_conditional_support` | two awaited ticks; stub I/O | same | same |
| `failure_path::health_names_the_cause_of_a_failure_streak` | finite tick sequence; stub I/O | same | same |
| `failure_path::a_recovery_keeps_the_cause_and_stops_explaining_it` | finite tick sequence; stub I/O | same | same |
| `failure_path::a_tick_stamps_the_attempt_on_every_outcome` | two awaited ticks; stub I/O | same | same |
| `live::a_live_poll_cycle_seeds_then_goes_conditional` | three real-network ticks | reqwest whole-request 90 s per fetch | none needed; gated on presence of `FUSIFORM_LIVE`, not ignored |
| `live::the_live_document_normalizes_today` | real-network HTTP poll | reqwest whole-request 90 s | none needed; same environment gate |
| CLI `both_binaries_answer_version_with_no_arguments_and_no_daemon` | subprocess and stdout/stderr capture | unbounded `Command::output` | 10 s per command; concurrent pipe drains and bounded channel receives; kill/reap on expiry |
| protocol `deps::the_wire_crate_stays_free_of_the_modules_internals` | `cargo tree`, output capture | unbounded `Command::output` | 300 s command/output budget; kill/reap on expiry |
| protocol `deps::the_dependency_tree_is_small_enough_to_read` | same helper | same | same |
| CLI `a_hung_version_probe_is_killed_at_its_deadline` (new) | deliberately stalled subprocess | new test | 100 ms budget; expects named deadline panic, not elapsed time |
| protocol `deps::a_hung_dependency_probe_is_killed_at_its_deadline` (new) | deliberately stalled subprocess | new test | same |

`bootstrap.rs` and `tick.rs` have no network, task joins or condition-wait loops;
they exercise seeding and the synchronous classified-outcome application path.
`src/fetch.rs` and `src/loop_.rs` contain no test modules. Production fetches
already bound both request headers and body reads through the same reqwest
client's 90-second whole-request timeout. Other test loops iterate finite local
data or monotonically reduce an integer; no other task joins or blocking
channel receives were found. The two subprocess test files were added to the
allowed scope by explicit parent approval; no dependencies or wire types were
changed.

## Gates

Tool versions: cargo 1.99.0, rustc 1.99.0, rustfmt 1.10.0-stable,
clippy 0.1.99, cortexkit-mutate 0.7.0.

Required chain:

```text
cargo fmt --all -- --check && cargo clippy --locked --workspace --all-targets -- -D warnings && cargo test --locked --workspace --all-targets && ck-mutate check
```

The workspace suite reports 488 passed, zero failed, zero ignored across nine
nonempty and three empty targets. Two live-network tests return early because
`FUSIFORM_LIVE` is absent; the live network itself was not exercised.
Clippy checks all six workspace packages/all targets without warnings;
format checking succeeds; `ck-mutate check` verifies 34 catalogue controls'
anchors and exact test names (it does not execute those standing mutations).
All 29 new planted controls were executed green on real helpers and red with
broken checks. The two new subprocess-deadline tests also pass.

Intermediate verification exposed and fixed a non-idempotent nested-macro
formatting shape, inherited nonblocking accepted sockets on macOS, and duplicate
table-extractor anchors in the standing mutation catalogue. Final helpers use
ordinary explicit violation-collection macros, blocking accepted sockets with
I/O budgets, and a single shared table-name extractor. No standing catalogue
entry was changed.
