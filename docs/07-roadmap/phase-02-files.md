# Phase 02 — Dateiablage, Versionierung, Konfliktlogik

> **Code:** P2. **Goal:** Preserve data under concurrency and failures.  
> **Status:** Tickets CC-P02-01 bis -08 umgesetzt, unabhängig geprüft und gemerged (PR #10 bis #15). Alle vier Exit-Gates sind mit Tests belegt (siehe unten). P2 ist abgeschlossen (2026-10-29). Offen bleiben: echtes Plattenvoll (nur Fehlerinjektion), unix-Pfade, ein echter Stromausfall; siehe `docs/gaps.md`.

## Eintrittsvoraussetzungen

Vorherige Phasen-Gates sind nachweislich erfüllt; Owner, Testumgebung, CI und Migrationsstrategie stehen fest. Keine Abkürzung bei Threat Review oder API-Vertrag.

## Umsetzungstickets

- [x] **CC-P02-01** — Project/File metadata + safe filename/path handling (CC-F06)
  - Status: Umsetzung lokal vorhanden (PR-008, gemergt als #10): Projekte und Dateimetadaten mit Soft-Delete/Restore, Namensvalidierung und Eindeutigkeit, Paginierung, Rollenmatrix, IDOR-Tests. Nicht enthalten: Ordner, Dateiinhalte, endgültiges Löschen. Hinweis für CC-P02-07: Ein Abbruch während des Veröffentlichens kann ein nicht referenziertes Objekt oder eine `.part`-Datei hinterlassen; die Abstimmung muss nicht referenzierte Objekte (älter als eine Karenzzeit) und alte `.part`-Dateien löschen.
- [x] **CC-P02-02** — Stream upload session, quota reservation, bounded memory, SHA-256 verify (CC-F07)
  - Status: Umsetzung lokal vorhanden (PR-010, gemergt als #12): Upload-Sitzungen mit Quota-Reservierung, Streaming mit Byte-Limit und SHA-256, Idempotenz, Obergrenzen, Ablauf, Rechteprüfung vor dem Speichern, Tests zu Kollision und Nebenläufigkeit (parallele Reservierungen, parallele PUTs, abgestufte Rolle während des Uploads, gelöschte Datei, Ablauf, Plattenvoll per Fehlerinjektion), 64-MiB-HTTP-Test (1-GiB-Test manuell mit `CC_LARGE_TESTS=1`).
- [x] **CC-P02-03** — local FS storage adapter and fault-injection tests (CC-F09)
  - Status: Umsetzung lokal vorhanden (PR-009, gemergt als #11): `craftcloud-storage` mit Fehlerinjektion (Fehler bei Byte N, fsync, Rename, volles Gerät), Abbruch-/Drop-Tests, Nebenläufigkeit, Range-Randfälle, 1-GiB-Streaming-Test (manuell, `#[ignore]`). Echte ENOSPC-Fälle sind nur über die Fehlerabbildung getestet.
- [x] **CC-P02-04** — Immutable blobs, version commit with CAS/ETag and conflict codes (CC-F08)
  - Status: Umsetzung lokal vorhanden (PR-011, gemergt als #13): Migration 0009, Commit mit CAS, 412-Konfliktantwort, Restore, Idempotenz, Quota-Umbuchung reserved→committed genau einmal.
- [x] **CC-P02-05** — File download with ACL recheck and integrity verification
  - Status: Umsetzung lokal vorhanden (PR-011, gemergt als #13): Rechteprüfung pro Anfrage, sichere Header, Einzel-Range (206/416), SHA-256 beim Streamen mit zurückgehaltenem letzten Block, defekte Blobs werden markiert.
- [x] **CC-P02-06** — Soft delete, retention, quota accounting, GC leases (CC-F10)
  - Status: Umsetzung lokal vorhanden (PR-008 Soft-Delete/Aufbewahrung, PR-010 Quota-Buchhaltung, PR-012 Rest; gemergt als #10, #12, #14): endgültiges Löschen von Papierkorb-Einträgen und alten Versionen (`CRAFTCLOUD_VERSION_RETENTION_DAYS`), zweiphasige Blob-Garbage-Collection mit Status `deleting` (Migration 0010), Freigabe von `committed_bytes` genau einmal beim Übergang nach `deleting`, Wartungsjob in-process plus `craftcloud-server admin maintenance --once [--dry-run]`, Advisory Lock, `GET /api/v1/admin/maintenance`. Die „Lease“ ist die Neuprüfung unter Organisations- und Blob-Sperre plus der Zwischenstatus `deleting`; es gibt keine Lease-Token für laufende Downloads (auf unix bleibt ein offener Download lesbar, auf Windows scheitert das Löschen und wird wiederholt).
- [x] **CC-P02-07** — Reconciliation job for orphaned uploads and blobs
  - Status: Umsetzung lokal vorhanden (PR-012, gemergt als #14): abgelaufene Upload-Sitzungen aller Organisationen schließen, Objekte ohne Blob-Zeile und `.part`-Dateien älter als Upload-TTL + 1 h löschen, committete Blobs mit fehlendem Objekt melden (Metadaten bleiben, Zähler und Log), Integritätsprüfung (SHA-256, älteste Prüfung zuerst, Byte-Budget je Lauf; `corrupt` wird bei passender Prüfsumme wieder `committed`), Korrektur abweichender Zähler. Prozessabsturz-Tests folgen in CC-P02-08.
- [x] **CC-P02-08** — Two-writer concurrency, partial upload and full disk tests
  - Status: Umsetzung lokal vorhanden (PR-013, PR #15): Prozessabsturz-Tests gegen das echte `craftcloud-server`-Binary (`apps/craftcloud-server/tests/crash.rs`, `cargo xtask db-test`). Das Binary wird mit dem Feature `fault-injection` gebaut und bricht über `CC_FAULT_ABORT_AT=<Punkt>[:<Fortschritt>]` per `std::process::abort()` an benannten Punkten ab (`upload-mid-stream`, `upload-before-record`, `version-after-commit`, `gc-after-claim`); ohne das Feature ist der Code nicht im Binary. Pro Szenario: Server bis zum Absturz treiben, normal neu starten, dann Version, Download-Prüfsumme, Zähler, Objekte und Wartung prüfen. Zwei-Schreiber-412 und Quota-Nebenläufigkeit waren schon bewiesen (CC-P02-04, CC-P02-02) und wurden nicht dupliziert. Echtes Plattenvoll (ENOSPC) ist nicht getestet: es bleibt bei der Fehlerinjektion (507-Antwort), weil auf dem Windows-Entwicklungsrechner kein Datenträger mit festem Limit ohne Administratorrechte und ohne VHD/Partition möglich ist.

## Exit-Gates / Abnahme

- [x] Two users same base → exactly one commit, one 412
  - Beleg: `two_writers_on_one_base_yield_one_version_and_one_412_and_the_loser_can_save_a_copy`, `parallel_commits_on_the_same_base_have_exactly_one_winner` (`crates/craftcloud-api/tests/versions.rs`, `cargo xtask db-test`).
- [x] Crash or disk full does not show incomplete version
  - Beleg (PR-012, PR-013): ein Absturz zwischen den Phasen der Blob-Garbage-Collection (`a_crash_between_the_gc_phases_is_completed_by_the_next_run`) hinterlässt nur einen `deleting`-Marker, den der nächste Lauf beendet, nie eine Version ohne Blob (`concurrent_restores_and_a_run_never_leave_a_dangling_version`). Plattenvoll- und Abbruchfälle über Fehlerinjektion (`a_full_disk_answers_507_and_leaves_no_blob_no_object_and_no_reservation`, `cancelling_the_request_after_the_object_is_published_leaves_nothing_unaccounted`, `a_write_failing_at_byte_n_never_publishes_anything`); Prozessabsturz-Tests gegen das echte Server-Binary (PR-013, `apps/craftcloud-server/tests/crash.rs`): `killing_the_server_mid_upload_leaves_no_version_and_expiry_releases_the_reservation` (Abbruch nach 256 KiB in der `.part`-Datei), `killing_the_server_after_publishing_the_object_but_before_recording_it_leaves_no_version` (Objekt veröffentlicht, Blob-Transaktion nicht committet; nach der Karenzzeit Quarantäne), `killing_the_server_after_the_commit_but_before_the_response_keeps_a_complete_durable_version` (Commit dauerhaft, Wiederholung des Commits möglich), `killing_maintenance_between_the_gc_claim_and_the_object_delete_is_finished_by_the_next_run`, `a_client_that_drops_the_connection_mid_upload_leaves_no_version_and_releases_the_reservation`. Echtes Plattenvoll nur über Fehlerinjektion, nicht mit einem vollen Datenträger.
- [x] Download checksum matches committed upload
  - Beleg: `a_64_mib_file_round_trips_with_a_matching_checksum` (Prüfsumme des Downloads gleich der des Uploads, `Repr-Digest` gleich), `a_corrupted_object_aborts_the_download_and_marks_the_blob`, `downloads_carry_the_safe_headers_and_the_exact_bytes`.
- [x] Quota cannot be bypassed through concurrent reservations
  - Beleg (PR-010): `parallel_reservations_can_never_exceed_the_quota` (30 parallele Reservierungen gegen 1000 Bytes: genau 10), `quotas_block_at_the_exact_boundary_and_lowering_never_deletes`.

P2 abgeschlossen am 2026-10-29 (lokaler Prototyp); Nachweise in `cargo xtask db-test` (208 Tests) und den genannten Testnamen.

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
