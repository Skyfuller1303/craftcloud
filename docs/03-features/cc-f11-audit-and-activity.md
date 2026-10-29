# CC-F11 — Audit-Events und Aktivitätsverlauf

> **Status:** lokal umgesetzt (PR-016, CC-P03-05; nicht geprüft, nicht gemergt; siehe „Umsetzungsstand“ am Ende). **Zielphase:** P3 (Admin & Operations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Admin sieht sicherheitsrelevante Aktionen (Login-Fehler aggregiert, Rollenwechsel, Invite, Download/Export je Policy, Projektlöschung, Quota, Backup/Restore). Audit-Events sind append-only aus Anwendungssicht, fassen Actor+Org+Objekt+Action+Zeitpunkt+Request-ID und Ergebnis zusammen. Keine dauerhafte Speicherung von Klartext-Access-Tokens, Passwörtern, Originaldateien.

## Detaillierter Funktionsumfang und technische Umsetzung

`GET /organizations/{org}/audit?cursor=...`, serverseitige Event-Erzeugung bei Transaktionscommit/Outbox-Pattern oder eindeutige Failure-Semantik. Filter und Export nur für autorisierte Rollen, Limit/Retention konfigurierbar; personenbezogene Daten minimieren. Bei Systemfehler audit failure policy für sensitive write actions klären.

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

Admin löscht eigene Rolle, Audit export cross org, Logging bei DB-Timeout, duplizierte Retry-Events, Manipulationsversuche, PII in Fehlermeldungen, Löschpflicht gegen Auditaufbewahrung.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Security-sensitive Änderung erzeugt zeitnah genau einen logischen Event mit nachvollziehbarer Request-ID; Org B kann Ereignisse von Org A nicht lesen; Events sind ohne Secrets exportierbar.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Umsetzungsstand (PR-016)

Lokal umgesetzt, nicht geprüft und nicht gemergt. Grundlage ist ADR-0016 (Aufbewahrung ein Jahr, personenbezogene Daten minimieren, keine IP-Adressen); die darüber hinaus getroffenen Entscheidungen (Aggregation, Fehlerrichtlinie, Umfang) sind zur Aufnahme in das Decision-Log vorgeschlagen.

**Tabelle `audit_events`** (Migration 0011): `id`, `org_id` (NULL für Instanz-Ereignisse), `actor_id` (NULL für das System oder unbekannte Konten), `action` (stabiler Punkt-Code wie `member.add`), `target_type`/`target_id`, `workspace_id`, `result` (`success`, `denied`, `failed`), `request_id`, `created_at` (Datenbankzeit), `event_count`/`last_at`/`agg_key` (aggregierte Zeilen) und `details` (flaches JSON aus IDs, festen Codes, Zählern und Flags). Es gibt keine Fremdschlüssel (die Historie überlebt die Objekte, die Aufbewahrung wird nie blockiert) und keine Spalte für IP-Adresse, User-Agent, Namen oder E-Mail-Adressen. `details` entsteht im Code nur aus getypten Werten (`Detail::Id/Code/Count/Flag`), ein Freitext-Feld existiert nicht. Datenbank-Trigger lehnen `UPDATE` (außer dem aufwärts laufenden Zähler einer aggregierten Zeile), `DELETE` (außer durch den Tabellen-Owner, also innerhalb der Funktion `audit_purge`) und `TRUNCATE` ab. Mit getrennten Datenbankrollen (Migration 0012, `CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE`/`CRAFTCLOUD_DATABASE_RUNTIME_ROLE`) hält das auch gegen die Laufzeitrolle des Servers; mit einer Rolle ist der Schutz nur beratend (siehe docs/06-operations/deployment.md).

**Schreibsemantik.** Ein sensibler Schreibvorgang schreibt sein Ereignis in **derselben Transaktion** wie die Änderung (`OrgTx::audit` bzw. die `*_audited`-Funktionen von `craftcloud-db`): Es entsteht genau ein Ereignis je Commit, bei Rollback keines, und ein Wiederholungsaufruf, der nichts ändert (gleiche Rolle noch einmal, bereits benutzter Reset- oder Einladungstoken, erneut gesendeter Commit derselben Upload-Sitzung, gleicher Kontostatus), schreibt keines. **Fehlerrichtlinie (fail closed):** Schlägt das Einfügen des Ereignisses fehl, schlägt die Anfrage fehl (500) und die Änderung wird zurückgerollt. Ereignisse ohne zu ändernde Daten stehen **außerhalb** einer Transaktion und sind best-effort: Sie werden nach der Antwort von einem eigenen Task geschrieben (zwei Sekunden Zeitlimit; zwei getrennte Budgets gleichzeitiger Schreibvorgänge, etwa ein Viertel des Datenbankpools: Verweigerungen und Login-Fehlschläge werden bei Überlast verworfen und gezählt, Downloads und Exporte warten bis zu zwei Sekunden; Fehler werden geloggt, die Antwort ändert sich nie): verweigerte Zugriffe (403), fehlgeschlagene Logins, Downloads und Exporte. Verweigerte Anfragen (je Aktion, Ergebnis, Akteur, Organisation und 15-Minuten-Fenster, **nicht** je Objekt), fehlgeschlagene Logins und Downloads (zusätzlich je Ziel) werden zu **einer Zeile mit Zähler** zusammengefasst; ein fehlgeschlagener Login hat keinen Akteur (niemand ist angemeldet), das angegriffene Konto ist das Ziel, unbekannte Adressen teilen sich eine Zeile ohne Ziel, die Adresse selbst wird nie gespeichert. 404-Antworten (Nicht-Mitglieder, unbekannte IDs) werden nicht aufgezeichnet, damit Fremde das Protokoll einer Organisation nicht füllen können. Die bisherigen `craftcloud::audit`-Logzeilen bleiben unverändert bestehen.

**Aufgezeichnete Aktionen.** Organisation: `organization.create`, `organization.update` (die Details `name_changed` und `invites_setting_changed` sind nur wahr, wenn sich der Wert wirklich geändert hat), `organization.quota`, `organization.owner_recover` (CLI `set-org-owner`, ohne Akteur). Mitglieder: `member.add`, `member.change_role`, `member.remove`, `workspace.member.add`, `workspace.member.change_role`, `workspace.member.remove`. Einladungen: `invitation.create`, `invitation.revoke`, `invitation.accept`, `invitation.invalidate` (der Link stirbt bei der Annahme, weil der Einladende sein Recht verloren hat; ohne Akteur). Workspaces: `workspace.create`, `workspace.update`, `workspace.quota`. Projekte und Dateien: `project.create`, `project.update`, `project.delete`, `project.restore`, `project.purge`, `file.create`, `file.update`, `file.delete`, `file.restore`, `file.purge`. Versionen: `version.commit`, `version.restore`, `version.trim`. Download: `file.download` (jedes Ausliefern von Inhalt, auch ein Range-Lesen ab beliebiger Stelle, zusammengefasst je Person, Version und 15-Minuten-Fenster mit Zähler; HEAD nicht; `CRAFTCLOUD_AUDIT_DOWNLOADS`, Standard `true`). Quota-Änderungen (`organization.quota`, `workspace.quota`) werden nur aufgezeichnet, wenn sich der Wert ändert (Detail `quota_bytes`/`unlimited`). Konto und Anmeldung (Instanz-Ebene): `auth.login` (Erfolg in der Sitzungstransaktion, Fehlschlag aggregiert), `auth.logout_all`, `auth.session_revoke` (eine andere eigene Sitzung beenden; die benutzte Sitzung zu beenden ist ein Logout und wird nicht aufgezeichnet), `auth.password_change`, `password_reset.issue`, `password_reset.use`, `password_reset.void`, `user.disable`, `user.enable`, `operator.create`. Betrieb (Instanz-Ebene): `storage.rebind` (CLI `storage-bind --force`), `maintenance.quarantine`. Protokoll selbst: `audit.export`. Verweigerungen (`result=denied`) tragen den Code der verweigerten Aktion. Nicht aufgezeichnet: Upload-Sitzungen (`upload.*`, nur Logzeilen), reine Lesezugriffe außer Downloads und Export, einzelne Logout-Vorgänge.

**API** (`contracts/openapi.yaml`): `GET /organizations/{org_id}/audit` (nur Organisations-Owner und -Admin, Aktion `AuditRead`; Mitglied 403, Nicht-Mitglied und Operator ohne Mitgliedschaft 404), `GET /organizations/{org_id}/audit/export` (NDJSON, gestreamt mit begrenztem Speicher, höchstens 100 000 Ereignisse je Anfrage, danach eine Zeile `{"truncated":true,"next_cursor":...}`), `GET /admin/audit` und `GET /admin/audit/export` (nur Operator, nur Instanz-Ereignisse). Filter: `action` (Präfix an Punktgrenzen), `actor`, `target`, `workspace`, `result`, `from` (einschließlich) und `to` (ausschließlich) als RFC 3339; Keyset-Paging über `(created_at, id)` absteigend, stabil auch bei gleichen Zeitstempeln.

**Aufbewahrung.** `CRAFTCLOUD_AUDIT_RETENTION_DAYS` (Standard 365, 30 bis 3650). Der Wartungsjob löscht ältere Ereignisse im Schritt `purge_audit` in Stapeln mit der Datenbankuhr; der Schritt berührt keine Objekte im Speicher und wird deshalb weder durch Uhrabweichung noch durch einen Epochen-Konflikt gesperrt. Im Lauf-Ergebnis stehen `purged_events` (bzw. `events_due` im Probelauf).

**Oberfläche.** Der Navigationspunkt „Aktivität“ der Admin-Oberfläche zeigt das Protokoll (Filter, Tabelle mit Weiterladen, Exportlink, Umschalter auf das Instanz-Protokoll für den Operator); der Server entscheidet bei jeder Anfrage.

**Nicht enthalten / offen:** Datenschutz-Löschpflicht gegen Aufbewahrung ist nur durch die Pseudonymität (IDs statt Namen) und die Frist gelöst, nicht durch ein Löschverfahren je Person; keine Manipulationssicherung gegen einen Betreiber mit Datenbankzugriff (kein Hash-Kettenverfahren, keine externe Senke); kein Export als Datei-Download mit Signatur; Aktionsnamen werden in der Oberfläche als Codes angezeigt; Mitgliedsnamen nur, wenn der Betrachter die Mitgliederliste sehen darf und die Person auf deren erster Seite steht; Ereignisse zu Backup/Restore (CC-F14) entstehen mit diesem Feature.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
