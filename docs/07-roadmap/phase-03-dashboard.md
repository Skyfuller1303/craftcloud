# Phase 03 — Web-Admin, Betrieb und Restore

> **Code:** P3. **Goal:** A self-hosted service usable without CLI.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P03-01** — React/TS admin app scaffolding and typed API client (CC-F12)
- [ ] **CC-P03-02** — Setup, login and organization/workspace selector UI
- [ ] **CC-P03-03** — Members, invites, roles, projects, file upload/download/version views
- [ ] **CC-P03-04** — 401/403/412/429/503/error/retry UX, a11y test
- [ ] **CC-P03-05** — Audit log + activity filtering with retention (CC-F11)
- [ ] **CC-P03-06** — Structured logs, health, metrics, PII redaction (CC-F15)
- [ ] **CC-P03-07** — Docker Compose with nonroot server, proxy TLS, secrets/volumes
- [ ] **CC-P03-08** — Consistent DB/blob backup + restore with checksums (CC-F14)
- [ ] **CC-P03-09** — Security review and external operator setup test

## Exit-Gates / Abnahme

- [ ] Fresh external tester provisions server from docs and manages team via web
- [ ] Backup restored on separate instance with ACL/version byte checks
- [ ] Admin UI keyboard operable and responsive
- [ ] No token leaks in logs/URLs/browser storage

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

No managed cloud / commercial SLA claims.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
