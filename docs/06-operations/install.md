# CraftCloud auf einem eigenen Linux-Server installieren

Diese Anleitung richtet sich an Administratoren, die CraftCloud erstmals betreiben. CraftCloud speichert Konten, Organisationen und Dateien; PostgreSQL und der HTTPS-Proxy Caddy laufen im mitgelieferten Docker-Stack. Auf dem Host müssen Sie weder PostgreSQL noch Rust oder Node.js installieren. Der Installationsbefehl baut das Server-Image aus dem Quellarchiv; es werden keine fertigen CraftCloud-Images aus einer Registry geladen.

Der Stand ist ein lokaler Prototyp. Ein externer Betreiber muss die unten beschriebene Abnahme auf einer echten VM noch durchführen. Insbesondere sind eine Docker-Desktop-Probe und ein funktionierender Healthcheck keine Freigabe für einen Mehrmandanten-Produktivbetrieb: SMTP und E-Mail-Verifikation fehlen weiterhin.

## Host, Software und Netzwerk vorbereiten

Vorgesehene Plattformen sind Debian 13 (trixie) und Ubuntu 24.04 LTS auf x86_64 oder arm64. Benötigt werden Docker Engine mindestens 25, das Docker-Compose-Plugin mindestens 2.24 sowie die üblichen Linux-Werkzeuge `sh`, `tar`, `sha256sum`, `curl`, `ss`, `getent`, `df` und `install`. Verwenden Sie gepflegte Pakete aus einer vertrauenswürdigen Quelle und kontrollieren Sie `docker version` und `docker compose version`. Der Benutzer muss den Docker-Daemon erreichen können. Docker-Zugriff verleiht praktisch Root-Rechte; geben Sie ihn nur Administratoren.

Mindestens 2 GiB RAM und 10 GiB freier Speicher sind erforderlich. Für den lokalen Build und kleine Installationen empfehlen wir 4 CPU-Kerne, 8 GiB RAM und mindestens 30 GiB freien SSD-Speicher, zusätzlich zum Platz für Dateien, Versionen, Datenbank und sieben vollständige Backups. Die Mindestwerte garantieren keinen erfolgreichen Build bei zusätzlicher Last. Das Datenverzeichnis muss auf einem lokalen Linux-Dateisystem mit Hard-Links und Unix-Dateirechten liegen, beispielsweise ext4 oder XFS.

Richten Sie einen DNS-A-Eintrag und, nur bei funktionierendem IPv6, einen AAAA-Eintrag für beispielsweise `cloud.example.org` auf den Server ein. Öffnen Sie TCP 80 und 443; UDP 443 ist für HTTP/3 optional. PostgreSQL und der interne Server-Port werden nicht veröffentlicht. Der Build braucht ausgehenden Zugriff auf die in Dockerfile und Lockfiles genannten Paketquellen; Caddy braucht Zugriff auf die Zertifizierungsstelle. Schützen Sie SSH, halten Sie Host und Docker aktuell und synchronisieren Sie die Uhr.

Für die Standardverzeichnisse benötigt der Installer Schreibrechte auf `/opt`, `/etc`, `/var/lib` und das systemd-Verzeichnis. Verwenden Sie dafür beispielsweise `sudo`; das Skript prüft Voraussetzungen und Zugriffe, installiert aber keine Host-Pakete selbst.

## Release beziehen und prüfen

Beziehen Sie `craftcloud-0.1.0.tar.gz`, `SHA256SUMS` und `craftcloud-0.1.0.cdx.json` über den vom Projektverantwortlichen freigegebenen Kanal. Eine öffentliche Release-Adresse gibt es derzeit nicht. Die SBOM enthält die Rust- und npm-Abhängigkeiten aus beiden Lockfiles; sie ersetzt keinen Schwachstellenscan und beschreibt nicht alle Betriebssystempakete der Basis-Images.

```sh
sha256sum --check SHA256SUMS
tar -xzf craftcloud-0.1.0.tar.gz
sudo sh craftcloud-0.1.0/deploy/install/install.sh \
  --domain cloud.example.org --acme-email admin@example.org \
  --non-interactive ./craftcloud-0.1.0.tar.gz
```

Prüfen Sie die Prüfsummen vor dem Entpacken und Ausführen. Der Installer prüft das Archiv nochmals gegen die danebenliegende `SHA256SUMS`. Prüfsummen erkennen Übertragungsfehler; ein Angreifer, der Archiv und Prüfsummen austauscht, wird dadurch nicht erkannt. Beziehen Sie die Prüfsummen deshalb über einen separat vertrauenswürdigen Kanal. Die Datei `SHA256SUMS.minisign.txt` ist nur eine Vorbereitung, keine Signatur. Sobald ein Projektschlüssel existiert, kann der Herausgeber mit `minisign -Sm SHA256SUMS` signieren; Betreiber prüfen dann mit dem separat bezogenen öffentlichen Schlüssel, beispielsweise `minisign -Vm SHA256SUMS -P <public-key>`. Bis dahin darf niemand eine signierte Veröffentlichung behaupten.

`--dry-run` prüft Eingaben, Docker/Compose und Prüfsumme und beschreibt den Ablauf, ohne eine Installation anzulegen. RAM, Speicherplatz und belegte Ports werden beim echten Lauf geprüft. `--data-dir /srv/craftcloud` verlegt die Daten; verwenden Sie ein neues oder leeres, ausschließlich für diese Instanz bestimmtes Verzeichnis. Bereits eingerichtete Verzeichnisse müssen die passenden `.craftcloud-installation`-Marker enthalten. Installations-, Konfigurations- und Datenpfade dürfen einander nicht enthalten und bestehen ausschließlich aus Buchstaben, Ziffern, `/`, `.`, `_` und `-`. `--http-port` und `--https-port` dienen insbesondere lokalen Tests. Bei `--domain localhost` wird Caddys interne Test-CA verwendet. Ein solches Zertifikat ist nicht öffentlich vertrauenswürdig. In Produktion verwenden Sie Ihre echte Domain und die Standardports.

Der Installer legt versionierte Quellen unter `/opt/craftcloud/releases/<version>` an. `/opt/craftcloud/current` zeigt auf den aktiven Stand. Die Konfiguration liegt in `/etc/craftcloud/craftcloud.env` mit Modus 0600. Daten liegen standardmäßig unter `/var/lib/craftcloud/storage`, `postgres` und `backups`. Wiederholte Installation derselben Version bewahrt Konfiguration und bestehende Geheimnisse. Eine andere Version installieren Sie über `upgrade`.

## Erstes Konto und Betrieb

Nach dem Image-Build, der Datenbankinitialisierung und den Migrationen wartet das Skript auf `/health/ready`. Es nennt anschließend die Setup-URL und den Pfad zur Bootstrap-Geheimnisdatei, niemals das Geheimnis. Öffnen Sie die ausgegebene URL im Browser, lesen Sie den Token nur in einer privaten lokalen Administratorsitzung und tragen Sie ihn in das Setup ein. Legen Sie Ihr Betreiberkonto, ein eigenes Passwort und die erste Organisation an. Speichern Sie den Token nicht in Tickets, Logs, Shell-Argumenten oder Bildschirmaufzeichnungen.

Nach erfolgreichem Setup ist der Token serverseitig nicht erneut verwendbar. Entfernen Sie anschließend die Bootstrap-Bereitstellung mit dem folgenden Befehl; er prüft zuerst, dass die Ersteinrichtung abgeschlossen ist, und startet den Server ohne Bootstrap-Geheimnis neu. Bis dahin erhält auch ein Host-Neustart die Möglichkeit zum Setup.

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl setup-complete
```

Verwahren Sie die Konfigurationsdateien weiterhin ausschließlich administrativ. Prüfen Sie die Anmeldung, legen Sie einen Workspace an und laden Sie eine kleine Testdatei hoch und wieder herunter. Einladungen und Passwort-Reset-Links werden **nicht per E-Mail versendet**: Berechtigte Personen geben die jeweils einmalig angezeigten Links selbst über einen vertrauenswürdigen Kanal weiter.

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl status
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl logs
systemctl status craftcloud.service
systemctl list-timers craftcloud-backup.timer
curl --fail https://cloud.example.org/health/ready
```

Der systemd-Dienst startet und stoppt den Compose-Stack. Änderungen an der Konfiguration erfordern einen kontrollierten Neustart. Bearbeiten Sie `craftcloud.env` nur mit einem lokalen Administratorwerkzeug und erhalten Sie Modus 0600; geben Sie keine ganze Compose-Konfiguration oder Umgebungsdatei in öffentliche Diagnose-Logs aus.

## TLS mit eigenem Zertifikat

Die Standardinstallation verwendet ACME und erneuert Zertifikate durch Caddy. Die Konfiguration wird bei der Installation nach `/etc/craftcloud/caddy` kopiert und ausschließlich lesend in den Proxy eingebunden; Upgrades bewahren sie. Vergleichen Sie deshalb nach jedem Upgrade die neue Caddy-Vorlage mit Ihrer gespeicherten Konfiguration und übernehmen Sie nötige Sicherheitsänderungen.

Für ein eigenes Zertifikat legen Sie die vollständige Zertifikatskette unter `/etc/craftcloud/caddy/fullchain.pem` und den privaten Schlüssel unter `/etc/craftcloud/caddy/private.key` ab. Das Verzeichnis gehört root mit Modus 0700, der private Schlüssel root mit Modus 0600. Ändern Sie in `craftcloud.env` ausschließlich `CRAFTCLOUD_TLS_MODE=custom` und starten Sie den Stack mit `sudo systemctl restart craftcloud.service` neu. Die mitgelieferte Datei `tls-custom.caddy` referenziert genau diese beiden Dateien. Erneuerung und geschützte Bereitstellung des eigenen Zertifikats verantworten Sie; erneuern Sie es rechtzeitig und starten Sie den Proxy nach dem Austausch neu. Domain, Zertifikat, öffentliche URL und sichere Cookies müssen zusammenpassen. Es gibt keinen Installer-Schalter für eigene Zertifikate. Dokumentation zur manuellen Compose-Konfiguration: [deployment.md](deployment.md).

## Backups und Wiederherstellung

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl backup
```

Ein Backup enthält Datenbank und Dateiinhalte als konsistenten Stand. Der systemd-Timer startet täglich um 03:15 in der Zeitzone des Hosts, mit zufälliger Verzögerung bis 30 Minuten. Standardmäßig bleiben die letzten sieben erfolgreichen Backups erhalten; `CRAFTCLOUD_BACKUP_KEEP` in der Konfiguration ändert die Anzahl. Kontrollieren Sie regelmäßig Timer, Exitcodes, freien Speicher und das Backup-Alter in der Betreiberansicht. Ein angelegter Timer allein beweist keine erfolgreiche Sicherung.

Kopieren Sie Backups verschlüsselt auf ein anderes System und verwahren Sie Entschlüsselungsschlüssel getrennt. Konfiguration, Geheimnisse und Caddys Zertifikatsdaten sind nicht Teil des Anwendungsbackups und benötigen ein eigenes geschütztes Sicherungskonzept. Lokale Backups schützen nicht vor Verlust des Hosts. Die Aufbewahrung ist eine Anzahl von Backups, keine garantierte Zahl von Tagen.

Für einen Restore folgen Sie dem [Backup- und Upgrade-Runbook](backup-upgrade-runbook.md). Stellen Sie zuerst eine getrennte Instanz mit leerer Datenbank und leerem Speicher bereit; starten Sie den Server erst nach dem Restore. Prüfen Sie Manifest und getrennt aufbewahrte Prüfsumme, führen Sie den Compose-Dienst `restore` mit der Owner-Rolle aus und starten Sie anschließend Migration und Server. Ein Restore überschreibt niemals eine laufende Instanz. Prüfen Sie nachher Konten, Mitgliedschaften, offene Einladungen und Dateiprüfsummen: Nach dem Backup entzogene Rechte können im älteren Stand wieder gelten. Alle Personen müssen sich neu anmelden.

## Upgrade und Rückwechsel

Lesen Sie `CHANGELOG.md`, beschaffen und prüfen Sie das nächste Archiv und führen Sie vorher eine Restore-Probe Ihres aktuellen Backups auf einer getrennten Instanz durch. Planen Sie ein Wartungsfenster; ein Upgrade kann Dienste neu starten.

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl \
  upgrade /pfad/craftcloud-0.1.1.tar.gz
```

Der Befehl prüft das Archiv, erstellt zwingend ein Backup, baut das neue Image, führt Migrationen aus, wechselt `current` und prüft die Gesundheit. Scheitert ein Schritt, behandeln Sie den Exitcode als Fehler. Nach einer bereits begonnenen Migration kann der alte Server das Schema nicht mehr verwenden. Ein Rückwechsel des Symlinks ist dann keine Wiederherstellung der Datenbank. Die Ausgabe nennt das vorherige Backup und verweist auf den manuellen Restore; ein automatischer Restore findet nicht statt.

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl rollback
```

Verwenden Sie `rollback` nur nach Prüfung der Migrationslage. Falls ein Restore nötig ist, sichern Sie zunächst den fehlgeschlagenen Stand, stoppen Sie die Instanz und folgen Sie dem Runbook. Prüfen Sie nach jedem Upgrade oder Rückwechsel Anmeldung, Rollen, Download einer bekannten Datei mit Prüfsumme und einen neuen Upload.

## Deinstallation

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl uninstall
```

Das entfernt Dienste und Programmdateien, bewahrt aber Daten und Konfiguration. `uninstall --purge` fordert eine ausdrückliche Bestätigung und löscht auch Daten, lokale Backups und Geheimnisse. Bei nichtinteraktiver Eingabe muss zusätzlich `--confirm-purge` angegeben werden. Verwenden Sie dies nur für bewusst entbehrliche Instanzen oder nach geprüfter externer Sicherung.

## Fehlerbehebung und externe Abnahme

Bei einer falschen Prüfsumme brechen Sie ab und beschaffen Archiv und Prüfsummen erneut aus vertrauenswürdiger Quelle. Bei belegten Ports prüfen Sie den vorhandenen Dienst mit `ss`; beenden Sie keine fremden Dienste. Bei einer DNS-Warnung kontrollieren Sie A/AAAA-Einträge und Firewall. Bei Zertifikatsproblemen prüfen Sie Erreichbarkeit, Domain und Systemzeit. Bei fehlendem Speicher schaffen Sie Platz außerhalb lebender Datenbank- und Storage-Verzeichnisse. Bei Build-Fehlern kontrollieren Sie Netzwerk, freien Speicher und RAM. Bei fehlgeschlagenen Migrationen oder Readiness prüfen Sie `craftcloud-ctl status` und gezielt die Dienstlogs; führen Sie keinen blinden Datenbank-Downgrade aus.

Das offene [P3-Gate](../07-roadmap/phase-03-dashboard.md) benötigt eine projektfremde Person auf einer frischen Debian-13-VM. Sie muss nur mit dieser Anleitung Software, DNS und Firewall einrichten, ein echtes ACME-Zertifikat beziehen, installieren, einrichten, anmelden, Personen einladen, Rollen ändern sowie Dateien hoch- und herunterladen. Zusätzlich sind Host-Neustart mit systemd-Wiederanlauf, tatsächlicher Timerlauf mit Rotation, verschlüsselte Kopie außer Haus, Restore in eine getrennte Instanz, Upgrade, Fehlerfall nach Migration, Rückwechsel und beide Deinstallationsvarianten zu prüfen. Ubuntu 24.04, arm64, die Mindestversionen von Docker/Compose und eigene Zertifikate benötigen eigene Nachweise.

Windows Server ist kein Ziel dieser Installation. Eine spätere Unterstützung benötigt eine gesonderte Entscheidung über Linux-VM oder Windows-Container, Dienstverwaltung, Dateirechte, Pfad- und Netzwerkverhalten sowie einen vollständigen Installations- und Restore-Test. Docker Desktop unter Windows bleibt eine Entwicklungsumgebung.

## Releases lokal vorbereiten

Auf einem Entwicklerrechner mit Git, der Rust-Toolchain des Projekts und Python ab 3.11 erzeugt `cargo xtask release 0.1.0` aus einem sauberen Git-Stand das Archiv, die SBOM und Prüfsummen unter `dist/`. Ungültige SemVer-Versionen sowie geänderte oder unversionierte Dateien werden abgelehnt. Die Quelldateien stammen aus `git archive`; zusätzlich enthält das Archiv `CRAFTCLOUD-RELEASE.json` mit Version und Commit. `craftcloud-server --version` zeigt die Build-Identität. Ein Tag `v0.1.0`, eine Signatur oder Veröffentlichung entstehen durch diesen Befehl nicht.
