# Phase 04 — CraftCloud SDK und erste Craft-App

> **Code:** P4. **Goal:** Validate actual desktop interop and local-first.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

> **Neuplanung 2026-10-30:** Die Tickets dieser Phase werden über die Briefings in [docs/12-app-cloud](../12-app-cloud/README.md) umgesetzt (AC-01 bis AC-05). Die Exit-Gates unten gelten weiter.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P04-01** — Client transport interface and core DTOs; version-discovery contract (CC-F13)
- [ ] **CC-P04-02** — Secure Desktop user authorization and token storage ADR
- [ ] **CC-P04-03** — Typed streaming upload/download with idempotency/CAS
- [ ] **CC-P04-04** — Mock server + real integration tests across errors and retries
- [ ] **CC-P04-05** — Choose one app with maintainer; add Open/Save Copy opt-in flow
- [ ] **CC-P04-06** — Offline local-save + conflict UX + token expiry flow
- [ ] **CC-P04-07** — Platform/WASM compatibility matrix and accessibility review
- [ ] **CC-P04-08** — Separate cross-repo PR, maintainers review

## Exit-Gates / Abnahme

- [ ] Native app starts and edits locally with cloud disabled
- [ ] App saves file copy to server and retrieves byte-identical bytes
- [ ] Conflict preserves local copy and informs user
- [ ] Server unavailable does not prevent local save

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

No forced accounts, no global launcher integration yet.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
