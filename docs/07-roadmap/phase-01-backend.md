# Phase 01 — Backend-Kern: Identität, Mandanten, Rechte

> **Code:** P1. **Goal:** Make a securely bootstrapped API service.  
> **Status:** Tickets CC-P01-01 bis -07 und -09 umgesetzt, unabhängig geprüft und gemerged (PR #1 bis #7; CC-P01-09 mit dem Vertragstest-PR), CC-P01-08 teilweise (Einladungen ja, E-Mail-Verifikation nein). Alle vier Exit-Gates sind mit Tests bzw. dem Smoke-Lauf belegt (siehe unten). P1 gilt erst als abgeschlossen, wenn CC-P01-08 geklärt ist (Verifikation umsetzen oder per ADR zurückstellen). Bekannte Lücken stehen je Ticket und in `docs/gaps.md`.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [x] **CC-P01-01** — Rust Cargo workspace und Leerlauf-Service mit strikter Lint-Policy aufsetzen
  - Status: umgesetzt, geprüft, gemerged (PR #1). Workspace mit `unsafe_code = "forbid"`, Clippy-Verbote für `unwrap`/`expect`/`panic`/Indexing, `cargo xtask ci` (fmt, clippy `-D warnings`, Tests, Doku-Links); MSRV 1.99 entspricht der Toolchain (ADR-0010).
  - Lücken: keine bekannt. CI läuft bisher nur lokal über `cargo xtask ci`, es gibt keinen gehosteten CI-Job.
- [x] **CC-P01-02** — Konfigurationsvalidator und Secret-Dateien, Startup-/Shutdown-Lifecycle
  - Status: umgesetzt, geprüft, gemerged (PR #2). Typisierte Env-Konfiguration mit Fehlerliste, `*_FILE`-Secrets (Datenbank-URL, Bootstrap-Secret) werden beim Start geladen und geprüft, Trusted-Proxy-Liste, JSON-Fehler für 413/Timeout, begrenzter Shutdown; siehe ADR-0011.
  - Lücken: Unix-spezifische Pfade (Öffnen von Secret-Dateien mit `O_NOFOLLOW`, Rechteprüfung) sind auf dem Windows-Entwicklungsrechner nicht ausgeführt worden, nur kompiliert für die Nicht-Unix-Variante.
- [x] **CC-P01-03** — PostgreSQL-Migrationssystem, UUID und tenant-scoped query layer
  - Status: umgesetzt, geprüft, gemerged (PR #3). Vorwärts-Migrationen 0001 bis 0006 (`craftcloud-server migrate`, nie implizit), zusammengesetzte Fremdschlüssel für die Mandantengrenze, `OrgScope`-Repositories, Docker-Integrationstests (`cargo xtask db-test`); siehe ADR-0012.
  - Lücken: keine Rollback-Migrationen (bewusst nur vorwärts); Backup/Restore gehört zu CC-F14.
- [x] **CC-P01-04** — one-shot Admin-Bootstrap mit Recovery-Runbook (CC-F01)
  - Status: umgesetzt, geprüft, gemerged (PR #4). Setup-Routen, Singleton-Transaktion, Argon2id, Offline-Wiederherstellung (`admin set-org-owner`) im Runbook.
  - Lücken: Der Operator-Account hat keine verifizierte E-Mail-Adresse (siehe CC-P01-08).
- [x] **CC-P01-05** — Auth, Sessions, Logout/Revocation, CSRF/Rate-Limiting (CC-F02)
  - Status: umgesetzt, geprüft, gemerged (PR #5). Login, serverseitige Cookie-Sitzungen (nur Hash gespeichert), CSRF-Header plus Origin-Prüfung, Rate-Limiting, Logout/Logout-all, Geräteliste, Passwortänderung, Reset-Links (Operator-Route und Offline-CLI).
  - Lücken: Ratenzähler liegen im Speicher (eine Instanz, Neustart leert sie); kein MFA; Desktop-Anmeldung (Device Flow) bleibt offen (D13); ohne SMTP werden Reset-Links nur vom Operator ausgegeben.
- [x] **CC-P01-06** — Organizations, Workspaces und Membership CRUD (CC-F04)
  - Status: umgesetzt, geprüft, gemerged (PR #6 Organisationen und Mitglieder, PR #7 Workspaces und Workspace-Mitglieder). Anlegen/Lesen/Ändern mit ETag, Mitglieder- und Rollenverwaltung, Konto-Deaktivierung, Paginierung.
  - Lücken: kein Löschen/Soft-Delete von Organisationen und Workspaces in P1 (bewusste, in `docs/gaps.md` geführte Lücke; Export- und Aufbewahrungspfad fehlen); das Ticket gilt für Anlegen, Lesen, Ändern und Mitgliedschaften als erfüllt.
- [x] **CC-P01-07** — zentrale Policy Engine + negative Tenant-Tests (CC-F05)
  - Status: umgesetzt, geprüft, gemerged (PR #6, erweitert in PR #7). Reine Funktion `craftcloud_core::policy::can` mit Tabellentests über alle 25 Aktionen und Rollen, `OrgScope` nur aus einem Permit; Negativ- und IDOR-Tests je Endpunktgruppe.
  - Lücken: Rollen werden pro Request aus der Datenbank gelesen (kein Cache, bewusst); fein granulare Projekt-/Datei-Aktionen kommen mit CC-F06.
- [ ] **CC-P01-08** — Invite-/E-Mail-Verifikation und safe invite acceptance (CC-F03)
  - Status: Einladungen umgesetzt, geprüft, gemerged (PR #7): Link-Einladungen (72 h, einmalig, widerrufbar, nur SHA-256 gespeichert), sichere Annahme mit Neubewertung der Rechte des Einladenden, Herkunft und Operator-Auflistung der Konten. **Nicht erfüllt:** E-Mail-Verifikation und -Versand; Linkbesitz gilt als Nachweis der Adresse (ADR-0013). Das Ticket bleibt offen.
  - Lücken: kein SMTP, keine Adressverifikation (Account-Squatting als Restrisiko, siehe Threat-Model); keine Browserseite `/invite`; keine Aufräumroutine für alte Einladungen.
- [x] **CC-P01-09** — Info/Health/Request-ID/Error schema und OpenAPI-Snapshot
  - Status: umgesetzt mit diesem Vertrags-PR. `crates/craftcloud-api/tests/contract.rs` vergleicht `contracts/openapi.yaml` mit der Routentabelle `ROUTES` und sondiert den Router (jede gelistete Methode wird bedient, jede andere antwortet 405, undokumentierte Pfade 404), prüft den normalisierten Snapshot `contracts/openapi.snapshot` (`cargo xtask openapi-snapshot [--update]`), dass jede Fehlerantwort das gemeinsame `Error`-Schema nutzt, und echte Fehlerantworten (404, 405, 413, 415, 422, 401, 503) samt `x-request-id` gleich `request_id` und `Cache-Control: no-store`.
  - Lücken: `/health/ready` existiert noch nicht (CC-F15) und ist bewusst nicht im Vertrag; die Routentabelle wird von Hand gepflegt (ein Test erkennt eine nicht eingetragene Route).

## Exit-Gates / Abnahme

- [x] Fresh install → secure admin → org/workspace/member vollständig via HTTP
  - Beleg: Smoke-Lauf `p1-e2e-smoke.log` (echter Server, echte PostgreSQL-Datenbank, nur HTTP: Setup, Operator legt Organisation an, lädt Admin ein, Admin legt Workspace an und lädt Mitglied ein, Annahme als neues Konto, Mitglied sieht den Workspace, Außenstehender 404); einzelne Schritte als Tests in `crates/craftcloud-api/tests/setup.rs`, `orgs.rs` und `workspaces_invites.rs` (`accepting_creates_the_account_membership_workspace_role_and_a_session`). Der Smoke-Lauf ist ein dokumentierter manueller Lauf außerhalb des Repositorys, kein CI-Test.
- [x] Cross-tenant und viewer/write Angriffe getestet
  - Beleg: `a_member_of_one_organization_sees_nothing_of_another`, `plain_members_cannot_write_and_non_members_do_not_exist_for_them`, `workspaces_never_cross_organizations_and_swapped_ids_find_nothing`, `workspace_member_role_matrix_including_workspace_admin_limits`, `who_may_invite_what`, Policy-Tabellentests in `crates/craftcloud-core/src/policy.rs`.
- [x] Login/CSRF/Revocation-Tests erfolgreich
  - Beleg: `every_login_failure_looks_identical`, `csrf_header_and_origin_are_both_required_for_unsafe_methods`, `logout_revokes_the_session_and_logout_all_revokes_every_session_of_the_user`, `issuing_a_link_revokes_the_sessions_of_that_account_immediately`, `every_unsafe_workspace_and_invitation_route_needs_session_csrf_and_origin`; Ausführung mit `cargo xtask db-test`.
- [x] Crash im Bootstrap hinterlässt keine teilfertigen Admins
  - Beleg: `failure_inside_the_transaction_leaves_nothing_behind` (`crates/craftcloud-api/tests/setup.rs`), `twenty_concurrent_requests_create_exactly_one_operator`, `concurrent_setups_produce_exactly_one_winner` (Datenbank-Integrationstest).

Hinweis: Die Gates belegen die Backend-Fähigkeiten. Offen für den Phasenabschluss bleiben CC-P01-08 (E-Mail-Verifikation) sowie die Lücken je Ticket; der Phasenwechsel verlangt eine ausdrückliche Entscheidung dazu.

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

Kein Blobstore oder echtes Adminpanel nötig, API-Integrationstests reichen als Abnahme.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
