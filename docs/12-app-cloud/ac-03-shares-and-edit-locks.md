# AC-03 — Freigaben an Personen und Bearbeitungssperre

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac03`, Branch `feat/ac-03-shares-and-locks`. **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-03\REPORT.md`. **Migration:** `0016_shares_and_locks.sql` (Nummer reserviert; läuft parallel zu AC-01, das `0015` belegt). **Review:** unabhängig, sicherheitsrelevant.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac03 (Branch feat/ac-03-shares-and-locks).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-03-shares-and-edit-locks.md vollständig
und setze AC-03 genau danach um. Halte die Arbeitsweise aus dem README ein.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-03\REPORT.md im vorgegebenen Format.
```

## Ziel

1. Eine Person gibt eine **Datei oder ein Projekt** an bestimmte andere Personen frei, zum Lesen oder Bearbeiten, ohne sie in den ganzen Workspace aufzunehmen. Die Freigabe ist im Webpanel bedienbar und später im Launcher (AC-07).
2. Wer eine Datei in einer App öffnet, nimmt eine **Bearbeitungssperre**. Andere sehen „In Bearbeitung von Anna, seit 10:42“, können lesen und herunterladen, aber keine neue Version speichern, bis die Sperre frei ist. FigCraft-Dateien sind ausgenommen (Live-Coworking, AC-06).

## Ist-Stand

- Rollen: Organisation owner > admin > member; Workspace admin > editor > viewer (`docs/05-security/permission-matrix.md`, `crates/craftcloud-core/src/policy.rs`). Zugriff auf Projekte und Dateien ergibt sich nur aus der Workspace-Rolle. Gast ist als „später“ vermerkt.
- Dateien: `files` mit `app` und `format` (`0007_projects_and_files.sql`), Versionen mit CAS (`If-Match`, 412 `VERSION_CONFLICT`, ADR-0009), Upload-Sitzungen (`uploads.rs`), Papierkorb.
- Kein Sperrkonzept; Konflikte werden optimistisch über die Basisversion erkannt.
- Webpanel: Ansichten Dateien und Projekte (`apps/craftcloud-admin/src/screens/FilesScreen.tsx`, `ProjectsScreen.tsx`), Design `docs/11-design/`.

## Entscheidungen (fest)

### Freigaben

1. **Tabelle `grants`:** `id`, `org_id`, `workspace_id`, `target_kind` (`project` | `file`), `target_id`, `grantee_user_id`, `role` (`viewer` | `editor`), `created_by`, `created_at`, `revoked_at`. Eindeutig je (Ziel, Person) solange nicht widerrufen. Ein Projekt-Grant gilt für alle Dateien des Projekts.
2. **Neue Organisationsrolle `guest`:** Mitglied der Organisation ohne Workspace-Zugriff; sieht ausschließlich Objekte mit Grant. Einladung als Gast über den bestehenden Einladungsweg (Rolle `guest` zulässig für Owner/Admin und für Workspace-Admins bei eigenen Workspaces). Bestehende Mitglieder (`member`) können ebenfalls Grants erhalten.
3. **Wirksames Recht** = Maximum aus Workspace-Rolle und Grants (Datei-Grant, Projekt-Grant). Umsetzung **zentral in `can()`**: Die Ressourcenauflösung lädt Grants des Akteurs für Datei und Projekt in derselben Abfrage/Transaktion. Kein Grant kann mehr als `editor` geben; Grants geben nie Verwaltungsrechte (Mitglieder, Projekte anlegen, Freigaben anderer ändern).
4. **Wer darf freigeben:** Workspace-Editor und höher für Objekte im Workspace; Personen mit Datei-/Projekt-Grant `editor` dürfen **nicht** weiter freigeben (Entscheidung gegen Weiterverbreitung; Org-Einstellung später möglich). Grant-Rolle höchstens die eigene Rolle.
5. **Fremde Organisationen:** Grants nur an Mitglieder derselben Organisation (inkl. Gast). Kein Grant über Organisationsgrenzen; Personen außerhalb werden als Gast eingeladen.
6. **Routen:** `GET/POST /files/{id}/grants`, `GET/POST /projects/{id}/grants`, `PATCH /grants/{id}` (Rolle, mit `If-Match`), `DELETE /grants/{id}`, `GET /organizations/{org_id}/shared-with-me` (Dateien und Projekte mit Grant für den Akteur, Keyset-Paging). Personensuche für den Dialog: `GET /organizations/{org_id}/members?query=` nur für Berechtigte, liefert Name und E-Mail der Mitglieder (bestehende Mitgliederliste erweitern, Paging, Ratenbegrenzung gegen Aufzählung).
7. **Widerruf wirkt sofort** bei der nächsten Anfrage; laufende Uploads werden beim Abschluss unter dem Organisations-Lock erneut geprüft (wie SR-02).
8. **Audit:** `grant.created`, `grant.changed`, `grant.revoked`.

### Bearbeitungssperre

9. **Tabelle `file_locks`:** `file_id` (PK), `org_id`, `holder_user_id`, `holder_session_kind` (`browser` | `device`), `holder_session_id`, `client_id` (z. B. `photocraft`), `acquired_at`, `refreshed_at`, `expires_at`.
10. **Lease:** Ablauf 5 min nach letztem Herzschlag; Clients senden alle 60 s. Abgelaufene Sperren gelten als frei, ohne Job (Prüfung `expires_at > now()`); der Wartungsjob löscht alte Zeilen.
11. **Routen:** `POST /files/{id}/lock` (nehmen oder verlängern; braucht Bearbeitungsrecht; 200 mit `expires_at`; 423 `FILE_LOCKED` mit `holder: {display_name}`, `since`, `expires_at`, wenn jemand anderes hält). `DELETE /files/{id}/lock` (eigene Sperre freigeben). `DELETE /files/{id}/lock?force=true` (Workspace-Admin und höher; Audit `file.lock_forced`). Lock-Status in `GET /files/{id}` und in Dateilisten (`lock: {holder_display_name, since, expires_at, mine}`).
12. **Wirkung:** Solange eine fremde Sperre gilt, liefern neue Version (`POST /files/{id}/versions`), Upload-Anlage für neue Version, Umbenennen, Papierkorb und Wiederherstellen einer alten Version 423 `FILE_LOCKED`. Lesen, Herunterladen, Versionsliste bleiben erlaubt. Die haltende Person speichert normal; die Sperre bleibt bis zum Freigeben bestehen.
13. **Gleiche Person, zweites Gerät:** Sperre ist an die Sitzung gebunden. Eine zweite Sitzung derselben Person erhält 423 mit `mine: true` und darf mit `takeover=true` übernehmen (Audit), damit ein abgestürzter Laptop nicht 5 min blockiert.
14. **FigCraft:** `POST /files/{id}/lock` auf Dateien mit `app = figcraft` antwortet 409 `LOCK_NOT_SUPPORTED`; Speichern ist dort nie durch Sperren blockiert.
15. **Rechteentzug** löscht Sperren der betroffenen Person im Workspace bzw. auf dem Ziel in derselben Transaktion.
16. **Audit:** `file.locked`, `file.unlocked`, `file.lock_forced`, `file.lock_taken_over`; Herzschläge nicht.

### Webpanel

17. **Teilen-Dialog** an Datei- und Projektzeilen (Zeilenaktion „Teilen“): Liste der Freigaben (Name, Rolle, entfernen), Person hinzufügen per Suche, Rolle wählen. Nur sichtbar für Berechtigte.
18. **Ansicht „Für mich freigegeben“** in der Navigation (für alle Rollen inkl. Gast): Dateien und Projekte mit Grant, gruppiert nach Workspace, mit Download und Versionen.
19. **Sperranzeige:** Badge „In Bearbeitung“ mit Name und Zeit in Dateiliste und Versionsdialog; Upload einer neuen Version im Webpanel bei fremder Sperre deaktiviert mit Erklärung; Admins sehen „Sperre aufheben“ mit Bestätigung.
20. Design nach `docs/11-design/`, Texte EN/DE, axe EN/DE, Tastaturtests für den Teilen-Dialog.

## Erlaubte Dateien

`crates/craftcloud-core/src/{policy,capabilities,identity}.rs`, `crates/craftcloud-db/**` (Migration `0016_shares_and_locks.sql`, neue Module `grants.rs`, `locks.rs`), `crates/craftcloud-api/src/**` (neue Module `grants.rs`, `locks.rs`; bestehende Routen für 423), `crates/craftcloud-backup/src/tables.rs` (`grants` sichern, `file_locks` nicht), `contracts/openapi.yaml` + Snapshot, `apps/craftcloud-admin/src/**`, Doku: `docs/05-security/permission-matrix.md`, `docs/03-features/cc-f05-roles-and-permissions.md`, `cc-f08-versions-and-conflicts.md`, neues ADR `docs/09-decisions/adr-0020-grants-and-edit-locks.md`, `docs/gaps.md`. Nicht: Dateien aus AC-01 (`device.rs`, `0015_*`), falls parallel vorhanden.

## Tests (Pflicht)

- Policy-Unit-Tests: Tabelle Rolle × Grant × Aktion (vollständig, generiert), Grant gibt nie Verwaltungsrechte, Gast ohne Grant sieht nichts (404, keine Existenzauskunft).
- `cargo xtask db-test`: Grants anlegen/ändern/widerrufen mit allen Rollen, Weitergabe durch Grant-Editor verboten, fremde Org verboten, „Für mich freigegeben“ zeigt genau die Grants, Projekt-Grant wirkt auf neue Dateien, Widerruf während laufendem Upload → Abschluss 401/404. Sperre: nehmen, verlängern, Ablauf, fremde Sperre blockiert jede schreibende Route (Matrix-Test über alle Routen), Lesen erlaubt, force durch Admin, takeover durch zweite Sitzung, FigCraft 409, Rechteentzug löscht Sperre, Gleichzeitigkeit (zwei Personen nehmen gleichzeitig → genau eine erhält 200).
- Web: Teilen-Dialog, Freigaben-Ansicht, Sperrbadge, axe EN/DE.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Entscheidungen 1–20 umgesetzt oder begründet abweichend.
3. Berechtigungsmatrix-Doku und Policy-Tests stimmen überein (Test, der die Matrix-Tabelle gegen `can()` prüft, falls vorhanden; sonst erweitern).
4. Keine Existenzauskunft über fremde Dateien durch Grant- oder Sperr-Routen (404 statt 403 wie bestehend).
5. ADR-0020 angelegt.

## Nicht Teil von AC-03

Öffentliche Links ohne Anmeldung (CC-F18), Benachrichtigungen per E-Mail, Ordnerfreigaben über Projekte hinaus, Sperren in Apps (AC-04/AC-05 nutzen die Routen), FigCraft-Livelinks (AC-06).
