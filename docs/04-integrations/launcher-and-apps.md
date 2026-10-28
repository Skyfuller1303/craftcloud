# Integrationsstrategie für Craft Launcher und weitere Apps

## Grundregel

Der Launcher ist **kein Cloud-Login-Gateway**, ohne das Apps starten. Er ist ein optionaler Ort für Kontoverbindungen. `storytold/craft-launcher/AGENTS.md` schreibt Rust-only egui + WASM vor; keine React-UI in diese Codebasis. Für eine erste Integration wird eine einzelne App samt Maintainer gewählt, nicht der Launcher.

## Phase P4: First-app Spike

Ein minimales UI: `Save a Copy to CraftCloud…` bzw. `Open from CraftCloud…`. Nutzer setzt Server-URL (nicht automatisch erzwungen); sichere Login-Methode über Systembrowser; Liste autorisierter Workspaces/Projekte; neue Datei-Version hochladen mit Precondition. Status Cloud/Offline/Conflict wird angezeigt. Die lokale Datei bleibt Hauptsicherheitskopie. Kein Hintergrundsync ohne explizite Zustimmung.

## Beispiel WordCraft / GridCraft

- Formatunabhängiger Client-SDK-Aufruf: Stream von lokalen Bytes und Metadaten `app_id`, `content_type`, `format_version`.
- Anwendungsinterne Speicherung kontrolliert Dateilocks und Serialisierung; erst nach erfolgreicher lokalen Speicherung in Cloud uploaden.
- App öffnet Cloud-Datei als temporäre sichere lokale Kopie; ein beschädigter Download muss checksummed abgewiesen werden.
- Speichern bei Serverausfall: lokal weiterarbeiten, Upload später manuell/wiederholt; Status in UI.
- Browser/WASM-Build: CORS, Origin, Cookie/Token, Blob-/File API und Dateigrößen separat prüfen.

## Launcher P5

- Einstellungen: optional Cloud-Server URL, Status und „Verbindung verwalten“.
- Kein Passwort-/Admin-Key in Launcher-App-Settings.
- Nicht mit Release-Mirror oder App-Update-URL verwechseln.
- Alle Installer-, SHA-256- und Repo-Quelle-Regeln des Launchers bleiben unverändert.
- Launcher ohne Cloudkonfiguration verhält sich exakt wie vorher.

## Integrations-PR Checkliste pro App

- [ ] `AGENTS.md`, CLAUDE/ROADMAP, Build- und Plattformregeln dieser **App** geprüft.
- [ ] API Versions-/Capabilities-Handshake implementiert.
- [ ] Offline / 401 / 403 / 412 / 503 / damaged file getestet.
- [ ] OS-Keychain/Browsertoken nach Securityentscheidung; kein Klartext in App-Projekt.
- [ ] Originaldatei vor Fehler geschützt, eindeutige Sync-Indikatoren und Konflikt-UX.
- [ ] Windows/macOS/Linux und gegebenenfalls WASM getestet, sofern App unterstützt.
- [ ] Maintainer-reviewed eigenständiger PR; keine automatische Veröffentlichung.
