# CraftCloud — Architecture RFC and implementation briefings

> **Status:** independent documentation-first proposal, **not an official storytold repository**, not deployable server software.  
> **Research date:** 2026-10-28.  
> **Language:** German engineering documentation; external RFC draft in English.

CraftCloud is an **optional, local-first, self-hostable** account, organization, workspace, file and project service for the Craft App Suite. Local applications must remain usable without account, network access, or the CraftCloud service.

This package is a **proposed future repository structure** consistent with documented patterns from `storytold/figcraft`, `storytold/wordcraft`, `storytold/gridcraft`, `storytold/filmcraft`, `storytold/photocraft`, `storytold/vectorcraft`, `storytold/craft-launcher`, `storytold/craft-libs`, `storytold/craft-ai-integrations`, and `storytold/artcraft-services`. It neither claims maintainers' endorsement nor copies their code. Before publishing, confirm brand permission, ownership, copyright and licensing.

## Status: not production-ready, local prototype

The repository now contains a minimal Rust workspace skeleton (PR-001 / CC-P01-01): `craftcloud-core` (typed capabilities), `craftcloud-api` (axum router with `GET /health/live` and `GET /api/v1/info` only), `craftcloud-server` (binary) and `xtask`. There is no authentication, database, storage or upload code. `/api/v1/info` reports `server_mode: "development-prototype"`. Do not deploy it.

```sh
cargo build --locked -p craftcloud-server   # build the server
CRAFTCLOUD_BIND=127.0.0.1:8080 cargo run -p craftcloud-server   # default bind is 127.0.0.1:8080
cargo xtask ci   # fmt check, clippy -D warnings, tests, relative-link check of all Markdown files
```

Invalid configuration prints every problem and exits with code 2 without binding a socket (runtime failures exit with 1); keys and rules are in [docs/06-operations/deployment.md](docs/06-operations/deployment.md) and [ADR-0011](docs/09-decisions/adr-0011-configuration-policy.md).

The workspace declares `rust-version = "1.95"`; this is not verified locally yet (ADR-0010). Roadmap: [ROADMAP.md](ROADMAP.md) and [docs/07-roadmap/phases.md](docs/07-roadmap/phases.md).

## Start here

1. [Documentation index](docs/README.md)
2. [Upstream comparison](docs/01-repository/upstream-comparison.md)
3. [Open questions & decisions](docs/00-product/open-questions.md)
4. [System architecture](docs/02-architecture/system.md)
5. [Domain model](docs/02-architecture/domain-model.md)
6. [Phase plan](docs/07-roadmap/phases.md)
7. [Phase 0 acceptance checklist](docs/07-roadmap/phase-00-rfc.md)
8. [Maintainer RFC](docs/10-community/github-rfc-en.md)

## Proposed long-term monorepo (not implemented in this package)

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

> **Do not treat omitted files as finished implementation.** Only the PR-001 skeleton (see Status above) exists as code; there is still no executable deployment stack, no unapproved logos, and no copied upstream license texts.

## Sequencing

`RFC → threat model and contract → backend/auth/RBAC → storage/versioning → admin interface → single-app integration → realtime adapter → managed/enterprise capabilities`.

## Core decisions pending approval

- Whether project belongs inside `storytold` or starts as independent community RFC.
- Licensing: propose MIT OR Apache-2.0 **only if rightsholders accept**; no re-use of restricted `artcraft-services` source.
- Which UI stack: a React/TS web admin is pragmatic for a separate server product, but **not** the Rust-only `craft-launcher` pattern; do not move React into the Launcher.
- Database: PostgreSQL as first production target; SQLite only if separately designed and tested, not a silent promise.
- Namespace: `craftcloud-*` for packages is a recommendation, not upstream policy.

The `docs/01-repository/` folder distinguishes observations from recommendations and unknowns.
