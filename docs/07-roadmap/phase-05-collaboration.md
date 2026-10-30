# Phase 05 — FigCraft Adapter und optionale Launcher-Einstellungen

> **Code:** P5. **Goal:** Integrate with existing ecosystem rather than duplicating.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

> **Neuplanung 2026-10-30:** Die Tickets dieser Phase werden über die Briefings in [docs/12-app-cloud](../12-app-cloud/README.md) umgesetzt (AC-03, AC-06, AC-07). Die Exit-Gates unten gelten weiter.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P05-01** — FigCraft server/protocol workshop and version compatibility matrix
- [ ] **CC-P05-02** — Choose authorization/capability architecture and revocation SLA
- [ ] **CC-P05-03** — Integration prototype Editor vs Viewer vs Revoke with synthetic docs (CC-F16)
- [ ] **CC-P05-04** — Cross-server audit association and duplicate file ownership model
- [ ] **CC-P05-05** — Optional link-sharing policy gated on threat-review (CC-F18)
- [ ] **CC-P05-06** — Launcher settings RFC with Rust-only constraint (CC-F17)
- [ ] **CC-P05-07** — Separate FigCraft and Launcher PRs with individual repository checks

## Exit-Gates / Abnahme

- [ ] Existing FigCraft links and self-hosting unaffected
- [ ] Viewer edit rejected and role removal takes effect
- [ ] Launcher opens apps without Cloud server
- [ ] Protocol incompatibility produces actionable error

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

Collab adapter only after FigCraft maintainers approve security changes.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
