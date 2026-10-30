# AC-06 — FigCraft-Livelinks über CraftCloud

> **Repository:** CraftCloud (Server, Compose, Webpanel); FigCraft nur lesend und für einen lokalen Spike. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac06`, Branch `feat/ac-06-figcraft-live`; Spike in `D:\ArtCraft\FigCraft-cloud-spike` (detached, nur lokal). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-06\REPORT.md`. **Migration:** `0018_figcraft_live.sql`. **Voraussetzung:** AC-03 gemerged (Grants), AC-02 gemerged (Serverschlüssel für Verschlüsselung). **Review:** unabhängig, sicherheitsrelevant.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac06 (Branch feat/ac-06-figcraft-live).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-06-figcraft-live-hosting.md vollständig.
Beginne mit Teil A (Spike) und schreibe dessen Ergebnis in den Bericht, bevor du Teil B baust.
Wenn der Spike eine der Annahmen A1–A4 widerlegt, baue Teil B nur so weit, wie es ohne
Änderung an FigCraft möglich ist, und beschreibe die Lücke. Halte die Arbeitsweise aus dem README ein.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-06\REPORT.md im vorgegebenen Format.
```

## Ziel

Eine FigCraft-Datei in CraftCloud kann **live** geöffnet werden. Jede Person mit Bearbeitungs- oder Leserecht bekommt einen persönlichen FigCraft-Link, der rund um die Uhr funktioniert, weil CraftCloud den FigCraft-Server dauerhaft betreibt. Rechteentzug in CraftCloud trennt die Person sofort. Der Stand wird regelmäßig als Version in CraftCloud gesichert.

## Ist-Stand (verifiziert 2026-10-30)

- `figcraft-server` (`D:\ArtCraft\FigCraft\apps\figcraft-server`, Doku `docs/server.md`, `docs/collab-protocol.md`): eigenständiger WebSocket-Server, Start `figcraft-server --listen … --data …`, `GET /healthz`, Konfiguration `server.toml`, Packaging unter `packaging/server/` (Dockerfile, Caddyfile, systemd).
- Keine Nutzerkonten. Zugriff über Links (`edit` oder `view`, je Datei, Token nur als SHA-256 gespeichert, Widerruf trennt sofort), Serverschlüssel verwaltet alle Dateien (`Manage{key}`), Open-Publish optional mit Owner-Token.
- Linkformate: `figcraft://join?server=wss://host/&file=<id>&token=<token>` und `ws://host:port/?file=<id>&token=<token>`.
- Protokoll `PROTOCOL = 2`, `WIRE_VERSION = 1`; Erweiterungen über Wörter in `client_version`.
- Persistenz: `<data>/files/<id>/{meta.json,links.json,snapshot.bin,log.bin,…}` – Kollaborationszustand, nicht `.figcraft`.
- CraftCloud: `docs/04-integrations/figcraft-adapter.md` (Varianten A–D), ADR-0007 (Proposed), CC-F16. Parallele Initiative CraftHub (FigCraft-Protokoll v3 mit Konten, figcraft#357) laut `docs/00-product/overlap-map.md`.

## Teil A — Spike (zuerst, Ergebnis im Bericht)

Prüfe im FigCraft-Quelltext und, wo nötig, mit einem lokal gebauten `figcraft-server` (eigener Port, eigener Datenordner unter `D:\ArtCraft\output\craftcloud-codex\AC-06\spike\`):

- **A1 Veröffentlichen per Serverschlüssel:** Kann ein Client mit `Manage{key}` eine Datei aus einem `.figcraft`-Dokument anlegen (oder gibt es einen CLI-Weg, z. B. `figcraft-cli` oder `figcraft-server files import`)? Welche Nachrichten/Befehle?
- **A2 Links verwalten:** Links für eine Datei mit Zugriff `edit`/`view` anlegen und widerrufen per Protokoll oder CLI, Rückgabe des Tokens, Trennen beim Widerruf.
- **A3 Exportieren:** Den aktuellen Stand einer Live-Datei als `.figcraft` zurückholen (Protokoll, CLI oder Bibliothek `crates/collab*`), ohne den Server anzuhalten.
- **A4 Pfad oder Host:** Funktioniert `figcraft-server` hinter einem Pfadpräfix (`wss://cloud.example/figcraft/`) oder nur auf eigenem Host/Port? Was erwartet `public_url`?

Für jede Annahme: bestätigt / widerlegt / teilweise, mit Datei:Zeile. Ohne bestätigtes A1–A3 kein Teil B außer Compose-Dienst und Konfiguration.

## Teil B — Umsetzung

### Entscheidungen (fest, sofern der Spike sie trägt)

1. **Begleitdienst:** Compose-Dienst `figcraft` aus einem gepinnten FigCraft-Release (Tarball mit SHA-256-Prüfung oder Build aus gepinntem Commit; Digest/Commit im Bericht), nonroot, read-only Root, eigenes Volume `figcraft-data`, nur im internen Netz erreichbar; Caddy leitet WebSockets weiter (eigener Host `CRAFTCLOUD_FIGCRAFT_DOMAIN`, Standard `figcraft.<CRAFTCLOUD_DOMAIN>`, außer A4 bestätigt Pfadpräfix). Open-Publish **aus**: nur CraftCloud veröffentlicht. Serverschlüssel als Secret-Datei, nur für `server` und `figcraft` lesbar.
2. **Konfiguration CraftCloud:** `CRAFTCLOUD_FIGCRAFT_URL` (intern, für Verwaltung), `CRAFTCLOUD_FIGCRAFT_PUBLIC_URL` (für Links), `CRAFTCLOUD_FIGCRAFT_KEY_FILE`. Fehlt sie, ist die Funktion aus (`/api/v1/info` meldet `figcraft_live: false`).
3. **Tabellen:** `figcraft_live_files` (`file_id` PK, `org_id`, `figcraft_file_id`, `published_from_version_id`, `last_snapshot_version_id`, `last_snapshot_at`, `state`) und `figcraft_links` (`id`, `file_id`, `user_id`, `access` `edit`|`view`, `figcraft_link_id`, `token_ciphertext` (verschlüsselt mit dem Serverschlüssel aus AC-02), `created_at`, `revoked_at`). Ein Link je (Datei, Person, Zugriff).
4. **Routen:** `POST /files/{id}/live` (veröffentlicht bei Bedarf, braucht Leserecht; legt für den Akteur einen Link mit `edit` bei Bearbeitungsrecht, sonst `view` an oder liefert den bestehenden; Antwort `{join_url, web_url?, access}`), `GET /files/{id}/live` (Status, Teilnehmerzahl falls verfügbar), `POST /files/{id}/live/snapshot` (Version jetzt sichern, Bearbeitungsrecht), `DELETE /files/{id}/live` (Livebetrieb beenden, Workspace-Admin; Links widerrufen, letzter Stand als Version).
5. **Rechte folgen CraftCloud:** Bei Entzug oder Herabstufung (Grant, Workspace-Rolle, Mitgliedschaft, Konto deaktiviert) widerruft CraftCloud die betroffenen FigCraft-Links in derselben Anfrage nach dem Commit (ausgehender Aufruf außerhalb der DB-Transaktion, mit Wiederholung). Zusätzlich gleicht der Wartungsjob alle 5 Minuten Links gegen Rechte ab und widerruft Überschüsse (Ausfallsicherheit). Maximale Widerrufslatenz im Bericht messen und in ADR festhalten.
6. **Sicherung als Version:** Alle 15 Minuten bei Änderungen und beim Beenden exportiert CraftCloud den Stand als `.figcraft` und legt eine neue Version an (Akteur „FigCraft-Live“, Audit mit Teilnehmern, soweit bekannt). Uploads außerhalb von FigCraft auf eine Live-Datei: 409 `FILE_IS_LIVE` mit Hinweis.
7. **Link zeigen:** Webpanel-Zeilenaktion „Live öffnen“ zeigt den persönlichen Link mit Kopieren und „In FigCraft öffnen“ (`figcraft://`-URL). Der Link ist an die Person gebunden; wer ihn weitergibt, gibt Zugriff weiter – Hinweis im Dialog, Widerruf über „Link erneuern“ (neuer Link, alter widerrufen).
8. **Audit:** `figcraft.published`, `figcraft.link_issued`, `figcraft.link_revoked`, `figcraft.snapshot`, `figcraft.unpublished`. Nie Token.
9. **Backup:** Restore setzt `figcraft_live_files` auf „neu veröffentlichen“ und Links auf widerrufen (Tokens des alten FigCraft-Datenordners gelten nicht mehr); Runbook ergänzen.

### Erlaubte Dateien

`crates/craftcloud-api/src/**` (neues Modul `figcraft.rs`), `crates/craftcloud-db/**` (Migration `0018_figcraft_live.sql`), `crates/craftcloud-core/src/policy.rs`, ggf. neue Crate `crates/craftcloud-figcraft` (Client zum FigCraft-Verwaltungsprotokoll; Abhängigkeit auf FigCraft-Crates nur per gepinntem Git-Rev oder Pfad, Begründung im Bericht), `crates/craftcloud-backup/**`, `apps/craftcloud-server/src/**`, `apps/craftcloud-admin/src/**`, `deploy/compose/**`, `contracts/openapi.yaml` + Snapshot, Doku `docs/04-integrations/figcraft-adapter.md`, `docs/03-features/cc-f16-figcraft-collaboration-adapter.md`, `docs/06-operations/deployment.md`, `docs/05-security/threat-model.md`, neues ADR `docs/09-decisions/adr-0022-figcraft-live-hosting.md` (ersetzt ADR-0007 teilweise), `docs/gaps.md`. In FigCraft **keine** Änderungen (Spike nur lesend und mit lokalem Build).

## Tests (Pflicht)

- Unit: Linkverwaltung mit Mock-FigCraft, Abgleich Rechte ↔ Links, Verschlüsselung der Tokens.
- Integration (ignorierter Test oder `db-test` mit echtem `figcraft-server` aus dem Spike-Build): veröffentlichen, Link für Editor und Viewer, Viewer kann nicht bearbeiten (FigCraft verweigert), Grant entziehen trennt die Verbindung, Snapshot erzeugt byte-gleich ladbare `.figcraft`-Version, Server-Neustart von FigCraft: Links gelten weiter.
- Compose: Stack mit `figcraft` auf hohen Ports hochfahren, Live öffnen über Caddy (`wss`), Logs im Ausgabeordner; eigenes Compose-Projekt `craftcloud-ac06`, am Ende `down -v`.

## Abnahme

1. Spike-Ergebnis A1–A4 mit Belegen.
2. `cargo xtask ci` und `cargo xtask db-test` grün; Integrations- und Compose-Lauf protokolliert.
3. Widerruf trennt messbar (Latenz im Bericht); Abgleichsjob widerruft Überschüsse (Test).
4. Bestehende FigCraft-Selbsthosting-Nutzung unverändert (keine Änderung an FigCraft).
5. ADR-0022 angelegt; offene Abstimmung mit FigCraft-Maintainern und CraftHub im Bericht.

## Nicht Teil von AC-06

Änderungen am FigCraft-Protokoll (Variante B), Konten in FigCraft, öffentliche Links ohne Anmeldung, Live-Bearbeitung für andere Apps.
