# Reliability, capacity and operational objectives

## Single-node MVP, no false HA promises

One Rust service, one PostgreSQL database, durable file volume, TLS reverse proxy. The API is restartable, but initial deployment is **not highly available**. When database, storage, proxy or network is unavailable, return bounded actionable errors; no corruption or silent data loss.

## Suggested pilot SLOs (to validate, not promises)

- Authorized metadata API P95 <= 300 ms locally under defined 20-user test workload, excluding file streaming and WAN latency.
- 99.5% service availability target in pilot after operations monitoring, not a guaranteed SLA.
- Database/schema backup daily plus file manifest, tested restore at least monthly during pilot.
- RPO/RTO to be negotiated; initial target RPO <= 24h and RTO <= 4h **only after demonstrated drill**.
- Limits: max body/upload chunk, file size, API requests, invitations, sessions, org quota and concurrent workers, all configurable with safe defaults.

## Transactions + object storage

Databases and object stores lack shared atomic commit. Use state machine: `pending` → `uploaded_verified` → `committed` or `expired/failed`; metadata transaction references only verified immutable blob. Background GC is idempotent and honors holds/backup retention. Retry after process crash must resume or cleanly expire.

## Observability

Structured logs and request IDs; metrics: API rate/error/latency by route template, active sessions, uploads pending/failed, blob consistency, quota, DB pool, GC, backup age. No tokens, original filenames in broad logs by default, raw file contents, plaintext invites, password hashes or sensitive user agent fingerprinting.

## Version support

Semantic versioning for service release; API major version in path; DB schema version explicitly tracked; database migrations backed up and rollback assessed individually. Update client only after compatibility tests. Production upgrades gated by backup verification and release notes.
