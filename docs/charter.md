# Fusiform — charter

Status: chartered 2026-08-11; **shipped and deployed 2026-08-12** as the
fleet's 17th supervised module. This document remains the founding contract
and the first thing a new owner reads, so where the code has since decided
something differently, the difference is marked HERE rather than left for the
reader to discover downstream.

The v1 scope list below is the charter's original intent, annotated with what
was actually built. Two items changed on evidence: push was WITHDRAWN, and the
read surface is wider and differently named than planned. The reasoning for
each lives in `docs/design/schema-and-store.md` and the findings docs; the
annotations here exist so nobody builds a mental model the code rejects.

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

1. **Fetch** models.dev on a cadence (supervised, health-probed). See
   "Polling cadence" below — this line said "jittered" from the charter's
   first draft and nothing ever implemented it.
2. **Normalize** into fusiform's own schema (see Schema, below).
3. **Diff** against the last snapshot; store snapshot history + provenance
   (source, fetch time, content hash) in the module store.
4. **Push**: on change, call broca's `catalog.refresh` over a subc route.
   Fusiform pushes; consumers validate and swap. No consumer polls.

   **WITHDRAWN — fusiform emits no pushes, and `emits_push` is `false` in the
   manifest.** Not deferred: the mechanism was wrong, not merely
   untransportable. If push were the ONLY notification path, a dropped push
   would leave a consumer permanently stale, and an acknowledgement would
   report that to a producer who cannot repair it. With consumers keeping a
   poll backstop, a dropped push costs LATENCY ONLY — and the backstop is a
   better acknowledgement than a message, because it cannot itself be dropped.
   So "no consumer polls" is exactly inverted: consumer polling is what makes
   the design sound. See §10 of `docs/design/schema-and-store.md` and
   `docs/findings/2026-08-11-push-has-no-acknowledgement.md`.
5. **Serve**: a read surface (`catalog.get`, `catalog.diff`) for any
   consumer that wants pull semantics.

   **`catalog.diff` was never built; the shipped surface is four tools:**
   `catalog.get` (current or point-in-time, with `withheld`, `uncertain` and
   `overridden` reported rather than silently applied), `catalog.history`
   (every era for one fact, with observation windows), `catalog.status`
   (poll record, counts, overrides), and `catalog.correct` (operator-driven
   correction of fusiform's own past record). A diff is derivable from
   `catalog.history` and carries no provenance of its own, which is why it
   lost to an era history that does.

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

> **On the size estimates in this section.** Where an item describes work in
> another repository, its scope language is the least reliable thing here, and
> not through carelessness: scope language is written by whoever proposes a
> change, which is systematically the party furthest from the code being
> changed. The information needed to size it is on the other side of a
> boundary, and nothing in the writing process crosses that boundary.
>
> Item 3 is the worked example. "The retirement of `cortexkit-model-catalog`"
> read as a migration for weeks; the measurement is one type at one call site.
> Lifecycle verbs — retire, migrate, deprecate, consolidate, cut over — carry
> an implied scope through connotation, and each implies a scale it has not
> measured.
>
> The fix costs one message, and nobody sent it: **ask the other party for the
> count before writing the estimate.** "How many call sites touch this?" would
> have collapsed item 3 the day it was written.
>
> **Items 1 and 2 are a different hazard and a worse one.** They describe
> consumers that do not exist yet, so there is nobody to ask — and what they
> carry is not a size estimate at all. It is DESIGN PRESCRIPTION: "a job-shaped
> sibling of broca, NOT broca's run-loop machinery"; "multiple sources
> normalized into one schema". Those sentences were written by people reasoning
> about a module nobody has built.
>
> A claim about a system with no keyboard on the other side does not merely
> survive unchecked — it is **self-confirming**. Whoever eventually builds that
> driver will read this item, take the prescription as a constraint, and build
> to it. At which point it is true, and was retroactively always true, and
> nothing was ever wrong. Reality gets shaped by the claim rather than being
> free to contradict it.
>
> So, explicitly, for whoever builds it: **these are positions, not
> requirements, and you are free to disagree with them.** They were reached
> without the information you will have. If the job shape is wrong when you get
> there, the charter is wrong — say so and change it. The marking is the only
> defence, because an unmarked position becomes a requirement its implementer
> inherits without ever seeing it argued.
>
> (This paragraph replaces one that said "those numbers are not estimates at
> all". Items 1 and 2 contain no numbers. The sentence was true of what it
> meant and false of what was on the page — the same defect the paragraph above
> is about, committed inside the correction for it.)

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

## Polling cadence (fleet precedent, set 2026-08-12)

Fusiform is the first supervised CortexKit module that polls a third party on
a timer, so this is a precedent rather than a preference — SUBC confirmed no
fleet convention exists. Written after deployment rather than before, because a
cadence claim from an undeployed module is a claim about a system that does not
exist.

**Measured, from the running module:**

| Property | Value |
| --- | --- |
| Interval | 30 minutes, `POLL_INTERVAL_MS`, not configurable |
| Phase | relative to process start, not the wall clock |
| Missed ticks | `MissedTickBehavior::Skip` |
| First tick | immediate — `tokio::time::interval` fires at once, measured 1ms |
| Jitter | **none** |
| Cost of a poll that finds nothing | one conditional GET, 304, zero body bytes |
| Cost of a poll that finds a change | ~355 KB gzipped |

**On jitter, honestly.** The charter said "jittered" before any code existed
and no code ever added it. What jitter would buy here is not aligning with
other clients of the same upstream on the half hour — and that property is
already present, because the interval runs from PROCESS START rather than from
a wall-clock boundary, so the phase is whatever second the module happened to
come up and re-randomises on every restart.

Present by accident is worth saying out loud rather than dressing up: nothing
chose it, and a future change to wall-clock-aligned scheduling would remove it
silently. The line is not being made true by adding a jitter knob, because that
would be adding a mechanism to justify a sentence.

The other thing jitter conventionally protects against — a restart burst — is
bounded by the supervisor, which allows 3 restarts with a 100ms backoff. Four
conditional GETs in a fraction of a second, each costing zero body bytes on a
304.

**The two rules that actually matter, which any future poller should copy:**

1. **The poll is not on the health path.** Health reads atomics the loop
   stamps and does arithmetic — no lock, no disk, no network. A health probe
   that queues behind a degraded upstream is useless exactly when it is needed.
2. **A poll that finds nothing costs nothing.** Conditional GET with a stored
   ETag; a 304 transfers no body. This is what makes a 30-minute cadence a
   politeness question rather than a cost one.

A future poller with a heavier upstream, or a fleet with several of them
hitting the same host, should revisit the jitter question on ITS evidence. This
section records what fusiform does and why, not a rule for everyone.

## Non-goals

- No model execution, routing, or selection. Fusiform describes; it never
  dispatches.
- No policy. Which models a consumer serves, allows, or prefers is the
  consumer's business (or the decision plane's). Fusiform carries facts.
- No credentials in v1; no scraping past public endpoints.

## Operational bar (fleet standard, non-optional)

Supervised module under subc (`ck-fusiform`, pinned codesign identifier);
Health-Path-Rule v3 compliant health checks (insulated lane, no subprocess
exec, no live store reads on the reply path); deploy via the fleet ladder
(stage-signed under `signing-topology/v2`, digests published, inode
verification); CI on GitHub-hosted runners with `--locked` builds, every
dependency from crates.io.

### Backup enrollment, and the obligation it creates

Installed 2026-08-12 after ENGRAM reviewed the descriptor.
`crates/fusiform-module/data/engram-catalog.json`, installed by
`scripts/install-enrollment.sh`:

```json
{ "entry_id": "fusiform/store", "class": "portable",
  "mechanism": "whole-db", "path": "store.db",
  "writer_interaction": "backup-api-live" }
```

**`class: "portable"` makes this fleet-wide, and that is the part a future
maintainer must not discover by accident.** An absent or unreadable
`store.db` fails the ENTIRE fleet's capture, not just fusiform's entry —
engram refuses to publish a generation marked complete while promised data is
missing. So the installer's refusal to enroll when no store exists is
load-bearing for every other module, and moving or renaming this store
without updating the descriptor takes down backups fleet-wide.

**Why `whole-db` rather than `page-db`** (ENGRAM's ruling, on measured
numbers): page-db earns its keep on large files with scattered small changes
— broca's WAL tree, prefrontal's 1.7 GB store. Fusiform is 19 MB with 9
changed facts in 6 hours, roughly 15 CDC chunks, re-uploading almost nothing
when quiet. Page-db would add page hashing and slab packing to save bytes
that are not being spent.

**Why the WAL is safe.** Engram does not file-copy; it opens through SQLite's
online backup API on a `mode=ro` connection, so the pager resolves committed
WAL frames into a transactionally consistent snapshot with no checkpoint and
no interference with the single writer. It also refuses a generation where a
header declares WAL and the `-wal` companion is absent, checked before any
connection opens — because opening one creates the companion it looks for.
That matters here because a naive file copy of a WAL-mode database comes back
BEHIND rather than corrupt, and `PRAGMA integrity_check` returns `ok` on it.

**The monotonic-state clause this section used to carry was wrong.** It said
to enroll with "restore-with-monotonic-fence if any monotonic state appears".
No such policy exists in engram — the name was invented in this repository
and attributed to their system. Fusiform's monotonic state, the catalog
version, is immune by construction instead: `max(now_ms, current + 1)`, so a
restore cannot rewind it. Immune by construction beats immune by policy, and
in this case the policy did not exist to rely on.
