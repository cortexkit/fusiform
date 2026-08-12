# Fusiform — charter

Status: chartered 2026-08-11. No code yet; this document is the founding
contract. The first owner session starts from here.

## What CortexKit is (context for the new owner)

CortexKit is a local-first fleet of cooperating AI-infrastructure modules on a
user's machine, coordinated by **subc** (`ck-subc`) — a user-isolated daemon
that supervises module processes, routes frames between them over loopback TCP
with an HMAC handshake, and enforces capability manifests. Modules are
independent binaries in independent repos, each owned by a dedicated Alfonso
seat, speaking a shared wire protocol (`subc-protocol`). The fleet currently
runs ~16 supervised modules. Naming convention: brain anatomy, one word.

The modules fusiform will interact with most:

- **broca** (`ck-broca`) — the LLM run engine: durable runs, WAL-backed
  sessions, provider dispatch, usage export. **Fusiform's first consumer.**
  Today broca embeds a models.dev snapshot at compile time (`include_str!`),
  so learning a new model string costs a full release cycle; a 17-day-stale
  snapshot cost live panel seats ("catalog has no served model"). Broca has
  settled a `catalog.refresh` control op on its management surface: fusiform
  pushes, broca validates and swaps.
- **astrocyte** — AI spend metering (all-modality by charter since July 17:
  LLM now; image/video/other AI spend later). Ingests models.dev **price
  snapshots** today on its own cadence — the planned **second consumer**;
  its pricing lane consolidates onto fusiform when fusiform is stable.
- **insula** — provider quota/usage tracking (rate windows, account state).
  Adjacent, not a consumer initially.
- **synapse** — local model serving (embeddings, rerankers, micro-LLMs,
  hardware budgets, certification-by-probe). Owns the LOCAL half of any
  future generation story.
- **claustrum** — the credential vault. Fusiform's upstream fetches use
  plain HTTPS to public endpoints in v1 (no credentials); if an
  authenticated source ever joins, its credential comes from claustrum,
  never from config.
- **prefrontal** — the executive/session module (agent seats, work graph).
  Not a data-plane consumer; its interest is indirect (model rosters for
  panels/specs resolve through broca).

Other modules (thalamus, magic-context, engram, aft, callosum, wernicke,
cerebellum, plexus, entorhinal, subc-mcp) are not in fusiform's blast radius
beyond the shared daemon contract.

## Why fusiform exists

Three copies of the same upstream knowledge already live in the fleet:
broca's compile-time embed, astrocyte's price-snapshot ingestion, and
commons' model-catalog crates. Each rots on its own schedule; the freshest
was 17 days stale when this was chartered. And the fleet's roadmap includes
non-LLM modalities (image/video/audio generation) that **no single upstream
covers** — models.dev is LLM-shaped.

The alternative to a module (a scheduled fetch script) was considered and
rejected on a structural argument: an out-of-band script is an unsupervised
mechanism whose failure mode is silence — no health probe, no restart budget,
no fleet monitoring, no deploy ritual, no owner. This fleet's entire
maintenance machinery is module-shaped, so the module is the LOWER-maintenance
option here.

## Mission

One structured, versioned, provenance-carrying data plane answering: **what
AI models exist, what can they do, what do they cost, and how fresh is this
knowledge** — for every modality, normalized across providers, served to
fleet consumers over subc routes and pushed on change.

## v1 scope (deliberately small)

1. **Fetch** models.dev on a cadence (supervised, health-probed, jittered).
2. **Normalize** into fusiform's own schema (see Schema, below).
3. **Diff** against the last snapshot; store snapshot history + provenance
   (source, fetch time, content hash) in the module store.
4. **Push**: on change, call broca's `catalog.refresh` over a subc route.
   Fusiform pushes; consumers validate and swap. No consumer polls.
5. **Serve**: a read surface (`catalog.get`, `catalog.diff`) for any
   consumer that wants pull semantics.

Image/video/audio rows are v1 **schema**, not v1 content: the schema carries
modality from day one so non-LLM rows land without a schema migration, but
v1 ships no non-LLM source. Do not build speculative fetchers.

## Constraints (settled before chartering; not open for re-litigation)

- **Fetch is separable from swap.** Fusiform never forces a catalog into a
  consumer. Consumers own their swap guards; broca specifically refuses a
  swap that would re-route a provider's wire family or un-serve a model with
  recent production usage (a July refresh silently removed 319 retired
  models — the guard exists because of it). A refresh is a restore from a
  stale-capable upstream and gets restore-fence treatment.
- **Wire-family resolution stays with consumers, forever.** Anything that
  picks a renderer — and therefore exact request bytes — must never be
  sourced from data an upstream edit can silently change. Fusiform says
  WHAT exists, never HOW to speak to it. (Broca keeps `family.rs`
  hand-tabled; that is correct and permanent.)
- **Upstreams are data, never authority.** Every row carries provenance
  (source + fetched-at + hash). A consumer can always answer "why does the
  catalog say this."
- **Push contract is versioned wire schema** under `subc-protocol`
  conventions; cross-repo payload boundaries get vendored golden fixtures
  or a shared published type on day one (fleet rule; see subconscious
  `docs/hunting-loop-briefing.md` for why).
- **Empty-store operator path** is the acceptance bar for carriage: a fresh
  install with no network must come up healthy (seed snapshot embedded as
  bootstrap, demoted to seed-only the moment the first fetch lands).

## Open items (the new owner inherits these WITH their history)

1. **Who drives non-LLM generation?** Settled July 17: astrocyte METERS all
   AI spend; generators are "future modules with usage telemetry of their
   own" — driving was left unowned, deliberately. Current position
   (2026-08-11, SUBC + Ufuk): remote generation (image/video/audio APIs)
   wants a **job-shaped sibling of broca** — provider auth, retries, error
   taxonomy, astrocyte export, but submit→poll→bytes jobs, NOT broca's
   run-loop machinery (WAL/turns/tool-dispatch/resume, none of which a
   one-shot render needs). Local generation grows in **synapse**. The
   driver is chartered when its first real consumer exists; `occipital`
   (visual cortex) is the parked name candidate. Fusiform's job is to make
   that driver THIN: it inherits what-exists/what-it-costs from fusiform
   instead of embedding it.
2. **Second modality's first source.** When image/video gets a consumer,
   pick the upstream then — no single catalog covers it today; expect
   multiple sources normalized into one schema (that is the domain argument
   that justified this module).
3. **Astrocyte consolidation and the retirement of `cortexkit-model-catalog`.**
   Astrocyte's models.dev price lane moves to fusiform-served data when
   fusiform is stable; coordinate with ASTRO, do not force.

   **Settled 2026-08-12 (Ufuk), after checking with SUBC, ASTRO and BROCA.**
   Fusiform OWNS the registry, and the commons crate is SUPERSEDED rather
   than turned over. The served schema ships as `fusiform-protocol` in this
   repository, not as a major version of `cortexkit-model-catalog`.

   The earlier plan here was the reverse, on the premise that a cross-repo
   payload needs a neutral published home. SUBC ruled that the payload rule
   requires one definition consumed by both sides and never required
   neutrality: served types are declarations authored by the producer, and
   producer ownership makes drift unauthorable because schema and crate move
   in one commit. `subc-protocol` is the precedent. The commons crate living
   where it does was a workaround for there being no owner module.

   Conditions, all met: the crate depends on serde only (ASTRO's condition,
   enforced by a test that names each forbidden dependency); version
   discipline is enforced in CI, because a path-dep consumer cannot see a
   code change that does not move the version — verified in this repo's own
   lock file, where a path dep records no source and no checksum while a
   registry dep records both; and produced outputs are pinned as golden
   fixtures of real served payloads.

   Retirement remains the TAIL: nothing is removed while a dependent exists.
   The crate gets a superseded header pointing at the successor. The gate is
   astrocyte's switch alone — measured, they are the only dependent, and
   their entire use is one type (`CatalogDoc`) at one call site, so the
   cutover is a function's input type rather than a migration. BROCA never
   consumed it; their `broca-catalog` parses independently, which is why
   their tier parsing was correct while the crate's was wrong.

## Non-goals

- No model execution, routing, or selection. Fusiform describes; it never
  dispatches.
- No policy. Which models a consumer serves, allows, or prefers is the
  consumer's business (or the decision plane's). Fusiform carries facts.
- No credentials in v1; no scraping past public endpoints.

## Operational bar (fleet standard, non-optional)

Supervised module under subc (`ck-fusiform`, pinned codesign identifier);
Health-Path-Rule v3 compliant health checks (insulated lane, no subprocess
exec, no live store reads on the reply path); store is backup-class the
moment it holds snapshot history (enroll with engram, consistent-snapshot
capture, restore-with-monotonic-fence if any monotonic state appears);
deploy via the fleet ladder (stage-signed, marker differential, inode
verification); CI on Blacksmith with `--locked` builds; `cortexkit-ci`
GitHub App secrets for private-repo CI.
