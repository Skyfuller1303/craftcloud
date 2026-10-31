# AC-12 — Speicherstufen: neueste Version lokal, Archiv extern

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac12`, Branch `feat/ac-12-storage-tiers` (Basis: aktueller `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-12\REPORT.md`. **Migration:** die nächste freie Nummer, die der Koordinator beim Start nennt. **Review:** unabhängig, sicherheitsrelevant (Fremdanbieter-Speicher, Verschlüsselung, Datenverlust bei Absturz).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac12 (Branch feat/ac-12-storage-tiers).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-12-storage-tiers.md,
docs/03-features/cc-f09-storage-drivers.md und docs/09-decisions/adr-0004-storage.md vollständig
und setze AC-12 genau danach um. Halte die Arbeitsweise aus dem README ein.
Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-12\REPORT.md im vorgegebenen Format.
```

## Ziel

Der CraftCloud-Server hält auf seinem eigenen Datenträger nur die **neueste Version** jeder Datei. Ältere Versionen (das Versionsarchiv) liegen auf einem externen Speicher, den der Betreiber wählt, zum Beispiel eine Hetzner Storage Box (SFTP) oder ein S3-kompatibler Objektspeicher (Hetzner Object Storage, Backblaze B2, MinIO, Wasabi, AWS S3). Für Personen und Apps ändert sich nichts: Versionsliste, Download, Wiederherstellen und Konfliktschutz funktionieren wie bisher, ältere Versionen laden nur etwas langsamer. Ohne Konfiguration eines Archivspeichers verhält sich die Instanz genau wie heute.

Wunsch des Projektverantwortlichen (2026-10-31): „immer nur die neueste Datei lokal, das Archiv extern hosten“.

## Ist-Stand (bei Start prüfen)

- Crate `craftcloud-storage` mit Trait `ObjectStore` (temporär schreiben, prüfen und unveränderlich veröffentlichen, Range-Lesen, `head`, `delete`, Auflisten) und `LocalStore`; Fehlerinjektion in `faults.rs`.
- Tabelle `blobs` (Migration 0008) mit opakem `object_key` (32 Hex-Zeichen), `sha256`, `size_bytes`, Zustand `staged | committed | orphaned`; Versionen verweisen auf Blobs. Keine Deduplizierung.
- Wartung (`crates/craftcloud-db/src/maintenance.rs`): Aufbewahrung 30 Tage, Versionsaufbewahrung, Blob-Garbage-Collection mit Claim, Abgleich Speicher gegen Datenbank.
- Backup (`crates/craftcloud-backup`): Datenbank und lokale Objekte, Prüfung per SHA-256.
- FigCraft-Live (AC-06) liest die Version `published_from_version_id`; diese kann nach AC-12 im Archiv liegen.
- Serverschlüssel aus AC-02 (`CRAFTCLOUD_SECRET_KEY_FILE`) für verschlüsselte Felder.

## Entscheidungen (fest)

1. **Zwei Stufen, ein Vertrag.** Primärspeicher (lokal, `LocalStore`) und optionaler Archivspeicher (extern) implementieren denselben `ObjectStore`-Vertrag; dieselben Vertragstests laufen gegen beide. Die API, OpenAPI und die Apps sehen keinen Unterschied außer einem Feld `storage_tier: "local" | "archive"` in der Versionsliste (nur informativ).
2. **Was wo liegt.**
   - Die aktuelle Version (Head) jeder Datei liegt immer lokal. Laufende Uploads und `staged`-Blobs ebenfalls.
   - Eine Version wird archivierbar, sobald sie nicht mehr Head ist und seit `CRAFTCLOUD_ARCHIVE_AFTER` abgelöst ist (Dauer, Standard `24h`, erlaubt `0` bis `90d`). Die kurze Frist hält die gerade ersetzte Version für schnelles Zurückholen lokal; `0` bedeutet „sofort nach Ablösung“.
   - Wird eine archivierte Version wiederhergestellt (neue Head-Version aus alten Bytes), kopiert der Server die Bytes in ein neues lokales Objekt; die archivierte Version bleibt im Archiv.
   - Dateien im Papierkorb behalten ihren Head lokal bis zur endgültigen Löschung.
3. **Verschieben ist absturzsicher und idempotent.** Je Blob ein Zustand `tier_state`: `local` → `archiving` → `archived` (lokale Kopie gelöscht) bzw. `local` bei Rückholung. Ablauf: in das Archiv schreiben → Länge und SHA-256 durch Zurücklesen prüfen (oder eine vom Anbieter gelieferte, kryptografisch gleichwertige Prüfsumme; im Bericht begründen) → in einer Transaktion `archived` und den Archiv-Schlüssel setzen → erst nach dem Commit lokal löschen. Jeder Absturz zwischen zwei Schritten lässt höchstens eine überzählige Kopie zurück, nie eine fehlende; der Abgleich räumt Überzähliges auf. Ein lokales Objekt wird nie gelöscht, bevor die Archivkopie geprüft und in der Datenbank festgehalten ist.
4. **Verschlüsselung vor dem Hochladen (Pflicht).** Daten verlassen den Server nur verschlüsselt: XChaCha20-Poly1305 in festen Blöcken (z. B. 64 KiB) mit Blockindex und Objekt-ID als AAD, damit Range-Lesen ohne Entschlüsseln des ganzen Objekts geht und Blöcke weder vertauscht noch abgeschnitten werden können. Je Objekt ein zufälliger Datenschlüssel, verpackt mit einem eigenen, aus dem Serverschlüssel abgeleiteten Archivschlüssel (eigener Kontext `"archive"`); der verpackte Schlüssel liegt in der Datenbank. Der Archivanbieter sieht weder Inhalte noch Dateinamen, Organisation oder Format; Objektnamen im Archiv sind opak. Ohne Serverschlüssel startet der Server mit aktivem Archiv nicht. Verwendete Krypto-Crates sind verbreitet und geprüft (Empfehlung `chacha20poly1305` der RustCrypto-Gruppe); keine eigene Kryptografie.
5. **Adapter.**
   - **S3-kompatibel** (zuerst): `CRAFTCLOUD_ARCHIVE_STORE=s3`, Endpoint, Region, Bucket, optionales Präfix, Zugangsschlüssel als Dateien (`*_FILE`). Multipart-Upload mit Abbruch bei Fehler, TLS-Prüfung immer an (Ausnahme nur Loopback in Tests, z. B. MinIO-Container). Bibliothek: eine verbreitete Crate mit rustls (Empfehlung `object_store` mit Feature `aws`, alternativ `rust-s3`); Version, Lizenz, Grund im Bericht.
   - **SFTP** (für die Hetzner Storage Box, Port 23): `CRAFTCLOUD_ARCHIVE_STORE=sftp`, Host, Port, Benutzer, Basisverzeichnis, privater Schlüssel als Datei, **Pflicht**: festgelegter Host-Schlüssel (`CRAFTCLOUD_ARCHIVE_SFTP_KNOWN_HOSTS_FILE`); unbekannter oder geänderter Host-Schlüssel bricht ab. Kein Passwort-Login. Schreiben in temporären Namen und Umbenennen ohne Überschreiben. Begrenzte parallele Verbindungen (Standard 4, Storage Box erlaubt nur wenige). Bibliothek: `russh` + `russh-sftp` oder gleichwertig; im Bericht begründen.
   - Kein Einbinden als Netzlaufwerk (SMB, sshfs) als empfohlener Weg; die Betriebsdoku erklärt, warum (keine Stufen, Ausfälle blockieren den Server).
6. **Lesen archivierter Versionen.** Download und Range-Requests laufen durch den Server (Entschlüsseln im Stream, Byte-Limits wie heute); keine vorsignierten URLs zum Anbieter. Ist das Archiv nicht erreichbar: `503 ARCHIVE_UNAVAILABLE` mit `Retry-After`, nur für archivierte Versionen; Head-Versionen und alle Schreibwege bleiben unberührt. Optionale lokale Zwischenablage für zuletzt gelesene Archivobjekte ist **nicht** Teil von AC-12.
7. **Löschen, Aufbewahrung, Kontingent.** Endgültiges Löschen (Papierkorb nach 30 Tagen, Versionsaufbewahrung, Organisation entfernen) löscht in beiden Stufen; ein fehlgeschlagenes Löschen im Archiv wird wiederholt und im Operator-Bereich angezeigt, nie stillschweigend vergessen. Das Kontingent zählt die logische Größe aller Versionen unabhängig von der Stufe (unverändert).
8. **Hintergrund-Worker.** Ein Worker im Server archiviert in kleinen Paketen mit Claim (wie die Blob-GC), Backoff bei Fehlern, Begrenzung der Bandbreite (`CRAFTCLOUD_ARCHIVE_MAX_MBPS`, optional) und der Parallelität. Bei der ersten Aktivierung werden bestehende alte Versionen schrittweise übernommen. Regelmäßige Stichprobenprüfung (`head` und gelegentlich vollständiges Zurücklesen mit SHA-256) meldet fehlende oder beschädigte Archivobjekte.
9. **Ausschalten und Umziehen.** CLI `craftcloud-server archive recall [--org <id>]` holt alle archivierten Objekte zurück auf den lokalen Speicher (mit Platzprüfung vorab); `craftcloud-server archive status` zeigt Zählungen. Wechsel des Archivanbieters erfolgt über Rückholung und erneute Archivierung; ein direkter Umzug zwischen Archiven ist nicht Teil von AC-12. Der Server startet nicht, wenn archivierte Objekte existieren, aber kein Archivspeicher konfiguriert ist (klare Startmeldung mit Verweis auf `archive recall`).
10. **Backup und Restore.** Das Backup enthält Datenbank (mit verpackten Datenschlüsseln) und lokale Objekte; archivierte Objekte werden referenziert, nicht kopiert. `backup verify` prüft zusätzlich die Erreichbarkeit der referenzierten Archivobjekte (Stichprobe, Zahl konfigurierbar). Die Betriebsdoku empfiehlt Versionierung oder Snapshots beim Archivanbieter (Storage Box: Snapshots; S3: Bucket-Versionierung oder Object Lock) und erklärt, dass Serverschlüssel und Datenbank-Backup zusammen nötig sind, um das Archiv lesen zu können.
11. **Operator-Bereich.** „System“ zeigt: Archiv konfiguriert (Anbieter-Typ, Endpoint ohne Zugangsdaten), Bytes lokal und im Archiv, ausstehende Archivierungen, fehlgeschlagene Löschungen, letzte Prüfung und letzter Fehlercode. Versionsliste im Webpanel zeigt ein kleines „Archiv“-Kennzeichen. Texte EN/DE.
12. **Audit.** Aktivieren/Deaktivieren des Archivs, Rückholung, dauerhafte Prüf- oder Löschfehler. Keine Einträge pro einzelnem Objekt im normalen Betrieb (nur Zähler und Logs ohne Inhalte).

## Erlaubte Dateien

`crates/craftcloud-storage/**` (Adapter, Verschlüsselung, Vertragstests), `crates/craftcloud-db/**` (Migration, Wartung), `crates/craftcloud-api/src/**` (Lesen, Versionsliste, Operator-Status), `crates/craftcloud-backup/**`, `apps/craftcloud-server/src/**` (Konfiguration, Worker, CLI), `apps/craftcloud-admin/src/**` (System-Bereich, Versionskennzeichen), `contracts/openapi.yaml` und Snapshot, `deploy/compose/**` (MinIO und ein SFTP-Testserver nur in Test-/Dev-Compose, nie im Installationspfad), `deploy/install/**`, `docs/03-features/cc-f09-storage-drivers.md`, `docs/03-features/cc-f10-quotas-and-retention.md`, `docs/03-features/cc-f14-backup-restore-and-upgrades.md`, `docs/05-security/threat-model.md`, `docs/06-operations/**`, neues ADR `docs/09-decisions/adr-0028-storage-tiers.md`, `docs/gaps.md`.

## Tests (Pflicht)

- Vertragstests: dieselbe Suite gegen `LocalStore`, S3 (MinIO-Container) und SFTP (SFTP-Testcontainer mit festem Host-Schlüssel).
- Verschlüsselung: Rundlauf, Range-Lesen über Blockgrenzen, vertauschte, abgeschnittene und veränderte Blöcke werden erkannt, falscher Schlüssel schlägt fehl.
- Absturzsicherheit: Fehlerinjektion an jedem Schritt aus Entscheidung 3 (vor/nach Schreiben, vor/nach Commit, vor/nach lokalem Löschen); danach existiert jede Version genau lesbar, Abgleich entfernt Überzähliges.
- DB/HTTP: Head bleibt lokal; Archivierung erst nach Frist; Download und Range einer archivierten Version bytegleich (SHA-256); Wiederherstellen aus dem Archiv; `503 ARCHIVE_UNAVAILABLE` nur für archivierte Versionen; Löschen in beiden Stufen inklusive Wiederholung; fremde Organisation 404 wie bisher; Kontingent unverändert.
- SFTP: unbekannter Host-Schlüssel wird abgelehnt.
- CLI: `archive recall` und `archive status`; Startabbruch bei archivierten Objekten ohne Konfiguration.
- Backup: `backup verify` meldet ein fehlendes Archivobjekt.
- Secret-Scan der Testausgaben: keine Zugangsschlüssel, privaten Schlüssel oder Datenschlüssel.
- Web: System-Bereich und Versionskennzeichen EN/DE mit axe.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Ohne Archivkonfiguration unverändertes Verhalten (Test belegt).
3. Kein Datenverlust bei Absturz an irgendeinem Schritt (Fehlerinjektionstests belegt).
4. Nur verschlüsselte Bytes beim Anbieter (Test liest das Rohobjekt aus MinIO/SFTP und findet keinen Klartext der Testdatei).
5. Betriebsdoku beschreibt Einrichtung für Hetzner Storage Box (SFTP, Host-Schlüssel ermitteln und prüfen, SSH-Schlüssel hinterlegen) und für einen S3-kompatiblen Speicher (Bucket, Zugangsschlüssel mit minimalen Rechten), dazu Rückholung und Notfall-Restore.
6. Entscheidungen 1–12 umgesetzt oder begründet abweichend; ADR-0028 angelegt.

## Nicht Teil von AC-12

Automatisches Verschieben der Head-Version nach Alter (kalte Dateien), lokaler Lese-Cache für Archivobjekte, Deduplizierung, direkter Umzug zwischen zwei Archivanbietern, WebDAV- oder SMB-Adapter, Speicherklassen beim Anbieter (Glacier o. ä.), Archiv je Organisation mit eigenem Anbieter.
