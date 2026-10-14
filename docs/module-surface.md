# Module surface

Fusiform ships two binaries:

- `ck-fusiform` (crate `fusiform-module`): the module. It runs under the
  CortexKit supervisor (subc), polls the upstream catalog, keeps the store, and
  answers tool calls.
- `ck-models` (crate `fusiform-cli`): the operator CLI, run as `ck models
  <verb>`. It is a subc consumer that calls the module's tools; it never opens
  the database. Commands are in [`operations.md`](operations.md).

Integration test names below are libtest paths in the module crate's
`--test it` target; unit tests name their source file under
`crates/fusiform-module/src/`.

## 1. Tools

Tools, as served: `catalog.get`, `catalog.history`, `catalog.status`,
`catalog.correct`, `catalog.mark_artifact`, `catalog.retract_artifact`,
`plan.prices`.

| Tool | Execution mode | What it does |
| --- | --- | --- |
| `catalog.get` | pure | read the catalog, optionally at a past instant or on a billing plane |
| `catalog.history` | pure | every era for one fact |
| `catalog.status` | pure | recent polls, versions and counts |
| `catalog.correct` | mutating | record that fusiform's record was wrong over a window |
| `catalog.mark_artifact` | mutating | mark one poll as a representation change of fusiform's own |
| `catalog.retract_artifact` | mutating | take a mark back |
| `plan.prices` | pure | curated subscription list prices |

Tests check that the list above names exactly the tools in the protocol
crate's `TOOLS` constant
(`wire_fact_table::the_module_surface_doc_lists_exactly_the_served_tools`),
and that the manifest and the dispatch table list the same tools
(`route::the_manifest_and_the_dispatch_agree_on_which_tools_exist`). Both are
in `crates/fusiform-module/tests/it/`. Request
and response fields are in [`wire-contract.md`](wire-contract.md).

Every tool name contains a dot (`catalog.*` for the observed plane, `plan.*`
for the curated one). An MCP gateway that accepts only `[A-Za-z0-9_-]+` tool
names will not expose them; fusiform's consumers call them through the
supervisor (subc), which routes each tool call to the module.

## 2. Manifest

`ck-fusiform` registers with this manifest (`fusiform_module::manifest()` in
`crates/fusiform-module/src/lib.rs`):

| Field | Value | Why |
| --- | --- | --- |
| module id | `fusiform` | |
| trust tier | first-party | |
| provides | one tool provider with the seven tools above | |
| `identity_scope` | empty | the answer does not depend on the caller's project or session |
| concurrency | module-managed | |
| `emits_push` | `false` | fusiform is pull-only |
| `sub_supervises` | `true` | |
| self signal | `catalog_poll`: a poller with effect `observe`, anchored to a fixed interval, literal cadence of 30 minutes, domain `models.dev` | the poll loop observes the upstream and changes nothing there |
| provenance | the build's git commit when stamped, otherwise "never derived" | see §5 |
| capabilities | none declared | |
| storage bindings | none declared | the module makes no storage claim; the supervisor assigns the store and sends its descriptor in the handshake |

`route::the_manifest_declares_what_is_true_and_omits_what_is_not` pins the
trust tier, the absent storage bindings and the empty `identity_scope`.
Each tool's input schema in the manifest names exactly the fields its
request type decodes, so a caller that validates its arguments against the
schema can use every argument the route accepts
(`route::every_tool_schema_names_exactly_its_request_fields`).

`ck-fusiform --manifest` prints the manifest as JSON with an empty top-level
`runtime_computed` array, without connecting to a daemon
(`manifest_probe::the_daemon_binary_prints_its_registration_manifest_with_no_daemon`).
`ck-fusiform --version` (or `-V`) prints the binary version, the build
revision and the protocol crate version, also without a daemon.

## 3. Lifecycle

1. The module connects to the supervisor. The handshake acknowledgement carries
   the storage descriptor; the store is opened from it, never from a path the
   module derives itself.
2. An empty store is seeded from the embedded snapshot
   (`bootstrap::a_fresh_install_with_no_network_answers_a_read`). Seeding twice
   writes nothing (`bootstrap::seeding_twice_does_not_rewrite_history`).
3. The curated plan-price file is applied, on every start
   ([`plan-prices.md`](plan-prices.md) §6).
4. Health signals are adopted from the store, so a restart does not reset the
   catalog's age or failure history
   (`restart_health::a_restart_does_not_reset_the_staleness_clock`,
   `restart_adoption::a_restart_adopts_the_failure_count`).
5. The poll loop starts, running its first tick immediately and then every 30
   minutes. If a tick comes due while the previous one is still running, or
   while the machine is suspended, it is skipped: the loop does not run
   catch-up polls in a burst.

Tool calls before the store is open are refused with `unavailable`, never
answered with an empty catalog. Requests run off the connection's reader
thread, so one slow read does not block health replies or other calls
(`request_serving::blocked_request_keeps_reader_runtime_responsive`).

## 4. The poll

Each tick fetches `https://models.dev/api.json` with a 90-second whole-request
timeout and records exactly one observation:

| Result | Outcome | Effect |
| --- | --- | --- |
| 304 Not Modified | `not_modified` | confirms current values; writes no eras (`tick::a_not_modified_response_is_an_observation`) |
| 200, same normalized content | `unchanged` | confirms current values; writes no eras (`tick::an_unchanged_body_records_the_look_and_nothing_else`) |
| 200, changed content | `changed` | writes one era per changed fact, then advances the catalog version (`tick::the_version_advances_once_per_change`) |
| network error, error status, unparseable body | `failed` with class `network`, `http_status` or `parse` | writes nothing else (`tick::a_failed_poll_records_the_attempt_and_nothing_more`) |
| a document that drops too much of the catalog | `failed` with class `implausible` | writes nothing else (`shrink_guard::a_collapsed_document_is_refused_and_writes_nothing`) |

1. Every poll after the first sends the stored ETag as `If-None-Match`, verbatim
   (`conditional_get::a_stored_etag_is_sent_as_if_none_match`,
   `conditional_get::the_validator_is_sent_verbatim_including_a_weak_prefix`).
   A new process reads the full document once before trusting a stored
   validator (`conditional_get::a_new_process_reads_the_document_before_trusting_a_stored_validator`).
2. **Shrink guard.** When the store holds at least 100 present models, a
   document that would remove more than 25% of them is refused as
   `implausible`. Growth is never refused
   (`shrink_guard::the_limit_is_where_it_says_it_is`,
   `shrink_guard::growth_is_never_refused`,
   `shrink_guard::a_small_store_is_not_guarded`). A genuine mass withdrawal
   therefore needs an operator decision; health reports the refusal
   (`shrink_guard::a_refusal_is_visible_in_health`).
3. Polling continues against a server that ignores conditional requests
   (`failure_path::polling_continues_against_a_server_without_conditional_support`)
   and recovers when the upstream returns
   (`failure_path::the_loop_recovers_when_the_upstream_returns`).

The poll interval is also the width of every observation window the store
records; it is a property of the history, not only a load setting.

## 5. Build identity

`scripts/release-build.sh` builds release binaries with `CK_BUILD_REV` set to
the full commit hash and refuses a dirty tree. An ordinary `cargo build`
leaves it unset, and the binaries report the revision as `unknown` and the
manifest provenance as never derived
(`route::an_unstamped_build_names_why_its_commit_is_absent`).
`fusiform_protocol::BUILD_REV` names whoever compiled the protocol crate, so a
consumer that compiles it in sees its own build, not fusiform's.

## 6. Health

The health report is computed from in-memory signals only: no lock, no disk.

| Status | When |
| --- | --- |
| `failing` | no store is open, so no read can be answered (`health.rs`: `no_store_is_failing_regardless_of_everything_else`) |
| `degraded` | two or more ticks started and recorded nothing: the store cannot write (`restart_health::ticks_that_reach_no_verdict_are_reported_as_a_write_problem`) |
| `degraded` | the loop has not attempted a fetch in 70 minutes: it is not running (`restart_health::a_stopped_loop_is_named_rather_than_reported_as_staleness`) |
| `degraded` | the startup read of the store failed, so the catalog's age is unknown (`health.rs`: `a_failed_startup_read_is_not_a_fresh_install`) |
| `degraded` | the last successful observation is more than 2 hours old; the detail names the failure class if polls are failing (`health_detail::a_stale_catalog_names_its_cause`) |
| `degraded` | 4 or more consecutive fetch failures (`health.rs`: `a_failure_streak_is_degraded_before_the_staleness_clock_fires`) |
| `ok` | otherwise, including a fresh install with no observation yet (`health.rs`: `a_fresh_install_with_no_observation_yet_is_ok`) |

A missing store and a write failure are checked before staleness, so the
detail names the cause rather than its symptom. A stale catalog is
`degraded`, not `failing`: fusiform can still answer reads correctly about a
world that may have moved (`health.rs`: `a_stale_catalog_is_degraded_not_failing`).

Every report carries the metrics it was derived from
(`health.rs`: `the_report_always_carries_the_numbers_it_was_derived_from`).
Names starting `process_` count what the current process has done since it
started; the others describe the catalog and survive a restart
(`restart_adoption::every_metric_is_process_scoped_by_name_or_survives_a_restart`,
`restart_adoption::the_metric_names_are_pinned`):

| Metric | Scope |
| --- | --- |
| `process_poll_attempts`, `process_polls_recorded`, `process_polls_unrecorded` | process |
| `process_consecutive_failures` | process |
| `process_attempt_age_ms` | process |
| `failures_ever`, `last_failure_age_ms`, `last_failure_class` | catalog |
| `observation_age_ms`, `last_write_age_ms` | catalog |
| `store_open` | process state |

An age that has never been set is `null`, not zero.
