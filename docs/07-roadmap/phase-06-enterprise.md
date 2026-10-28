# Phase 06 — SSO, S3, Suche, Enterprise-Funktionen

> **Code:** P6. **Goal:** Make larger installations maintainable.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P06-01** — OIDC Authorization Code/PKCE identity and group policy (CC-F19)
- [ ] **CC-P06-02** — S3-compatible storage adapter with same contract tests (CC-F09)
- [ ] **CC-P06-03** — metadata search, scoped indexing and safe previews (CC-F21)
- [ ] **CC-P06-04** — API tokens/service accounts with scoped credentials (CC-F22)
- [ ] **CC-P06-05** — Privacy export/erase retention, optional at-rest encryption policy (CC-F20)
- [ ] **CC-P06-06** — Performance/load, backups for large volumes and migration rehearsal
- [ ] **CC-P06-07** — Monitoring dashboards and operator upgrade paths

## Exit-Gates / Abnahme

- [ ] OIDC subject binding is safe against email reassignment
- [ ] S3 and filesystem pass identical integrity/security tests
- [ ] Tenant queries cannot enumerate other organizations
- [ ] Large volume restore completed in documented period

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

E2EE is a separate research program; do not falsely promise it.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
