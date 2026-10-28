# CC-F07 — Streaming Upload, Download und Integrität

> **Status:** geplantes Feature-Briefing, nicht implementiert. **Zielphase:** P2 (Files & Versioning). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Dateitransfer ohne komplette Dateien im RAM. Client initialisiert Upload mit erwarteter Größe, Sha256 und gewünschter Zielressource. Server reserviert Quota und schreibt in isoliertes temporäres Verzeichnis, streaming mit Limits; Abschluss prüft Hash/Größe und markiert Blob atomar ready. Download nach AuthZ streamt nur unveränderliche Versionen. Keine direkte Exposition von S3-Credentials.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST /uploads`, `PUT /uploads/{id}/content` (resumable chunk alternative später), `POST /uploads/{id}/finalize`, `GET /files/{id}/versions/{id}/content`. `Content-Length` optional, reale Bytezahl hart limitieren; Request-Timeout und Cancellation; `Content-Disposition` safe encoded; kein Mime trust; checksum integrity. Bei mehreren Uploads Idempotency-Key, expiration, cleanup.

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

Abbruch bei 99%, Client sendet mehr als erwartet, Chunk doppelt, Hash mismatch, voller Datenträger, Symlink-Race, Prozessabsturz zwischen write/fsync/rename/DB, falsche Last-Modified, Token revocation mitten im Stream, S3-Error, concurrent quota reservations.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Streaming ohne unbounded RAM; teilfertige Daten nie als Datei-Head sichtbar; falscher Hash ist Fehler und Quota wird freigegeben; zwei parallele Streams verlieren keine Bytes und überschreiten keine Quota.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
