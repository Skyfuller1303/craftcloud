# Verifikationsmatrix – was muss wirklich getestet werden?

> Diese Matrix listet **geplante Tests**; kein Test wurde durch Erstellen dieses Planungsdokuments ausgeführt.

| Suite | Positive Case | Negative / Failure Case | Gate |
|---|---|---|---|
| Bootstrap | one-shot admin | concurrent double setup, stale token, crash | P1 |
| Auth | login, logout, revoke | brute force, CSRF, session fixation, expired | P1 |
| RBAC | workspace editor write | viewer write, revoked member, cross-org UUID | P1 |
| Invitation | recipient accepted | wrong email, replay, expired token | P1 |
| Files | metadata list, upload | path traversal, overlong name, hidden file | P2 |
| Streaming | 1 GB streamed under bounded memory | partial network cut, checksum mismatch, full disk | P2 |
| Versioning | single commit, restore | two-writer CAS, retry after 504 | P2 |
| Quota | correct accounting | concurrent reservations, cancelled uploads | P2 |
| Storage | FS blob checksum | symlink race, orphan, rollback recovery | P2 |
| Admin UI | entire organization journey | a11y, CSRF, 401/412/503 handling | P3 |
| Backup | DB + blob restore | missing blob / manifest corruption / old schema | P3 |
| SDK | open/save-copy | no cloud server, 401/412, TLS errors | P4 |
| FigCraft | editor/viewer/collab | revoke mid-session, protocol mismatch | P5 |
| OIDC | correct idp subject | forged issuer, changed email, revoked groups | P6 |
| S3 | FS-equivalent bytes | abort multipart, credential deny, partial object | P6 |

## Testarten

Unit tests: pure validation, state transitions, policy. Integration: PostgreSQL constraints + blobstore adapter + real request handler. E2E: two real accounts and workspaces, browser/SDK flows. Property/fuzz: parsers, IDs, filenames, version transition invariants; constrained random data. Performance: reproducible load envelope, bounded memory during upload; hardware/network disclosed. Security: OWASP-inspired abuse suite, dependency audit, sensitive log redaction; external assessment before production claims.

## Nachweisformat

Jeder PR test report: command, environment, commit SHA, exact date, cases pass/fail/skipped, logs as redacted artifacts, reviewer. Keine Formulierung „all tests pass“ aus bloß geplanten Checklisten. Regressionsfälle werden in PR und Feature-Briefing referenziert.
