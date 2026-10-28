# Phase 02 — Dateiablage, Versionierung, Konfliktlogik

> **Code:** P2. **Goal:** Preserve data under concurrency and failures.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P02-01** — Project/File metadata + safe filename/path handling (CC-F06)
- [ ] **CC-P02-02** — Stream upload session, quota reservation, bounded memory, SHA-256 verify (CC-F07)
- [ ] **CC-P02-03** — local FS storage adapter and fault-injection tests (CC-F09)
- [ ] **CC-P02-04** — Immutable blobs, version commit with CAS/ETag and conflict codes (CC-F08)
- [ ] **CC-P02-05** — File download with ACL recheck and integrity verification
- [ ] **CC-P02-06** — Soft delete, retention, quota accounting, GC leases (CC-F10)
- [ ] **CC-P02-07** — Reconciliation job for orphaned uploads and blobs
- [ ] **CC-P02-08** — Two-writer concurrency, partial upload and full disk tests

## Exit-Gates / Abnahme

- [ ] Two users same base → exactly one commit, one 412
- [ ] Crash or disk full does not show incomplete version
- [ ] Download checksum matches committed upload
- [ ] Quota cannot be bypassed through concurrent reservations

## Test- und Review-Artefakte

- Relevantes `docs/03-features/CC-Fxx`-Briefing als Quelle, API-Contract-Diff und Änderungs-ADR.
- Testprotokoll mit realen Kommandos und Ergebnissen (erfolgreich/fehlgeschlagen/nicht ausgeführt) statt behaupteter grüner Tests.
- Rollen-/Tenant-Matrix und negative Tests für jeden neuen Endpoint.
- Migrations-, Rollback- und Recovery-Auswirkung, insbesondere bei persistenten Dateien.
- Screenshots oder Video nur falls UI betroffen und nur mit originären, lizenzierten Assets und synthetischen Daten.

## Abgrenzung

Kein Content merge, kein Finder-/Explorer-Sync, keine S3-Pflicht.

## Übergabe an nächste Phase

Übergabeprotokoll: Datenmodell, API/SDK-Versionen, offene Risiken, Testmatrix, Betriebsstatus und Verantwortliche. Erst weitergehen, wenn die genannte Abnahme überprüft und verlinkt wurde.
