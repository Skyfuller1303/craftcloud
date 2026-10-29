# Ranked gaps — documentation-first RFC

## Blocking decisions

1. `craftrules` inaccessible via public GitHub; confirm shared standards.
2. Official repository/name and licensing consent absent.
3. Identity provider strategy and role matrix not reviewed.
4. No locked API specification or database schema agreed with integration maintainers.

## Implementation gaps (in recommended order)

5. Secure bootstrap and HTTP skeleton. *PR-004: one-shot operator bootstrap via secret file implemented (no login/session, no CLI recovery yet).* *PR-001 skeleton: health + info only* (no bootstrap, auth, DB or storage). PR-002 added typed env configuration, secret-file loader, trusted-proxy handling and JSON errors for 413/503.
6. Tenant database, policy, Auth/session and invitations. *PR-005: password login, server-side cookie sessions, CSRF, rate limiting and password reset links implemented; policy engine, account disable API and invitations still open; device list implemented. PR-006: organizations, memberships, RBAC policy engine and account disabling implemented; workspace routes, invitations (PR-007) and organization deletion/soft-delete with export path still open.* *PR-007: workspaces, workspace members and link invitations (72 h, single use, revocable, inviter re-checked at acceptance) implemented. Still open: workspace and organization deletion/soft-delete with export path, e-mail delivery and address verification (link possession is the proof until SMTP exists), invitation resend, a browser page for `/invite`, and cleanup of old closed invitation rows. Email verification is needed before multi-tenant production use: until then an inviter can accept their own link and squat an address (residual risk; the operator can list accounts with origin and disable them, see threat model).* *PR-003: PostgreSQL schema (users, organizations, workspaces and memberships), forward-only migrations, tenant-scoped workspace repository; no policy engine, auth, sessions or invitations yet.*
7. Project/file metadata and streaming storage with checksums. *PR-009: local filesystem object store (`craftcloud-storage`) with streamed writes, byte limits, SHA-256, atomic publish, range reads, listing for reconciliation and fault injection implemented; no HTTP routes yet. Still open: upload/download routes, S3 adapter, resumable uploads, reconciliation job, real disk-full tests on a small volume (needs admin rights), symlink tests on Windows (need privilege), reconciliation (CC-P02-07) must delete unreferenced objects older than a grace period and stale `.part` files, because a cancelled publish can leave an unreferenced object or temp file, unix-only paths untested on the Windows development host.* *PR-008: projects, flat file metadata, soft delete with restore window, safe names and pagination implemented (metadata only). Still open: file content, versions and checksums (CC-F07/F08), storage adapters, purge of expired trash entries and quota accounting (CC-P02-06), folder hierarchy, moving files between projects.*
8. Version commit with CAS, soft delete, retention, quotas.
9. Browser admin interface with usable auth/workspace flows.
10. Backup/restore, deployment, audit/metrics.
11. Client SDK and single app integration.
12. FigCraft collaboration adapter and optional Launcher settings.
13. Enterprise SSO, S3, search, external shares, service accounts.

None of these are implemented by this blueprint. Update this list in the same PR as the corresponding milestone completion, following the style visible in FigCraft progress docs.

- Einladungsbereinigung (künftig): Einladungen, aus denen ein Konto entstand, sind über `users.created_via_invitation_id` (ON DELETE RESTRICT) referenziert; eine Bereinigung muss sie überspringen oder der Schlüssel wird auf `ON DELETE SET NULL` umgestellt.
