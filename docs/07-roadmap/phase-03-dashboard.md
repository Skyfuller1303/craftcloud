# Phase 03 — Web-Admin, Betrieb und Restore

> **Code:** P3. **Goal:** A self-hosted service usable without CLI.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P03-01** — React/TS admin app scaffolding and typed API client (CC-F12)
  - Status: Umsetzung lokal vorhanden (PR-014, nicht geprüft, nicht gemergt; Kästchen bleibt offen bis zum Review): `apps/craftcloud-admin` (Vite, React 19, TypeScript strict, React Aria Components, eigenes SCSS mit Design-Tokens als CSS Custom Properties, Hell/Dunkel/System, reduzierte Bewegung). Handgeschriebener, typisierter API-Client (`src/api/`), dessen genutzte Pfade und Methoden ein Test gegen `contracts/openapi.yaml` prüft; Cookies same-origin, CSRF-Header bei unsicheren Methoden, Token nur im Arbeitsspeicher. TSV-Kataloge `en.tsv`/`de.tsv` mit Parser und `npm run i18n:check`. Auslieferung durch `craftcloud-server` unter `/admin` (`CRAFTCLOUD_ADMIN_DIR`), `cargo xtask web` und Web-Schritt in `cargo xtask ci`. Nicht enthalten: generierter Client aus OpenAPI, Ansichten aus CC-P03-03.
- [ ] **CC-P03-02** — Setup, login and organization/workspace selector UI
  - Status: Umsetzung lokal vorhanden (PR-014, nicht geprüft, nicht gemergt; Kästchen bleibt offen bis zum Review): Ersteinrichtung (Bootstrap-Token, Operator, Organisation), Anmeldung und Abmeldung, Annehmen von Einladungen (Token im URL-Fragment), Auswahl von Organisation und Workspace, Navigationsrahmen mit Platzhaltern für CC-P03-03, Sprach- und Darstellungsumschalter. Komponententests mit gemocktem `fetch` (401/403/429/503, Tastaturfokus), axe-core-Prüfung je Ansicht in jsdom (ohne Farbkontrast; Kontrast wird aus den Tokens berechnet). Nicht enthalten: Passwort-Reset-Seite, Gerätelisten, Organisations-/Workspace-Anlage; echter Browser-Test mit Screenreader steht aus.
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
