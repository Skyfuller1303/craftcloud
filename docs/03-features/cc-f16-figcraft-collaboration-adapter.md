# CC-F16 — FigCraft Collaboration Adapter

> **Status:** AC-06 implementiert die lokale Begleitdienst-Integration nach bestandenem Spike-Gate; native Konten/Capabilities und Maintainer-Abstimmung bleiben offen. **Zielphase:** P5 (Collaboration / Launcher).
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

> **Neuplanung 2026-10-30:** Konkrete Umsetzung in [docs/12-app-cloud](../12-app-cloud/README.md) (AC-06).

Der aktuelle Umfang ist in [ADR-0022](../09-decisions/adr-0022-figcraft-live-hosting.md) und [FigCraft-Adapter](../04-integrations/figcraft-adapter.md) beschrieben: persönlicher Link, Edit-/View-Rechte, dauerhafter Widerrufsauftrag, native Versionssicherung und optionales Compose-Hosting. Die folgenden Capability-/RFC-Ziele bleiben der spätere gemeinsame Entwurf; sie sind keine bereits vorhandene Authentifizierung im FigCraft-Protokoll.

## Problem und Nutzen

`figcraft-server` ist ein eigenständiger live-edit WebSocket-Server mit persistenten Dateien, Revocable Share Links und Token/Owner-Key-Modell. CraftCloud soll dessen Design nicht neu erfinden und vor allem nicht vorgeben, bereits Nutzerberechtigungen im FigCraft-Protokoll zu besitzen. Adapter verbindet langfristig Workspace-ACL und FigCraft-Session-Lifecycle nach expliziter Zustimmung der FigCraft-Maintainer.

## Detaillierter Funktionsumfang und technische Umsetzung

RFC für Capability Mapping: `CraftCloud user/project/file` -> `FigCraft file/session ID`, kurzlebige signierte Scope-Capability für Viewer/Editor, Revocation-Event und Reconnect-Handling. Integrationsstrategie wählen: FigCraft authentifiziert external capability nativ, oder vertrauenswürdiger Gateway, nicht blind token forwarding. Interop-Tests und Protocol-Version-Handshake mit `collab-protocol.md` und `server.md`. Bestehende Share-Links müssen sauber weiter funktionieren.

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

User verliert Rolle während WebSocket Session, Viewer sendet Edit, Session server reboot, alter Client, link takeover, Reconnect mit Offline-Edits, unterschiedliche Head/Version Semantik, audit actor attribution, gemeinsames Library-System.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Proof zeigt: Editor darf bearbeiten, Viewer nicht, Revokation disconnects oder sperrt spätestens bei definierter Latenz, bestehende Self-Hosted-Lösung bleibt unverändert nutzbar; kein ungeprüfter Zugriff auf FigCraft-Dataverzeichnis.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
