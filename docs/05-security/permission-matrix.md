# Berechtigungsmatrix (Vorschlag – per Endpoint erzwingen)

Roles: **Operator** (Instanzbetrieb, keine automatische Einsicht in Kundendateien per API), **Owner** (Org), **OrgAdmin**, **WorkspaceAdmin**, **Editor**, **Viewer**, **Guest** (später). `Operator` ist nicht einfach eine Org-Rolle und hat deshalb keine Spalte in dieser Tabelle; der in CC-F01/E2E-01 genannte „initiale Instanzadministrator“ ist diese Operator-Rolle. Es muss eine streng definierte Break-glass-Policy geben, falls Supportzugriff erforderlich wird.

| Aktion | Owner | OrgAdmin | WorkspaceAdmin | Editor | Viewer | Guest (later) |
|---|---|---|---|---|---|---|
| Organisation verwalten | ja | beschränkt | nein | nein | nein | nein |
| Admin-Rollen zuweisen | ja | beschränkt; niemals Owner | nein | nein | nein | nein |
| Mitglied einladen | ja | ja | nur Workspace, wenn freigegeben | nein | nein | nein |
| Workspace verwalten | ja | ja | eigener Workspace | nein | nein | nein |
| Projekt anlegen/umbenennen | ja | ja | eigener Workspace | ja, falls erlaubt | nein | nein |
| Projekt-/Dateiliste lesen | im Scope | im Scope | im Scope | im Scope | im Scope | explizite Freigabe |
| Datei herunterladen | im Scope | im Scope | im Scope | im Scope | im Scope | wenn erteilt |
| Upload/Version committen | im Scope | im Scope | im Scope | im Scope | nein | nein |
| Datei löschen | im Scope | im Scope | im Scope | konfigurierbar | nein | nein |
| Audit-Events lesen | ja | ja, im Scope | begrenzt | nein | nein | nein |
| Quota/Backup einstellen | ja, Scope | je Policy | nein | nein | nein | nein |
| Gast-Freigabe erstellen | ja | ja | je Policy | je Policy | nein | nein |

**Wichtig:** Diese Tabelle ist eine Vorentscheidung, kein bereits implementiertes Modell. Finalen Policy-Entscheid je Request+Ressource+Scope festlegen; Org-Admin darf keine Rolle mit höheren Rechten vergeben. Letzter Owner nicht einfach entfernbar. File/Project/Workspace-FKs müssen Scope-Übereinstimmung garantieren.

## Umgesetzt (PR-006)

Die Policy-Engine `craftcloud_core::policy::can(actor, action, resource)` ist rein (keine I/O, kein Cache). Der Actor (Operator-Flag, aktive Org-Mitgliedschaft, Workspace-Rolle) wird bei **jedem** Request neu aus der Datenbank geladen. Eine `OrgScope` für Repository-Aufrufe kann nur aus einem `Allow`-Permit gebaut werden. Entscheidungen: `Allow`, `Forbidden` (403, nur für Aufrufer, die die Ressource sehen dürfen) und `Hidden` (404, Nicht-Mitglieder, Operator ohne Mitgliedschaft, unbekannte IDs).

Rollen: `owner > admin > member` (genau eine Rolle je Nutzer und Organisation, kein Zusammenführen); `member` hat keine Org-Rechte. Der Operator ist keine Org-Rolle und hat keinen impliziten Zugriff auf Org-Daten.

| Aktion | Operator (ohne Mitgliedschaft) | Nicht-Mitglied | member | admin | owner |
|---|---|---|---|---|---|
| Eigene Organisationen listen | ja | ja | ja | ja | ja |
| Organisation anlegen (`POST /organizations`) | ja | nein (403) | nein (403) | nein (403) | nein (403) |
| Alle Organisationen als Metadaten (`/admin/organizations`) | ja | nein (403) | nein (403) | nein (403) | nein (403) |
| Organisation lesen / Mitglieder listen | 404 | 404 | ja | ja | ja |
| Organisation umbenennen | 404 | 404 | 403 | ja | ja |
| Mitglied hinzufügen (Rolle `member`/`admin`) | 404 | 404 | 403 | ja | ja |
| Mitglied mit Rolle `owner` hinzufügen | 404 | 404 | 403 | **403** | ja |
| Rolle ändern (member/admin untereinander, auch sich selbst) | 404 | 404 | 403 | ja | ja |
| Owner anfassen (Rolle ändern, entfernen) oder zu `owner` befördern | 404 | 404 | 403 | **403** | ja (letzter *aktivierter* Owner: 409) |
| Mitglied entfernen | 404 | 404 | nur sich selbst (Austritt) | member/admin, auch sich selbst | alle (letzter Owner: 409) |
| Konto deaktivieren/aktivieren, Reset-Link ausstellen/widerrufen | ja (nicht den Operator selbst) | 403 | 403 | 403 | 403 |
| Workspace lesen / verwalten | siehe Abschnitt PR-007 | 404 | nur mit Workspace-Rolle | ja | ja |

Der „letzte Owner“ zählt nur aktive Mitgliedschaften mit aktiviertem Konto; ein deaktivierter Owner zählt nicht. Das Deaktivieren eines Kontos wird bewusst nie durch diese Regel blockiert (Sicherheitsreaktion), der Operator erhält die Liste der betroffenen Organisationen, und der Hostbetreiber stellt einen Owner mit `craftcloud-server admin set-org-owner` wieder her (Break-glass, siehe `docs/06-operations/deployment.md`). Ein `OrgScope` merkt sich die erlaubte Aktion: schreibende Repository-Funktionen verweigern einen Scope aus einer Lese-Aktion.

Zugehörige Tests: `crates/craftcloud-core/src/policy.rs` (Tabellentests über alle Aktionen und Rollen) und `crates/craftcloud-api/tests/orgs.rs` (Isolation, Rollenregeln, Letzter-Owner-Race, CSRF, Ablauf).

## Umgesetzt (PR-007): Workspaces und Einladungen

Workspace-Rollen: `admin > editor > viewer`. Organisations-Owner und -Admins verwalten jeden Workspace ihrer Organisation und dürfen jede Workspace-Rolle vergeben; alle anderen sehen nur Workspaces, in denen sie Mitglied sind. Alles Unsichtbare antwortet 404 (identisch zu unbekannten IDs, auch organisationsübergreifend). Nur aktive, aktivierte Organisationsmitglieder können Workspace-Mitglied werden; Workspaces haben in P1 keine Löschroute (Lücke).

| Aktion | Nicht-Mitglied / Operator ohne Mitgliedschaft | Org-`member` ohne Workspace-Rolle | viewer | editor | WS-admin | Org-admin / -owner |
|---|---|---|---|---|---|---|
| Workspace anlegen | 404 | 403 | – | – | – | ja |
| Workspace lesen, Mitglieder listen | 404 | 404 | ja | ja | ja | ja |
| Workspace umbenennen (If-Match) | 404 | 404 | 403 | 403 | ja | ja |
| Mitglied hinzufügen / Rolle ändern | 404 | 404 | 403 | 403 | nur `editor`/`viewer`; nie einen WS-admin (auch nicht sich selbst) | jede Rolle |
| Mitglied entfernen | 404 | 404 | nur sich selbst | nur sich selbst | `editor`/`viewer` und sich selbst | alle |
| Einladung anlegen | 404 | 403 | 403 | 403 | nur Rolle `member`, nur eigener Workspace, höchstens `editor`, nur wenn `workspace_admin_invites_enabled` | `admin`/`member` (nie `owner`), optional jeder Workspace und jede Workspace-Rolle |
| Einladungen listen | 404 | 403 | 403 | 403 | 403 | ja |
| Einladung widerrufen | 404 | 403 | 403 | 403 | nur eigene | ja |
| `workspace_admin_invites_enabled` ändern (`PATCH /organizations/{id}`) | 404 | 403 | 403 | 403 | 403 | ja |
| Konten der Instanz mit Herkunft listen (`GET /admin/users`) | nur Operator (sonst 403) | 403 | 403 | 403 | 403 | 403 |
| Status des Wartungsjobs lesen (`GET /admin/maintenance`, Aktion `MaintenanceRead`) | nur Operator (sonst 403) | 403 | 403 | 403 | 403 | 403 |
| Betreiber-Gesundheitsansicht lesen (`GET /admin/health`, Aktion `HealthRead`; PR-017) | nur Operator (sonst 403) | 403 | 403 | 403 | 403 | 403 |

Einladungen annehmen (`POST /invitations/accept`) ist die Tokenbesitz-Aktion: wer den Link hält, beweist (bis SMTP existiert) den Besitz der Adresse. Bei der Annahme wird die Berechtigung des **Einladenden** mit seinen aktuellen Rollen erneut ausgewertet (gleiche Policy wie beim Anlegen); wurde er herabgestuft, deaktiviert, entfernt, ist der Workspace weg oder der Schalter ausgeschaltet, antwortet die Annahme einheitlich 400 und die Einladung wird als ungültig markiert. Eine Einladung ändert nie die Organisationsrolle eines bestehenden Mitglieds und senkt nie eine vorhandene Workspace-Rolle.

Zugehörige Tests: `crates/craftcloud-core/src/policy.rs` (Tabellentests) und `crates/craftcloud-api/tests/workspaces_invites.rs` (`workspace_member_role_matrix_including_workspace_admin_limits`, `workspaces_never_cross_organizations_and_swapped_ids_find_nothing`, `who_may_invite_what`, `stale_invitations_of_demoted_or_disabled_inviters_and_changed_settings_are_refused`, ...).

## Umgesetzt (PR-008): Projekte und Dateimetadaten

Alle Projekt- und Dateiaktionen entscheiden anhand der Rollen im Workspace des Projekts (die Organisation und der Workspace werden aus der Projekt- bzw. Dateizeile bestimmt, nie aus Client-Parametern). Wer den Workspace nicht sehen darf, bekommt 404 (identisch zu unbekannten IDs, auch bei IDs der falschen Art); Viewer bekommen bei Schreibaktionen 403.

| Aktion | Nicht sichtbar (Außenstehender, Org-`member` ohne Workspace-Rolle, Operator) | viewer | editor | WS-admin | Org-admin / -owner |
|---|---|---|---|---|---|
| Projekte / Dateien listen und lesen (auch Papierkorb) | 404 | ja | ja | ja | ja |
| Projekt anlegen / umbenennen | 404 | 403 | ja | ja | ja |
| Projekt in den Papierkorb / wiederherstellen | 404 | 403 | 403 | ja | ja |
| Datei anlegen / umbenennen / in den Papierkorb / wiederherstellen | 404 | 403 | ja | ja | ja |

Ein Projekt im Papierkorb ist schreibgeschützt (Dateien lesbar, Schreibaktionen 409 `PROJECT_DELETED`). Wiederherstellen ist nur innerhalb des Aufbewahrungsfensters möglich (410 `RETENTION_EXPIRED`). Quelle: ADR-0013 Punkt 8, ADR-0014.

Zugehörige Tests: `crates/craftcloud-core/src/policy.rs` (`project_and_file_actions_follow_workspace_roles`) und `crates/craftcloud-api/tests/projects_files.rs` (`project_role_matrix`, `file_role_matrix_and_metadata_defaults`, `projects_and_files_never_cross_organizations_workspaces_or_kinds`).

## Umgesetzt (PR-016): Audit-Log

| Aktion | Operator (ohne Mitgliedschaft) | Nicht-Mitglied | member | admin | owner |
|---|---|---|---|---|---|
| Audit-Log der Organisation lesen und exportieren (`GET /organizations/{id}/audit`, `.../audit/export`, Aktion `AuditRead`) | 404 | 404 | 403 | ja | ja |
| Instanz-Audit-Log lesen und exportieren (`GET /admin/audit`, `.../export`, Aktion `InstanceAuditRead`) | ja | 403 | 403 | 403 | 403 |

Abweichung vom Vorschlag in der ersten Tabelle: Der Org-Admin liest das Protokoll der Organisation vollständig (nicht „begrenzt“), WorkspaceAdmins ohne Org-Rolle lesen es nicht, und der Operator hat **keinen** impliziten Zugriff auf das Protokoll einer Organisation (nur auf Instanz-Ereignisse). Verweigerte Anfragen (403) werden aggregiert aufgezeichnet, 404-Antworten nicht. Zugehörige Tests: `crates/craftcloud-core/src/policy.rs` (`project_and_file_actions_follow_workspace_roles` mit der Zeile `AuditRead`, `instance_actions_are_operator_only`) und `crates/craftcloud-api/tests/audit.rs` (`only_owners_and_admins_read_the_audit_log_of_their_organization`).

## AC-03: Objektfreigaben und Bearbeitungsleases

Die verbindliche Entscheidung steht in [ADR-0020](../09-decisions/adr-0020-grants-and-edit-locks.md). `guest` ist eine Organisationsrolle ohne automatische Workspace-Mitgliedschaft. Eine Workspace-ID bei einer Gast-Einladung ist nur Einladungskontext. Gäste werden weder über die Annahme noch über Member-Add zu Workspace-Mitgliedern.

Objektberechtigungen sind das Maximum aus ursprünglicher Workspace-Rolle und aktiven Datei-/Projekt-Grants. `viewer` liest das freigegebene Ziel; `editor` darf zulässige Dateiaktionen ausführen. Projekt-Grants erlauben Projektansicht, Dateiaktionen und Dateianlage innerhalb dieses Projekts, aber keine Projektanlage oder Projektumbenennung, Papierkorb-/Restore-Verwaltung, Workspace- oder Mitgliederverwaltung. Datei-Grants erlauben nur die jeweilige Datei; Geschwisterdateien und Projektlisten bleiben verborgen. Der autorisierte Datei-DTO darf die minimale Parent-ID und den Namen enthalten. Freigaben verwalten ausschließlich Workspace-Editoren und höhere Rollen. Grants verleihen keine Freigabe- oder Member-Suchrechte.

| Aktion | Org-Owner/Admin | WS-Admin | WS-Editor | WS-Viewer | Grant-Editor | Grant-Viewer | Org-Gast ohne Grant | Org-Gast mit Grant |
|---|---|---|---|---|---|---|---|---|
| Freigaben sehen/anlegen/ändern/widerrufen | ja | eigener Workspace | eigener Workspace | 403 | 403 | 403 | 404 | 403 |
| Mitglieder auflisten | ja | ja | ja | ja | ja | ja | 403 | 403 |
| Mitglieder für Freigabe suchen | ja | ja | ja | 403 | 403 | 403 | 403 | 403 |
| Projekt-Grant: Projekt sehen/Dateien listen | ja | ja | ja | ja | Ziel und Dateien im Projekt | Ziel und Dateien im Projekt | 404 | nur mit Grant |
| Datei-Grant: Datei lesen/herunterladen | ja | ja | ja | ja | nur Ziel-Datei | nur Ziel-Datei | 404 | nur mit Grant |
| Datei-Grant: Datei ändern/Upload abschließen | ja | ja | ja | 403 | nur Ziel-Datei | 403 | 404 | nur mit Editor-Grant |
| Lock fremd erzwingen (`DELETE ?force=true`) | ja | ja | 403 | 403 | 403 | 403 | 404 | 403 |

Lease gehört einer aktiven Browser- oder Geräte-Sitzung (AC-01) und läuft nach fünf Minuten ab; sie endet außerdem mit dem Widerruf dieser Sitzung; Heartbeat verlängert sie. Dieselbe Person in einer anderen Sitzung darf nur mit `takeover=true` übernehmen. Fremde aktive Leases blockieren Datei-Metadatenänderung, Upload-Anlage/-Abschluss, Versionsabschluss/-Restore, Datei-Papierkorb/-Restore sowie Projekt-Papierkorb/-Restore mit 423 `FILE_LOCKED`. Lesen und Download bleiben möglich. FigCraft-Dateien antworten für Lock-Operationen mit 409 `LOCK_NOT_SUPPORTED`. Sichtbare, aber nicht erlaubte Lock-Aktionen antworten 403, nicht sichtbare 404; Dateien und Projekte im Papierkorb lehnen Lock und Grant mit 409 `FILE_DELETED`/`PROJECT_DELETED` ab. Die Mitgliedersuche ist je Benutzer ratenbegrenzt (429).

## Abnahmebelege

Die vollständige Grant-Aktionsmatrix liegt in `craftcloud-core/src/policy.rs::object_grant_matrix_is_scoped_and_never_grants_management`; Gäste-MemberList in `member_list`. HTTP-Abdeckung: `craftcloud-api/tests/shares_locks.rs::file_grant_does_not_reveal_project_or_sibling_and_shared_item_is_minimal`, `project_editor_grant_covers_files_but_not_project_management`, `leases_guard_writes_project_restore_and_audit_only_state_changes`. Upload-Zulassung und Entzug während des Streams: `craftcloud-api/tests/uploads.rs::file_grant_editor_can_upload_and_revocation_during_stream_is_rechecked`. Datei-Bindung beim Commit: `craftcloud-api/tests/versions.rs::an_upload_is_bound_to_its_file_even_with_a_foreign_target_lease`. DB-Rennen, Projekt-Lease-Suche und Grant-Demotion/Revoke-Cleanup: `craftcloud-db/tests/integration.rs::migration_0016_grant_shape_keys_and_shared_projection_are_valid`.

## AC-02: Zweiter Faktor und Organisationspflicht

Persönliche 2FA-Einrichtung und Status liegen im Kontoscope und bleiben auch dann erreichbar, wenn eine Organisation die Einrichtung verlangt. Setup verlangt eine erneute Passwortprüfung, Bestätigung den ersten TOTP-Code, Deaktivierung Passwort und zweiten Faktor, neue Wiederherstellungscodes einen gültigen zweiten Faktor. Eine bestehende Sitzung allein genügt nicht für diese Sicherheitsänderungen.

Die zusätzliche zentrale Aktion für `require_two_factor` ist ausschließlich für Owner und Admin der jeweiligen Organisation zulässig. Wer die Pflicht einschaltet, benötigt selbst aktives 2FA. Eine Mitgliedschaft oder ein Datei-/Projekt-Grant ohne eingerichteten zweiten Faktor umgeht die Pflicht nicht. Sichtbare Ressourcen der verpflichtenden Organisation antworten bis zur Einrichtung mit `403 TWO_FACTOR_SETUP_REQUIRED`; nicht sichtbare oder fremde Ressourcen behalten ihre bisherigen `404`-Grenzen. Andere Organisationen ohne Pflicht bleiben unverändert nutzbar.

Der bestehende Instanz-Operator kann beim Ausstellen eines Passwortresetlinks optional den zweiten Faktor zurücksetzen. Organisations- und Workspace-Administratoren erhalten dadurch keine neuen Rechte zum Zurücksetzen fremder Konten. Der Offline-Befehl `craftcloud-server admin user reset-2fa <email>` setzt Host- und Datenbankzugriff voraus. Änderungen und Reset widerrufen andere Browser- und Geräte-Sitzungen und werden ohne Geheimnisse auditiert. Siehe [ADR-0019](../09-decisions/adr-0019-two-factor.md).

## Zusätzliche Negativtests für AC-02

TOTP-Wiederholung, doppelte Recovery-Code-Nutzung, gleichzeitige Challenge-Einlösung, falsche Adresse oder Anmeldeart, Challenge-Ablauf und Versuchsgrenze, Passwortwechsel beziehungsweise Reset während einer Anmeldung sowie 2FA-Pflicht bei Grants und laufenden Uploads müssen serverseitig abgedeckt werden. Aktuelle Testergebnisse werden im AC-02-Abnahmebericht festgehalten.

## AC-08: Organisations- und Instanzkonsole

Die neuen Listen verwenden eigene zentrale Policy-Aktionen. Eine Operatorrolle ist kein Ersatz für eine Mitgliedschaft in einer Organisation. Workspace-Administratoren erhalten nur Zeilen aus Workspaces, in denen sie selbst Admin sind; diese Grenze wird in der Datenbankabfrage angewandt, nicht erst in der Oberfläche. Ein ausdrücklich angefragter fremder Workspace bleibt verborgen. Für Gäste und Org-Mitglieder ohne Workspace-Mitgliedschaft gilt bei einem Workspace-Filter `404`; ohne Filter (gesamte Organisation) antworten Mitglieder und Gäste mit `403`.

| Aktion | Org-Owner/Admin | WS-Admin als Org-Mitglied | WS-Editor | WS-Viewer | Org-Mitglied ohne WS-Adminrolle | Gast | Operator ohne Org-Mitgliedschaft | Fremde Organisation |
|---|---|---|---|---|---|---|---|---|
| Verwaltungsfilter und Sicherheitsstatus in der Personenliste (`MemberAdminList`) | ja | 403 | 403 | 403 | 403 | 403 | 404 | 404 |
| Organisationsfreigaben (`OrganizationGrantList`) | gesamte Organisation | nur eigene Admin-Workspaces | 403 | 403 | 403 | 403 (404 bei Workspace-Filter) | 404 | 404 |
| Organisationssperren (`OrganizationLockList`) | gesamte Organisation | nur eigene Admin-Workspaces | 403 | 403 | 403 | 403 (404 bei Workspace-Filter) | 404 | 404 |
| Fremde Geräte auflisten/widerrufen (`UserDeviceList`, `UserDeviceRevoke`) | nur mit zusätzlicher Operatorrolle | nur mit zusätzlicher Operatorrolle | nur mit zusätzlicher Operatorrolle | nur mit zusätzlicher Operatorrolle | nur mit zusätzlicher Operatorrolle | nur mit zusätzlicher Operatorrolle | ja | Instanzscope; kein Organisationsrecht |

Die bestehende Mitgliedersuche für Freigaben behält ihre Rechte und Ratenbegrenzung; sie erhält durch AC-08 keinen Zugriff auf 2FA- oder Kontostatus. Geräteantworten verwenden eine opaque Verwaltungsreferenz statt der internen Sitzungs-ID. Der Operatorwiderruf verwendet dieselbe atomare Bereinigung der Sitzungssperren wie der eigene Gerätewiderruf, protokolliert aber den Operator als Akteur und die opaque Verwaltungsreferenz des Geräts (`device_ref`) in den Audit-Details, nie die Sitzungs-ID. Abgelehnte Versuche nennen das Zielkonto, sobald es sich als UUID lesen lässt. Organisationsadmins sehen keine fremden Gerätenamen. Sperrenlisten nennen ausschließlich die Sitzungsart `browser` oder `device`. Die Einzelmutationen für Rollen, Mitgliedschaften, Einladungen, Freigaben und Force-Unlock behalten ihre bestehenden Policy-Prüfungen und Audit-Regeln.

Entscheidungen und Vertragspräzisierungen: [ADR-0024](../09-decisions/adr-0024-admin-console.md). Rollenmatrix, HTTP- und Datenbankprüfungen sowie Browserbelege werden im AC-08-Abnahmebericht ausgewiesen; dieser Abschnitt allein ist kein Testnachweis.

## Allgemeine Negativtests

Für **jeden** Endpunkt: anonymous, disabled user, member another org, viewer -> write, editor -> role change, stale role from cache, deleted project, revoked invite, expired session. Responses für fremde Daten sollen keine Existenzinformation preisgeben; je nach API-Konvention 404 statt 403 verwenden.
