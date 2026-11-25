# models.dev v1/v2 catalog pair

Two snapshots of models.dev's public catalog, fetched minutes apart on
2026-11-25, gzipped:

- `api-v1-20261125.json.gz`: `https://models.dev/api.json`, sha256 of the
  uncompressed bytes `b666fc36523df75efce7d815b4b97efd18fff14d07a0c72eafaebf221bbd6382`.
- `api-v2-20261125.json.gz`: `https://models.dev/experimental/v2.0/api.json`,
  sha256 of the uncompressed bytes `06d8b2678f695d1e068d8cda528157573dd75afde32bbfafcfcaa9db1576857f`.

They let tests compare how fusiform reads the two catalog versions on the same
upstream state. The data is models.dev's, published under the MIT license
(Copyright (c) 2025 models.dev).

[`cost-units.md`](cost-units.md) records the basis and admission for every
published v2 unit.

`explained-differences.json.gz` records shared-model additions of
`model.type: "chat"` only. Its rows are sorted by provider, model and fact; `old`
and `new` carry storage JSON text, or `null` for absence. Each row refers to a
quoted rule from the upstream's v1 translator in `causes`. The rows were
generated from the v1 fixture and saved v2 input. Tests compare them with the
actual normalized facts. This file does not yet account for other differences
in the cutover.
