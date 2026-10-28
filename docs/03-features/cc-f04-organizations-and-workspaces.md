# CC-F04 — Organisationen und Workspaces

> **Status:** Organisationen und Mitgliedschaften lokal umgesetzt (PR-006, Review offen): Anlegen nur durch den Operator, Lesen/Umbenennen mit ETag, Mitglieder verwalten, Konten (de)aktivieren, Pagination. Workspace-Routen (`POST/GET /organizations/{id}/workspaces`, `PATCH /workspaces/{id}`) und Löschen/Soft-Delete von Organisationen sind offen (PR-007, Lücke dokumentiert). Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P1 (Backend Foundations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Organization ist Eigentums- und Mandantengrenze, Workspace ist Team-/Projektcontainer innerhalb einer Organization. Ein User kann Mitglied mehrerer Orgs sein, ohne dass Ressourcen quer eingesehen werden. Workspace-Slug nur innerhalb einer Org eindeutig; interne IDs UUID und unveränderlich. Beim Anlegen optional Quota und Beschreibungsmetadaten.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST/GET /organizations`, `GET/PATCH /organizations/{id}`, `POST/GET /organizations/{id}/workspaces`, `PATCH /workspaces/{id}`. Die API ermittelt den org scope aus autorisiertem Objektzugriff, nicht nur aus einem frei manipulierbaren Query-Parameter. Löschoperationen sind zuerst Soft-Delete mit bestätigtem Export-/Backup-Pfad.

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

Identische Slugs in unterschiedlichen Orgs, verschobene Benutzerrechte, Mitarbeiter mit Zugriff auf Org A/B, Rename während Upload, Workspace löschen mit aktiven Uploads, Org-Owner verlässt Organisation, letzter Owner versucht sich selbst zu löschen.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Anfrage auf beliebigen Workspace einer anderen Org gibt kein verwertbares Datenleck; Org/Workspace-Konsistenz durch DB-FKs geschützt; letzter Owner kann nicht versehentlich verschwinden.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
