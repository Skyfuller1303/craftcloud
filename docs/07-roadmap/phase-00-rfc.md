# Phase 00 — RFC, Projektfreigabe und Architekturentscheidungen

> **Code:** P0. **Goal:** Maintainer approval, scope and governance.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Projektvorschlag und öffentlich zugängliche Repository-/Issue-Quellen genügen für RFC; echte Coding-Implementierung braucht die genannten Maintainer-/Lizenz-Entscheidungen.

## Umsetzungstickets

- [ ] **CC-P00-01** — Abgleich der bestehenden CraftCloud-/DriveCraft-/Nextcloud-Issues, Overlap-Landkarte finalisieren
- [ ] **CC-P00-02** — Maintainer-RFC unter org-übergreifendem ArtCraft-Issue einreichen; keine Repo im Namen von storytold ohne Zustimmung
- [ ] **CC-P00-03** — Zugriff/Standards aus craftrules erfragen und Upstream-Compliance-Check aktualisieren
- [ ] **CC-P00-04** — Lizenz-/Markenfreigabe und Clean-room-Grenzen definieren
- [ ] **CC-P00-05** — API-Ownership, Client-SDK-Governance und erste Craft-App priorisieren
- [ ] **CC-P00-06** — ADRs zu Single-node/Postgres/React/Admin/FS/Session/Versioning reviewen
- [ ] **CC-P00-07** — threat model v1; Review mit Backend+FigCraft-Maintainern

## Exit-Gates / Abnahme

- [ ] RFC-Feedback dokumentiert
- [ ] Entscheidung über Repo-Namen und Lizenz nicht nur implizit
- [ ] Risiko-Register und klare Out-of-scope-Liste
- [ ] App-Maintainer für Pilot benannt

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

Keine Entwicklung in einer offiziellen Org, bevor Eigentum, Lizenz und Schnittstellen geklärt sind.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
