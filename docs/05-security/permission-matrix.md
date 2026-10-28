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

## Negativtests

Für **jeden** Endpunkt: anonymous, disabled user, member another org, viewer -> write, editor -> role change, stale role from cache, deleted project, revoked invite, expired session. Responses für fremde Daten sollen keine Existenzinformation preisgeben; je nach API-Konvention 404 statt 403 verwenden.
