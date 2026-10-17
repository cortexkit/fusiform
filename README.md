# fusiform

Fusiform is the CortexKit catalog of AI models: what models exist, what each
can do, and what it costs, across providers and modalities, with the full
history of every fact. It runs as `ck-fusiform`, a module supervised by the
CortexKit daemon (subc). It polls the public [models.dev](https://models.dev)
catalog every 30 minutes, records every poll and every change in an
append-only SQLite store, and serves point-in-time reads, fact histories,
curated billing planes and subscription prices to other modules as tools. An
operator reads it with `ck models`.

The module enrolls its own store with the CortexKit backup module, engram: at
every start it writes its backup descriptor, `engram-catalog.json`, beside
`store.db`. See [`docs/backup-and-restore.md`](docs/backup-and-restore.md).

| Crate | Role |
| --- | --- |
| `fusiform-core` | domain types, money, and the models.dev normalizer; no I/O |
| `fusiform-store` | the append-only SQLite store: schema, ingest, point-in-time reads, corrections |
| `fusiform-protocol` | the wire types consumers compile against; published to crates.io |
| `fusiform-module` | the `ck-fusiform` module: poll loop, tool routing, health, curated data |
| `fusiform-cli` | the `ck-models` operator CLI, run as `ck models <verb>` |
| `fusiform-testkit` | shared test helpers; never linked into a binary |

## Build and test

Everything below needs only a Rust toolchain. Every dependency, including the
CortexKit crates, comes from crates.io at the versions in `Cargo.lock`, so the
repository builds from its own checkout.

```sh
cargo build --release --locked
cargo test --locked --workspace --all-targets
```

`cargo t` (`test --locked`) and `cargo c` (clippy with `-D warnings`) are
aliases in `.cargo/config.toml`. The full gate a change must pass, including
the mutation-proof check, is in `CONTRIBUTING.md`.

Tests that reach the live upstream do nothing unless `FUSIFORM_LIVE=1` is set,
and tests that read a full captured models.dev document do nothing unless
`FUSIFORM_FULL_PAYLOAD` names that file. When unset they print a skip notice
and pass. CI sets neither.

Release and packaging scripts:

- `scripts/release-build.sh` builds release binaries with the commit stamped
  in and refuses a dirty tree. It runs anywhere.
- `scripts/stage.sh` signs with the Apple Developer identity named in
  `FUSIFORM_SIGNING_IDENTITY`. It is macOS-only and refuses elsewhere.
- `scripts/refresh-seed.sh` refreshes the embedded copy of the upstream
  catalog.

### Mutation proofs

`mutations.toml` records, for each standing guard, an edit that breaks it and
the tests that must go red. See [`docs/mutations.md`](docs/mutations.md).

## Documentation

Start with [`docs/architecture.md`](docs/architecture.md). Contracts, the data
model and operator guides are linked from there.

## License

MIT; see `LICENSE`. The embedded models.dev catalog is MIT-licensed by its
authors; see `THIRD-PARTY-NOTICES.md`.
