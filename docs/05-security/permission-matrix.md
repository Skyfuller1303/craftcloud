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
| Workspace lesen / verwalten (Routen mit PR-007) | 404 | 404 | nur mit Workspace-Rolle (`admin` verwaltet) | ja | ja |

Der „letzte Owner“ zählt nur aktive Mitgliedschaften mit aktiviertem Konto; ein deaktivierter Owner zählt nicht. Das Deaktivieren eines Kontos wird bewusst nie durch diese Regel blockiert (Sicherheitsreaktion), der Operator erhält die Liste der betroffenen Organisationen, und der Hostbetreiber stellt einen Owner mit `craftcloud-server admin set-org-owner` wieder her (Break-glass, siehe `docs/06-operations/deployment.md`). Ein `OrgScope` merkt sich die erlaubte Aktion: schreibende Repository-Funktionen verweigern einen Scope aus einer Lese-Aktion.

Zugehörige Tests: `crates/craftcloud-core/src/policy.rs` (Tabellentests über alle Aktionen und Rollen) und `crates/craftcloud-api/tests/orgs.rs` (Isolation, Rollenregeln, Letzter-Owner-Race, CSRF, Ablauf).

## Negativtests

Für **jeden** Endpunkt: anonymous, disabled user, member another org, viewer -> write, editor -> role change, stale role from cache, deleted project, revoked invite, expired session. Responses für fremde Daten sollen keine Existenzinformation preisgeben; je nach API-Konvention 404 statt 403 verwenden.
