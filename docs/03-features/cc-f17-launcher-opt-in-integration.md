# CC-F17 — Optionaler Craft Launcher Einstieg

> **Status:** geplantes Feature-Briefing, nicht implementiert. **Zielphase:** P5 (Collaboration / Launcher). **Priorität:** später / nur nach Gate.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

> **Neuplanung 2026-10-30:** Konkrete Umsetzung in [docs/12-app-cloud](../12-app-cloud/README.md) (AC-07; der Launcher darf jetzt ein eigenes Konto mit Anmeldung per E-Mail und Passwort halten, gibt aber keine Tokens an Apps weiter).

## Problem und Nutzen

Launcher bleibt App-Übersicht zuerst, ohne Login-Schranke. Später kann eine **optionale** Verbindungsverwaltung in Einstellungen integriert werden: CraftCloud-Server auswählen, Status prüfen und ggf. zur Browser-Anmeldung führen. Keine Veränderung des Install-/Update-Pfads, kein ungeprüfter Remote-Build-Download, kein React/TS innerhalb des Launchers.

## Detaillierter Funktionsumfang und technische Umsetzung

Separate RFC/PR in `storytold/craft-launcher`; Rust egui/eframe only, bestehende UI tokens und `AGENTS.md` beachten. Verbindung als config alias, server URL, status und expliziter button. Kein autonomes Provisioning ohne Admin-Einwilligung. Besser initial auf SDK in den Apps fokussieren; Launcher ist später Convenience, nicht Auth Authority.

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

Server nicht erreichbar beim App-Start, Identity-token vom App-Kontext geklaut, externe URL manipuliert, Launcher Update-Mirror verwechselt mit Cloud-Server, OAuth-Browser-Fenster geschlossen.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Ohne eingerichtete Cloud identisches Start-/Update-Verhalten; alle Cloudfeatures opt-in; kein externer Downloadpfad durch Cloud-Konfiguration; Integration passiert separat unter Launcher-Review.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
