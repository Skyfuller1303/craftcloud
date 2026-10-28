# Entscheidungsliste – vor dem ersten Implementierungs-PR klären

| ID | Frage | Vorläufige Empfehlung | Entscheider / Gate |
|---|---|---|---|
| D01 | Eigene Repo unter `storytold` oder zunächst Community-Repo? | RFC im übergreifenden ArtCraft-Issue; Repo nur mit Maintainer-Akzeptanz | Maintainer, Phase 0 |
| D02 | Name `CraftCloud` und Produkt-Branding frei? | Arbeitstitel; keine Logos/Markenassets | Rechteinhaber, Phase 0 |
| D03 | Lizenz? | Neu entwickelter Code ggf. MIT OR Apache-2.0; schriftlich freigeben | Rechteinhaber, Phase 0 |
| D04 | `craftrules` Standards verfügbar? | Zugriff erfragen; dokumentierte öffentlich sichtbare Regeln vorläufig anwenden | Maintainer, Phase 0 |
| D05 | Hat ein offizieller Hosted-Service Vorrang? | Shared API, konfigurierbare Endpoint-URL, keine Host-Bindung | Produktteam, Phase 0 |
| D06 | Erstintegration? | Kleine Datei-Open/Save-Integration in nur einer App, nicht sofort FigCraft-Live-Collab | App-Maintainer, Phase 1/4 |
| D07 | Was gilt als Artefakt? | Datei ist opaque blob mit `app_id`, `mime`, `version`, `checksum` | Maintainer, Phase 1 |
| D08 | DB? | PostgreSQL als primäre Zielplattform; SQLite erst nach Last-/Migrationsentscheidung | Backend, Phase 1 |
| D09 | Webinterface? | Separate React/TypeScript-Adminoberfläche, nicht im Rust-only Launcher | UI-Maintainer, Phase 0 |
| D10 | Was sind Sharing-Grenzen? | Workspace-/Projekt-RBAC, externe Gastlinks nach MVP | Security, Phase 1 |
| D11 | FigCraft-Server-Verbindung? | Adapter/RFC; keine direkte Session-Token-Weitergabe | FigCraft-Maintainer, Phase 5 |
| D12 | Backup-Format/Supportfenster? | deklarierte API- und DB-Schema-Versionen; Restore-Gate vor Beta | Ops, Phase 2 |
| D13 | Web-/Mobile-Client-Login? | Browser-Cookie; Desktop Device Authorization Flow prüfen; kein Passwort in Launcher speichern | Security, Phase 4 |
| D14 | E2EE? | Kein E2EE-Claim für MVP; separates Kryptodesign, wenn gewünscht | Security/Produkt, spätere Phase |

**Regel:** Keine stillschweigende Festlegung kritischer Entscheidungen. ADR mit Alternativen/Trade-offs vor dem Merge. `docs/09-decisions/` enthält Vorschläge, keine genehmigten Entscheidungen.
