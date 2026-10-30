# CC-F08 — Immutable Versionen, CAS und Konflikte

> **Status:** Versionen, CAS-Commit und Download lokal umgesetzt (PR-011, Review offen): unveränderliche Versionen (Nummer je Datei, Blob, Größe, SHA-256, Basis), `POST /files/{id}/versions` mit `If-None-Match: *` bzw. `If-Match: "version:<uuid>"` (428/412 `VERSION_CONFLICT` mit `details.current_version_id`), idempotenter Commit über die Upload-ID, Wiederherstellen alter Versionen als neue Version ohne neue Bytes, Versionslisten, Download des Kopfes oder einer Version mit Rechteprüfung, Einzel-Range, `Repr-Digest` und Integritätsprüfung (defekte Blobs werden markiert). Nicht umgesetzt: Aufbewahrung/Löschen alter Versionen und Blobs (CC-P02-06), periodische Integritätsprüfung (CC-P02-07), Mehrfach-Ranges, fortsetzbare Uploads. Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P2 (Files & Versioning). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

> **AC-03 Ergänzung:** Versionsabschluss und Restore prüfen eine aktive Dateilease erneut unter dem Organisations-Lock. Eine fremde Session-Lease antwortet mit 423 `FILE_LOCKED`; Lesen und Download bleiben erlaubt. Datei- und Projekt-Papierkorb/Restore sind ebenfalls leasegeschützt. Lease-Identität ist an die authentifizierte Sitzung gebunden; `client_id` ist nur Anzeige-/Clientmetadatum. Siehe [ADR-0020](../09-decisions/adr-0020-grants-and-edit-locks.md).

## Problem und Nutzen

Jeder erfolgreich gespeicherte Cloud-Stand ist immutable. Der `files.head_version_id` wird nur mit CAS/optimistic concurrency aktualisiert. Ein Editor kann alte Revisionen sehen, markieren oder unter bewusstem neuen Stand wiederherstellen; alte Versionen werden nicht überschrieben. Basisversion für neuen Commit explizit. Konfliktseite beschreibt lokale und remote Version, nie automatischer Überschreibungszwang.

## Detaillierter Funktionsumfang und technische Umsetzung

`GET /files/{id}/versions`, `POST /files/{id}/versions`, `POST /files/{id}/restore` (erzeugt neue Version mit Referenz auf alten Blob). `If-Match: "version:UUID"`, Commit API referenziert verified `upload_id`. Idempotency-Key und DB-Transaktion um Head und Version. Konflikt liefert 412 `VERSION_CONFLICT` mit aktueller Version; sichere UI-Optionen kopieren/neu laden/vergleichend öffnen.

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

Zwei Writer gleiche Basisversion, Commit Retry nach 504, Restore während Dateilöschung, Rechteentzug zwischen Upload und Commit, Quota nach Upload überschritten, Version aus anderem Tenant, unvollständige Blob-GC.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Parallele Commits führen genau zu einem erfolgreichen Head-Wechsel; der andere erhält 412, behält seine lokale Kopie und kann als neue Datei speichern; Restore legt neue Revision an; Audit zeigt Autor und Version.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
