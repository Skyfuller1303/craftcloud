# Phase 01 — Backend-Kern: Identität, Mandanten, Rechte

> **Code:** P1. **Goal:** Make a securely bootstrapped API service.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P01-01** — Rust Cargo workspace und Leerlauf-Service mit strikter Lint-Policy aufsetzen
  - Status: Umsetzung lokal vorhanden (Workspace, `/health/live`, `/api/v1/info`, `cargo xtask ci`), Review und Freigabe offen; MSRV 1.99 entspricht der lokalen Toolchain (ADR-0010).
- [ ] **CC-P01-02** — Konfigurationsvalidator und Secret-Dateien, Startup-/Shutdown-Lifecycle
  - Status: Umsetzung lokal vorhanden (Env-Konfiguration, Secret-Datei-Loader noch nicht verdrahtet, Trusted-Proxy, begrenzter Shutdown), Review offen; siehe ADR-0011.
- [ ] **CC-P01-03** — PostgreSQL-Migrationssystem, UUID und tenant-scoped query layer
  - Status: Umsetzung lokal vorhanden (Schema 0001/0002, `migrate`-Befehl, `OrgScope`-Repositories, Docker-Integrationstests), Review offen; siehe ADR-0012.
- [ ] **CC-P01-04** — one-shot Admin-Bootstrap mit Recovery-Runbook (CC-F01)
  - Status: Umsetzung lokal vorhanden (Setup-Routen, Singleton-Transaktion, Argon2id, Offline-Wiederherstellung dokumentiert), Review offen; kein Login (PR-005).
- [ ] **CC-P01-05** — Auth, Sessions, Logout/Revocation, CSRF/Rate-Limiting (CC-F02)
  - Status: Umsetzung lokal vorhanden (Login, Cookie-Sitzungen, CSRF+Origin, Rate-Limiting, Logout/Logout-all, Passwortänderung, Reset-Links inkl. Offline-CLI), Review offen.
- [ ] **CC-P01-06** — Organizations, Workspaces und Membership CRUD (CC-F04)
  - Status: Organisationen, Mitgliedschaften, Policy-Engine (RBAC) und Konto-Deaktivierung lokal umgesetzt, Review offen; Workspace-Routen folgen in PR-007 (siehe CC-P01-08); Organisation löschen bleibt Lücke.
- [ ] **CC-P01-07** — zentrale Policy Engine + negative Tenant-Tests (CC-F05)
- [ ] **CC-P01-08** — Invite-/E-Mail-Verifikation und safe invite acceptance (CC-F03)
  - Status: Umsetzung lokal vorhanden als PR-007 (Link-Einladungen, sichere Annahme, Workspaces); Review offen. E-Mail-Verifikation und Versand fehlen (Linkbesitz gilt als Nachweis, ADR-0013).
- [ ] **CC-P01-09** — Info/Health/Request-ID/Error schema und OpenAPI-Snapshot

## Exit-Gates / Abnahme

- [ ] Fresh install → secure admin → org/workspace/member vollständig via HTTP
- [ ] Cross-tenant und viewer/write Angriffe getestet
- [ ] Login/CSRF/Revocation-Tests erfolgreich
- [ ] Crash im Bootstrap hinterlässt keine teilfertigen Admins

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
