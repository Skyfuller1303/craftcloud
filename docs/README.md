# CraftCloud – Dokumentationsverzeichnis

> **Nur Briefings und Architekturvorschläge; kein lauffähiges Backend.** Alle Dokumente sind auf den Stand vom 28.10.2026 bezogen.

## Empfohlene Lesereihenfolge

1. [Vision und Grenzen](00-product/vision-and-boundaries.md)
2. [Open Questions](00-product/open-questions.md)
3. [Repository-/Upstream-Abgleich](01-repository/upstream-comparison.md)
4. [Architektur](02-architecture/system.md)
5. [Phaseplan](07-roadmap/phases.md)
6. [Maintainer RFC (English)](10-community/github-rfc-en.md)

## Alle Dokumente

Die `docs/`-Schreibweise ist bewusst **lowercase**, entsprechend den sichtbaren `storytold`-Repos.

### 00-product

- [Open Questions](00-product/open-questions.md)
- [Use Cases And Personas](00-product/use-cases-and-personas.md)
- [Vision And Boundaries](00-product/vision-and-boundaries.md)
- [Overlap Map](00-product/overlap-map.md)

### 01-repository

- [First Pull Request Checklist](01-repository/first-pull-request-checklist.md)
- [Governance And Ci](01-repository/governance-and-ci.md)
- [Layout And Boundaries](01-repository/layout-and-boundaries.md)
- [License Trademarks And Dependencies](01-repository/license-trademarks-and-dependencies.md)
- [Upstream Comparison](01-repository/upstream-comparison.md)

### 02-architecture

- [Api Conventions](02-architecture/api-conventions.md)
- [Domain Model](02-architecture/domain-model.md)
- [Reliability And Operating Model](02-architecture/reliability-and-operating-model.md)
- [Sync State Machine](02-architecture/sync-state-machine.md)
- [System](02-architecture/system.md)

### 03-features

- [Cc-F01 Bootstrap And Setup](03-features/cc-f01-bootstrap-and-setup.md)
- [Cc-F02 Identity And Sessions](03-features/cc-f02-identity-and-sessions.md)
- [Cc-F03 Users And Invitations](03-features/cc-f03-users-and-invitations.md)
- [Cc-F04 Organizations And Workspaces](03-features/cc-f04-organizations-and-workspaces.md)
- [Cc-F05 Roles And Permissions](03-features/cc-f05-roles-and-permissions.md)
- [Cc-F06 Projects And File Metadata](03-features/cc-f06-projects-and-file-metadata.md)
- [Cc-F07 Uploads And Downloads](03-features/cc-f07-uploads-and-downloads.md)
- [Cc-F08 Versions And Conflicts](03-features/cc-f08-versions-and-conflicts.md)
- [Cc-F09 Storage Drivers](03-features/cc-f09-storage-drivers.md)
- [Cc-F10 Quotas And Retention](03-features/cc-f10-quotas-and-retention.md)
- [Cc-F11 Audit And Activity](03-features/cc-f11-audit-and-activity.md)
- [Cc-F12 Web Admin Dashboard](03-features/cc-f12-web-admin-dashboard.md)
- [Cc-F13 Client Sdk And Discovery](03-features/cc-f13-client-sdk-and-discovery.md)
- [Cc-F14 Backup Restore And Upgrades](03-features/cc-f14-backup-restore-and-upgrades.md)
- [Cc-F15 Observability And Admin Health](03-features/cc-f15-observability-and-admin-health.md)
- [Cc-F16 Figcraft Collaboration Adapter](03-features/cc-f16-figcraft-collaboration-adapter.md)
- [Cc-F17 Launcher Opt In Integration](03-features/cc-f17-launcher-opt-in-integration.md)
- [Cc-F18 External Shares And Guests](03-features/cc-f18-external-shares-and-guests.md)
- [Cc-F19 Oidc Sso And Provisioning](03-features/cc-f19-oidc-sso-and-provisioning.md)
- [Cc-F20 Encryption Privacy And Data Export](03-features/cc-f20-encryption-privacy-and-data-export.md)
- [Cc-F21 Search And Previews](03-features/cc-f21-search-and-previews.md)
- [Cc-F22 Public Api Tokens And Service Accounts](03-features/cc-f22-public-api-tokens-and-service-accounts.md)

### 04-integrations

- [Figcraft Adapter](04-integrations/figcraft-adapter.md)
- [Launcher And Apps](04-integrations/launcher-and-apps.md)
- [Sdk Contract](04-integrations/sdk-contract.md)

### 05-security

- [Permission Matrix](05-security/permission-matrix.md)
- [Security Requirements](05-security/security-requirements.md)
- [Threat Model](05-security/threat-model.md)

### 06-operations

- [Backup Upgrade Runbook](06-operations/backup-upgrade-runbook.md)
- [Deployment](06-operations/deployment.md)
- [Incident Response](06-operations/incident-response.md)

### 07-roadmap

- [Backlog And Pr Sequence](07-roadmap/backlog-and-pr-sequence.md)
- [Effort And Risk Register](07-roadmap/effort-and-risk-register.md)
- [Phase 00 Rfc](07-roadmap/phase-00-rfc.md)
- [Phase 00 Log](07-roadmap/phase-00-log.md)
- [Phase 01 Backend](07-roadmap/phase-01-backend.md)
- [Phase 02 Files](07-roadmap/phase-02-files.md)
- [Phase 03 Dashboard](07-roadmap/phase-03-dashboard.md)
- [Phase 04 Sdk](07-roadmap/phase-04-sdk.md)
- [Phase 05 Collaboration](07-roadmap/phase-05-collaboration.md)
- [Phase 06 Enterprise](07-roadmap/phase-06-enterprise.md)
- [Phase 07 Release](07-roadmap/phase-07-release.md)
- [Phases](07-roadmap/phases.md)

### 08-quality

- [E2E Acceptance Scenarios](08-quality/e2e-acceptance-scenarios.md)
- [Quality And Privacy Gates](08-quality/quality-and-privacy-gates.md)
- [Verification Matrix](08-quality/verification-matrix.md)

### 09-decisions

- [Adr 0001 Separate Repository](09-decisions/adr-0001-separate-repository.md)
- [Adr 0002 Single Service](09-decisions/adr-0002-single-service.md)
- [Adr 0003 Database](09-decisions/adr-0003-database.md)
- [Adr 0004 Storage](09-decisions/adr-0004-storage.md)
- [Adr 0005 Ui](09-decisions/adr-0005-ui.md)
- [Adr 0006 Auth](09-decisions/adr-0006-auth.md)
- [Adr 0007 Collaboration](09-decisions/adr-0007-collaboration.md)
- [Adr 0008 Licensing](09-decisions/adr-0008-licensing.md)
- [Adr 0009 Conflict Status And Auth Schemes](09-decisions/adr-0009-conflict-status-and-auth-schemes.md)
- [Adr 0010 Http Framework And Toolchain](09-decisions/adr-0010-http-framework-and-toolchain.md)
- [Adr 0011 Configuration Policy](09-decisions/adr-0011-configuration-policy.md)
- [Adr 0012 Database And Migrations](09-decisions/adr-0012-database-and-migrations.md)
- [Adr 0013 P1 Identity Product Decisions](09-decisions/adr-0013-p1-identity-product-decisions.md)
- [Adr 0014 P2 Storage Defaults](09-decisions/adr-0014-p2-storage-defaults.md)
- [Adr 0015 Admin Ui Language And Design](09-decisions/adr-0015-admin-ui-language-and-design.md)
- [Adr 0016 P3 P4 Operations And Pilot](09-decisions/adr-0016-p3-p4-operations-and-pilot.md)
- [Adr 0017 Email Verification Deferral](09-decisions/adr-0017-email-verification-deferral.md)
- [Decision Log](09-decisions/decision-log.md)

### 10-community

- [Github Rfc En](10-community/github-rfc-en.md)
- [Maintainer Checklist](10-community/maintainer-checklist.md)
- [Source Register](10-community/source-register.md)

### General

- [Roadmap index](roadmap.md)
- [Architecture](architecture.md)
- [Gaps](gaps.md)
