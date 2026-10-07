# fusiform

The CortexKit AI-model capability catalog: one structured data plane describing
what AI models exist, what they can do, and what they cost — across every
modality (LLM, image generation, video generation, audio, embeddings), across
every provider.

Named for the fusiform gyrus: the brain's recognition-and-categorization
region, the part that knows what things are and what their properties are.

Runs as `ck-fusiform`, a supervised module of the subc daemon. See
`docs/charter.md` for scope, constraints, and the fleet context.

## Building

The repository builds from a clone of itself alone. Every dependency comes from
crates.io, including the CortexKit crates it uses (the subc client and wire
crates from subconscious, and the store crates from commons), at the versions
pinned in `Cargo.lock`:

```sh
cargo build --release --locked
cargo test --workspace --locked
```

`cargo t` and `cargo c` are aliases for the guarded test and clippy runs (see
`.cargo/config.toml`), and `.githooks/pre-push` refuses a push whose lock,
formatting, or committed lock would fail CI.

There is deliberately no dependency on engram, the fleet's backup module.
Fusiform is enrolled with it through a descriptor file
(`crates/fusiform-module/data/engram-catalog.json`), and validating that
descriptor is engram's job at capture time.

Scripts: `scripts/release-build.sh` builds with the commit stamped in and runs
anywhere. `scripts/stage.sh` signs with the Apple Developer identity named in
`FUSIFORM_SIGNING_IDENTITY` and is
macOS-only; it refuses immediately elsewhere rather than failing partway.

## Mutation proofs

A guard is only worth something if its test fails when the rule it guards is
broken. `mutations.toml` records that proof for each standing guard: one row
breaks the rule (an exact `old` text replaced by `new`) and names the tests that
must go red, by full libtest path. Integration tests build into one binary per
crate, so a test in `crates/fusiform-store/tests/it/serve.rs` is
`serve::<name>` under `--test it`.

`ckdev-mutate` (the `cortexkit-mutate` crate in cortexkit/commons, installed at
the commit pinned in `.github/workflows/ci.yml`) replays the rows. CI's
`mutations` job replays every row on each push to `master`, the rows a change
touches on pull requests and `train/**` pushes, and every row with `--broad` on
the nightly schedule. `--broad` runs all of a package's test targets and grades
a catch outside the expected target as `CAUGHT_BROADLY`, unless the row is
marked `hub` with the shared property it guards as the reason.

A new guard gets a row, not a proof written into a commit message. Prove it
and let the runner append the row; it appends only when the named tests go red:

```sh
ckdev-mutate prove --id point-in-time-boundary-inclusive-outer \
  --guards "a point-in-time read AT a boundary instant serves that era" \
  --file crates/fusiform-store/src/serve.rs \
  --old 'AND e.boundary_at_ms <= ?2' --new 'AND e.boundary_at_ms < ?2' \
  --test-file crates/fusiform-store/tests/it/serve.rs \
  --package fusiform-store --target='--test it' \
  --expect-red serve::a_read_at_a_boundary_instant_returns_the_new_value
```

If the guard sits in a function a caller must reach, add a second row that
removes the call. `ckdev-mutate check` validates every row without editing
anything; `ckdev-mutate run --only <id>` replays one. The rows' anchors must match
exactly once, so an edit to guarded code can fail `check` until the row's
`old` text is updated to match.

When nobody knows yet which test should catch a mutant (an age-selected sweep,
auditing old code), use `ckdev-mutate explore`. It runs the whole package (or the
workspace with `--workspace`), lists every test that went red, or explains a
survivor, and `--append` writes the row on a catch:

```sh
ckdev-mutate explore --package fusiform-module \
  --file crates/fusiform-module/src/fetch.rs \
  --old 'blake3::hash(bytes)' --new 'blake3::hash(&[])'
```

## License

MIT; see `LICENSE`.
