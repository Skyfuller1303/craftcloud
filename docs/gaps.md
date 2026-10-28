# Ranked gaps — documentation-first RFC

## Blocking decisions

1. `craftrules` inaccessible via public GitHub; confirm shared standards.
2. Official repository/name and licensing consent absent.
3. Identity provider strategy and role matrix not reviewed.
4. No locked API specification or database schema agreed with integration maintainers.

## Implementation gaps (in recommended order)

5. Secure bootstrap and HTTP skeleton. *PR-001 skeleton: health + info only* (no bootstrap, auth, DB or storage). PR-002 added typed env configuration, secret-file loader, trusted-proxy handling and JSON errors for 413/503.
6. Tenant database, policy, Auth/session and invitations.
7. Project/file metadata and streaming storage with checksums.
8. Version commit with CAS, soft delete, retention, quotas.
9. Browser admin interface with usable auth/workspace flows.
10. Backup/restore, deployment, audit/metrics.
11. Client SDK and single app integration.
12. FigCraft collaboration adapter and optional Launcher settings.
13. Enterprise SSO, S3, search, external shares, service accounts.

None of these are implemented by this blueprint. Update this list in the same PR as the corresponding milestone completion, following the style visible in FigCraft progress docs.
