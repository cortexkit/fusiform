# Security

## Reporting a vulnerability

GitHub's private vulnerability reporting is not yet enabled for this
repository. Until it is, open an issue that says you have a security report
and asks for a private way to send it. Put no details of the vulnerability in
the issue itself.

When you send the report, include what you found, how to reproduce it, and
what you think the impact is.

## Threat model

Fusiform runs locally as a module under the CortexKit supervisor. This section
states what it trusts, what it treats as untrusted input, and what it never
does.

### Untrusted input: the models.dev catalog

Fusiform fetches `https://models.dev/api.json` over HTTPS every 30 minutes. The
document is not signed or otherwise authenticated beyond TLS, so its content is
treated as untrusted data:

- It is parsed into typed values with explicit unknowns. Fields whose meaning
  cannot be established are refused or stored as unknown, never defaulted.
- Fields that would tell a consumer how to render a request (`provider.npm`,
  per-model provider overrides, request-selecting mode parameters) are never
  served.
- Money is parsed into exact integers; negative and non-numeric rates are
  refused.
- A document that would remove more than a quarter of the stored catalog is
  refused as implausible and writes nothing.
- Everything else the upstream publishes is recorded and served as the
  upstream's claim. A wrong or malicious value from models.dev will be served
  until models.dev changes it or an operator records a correction.

The embedded seed is a copy of the same catalog, captured by
`scripts/refresh-seed.sh` and committed, so it carries the same trust as the
fetch it came from.

### Trusted: the supervisor and the store

- **The supervisor (subc)** authenticates the module's connection, hands it the
  storage descriptor, and routes tool calls to it. Fusiform trusts the
  descriptor's path and does not derive one of its own.
- **Callers.** Fusiform does no authorization of its own: a request carries no
  caller identity, so any client the supervisor routes to it can call any
  tool, including the three that write (`catalog.correct`,
  `catalog.mark_artifact`, `catalog.retract_artifact`). Their blast radius is
  bounded by what a request can express instead: each names one model or one
  observation, there is no wildcard, each previews unless `dry_run` is
  `false`, and none can change a stored value or delete a row. A correction
  can still make reads of one model's named facts inside a past window
  withhold those facts.
- **The store** is a SQLite file in the module's data directory, created and
  permissioned by the supervisor's storage policy. Fusiform trusts its
  contents. Anyone who can write that file can change what fusiform serves.
- **Curated data files** are compiled into the binary and trusted as reviewed
  source.

### What fusiform never does

- It stores no credentials and asks for none. The `ck-models` CLI reads the
  supervisor's per-user connection file to reach the module, like every subc
  client.
- It makes no outbound network calls other than the models.dev fetch. The only
  other connection is the local one to the supervisor.
- It sends nothing upstream beyond the HTTP request itself, with a
  `ck-fusiform/<version>` user agent and, after the first poll, the stored
  ETag.
- It executes nothing it fetches and never selects how a consumer talks to a
  provider.
