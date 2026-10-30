# CC-F05 — RBAC und Objektberechtigungen

> **Status:** Policy-Engine und Org-Rollen lokal umgesetzt (PR-006, Review offen): reine Funktion `can(actor, action, resource)`, Actor pro Request frisch aus der Datenbank, Tabellentests, Scope nur aus `Allow`-Permit. Letzter-Owner-Regel zählt nur aktivierte Owner (Break-glass-CLI `admin set-org-owner`). Workspace-Aktionen sind in der Policy hinterlegt, ihre Routen kommen mit PR-007; Audit-Speicher folgt mit CC-F11 (bis dahin strukturierte `audit`-Logereignisse). Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P1 (Backend Foundations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

> **Neuplanung 2026-10-30:** Konkrete Umsetzung in [docs/12-app-cloud](../12-app-cloud/README.md) (AC-03 Freigaben an Personen, Gastrolle und Bearbeitungssperre).
> **AC-03 Backend-Erweiterung:** Workspace-Rollen werden mit tenantgebundenen Projekt-/Datei-Grants kombiniert. `guest` erhält durch Einladung keine Workspace-Mitgliedschaft. Grant-Management und Mitgliedersuche sind Workspace-Editoren und höher vorbehalten; Rechte bleiben serverseitig in `can()` und werden nicht aus UI-Daten hergeleitet. Details und Scope-Matrix siehe [ADR-0020](../09-decisions/adr-0020-grants-and-edit-locks.md) und [Berechtigungsmatrix](../05-security/permission-matrix.md). Implementierung und Sicherheitsintegrationstests laufen im AC-03 Worktree.

## Problem und Nutzen

Zentrales Berechtigungsmodell mit Instanzadmin als Operator, Org-Owner, Org-Admin, Workspace-Admin, Editor, Viewer. Rollen sind **Scopes**, nicht globale Strings, und werden innerhalb der Autorisierungsservice-Schicht geprüft. Kein freies Merge von mehreren Rollen ohne definierten Vorrang. Alle Daten-APIs filtern schon in DB-Anfragen nach Tenant und erlaubtem Projekt.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST /workspaces/{id}/members`, `PATCH .../members/{user_id}`, `DELETE ...`; intern Policy `can(actor, action, resource, context)` mit Policy-Entscheidung und Audit. Aktionstabelle in `docs/05-security/permission-matrix.md`. Öffentliche, unauthentifizierte Share-Links erst nach eigens geprüftem Policy-Modell.

## User Journey und gewünschtes Verhalten

1. Client öffnet die zugehörige Oberfläche oder ruft die versionierte API auf; authentifiziert sich nur, wenn Cloudzugriff gewünscht ist.
2. API validiert Pflichtfelder, Request-Größe, Resource Scope und Actor-Berechtigung; unautorisierte Zugriffe müssen **vor** kostenintensiven Storage-/DB-Aktionen stoppen.
3. Die Domänenschicht führt die Operation mit eindeutiger Ownership und klar dokumentiertem Statusübergang aus.
4. Client erhält eine stabile Response mit maschinenlesbarem Fehlercode und Request-ID; Retry ist nur bei nachweislich idempotenter Aktion automatisch zulässig.
5. Negative Fälle werden als Fehler **mit Recovery-Pfad** behandelt; lokale Craft-Dateien bleiben erhalten.

## Daten- und Rechtevertrag

- **Objekt-Scope:** jedes betroffene Objekt erhält `org_id`/Workspace-Kontext; für User- oder Serverobjekte wird Instanzscope explizit geprüft.
- **Zulässige Rollen:** durch serverseitiges Policy-Modul und `permission-matrix.md` festzulegen/umzusetzen; UI-Sichtbarkeit ist kein Schutz.
- **Audit:** sensitive Schreib-/Berechtigungsaktionen mit Actor, Ziel, Resultat und Request-ID; niemals Passwort, Invite-Token oder Dateibytes loggen.
- **Transaktionen:** Metadatenänderungen konsistent; externe Storage-Operationen benötigen ein abgestimmtes Recovery-/Compensation-Design.
- **Versionierung:** API-Kontrakt ändern nur mit kompatibler Migration und Client-Upgrade-Matrix.

## Risiken, Grenzfälle und Fehlerbehandlung

IDOR durch vertauschte UUID, Admin eines Workspaces möchte anderen verwalten, Member removed während Stream, Rechteänderung nach Upload vor Commit, unzulässiger Project-Transfer, untrusted `role=owner`, stale cached permissions.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Negative Cross-Tenant-Tests für jede Objektklasse; Viewer darf keine Upload-Session erzeugen; ein entzogener Editor kann keine offene Upload-Session committen; Rolle/Scope wird nur serverseitig entschieden.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
