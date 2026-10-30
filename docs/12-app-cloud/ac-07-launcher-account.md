# AC-07 — Launcher-Konto und Dateibrowser

> **Repository:** craft-launcher (`D:\ArtCraft\craft-launcher`), nur lokal. **Worktree:** `D:\ArtCraft\craft-launcher-cloud`, Branch `feat/craftcloud-account` (legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-07\REPORT.md`. **Voraussetzung:** AC-01, AC-02, AC-03, AC-04 gemerged. „In App öffnen“ startet die App mit dem Cache-Pfad als Argument; läuft AC-05 parallel, genügt dieser Aufruf, die Verknüpfung übernimmt die App nach AC-05. **Keine Veröffentlichung** in `storytold/craft-launcher` oder Forks.

## Prompt für Codex

```
Lies D:\ArtCraft\CraftCloud\craftcloud\docs\12-app-cloud\README.md und
D:\ArtCraft\CraftCloud\craftcloud\docs\12-app-cloud\ac-07-launcher-account.md vollständig,
danach D:\ArtCraft\craft-launcher-cloud\AGENTS.md. Arbeite nur im Worktree
D:\ArtCraft\craft-launcher-cloud. Keine Commits, Pushes, PRs oder Kommentare.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-07\REPORT.md im vorgegebenen Format.
```

## Ziel

Im Craft Launcher kann man sich optional bei der CraftCloud-Instanz des Unternehmens anmelden: Server-Adresse, E-Mail und Passwort, bei Bedarf zweiter Faktor. Danach zeigt der Launcher ein Profil (Name, E-Mail, Organisationen und Rollen), einen Dateibrowser („Meine Dateien“, „Für mich freigegeben“), Freigaben und Sperrstatus, und öffnet Dateien direkt in der passenden installierten App. Ohne Anmeldung verhält sich der Launcher exakt wie heute.

## Ist-Stand (verifiziert 2026-10-30)

- Aufbau (`AGENTS.md`): `crates/core` (WASM-tauglich: Katalog, Versionen), `crates/native` (HTTP mit `ureq` 3.4/rustls, Erkennung, Download, Installation), `crates/ui-egui` (gemeinsame UI), `apps/artcraft-launcher` (eframe, Tray), `apps/artcraft-launcher-web`. egui/eframe 0.37, ureq 3.5, MSRV 1.95, Edition 2024 (Stand `origin/main` 3fad1d6).
- Einstellungen über eframe-Storage (`SETTINGS_KEY`, `app.ron`), Datenordner `ArtCraftLauncherData` (portabel über `portable.txt`).
- Katalog `SUITE` mit 12 Apps (`crates/core/src/catalog.rs`), Erkennung installierter Apps `crates/native/src/detect.rs`, Starten `process.rs`, UI ↔ Backend über `protocol.rs` (`Command`/`Event`).
- Kein Konto, keine Anmeldung (`login.rs` ist Autostart), kein Schlüsselbund.
- Regeln aus `AGENTS.md`: nur Rust, kein Tauri/Electron/Webview; kein `unwrap`/`expect`/`panic`, kein `unsafe`; Netzwerk ist feindlich; Downloads nur von `github.com/storytold/<app>/releases` mit SHA256SUMS; keine personenbezogenen Daten in Anfragen, generischer User-Agent; WASM muss bauen (OS-Code `cfg(not(target_arch = "wasm32"))`); i18n über `tl!` und TSV-Kataloge; Schichtung core → native → ui.

## API-Stand auf main (73a06bd, 2026-10-30)

AC-01 und AC-03 sind gemerged. Maßgeblich ist `contracts/openapi.yaml`; bei Abweichung gewinnt der Vertrag, und der Bericht nennt sie. Die Aufrufe laufen über das SDK aus AC-04, nicht direkt aus der App.

- **Anmeldung:** `POST /api/v1/auth/device/login` mit `email`, `password`, `client_kind` (`app`, `launcher` oder `cli`), `client_id` (`^[a-z0-9-]{1,32}$`, z. B. `photocraft`) und `device_name`. Die Antwort enthält `ccd_at_…` (15 min) und `ccd_rt_…`; Erneuerung über `POST /api/v1/auth/device/refresh`, Abmelden über `POST /api/v1/auth/device/logout`. Jeder wiederverwendete Refresh-Token widerruft die ganze Familie (keine Gnadenfrist): Refresh-Aufrufe müssen serialisiert werden, nach `401 REFRESH_TOKEN_INVALID` ist neu anzumelden. `401 SECOND_FACTOR_REQUIRED` mit `challenge_id` kommt mit AC-02.
- **Transport:** Die Geräte-Anmeldung verlangt TLS (`400 DEVICE_TLS_REQUIRED`), außer der Server ist mit einer Loopback-`CRAFTCLOUD_PUBLIC_URL` gestartet (z. B. `http://127.0.0.1:18095`). Lokale Ende-zu-Ende-Läufe nutzen genau diese Ausnahme. Bearer und Cookie gleichzeitig ergibt `400 AMBIGUOUS_CREDENTIALS`.
- **Fähigkeiten:** `GET /api/v1/info` liefert `api_version` (`1.0`), `features` (derzeit `sharing.grants`, `sharing.edit-locks`) und `server_mode`. Clients schalten Funktionen nur nach diesen Merkmalen frei; unbekannte Werte sind kein Fehler.
- **Sperren:** `POST /api/v1/files/{id}/lock` mit `{client_id, takeover}` erwirbt oder verlängert die Sperre (Herzschlag = derselbe Aufruf alle 60 s, Ablauf nach 300 s). `DELETE /api/v1/files/{id}/lock` gibt sie frei; `?force=true` nur für Workspace-Admins. Die Antwort `FileLock` enthält `holder_display_name`, `since`, `expires_at`, `mine` (gleiche Person) und `held_by_current_session`. Die Sperre gehört der Geräte-Sitzung der App: Eine zweite Instanz oder der Browser derselben Person sieht `mine = true`, aber `held_by_current_session = false` und muss ausdrücklich `takeover: true` senden.
- **Fehlercodes rund um Sperren:** `423 FILE_LOCKED` bei fremder Sperre auf Sperren, Upload-Abschluss (`PUT /uploads/{id}/content`), Versions-Commit, Wiederherstellen, Umbenennen und Papierkorb; `409 LOCK_NOT_SUPPORTED` für `app = figcraft`; `409 FILE_DELETED` / `PROJECT_DELETED` für Ziele im Papierkorb; `403` bei fehlendem Recht, `404` wenn das Objekt nicht sichtbar ist. Widerruf der Sitzung, Abmelden, Passwortwechsel oder Deaktivierung entfernen die Sperren sofort.
- **Hochladen:** `POST /api/v1/files/{id}/uploads`, dann `PUT /api/v1/uploads/{id}/content`, dann `POST /api/v1/files/{id}/versions` mit `If-Match: "version:<uuid>"` der Basisversion (erste Version: `If-None-Match: *`). Abweichender Kopf ergibt `412 VERSION_CONFLICT` mit `details.current_version_id`; der Upload bleibt gespeichert und kann neu angewendet oder als Kopie committet werden. Nur der Ersteller des Uploads darf ihn committen. Ein laufender Upload scheitert nicht am Ablauf des 15-min-Zugriffstokens.
- **Freigaben:** `GET/POST /api/v1/projects/{id}/grants`, `GET/POST /api/v1/files/{id}/grants`, `PATCH/DELETE /api/v1/grants/{id}` (Rollenänderung mit `If-Match: "grant-v<n>"`, sonst 428/412). Verwalten dürfen nur Workspace-Editoren und höher, nie Grant-Inhaber oder Gäste. Personensuche `GET /api/v1/organizations/{id}/members?query=` ist pro Person auf 60 Anfragen je Fenster begrenzt (`429 RATE_LIMITED`): Eingabe entprellen, frühestens ab 2 Zeichen suchen. „Für mich freigegeben“: `GET /api/v1/organizations/{id}/shared-with-me` (Cursor-Paginierung). Geräte der eigenen Person: `GET /api/v1/auth/devices`, `DELETE /api/v1/auth/devices/{id}`.
- **Folgen für AC-07:** `client_kind = launcher`, `client_id = craft-launcher`. Der Launcher nimmt selbst **keine** Sperre; er zeigt nur den Sperrstatus und bietet „Sperre aufheben“ (`?force=true`) an, wenn die Rolle es erlaubt (Antwort `403` als Hinweis, nicht als Fehler anzeigen). Die Dateiliste kommt aus den vorhandenen Workspace-, Projekt- und Datei-Routen plus `shared-with-me`.

## Stand nach AC-02 und AC-04 (main 907c720) und lokaler Testbetrieb

- **Nur lokal:** Gearbeitet wird ausschließlich in den vom Koordinator angelegten Git-Worktrees (eigene Ordner und Branches auf Basis von `origin/main` des jeweiligen Repos). Die Hauptcheckouts unter `D:\ArtCraft\<Repo>` werden weder verändert noch auf einen anderen Branch umgeschaltet. Keine Commits in fremde Repos, keine Pushes, PRs oder Kommentare.
- **SDK-Quelle:** Pfad-Abhängigkeit auf den festen SDK-Stand `D:\ArtCraft\CraftCloud\sdk-pin\crates\…` (`craftcloud-client`, `craftcloud-client-native`, `craftcloud-egui`). Dieser Worktree wird nur vom Koordinator verschoben; nichts darin ändern. Eine spätere Verteilung (Git-Abhängigkeit oder Veröffentlichung) ist nicht Teil dieser Aufgabe.
- **Versionen:** Die Apps stehen auf egui/eframe 0.37, ureq 3.5, MSRV 1.95. Das SDK wird vor dem Start auf dieselbe Linie gebracht; `cargo tree -d` darf für egui, eframe, epaint, ureq und rustls keine doppelten Versionen zeigen. Doppelte Versionen sind ein Blocker, der im Bericht steht, keine Umgehung durch Patchen der Apps.
- **2FA ist da:** `CloudClient::login` liefert bei aktivierter 2FA `CloudError` mit Challenge; `CloudClient::complete_second_factor(challenge, Proof::Totp(..) | Proof::Recovery(..))` schließt ab. Der Anmeldedialog in `craftcloud-egui` hat den zweiten Schritt bereits; die App muss nur den Ablauf durchreichen.
- **Schlüsselbund:** `BoundKeychainTokenStore` immer mit `.with_lock_dir(<App-Datenordner>/CraftCloud)` erzeugen, damit mehrere Instanzen der App die Token-Erneuerung serialisieren. Kein Temp-Ordner.
- **Abmelden:** `logout` kann nach lokalem Abmelden einen Fehler liefern, wenn die Server-Sitzung nicht beendet werden konnte; die App zeigt das als Hinweis an.
- **Fehlerarten:** `CloudError::Unreachable` (Server nicht erreicht, Sitzung bleibt) und `CloudError::Offline` (Ausgang unklar) beide als nicht blockierenden Hinweis mit „Erneut versuchen“ zeigen.
- **Sperren:** Herzschlag über `FileLeaseHeartbeat::start_keyed(<file_id>, …)`; vor erneutem Öffnen derselben Datei `wait_for_release(<file_id>, 5 s)`.
- **Lokaler Testserver:** eigene Instanz per `cargo run -p craftcloud-server` aus `sdk-pin` mit eigener Datenbank (Docker-Container mit eigenem Namen und Port, nicht `craftcloud-try-pg`, nicht Port 8080), `CRAFTCLOUD_PUBLIC_URL=http://127.0.0.1:<freier Port>` (Loopback, damit Geräte-Anmeldung ohne TLS funktioniert). Container, Daten und Schlüsselbund-Einträge am Ende entfernen und im Bericht belegen.
- **Speicher und Caches:** Cargo nur über `python -I D:\ArtCraft\outputigcraft-pr634-reviewun-memory-limited.py --gib 8 --json-log <log> -- <cargo.exe> …`, `CARGO_BUILD_JOBS=2`, nie über PowerShell. Build-Cache wie im `AGENTS.md` des Repos (PhotoCraft: `CARGO_BUILD_BUILD_DIR=D:\ArtCraft\PhotoCraft	arget`; FigCraft: `D:\ArtCraft\FigCraft	arget`); eigene Endausgabe pro Worktree. Am Ende `cargo clean --profile dev` für die eigenen Ausgaben, Platz vorher/nachher im Bericht.

## Entscheidungen (fest)

1. **Opt-in:** Neuer Bereich in den Einstellungen „CraftCloud“ und ein Navigationseintrag „Dateien“, der erst nach Einrichtung erscheint. Install-, Update- und Startpfade bleiben unverändert; kein Netzwerkzugriff zu CraftCloud ohne eingerichteten Server.
2. **Abgrenzung zur Download-Regel:** CraftCloud-Anfragen gehen nur an den vom Nutzer eingetragenen Server (`https://`, Loopback ausgenommen) und laden **nie** ausführbare Dateien. Die bestehende Regel „Downloads nur von GitHub-Releases“ bleibt für Apps unverändert. Der Bericht formuliert die nötige Ergänzung von `AGENTS.md` als Vorschlag (nicht selbst ändern).
3. **SDK:** `craftcloud-client` (in `crates/core` nutzbar), `craftcloud-client-native` (in `crates/native`), `craftcloud-egui` (in `crates/ui-egui`) aus AC-04 per Pfad-Abhängigkeit auf `D:\ArtCraft\CraftCloud\sdk-pin` (siehe Abschnitt oben). Netzwerk läuft im Backend-Thread über neue `Command`/`Event`-Varianten, nicht im UI-Thread.
4. **Anmeldung:** Formular E-Mail und Passwort aus `craftcloud-egui`, zweiter Schritt für TOTP oder Wiederherstellungscode (AC-02). Geräte-Sitzung mit `client_kind = launcher`, Gerätename aus Rechnername (Nutzer kann ändern). Erneuerungstoken im Schlüsselbund; ohne Schlüsselbund keine Speicherung. Passwort wird nie gespeichert.
5. **Profil:** Name, E-Mail, Organisationen mit Rolle, Hinweis bei Org-2FA-Pflicht ohne eingerichtetes 2FA (Link ins Webpanel), Geräte-Sitzungen (Liste, eigenes abmelden), „Abmelden“, „Im Webpanel öffnen“ (Systembrowser, nur `https`-URL des Servers).
6. **Dateibrowser:** Organisation → Workspace → Projekt → Dateien, plus „Für mich freigegeben“. Spalten Name, App-Symbol (aus Katalog), Größe, geändert, Sperre. Aktionen: „In <App> öffnen“, „Herunterladen…“ (Speicherort wählen), „Versionen“, „Teilen…“ (Grant-Dialog wie im Webpanel, AC-03), „Live öffnen“ für FigCraft (AC-06, öffnet `figcraft://`-Link), „Sperre aufheben“ nur für Berechtigte.
7. **In App öffnen:** App über `files.app` ↔ Katalog-ID zuordnen, installierte App über `detect.rs` finden, Start mit dem Argument `craftcloud://open?server=…&file=…` (AC-05). **Keine Tokenübergabe**; die App meldet sich selbst an. Ist die App nicht installiert: Hinweis mit „Installieren“ (bestehender Installationsweg). Unterstützt die installierte Version das Argument nicht (Erkennung über Versionsangabe im Katalog oder Rückmeldung), Fallback: Datei herunterladen und mit dem Pfad starten, ausdrücklich ohne Cloud-Verknüpfung und mit Hinweis.
8. **Kein Hintergrund-Sync,** keine Benachrichtigungen. Aktualisieren per Knopf und beim Öffnen der Ansicht.
9. **Datenschutz:** generischer User-Agent bleibt; Gerätename ist die einzige freiwillige Angabe. Logs enthalten weder Token noch E-Mail noch Dateinamen.
10. **WASM:** Web-Launcher baut weiter; Cloud-Bereich dort ausgeblendet (`cfg`), außer der Transport aus AC-04 ist WASM-fähig – dann Folgearbeit.
11. **i18n:** alle Texte über `tl!`, Englisch und Deutsch in den TSV-Katalogen.

## Erlaubte Dateien

`crates/core/src/**` (nur Typen/Zuordnung App-ID, keine I/O), `crates/native/src/**` (neues Modul `cloud.rs`, `protocol.rs`), `crates/ui-egui/src/**` (neue Ansichten `cloud_settings.rs`, `cloud_files.rs`), `apps/artcraft-launcher/**`, i18n-Kataloge, `Cargo.toml`/`Cargo.lock`. Nicht: Download-/Verifikationscode, Update-Mirror, `AGENTS.md`, CI-Workflows.

## Tests (Pflicht)

- Vorhandene Launcher-Prüfungen laut Repo (`cargo xtask` des Launchers bzw. die in `AGENTS.md` genannten Befehle) grün, inklusive WASM-Build.
- Neue Tests: ohne Einrichtung kein CraftCloud-Netzwerk und kein Schlüsselbund; Zuordnung `files.app` → Katalog; Startargument korrekt gebaut und URL-validiert (kein Einschleusen von Argumenten über Dateinamen oder Server-URL); Backend-Ablauf Login/2FA/Refresh/Abmelden mit Mock-Transport; Fehlerzustände 401/403/423/503/offline zeigen verständliche Hinweise.
- Manueller Ende-zu-Ende-Lauf gegen lokalen CraftCloud-Server (eigener Port): Anmelden mit 2FA, Dateien sehen, Datei teilen, Datei in PhotoCraft (AC-05-Build) öffnen, Sperre sichtbar, FigCraft-Livelink öffnen. Screenshots EN/DE, hell/dunkel.

## Abnahme

1. Launcher-Prüfungen grün (selbst ausgeführt, Logs), WASM-Build grün.
2. Ende-zu-Ende-Lauf mit Screenshots.
3. Ohne Einrichtung identisches Verhalten (Test + manueller Check).
4. Entscheidungen 1–11 umgesetzt oder begründet abweichend; Vorschlag zur `AGENTS.md`-Ergänzung im Bericht.
5. Bericht nennt, was für eine Einreichung beim Launcher-Maintainer fehlt (Abstimmung mit CollabCraft-Vorschlag „Launcher als Schlüsselbund der Suite“, siehe `docs/00-product/overlap-map.md`).

## Nicht Teil von AC-07

Registrierung eines systemweiten `craftcloud://`-Handlers, Single Sign-on für alle Apps über den Launcher, Organisationsverwaltung im Launcher, Benachrichtigungen, Einreichung upstream.
