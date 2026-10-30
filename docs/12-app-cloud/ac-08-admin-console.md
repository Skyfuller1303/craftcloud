# AC-08 — Admin-Konsole

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac08`, Branch `feat/ac-08-admin-console` (Basis: `main` nach Merge von AC-02; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-08\REPORT.md`. **Migration:** keine, außer der Bericht begründet eine. **Review:** unabhängig, sicherheitsrelevant (neue Admin-Routen).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac08 (Branch feat/ac-08-admin-console).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-08-admin-console.md und docs/11-design/ vollständig
und setze AC-08 genau danach um. Halte die Arbeitsweise aus dem README ein.
Cargo nur über den Speicher-Launcher mit --gib 8. `cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-08\REPORT.md im vorgegebenen Format.
```

## Ziel

Das Webpanel bekommt eine Verwaltung, wie man sie von einer Admin-Konsole für Kreativ-Software kennt: Organisations-Admins sehen auf einer Übersichtsseite Personen, Speicher, offene Einladungen und letzte Aktivität, verwalten Personen und Workspaces in Tabellen mit Suche, Filtern und Mehrfachaktionen, sehen alle Freigaben und aktiven Sperren der Organisation und stellen Richtlinien ein. Der Instanz-Operator hat einen eigenen Bereich für Organisationen, Konten und Systemzustand. Lizenzen, Plätze und Abrechnung gibt es nicht.

## Ist-Stand (verifiziert 2026-10-30, main 73a06bd)

- Bildschirme in `apps/craftcloud-admin/src/screens/`: Projekte, Dateien (mit Upload, Versionen, Teilen, Sperren), Mitglieder, Einladungen, Aktivität, System, Geräte (eigene), Für mich freigegeben, Login, Setup, Einladung annehmen, Passwort zurücksetzen. Navigation in `Shell.tsx`.
- Designsystem: `docs/11-design/` (Tokens, Komponenten, Tabellen mit Kartenansicht bei 360 px, Dialoge, Barrierefreiheit), SCSS-Partials unter `src/styles/`, i18n als TSV (`src/locales/en.tsv`, `de.tsv`).
- Vorhandene Routen für die Verwaltung: `/organizations/{id}/members` (Liste, `?query=` mit Ratenbegrenzung, Rolle ändern, entfernen), `/organizations/{id}/invitations`, `/organizations/{id}/workspaces`, `/workspaces/{id}/members`, `/workspaces/{id}/quota`, `/organizations/{id}/usage`, `/organizations/{id}/audit` und `/audit/export`, `/projects/{id}/grants`, `/files/{id}/grants`, `/files/{id}/lock` (Freigabe mit `?force=true`). Operator: `/admin/organizations`, `/admin/organizations/{id}/quota`, `/admin/users` (deaktivieren, aktivieren, Reset-Link), `/admin/audit`, `/admin/health`, `/admin/maintenance`.
- Es fehlt: eine Liste aller Freigaben und aller aktiven Sperren einer Organisation, eine Personen-Detailansicht, Mehrfachaktionen, eine Übersichtsseite, die Verwaltung fremder Geräte-Sitzungen.

## Entscheidungen (fest)

1. **Navigation:** Bereich „Verwaltung“ (nur für Org-Owner und -Admins sichtbar; Server prüft trotzdem jede Anfrage) mit Übersicht, Personen, Workspaces, Freigaben, Sperren, Aktivität, Einstellungen. Bereich „Instanz“ nur für den Operator mit Organisationen, Konten, System. Die bisherigen Einzelbildschirme gehen darin auf; bestehende URLs leiten weiter.
2. **Übersicht:** Kacheln für aktive Personen, offene Einladungen, belegten Speicher gegen Quota (je Workspace), aktive Sperren, 2FA-Abdeckung (Anteil der Personen mit 2FA, nur wenn `/info` das Merkmal von AC-02 meldet) und die letzten 10 Audit-Ereignisse. Keine neuen Metriken-Endpunkte, außer der Bericht zeigt, dass mehrere Aufrufe zu teuer sind; dann ein Endpunkt `GET /organizations/{id}/overview`.
3. **Personen:** Tabelle mit Suche, Filter nach Rolle, Status und 2FA, Sortierung, Cursor-Paginierung. Mehrfachaktionen: Rolle ändern, entfernen; jede Aktion läuft je Person einzeln über die bestehende Route und zeigt das Ergebnis je Zeile (kein Sammel-Endpunkt, keine Teil-Transaktion über mehrere Personen). Einladen mehrerer Personen per eingefügter E-Mail-Liste (eine Adresse je Zeile, höchstens 50, gleiche Rolle), ebenfalls je Person über die bestehende Route. Detailansicht als Seitenbereich: Org-Rolle, Workspace-Rollen, erhaltene Freigaben, gehaltene Sperren, letzte Aktivität.
4. **Freigaben:** neue Route `GET /organizations/{id}/grants` (Owner/Admin der Org, Cursor-Paginierung, Filter nach Workspace, Person, Ziel-Art) und Tabelle mit Widerruf über die bestehende `DELETE /grants/{id}`. Workspace-Admins sehen nur ihre Workspaces.
5. **Sperren:** neue Route `GET /organizations/{id}/locks` (Owner/Admin der Org, Workspace-Admins nur ihre Workspaces) mit Datei, Halter, seit, Ablauf, Art der Sitzung (Browser oder Gerät, ohne Sitzungs-ID); Freigeben über `DELETE /files/{id}/lock?force=true` mit Bestätigung.
6. **Geräte anderer Personen:** nur der Instanz-Operator darf Geräte-Sitzungen eines Kontos sehen und widerrufen (`GET` und `DELETE /admin/users/{id}/devices`), weil Geräte zum Konto gehören und ein Konto mehreren Organisationen angehören kann. Org-Admins sehen keine Gerätenamen anderer Personen. Widerruf entfernt die Sperren dieser Sitzung (bestehende Funktion) und wird auditiert.
7. **Einstellungen der Organisation:** Name, Einladungsrecht für Workspace-Admins (vorhanden), 2FA-Pflicht (von AC-02, falls gemerged; sonst ausgeblendet und im Bericht genannt).
8. **Instanz:** Organisationen mit Personen- und Speicherzahl, Quota setzen; Konten instanzweit mit Suche, deaktivieren, aktivieren, Reset-Link, Geräte; System mit Health, Wartungsläufen und dem Zeitpunkt des letzten Backups, falls der Server ihn kennt (sonst Hinweis auf `craftcloud-ctl status` aus OPS-01).
9. **Datenschutz und Sicherheit:** Neue Listen sind organisationsgebunden und laufen über `can()`; fremde Organisationen ergeben 404. Keine E-Mail-Adressen in Audit-Details, keine Tokens oder Sitzungs-IDs in Antworten der neuen Routen. Die bestehende Ratenbegrenzung der Personensuche gilt auch in der Konsole (Eingabe entprellen).
10. **Design:** strikt nach `docs/11-design/`, vorhandene Komponenten (Badge, Banner, EmptyState, Toaster, Tabellen mit Kartenansicht), Englisch und Deutsch, Tastatur und axe ohne Verstöße inklusive Kontrast, 360 px ohne horizontales Scrollen.

## Erlaubte Dateien

`crates/craftcloud-api/src/**` (neue Routen für Freigaben-, Sperren- und Geräteliste), `crates/craftcloud-db/src/**`, `crates/craftcloud-core/src/policy.rs` (neue Aktionen), `apps/craftcloud-admin/src/**`, `contracts/openapi.yaml` und Snapshot, `docs/05-security/permission-matrix.md`, `docs/03-features/` (betroffene Feature-Briefings, Status), neues ADR `docs/09-decisions/adr-0024-admin-console.md`, `docs/gaps.md`.

## Tests (Pflicht)

- Policy: Matrix für die neuen Aktionen (Owner, Admin, Workspace-Admin, Editor, Viewer, Gast, Operator, fremde Org).
- DB/HTTP: neue Listen mit Paginierung, Filtern und Mandantentrennung; Workspace-Admin sieht nur eigene Workspaces; Operator-Geräteroute widerruft und entfernt Sperren; keine Sitzungs-IDs oder Tokens in Antworten.
- Web: Komponententests je Bildschirm EN/DE mit axe; Mehrfachaktion mit Teilfehlern zeigt Ergebnis je Zeile; Einladungsliste mit ungültigen Zeilen.
- Echter Browserlauf gegen eine eigene Instanz (nicht Port 8080) wie beim Designlauf: alle neuen Bildschirme hell/dunkel × EN/DE × 1280/360 px, Screenshots und axe-Ergebnis im Ausgabeordner.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Browserlauf mit 0 axe-Verstößen, 0 CSP-Fehlern, ohne horizontales Scrollen bei 360 px.
3. Berechtigungsmatrix und Policy-Tests stimmen überein; neue Routen in OpenAPI.
4. Entscheidungen 1–10 umgesetzt oder begründet abweichend; ADR-0024 angelegt.

## Nicht Teil von AC-08

Lizenzen, Plätze, Produktzuweisung und Abrechnung; SSO, SCIM und Verzeichnisanbindung; E-Mail-Versand; Berichte und Diagramme über die Kacheln hinaus; Verwaltung im Launcher.
