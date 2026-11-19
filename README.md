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
aliases in `.cargo/config.toml`. The full gate a change must pass is in
`CONTRIBUTING.md`.

Tests that reach the live upstream do nothing unless `FUSIFORM_LIVE=1` is set,
and tests that read a full captured models.dev document do nothing unless
`FUSIFORM_FULL_PAYLOAD` names that file. When unset they print a skip notice
and pass. CI sets neither.

fusiform runs on Windows, Linux and macOS. The CortexKit installer
(`ck setup`) does not install it yet, and it is not in the public release
index, so today it is built from source on any of them; the scripts below are
maintainer tools.

- `scripts/release-build.sh` builds release binaries with the commit stamped
  in and refuses a dirty tree. It runs on every platform.
- `scripts/stage.sh` prepares a signed build for a maintainer's own macOS
  placement, using the signing identity named in `FUSIFORM_SIGNING_IDENTITY`.
  It refuses on other platforms; use `scripts/release-build.sh` there.
- `scripts/refresh-seed.sh` refreshes the embedded copy of the upstream
  catalog.

## Prepare signed-store curation requests

`curation-write` is a maintainer example, separate from `ck models` and its
local overrides. It reads the public HTTPS feed, verifies the signed head,
snapshot, and subsequent releases, then writes request bodies to disk. Pin
the release key ID and unpadded base64url Ed25519 public key from a trusted
source. The example has no write token and never sends a request to the
curation API.

```sh
curation_write() {
  cargo run --locked -p fusiform-publish --example curation-write -- \
    --url "$STORE_URL" --key-id "$STORE_KID" --public-key "$STORE_PUBLIC_KEY" "$@"
}

curation_write vendor model-aliases --file crates/fusiform-module/data/model-aliases.json \
  --note "$NOTE" --source "$SOURCE" --review-by "$REVIEW_DATE" --out requests/aliases
curation_write draft grades "$MODEL_KEY" --value "$GRADE_JSON" \
  --note "$NOTE" --out requests/grade
curation_write from-local --model "$MODEL_KEY" --local-json local-judgments.json \
  --out requests/local
```

- `vendor` validates the whole replacement with the other signed documents
  and the verified upstream catalog. It reports row additions, removals, and
  changes, and copies the file's JSON verbatim into `request.json`. The
  comparison is against the last published copy, not any unpublished edit.
- `draft` accepts `grades`, `effort_grades`, `kind_grades`, `workload_efforts`,
  and `decommissioned`. Pass the whole value for one model key as `--value`,
  or use `--remove`. The example shows the signed value beside the proposal
  and checks the model and any named efforts against the verified catalog.
- `from-local` accepts a hand-prepared JSON object with a `note` and zero or
  more of the five draft-family keys, each containing the value for the
  selected model. Use `null` for an explicit removal; omit a family to leave
  it alone. For example:
  `{"note":"Record a reviewed grade","grades":{"elo":2050,"eq":1980,"graded_effort":"high"}}`.
  `ck models grades --model KEY --json` and `ck models marks --json` can help
  gather values, but there is no single JSON export of local judgments with
  workload efforts. Changed proposals go to `<out>/<family>/request.json`.

Review the body and the printed `PUT` path, then send each body yourself to
that path on the store, with the store's curation token as
`Authorization: Bearer <token>` and `Content-Type: application/json`. The
token is issued by the store's operator and is never stored in this
repository. A vendor replacement takes effect
immediately; judgment drafts wait for approval. Existing request files are
never overwritten.

## Documentation

Start with [`docs/architecture.md`](docs/architecture.md). Contracts, the data
model and operator guides are linked from there.

## License

MIT; see `LICENSE`. The embedded models.dev catalog is MIT-licensed by its
authors; see `THIRD-PARTY-NOTICES.md`.
