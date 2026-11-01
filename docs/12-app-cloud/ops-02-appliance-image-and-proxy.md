# OPS-02 — Komplett-Image, Datenbank-Erkennung und eigener Webserver (nginx/Apache)

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ops02`, Branch `feat/ops-02-appliance` (Basis: aktuelles `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\OPS-02\REPORT.md`. **Migration:** keine. **Review:** unabhängig (Betrieb, Netzwerkgrenze, Geheimnisse). **Keine Veröffentlichung:** keine Images in einer Registry, kein GitHub-Release, kein Push.

> **Stand 2026-11-01:** lokal umgesetzt im Worktree `wt-ops02` (Branch `feat/ops-02-appliance`), noch nicht committet und nicht gepusht; Abnahme und offene Punkte stehen im Bericht `D:\ArtCraft\output\craftcloud-codex\OPS-02\REPORT.md`, die Entscheidungen im [ADR-0029](../09-decisions/adr-0029-appliance-image-and-proxy.md). Abweichung von Entscheidung 4: `CRAFTCLOUD_TRUSTED_PROXIES` ist die Gateway-Adresse des Proxy-Netzes (`/32`), nicht `127.0.0.1/32`, weil ein Container die Loopback-Adresse des Hosts nie sieht.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ops02 (Branch feat/ops-02-appliance).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ops-01-install-and-releases.md (Ist-Stand der Installation)
und docs/12-app-cloud/ops-02-appliance-image-and-proxy.md vollständig und setze OPS-02 genau danach um.
Halte die Arbeitsweise aus dem README ein. Cargo nur über den Speicher-Launcher mit --gib 8.
`cargo xtask ci` muss wirklich laufen. Die Installationstests laufen mit Docker Desktop in eigenen
Compose-Projekten und werden am Ende vollständig entfernt (nur eigene Container, Volumes, Netze, Images).
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\OPS-02\REPORT.md im vorgegebenen Format.
```

## Ziel

Der Nutzer wünscht sich (2026-11-01) ein **Komplettpaket für den Server**: ein einziges CraftCloud-Image, das alles Nötige für den Betrieb enthält, einen Installer, der eine vorhandene Datenbank erkennt, und die Möglichkeit, statt des mitgelieferten Caddy einen vorhandenen **nginx oder Apache mit certbot** als Reverse Proxy zu verwenden. Viele Server haben bereits einen Webserver auf Port 80/443; dann darf CraftCloud nicht mit eigenem Caddy kollidieren.

## Ist-Stand (verifiziert 2026-11-01, main 74207d2)

- `deploy/compose/Dockerfile`: mehrstufig (Node für die Admin-Oberfläche, Rust für den Server, distroless `cc-debian13:nonroot`), `ENTRYPOINT ["/usr/local/bin/craftcloud-server"]`, Healthcheck `craftcloud-server healthcheck`. Image `${CRAFTCLOUD_IMAGE:-craftcloud-server:local}`.
- `deploy/compose/figcraft/Dockerfile`: eigenes Image `craftcloud-figcraft:ddda495a6ff4`, baut `figcraft-server` aus dem gepinnten FigCraft-Commit; `compose.figcraft.yaml` ist standardmäßig aktiv (FigCraft-Livelinks sollen immer laufen, Entscheidung 2026-10-31).
- `deploy/compose/compose.yaml`: Dienste `secrets-init`, `db` (postgres 18.6 per Digest), `db-init`, `migrate`, `server`, `caddy` (2.11 per Digest), `backup`/`restore` im Profil `tools`. Nur Caddy hängt am Netz `frontend` und veröffentlicht Ports; der Server vertraut `CRAFTCLOUD_TRUSTED_PROXIES` (Standard: Frontend-Subnetz).
- `deploy/install/install.sh` und `craftcloud-ctl` (OPS-01): bauen die Images **lokal** aus dem Release-Archiv (Option a), prüfen Ports 80/443, legen `/opt/craftcloud`, `/etc/craftcloud/craftcloud.env` und `/var/lib/craftcloud` an; systemd-Units für Stack und tägliches Backup.
- Client-IP-Ermittlung: `crates/craftcloud-api/src/client_ip.rs` — `X-Forwarded-For` nur von vertrauenswürdigen Proxys, `X-Forwarded-Proto` für HTTPS-Erkennung; `Host`, `X-Forwarded-Host` und `Forwarded` werden nie verwendet.
- Datenbank: nur PostgreSQL (sqlx). Die Migrationen nutzen nach Stichprobe keine Funktionen, die erst PostgreSQL 18 bietet; geprüft ist bisher nur 18.6.
- AC-12 (Speicherstufen, noch nicht gemerged) ergänzt nur `docs/06-operations/archive-storage.md` im Betriebsbereich; keine Überschneidung mit `deploy/`.

## Entscheidungen (fest, Koordinator 2026-11-01, im Entscheidungslog)

1. **Ein Image für alles, was CraftCloud selbst ausführt.** Neues Image `craftcloud:<version>` enthält `craftcloud-server`, die gebaute Admin-Oberfläche, `figcraft-server` (aus dem gepinnten Commit) und einen kleinen Rollenstarter. Rollen über das erste Argument: `serve` (Standard), `figcraft`, `migrate`, `backup`, `restore`, `healthcheck`, `all-in-one`. `all-in-one` startet Server und FigCraft-Dienst in **einem** Container unter einem minimalen Supervisor (eigener kleiner Rust-Starter oder `tini` plus Prozessüberwachung; beendet sich ein Kindprozess, wird er mit Backoff neu gestartet, beim Stoppsignal werden beide sauber beendet). Compose verwendet weiterhin getrennte Dienste aus **demselben** Image (bessere Isolation, getrennte Limits); `all-in-one` ist für `docker run`-Einzelinstallationen. Das bisherige `craftcloud-figcraft`-Image entfällt; die bisherigen Image-Variablen bleiben als Aliasse eine Version lang gültig.
2. **PostgreSQL bleibt ein eigener Container oder ein vorhandener Server**, nie im CraftCloud-Image. Begründung: Upgrades, Backups und Ausfälle der Datenbank sollen getrennt behandelbar bleiben. Andere Datenbanken (MySQL, MariaDB, SQLite, SQL Server) werden nicht unterstützt.
3. **Datenbank-Erkennung im Installer.** Neue Option `--db auto|bundled|external` (Standard `auto`). `auto` prüft: (a) ist `--database-url-file` gesetzt, wird diese Datenbank verwendet; (b) läuft auf dem Host ein PostgreSQL (Port 5432 auf localhost oder Unix-Socket, `pg_lsclusters`/`psql --version`, systemd-Unit `postgresql*`), meldet der Installer Version und fragt interaktiv, ob es genutzt werden soll (nicht-interaktiv: nur mit ausdrücklichem `--db external`); (c) wird MySQL/MariaDB erkannt, erklärt er, dass diese nicht unterstützt werden, und bietet den mitgelieferten PostgreSQL-Container an; (d) sonst `bundled`. Unterstützt: **PostgreSQL 16, 17 und 18** (Debian 13 liefert 17, Ubuntu 24.04 liefert 16). Ältere Versionen werden mit klarer Meldung abgelehnt. Für `external` erzeugt der Installer **kein** Superuser-Passwort, sondern gibt die SQL-Befehle für die beiden Rollen (Owner und Runtime, wie `db-init.sh`) als Datei aus, die der Betreiber selbst ausführt, oder führt sie mit einer vom Betreiber übergebenen Admin-URL-Datei aus. Verbindung zu einer externen Datenbank auf einem anderen Host nur mit `sslmode=verify-full` oder ausdrücklichem `--db-allow-insecure-local` für Loopback/Unix-Socket. Der Server prüft beim Start die PostgreSQL-Hauptversion und verweigert nicht unterstützte Versionen mit einer eindeutigen Meldung.
4. **Reverse Proxy wählbar.** Neue Option `--proxy auto|caddy|nginx|apache|none` (Standard `auto`). `auto`: läuft nginx oder Apache bereits (Port 80/443 belegt, Prozess bzw. systemd-Unit erkannt), wird diese Variante vorgeschlagen; sind 80/443 frei und kein Webserver installiert, bleibt der mitgelieferte Caddy (automatisches TLS) Standard. Bei `nginx`, `apache` und `none` startet kein Caddy; der Server-Container veröffentlicht nur `127.0.0.1:<port>` (Standard 8080) bzw. lauscht auf einem Unix-Socket, und `CRAFTCLOUD_TRUSTED_PROXIES` wird auf `127.0.0.1/32` gesetzt. Der Installer **schreibt nur Vorlagen** nach `/etc/craftcloud/proxy/` (`craftcloud.nginx.conf`, `craftcloud.apache.conf`) und gibt die Schritte aus; er ändert keine vorhandene Webserver-Konfiguration ohne `--proxy-install` und legt dann vorher eine Sicherung an, prüft mit `nginx -t` bzw. `apachectl configtest` und lädt nur bei Erfolg neu.
5. **Inhalt der Proxy-Vorlagen:** TLS-Server für die Domain, Weiterleitung von HTTP auf HTTPS, `proxy_pass` auf den lokalen Port, WebSocket-Upgrade für den FigCraft-Livepfad und die Live-Endpunkte (bestehende Pfade aus `compose.figcraft.yaml`/`proxy.caddy` übernehmen), `client_max_body_size`/`LimitRequestBody` passend zur maximalen Dateigröße (5 GiB plus Reserve, aus der Konfiguration abgeleitet), lange Timeouts für Uploads und Livesitzungen, `X-Forwarded-For` (nginx: `$proxy_add_x_forwarded_for` nur mit gesetztem `real_ip`-Schutz bzw. Überschreiben des eingehenden Headers) und `X-Forwarded-Proto https`, **kein** Weiterreichen von `X-Forwarded-Host`, HSTS, Request-Buffering für Uploads abgeschaltet (`proxy_request_buffering off`). Apache: `mod_proxy`, `mod_proxy_http`, `mod_proxy_wstunnel`, `mod_headers`, `mod_ssl` als Voraussetzung prüfen und benennen.
6. **certbot:** Bei `nginx`/`apache` nennt der Installer den passenden Befehl (`certbot --nginx -d <domain>` bzw. `certbot --apache -d <domain>`) und führt ihn nur mit `--proxy-install --certbot` aus; fehlt certbot, wird er **nicht** automatisch installiert, sondern der Paketname für Debian/Ubuntu genannt. Erneuerung bleibt beim certbot-Timer des Systems. Ohne certbot ist ein eigenes Zertifikat über Pfadangaben in der Vorlage möglich.
7. **Image-Verteilung ohne Registry:** `cargo xtask release <version>` erzeugt zusätzlich ein vorgebautes Image-Archiv `craftcloud-<version>-image-<arch>.tar.zst` (`docker save`, je Architektur amd64 und arm64 sofern lokal baubar, sonst nur amd64 und im Bericht nennen) mit Eintrag in `SHA256SUMS`. `install.sh --image-archive <datei>` lädt es per `docker load` nach Prüfsummenprüfung; ohne Archiv baut der Installer wie bisher lokal (Option a bleibt Fallback). So entfällt der lange Build auf dem Server, ohne dass ein privates Repository eine Registry braucht. Registry (GHCR) bleibt ein späterer Schritt.
8. **Einzelbefehl für den Start ohne Compose:** `docs/06-operations/install.md` beschreibt zusätzlich einen minimalen `docker run`-Weg mit `all-in-one` gegen eine externe oder separat gestartete PostgreSQL-Instanz (für Tests und kleine Installationen); Compose plus `install.sh` bleibt der empfohlene Weg.
9. **Windows Server:** nicht Ziel. Die Doku empfiehlt eine Linux-VM (Debian 13 oder Ubuntu 24.04) auf Hyper-V und erklärt kurz, warum (Linux-Container, WSL 2 auf Servern nicht empfohlen).
10. **Geheimnisse:** unverändert wie OPS-01: kein Skript gibt Passwörter oder Inhalte von Geheimnisdateien aus, auch nicht die Datenbank-URL einer externen Datenbank. Die Proxy-Vorlagen enthalten keine Geheimnisse.

## Erlaubte Dateien

`deploy/compose/**` (gemeinsames Image, Rollen, Wegfall des getrennten FigCraft-Images, Proxy-Profile), `deploy/install/**`, `deploy/proxy/**` (neu: nginx- und Apache-Vorlagen), `apps/craftcloud-server/src/**` (nur Rollenstarter, `all-in-one`, Prüfung der PostgreSQL-Version beim Start, Unix-Socket-Bind falls umgesetzt), `crates/craftcloud-db/src/**` (nur Versionsprüfung), `xtask/**` (Image-Archiv im Release), `docs/06-operations/**`, `docs/12-app-cloud/ops-02-appliance-image-and-proxy.md` (Statuszeile), neues ADR `docs/09-decisions/adr-0029-appliance-image-and-proxy.md`, `docs/gaps.md`, `CHANGELOG.md`.

Nicht ändern: Datenmodell, Migrationen, API-Verhalten, Admin-Oberfläche, andere Briefings, andere Worktrees.

## Tests (Pflicht)

- `cargo xtask ci` grün; Unit-Tests für Rollenstarter (unbekannte Rolle, Signalweitergabe, Neustart mit Backoff im `all-in-one`), PostgreSQL-Versionsprüfung (15 abgelehnt, 16–18 akzeptiert), `xtask release` mit Image-Archiv (Prüfsumme stimmt).
- `cargo xtask db-test` grün gegen PostgreSQL 18.6 **und** einmal gegen 16 (eigener Container mit eigenem Namen und Port, nicht `craftcloud-try-pg`; am Ende entfernt). Falls db-test die Version nicht parametrisieren kann, kleine Option ergänzen.
- `shellcheck` für alle Skripte, falls vorhanden; sonst `sh -n` und im Bericht nennen.
- **Installationstests** in `debian:trixie`-Containern mit gemountetem Docker-Socket, eigene Compose-Projektnamen, Ports oberhalb 18000:
  1. `--proxy caddy --db bundled --image-archive …` (kein lokaler Build): Setup-Status, Backup, Upgrade, Rollback, Deinstallation.
  2. `--proxy nginx`: nginx im Testcontainer installiert, `--proxy-install` mit selbstsigniertem Zertifikat statt certbot, Aufruf über nginx: Login-Seite, Upload einer Datei von 100 MiB ohne Abbruch, WebSocket-Verbindung des FigCraft-Livepfads, `X-Forwarded-For`-Spoofing von außen wird nicht übernommen (Audit/Rate-Limit sieht die echte Adresse).
  3. `--proxy apache`: wie 2 mit Apache, mindestens Login und WebSocket.
  4. `--db external` gegen einen separaten PostgreSQL-16-Container; MySQL-Erkennung mit einem MariaDB-Testcontainer liefert die erwartete Meldung.
  5. `docker run … all-in-one` gegen eine externe Datenbank: Server und FigCraft-Dienst laufen, `docker stop` beendet beide innerhalb des Stopp-Timeouts.
- Test, dass keine Ausgabe Geheimnisse enthält (Ausgaben der Installationstests nach den erzeugten Geheimnissen und der externen DB-URL durchsuchen).

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` (18.6 und 16) grün, Exitcodes im Bericht.
2. Installationstests 1–5 protokolliert.
3. `install.md` erklärt für Betreiber ohne Vorwissen die drei Wege (Caddy, vorhandener nginx/Apache mit certbot, `docker run`) und die Datenbankwahl; der Bericht nennt, was auf einer echten Debian-VM noch geprüft werden muss (echtes certbot-Zertifikat, Firewall).
4. Entscheidungen 1–10 umgesetzt oder begründet abweichend; ADR-0029 angelegt.
5. Aufräumen belegt: keine eigenen Docker-Ressourcen übrig, `dist/` und Build-Caches bereinigt.

## Nicht Teil von OPS-02

Registry/GHCR, Signatur von Images (cosign), `.deb`-Paket, Kubernetes/Helm, Windows-Server-Installation, automatische Installation von certbot oder Webservern, Hochverfügbarkeit, andere Datenbanken als PostgreSQL.
