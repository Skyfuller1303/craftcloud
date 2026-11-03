# ADR-0032: Dateigebundene Chunk-Deduplizierung

- **Status:** Phase A und B implementiert und lokal abgenommen; unabhängige Reviews sowie vollständige sequenzielle CI und PostgreSQL-Prüfung bestanden (2026-11-03). Veröffentlichung steht aus.
- **Datum:** 2026-11-03

## Kontext

AC-12 legt unveränderliche Versionen als vollständige Blobs ab. Große Dateien ändern sich häufig nur lokal. Deduplizierung über Dateigrenzen wäre ein Inhalts-Existenzorakel und widerspräche der Mandantentrennung aus ADR-0004.

## Entscheidung

Version 1 verwendet FastCDC v2020 mit festen Grenzen von 256 KiB, Zielgröße 1 MiB und Maximum 4 MiB. Die kanonische Kennung wird aus dem Klartext-SHA-256 eines Blocks gebildet: HMAC-SHA-256 über einen versionierten Zweckstring, Datei-ID und Klartext-SHA-256 mit einem aus dem Instanz-Serverschlüssel abgeleiteten Schlüssel. Clients übermitteln in Phase B Klartext-Prüfsummen; ausschließlich der Server bildet daraus seine Kennungen. Clients erhalten keinen Serverschlüssel. Kennungen und Objekte sind auf genau eine Datei begrenzt.

Bestehende Blobs bleiben `whole`. Neue Chunk-Manifeste sind je Version geordnet; das Blob bleibt die logische Version und behält Gesamtgröße sowie Gesamt-SHA-256. Quoten bleiben logisch. `physical_bytes` ist eine zusätzliche Betreiberangabe.

Ohne konfigurierten Serverschlüssel bleibt der bisherige Whole-Blob-Pfad funktionsfähig. Bei Schlüsselrotation dürfen aktueller und vorheriger Schlüssel für die Suche nach schon vorhandenen Kennungen verwendet werden. Das Entfernen eines alten Schlüssels reduziert Wiederverwendung über diesen Schlüsselzeitraum, macht gespeicherte Objektkennungen oder Downloads nicht unlesbar.

Chunk-Referenzen werden transaktional mit dem Versionsmanifest geführt. Eine Datenbank-Trigger-Referenzzählung markiert nicht mehr referenzierte Chunks als GC-fähig, auch wenn Versionen über Wartung oder Kaskaden entfernt werden. GC prüft zusätzlich tatsächliche Versions-, Upload- und Backup-Referenzen und hält einen dauerhaften Löschzustand für Wiederanläufe. Ein bereits vorhandener Block wird vor dem Verwerfen eines verifizierten Uploadkandidaten auf Größe und SHA-256 geprüft; beschädigte lokale Bytes werden unter Transaktionsschutz durch den geprüften Kandidaten ersetzt.

Quoten bleiben beim bestehenden logischen Modell, damit eine Änderung des Speicherformats keine zusätzliche Speichermenge freigibt oder zuvor erlaubte Dateien plötzlich sperrt. `physical_bytes` ergänzt die Verwaltung um die Summe der registrierten Objektlängen; registrierte temporäre, verwaiste und zur Löschung beanspruchte Daten zählen bis zur Bereinigung mit. Noch nicht registrierte Schreibdateien, Dateisystembelegung und Verschlüsselungs-Overhead sind davon zu unterscheiden.

Das Backupformat v3 enthält neben den physischen Blockdaten geordnete Versions- und Zuordnungsindizes. Es prüft die einzelnen Blöcke sowie die zusammengesetzten Versionen; archivierte Blöcke bleiben über dauerhafte Backup-Referenzen geschützt. Restore berechnet Referenzzähler neu, bricht alte Upload-Sitzungen ab und übernimmt deren temporäre Blockzuordnungen nicht. Ältere Backupformate bleiben lesbar.

## Alternativen und Risiken

Ganzblob-Speicherung wurde wegen des Speichermehrbedarfs verworfen. Globale, organisationsweite oder auch nur dateiübergreifende Deduplizierung wurde wegen Informationsabfluss verworfen. Die HMAC-Kennung verhindert gemeinsame Adressierung über Dateigrenzen. Die Datenbank enthält zusätzlich Klartext-SHA-256 für die Integritätsprüfung; dieses Verfahren verspricht keine Vertraulichkeit gegenüber einem kompromittierten Datenbankzugang. Verlust oder Wechsel des Serverschlüssels kann die Wiederverwendungsrate senken; für verschlüsselte Archivdaten gelten weiterhin die gesonderten Anforderungen an Schlüssel und Wiederherstellung aus AC-12.

FastCDC ist eine neue Abhängigkeit (`fastcdc` 3.1.0, MIT, https://github.com/nlfiedler/fastcdc-rs), gewählt für deterministisches inhaltsdefiniertes Chunking mit gepflegter Versionierung. Die konkrete Speicher- und Schlüsselrotationsevidenz steht im AC-15-Bericht.
