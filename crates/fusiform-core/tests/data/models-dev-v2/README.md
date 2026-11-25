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

`explained-differences.json.gz` pins the reviewed cutover differences against
the saved v1 fixture and v2 input. Normalized and served wire rows are sorted
by provider, model and fact, followed by old and new JSON text (`null` for
absence) and a key into `causes`, which explains each approved change. Rates
kept in the store because v2 refused to replace their schedules, and provider
metadata differences, are listed separately.

To regenerate, update the decompressed JSON with reviewed rows from the saved
pair, then run `gzip -9n -c reviewed.json > explained-differences.json.gz`.
