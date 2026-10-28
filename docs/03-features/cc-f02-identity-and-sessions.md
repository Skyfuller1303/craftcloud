# CC-F02 — Identität, Login und Sessions

> **Status:** geplantes Feature-Briefing, nicht implementiert. **Zielphase:** P1 (Backend Foundations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Lokales Benutzerkonto für CraftCloud Web-Admin; unabhängig vom optional späteren ArtCraft-Hosted-Account. E-Mail normalisieren, Passwort-Hashes mit Argon2id und wartbaren Parametern, Login-Versuch pro Account/IP begrenzen. Account-Status active/disabled, Passwortwechsel, Logout an allen Geräten, Session-Timeout und eindeutige Revokation.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST /auth/login`, `POST /auth/logout`, `GET /auth/session`, `POST /auth/password/change`, `GET /auth/sessions`, `DELETE /auth/sessions/{id}` (proposed). Web: HttpOnly+Secure+SameSite Cookies, CSRF auf mutierenden Requests; nicht still JWT in localStorage. Desktop-Anbindung später über Device Flow oder system-browser; keine Wiederverwendung des User-Passworts in Tool-Konfiguration.

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

Timing-oracle/Account-Enumeration, Rate-Limit-Bypass, Session Fixation, gestohlene Cookies, paralleler Logout, Session-Verlängerung nach Passwortänderung, Crypto-Parameter-Migration, CSRF per Bild/Form, Account-Disable. SSO ist spätere Funktion, nicht heimlicher Startzwang.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Falsches Passwort gibt generische Antwort; Logout invalidiert Cookie, Passwortreset/Disable invalidiert aktive Sessions; Sessiontoken steht nur gehasht serverseitig; CSRF scheitert zuverlässig; kein Token im Serverlog.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
