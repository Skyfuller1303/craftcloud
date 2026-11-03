# AC-15 — Blockweise Deduplizierung innerhalb einer Datei und Delta-Upload

> **Repository:** CraftCloud (Server und SDK); PhotoCraft nur für den Abschlusstest. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac15`, Branch `feat/ac-15-chunk-dedup` (Basis: aktueller `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-15\REPORT.md`. **Migration:** die nächste freie Nummer vom Koordinator. **Review:** Opus-Review (Sicherheit, Datenverlust, Speicherformat). Zwei Phasen; Phase B beginnt erst nach Abnahme von Phase A.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac15 (Branch feat/ac-15-chunk-dedup).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-15-chunk-dedup.md,
docs/12-app-cloud/ac-12-storage-tiers.md, docs/03-features/cc-f09-storage-drivers.md,
docs/09-decisions/adr-0004-storage.md, docs/09-decisions/adr-0028-storage-tiers.md
und docs/05-security/threat-model.md vollständig und setze Phase <A|B> von AC-15 genau danach um.
Halte die Arbeitsweise aus dem README ein. Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-15\REPORT.md im vorgegebenen Format.
```

## Ziel

Jede Version liegt heute als vollständiges Objekt im Speicher (ADR-0004, „keine Deduplizierung“). Bei großen App-Dateien ändert ein Speichern meist nur einen kleinen Teil: Eine `.pcraft`-Datei ist ein ZIP-Archiv mit STORE-Einträgen aus inhaltsadressierten, einzeln zstd-komprimierten 256×256-Kacheln; unveränderte Kacheln bleiben byte-identisch. PSD-Dateien ändern sich ähnlich lokal. AC-15 speichert Versionen **einer Datei** als Folge inhaltsdefinierter Blöcke und legt nur neue Blöcke ab (Phase A, spart Speicher). Phase B lässt den Client nur fehlende Blöcke hochladen (spart Bandbreite und Zeit).

Erwartung (vom Bericht mit Messung zu belegen): Eine Version mit kleiner Änderung an einer 283-MB-Datei belegt einige MB statt 283 MB.

## Ist-Stand (bei Start prüfen)

- `crates/craftcloud-storage`: `ObjectStore` mit unveränderlichen Objekten, Range-Lesen, zufälligen Schlüsseln; keine Deduplizierung (bewusst, Mandantentrennung).
- Tabelle `blobs` (Migration 0008): ein Blob je Version, `sha256`, `size_bytes`, Zustände `staged | committed | orphaned`; Garbage-Collection und Abgleich im Wartungsjob.
- Upload: `POST /api/v1/files/{id}/uploads` → `PUT /api/v1/uploads/{id}/content` (Streaming, Prüfung Länge und SHA-256) → Commit mit `If-Match`. Download: `GET …/content` mit `Range`, `ETag`, `Repr-Digest` (SHA-256 der ganzen Datei), Prüfung der gestreamten Bytes.
- AC-12: überholte Versionen wandern in den Archivspeicher; Backup (`craftcloud-backup`) sichert Datenbank und Objekte.
- Quoten zählen heute `size_bytes` je Blob.
- PhotoCraft lädt seit 2026-11-03 große Dateien in 16-MiB-Range-Abschnitten, vier parallel, mit Fortschritt; der Download-Vertrag (`Range`, `Repr-Digest` der ganzen Datei) muss unverändert bleiben.

## Entscheidungen (fest)

1. **Geltungsbereich der Deduplizierung: genau eine Datei** (alle Versionen derselben `file_id`, einschließlich Wiederherstellungen). Nie über Dateien, Projekte, Workspaces oder Organisationen hinweg. Damit verrät der Speicher nichts über fremde Inhalte (Mandantentrennung aus ADR-0004 bleibt). Eine Kopie („Beide behalten“, Datei duplizieren) bekommt eigene Blöcke.
2. **Blockbildung:** inhaltsdefiniertes Chunking (FastCDC, Ziel 1 MiB, min 256 KiB, max 4 MiB, feste Parameter je Speicherformat-Version). Blockkennung ist SHA-256 des Blockinhalts **zusammen mit der `file_id` als Schlüsselmaterial** (HMAC-SHA-256 mit einem aus dem Serverschlüssel abgeleiteten Schlüssel und `file_id`), damit gleiche Inhalte verschiedener Dateien nie dieselbe Kennung haben. Abhängigkeit (z. B. `fastcdc`) im Bericht mit Version, Lizenz, Grund.
3. **Datenmodell:** neue Tabellen `chunks` (`id`, `file_id`, `digest`, `object_key`, `size_bytes`, `refcount` bzw. Zustände wie Blobs, `storage_tier`) und `version_chunks` (`version_id`, `ordinal`, `chunk_id`, `offset`). Der bestehende Blob bleibt als logische Sicht der Version (`sha256`, `size_bytes` der ganzen Datei) erhalten; `blobs.layout` = `whole | chunked`. Bestehende Daten bleiben `whole` und werden nicht automatisch umgeschrieben; ein Admin-Befehl `craftcloud-server admin storage rechunk --file <id>|--all --dry-run` kann sie später umstellen.
4. **Schreiben (Phase A):** Der Server zerlegt den eingehenden Upload-Stream, prüft wie bisher Länge und SHA-256 der ganzen Datei, legt nur Blöcke an, deren Kennung für diese Datei noch nicht existiert, und veröffentlicht alles erst beim Commit (nichts Teilweises sichtbar; Abbruch hinterlässt höchstens GC-fähige Blöcke).
5. **Lesen:** Download setzt die Blöcke in Reihenfolge zusammen, `Range` wird über `offset` auf die betroffenen Blöcke abgebildet. `Repr-Digest`, `ETag`, `Content-Length`, Statuscodes bleiben byte-genau wie heute; die Streaming-Prüfung der ganzen Datei bei vollständigem Download bleibt.
6. **Quoten:** Quoten zählen weiterhin die **logische** Größe jeder Version (`size_bytes`), damit Nutzer und Admins dieselben Zahlen sehen wie bisher und sich Quoten nicht durch Speicherdetails verschieben. Zusätzlich meldet `GET /api/v1/…/usage` für Admins `physical_bytes` (tatsächlich belegt). Abweichung als Entscheidung im ADR begründen.
7. **Löschen und GC:** Ein Block wird erst gelöscht, wenn keine Version derselben Datei ihn mehr referenziert. GC arbeitet in Stapeln mit Claim wie heute; der Abgleich Speicher gegen Datenbank kennt Blockobjekte.
8. **Archiv (AC-12):** Archivierung verschiebt Blöcke, die nur noch von archivierbaren Versionen referenziert werden; Blöcke, die auch der Head braucht, bleiben lokal. Wiederherstellen einer archivierten Version holt fehlende Blöcke lokal zurück.
9. **Backup:** sichert Blöcke und Zuordnungen; Prüfung per Blockdigest und per SHA-256 der zusammengesetzten Version. Restore-Drill aus `craftcloud-backup` um eine gechunkte Datei mit mehreren Versionen erweitern.
10. **Phase B (Delta-Upload, eigenes Merge):** neue, optionale API: `POST /api/v1/files/{id}/uploads` mit `"mode": "chunked"` und der Blockliste (Größe und **Klartext-SHA-256** je Block, berechnet vom Client mit denselben FastCDC-Parametern; der Server bildet daraus selbst die HMAC-Kennung) → Antwort nennt fehlende Blöcke → `PUT /api/v1/uploads/{id}/chunks/{ordinal}` je fehlendem Block (nginx-Grenze beachten: Route wie `/content` als Streaming-Ausnahme dokumentieren) → Commit wie heute mit Prüfung der ganzen Datei. Der Server antwortet nur zu Blöcken **dieser Datei**, auf die der Aufrufer Bearbeitungsrecht hat (kein Existenz-Orakel über Dateigrenzen). SDK: `upload_with_cancel` nutzt den Modus automatisch, wenn `/api/v1/info` das Feature `chunked_upload` meldet, mit Fortschritt je Block; sonst bisheriger Weg. Alte Clients bleiben unverändert funktionsfähig.

## Erlaubte Dateien

Migration, `crates/craftcloud-storage` (nur Hilfen, kein Bruch des `ObjectStore`-Vertrags), `crates/craftcloud-db/src/{blobs*,versions*,maintenance.rs,uploads*}`, `crates/craftcloud-api/src/{uploads.rs,versions.rs,maintenance.rs,archive_worker.rs,info.rs}`, `crates/craftcloud-backup`, SDK `crates/craftcloud-client*` (Phase B), OpenAPI, `deploy/proxy/*.conf` (Phase B), `docs/06-operations/deployment.md`, `docs/03-features/cc-f09-storage-drivers.md`, neues ADR `docs/09-decisions/adr-0032-per-file-chunk-dedup.md` (ersetzt den Satz „keine Deduplizierung“ aus ADR-0004 für den Bereich einer Datei).

## Tests (Pflicht)

- Chunking: deterministisch, Grenzen stabil bei Einfügen am Anfang/Mitte/Ende (Anteil wiederverwendeter Blöcke gemessen), Min/Max eingehalten, leere und 1-Byte-Datei.
- Gleicher Inhalt in zwei verschiedenen Dateien ergibt verschiedene Kennungen und verschiedene Objekte.
- Upload zweier Versionen mit kleiner Änderung: zweite belegt nur neue Blöcke (Messung im Test), Download beider Versionen byte-genau, `Repr-Digest` korrekt, `Range` über Blockgrenzen korrekt (Anfang, Mitte, über drei Blöcke, letztes Byte).
- Absturzpunkte (Fehlerinjektion aus `faults.rs`): Abbruch mitten im Upload, vor und nach Commit; danach keine sichtbare Teilversion, GC räumt auf.
- Löschen einer Version löscht nur nicht mehr referenzierte Blöcke; Archiv- und Backup-Drill mit gechunkter Datei.
- Mischbetrieb: `whole`-Versionen und `chunked`-Versionen derselben Datei.
- Phase B: fehlende-Blöcke-Antwort, falsche Blocklänge/Digest wird abgelehnt, Commit prüft ganze Datei, Aufrufer ohne Recht bekommt 404, altes SDK funktioniert weiter.
- Lastmessung mit einer synthetischen 300-MB-Datei: Upload, zweite Version mit 1 % Änderung, Download; Zeiten und belegter Platz im Bericht.

## Abnahme

1. Phase A: zweite Version einer 300-MB-Datei mit 1 % Änderung belegt höchstens 10 % zusätzlichen physischen Speicher; alle Downloads byte-genau mit unverändertem HTTP-Vertrag.
2. Keine Deduplizierung über Dateigrenzen (Test belegt).
3. Quoten unverändert logisch; `physical_bytes` für Admins sichtbar.
4. Archiv, Backup, Restore, GC und Abgleich funktionieren mit gechunkten Versionen.
5. Phase B: zweite Version überträgt höchstens 10 % der Datei; altes SDK unverändert lauffähig.
6. `cargo xtask ci` und `cargo xtask db-test` grün.

## Nicht Teil von AC-15

Kompression (AC-16), Deduplizierung über Dateigrenzen, formatbewusstes Zerlegen (z. B. `.pcraft`-Einträge statt CDC), automatisches Umschreiben bestehender Daten ohne Admin-Befehl.
