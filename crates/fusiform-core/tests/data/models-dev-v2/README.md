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
