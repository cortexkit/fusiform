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

**This repo does not build from a clone of itself alone.** The subconscious
crates come from crates.io, but the commons store crates have never been
published, so commons is still a path dependency, expected beside this repo:

```
cortexkit/
  fusiform/      <- this repo
  commons/       <- cortexkit-store, cortexkit-store-types
```

Commons is public, so cloning it beside this one is the whole setup. Measured
2026-09-22 with `gh api repos/cortexkit/<name> --jq .visibility`; re-run that if
a clone fails on permission, since visibility is a setting in another repository
and nothing here can notice it change.

Cargo loads every workspace manifest before honouring `-p` or `--exclude`, so
one missing sibling gates everything, including `cargo build --release` and
`cargo metadata`. No partial build is possible.

There is deliberately no dependency on engram, the fleet's backup module.
Fusiform is enrolled with it through a descriptor file
(`crates/fusiform-module/data/engram-catalog.json`), and validating that
descriptor is engram's job at capture time, not a test here compiled against
engram's source through a sibling path.

Scripts: `scripts/release-build.sh` builds with the commit stamped in and runs
anywhere. `scripts/stage.sh` signs with an Apple Developer identity and is
macOS-only; it refuses immediately elsewhere rather than failing partway.
