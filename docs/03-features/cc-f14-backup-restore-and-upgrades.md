# CC-F14 — Backup, Restore und Upgrade-Sicherheit

> **Status:** lokal umgesetzt (PR-019, CC-P03-08), noch nicht reviewt und nicht gemerged: `craftcloud-server admin backup create | verify | restore` (Crate `craftcloud-backup`), Betreiber-Runbook in [docs/06-operations/backup-upgrade-runbook.md](../06-operations/backup-upgrade-runbook.md), Restore-Drill im Test `crates/craftcloud-api/tests/backup.rs`. Offen: Verschlüsselung, Signatur, inkrementelle Backups, S3, Messung mit großen Beständen und auf einem Linux-Host (siehe `docs/gaps.md`). **Zielphase:** P3 (Admin & Operations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Ein Betreiber muss aus einer konsistenten Kombination aus Datenbank-Snapshot und Blob-Inventar wiederherstellen können. Dateien sind ohne DB-Metadaten unbrauchbar, umgekehrt sind DB-Referenzen ohne Blobs inkonsistent. Backup erzeugt manifest mit version/time/checksums, verhindert unkontrolliertes GC während Snapshot und dokumentiert Recovery-Point. Keine bloße SQL-Dump-Anleitung als fertig zertifiziert darstellen.

## Detaillierter Funktionsumfang und technische Umsetzung

`craftcloud admin backup verify` / `restore` als spätere CLI, oder externe orchestrierte Procedure. PostgreSQL consistent snapshot + Storage-Manifest, Checksum-Kontrolle, Versionen und Migrationen. Vor Upgrade: Backup mit Restoreprobe; nach Upgrade: Gesundheits- und Datenintegritätsprüfung. Security: Backup encrypted at rest at operator discretion; Secret-Key escrow dokumentieren.

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

Backup während Upload, Version-Commit, S3 GC, Container-Stopp, beschädigte Manifestdatei, alte Schema-Version, verlorener Schlüssel, unvollständiger Snapshot, Test-Restore mit fremder PUBLIC_URL.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Der Restore-Drill stellt zwei Benutzer, zwei Workspaces, Versionshistorie und referenzierte Bytes wieder her; inkonsistente Manifeste werden abgelehnt; Runbook dokumentiert tatsächlich gemessene RPO/RTO.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
