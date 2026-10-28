# CraftCloud — Architecture RFC and implementation briefings

> **Status:** independent documentation-first proposal, **not an official storytold repository**, not deployable server software.  
> **Research date:** 2026-10-28.  
> **Language:** German engineering documentation; external RFC draft in English.

CraftCloud is an **optional, local-first, self-hostable** account, organization, workspace, file and project service for the Craft App Suite. Local applications must remain usable without account, network access, or the CraftCloud service.

This package is a **proposed future repository structure** consistent with documented patterns from `storytold/figcraft`, `storytold/wordcraft`, `storytold/gridcraft`, `storytold/filmcraft`, `storytold/photocraft`, `storytold/vectorcraft`, `storytold/craft-launcher`, `storytold/craft-libs`, `storytold/craft-ai-integrations`, and `storytold/artcraft-services`. It neither claims maintainers' endorsement nor copies their code. Before publishing, confirm brand permission, ownership, copyright and licensing.

## Start here

1. [Documentation index](docs/README.md)
2. [Upstream comparison](docs/01-repository/upstream-comparison.md)
3. [Open questions & decisions](docs/00-product/open-questions.md)
4. [System architecture](docs/02-architecture/system.md)
5. [Domain model](docs/02-architecture/domain-model.md)
6. [Phase plan](docs/07-roadmap/phases.md)
7. [Phase 0 acceptance checklist](docs/07-roadmap/phase-00-rfc.md)
8. [Maintainer RFC](docs/10-community/github-rfc-en.md)

## Proposed long-term monorepo (not implemented in this ZIP)

```text
craftcloud/
├── apps/
│   ├── craftcloud-server/       # Rust service binary (later)
│   └── craftcloud-admin/        # browser admin UI (later; technology subject to maintainer decision)
├── crates/
│   ├── craftcloud-core/         # pure domain objects and validation
│   ├── craftcloud-api/          # HTTP routes, auth middleware
│   ├── craftcloud-auth/         # sessions, credentials, policy
│   ├── craftcloud-db/           # queries, migrations
│   ├── craftcloud-storage/      # fs/S3 backends
│   └── craftcloud-client/       # Rust client SDK; wasm-safe core where possible
├── xtask/                       # tooling, CI helpers (later)
├── packaging/                   # docker, reverse proxy, systemd (later)
├── contracts/                   # versioned protocol design
├── docs/                        # detailed briefings (this package)
├── .github/                     # PR/issue templates (later)
├── AGENTS.md
├── CLAUDE.md
├── CONTRIBUTING.md
├── SECURITY.md
├── Cargo.toml                   # future workspace manifest
└── README.md
```

> **Do not treat omitted files as finished implementation.** This ZIP deliberately does not contain a fake Cargo workspace, fake CI tests, executable deployment stack, unapproved logos, or copied upstream license texts.

## Sequencing

`RFC → threat model and contract → backend/auth/RBAC → storage/versioning → admin interface → single-app integration → realtime adapter → managed/enterprise capabilities`.

## Core decisions pending approval

- Whether project belongs inside `storytold` or starts as independent community RFC.
- Licensing: propose MIT OR Apache-2.0 **only if rightsholders accept**; no re-use of restricted `artcraft-services` source.
- Which UI stack: a React/TS web admin is pragmatic for a separate server product, but **not** the Rust-only `craft-launcher` pattern; do not move React into the Launcher.
- Database: PostgreSQL as first production target; SQLite only if separately designed and tested, not a silent promise.
- Namespace: `craftcloud-*` for packages is a recommendation, not upstream policy.

The `docs/01-repository/` folder distinguishes observations from recommendations and unknowns.
