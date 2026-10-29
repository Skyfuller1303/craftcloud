# Phase 02 — Dateiablage, Versionierung, Konfliktlogik

> **Code:** P2. **Goal:** Preserve data under concurrency and failures.  
> **Status:** geplant / offen, keine abgeschlossenen Arbeiten behauptet.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [ ] **CC-P02-01** — Project/File metadata + safe filename/path handling (CC-F06)
  - Status: Umsetzung lokal vorhanden (PR-008, Review offen): Projekte und Dateimetadaten mit Soft-Delete/Restore, Namensvalidierung und Eindeutigkeit, Paginierung, Rollenmatrix, IDOR-Tests. Nicht enthalten: Ordner, Dateiinhalte, endgültiges Löschen. Hinweis für CC-P02-07: Ein Abbruch während des Veröffentlichens kann ein nicht referenziertes Objekt oder eine `.part`-Datei hinterlassen; die Abstimmung muss nicht referenzierte Objekte (älter als eine Karenzzeit) und alte `.part`-Dateien löschen. Das Ticket wird nach der Übergabe angehakt.
- [ ] **CC-P02-02** — Stream upload session, quota reservation, bounded memory, SHA-256 verify (CC-F07)
  - Status: Umsetzung lokal vorhanden (PR-010, Review offen): Upload-Sitzungen mit Quota-Reservierung, Streaming mit Byte-Limit und SHA-256, Idempotenz, Obergrenzen, Ablauf, Rechteprüfung vor dem Speichern, Tests zu Kollision und Nebenläufigkeit (parallele Reservierungen, parallele PUTs, abgestufte Rolle während des Uploads, gelöschte Datei, Ablauf, Plattenvoll per Fehlerinjektion), 64-MiB-HTTP-Test (1-GiB-Test manuell mit `CC_LARGE_TESTS=1`). Das Ticket wird nach der Übergabe angehakt.
- [ ] **CC-P02-03** — local FS storage adapter and fault-injection tests (CC-F09)
  - Status: Umsetzung lokal vorhanden (PR-009, Review offen): `craftcloud-storage` mit Fehlerinjektion (Fehler bei Byte N, fsync, Rename, volles Gerät), Abbruch-/Drop-Tests, Nebenläufigkeit, Range-Randfälle, 1-GiB-Streaming-Test (manuell, `#[ignore]`). Echte ENOSPC-Fälle sind nur über die Fehlerabbildung getestet. Das Ticket wird nach der Übergabe angehakt.
- [ ] **CC-P02-04** — Immutable blobs, version commit with CAS/ETag and conflict codes (CC-F08)
  - Status: Umsetzung lokal vorhanden (PR-011, Review offen): Migration 0009, Commit mit CAS, 412-Konfliktantwort, Restore, Idempotenz, Quota-Umbuchung reserved→committed genau einmal. Das Ticket wird nach der Übergabe angehakt.
- [ ] **CC-P02-05** — File download with ACL recheck and integrity verification
  - Status: Umsetzung lokal vorhanden (PR-011, Review offen): Rechteprüfung pro Anfrage, sichere Header, Einzel-Range (206/416), SHA-256 beim Streamen mit zurückgehaltenem letzten Block, defekte Blobs werden markiert. Das Ticket wird nach der Übergabe angehakt.
- [ ] **CC-P02-06** — Soft delete, retention, quota accounting, GC leases (CC-F10)
  - Status: Teilweise (PR-008 Soft-Delete/Aufbewahrung, PR-010 Quota-Buchhaltung). Offen: endgültiges Löschen, Garbage Collection mit Leases, Freigabe beim Löschen committeter Versionen.
- [ ] **CC-P02-07** — Reconciliation job for orphaned uploads and blobs
- [ ] **CC-P02-08** — Two-writer concurrency, partial upload and full disk tests

## Exit-Gates / Abnahme

- [x] Two users same base → exactly one commit, one 412
  - Beleg: `two_writers_on_one_base_yield_one_version_and_one_412_and_the_loser_can_save_a_copy`, `parallel_commits_on_the_same_base_have_exactly_one_winner` (`crates/craftcloud-api/tests/versions.rs`, `cargo xtask db-test`).
- [ ] Crash or disk full does not show incomplete version
  - Teilbeleg: Plattenvoll- und Abbruchfälle über Fehlerinjektion (`a_full_disk_answers_507_and_leaves_no_blob_no_object_and_no_reservation`, `cancelling_the_request_after_the_object_is_published_leaves_nothing_unaccounted`, `a_write_failing_at_byte_n_never_publishes_anything`); Prozessabsturz-Tests (CC-P02-08) fehlen noch, deshalb nicht angehakt.
- [x] Download checksum matches committed upload
  - Beleg: `a_64_mib_file_round_trips_with_a_matching_checksum` (Prüfsumme des Downloads gleich der des Uploads, `Repr-Digest` gleich), `a_corrupted_object_aborts_the_download_and_marks_the_blob`, `downloads_carry_the_safe_headers_and_the_exact_bytes`.
- [x] Quota cannot be bypassed through concurrent reservations
  - Beleg (PR-010): `parallel_reservations_can_never_exceed_the_quota` (30 parallele Reservierungen gegen 1000 Bytes: genau 10), `quotas_block_at_the_exact_boundary_and_lowering_never_deletes`.

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
