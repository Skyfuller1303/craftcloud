# Wie ein erster echter Implementierungs-PR aussehen sollte

## Vor dem Branch

- P0 RFC akzeptiert oder klar als unabhängiger Community-Prototyp markiert.
- Lizenz-, Brand- und Host-Governance nicht nur implizit.
- `craftrules` geprüft oder dokumentiert als unzugänglich.
- `CC-P01-01` Ticket eröffnet, CI-Anforderungen/Owner benannt.

## Scope von PR-001

Kleines Rust-Workspace-Skeleton, `craftcloud-core` mit einer reinen typed Capability-Struktur, `craftcloud-server` nur mit `/health/live` und `/api/v1/info` und fake/default capabilities **mit eindeutiger developer Kennzeichnung**, `xtask` CLI-Hülle, CI Workflow und minimale Tests. Noch kein Passwort-/Dateiupload-Code ohne Security-Spezifikation.

## Qualitätsanforderungen

- Linux/macOS/Windows-Rust-Build-Strategy dokumentiert; kein behaupteter wasm build für Server.
- `cargo fmt`, clippy, tests, docs checker im echten CI vorhanden; kein Stub, der immer 0 ausgibt.
- `#![forbid(unsafe_code)]`, Deny-Panic-Lints wie im Craft-Ökosystem.
- `Cargo.lock` versioniert, Dependency-Lizenzen geprüft.
- README mit „not production-ready“ und Links auf docs/roadmap.
- Kein Binär-/Logo-/Fontasset ohne Rechte und Attribution.

## PR Template Check

Motivation + Scope + Endpoints + Tests actually run + Security notes + DB migrations (none) + Compatibility (none yet) + Rollback + Screenshots if UI. Maintainer-Reviewer bestätigen: nicht unbemerkt bestehende Craft-Apps verändert.
