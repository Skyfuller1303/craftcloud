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

## Negativtests

Für **jeden** Endpunkt: anonymous, disabled user, member another org, viewer -> write, editor -> role change, stale role from cache, deleted project, revoked invite, expired session. Responses für fremde Daten sollen keine Existenzinformation preisgeben; je nach API-Konvention 404 statt 403 verwenden.
