# Phase 07 — Pilot, Security Review und 1.0-Kandidatur

> **Code:** P7. **Goal:** Get real external evidence and an operable product.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P07-01** — RC freeze of API contract and schema migration plan
- [ ] **CC-P07-02** — Threat review, pentest plan, dependency/SBOM and release signing
- [ ] **CC-P07-03** — Pilot with 2–3 test organizations and explicit synthetic/non-sensitive projects
- [ ] **CC-P07-04** — Restore drill, chaos test, incident tabletop
- [ ] **CC-P07-05** — Docs, FAQs, contribution and migration support matrix
- [ ] **CC-P07-06** — Agree license, trademark and ownership; release assets checksums
- [ ] **CC-P07-07** — Maintainer sign-off and publish only after measured gates

## Exit-Gates / Abnahme

- [ ] No critical unresolved security/data-loss bugs
- [ ] Backup recovery independently reproduced
- [ ] No mandatory cloud login regressions
- [ ] Support policy and operational limits honestly disclosed

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

Pilot sign-off is a decision; do not automatically publish 1.0 at milestone completion.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
