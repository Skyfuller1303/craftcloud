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

## API-Stand auf main (73a06bd, 2026-10-30)

AC-01 und AC-03 sind gemerged. Maßgeblich ist `contracts/openapi.yaml`; bei Abweichung gewinnt der Vertrag, und der Bericht nennt sie. Die Aufrufe laufen über das SDK aus AC-04, nicht direkt aus der App.

- **Anmeldung:** `POST /api/v1/auth/device/login` mit `email`, `password`, `client_kind` (`app`, `launcher` oder `cli`), `client_id` (`^[a-z0-9-]{1,32}$`, z. B. `photocraft`) und `device_name`. Die Antwort enthält `ccd_at_…` (15 min) und `ccd_rt_…`; Erneuerung über `POST /api/v1/auth/device/refresh`, Abmelden über `POST /api/v1/auth/device/logout`. Jeder wiederverwendete Refresh-Token widerruft die ganze Familie (keine Gnadenfrist): Refresh-Aufrufe müssen serialisiert werden, nach `401 REFRESH_TOKEN_INVALID` ist neu anzumelden. `401 SECOND_FACTOR_REQUIRED` mit `challenge_id` kommt mit AC-02.
- **Transport:** Die Geräte-Anmeldung verlangt TLS (`400 DEVICE_TLS_REQUIRED`), außer der Server ist mit einer Loopback-`CRAFTCLOUD_PUBLIC_URL` gestartet (z. B. `http://127.0.0.1:18095`). Lokale Ende-zu-Ende-Läufe nutzen genau diese Ausnahme. Bearer und Cookie gleichzeitig ergibt `400 AMBIGUOUS_CREDENTIALS`.
- **Fähigkeiten:** `GET /api/v1/info` liefert `api_version` (`1.0`), `features` (derzeit `sharing.grants`, `sharing.edit-locks`) und `server_mode`. Clients schalten Funktionen nur nach diesen Merkmalen frei; unbekannte Werte sind kein Fehler.
- **Sperren:** `POST /api/v1/files/{id}/lock` mit `{client_id, takeover}` erwirbt oder verlängert die Sperre (Herzschlag = derselbe Aufruf alle 60 s, Ablauf nach 300 s). `DELETE /api/v1/files/{id}/lock` gibt sie frei; `?force=true` nur für Workspace-Admins. Die Antwort `FileLock` enthält `holder_display_name`, `since`, `expires_at`, `mine` (gleiche Person) und `held_by_current_session`. Die Sperre gehört der Geräte-Sitzung der App: Eine zweite Instanz oder der Browser derselben Person sieht `mine = true`, aber `held_by_current_session = false` und muss ausdrücklich `takeover: true` senden.
- **Fehlercodes rund um Sperren:** `423 FILE_LOCKED` bei fremder Sperre auf Sperren, Upload-Abschluss (`PUT /uploads/{id}/content`), Versions-Commit, Wiederherstellen, Umbenennen und Papierkorb; `409 LOCK_NOT_SUPPORTED` für `app = figcraft`; `409 FILE_DELETED` / `PROJECT_DELETED` für Ziele im Papierkorb; `403` bei fehlendem Recht, `404` wenn das Objekt nicht sichtbar ist. Widerruf der Sitzung, Abmelden, Passwortwechsel oder Deaktivierung entfernen die Sperren sofort.
- **Hochladen:** `POST /api/v1/files/{id}/uploads`, dann `PUT /api/v1/uploads/{id}/content`, dann `POST /api/v1/files/{id}/versions` mit `If-Match: "version:<uuid>"` der Basisversion (erste Version: `If-None-Match: *`). Abweichender Kopf ergibt `412 VERSION_CONFLICT` mit `details.current_version_id`; der Upload bleibt gespeichert und kann neu angewendet oder als Kopie committet werden. Nur der Ersteller des Uploads darf ihn committen. Ein laufender Upload scheitert nicht am Ablauf des 15-min-Zugriffstokens.
- **Folgen für AC-05:** Die App legt neue Cloud-Dateien mit `app` = Katalog-ID (`photocraft`, `figcraft`) und passendem `format` an, weil davon Sperren und Formatregister abhängen. „Schreibgeschützt öffnen“ gilt bei `423` und bei fehlendem Bearbeitungsrecht (Viewer-Grant, Gast). Tritt `423` erst beim Upload-Abschluss oder Commit auf, bleibt die lokale Datei gespeichert und der Dialog bietet „Als Kopie speichern“ an. FigCraft-Dateien nehmen keine Sperre und behandeln `409 LOCK_NOT_SUPPORTED` nicht als Fehler.

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
