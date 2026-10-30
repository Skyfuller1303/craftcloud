# AC-05 — Cloud-Speichern in den Craft-Apps

> **Repositories:** PhotoCraft (zuerst), FigCraft, VectorCraft (sobald lokal verfügbar). **Worktrees:** `D:\ArtCraft\PhotoCraft-cloud` (Branch `feat/craftcloud-open-save`), `D:\ArtCraft\FigCraft-cloud` (Branch `feat/craftcloud-open-save`); legt der Koordinator an. **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-05\REPORT.md`. **Voraussetzung:** AC-04 gemerged. **Nur lokal**, keine Veröffentlichung in `storytold/*` oder Forks.

## Prompt für Codex

```
Lies D:\ArtCraft\CraftCloud\craftcloud\docs\12-app-cloud\README.md und
D:\ArtCraft\CraftCloud\craftcloud\docs\12-app-cloud\ac-05-app-integration.md vollständig.
Arbeite zuerst im Worktree D:\ArtCraft\PhotoCraft-cloud, danach in D:\ArtCraft\FigCraft-cloud,
jeweils nach dem AGENTS.md des Repos und den Projekt-Skills (photocraft-dev bzw. figcraft-dev).
Keine Commits, Pushes, PRs oder Kommentare. Schreibe am Ende
D:\ArtCraft\output\craftcloud-codex\AC-05\REPORT.md im vorgegebenen Format.
```

## Ziel

In PhotoCraft und FigCraft gibt es im Datei-Menü „Aus CraftCloud öffnen…“ und „In CraftCloud speichern…“. Eine aus der Cloud geöffnete Datei wird mit normalem „Speichern“ (Strg+S) lokal gespeichert und als neue Version hochgeladen. Ohne eingerichteten Server sehen die Menüpunkte nur „CraftCloud einrichten…“ vor; die App verhält sich sonst exakt wie heute. VectorCraft folgt mit demselben Muster, sobald das Repository lokal vorliegt.

## Ist-Stand (verifiziert 2026-10-30)

- **PhotoCraft** (`D:\ArtCraft\PhotoCraft`): egui/eframe 0.36, `rfd` 0.17, MSRV 1.95, WASM-App `apps/photocraft-web`. Natives Format `.pcraft` (Zip oder Ordner, `crates/format/src/lib.rs` `save_to_bytes`/`load_from_bytes`/`load_path`, `store.rs` `save_zip`/`save_path`). PSD-Import/-Export (`crates/io` `psd_import.rs`, `psd_export.rs`). Öffnen: `crates/ui-egui/src/file_open.rs` `open_path`/`open_paths`, `crates/engine/src/file_cmds.rs` `import`/`encode`/`save_doc`, asynchrone Dialoge `crates/ui-egui/src/file_dialog.rs` über `Services::file_dialog`, Speichern unter `crates/ui-egui/src/lib.rs` `save_as`. Zuletzt geöffnete Dateien und Autosave (`crates/format/src/autosave.rs`). Keine Speicherabstraktion außer `AtomicFs`; API arbeitet mit Pfaden.
- **FigCraft** (`D:\ArtCraft\FigCraft`): `Services`-Trait mit `pick_open`/`pick_save` (`crates/ui-egui/src/lib.rs`), Befehle `file.open`, `file.save`, `file.saveAs`; natives Format `.figcraft` (Zip mit `document.json` und Bildern), `.fig` Import und experimenteller Export. Livebearbeitung über `figcraft-server` (AC-06).
- **VectorCraft:** nicht lokal vorhanden; im Launcher-Katalog als `storytold/vectorcraft` geführt. Formate offen.

## Entscheidungen (fest)

1. **Pfadbasiert integrieren:** Die App bleibt pfadbasiert. Das SDK lädt in den Cache und übergibt den Pfad an den vorhandenen Öffnen-Weg; nach dem lokalen Speichern übergibt die App den Pfad an das SDK zum Hochladen. Keine Umstellung der Format-Crates auf Streams.
2. **Was gespeichert wird:** das Format, in dem die App speichert. PhotoCraft: Speichern in der Cloud standardmäßig als `.pcraft` (verlustfrei); „Als PSD in CraftCloud speichern“ als zweite Option über den vorhandenen Export. Öffnen akzeptiert alle Endungen aus dem Formatregister für die App. FigCraft: `.figcraft`; `.fig` nur öffnen (Import).
3. **Menü:** Unterhalb „Öffnen…“ bzw. „Speichern unter…“ je ein Eintrag. Tastenkürzel nur, wenn frei (keine bestehenden überschreiben). Statusleiste/Fenstertitel zeigen bei verknüpften Dateien ein Wolkensymbol und den Workspace.
4. **Sperre:** Beim Öffnen (PhotoCraft) nimmt das SDK die Bearbeitungssperre; bei fremder Sperre Auswahl „Schreibgeschützt öffnen“. Die App meldet Schließen des Dokuments an das SDK (Herzschlag endet, Sperre frei). FigCraft nimmt keine Sperre.
5. **Konflikt und Fehler:** Dialoge aus `craftcloud-egui`. Lokales Speichern erfolgt immer vor dem Upload und wird durch keinen Cloud-Fehler verhindert.
6. **Autosave:** Autosave bleibt lokal und lädt **nicht** hoch.
7. **Kein Einfluss ohne Cloud:** Feature-Flag `craftcloud` (Standard an im Desktop-Build, aus im WASM-Build); ohne eingerichteten Server werden weder Netzwerk noch Schlüsselbund berührt (Test).
8. **SDK-Einbindung lokal:** Pfad-Abhängigkeit auf `D:\ArtCraft\CraftCloud\craftcloud\crates\craftcloud-client-native` und `craftcloud-egui`. Im Bericht festhalten, dass das für eine Einreichung durch eine veröffentlichte Abhängigkeit ersetzt werden muss.
9. **i18n und Barrierefreiheit:** Texte über das i18n-System der jeweiligen App (TSV/Kataloge laut deren AGENTS.md), Englisch und Deutsch; Dialoge per Tastatur bedienbar.
10. **Öffnen per Startargument:** Die App akzeptiert ein Startargument `craftcloud://open?server=<https-url>&file=<file_id>` (vom Launcher, AC-07, und vom Webpanel als Link). Sie prüft, dass der Server bereits eingerichtet ist (sonst Rückfrage „Diesem Server vertrauen?“ mit angezeigter URL), meldet sich mit **ihrer eigenen** Geräte-Sitzung an und öffnet wie „Aus CraftCloud öffnen“. Es werden nie Tokens zwischen Launcher und App übergeben. Registrierung als systemweiter URL-Handler ist nicht Teil von AC-05.
11. **Repo-Regeln gehen vor:** AGENTS.md der App (z. B. kein `unwrap`, Formatvollständigkeit, Layering) und die Projekt-Skills (`photocraft-dev`, `figcraft-dev`, CI-Skills) gelten vollständig. Bei Widerspruch zu diesem Briefing gewinnt das Repo und der Bericht nennt es.

## Erlaubte Dateien

PhotoCraft: `crates/ui-egui/src/**` (Menü, Dialog-Anbindung, neues Modul `cloud.rs`), `crates/engine/src/file_cmds.rs` nur für einen Hook nach erfolgreichem Speichern, `apps/photocraft/**` (Feature-Flag, Abhängigkeiten), i18n-Kataloge, `Cargo.toml`/`Cargo.lock`. FigCraft: entsprechende Dateien in `crates/ui-egui`, `apps/figcraft`, i18n. Keine Änderungen an Format-Crates, Server-Code von FigCraft oder CI-Workflows.

## Tests (Pflicht)

- PhotoCraft und FigCraft: vorhandene CI-Befehle laut Skill (`photocraft-ci` bzw. `figcraft-ci`, nur Windows-Teil; WSL-Teil nur, wenn kein anderer Job läuft) grün.
- Neue Tests: ohne Server keine Netzwerk- und Schlüsselbund-Aufrufe; „Speichern“ auf verknüpfter Datei ruft genau einen Upload mit richtiger Basis auf (Mock-Transport); Cloud-Fehler verhindert lokales Speichern nicht; Schließen gibt Sperre frei.
- Manueller Ende-zu-Ende-Lauf gegen einen lokalen CraftCloud-Server (eigene Instanz auf freiem Port, nicht 8080): PSD öffnen, ändern, speichern, im Webpanel neue Version sehen, zweite Person sieht Sperre; Screenshots im Ausgabeordner.

## Abnahme

1. CI-Befehle der Apps grün (selbst ausgeführt, Logs).
2. Ende-zu-Ende-Lauf mit Screenshots für PhotoCraft und FigCraft.
3. App ohne Cloud-Einrichtung: unverändertes Verhalten (Test + kurzer manueller Check).
4. Entscheidungen 1–11 umgesetzt oder begründet abweichend.
5. Bericht nennt den Diff-Umfang je Repo und was für eine Einreichung bei den Maintainern noch fehlt.

## Nicht Teil von AC-05

VectorCraft, solange kein Repository vorliegt; Livebearbeitung in FigCraft (AC-06); Launcher (AC-07); Einreichung upstream.
