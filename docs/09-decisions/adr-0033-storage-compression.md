# ADR-0033: Kompression dateigebundener Speicherblöcke

- **Status:** Lokal implementiert und geprüft; unabhängig geprüft, CI und PostgreSQL-Gate bestanden
- **Datum:** 2026-11-03

## Kontext

AC-15 speichert dateigebundene Chunks mit Klartext-SHA-256 und HMAC-Kennung. Bereits intern komprimierte Formate sparen wenig; PSD, TIFF, SVG, Text und Rohdaten profitieren häufig deutlich. Kompression darf weder HTTP-Bytes noch logische Quoten, Chunk-Kennungen, Archive, Backups oder Deduplizierung verändern.

## Entscheidung

Nur neue Chunks erhalten eine physische Kodierung: `raw` oder zstd. zstd-Stufe 3 ist Standard; `storage.compression_level` erlaubt 0 bis 19, und `CRAFTCLOUD_STORAGE_COMPRESSION_LEVEL` hat Vorrang. Stufe 0 erzeugt neue Roh-Chunks. Ein zstd-Objekt wird nur übernommen, wenn es mindestens 5 Prozent kleiner als Klartext ist. `size_bytes` bleibt logische Länge; `stored_bytes` misst gespeicherte Chunks. Ganzblob-Versionen bleiben unverändert.

Klartext-SHA-256 und file-scoped HMAC bleiben Identitäts- und Integritätswerte. Beim Schreiben wird jeder kodierte Chunk vor Veröffentlichung dekodiert und gegen den Klartextdigest geprüft. Jeder Leser dekodiert genau einen Frame, verwirft konkatenierte Frames, begrenzt Fenster und Ausgabe auf die logische Chunklänge und prüft danach SHA-256. Kompressions- und Dekompressionsarbeiten teilen zwei Blocking-Pool-Slots; der Semaphore-Permit bleibt bis Abschluss der Closure gehalten.

Quoten bleiben logisch. Physische Nutzungswerte summieren gespeicherte Längen. Archivierung und Backup kopieren kodierte Bytes unverändert; Prüfung und Restore müssen nach Dekodierung die Klartextdigests und zusammengesetzten Versionsdigests bestätigen.

Backupformat v4 ergänzt die Kodierung und gespeicherte Blockgröße; v1 bis v3 bleiben lesbar. Ersetzte Archivkennungen erhalten über transaktionale Trigger eine dauerhafte Retirement-Zuständigkeit. Erst nach Schonfrist, vollständigem Archivinventar und Ausschluss aktueller sowie gesicherter Referenzen darf die Wartung sie entfernen. Ein begrenzter erster Schritt vor gewöhnlicher GC verhindert dauerhafte Verdrängung. Die Warteschlange gehört zu Backup und Restore; ihre Bytes zählen ohne Doppelzählung bis zur Bereinigung weiter.

## Rollout und Rücknahme

Migration 0027 markiert bestehende Chunks als `raw` und setzt `stored_bytes=size_bytes`. Während eines Rollouts dürfen keine alten und neuen Serverprozesse gleichzeitig schreiben: alte Binaries kennen `encoding=zstd` nicht. Vor Rollback müssen alle zstd-Chunks in Roh-Chunks zurückgeschrieben oder aus einem kompatiblen Backup restauriert werden. Stufe 0 stoppt nur neue Kompression; sie wandelt bestehende Chunks nicht um.

## Alternativen und Risiken

Whole-Kompression wurde verworfen, weil sie Range-Lesen und Einzelchunk-Restore verteuert. Eine Kompression ohne Schwellenwert wurde verworfen, weil bereits komprimierte App-Formate sonst wachsen können. zstd 0.13.3 ist eine neue Rust-Abhängigkeit (MIT); `zstd-safe` 7.4.0 und `zstd-sys` 2.1.2+zstd.1.5.7 tragen BSD-3-Clause. Gewählt für sichere Frame-Grenzen und performante Stufe 3 ohne eigene Codec-Implementierung.

Ein reales 340.357.213-Byte-PSD-Beispiel ergab bei zstd-Stufe 3 236.335.428 Bytes (69,44 Prozent) in 3,04 Sekunden; Stufe 19 ergab 201.880.806 Bytes (59,31 Prozent) in 115,36 Sekunden. Stufe 3 bleibt Standard: deutlich geringere Upload-CPU bei weiterhin messbarer Einsparung.
