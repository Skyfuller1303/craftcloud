# Governance, Build- und CI-Strategie

## Repository governance (Vorschlag)

Branches: `main` protected; topic branches `feat/cc-fNN-...`, `fix/...`, `docs/...`. Jede fachliche Änderung als PR mit verlinktem Ticket, Risiko, Testnachweis, DB-Migration, Client-Kompatibilität, Screenshots bei UI und Rollback. Conventional Commit-Präfixe analog `craft-ai-integrations`. Merge nach Maintainer-Freigabe; Sicherheitsrelevantes mit zusätzlichem Review.

## Minimale Workflows bei tatsächlicher Implementierung

1. **Docs**: relative Links, Markdown-Lint, geänderte Contracts/ADRs miteinander verlinken, keine unstimmigen Versionsstände.
2. **Rust**: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace` und gezielte Tests.
3. **Safety**: `cargo deny`/Lizenzprüfung nach Wartungsentscheidung, Secret Scanner, Dependency Advisories, SBOM, Container vulnerability scan; Ergebnisse nicht ignorieren.
4. **DB**: Integrationstests gegen echte PostgreSQL-Testinstanz, Migration Up/Upgrade auf Snapshot, Downgrade nicht einfach voraussetzen; Backup-/Restore-Test.
5. **API**: OpenAPI-Syntax und Breaking-Change-Diff; negative Auth-Tests, Contract-Client und Server aus gleichem Spec-Commit.
6. **Frontend**: `npm ci` (oder vereinbarter Package Manager), Typecheck, Lint, Unit/UI-Tests, Accessibility-Checks, Bundle-Limits.
7. **Deployment**: nichtprivilegierter Container, read-only FS soweit möglich, persistent storage explizit, health/startup probes, SIGTERM/graceful shutdown, image digest/signature policy.
8. **Release**: verifizierbare Artefakte, Checksums, semver, Changelog, Datenmigrationen, upgrade/downgrade/restore notes.

## Qualitäts-Gates

| Phase | Gate | Evidenz |
|---|---|---|
| P0 | Maintainer-/Lizenzentscheidung, API- und Threat-Review | RFC, ADR, Eigentümer |
| P1 | Auth + Tenant-Queries + Fehler-Tests laufen | CI-Log, Testbericht |
| P2 | Chunk-/Streaming-Upload + Versioning + Restore | E2E & Storage-Fault Test |
| P3 | Admin-Flows, ARIA/A11y, CSRF/role Tests | Screenshots + CI |
| P4 | Real app integration with opt-in/offline/error flow | App-Maintainer-Test |
| P5 | FigCraft-Adapter/Launcher ohne Regression bestehender Links | Maintainer-Abnahme beider Repos |
| P6 | OIDC/S3 bestehen identische Integritäts-/Security-Tests | CI-Log, Testbericht |
| P7 | Pilot mit gesichertem Restore | Operative Checkliste |

## Unterschiede der Upstream-Workflows

FigCraft verweist auf `cargo xtask ci`, andere Repos haben eigene `CONTRIBUTING.md`-Regeln. Für CraftCloud darf kein fiktiver Upstream-Workflow als zwingend ausgegeben werden. `cargo xtask ci` ist ein sinnvoller Zielbefehl **erst, wenn dafür echter Code existiert**.
