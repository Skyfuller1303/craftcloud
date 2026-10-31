# OPS-01 — Installation auf eigenem Server und Releases

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ops01`, Branch `feat/ops-01-install` (Basis: aktuelles `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\OPS-01\REPORT.md`. **Migration:** keine. **Review:** unabhängig (Betrieb und Geheimnisse). **Keine Veröffentlichung:** kein GitHub-Release, keine Images in einer Registry, kein Push.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ops01 (Branch feat/ops-01-install).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ops-01-install-and-releases.md vollständig
und setze OPS-01 genau danach um. Halte die Arbeitsweise aus dem README ein.
Cargo nur über den Speicher-Launcher mit --gib 8. `cargo xtask ci` muss wirklich laufen; der Installationstest
läuft mit Docker Desktop in einem eigenen Compose-Projekt und wird am Ende vollständig entfernt.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\OPS-01\REPORT.md im vorgegebenen Format.
```

## Ziel

Ein Betreiber installiert CraftCloud auf einem eigenen Linux-Server mit wenigen Befehlen aus einem geprüften Release-Archiv, aktualisiert es später sicher und hat tägliche Backups. Es werden **keine vorgebauten Container-Images** verteilt (Entscheidung 2026-10-30, Option a): Das Installationsskript baut das Image lokal aus dem Quellarchiv. Das funktioniert auch, solange das Repository privat ist.

## Ist-Stand (verifiziert 2026-10-30, main 73a06bd)

- `deploy/compose/compose.yaml`: Dienste `secrets-init`, `db` (postgres 18.6, per Digest gepinnt), `db-init`, `migrate`, `server`, `caddy` (2.11, per Digest gepinnt), `backup` und `restore` im Profil `tools`; Netze `frontend` und `backend`. Zusätzlich `compose.setup.yaml` und `compose.metrics.yaml`. Seit 2026-10-31 gehört auch `compose.figcraft.yaml` (FigCraft-Begleitdienst, AC-06) standardmäßig zur Installation: `install.sh` und `craftcloud-ctl` übergeben es in jedem Compose-Aufruf, `--no-figcraft` beziehungsweise `craftcloud-ctl figcraft disable` schaltet es ab (siehe `docs/06-operations/install.md`).
- `deploy/compose/Dockerfile`: mehrstufiger Build (rust 1.99-trixie, node 24-trixie-slim, distroless cc-debian13 nonroot), alle Basis-Images per Digest gepinnt. Image-Name `${CRAFTCLOUD_IMAGE:-craftcloud-server:local}`.
- `deploy/compose/secrets-init.sh` erzeugt die Geheimnisdateien; `db-init.sh` legt die getrennten DB-Rollen an.
- `deploy/caddy/` enthält die Caddy-Konfiguration (TLS über ACME).
- Doku: `docs/06-operations/deployment.md` (Variablen, Compose, Setup, Backup), `docs/06-operations/backup-upgrade-runbook.md` (Backup, Upgrade, Restore).
- Server-CLI: `craftcloud-server migrate`, `craftcloud-server admin backup create`, Restore; der Server startet nicht bei ausstehenden Migrationen.
- Getestet ist der Stack bisher nur auf Docker Desktop mit WSL 2. Es gibt keine Release-Archive, keine Versionstags, kein Installationsskript, keinen systemd-Dienst und keinen Backup-Zeitplan. GitHub-Workflows gibt es nicht; CI läuft lokal über `cargo xtask ci`.

## Entscheidungen (fest)

1. **Zielplattform:** Linux x86_64 und arm64 mit Docker Engine ≥ 25 und Compose-Plugin ≥ 2.24. Offiziell getestet: Debian 13 (trixie) und Ubuntu 24.04 LTS. Windows Server ist **nicht** Ziel von OPS-01; Docker Desktop unter Windows bleibt eine Entwicklungsumgebung. Der Bericht nennt, was für Windows Server später nötig wäre.
2. **Release-Archiv:** `cargo xtask release <version>` erzeugt aus einem sauberen Git-Stand `craftcloud-<version>.tar.gz` (Inhalt per `git archive`, keine ignorierten Dateien, keine `target`/`node_modules`), `SHA256SUMS` und eine CycloneDX-SBOM (`craftcloud-<version>.cdx.json`, aus `Cargo.lock` und `package-lock.json`; vorhandenes Werkzeug oder eigener kleiner Generator, keine neue globale Installation). Ausgabe nach `dist/` (ignoriert). Der Befehl verweigert einen unsauberen Arbeitsbaum und eine Version, die nicht SemVer ist. Signatur der Prüfsummen ist vorbereitet (Platzhalter und Doku für minisign), wird aber nicht erzeugt, weil es noch keinen Projektschlüssel gibt.
3. **Versionierung:** SemVer, beginnend bei `0.1.0`, Tag `v<version>` (Tag nur lokal anlegen, wenn ausdrücklich beauftragt; in OPS-01 nicht). `craftcloud-server --version` zeigt Version und Git-Commit. Neue Datei `CHANGELOG.md` im Format „Keep a Changelog“.
4. **Installationsskript:** `deploy/install/install.sh`, POSIX `sh`, idempotent, ohne Root-Annahmen außer den ausdrücklich geprüften Schritten. Ablauf: Prüfsumme des Archivs gegen `SHA256SUMS` prüfen; Voraussetzungen prüfen (Docker, Compose-Version, freie Ports 80/443, mindestens 2 GiB RAM und 10 GiB freier Speicher, DNS-Auflösung der Domain als Warnung); Verzeichnisse `/opt/craftcloud/releases/<version>`, Symlink `/opt/craftcloud/current`, Konfiguration `/etc/craftcloud/craftcloud.env` (Rechte 0600), Daten `/var/lib/craftcloud/{storage,postgres,backups}`; Geheimnisse über das vorhandene `secrets-init` erzeugen (nie ausgeben, nie überschreiben); Image lokal bauen; `migrate`; Stack starten; `/health/ready` abwarten; am Ende Setup-URL und **Pfad** der Bootstrap-Geheimnisdatei ausgeben (nicht deren Inhalt). Optionen: `--domain`, `--acme-email`, `--data-dir`, `--http-port`/`--https-port` (für Tests), `--non-interactive`, `--dry-run`.
5. **Steuerbefehl:** `deploy/install/craftcloud-ctl` (POSIX `sh`) mit `status`, `logs`, `backup`, `upgrade <archiv>`, `rollback`, `uninstall`. `upgrade` prüft Prüfsumme, erstellt **zwingend** vorher ein Backup über den vorhandenen Backup-Dienst, entpackt nach `releases/<neu>`, baut das Image, führt `migrate` aus, schaltet `current` um und prüft `/health/ready`. Scheitert der Gesundheitscheck, schaltet `rollback` den Symlink zurück. Wurde schon migriert, beschreibt die Ausgabe den Restore aus dem eben erstellten Backup nach dem Runbook (kein automatischer Restore). `uninstall` entfernt Dienste und Programmdateien, lässt Daten und Konfiguration stehen, außer `--purge` mit ausdrücklicher Bestätigung.
6. **systemd:** `deploy/install/systemd/craftcloud.service` (startet und stoppt den Compose-Stack aus `current`), `craftcloud-backup.service` und `craftcloud-backup.timer` (täglich 03:15 mit zufälliger Verzögerung bis 30 min). Backups landen in `/var/lib/craftcloud/backups`; aufbewahrt werden die letzten 7 (Option `CRAFTCLOUD_BACKUP_KEEP`). Kopie außer Haus bleibt Aufgabe des Betreibers (Entscheidung aus P3).
7. **E-Mail fehlt weiterhin:** OPS-01 baut keinen SMTP-Versand. Die Installationsdoku sagt klar, dass Einladungen und Reset-Links bis dahin als Link weitergegeben werden.
8. **Geheimnisse und Logs:** Kein Skript gibt Passwörter, Tokens oder den Inhalt von Geheimnisdateien aus, auch nicht mit `--dry-run` oder `set -x`. Test dafür.
9. **Doku für Betreiber:** neue `docs/06-operations/install.md` in normaler Prosa für Admins ohne Projektkenntnis: benötigte Software (Linux, Docker Engine, Compose-Plugin; PostgreSQL und Caddy laufen im Stack mit), Mindest- und empfohlene Hardware, DNS und Firewall, TLS (ACME oder eigenes Zertifikat), Installation, erster Login, Upgrade, Backup und Restore, Deinstallation, Fehlerbehebung. `deployment.md` und das Runbook verweisen darauf.

## Erlaubte Dateien

`xtask/**` (Befehl `release`), `apps/craftcloud-server/src/**` (nur `--version`), `deploy/install/**` (neu), `deploy/compose/**` (nur wenn für feste Installationspfade nötig, bestehende Entwicklungsnutzung bleibt gleich), `.gitignore` (`dist/`), `CHANGELOG.md` (neu), `docs/06-operations/**`, `docs/07-roadmap/phase-03-dashboard.md` (Verweis beim Gate „externer Betreiber-Test“), neues ADR `docs/09-decisions/adr-0023-install-and-releases.md`, `docs/gaps.md`.

## Tests (Pflicht)

- `cargo xtask ci` grün; Unit-Tests für `xtask release` (unsauberer Baum, ungültige Version, Prüfsummen stimmen, Archiv enthält keine ignorierten Dateien).
- `shellcheck` für alle Skripte, falls lokal vorhanden; sonst im Bericht als nicht ausgeführt nennen und stattdessen `sh -n` ausführen.
- **Installationstest:** in einem Container `debian:trixie` mit gemountetem Docker-Socket von Docker Desktop: Archiv erzeugen, `install.sh --non-interactive --domain localhost --http-port 18180 --https-port 18543 --data-dir <Ausgabeordner>/data` ausführen, Setup-Status prüfen, `craftcloud-ctl backup`, ein zweites Archiv mit erhöhter Patch-Version, `craftcloud-ctl upgrade`, `rollback`, `uninstall --purge`. Eigenes Compose-Projekt `craftcloud-ops01`, nie Port 8080, nie `craftcloud-try-pg`. Am Ende alle eigenen Container, Volumes, Netze und Images (`craftcloud-server:ops01-*`) entfernen und das belegen.
- Test, dass kein Skript Geheimnisse ausgibt (Ausgabe des Installationstests nach den erzeugten Geheimnissen durchsuchen).

## Abnahme

1. `cargo xtask ci` grün (selbst ausgeführt, Exitcode im Bericht).
2. Installationstest von Installation bis Deinstallation protokolliert, inklusive Upgrade und Rollback.
3. `install.md` ist für einen Betreiber ohne Vorwissen verständlich; der Bericht nennt die Schritte, die der externe Betreiber-Test (P3-Gate) auf einer echten Debian-VM noch prüfen muss.
4. Entscheidungen 1–9 umgesetzt oder begründet abweichend; ADR-0023 angelegt.
5. Aufräumen belegt: keine eigenen Docker-Ressourcen übrig, `dist/` und Build-Caches bereinigt.

## Nicht Teil von OPS-01

Vorgebaute Images und Registry (GHCR), Debian-Paket `.deb`, Windows-Server-Installation, Kubernetes/Helm, SMTP, Hochverfügbarkeit, automatischer Restore, Veröffentlichung eines Releases.
