# AC-16 — Transparente Kompression im Speicher

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac16`, Branch `feat/ac-16-storage-compression` (Basis: Endstand von AC-14/AC-15 aus `D:\ArtCraft\CraftCloud\wt-ac14-15`, Branch `feat/ac14-ac15-integration`; Stand 2026-11-03 dort noch uncommitted, daher vor dem Anlegen des AC-16-Worktrees lokal committen oder AC-16 nach Absprache direkt auf diesem Stand aufsetzen). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-16\REPORT.md`. **Migration:** `0027` (0024–0026 belegt AC-14/AC-15), sofern der Koordinator nichts anderes nennt. **Review:** unabhängig (Speicherformat, Ressourcenverbrauch).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac16 (Branch feat/ac-16-storage-compression).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-16-storage-compression.md,
docs/12-app-cloud/ac-15-chunk-dedup.md, docs/09-decisions/adr-0032-per-file-chunk-dedup.md,
docs/03-features/cc-f09-storage-drivers.md und docs/05-security/threat-model.md vollständig
und setze AC-16 genau danach um. Halte die Arbeitsweise aus dem README ein.
Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-16\REPORT.md im vorgegebenen Format.
```

## Ziel

Blöcke aus AC-15 werden vor dem Ablegen mit zstd komprimiert, wenn das Platz spart. Gewinn je Format sehr unterschiedlich: `.pcraft` speichert Kacheln bereits zstd-komprimiert (kaum Gewinn), PSD, TIFF, SVG, unkomprimierte Rohdaten und Textformate dagegen oft deutlich. Nutzer, Apps und HTTP-Vertrag merken nichts.

## Ist-Stand (bei Start prüfen)

- Nach AC-15: Versionen bestehen aus Blöcken (`chunks`, `version_chunks`) oder sind `whole`. Download setzt Blöcke zusammen, `Range` über Block-Offsets.
- Kein Kompressionscode im Server. Im Workspace vorhandene Kompressionsbibliotheken prüfen, bevor eine neue Abhängigkeit kommt.

## Entscheidungen (fest)

1. **Nur blockweise.** Komprimiert wird je Block (AC-15), nie eine ganze Version am Stück. So bleibt `Range`-Lesen billig: nur betroffene Blöcke werden entpackt. `whole`-Versionen bleiben unkomprimiert, bis `admin storage rechunk` sie umstellt.
2. **Algorithmus:** zstd, Stufe 3 (Instanzeinstellung `storage.compression_level` 1–19, Umgebung `CRAFTCLOUD_STORAGE_COMPRESSION_LEVEL` hat Vorrang; `0` schaltet Kompression ab). Ein Block wird nur komprimiert abgelegt, wenn er dadurch um mindestens 5 % kleiner wird; sonst roh. Spalte `chunks.encoding` = `raw | zstd`, `chunks.stored_bytes` zusätzlich zu `size_bytes`.
3. **Kennung bleibt Inhaltskennung:** Die Blockkennung aus AC-15 wird über den **unkomprimierten** Inhalt gebildet, damit Deduplizierung unabhängig von der Kompressionsstufe ist.
4. **Prüfung:** Beim Schreiben wird der komprimierte Block testweise entpackt und gegen den Digest geprüft, bevor er veröffentlicht wird. Beim Lesen wird nach dem Entpacken geprüft; Abweichung = beschädigter Block (bestehender Pfad wie `corrupt_response`).
5. **Ressourcen:** Entpacken mit fester Obergrenze (Fenstergröße und Ausgabegröße = `size_bytes` des Blocks), damit ein manipuliertes Objekt keinen Speicher-Ausbruch („Zip-Bombe“) auslöst. Kompression läuft auf dem Blocking-Pool mit begrenzter Parallelität (Instanz-Konstante, im Bericht begründen).
6. **Quoten:** zählen weiter logisch (wie AC-15). Admin-Nutzung zeigt `physical_bytes` nach Kompression.
7. **Archiv und Backup:** übernehmen Blöcke so, wie sie liegen (komprimiert bleibt komprimiert); Backup-Prüfung per Digest nach dem Entpacken.
8. **Messung im Bericht:** Kompressionsrate und Durchsatz für synthetische und reale Beispielformate: `.pcraft` (erwartet nahe 100 %), PSD, SVG/Text. Richtwerte des Koordinators vom 2026-11-03 an einem realen Plakat-PSD (340 357 213 Bytes, ganze Datei): zlib Stufe 1 84,0 %, zlib Stufe 6 67,0 %; lzma Stufe 6 auf einer 64-MiB-Stichprobe 46,9 % (langsam, 36 s). Dieselbe Datei als `.pcraft` belegt 283 057 093 Bytes (83 % des PSD); eine kleine `.pcraft` (1 MB) ließ sich mit zlib 6 nur auf 94 % verkleinern. zstd-Werte misst der Bericht.

## Erlaubte Dateien

Migration, `crates/craftcloud-db/src/{chunks*,maintenance.rs,instance_settings.rs}`, `crates/craftcloud-api/src/{uploads.rs,versions.rs,maintenance.rs}`, `crates/craftcloud-backup`, Admin-UI (Einstellung und Nutzungsanzeige), OpenAPI (nur `physical_bytes`), `docs/06-operations/deployment.md`, neues ADR `docs/09-decisions/adr-0033-storage-compression.md`.

## Tests (Pflicht)

- Gut komprimierbarer Block wird `zstd`, Zufallsdaten bleiben `raw`; beide byte-genau lesbar.
- `Range` über komprimierte und rohe Blöcke gemischt.
- Manipulierter komprimierter Block (falscher Inhalt, überlange Ausgabe) wird als beschädigt erkannt, ohne mehr als `size_bytes` Speicher zu belegen.
- Wechsel der Stufe ändert keine Kennungen; Deduplizierung greift über Stufenwechsel hinweg.
- Archiv-, Backup- und Restore-Drill mit komprimierten Blöcken.

## Abnahme

1. Gut komprimierbare Daten belegen messbar weniger physischen Speicher; `.pcraft` belegt nicht mehr als vorher (Rohfallback).
2. HTTP-Vertrag (Bytes, `Repr-Digest`, `Range`) unverändert.
3. Schutz gegen Entpack-Ausbruch belegt.
4. `cargo xtask ci` und `cargo xtask db-test` grün; Messwerte im Bericht.

## Nicht Teil von AC-16

HTTP-Transportkompression (`Content-Encoding`), Kompression ganzer `whole`-Versionen, formatspezifische Neukodierung (z. B. PSD-Kanäle umpacken).
