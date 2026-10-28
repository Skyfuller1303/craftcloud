# CraftCloud — proposed instructions for coding agents

Read `README.md`, `docs/README.md`, `docs/01-repository/upstream-comparison.md`, `docs/00-product/open-questions.md`, `docs/07-roadmap/phases.md`, and the selected feature briefing before coding.

## Non-negotiables (proposed and based on visible Craft conventions)

1. **Local-first**: a Craft app always works without a CraftCloud login. Network errors cannot corrupt or prevent opening local files.
2. **No unsupported ownership or licensing claims**: this repository blueprint is a proposal. Do not copy code from restricted `artcraft-services`. MIT OR Apache-2.0 requires maintainer agreement; never invent permission for names/logos.
3. **Never crash on bad input**: no panicking code paths in production, no `unsafe`, bounded parsing, checked arithmetic, no secrets in logs. Tests must cover each error and abuse case.
4. **Data boundaries**: client -> transport -> authentication -> authorization -> application domain -> persistence/storage. Never let the web UI decide access rights; every request and background job rechecks tenant/object scope.
5. **Security by default**: no unauthenticated public admin; deny-by-default; protect bootstrap; TLS at trusted proxy; CSRF/session cookie policy; hash credentials/tokens using appropriate modern schemes; protect SSRF/path traversal and tenant isolation.
6. **Contract first**: update API contract, migrations, tests, examples and docs before claiming an endpoint is stable. Meaningful backward-compatibility checks.
7. **Small reviewed changes**: single feature slice with test and docs; preserve a concise roadmap/gap list. Never claim a command passed without actually running it.
8. **UI asset provenance**: do not import external Craft logos, fonts, proprietary icons or images without explicit license and permission; attribute every distributed asset.
9. **Upstream integration boundaries**: never modify `storytold/figcraft` or `storytold/craft-launcher` from this repo by assumption; cross-repo work gets separate maintainers' reviews.
10. **No invented upstream standards**: `storytold/craftrules` referenced publicly but inaccessible at review date. Request access before claiming full compliance.

## Workflow for future implementers

Orient → write decision or failing test → implement one vertical slice → verify unit/integration/security tests → update docs/roadmap → PR review → merge under repo policy.

For feature work, include an ID in issue/PR, e.g. `CC-F08`, and record the corresponding acceptance criteria from `docs/03-features/`.

## Architecture layering

- `core`: types and validation; no HTTP/DB or UI concerns; keep wasm-safe if used by browser/Rust Craft apps.
- `api`: HTTP handlers / request validation / centralized middleware only.
- `auth`: session lifecycle, identity, policy evaluation.
- `db`: tenant-scoped repositories and migrations; no UI.
- `storage`: safe storage adapters; no raw client-controlled paths.
- `client`: typed DTO and retry semantics; never expose tokens in logs.
- `apps`: composition root; frontend has no trusted policy authority.

`AGENTS.md` is **CraftCloud-specific proposed policy**. It is not represented as an authoritative copy of upstream `AGENTS.md`.
