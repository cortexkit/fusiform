# Architecture

Fusiform is a catalog of AI models: what exists, what each model can do, and
what it costs, with the full history of every fact. It polls the public
[models.dev](https://models.dev) catalog, records every poll and every change
in an append-only store, and serves the result to other CortexKit modules as
tools over the supervisor's route.

## 1. Parts

| Crate | Role |
| --- | --- |
| `fusiform-core` | the domain: identity, observations, eras, rates, money, and the normalizer that turns a models.dev document into typed facts. No I/O. |
| `fusiform-store` | the SQLite store: schema and migrations, ingest (diffing a normalized document against the facts in force), point-in-time reads, corrections, artifact marks, plan-price history. |
| `fusiform-protocol` | the wire types consumers compile against. Depends on `serde` and `serde_json` only. Published to crates.io. |
| `fusiform-module` | the `ck-fusiform` binary: subc handshake, poll loop, fetcher, tool routing, health, and the curated data files. |
| `fusiform-cli` | the `ck-models` binary, run as `ck models <verb>`: an operator client that calls the module's tools. |
| `fusiform-testkit` | test helpers; never linked into a binary. |

## 2. Data flow

```text
models.dev/api.json
        │  conditional GET every 30 minutes (fusiform-module: fetch, loop_)
        ▼
  normalize (fusiform-core) ── renderer-selecting fields quarantined
        │
        ▼
  ingest (fusiform-store) ── diff against facts in force; write one era per
        │                    changed fact; one observation per poll, always
        ▼
  SQLite store (observation, era, catalog_version, …)
        │
        ▼
  route (fusiform-module) ── point-in-time read, then serve-time layers:
        │                    open-weight inheritance, aliases, curated rows,
        │                    billing planes
        ▼
  tools: catalog.get / history / status / correct / mark_artifact /
         retract_artifact, plan.prices
```

The curated subscription prices take a separate path: the compiled-in file is
applied to `plan_price_era` on every start and served by `plan.prices`.

## 3. The rules that shape everything else

1. **Fusiform describes; it never dispatches.** It says what a model is, never
   how to speak to it. Upstream fields that select a request renderer are
   parsed, quarantined and never served
   ([`wire-contract.md`](wire-contract.md) §4.5).
2. **The upstream is data, not authority.** A field whose meaning cannot be
   established becomes an explicit unknown or stops the parse; it never gets a
   plausible default.
3. **Absent, null, zero and retired are different states**, and the wire keeps
   them apart. An absent rate is a statement about what was published, never
   about price ([`wire-contract.md`](wire-contract.md) §4.2).
4. **Every poll is recorded; only a change opens an era.** Unchanged polls and
   failed polls are rows too, which is what lets an era state the window in
   which its change happened ([`data-model.md`](data-model.md) §2–3).
5. **History is append-only.** Nothing is updated or deleted. Withdrawals are
   tombstone eras, corrections are eras that mark a window, and artifact marks
   and retractions are events ([`data-model.md`](data-model.md) §3, §5, §6).
6. **Serve-time layers are never stored.** Inheritance, aliases, curated rows
   and billing planes are applied when a read is served, and only on current
   reads, so a point-in-time read replays what the upstream published.
7. **Refuse rather than answer wrongly.** An unknown provider, an instant
   before the record begins, an undeclared billing plane, and a store that
   cannot be read are refusals with distinct codes, never an empty catalog
   ([`wire-contract.md`](wire-contract.md) §2).
8. **The catalog version never goes backwards**, even across a restore,
   because it is derived from the wall clock ([`data-model.md`](data-model.md) §4).
9. **Curated facts carry provenance and expire.** Every hand-curated claim
   names its source and a review date, and the test suite fails when a date
   passes ([`curated-data.md`](curated-data.md) §6).

## 4. Process model

- The module connects to the supervisor first, and opens the store from the
  descriptor the supervisor sends. It never derives the store path itself.
- An empty store is seeded from an embedded copy of the upstream catalog, so a
  fresh install with no network can answer reads. The eras written from it
  have boundary kind `seed`, which records where the store started rather
  than a change the upstream made.
- One poll loop writes; tool calls read on separate read-only connections and
  run off the connection reader, so health and other calls stay responsive.
- Health is computed from in-memory signals with no I/O
  ([`module-surface.md`](module-surface.md) §6).
- Fusiform emits no pushes. Consumers pull, and compare `catalog_version` to
  learn whether anything changed.

## 5. What fusiform does not do

- It does not convert currencies. models.dev states no currency; its rates are
  served as USD under the policy `models-dev-usd-v1`, and every rate names that
  policy in its `unit_provenance`.
- It does not choose a wire family, a request renderer, or which request
  selects a mode.
- It does not say whether a published list price applies to a particular
  route, subscription or contract. That is the consumer's knowledge.
- It does not infer identity from similar names: aliases and creators are
  curated with a cited source.
- It does not hold credentials and makes no outbound calls other than the
  models.dev fetch. See `SECURITY.md`.

## 6. Where to read next

- [`data-model.md`](data-model.md): tables, eras, observations, boundary kinds,
  corrections, artifact marks, plan-price storage.
- [`module-surface.md`](module-surface.md): tools, manifest, lifecycle, poll,
  health.
- [`wire-contract.md`](wire-contract.md): every request and response field.
- [`billing-planes.md`](billing-planes.md), [`plan-prices.md`](plan-prices.md),
  [`curated-data.md`](curated-data.md): the curated planes.
- [`operations.md`](operations.md): operator tasks with `ck models`.
- [`mutations.md`](mutations.md): the mutation-proof catalogue.
- [`backup-and-restore.md`](backup-and-restore.md): backup enrollment and what
  a restore does.
