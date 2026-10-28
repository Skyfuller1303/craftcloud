# Phase 0 — Arbeitsprotokoll

> Nachweise zu den Tickets aus [phase-00-rfc.md](phase-00-rfc.md). Nur tatsächlich ausgeführte Schritte. Keine externe Kommunikation erfolgt.

## 2026-10-28

### Blueprint-Integrität

- `MANIFEST.json` (84 Einträge, Pfade relativ zu `craftcloud/`): alle Größen und SHA-256 stimmen, keine fehlenden oder zusätzlichen Dateien. Geprüft per Python-Skript vor jeder Änderung.
- Unveränderter Ausgangsstand ist der erste Commit dieses Repositorys.
- Hinweis: Neue Dateien dieses Protokolls (`overlap-map.md`, `phase-00-log.md`) sind nicht im Manifest; das Manifest beschreibt den Ausgangsstand.

### CC-P00-01 — Overlap-Landkarte: Entwurf erstellt

- Ergebnis: [../00-product/overlap-map.md](../00-product/overlap-map.md).
- Befund: Zwei bestehende Vorhaben (CollabCraft, CraftHub) decken große Teile ab; `storytold/crafthub` existiert seit 2026-10-15. Status bleibt offen bis zur Bestätigung durch Maintainer/Autoren.

### CC-P00-03 — craftrules

- `gh api repos/storytold/craftrules` als authentifizierter Nutzer `Skyfuller1303`: `404 Not Found`. Weiterhin nicht verifizierbar. Zugang muss bei Maintainern erfragt werden (Teil von CC-P00-02).

### Konsistenzbefunde im Blueprint (für CC-P00-06/07)

| # | Stelle | Befund |
|---|---|---|
| K1 | `00-product/use-cases-and-personas.md` Persona C | „einer erhält 409“; `sync-state-machine.md`, CC-F08 und P2-Gate verlangen 412; `api-conventions.md` lässt „409/412“ offen. Eine Festlegung fehlt. |
| K2 | `02-architecture/system.md` | Invariante nennt `document_version`; Datenmodell heißt `file_versions`. |
| K3 | `06-operations/deployment.md` | „Rust/Actix service“ — kein ADR wählt ein HTTP-Framework. |
| K4 | `02-architecture/api-conventions.md`, `contracts/openapi.yaml` | Beispiel nutzt `Bearer <device-token>`, obwohl Device-Flow (D13) offen ist; OpenAPI kennt nur `bearerAuth`, kein Cookie-Schema für Browser-Sessions. |
| K5 | `contracts/openapi.yaml` | `/health/live` (PR-001-Umfang) und `/setup/*` (CC-F01) fehlen. |
| K6 | `01-repository/governance-and-ci.md` | Gate-Tabelle: P4 = App-Integration, P5 = Pilot; `phases.md`: P5 = FigCraft/Launcher, P7 = Pilot. |
| K7 | CC-F01 vs. `deployment.md` | Konfigurationsnamen ohne bzw. mit Präfix `CRAFTCLOUD_`. |
| K8 | `05-security/permission-matrix.md` | Rolle „Operator“ beschrieben, aber ohne Spalte; E2E-01 spricht von „instance admin“. |
| K9 | `README.md`, `faq.md` | Verweise auf „ZIP“ — Paket liegt inzwischen als Verzeichnis vor. |

Nutzerentscheidung 2026-10-28: Phase 0 als **lokaler Prototyp ohne öffentliche Kommunikation**; GitHub-Beiträge nur als Entwurf mit Einzelfreigabe.

Behoben am 2026-10-28: K1 (412, ADR-0009), K2, K3 (Framework als offener Punkt in ADR-0009), K4/K5 (`cookieAuth`, `/health/live`, `/setup/status`, `/setup/complete` im OpenAPI-Entwurf; YAML mit PyYAML geparst), K6, K7, K8, K9 (README, FAQ). Nicht durchgeführt: OpenAPI-Lint mit einem Spezifikationsvalidator (kein Tool installiert).

### Nicht ausgeführt / blockiert

- CC-P00-02 (RFC einreichen): auf Nutzerwunsch zurückgestellt. Entwurf zur craftrules-Anfrage: [../10-community/draft-craftrules-access-request.md](../10-community/draft-craftrules-access-request.md), nicht gepostet.
- CC-P00-04, -05, -06, -07: benötigen Rechteinhaber bzw. Maintainer.
