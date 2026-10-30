# AC-07 — Launcher-Konto und Dateibrowser

> **Repository:** craft-launcher (`D:\ArtCraft\craft-launcher`), nur lokal. **Worktree:** `D:\ArtCraft\craft-launcher-cloud`, Branch `feat/craftcloud-account` (legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-07\REPORT.md`. **Voraussetzung:** AC-01, AC-02, AC-03, AC-04 gemerged; AC-05 für „In App öffnen“. **Keine Veröffentlichung** in `storytold/craft-launcher` oder Forks.

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

- Aufbau (`AGENTS.md`): `crates/core` (WASM-tauglich: Katalog, Versionen), `crates/native` (HTTP mit `ureq` 3.4/rustls, Erkennung, Download, Installation), `crates/ui-egui` (gemeinsame UI), `apps/artcraft-launcher` (eframe, Tray), `apps/artcraft-launcher-web`. egui/eframe 0.36, MSRV 1.90, Edition 2024.
- Einstellungen über eframe-Storage (`SETTINGS_KEY`, `app.ron`), Datenordner `ArtCraftLauncherData` (portabel über `portable.txt`).
- Katalog `SUITE` mit 12 Apps (`crates/core/src/catalog.rs`), Erkennung installierter Apps `crates/native/src/detect.rs`, Starten `process.rs`, UI ↔ Backend über `protocol.rs` (`Command`/`Event`).
- Kein Konto, keine Anmeldung (`login.rs` ist Autostart), kein Schlüsselbund.
- Regeln aus `AGENTS.md`: nur Rust, kein Tauri/Electron/Webview; kein `unwrap`/`expect`/`panic`, kein `unsafe`; Netzwerk ist feindlich; Downloads nur von `github.com/storytold/<app>/releases` mit SHA256SUMS; keine personenbezogenen Daten in Anfragen, generischer User-Agent; WASM muss bauen (OS-Code `cfg(not(target_arch = "wasm32"))`); i18n über `tl!` und TSV-Kataloge; Schichtung core → native → ui.

## Entscheidungen (fest)

1. **Opt-in:** Neuer Bereich in den Einstellungen „CraftCloud“ und ein Navigationseintrag „Dateien“, der erst nach Einrichtung erscheint. Install-, Update- und Startpfade bleiben unverändert; kein Netzwerkzugriff zu CraftCloud ohne eingerichteten Server.
2. **Abgrenzung zur Download-Regel:** CraftCloud-Anfragen gehen nur an den vom Nutzer eingetragenen Server (`https://`, Loopback ausgenommen) und laden **nie** ausführbare Dateien. Die bestehende Regel „Downloads nur von GitHub-Releases“ bleibt für Apps unverändert. Der Bericht formuliert die nötige Ergänzung von `AGENTS.md` als Vorschlag (nicht selbst ändern).
3. **SDK:** `craftcloud-client` (in `crates/core` nutzbar), `craftcloud-client-native` (in `crates/native`), `craftcloud-egui` (in `crates/ui-egui`) aus AC-04 per Pfad-Abhängigkeit. Netzwerk läuft im Backend-Thread über neue `Command`/`Event`-Varianten, nicht im UI-Thread.
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
