# CC-F01 — Erstinstallation und sicherer Bootstrap

> **Status:** Umsetzung lokal vorhanden (PR-004, Review offen): Token nur per Secret-Datei (ADR-0013), `GET /api/v1/setup/status`, `POST /api/v1/setup/complete`, Singleton-Transaktion, Argon2id; noch ohne Sitzung/Login (PR-005) und ohne CLI-Wiederherstellung. Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P1 (Backend Foundations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Ein Administrator installiert den Dienst als Self-Hosted-Stack. Vor dem ersten Benutzer darf es keine öffentlich erreichbare ungeschützte Setup-Seite geben. Beim initialen Start wird ein kurzlebiger One-Time-Bootstrap-Token über Konsolen-/Container-Logs **nur im sicheren lokalen Operator-Kontext** oder als explizit übergebene Secret-Datei bereitgestellt. Der Token darf nicht unbegrenzt reproduzierbar, nicht in normalen Access-Logs und nach erfolgreichem Setup nicht erneut verwendbar sein.

## Detaillierter Funktionsumfang und technische Umsetzung

`GET /api/v1/setup/status` darf nur generische Information ohne Secrets liefern; `POST /api/v1/setup/complete` prüft Token, Datenbank-Transaktion, Mindestpasswortanforderungen, Organisationsname und erstellt genau einen initialen Instanzadministrator. Danach Setup-Routen sperren. Bei unterbrochener Transaktion keine halb angelegten Admins. Konfiguration: `CRAFTCLOUD_PUBLIC_URL`, `CRAFTCLOUD_TRUSTED_PROXIES`, `CRAFTCLOUD_DATABASE_URL`, `CRAFTCLOUD_STORAGE_ROOT`, `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE` (Namensschema wie `06-operations/deployment.md`); No auto-admin through default credentials.

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

DB noch leer, zweite parallele Bootstrap-Anfrage, abgelaufener/wiederverwendeter Token, Proxy forwarded Host spoofing, Passwort im Prozessargument, verlorener Secret-File-Key, DB-Ausfall, Log-Leak. Bootstrap sollte fail-closed bleiben; Recovery durch dokumentiertes Offline-Operator-Tool, nicht öffentliches Reset-Endpoint.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Fresh install mit einem Operator erfolgreich; zweiter Setup-Versuch 403/409; Crash vor Commit keine halbfertigen Datensätze; kein Secret in Logs; Web-UI zeigt korrekten sicheren Setup-Zustand.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
