# CC-F10 — Quotas, Retention, Lösch- und Restore-Politik

> **Status:** Aufbewahrung, endgültiges Löschen und Garbage Collection lokal umgesetzt (PR-012, Review offen): Ein Wartungsjob (Schleife im Server alle `CRAFTCLOUD_MAINTENANCE_INTERVAL_SECS`, dazu `craftcloud-server admin maintenance --once [--dry-run]`; ein Lauf zur Zeit über einen PostgreSQL-Advisory-Lock) löscht Dateien und Projekte nach Ablauf von `CRAFTCLOUD_TRASH_RETENTION_DAYS` endgültig (Versionen vor Datei, Dateien vor Projekt), löscht überholte Nicht-Kopf-Versionen nach `CRAFTCLOUD_VERSION_RETENTION_DAYS` (ADR-0014; der Kopf nie; Verweise anderer Versionen über `base_version_id`/`restored_from_version_id` werden auf NULL gesetzt) und räumt Blobs in zwei Phasen ab: Phase 1 sperrt unter der Organisationssperre, prüft erneut, dass keine Version und keine offene Upload-Sitzung den Blob referenziert, setzt den neuen Status `deleting` und bucht `committed_bytes` genau einmal ab (nie unter 0); Phase 2 löscht das Objekt und danach die Zeile; ein Absturz dazwischen wird vom nächsten Lauf beendet. Hinzu kommen die Abstimmung (Objekte ohne Blob-Zeile und `.part`-Dateien älter als Upload-TTL + 1 h), die Integritätsprüfung (`CRAFTCLOUD_VERIFY_BYTES_PER_RUN`, `corrupt` wird bei erneut passender Prüfsumme wieder `committed`), die Korrektur abweichender Quota-Zähler und `GET /api/v1/admin/maintenance` (nur Operator). Nicht umgesetzt: Lease-Tokens für laufende Downloads (siehe `docs/gaps.md`: auf unix bleibt ein offener Download nach `unlink` lesbar, auf Windows scheitert das Löschen und wird im nächsten Lauf wiederholt), Löschen von Upload-Sitzungszeilen außerhalb von Datei-/Versions-Löschung, Backup-Bereinigung. Davor: Quota-Buchhaltung lokal umgesetzt (PR-010, Review offen): Quota je Organisation (Operator) und Workspace (Organisations-Owner/Admin), NULL = unbegrenzt, Reservierung vor jedem Upload unter der Organisationssperre, Zähler `reserved_bytes`/`committed_bytes` mit Neuberechnung aus den Zeilen, `GET /organizations/{id}/usage`. Soft-Delete und Aufbewahrung von Projekten und Dateien stammen aus PR-008. (Der dort genannte Rest ist mit PR-012 umgesetzt.) Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P2 (Files & Versioning). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Organisationen erhalten Speicherquoten, später Workspace- und Projektlimits. Quota wird mit **reserved + committed** berechnet, damit parallele Uploads keine Überschreitung verursachen. Soft-Delete erzeugt Zeitfenster zur Wiederherstellung, danach referenzzählende GC. Aufbewahrung/Backup und Kundendaten-Löschpflichten müssen widerspruchsfrei dokumentiert werden.

## Detaillierter Funktionsumfang und technische Umsetzung

`GET /organizations/{id}/usage`, `PATCH /workspaces/{id}/quota`, `DELETE /files/{id}`, `POST /files/{id}/restore`, serverseitige Job-Routine GC. Quota-Änderung darf bestehende Dateien nicht automatisch löschen, sondern nur weitere Writes blockieren. Delete-Metadaten und Audit getrennt von Blob Retention.

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

2 große Uploads parallel, Soft-Delete während Versionsrestore, Retention kleiner als Backup-Intervall, Orphan GC mit aktiver Lease, negative quota parameter, Betreiber ändert Storage Backend, Quota-Abuse durch abgebrochene Uploads.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Reservierte und committed bytes stimmen nach Crash/Expiry; Usage kann reproduzierbar neu berechnet werden; Restore innerhalb Frist möglich; nach Hard-Delete sind in aktiven Backups ggf. weiterhin Kopien und dies ist in Privacy Policy ersichtlich.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**

## Organisationsplätze (AC-11)

Plätze sind ein eigenes Organisationslimit und zählen aktive Mitglieder außer Gästen. Offene Nichtgast-Einladungen reservieren Plätze. Operatoren können ein Limit ändern oder auf unbegrenzt setzen. Eine Senkung unter die aktuelle Nutzung lässt bestehende Mitglieder aktiv und blockiert neue Zulassungen, bis wieder Kapazität besteht. Das Platzlimit verändert weder Storage-Quota noch Aufbewahrung.
