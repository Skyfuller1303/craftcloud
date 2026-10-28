# Umsetzungsbacklog / Vorschlag für kleine Pull Requests

## Erste 15 PRs nach P0-Freigabe

| PR | Umfang | Abhängig | Kernabnahme |
|---|---|---|---|
| PR-001 | Workspace skeleton, lint, docs check, health | P0 | `cargo check`, docs pass |
| PR-002 | typed config, secret files, trusted proxy | 001 | invalid config refuses startup |
| PR-003 | PostgreSQL schema + migration runner | 001 | fresh/upgrade DB integration tests |
| PR-004 | one-time bootstrap endpoint | 002–003 | concurrency/expiry safe |
| PR-005 | password hash + web sessions, logout | 004 | 401/CSRF/timeout tests |
| PR-006 | organizations, memberships, RBAC policy | 005 | cross-org isolation |
| PR-007 | workspaces, invitations | 006 | invite binding/revocation |
| PR-008 | project/file metadata and API pagination | 006 | no IDOR, path-safe |
| PR-009 | local object storage, checksum & bounded streams | 008 | abort/disk-full tests |
| PR-010 | upload sessions, quotas + GC state | 009 | collision/concurrency |
| PR-011 | version CAS commit and conflicts | 010 | two writer 412 |
| PR-012 | minimal admin setup/login UI | 005 | CSRF/a11y |
| PR-013 | admin members/projects/files UI | 011–012 | full manual journey |
| PR-014 | audit & structured diagnostics | 006 | secrets redaction |
| PR-015 | consistency backup/restore + Docker ops | 011/014 | independent restore proof |

## Slice discipline

Eine PR kann ein Problem inkl. Tests lösen, nicht das gesamte Dashboard. Keine zwei Auth-Architekturen in parallelen Branches. Cross-app PRs erst nach SDK-Vertrag; Integrationsänderung muss im jeweiligen App-Repo genehmigt werden. Der Backlog ist vorgeschlagen, keine gestarteten GitHub-Issues.

## Bug/feature labels (proposed)

`type:feature`, `type:bug`, `area:auth`, `area:storage`, `area:api`, `area:admin`, `area:integrations`, `security`, `breaking-change`, `needs-maintainer-decision`, `good-first-issue`, `phase:P0..P7`. Nicht voraussetzen, dass diese Labels im zukünftigen Repo bereits existieren.
