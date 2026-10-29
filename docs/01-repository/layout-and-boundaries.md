# Monorepo-Layout, Paketgrenzen und Eigentumsverhältnisse

## Logisches Layout

```text
apps/craftcloud-server/        Rust binary, config, health, shutdown
apps/craftcloud-admin/         standalone browser admin interface
crates/craftcloud-core/        domain types + deterministic validation
crates/craftcloud-api/         HTTP contract, handlers and middleware
crates/craftcloud-auth/        login, tokens, sessions, permission checks
crates/craftcloud-db/          tenant-scoped repository interfaces + PostgreSQL impl.
crates/craftcloud-storage/     FS, optional S3, streaming, integrity
crates/craftcloud-backup/      backup/verify/restore of database tables and blobs (depends on db + storage only)
crates/craftcloud-client/      typed Rust client; optional wasm-safe transport
xtask/                        local dev, contract checks, lint/test aggregations
packaging/                    Docker, reverse proxy examples, manifests
contracts/openapi.yaml        API contract sketch (currently incomplete)
docs/                         briefings, milestones, ADRs, verification
```

## Dependency flow

`core <- auth, db, storage <- api <- server`; `client` depends only on core DTOs / HTTP contract, not server internals; `admin` depends on the published contract, not database types. Cross-service integration uses explicit versioned protocols, never direct access to FigCraft databases/data directories.

## Design choices for owner review

- Prefer **one deployable server process** during MVP, with internal modular crates; microservices would add ops complexity without clear benefits.
- `crates/*` do not own environment variables; config is loaded and validated at server edge.
- `db` enforces tenant scope in every query; schema migrations run explicitly and safely on startup only under operator-controlled policy.
- `storage` only sees opaque blob keys chosen by server, never unvalidated user paths.
- `client` supports per-request cancellation, bounded retries, idempotency and typed error models.
- `admin` is optional from the server's perspective, but should be buildable and hosted from the deployment packaging.
- Distinguish product naming from crate identity. Proposed Rust packages use `craftcloud-*`, but do not claim upstream consensus.

## Implementation guardrails

- Keep route handlers skinny; no business logic in React UI.
- Every new table has foreign keys, tenant-scope policy and migration rollback/restore notes.
- No direct cross-tenant joins lacking explicit scope and tests.
- Maintain contract fixtures and API version policy in `docs/02-architecture/`.
- Use a feature flag or semver gate for experimental integration; avoid silent protocol widening.
