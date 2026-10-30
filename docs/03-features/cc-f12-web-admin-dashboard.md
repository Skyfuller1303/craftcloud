# CC-F12 — Browser-Adminpanel und Setup-UX

> **Status:** teilweise umgesetzt (lokal, ungeprüft): Gerüst, API-Client, Setup, Login, Einladung annehmen sowie Organisations-/Workspace-Auswahl (CC-P03-01/02, PR-014); Mitglieder, Einladungen, Projekte, Dateien mit Upload/Download/Versionen/Papierkorb, Passwort-Reset-Seite und die einheitliche Fehler-/Wiederholungsdarstellung (CC-P03-03/04, PR-015); Quota-, Audit-/Aktivitäts- und Einstellungsansichten sind geplant. **Zielphase:** P3 (Admin & Operations). **Priorität:** MVP/Blocker.  

Gestaltung: verbindlich nach [Admin-UI-Designsystem](../11-design/README.md).
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## AC-08: Organisationsverwaltung und Instanzkonsole

AC-08 ergänzt lokal eine Organisationsverwaltung mit Übersicht, gefilterter Personenliste, einzeln ausgeführten Mehrfachaktionen, Einladungslisten, Personendetails, Workspaces, Freigaben, Sperren, Aktivität und Einstellungen. Der getrennte Instanzbereich bündelt Organisationen, Konten einschließlich Gerätewiderruf und Systemzustand. Workspace-Admins sehen ausschließlich Freigaben und Sperren ihrer eigenen Admin-Workspaces; Organisationsrechte vermitteln keinen Zugriff auf kontoweite Geräte. Die neuen Verträge und Sicherheitsgrenzen stehen in [ADR-0024](../09-decisions/adr-0024-admin-console.md) und der [Berechtigungsmatrix](../05-security/permission-matrix.md). Verifizierter Prüfstand und verbleibende Einschränkungen stehen im AC-08-Abnahmebericht; es wurde nichts veröffentlicht.

## Problem und Nutzen

Das Webinterface ist Verwaltungswerkzeug: Setup, sichere Anmeldung, Org/Workspace-Auswahl, Projektliste, Dateien/Versionen, Mitglieder und Invites, Rollen/Quota, Storage-Health und Audit. Es soll den Craft-Stil in abstrahierten Tokens aufnehmen, aber keine fremden Assets kopieren. Für einen separaten Server ist React+TypeScript pragmatisch; NICHT in Rust-only Launcher integrieren.

## Detaillierter Funktionsumfang und technische Umsetzung

Feature-orientierte React-Komponenten `routes/setup`, `routes/organizations`, `routes/workspaces`, `routes/projects`, `routes/files`, `routes/members`, `routes/settings`, `routes/activity`. Typed API Client aus OpenAPI, serverseitiges Policy-Filtering plus UI-Hinweise. CSRF Cookie/Origin; kein Token in localStorage. Keyboard navigation, visible focus, loading/empty/error, WCAG 2.2 AA als Ziel, reduzierte Bewegung.

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

Ladezustand bei offline Backend, 403 nach Rollenwechsel ohne Refresh, unsaved form, Upload cancellation, 409 conflict, fehlerhafte Unicode-Namen, screen reader, Mobilansicht, 2 Admins bearbeiten gleichzeitig Quota.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Admin kann kompletten End-to-End-Kernpfad ohne CLI durchführen; keine Aktion suggeriert fehlenden Zugriff; 403/409/503 sind verständlich; Tab+Screenreader-Tests; visuelles Theme ohne ungeklärte Assets.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
