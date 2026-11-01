# AC-13 — Setup-Assistent für die Ersteinrichtung

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac13`, Branch `feat/ac-13-setup-wizard` (Basis: aktuelles `main` **nach** dem Merge von OPS-02 und AC-12; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-13\REPORT.md`. **Migration:** `0023` (vergibt der Koordinator beim Start endgültig). **Review:** unabhängiges Opus-Review (Sicherheit: Geheimnisse in der Datenbank, Operatorrechte, Setup-Token).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac13 (Branch feat/ac-13-setup-wizard).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-13-setup-wizard.md vollständig
und setze AC-13 genau danach um. Halte die Arbeitsweise aus dem README ein.
Cargo nur über den Speicher-Launcher mit --gib 8, RUSTUP_TOOLCHAIN=stable und RUSTUP_AUTO_INSTALL=0.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen. Keine Commits, kein Push.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-13\REPORT.md im vorgegebenen Format.
```

## Ziel

Der Nutzer fragte (2026-11-01), ob das Webpanel einen Einrichtungsassistenten für die Erstinstallation auf einem Server hat. Heute gibt es nur eine einzelne Seite. Nach AC-13 führt ein mehrstufiger Assistent den ersten Operator nach der Installation durch alles, was für einen nutzbaren Betrieb nötig ist: Systemprüfung, Konto, Zwei-Faktor, E-Mail-Versand, Organisationsgrundlagen, erster Workspace und erstes Projekt, Einladungen, optionale Firmenanmeldung und Archiv, Zusammenfassung. Der Assistent lässt sich unterbrechen, fortsetzen und einzelne Schritte überspringen. Ein Betreiber ohne Projektkenntnis soll nach `install.sh` ohne Konfigurationsdateien zu bearbeiten zu einer arbeitsfähigen Instanz kommen.

## Ist-Stand (verifiziert 2026-11-01, main 58f91ed)

- `apps/craftcloud-admin/src/screens/SetupScreen.tsx`: eine Seite mit Setup-Token, E-Mail, Name, Passwort, Organisationsname und Kurzname; ruft `GET /api/v1/setup/status` und `POST /api/v1/setup/complete`. Danach Operator plus erste Organisation; Tabelle `instance_setup` (Migration 0003, Singleton mit `completed_at`, `operator_user_id`). `craftcloud-ctl setup-complete` entfernt danach die Bootstrap-Bereitstellung (OPS-01).
- Vorhandene Bausteine, die der Assistent wiederverwendet: `POST /auth/2fa/setup` und `TwoFactorSettingsScreen.tsx` (AC-02); `GET /admin/email-delivery` (nur Status) und `POST /admin/email-delivery/test` (AC-09/09b, `EmailSystem.tsx`); `/organizations/{org_id}/workspaces`, `/workspaces/{workspace_id}/projects`; `/organizations/{org_id}/invitations`; `/admin/organizations/{org_id}/quota`, `/admin/organizations/{org_id}/seats` und der App-Zugriffsmodus (AC-11); `/admin/sso` und `SsoScimSettings.tsx` (AC-10); `/admin/health`, `SystemScreen.tsx`, `OverviewScreen.tsx`; Archivstatus im Systembereich (AC-12).
- E-Mail-Versand wird heute **nur** über Umgebungsvariablen konfiguriert (`CRAFTCLOUD_MAIL_TRANSPORT`, `CRAFTCLOUD_SMTP_*`, `CRAFTCLOUD_SMTP_OAUTH_*`, `CRAFTCLOUD_GRAPH_*` in `apps/craftcloud-server/src/config.rs`). Gleiches gilt für Aufbewahrungsfristen (`CRAFTCLOUD_VERSION_RETENTION_DAYS`, `CRAFTCLOUD_TRASH_RETENTION_DAYS`, `CRAFTCLOUD_AUDIT_RETENTION_DAYS`, `CRAFTCLOUD_QUARANTINE_DAYS`). Es gibt keine Tabelle für Instanzeinstellungen außer `instance_setup` und `instance_sso_policy`.
- Serverschlüssel für Verschlüsselung gespeicherter Geheimnisse: `CRAFTCLOUD_SECRET_KEY_FILE` (mit `SECRET_KEY_OLD_FILE` für Rotation), bereits für FigCraft-Tokens und Zwei-Faktor genutzt.

## Entscheidungen (fest, Koordinator 2026-11-01, im Entscheidungslog)

1. **Ablauf:** Schritt 1 bleibt das bestehende Formular mit Setup-Token (unauthentifiziert, einmalig). Alle weiteren Schritte laufen **nach der Anmeldung** des neuen Operators und nur für Instanz-Operatoren. Der Fortschritt liegt serverseitig in einer neuen Tabelle `instance_setup_steps` (Schritt, Status `pending|done|skipped`, Zeitpunkt, Benutzer), damit ein zweiter Operator oder ein neues Browserfenster an derselben Stelle weitermacht. Der Assistent ist erneut aufrufbar (Systembereich „Einrichtung fortsetzen“), bis alle Pflichtschritte erledigt oder übersprungen sind; danach bleibt er als „Einrichtung prüfen“ (nur lesend für den Systemcheck) erreichbar.
2. **Schritte und Pflicht:**
   1. Konto und erste Organisation (vorhanden; Pflicht).
   2. **Systemcheck** (nur lesend, nicht überspringbar, Warnungen blockieren nicht): `CRAFTCLOUD_PUBLIC_URL` stimmt mit dem Origin des Browsers überein; Anfrage kam über HTTPS (direkt oder vertrauenswürdiger Proxy mit `X-Forwarded-Proto`); PostgreSQL-Version (OPS-02-Prüfung); freier Speicher im Speicherverzeichnis; FigCraft-Dienst erreichbar; Uhrzeitabweichung zur Datenbank; Wartung aktiv; Archivstatus, falls konfiguriert. Jede Prüfung mit Ergebnis, kurzer Erklärung und Verweis auf den passenden Abschnitt in `docs/06-operations/install.md`.
   3. **Zwei-Faktor für den Operator**: empfohlen, überspringbar mit deutlichem Hinweis; nutzt den vorhandenen Ablauf.
   4. **E-Mail-Versand**: Auswahl „Keiner (Links von Hand weitergeben)“, SMTP (mit Passwort oder OAuth), Microsoft Graph; Testmail an die eigene Adresse; überspringbar.
   5. **Organisationsgrundlagen**: Plätze, Speicherkontingent, App-Zugriffsmodus der ersten Organisation (vorhandene AC-11-Endpunkte) und die Aufbewahrungsfristen der Instanz.
   6. **Erster Workspace und erstes Projekt** (vorhandene Endpunkte; überspringbar).
   7. **Personen einladen** (vorhandener Einladungsablauf; ohne E-Mail-Versand werden die Links angezeigt und kopierbar gemacht; überspringbar).
   8. **Optional**: Firmenanmeldung (Verweis auf die vorhandene SSO-Seite, kein eigener Formularnachbau), Archivspeicher (Status und Verweis auf die Doku; die Archivkonfiguration bleibt in der Konfigurationsdatei, weil sie Schlüssel und Zugangsdaten enthält).
   9. **Zusammenfassung** mit Status je Schritt und „Fertig“.
3. **Einstellungen in der Datenbank mit Vorrang der Konfigurationsdatei.** Neue Tabelle `instance_settings` (Schlüssel, Wert als JSON, `updated_at`, `updated_by`). In AC-13 über die Oberfläche änderbar: E-Mail-Versand (alle Transportarten) und die vier Aufbewahrungsfristen. Ist ein Wert per Umgebungsvariable gesetzt, **gewinnt die Umgebungsvariable**; die Oberfläche zeigt ihn dann schreibgeschützt mit dem Hinweis „durch die Konfigurationsdatei festgelegt“. Infrastrukturwerte (Datenbank-URL, Bind-Adresse, öffentliche URL, Speicherverzeichnis, Serverschlüssel, vertrauenswürdige Proxys, maximale Datei- und Body-Größe, Metriken, FigCraft-Adressen, Archivspeicher) bleiben ausschließlich in der Konfigurationsdatei. Änderungen wirken **ohne Neustart** (Mail-Transport wird bei Änderung neu aufgebaut; Wartung liest die Fristen je Lauf).
4. **Geheimnisse in der Datenbank:** SMTP-Passwort, OAuth-Client-Secret bzw. -Schlüssel und Graph-Client-Secret bzw. -Zertifikat werden mit dem Serverschlüssel authentifiziert verschlüsselt gespeichert (dasselbe Verfahren und dieselbe Rotation wie die vorhandenen verschlüsselten Werte). Sie sind **nur schreibbar**: Die API gibt sie nie zurück, nur „gesetzt seit <Zeit>“; leeres Feld bedeutet „unverändert“, ausdrückliches Entfernen ist eine eigene Aktion. Nie in Logs, Audit-Details oder Fehlermeldungen. Ein Backup enthält die verschlüsselten Werte; der Restore funktioniert nur mit demselben Serverschlüssel, und die Doku sagt das.
5. **Rechte und Schutz:** Alle neuen Endpunkte nur für Instanz-Operatoren, mit CSRF, vorhandenem Rate-Limit für Admin-Aktionen und Audit-Ereignissen (Schritt erledigt/übersprungen, Einstellung geändert mit Schlüssel und altem/neuem Wert außer bei Geheimnissen). Verlangt die Instanz Zwei-Faktor oder hat der Operator ihn eingerichtet, gelten für Geheimnisänderungen die vorhandenen Regeln für sensible Aktionen (frische Anmeldung bzw. erneute Bestätigung, wie bei anderen sicherheitsrelevanten Operatoraktionen). Der Setup-Token bleibt einmalig; nach Schritt 1 ist er wirkungslos, unabhängig vom Fortschritt des Assistenten.
6. **Hinweis im Panel:** Solange Pflichtschritte offen sind, zeigt die Übersicht für Operatoren ein schließbares Banner „Einrichtung unvollständig“ mit Link zum Assistenten; andere Rollen sehen es nicht.
7. **Gestaltung:** vorhandene Komponenten und SCSS-Tokens aus dem Designdurchlauf (`docs/10-design/` bzw. die dort beschriebenen Bausteine), Schrittleiste mit Nummern und Status, Tastaturbedienung, Fokus beim Schrittwechsel auf die Überschrift, Texte Englisch und Deutsch in den TSV-Dateien.

## Erlaubte Dateien

`crates/craftcloud-db/migrations/0023_*.sql` (neu), `crates/craftcloud-db/src/**` (Einstellungen, Schritte), `crates/craftcloud-api/src/**` (neue Endpunkte, Systemcheck, Laden der Einstellungen), `crates/craftcloud-mail/**` (Neuaufbau des Transports aus Datenbankwerten), `apps/craftcloud-server/src/**` (Vorrangregel Umgebung vor Datenbank), `crates/craftcloud-backup/**` (neue Tabellen im Backup), `apps/craftcloud-admin/src/**` (Assistent, Banner, Tests, TSV), `contracts/openapi.yaml` und Snapshot, `docs/06-operations/**`, `docs/05-security/threat-model.md`, neues ADR `docs/09-decisions/adr-0030-setup-wizard-and-instance-settings.md`, `docs/gaps.md`, Statuszeile in diesem Briefing.

Nicht ändern: Installer und Compose (OPS-02), Archivkonfiguration (AC-12), SSO-Logik (AC-10), andere Worktrees.

## Tests (Pflicht)

- `cargo xtask ci` und `cargo xtask db-test` grün.
- Datenbank-/API-Tests: Fortschritt wird gespeichert und ist für einen zweiten Operator sichtbar; Nicht-Operatoren erhalten 403 für alle neuen Endpunkte; Umgebungsvariable gewinnt gegen Datenbankwert und Schreibversuch auf einen festgelegten Wert wird abgelehnt; Geheimnis wird verschlüsselt gespeichert (Rohwert nicht in der Tabelle), nie zurückgegeben, leeres Feld ändert nichts; Mail-Transport wird nach Änderung ohne Neustart neu aufgebaut (Testmail über einen lokalen SMTP-Testserver im Test); Fristen wirken im nächsten Wartungslauf; Audit-Ereignisse ohne Geheimnisse; Backup/Restore mit den neuen Tabellen; Setup-Token nach Schritt 1 wirkungslos.
- Systemcheck: Tests für abweichende öffentliche URL, HTTP statt HTTPS, nicht unterstützte PostgreSQL-Version (Mock), wenig freier Speicher, FigCraft nicht erreichbar.
- Admin-Oberfläche: Durchlauf aller Schritte, Überspringen, Fortsetzen nach Neuladen, schreibgeschützte Felder bei Konfigurationsvorrang, Banner nur für Operatoren, Barrierefreiheit (vorhandene a11y-Tests erweitern), EN/DE.
- Kein Geheimnis in Logs: Testlauf mit gesetztem SMTP-Passwort, anschließend Logs und Audit nach dem Wert durchsuchen.

## Abnahme

1. `cargo xtask ci` Exit 0, `cargo xtask db-test` Exit 0 (Anzahl bestanden/fehlgeschlagen).
2. Alle Pflichttests vorhanden und grün; Tabelle Entscheidung → Test im Bericht.
3. Screenshots jedes Schritts (hell und dunkel) unter `D:\ArtCraft\output\craftcloud-codex\AC-13\screens\`.
4. `docs/06-operations/install.md` beschreibt die Ersteinrichtung mit dem Assistenten; ADR-0030 angelegt.
5. Aufräumen: eigene Docker-Ressourcen entfernt, `cargo clean --profile dev` für `wt-ac13\target` nach dem letzten Lauf.

## Nicht Teil von AC-13

Archivkonfiguration über die Oberfläche, Änderung von Infrastrukturwerten über die Oberfläche, Lizenzverwaltung über Plätze hinaus, Mehrsprachigkeit über EN/DE hinaus, Import bestehender Benutzer (außer über SCIM), Einrichtung von Backups auf dem Host (bleibt `craftcloud-ctl`/systemd).
