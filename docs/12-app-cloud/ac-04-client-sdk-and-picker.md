# AC-04 — Client-SDK und Cloud-Dateidialog

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac04`, Branch `feat/ac-04-client-sdk` (Basis: `main` nach Merge von AC-01 und AC-03). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-04\REPORT.md`. **Review:** unabhängig.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac04 (Branch feat/ac-04-client-sdk).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-04-client-sdk-and-picker.md vollständig
und setze AC-04 genau danach um. Halte die Arbeitsweise aus dem README ein.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-04\REPORT.md im vorgegebenen Format.
```

## Ziel

Alle Craft-Apps und der Launcher benutzen **denselben** Rust-Client und **dieselben** egui-Dialoge für Anmeldung, „Aus CraftCloud öffnen…“, „In CraftCloud speichern…“, Konflikt und Sperre. Eine App braucht danach nur noch wenige Zeilen Anbindung (AC-05). Ohne Cloud-Konfiguration hat das SDK keinerlei Wirkung.

## Ist-Stand

- Vertrag skizziert in `docs/04-integrations/sdk-contract.md` (Pseudocode) und `docs/03-features/cc-f13-client-sdk-and-discovery.md`; `docs/04-integrations/launcher-and-apps.md` (Checkliste je App).
- Server: Projekte, Dateien, Upload-Sitzung mit Idempotency-Key, gestreamter `PUT`, Commit mit `If-Match`, Download mit `Repr-Digest`, Versionen; nach AC-01 Bearer-Geräte-Sitzungen, nach AC-03 Grants und Sperren.
- Ziel-Apps: egui/eframe **0.36**, `rfd` 0.17, Rust Edition 2024; PhotoCraft MSRV 1.95, Launcher 1.90, FigCraft ähnlich. Launcher nutzt `ureq` 3.4 (rustls, platform-verifier). Alle drei haben eine `wasm32`-Variante, die ohne native Teile bauen muss.

## Entscheidungen (fest)

1. **Drei Crates im CraftCloud-Workspace**, MSRV **1.90** (niedrigste der Ziel-Apps), Edition 2024, keine Abhängigkeit auf Server-Crates:
   - `crates/craftcloud-client` — Typen (DTOs aus OpenAPI von Hand, mit Vertragstest gegen `contracts/openapi.yaml` wie im Admin-UI), Fehlertypen, `CloudTransport`-Trait, Ablauflogik (Login, Refresh, Upload in Schritten, Commit, Download, Sperre mit Herzschlag). WASM-tauglich, kein I/O.
   - `crates/craftcloud-client-native` — Transport mit `ureq` 3 (rustls, Plattform-Zertifikatsprüfung, keine Option zum Abschalten der Prüfung), Token-Ablage im Schlüsselbund über die Crate `keyring` (aktuelle 3.x, Begründung und Lizenz im Bericht; Fallback, falls kein Schlüsselbund verfügbar: **keine** Speicherung, Anmeldung bei jedem Start), lokaler Cache, Herzschlag-Thread.
   - `crates/craftcloud-egui` — Dialoge und Widgets mit egui 0.36: Server hinzufügen, Anmelden (E-Mail, Passwort, zweiter Faktor aus AC-02), Konto-Kopf (Name, Organisation, abmelden), Dateibrowser (Organisation → Workspace → Projekt → Dateien, plus „Für mich freigegeben“), Speichern-Dialog (Ziel, Dateiname, neue Datei oder neue Version), Konfliktdialog („Beide behalten“, „Überschreiben“, „Abbrechen“), Sperrhinweis („In Bearbeitung von …“, „Schreibgeschützt öffnen“), Fortschritt mit Abbrechen. Texte über eine Schnittstelle, die die App mit ihrem eigenen i18n-System füllt (Standardtexte EN und DE mitliefern). Optik übernimmt die egui-Visuals der Gast-App, keine eigenen Farben.
2. **Server-Discovery:** neue Route `GET /api/v1/info` (ohne Anmeldung): Serverversion, API-Version, Funktionsschalter (`device_login`, `two_factor`, `grants`, `locks`, `figcraft_live`), Grenzen (maximale Dateigröße, Blockgröße), Formatregister. Der Client prüft API-Version und meldet Inkompatibilität verständlich.
3. **Formatregister** im Server (`crates/craftcloud-core/src/formats.rs`, bestehende `app`/`format`-Validierung prüfen und erweitern): bekannte Apps und Endungen, mindestens `photocraft`: `pcraft`, `psd`, `psb`, `ora`; `figcraft`: `figcraft`, `fig`; `vectorcraft`: `svg`, `ai`, `vcraft` (Endungen von VectorCraft beim Maintainer bestätigen, im Bericht als offen markieren); `generic` für alles andere. Inhalt bleibt ein undurchsichtiger Blob; der Server liest keine Dateiinhalte.
4. **Verknüpfung lokal ↔ Cloud:** Der Client führt pro App eine kleine JSON-Datei im App-Datenordner (`craftcloud-links.json`): lokaler Pfad → Server, `file_id`, Basisversion, zuletzt bekannter SHA-256. „Speichern“ auf einer verknüpften Datei schreibt lokal und lädt dann mit `If-Match: "version:<basis>"` hoch. Keine Verknüpfung, keine Cloud-Aktion.
5. **Öffnen:** Download in einen Cache-Ordner `…/CraftCloud/<server-hash>/<file_id>/<version_id>.<ext>`, Prüfung gegen `Repr-Digest`, dann Übergabe des lokalen Pfads an die App. Vorher Sperre nehmen (außer FigCraft); bei 423 Auswahl „Schreibgeschützt öffnen“ oder „Abbrechen“. Herzschlag läuft, solange die App die Datei offen meldet; beim Schließen freigeben.
6. **Konflikt:** 412 → Konfliktdialog. „Beide behalten“ legt eine neue Datei „Name (Kopie von <Person>, <Datum>)“ an; „Überschreiben“ committet mit der aktuellen Kopfversion als Basis nach erneuter Bestätigung; „Abbrechen“ lässt alles lokal. Die lokale Datei wird nie überschrieben.
7. **Offline / Server weg:** lokales Speichern funktioniert immer; Cloud-Fehler erscheinen als nicht blockierender Hinweis mit „Erneut hochladen“. Keine Warteschlange im Hintergrund.
8. **Tokens:** Zugriffstoken nur im Speicher; Erneuerungstoken im Schlüsselbund, Eintrag je (Server, Konto, App). Nie in Log, Panik-Meldung oder Einstellungsdatei. `Debug`-Implementierungen der Token-Typen geben `***` aus.
9. **TLS:** nur `https://`, außer Loopback. Keine Weiterleitungen mit Authorization-Header auf andere Hosts (ureq-Konfiguration prüfen, Test).
10. **Beispiel-Client:** `crates/craftcloud-client-native/examples/roundtrip.rs` meldet sich an, lädt eine Datei hoch, lädt sie zurück und vergleicht Bytes; dient als Abnahme aus CC-F13.
11. **Demo-App:** `apps/craftcloud-egui-demo` (eframe), die alle Dialoge gegen einen echten lokalen Server zeigt; Screenshots im Bericht.

## Erlaubte Dateien

Neue Crates `crates/craftcloud-client`, `crates/craftcloud-client-native`, `crates/craftcloud-egui`, `apps/craftcloud-egui-demo`; Workspace-`Cargo.toml` (Mitglieder, gemeinsame Abhängigkeiten), `Cargo.lock`; `crates/craftcloud-core/src/formats.rs` und Anbindung in der Validierung; `crates/craftcloud-api/src/lib.rs` + neues Modul `info.rs`; `contracts/openapi.yaml` + Snapshot; `xtask` nur falls ein WASM-Prüfschritt (`cargo check -p craftcloud-client --target wasm32-unknown-unknown`, wenn Target installiert) ergänzt wird; Doku `docs/04-integrations/sdk-contract.md` (vom Pseudocode zur echten API), `docs/03-features/cc-f13-client-sdk-and-discovery.md`, neues ADR `docs/09-decisions/adr-0021-client-sdk.md`, `docs/gaps.md`.

## Tests (Pflicht)

- Unit im Client: Ablaufzustände mit Mock-Transport (Login, Refresh bei 401, Wiederverwendung → Abmeldung, Upload-Fehler in jedem Schritt, 412, 423, 503, Offline), Vertragstest gegen OpenAPI, Token-`Debug` maskiert.
- Integration (`cargo xtask db-test` oder eigener ignorierter Test mit echtem Server): Beispiel-Client-Roundtrip byte-identisch, Sperre mit Herzschlag und Ablauf, Konflikt „Beide behalten“.
- egui: Snapshot- oder Zustandstests der Dialoge, soweit ohne GPU möglich (egui `Context` headless).

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün; Client-Crates bauen mit MSRV 1.90 (`cargo +1.90 check -p craftcloud-client -p craftcloud-client-native -p craftcloud-egui`, falls die Toolchain vorhanden ist, sonst als nicht ausgeführt melden).
2. `craftcloud-client` baut für `wasm32-unknown-unknown` oder der Bericht nennt genau, was fehlt.
3. Beispiel-Roundtrip und Demo-App gegen lokalen Server gelaufen (Log, Screenshots).
4. Entscheidungen 1–11 umgesetzt oder begründet abweichend.
5. ADR-0021 angelegt; offener Punkt „Verteilung des SDK an die Apps“ (privates Repo, Git-Abhängigkeit oder Veröffentlichung) im Bericht.

## Nicht Teil von AC-04

Einbau in PhotoCraft, FigCraft, VectorCraft (AC-05), Launcher (AC-07), Device Authorization Grant per Browser, Hintergrund-Sync, Teilen-Dialog in egui (folgt mit AC-07 aus derselben Crate, darf aber schon vorbereitet werden).
