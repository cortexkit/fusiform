# fusiform-protocol

The wire contract of the fusiform daemon: the request and response types a
consumer compiles against to call fusiform's tools (`catalog.get`,
`catalog.history`, `catalog.status`, `catalog.correct`,
`catalog.mark_artifact`, `catalog.retract_artifact` and `plan.prices`), plus
the rate and currency types those responses carry and the refusal codes a
consumer matches on when a request is declined.

Types only: no store, no network, no runtime. The crate depends on `serde`
and `serde_json` and nothing else, and a test keeps its dependency tree that
shape, so depending on it does not couple a consumer to the daemon's
internals.

The daemon itself lives in the same repository:
<https://github.com/cortexkit/fusiform>.

## License

MIT.
