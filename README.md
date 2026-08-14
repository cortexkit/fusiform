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

**This repo does not build from a clone of itself alone.** It takes four
CortexKit siblings as path dependencies, expected as directories beside it:

```
cortexkit/
  fusiform/      <- this repo
  subconscious/  <- subc-client-rs, subc-protocol, subc-transport, subc-core
  commons/       <- cortexkit-paths, cortexkit-store, cortexkit-store-types
  engram/        <- engram-core (dev-dependency, enrollment test only)
```

Three of those are private repositories, so **cloning fusiform is not enough
to build it** — you need read access to the siblings as well. Cargo loads
every workspace manifest before honouring `-p` or `--exclude`, so a missing
sibling gates the whole workspace: no partial build, no single-crate build,
no test run.

Written down because it cost an external contributor an afternoon to discover
(issue #1), and because the failure names a path rather than a permission:
cargo reports the directory it could not find, which reads as a broken
manifest rather than a repository you cannot see.

`engram-core` is a **dev**-dependency and powers one test
(`crates/fusiform-module/tests/enrollment.rs`), which validates the backup
enrollment descriptor against engram's own parser rather than a hand-written
schema copy. The other three are regular dependencies and are not optional.

Scripts: `scripts/release-build.sh` builds with the commit stamped in and runs
anywhere. `scripts/stage.sh` signs with an Apple Developer identity and is
macOS-only; it refuses immediately elsewhere rather than failing partway.
