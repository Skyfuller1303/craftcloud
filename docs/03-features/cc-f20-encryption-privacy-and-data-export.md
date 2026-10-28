# CC-F20 — Datenschutz, Verschlüsselung, Export

> **Status:** geplantes Feature-Briefing, nicht implementiert. **Zielphase:** P6 (Enterprise / extensions). **Priorität:** später / nur nach Gate.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Transportverschlüsselung über TLS ist für produktive Cloud-Clients verpflichtend; ruhende Daten können durch Disk/S3 serverseitig verschlüsselt werden, wobei der Betreiber weiter auf Daten zugreifen kann. Zero-Knowledge/E2EE **nicht** behaupten, solange kein klientenseitiger Schlüssellebenszyklus, Multiuser-Keysharing, Recovery und Serverblindheit implementiert ist. Privacy-Export und Löschung müssen in Betriebersicht dokumentiert sein.

## Detaillierter Funktionsumfang und technische Umsetzung

Privacy-Export: versionierte Dateimanifeste+Blobs+Metadaten (nach Zugriffskontrolle); Data deletion: Soft-delete, retention, hard-delete und Backup-Verfallszeit. Key rotation und backup secrets runbook; Tenant encryption keys nur bei definierter Ownership/Recovery. Bei E2EE Neubewertung von Web-Vorschau, Suche, Server-Collab und Legal Hold.

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

Key loss, Mitarbeiteraustritt, e2ee collaboration conflict, forgotten user with shared files, backup retains data after erasure, S3 provider with different jurisdiction, read-only export too broad, malware uploads.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Dokumentierte Datenflüsse, klare Betreiber-/Anbieter-Rollen, exportierbare Daten je Berechtigung, kein irreführendes Ende-zu-Ende-Label und prüfbarer Delete/Backup-Zeitplan.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
