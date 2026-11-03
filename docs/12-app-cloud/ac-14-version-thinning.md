# AC-14 — Versionsausdünnung und Speicherbudget je Datei

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac14`, Branch `feat/ac-14-version-thinning` (Basis: aktueller `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-14\REPORT.md`. **Migration:** die nächste freie Nummer, die der Koordinator beim Start nennt. **Review:** unabhängig (Datenverlust durch Löschregeln).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac14 (Branch feat/ac-14-version-thinning).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-14-version-thinning.md,
docs/12-app-cloud/ac-12-storage-tiers.md, docs/12-app-cloud/ac-13-setup-wizard.md,
docs/09-decisions/adr-0014-*.md und den Abschnitt „Wartungsjob“ in docs/06-operations/deployment.md
vollständig und setze AC-14 genau danach um. Halte die Arbeitsweise aus dem README ein.
Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-14\REPORT.md im vorgegebenen Format.
```

## Ziel

Große App-Dateien belegen heute pro Speichern eine volle Kopie. Beispiel aus dem Betrieb (2026-11-03): eine `.pcraft`-Datei mit 283 MB, zehnmal am Tag gespeichert, belegt bei der Standardaufbewahrung von 30 Tagen grob 80 GB. Die bestehende Aufbewahrung (`CRAFTCLOUD_VERSION_RETENTION_DAYS` bzw. Instanzeinstellung `retention.version_days`) kennt nur „alles bis Alter X behalten“. AC-14 ergänzt eine **Ausdünnung nach Zeitraster** und ein **Höchstmaß an Versionen je Datei**, damit Betreiber Platz sparen, ohne die jüngste Arbeit oder Meilensteine zu verlieren.

## Ist-Stand (bei Start prüfen)

- Wartungsjob in `crates/craftcloud-api/src/maintenance.rs` und `crates/craftcloud-db/src/maintenance.rs`: Schritt „überholte Versionen nach `version_retention` löschen“, danach Blob-Garbage-Collection. Der Head bleibt immer.
- Instanzeinstellungen aus AC-13 (`crates/craftcloud-db/src/instance_settings.rs`): Schlüssel `retention.version_days`, `retention.trash_days`, `retention.audit_days`, Umgebung hat Vorrang, Schonfrist nach Verkürzung.
- AC-12: überholte Versionen können im Archivspeicher liegen; Löschen muss auch dort wirken (bestehender Archiv-Löschpfad).
- Versionen kennen `restored_from_version_id` und `base_version_id`; Verweise auf gelöschte Versionen werden auf NULL gesetzt.

## Entscheidungen (fest)

1. **Ausdünnungsraster (Standard, an):** Von den überholten Versionen einer Datei bleiben erhalten:
   - alle Versionen der letzten 24 Stunden,
   - je Kalendertag die jüngste Version der letzten 7 Tage,
   - je ISO-Woche die jüngste Version der letzten 4 Wochen,
   - danach greift die bestehende Altersgrenze `retention.version_days`.
   Zeitgrundlage ist `created_at` der Version (Datenbankuhr, UTC). Die Rasterlängen sind Instanzeinstellungen (`versions.keep_all_hours` 0–168, `versions.daily_days` 0–90, `versions.weekly_weeks` 0–52); `0` schaltet die jeweilige Stufe ab. Umgebungsvariablen gleichen Namens in Großbuchstaben mit Präfix `CRAFTCLOUD_` haben Vorrang, wie bei der Aufbewahrung.
2. **Höchstzahl je Datei:** `versions.max_per_file` (Standard `0` = unbegrenzt, sonst 2–10000). Überzählige Versionen werden nach dem Raster von der ältesten her gelöscht; der Head zählt mit und bleibt immer.
3. **Geschützte Versionen werden nie ausgedünnt:** der Head, die Version, auf die eine FigCraft-Veröffentlichung zeigt (`published_from_version_id`), Versionen mit offener Upload-Sitzung als Basis, und Versionen mit einer Markierung „behalten“. Die Markierung ist neu: Spalte `pinned boolean not null default false` auf Versionen, setzbar per `PATCH /api/v1/files/{file_id}/versions/{version_id}` mit `{"pinned": true|false}` durch Personen mit Bearbeitungsrecht (Aktion im zentralen `can()`, Audit `version.pin`/`version.unpin`). Gepinnte Versionen zählen nicht gegen `max_per_file`.
4. **Löschen geht über den bestehenden Pfad** (Version entfernen, Verweise auf NULL, Blob wird `orphaned`, GC räumt auf, Archivobjekt wird gelöscht). Kein zweiter Löschweg.
5. **Schonfrist:** Wird ein Raster verkürzt, gilt wie bei der Aufbewahrung die bestehende Schonfrist, bevor zusätzlich gelöscht wird. Ein Trockenlauf (`craftcloud-server admin maintenance --dry-run`) meldet je Schritt `versions_thinned_due`.
6. **Admin-Konsole:** Die bestehende Seite der Instanzeinstellungen bekommt die vier Werte mit Erklärtext und einer Beispielrechnung („bei 10 Speichervorgängen am Tag bleiben etwa N Versionen“). Die Versionsliste im Webpanel zeigt die Markierung „behalten“ und erlaubt das Umschalten.
7. **API:** Versionsliste liefert `pinned` zusätzlich (abwärtskompatibel). OpenAPI ergänzen. Das SDK (`crates/craftcloud-client`) bekommt `pinned` als `#[serde(default)]`-Feld und eine Methode `pin_version(file, version, pinned)`.

## Erlaubte Dateien

Migration, `crates/craftcloud-db/src/{maintenance.rs,instance_settings.rs,versions*.rs}`, `crates/craftcloud-api/src/{maintenance.rs,versions.rs,setup_wizard.rs}` soweit für Einstellungen nötig, Policy-Aktion in `crates/craftcloud-core`, OpenAPI, SDK-DTO, Admin-UI (Einstellungsseite, Versionsliste), `docs/06-operations/deployment.md`, `docs/05-security/permission-matrix.md`, neues ADR `docs/09-decisions/adr-0031-version-thinning.md`.

## Tests (Pflicht)

- Reine Funktion „welche Versionen behalten“ mit Tabellentests: leere Datei, nur Head, 100 Versionen über 60 Tage verteilt, Tagesgrenzen in UTC, Wochenwechsel, Schaltjahr, `0`-Stufen, `max_per_file` kleiner als Rasterergebnis, gepinnte und veröffentlichte Versionen.
- Datenbanktest: Wartungslauf löscht genau die erwarteten Versionen, Head und gepinnte bleiben, Blob wird GC-fähig, Archivobjekt (AC-12) wird gelöscht, Trockenlauf löscht nichts.
- Berechtigung: Pin ohne Bearbeitungsrecht ergibt 403/404 wie im Rest der API; Audit-Ereignis entsteht.
- Schonfrist nach Verkürzung des Rasters.

## Abnahme

1. Mit Standardwerten bleiben bei stündlichem Speichern über 60 Tage höchstens 24 + 7 + 4 + Head Versionen (zuzüglich gepinnter) übrig; Test belegt die Zahl.
2. Kein Wartungslauf löscht den Head, eine gepinnte oder eine veröffentlichte Version.
3. Einstellungen wirken über Umgebung und Admin-Konsole mit dokumentiertem Vorrang.
4. `cargo xtask ci` und `cargo xtask db-test` grün; OpenAPI-Vertragstest angepasst.
5. `deployment.md` erklärt Raster, Höchstzahl, Pin und die Speicherrechnung.

## Nicht Teil von AC-14

Deduplizierung (AC-15), Kompression (AC-16), Quoten je Datei, Ausdünnung im Client.
