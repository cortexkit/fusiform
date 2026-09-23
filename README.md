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
anywhere. `scripts/stage.sh` signs with an Apple Developer identity and is
macOS-only; it refuses immediately elsewhere rather than failing partway.
