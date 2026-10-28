# GitHub RFC draft — copy into an official issue only after review

**Title:** `[RFC] CraftCloud: optional shared, self-hosted project and team infrastructure for the Craft App Suite`

## Context

There are already several related pieces and proposals across the ecosystem:

- [`artcraft-services`](https://github.com/storytold/artcraft-services): hosted backend with user accounts, media management, object storage and AI-generation/billing features; it is not currently a dedicated multi-app self-hosted team cloud.
- [`figcraft-server`](https://github.com/storytold/figcraft/blob/main/docs/server.md): standalone persistent WebSocket collaboration server with revocable links and team libraries; user accounts and individual permissions are not implemented yet.
- [`craft-launcher`](https://github.com/storytold/craft-launcher): provides a central entry point for the apps, without mandatory cloud login.
- Related requests: [WordCraft #105 (CraftCloud)](https://github.com/storytold/wordcraft/issues/105), [Launcher #131 (Nextcloud)](https://github.com/storytold/craft-launcher/issues/131), [ArtCraft #2139 (DriveCraft)](https://github.com/storytold/artcraft/issues/2139).

## Proposal

Would the maintainers be open to a dedicated `storytold/craftcloud` repository (or another preferred name) as an **optional, local-first, self-hostable platform** providing:

1. Organization, workspace, user, invitation and role management.
2. Project/file storage and immutable version history with explicit conflict handling.
3. A small browser administration UI and easy Docker deployment with local storage (S3 later).
4. A versioned API and Rust client SDK that Craft apps can adopt individually.
5. Later: collaboration adapters, starting with existing FigCraft server capabilities **rather than replacing them**.

A user must still be able to use every Craft app locally without signing in or connecting to a server. A hosted managed service could use the same API in the future, but this proposal does not require it.

## Why a separate repo?

`artcraft-services` includes generation pipelines, credits and billing and carries a distinct WIP fair-source license. A smaller independently deployable service would have clearer boundaries and avoid coupling the entire Craft suite to the ArtCraft AI backend. We would not reuse restricted source without permission.

## Suggested implementation stages

- P0: architecture RFC, license/ownership, API contract, security model.
- P1: Rust backend, safe bootstrap, accounts, organizations, workspaces, RBAC.
- P2: projects, streaming storage, immutable revisions and conflict detection.
- P3: admin UI, packaging, monitoring, verified backup/restore.
- P4: opt-in integration with one Craft application.
- P5+: FigCraft collaboration adapter, optional Launcher connection settings, SSO/S3.

I have drafted a detailed documentation-first blueprint with per-feature briefs, explicit security/acceptance criteria, and migration/testing plans. It is intended as a proposal, not a claim that this already exists.

## Questions

1. Is there an existing internal plan or backend ownership decision for this kind of cross-app infrastructure?
2. Should a new repo own this functionality, or should it be aligned with `craft-libs`, FigCraft or `artcraft-services`?
3. Is `CraftCloud` an acceptable working name and what license/trademark policy should contributors follow?
4. What shared architecture/CI conventions (`craftrules`) should the proposal adopt?
5. Would maintainers be interested in reviewing an initial backend/API-only prototype after agreeing on the contract?
