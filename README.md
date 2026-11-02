# CraftCloud — Architecture RFC and implementation briefings

> **Status:** independent proposal with a working local prototype, **not an official storytold repository**, not production-ready server software.  
> **Research date:** 2026-10-28.  
> **Language:** German engineering documentation; external RFC draft in English.

CraftCloud is an **optional, local-first, self-hostable** account, organization, workspace, file and project service for the Craft App Suite. Local applications must remain usable without account, network access, or the CraftCloud service.

This package is a **proposed future repository structure** consistent with documented patterns from `storytold/figcraft`, `storytold/wordcraft`, `storytold/gridcraft`, `storytold/filmcraft`, `storytold/photocraft`, `storytold/vectorcraft`, `storytold/craft-launcher`, `storytold/craft-libs`, `storytold/craft-ai-integrations`, and `storytold/artcraft-services`. It neither claims maintainers' endorsement nor copies their code. Confirm repository ownership, brand permission and rights to third-party content separately.

## License

Original CraftCloud code is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. This license does not grant permission to use CraftCloud or other parties' names, logos, trademarks, or assets. Third-party components remain under their respective licenses; see [license, trademarks and dependencies](docs/01-repository/license-trademarks-and-dependencies.md).

## Status: not production-ready, local prototype

The repository contains a working Rust server (accounts, organizations, workspaces, files with versions, backups, app sign-in for the Craft apps, FigCraft live hosting), a browser admin UI with a setup wizard, a single appliance Docker image and an installer. It has only been tested locally (Docker Desktop); an acceptance run on a real Linux server is still open, and archive storage stays locked until AC-12c. `/api/v1/info` reports `server_mode: "development-prototype"`. Do not run it as a multi-tenant production service yet.

## Installation on a Linux server

CraftCloud runs as one image `craftcloud:<version>` (server, admin UI and FigCraft server) next to PostgreSQL 16 to 18. There is no registry: a release ships the source archive, a prebuilt image archive (`docker save`, zstd), an SBOM and `SHA256SUMS`, and the installer either loads the image archive or builds the image from source. The first test release is `0.1.0-alpha.1`. Requirements: Debian 13 or Ubuntu 24.04, Docker Engine 25 or newer, Docker Compose 2.24.4 or newer, DNS name pointing to the server, TCP 80 and 443 open.

```sh
sha256sum --check SHA256SUMS
tar -xzf craftcloud-0.1.0-alpha.1.tar.gz
sudo sh craftcloud-0.1.0-alpha.1/deploy/install/install.sh --non-interactive \
  --domain cloud.example.org --acme-email admin@example.org \
  --image-archive ./craftcloud-0.1.0-alpha.1-image-amd64.tar.zst \
  ./craftcloud-0.1.0-alpha.1.tar.gz
```

Then open `https://cloud.example.org/admin`, complete the setup wizard and run `craftcloud-ctl setup-complete`. The default uses a bundled Caddy with automatic certificates; `--proxy nginx|apache|none` keeps an existing web server (templates for certbot setups), and a single `docker run … all-in-one` container is available for tests. Full guide (German), including database choice, backups, upgrade, rollback and e-mail: [docs/06-operations/install.md](docs/06-operations/install.md). Releases are built with `cargo xtask release <version>` from a clean checkout.

## Development

```sh
cargo build --locked -p craftcloud-server   # build the server
CRAFTCLOUD_BIND=127.0.0.1:8080 cargo run -p craftcloud-server   # default bind is 127.0.0.1:8080
cargo xtask ci   # fmt check, clippy -D warnings, tests, admin UI checks (needs Node.js, see below), relative-link check of all Markdown files
cargo xtask ci --skip-web   # same without the admin UI step; the skip is printed, never silent
cargo xtask web   # admin UI only: npm ci, typecheck, build, component tests, translation check
cargo xtask db-test   # PostgreSQL integration tests in a throwaway Docker container (needs Docker; pinned postgres:18.6)
craftcloud-server migrate   # apply pending DB migrations (needs CRAFTCLOUD_DATABASE_URL_FILE); never run implicitly
```

Authentication is cookie-session based (`/api/v1/auth/*`) with CSRF protection and rate limiting; configure `CRAFTCLOUD_PUBLIC_URL` for browser use. Offline password recovery: `craftcloud-server admin issue-password-reset --email <address>`.

Organizations are created by the instance operator; members, roles (owner, admin, member) and access are enforced by a central policy engine (see [permission matrix](docs/05-security/permission-matrix.md)).

First-time setup (operator account and organization) is a one-shot `POST /api/v1/setup/complete` guarded by a bootstrap token file you create yourself; see [docs/06-operations/deployment.md](docs/06-operations/deployment.md). The browser admin UI (below) offers the same flow without `curl`.

**Admin UI (CC-P03-01/02).** `apps/craftcloud-admin` is a React and TypeScript single-page app (Vite, React Aria Components, own SCSS with design tokens, English and German via `src/locales/<lang>.tsv`). It needs Node.js (current LTS); `cargo xtask web` looks for it in the directory named by `CRAFTCLOUD_NODE_DIR`, then on `PATH` (a zip or tarball unpacked anywhere is enough; set the variable to that directory). Dependencies are pinned to exact versions in `package-lock.json` and installed with `npm ci --ignore-scripts`. Build it with `cargo xtask web` (output: `apps/craftcloud-admin/dist`) and let the server serve it on the same origin (the directory is read into memory at startup; rebuild, then restart the server; `CRAFTCLOUD_PUBLIC_URL` must not have a path):

```sh
CRAFTCLOUD_ADMIN_DIR=/abs/path/to/apps/craftcloud-admin/dist cargo run -p craftcloud-server   # UI at http://127.0.0.1:8080/admin/
```

For UI development run `npm run dev` in `apps/craftcloud-admin` (Vite proxies `/api` to `127.0.0.1:8080`); `npm test`, `npm run typecheck` and `npm run i18n:check` run single checks. New UI texts are English sources in code (`t("...")`) and must be added to every `src/locales/*.tsv`; a new language is a new file.

**Docker Compose (CC-P03-07).** `deploy/compose/` runs PostgreSQL, a one-shot migration, the nonroot server (distroless, read-only root filesystem, no capabilities) and a Caddy proxy with automatic TLS. `cargo xtask compose-secrets --domain cloud.example.org` writes the gitignored `deploy/compose/.env` with random secrets, then `docker compose -f compose.yaml -f compose.setup.yaml up -d --build` starts everything and the setup is completed at `https://<domain>/admin`. Steps, hardening, volumes, upgrade and the two-role database setup: [docs/06-operations/deployment.md](docs/06-operations/deployment.md). It is a development prototype, not a released deployment.

Invalid configuration prints every problem and exits with code 2 without binding a socket (runtime failures exit with 1); keys and rules are in [docs/06-operations/deployment.md](docs/06-operations/deployment.md) and [ADR-0011](docs/09-decisions/adr-0011-configuration-policy.md).

The workspace declares `rust-version = "1.99"`, the locally installed toolchain, so every local build exercises the declared MSRV (ADR-0010). Roadmap: [ROADMAP.md](ROADMAP.md) and [docs/07-roadmap/phases.md](docs/07-roadmap/phases.md).

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
│   ├── craftcloud-backup/       # consistent backup, verify and restore (CC-F14)
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

> **Do not treat omitted files as finished implementation.** The tree above is the original proposal; the actual layout is in the repository (for example `deploy/` holds Compose, installer and proxy templates instead of `packaging/`). The release is a local test prototype, and there are no unapproved logos.

## Sequencing

`RFC → threat model and contract → backend/auth/RBAC → storage/versioning → admin interface → single-app integration → realtime adapter → managed/enterprise capabilities`.

## Core decisions pending approval

- Whether project belongs inside `storytold` or starts as independent community RFC.
- Which UI stack: a React/TS web admin is pragmatic for a separate server product, but **not** the Rust-only `craft-launcher` pattern; do not move React into the Launcher.
- Database: PostgreSQL as first production target; SQLite only if separately designed and tested, not a silent promise.
- Namespace: `craftcloud-*` for packages is a recommendation, not upstream policy.

The `docs/01-repository/` folder distinguishes observations from recommendations and unknowns.
