# CraftCloud Roadmap — proposal, not official

**Stage:** P0 RFC / Architecture (local prototype, no public RFC yet). **Implementation:** not started. **Last reviewed:** 2026-10-28.

## Next

1. Obtain maintainer guidance and `craftrules` access.
2. Clarify repository ownership, `CraftCloud` name and legal license.
3. Agree on minimal self-hosted MVP: secure bootstrap, tenant/RBAC, files/versions, admin UI, verified backups.
4. Approve API conventions and threat model; name pilot app maintainer.

**Detailed roadmap:** [docs/07-roadmap/phases.md](docs/07-roadmap/phases.md) · **Ranked gaps:** [docs/gaps.md](docs/gaps.md).

### Revision log

2026-10-28 — PR-005 (CC-P01-05) implemented locally: login, cookie sessions (hashed at rest), CSRF header + Origin check, in-memory rate limiting, logout/logout-all, password change, operator-issued and offline password reset links, migration 0004. Not reviewed or merged.

2026-10-28 — PR-004 (CC-P01-04) implemented locally: one-shot instance setup (`/api/v1/setup/*`), Argon2id password hashing, bootstrap token via `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE`, migration 0003, `craftcloud-auth` crate. No login yet. Not reviewed or merged.

2026-10-28 — PR-003 (CC-P01-03) implemented locally: `craftcloud-db` (sqlx, runtime queries), migrations 0001/0002, explicit `craftcloud-server migrate`, startup schema verification, `CRAFTCLOUD_DATABASE_URL_FILE`, `cargo xtask db-test` (Docker). ADR-0012 proposed. Not reviewed or merged. MSRV is 1.99 (installed toolchain; 1.95 dropped by user decision, ADR-0010).

2026-10-28 — PR-002 (CC-P01-02) implemented locally: typed env config (exit code 2 on invalid config), secret-file loader (not wired), trusted-proxy client IP, JSON errors for 413/503 (`TIMEOUT`), bounded graceful shutdown. ADR-0011 proposed. Not reviewed or merged.

2026-10-28 — PR-001 (CC-P01-01) skeleton implemented locally: Rust workspace, `/health/live` + `/api/v1/info` only, `cargo xtask ci`. Not reviewed or merged; MSRV 1.95 not verified.

2026-10-28 — Phase 0 started locally: overlap map (CollabCraft, CraftHub found), craftrules still 404, consistency fixes K1–K9, ADR-0009 proposed. See docs/07-roadmap/phase-00-log.md.

2026-10-28 — Initial documentation-only draft. No code, tests, service, repository creation or maintainer agreement has occurred.
