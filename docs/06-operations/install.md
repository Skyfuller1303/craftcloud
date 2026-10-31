# CraftCloud auf einem eigenen Linux-Server installieren

Diese Anleitung richtet sich an Administratoren, die CraftCloud erstmals betreiben. CraftCloud speichert Konten, Organisationen und Dateien; PostgreSQL und der HTTPS-Proxy Caddy laufen im mitgelieferten Docker-Stack. Auf dem Host müssen Sie weder PostgreSQL noch Rust oder Node.js installieren. Der Installationsbefehl baut das Server-Image aus dem Quellarchiv; es werden keine fertigen CraftCloud-Images aus einer Registry geladen.

Der Stand ist ein lokaler Prototyp. Ein externer Betreiber muss die unten beschriebene Abnahme auf einer echten VM noch durchführen. Insbesondere sind eine Docker-Desktop-Probe und ein funktionierender Healthcheck keine Freigabe für einen Mehrmandanten-Produktivbetrieb. SMTP und E-Mail-Verifikation müssen für diesen Betrieb ausdrücklich eingerichtet und geprüft werden.

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

Der Installer legt versionierte Quellen unter `/opt/craftcloud/releases/<version>` an. `/opt/craftcloud/current` zeigt auf den aktiven Stand. Die Konfiguration liegt in `/etc/craftcloud/craftcloud.env` mit Modus 0600. Daten liegen standardmäßig unter `/var/lib/craftcloud/storage`, `postgres` und `backups`. Wiederholte Installation derselben Version bewahrt Konfiguration und bestehende Geheimnisse. Ein abgebrochener Erstlauf lässt sich durch einen erneuten Aufruf mit demselben Archiv fortsetzen: Das Archiv wird zuerst in ein Staging-Verzeichnis entpackt und erst nach der Prüfung atomar umbenannt, sodass nie ein halbes Release zurückbleibt. Eine andere Version installieren Sie über `upgrade`.

Neben den Datenbank- und Token-Geheimnissen erzeugt der Installer einmalig einen Schlüssel für die Zwei-Faktor-Anmeldung (`CC_SECRET_TWO_FACTOR_KEY` in `craftcloud.env`, Base64 von 32 Zufallsbytes). Ohne ihn antwortet die 2FA-Einrichtung mit `503`. Wiederholung und Upgrade ändern einen vorhandenen Schlüssel nie; fehlt er in einer älteren Konfiguration, wird er beim `upgrade` ergänzt. Der Schlüssel ist **nicht** im Backup: Bewahren Sie eine getrennte, geschützte Kopie auf, denn ein Restore der Datenbank braucht denselben Schlüssel, sonst müssen alle Personen 2FA neu einrichten. `craftcloud-ctl status` zeigt, ob er vorhanden ist; sein Wert wird nirgends ausgegeben. Eine Rotation mit altem Schlüssel (`CRAFTCLOUD_SECRET_KEY_OLD_FILE`) ist eine Betreiberaufgabe außerhalb des Installers, siehe [deployment.md](deployment.md).

## FigCraft-Live (standardmäßig aktiv)

Seit 2026-10-31 richtet der Installer den FigCraft-Begleitdienst (AC-06) standardmäßig mit ein, damit Personen rund um die Uhr live an gemeinsamen FigCraft-Dateien arbeiten können, ohne dass jemand einen eigenen FigCraft-Server startet. `craftcloud.env` enthält dafür `CRAFTCLOUD_FIGCRAFT=enabled`, den Pfad `CRAFTCLOUD_FIGCRAFT_SECRET_FILE` und die öffentliche Adresse `CRAFTCLOUD_FIGCRAFT_PUBLIC_URL` (`wss://<Domain>[:<HTTPS-Port>]/figcraft/`). Den Verwaltungsschlüssel (64 zufällige Hex-Zeichen) erzeugt der Installer einmalig als `/etc/craftcloud/figcraft.key` mit Modus 0600; er wird nie ausgegeben, nie überschrieben und ist **nicht** im Backup. Bewahren Sie wie beim 2FA-Schlüssel eine getrennte, geschützte Kopie auf. Der Begleitdienst wird beim Installieren lokal gebaut (Netzwerkzugriff auf `codeload.github.com`, zusätzliche Bauzeit und Speicher) und läuft nur im internen Compose-Netz; Caddy reicht `/figcraft/` weiter. Alle `docker compose`-Aufrufe von `install.sh` und `craftcloud-ctl` verwenden gemeinsam `compose.yaml`, `compose.install.yaml`, `compose.figcraft.yaml` (und während des Setups `compose.setup.yaml`).

Ausschalten bei der Installation:

```sh
sudo sh craftcloud-0.1.0/deploy/install/install.sh --no-figcraft \
  --domain cloud.example.org --non-interactive ./craftcloud-0.1.0.tar.gz
```

`CRAFTCLOUD_FIGCRAFT=disabled` in der Umgebung des Installers wirkt genauso. Der gewählte Wert steht danach in `craftcloud.env`; ein erneuter Installerlauf mit widersprechendem Schalter wird abgelehnt. Später wechseln Sie mit `craftcloud-ctl`:

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl figcraft status
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl figcraft disable
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl figcraft enable
```

Beim **Ausschalten** werden Server und Proxy ohne FigCraft neu gestartet und die Begleitcontainer entfernt. Live-Dateien sind dann nicht verfügbar: `/api/v1/info` meldet `figcraft_live: false`, die Live-Routen antworten mit `FIGCRAFT_LIVE_UNAVAILABLE`, und die ausgegebenen persönlichen Links funktionieren nicht mehr. Beenden Sie aktive Live-Dateien vorher über „Livebetrieb beenden“, damit der letzte Stand als Version gesichert ist; Änderungen seit dem letzten Snapshot (alle 15 Minuten) bleiben nur im Volume `figcraft-data` erhalten. Dieses Volume und der Schlüssel `figcraft.key` bleiben bestehen, `craftcloud-ctl figcraft enable` startet den Begleitdienst wieder mit demselben Datenstand. Ausstehende Rechteentzüge werden erst nach dem erneuten Einschalten abgearbeitet; widerrufen Sie bei einem dauerhaften Abschalten Personen und Links vorher. Bestehende Installationen ohne die Einstellung bleiben bis zum nächsten `craftcloud-ctl upgrade` unverändert; das Upgrade trägt dann den Standard `enabled` ein, erzeugt den Schlüssel und baut den Begleitdienst (mit `figcraft disable` danach abschaltbar; wer ihn von Anfang an nicht möchte, führt vor dem Upgrade `figcraft disable` aus). Der Datenbankpool muss mit FigCraft mindestens 6 Verbindungen erlauben; der Standard 10 genügt.

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

Die Standardinstallation verwendet ACME und erneuert Zertifikate durch Caddy. Die Konfiguration wird bei der Installation nach `/etc/craftcloud/caddy` kopiert und ausschließlich lesend in den Proxy eingebunden; Upgrades bewahren sie. Bei `craftcloud-ctl upgrade` werden die mitgelieferten Caddy-Dateien aktualisiert: Eine unveränderte Datei wird durch die neue Version ersetzt. Haben Sie eine mitgelieferte Datei geändert, bleibt Ihre Fassung erhalten, die neue Version liegt als `<Name>.new` daneben und das Upgrade warnt; vergleichen Sie beide und übernehmen Sie nötige Sicherheitsänderungen. Zur Erkennung führt der Installer die Prüfsummen der ausgelieferten Dateien in `/etc/craftcloud/caddy/.shipped-sha256`; bei einer Installation aus einer Version ohne diese Datei wird beim ersten Upgrade jede abweichende Datei als geändert behandelt. Wurden Dateien aktualisiert, erzeugt das Upgrade den Proxy-Container neu; `rollback` gleicht die Dateien mit dem wieder aktiven Release ab und erzeugt den Proxy ebenfalls neu. Ihre eigenen Dateien (`fullchain.pem`, `private.key` und alles, was das Release nicht ausliefert) fasst das Upgrade nie an.

Für ein eigenes Zertifikat legen Sie die vollständige Zertifikatskette unter `/etc/craftcloud/caddy/fullchain.pem` und den privaten Schlüssel unter `/etc/craftcloud/caddy/private.key` ab. Das Verzeichnis gehört root mit Modus 0700, der private Schlüssel root mit Modus 0600. Ändern Sie in `craftcloud.env` ausschließlich `CRAFTCLOUD_TLS_MODE=custom` und starten Sie den Stack mit `sudo systemctl restart craftcloud.service` neu. Die mitgelieferte Datei `tls-custom.caddy` referenziert genau diese beiden Dateien. Erneuerung und geschützte Bereitstellung des eigenen Zertifikats verantworten Sie; erneuern Sie es rechtzeitig und starten Sie den Proxy nach dem Austausch neu. Domain, Zertifikat, öffentliche URL und sichere Cookies müssen zusammenpassen. Es gibt keinen Installer-Schalter für eigene Zertifikate. Dokumentation zur manuellen Compose-Konfiguration: [deployment.md](deployment.md).

## Backups und Wiederherstellung

```sh
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl backup
```

Ein Backup enthält Datenbank und Dateiinhalte als konsistenten Stand, aber nicht den Zwei-Faktor-Schlüssel; der Befehl nennt nach dem Lauf den Ort des Schlüssels (nie den Wert). Backup, `upgrade`, `rollback`, `setup-complete` und `uninstall` schließen einander über eine Sperre `/etc/craftcloud/.ctl.lock` aus (`flock` aus util-linux); ein zweiter gleichzeitiger Aufruf, auch des Timers, bricht mit einer klaren Meldung ab und wird beim nächsten Timerlauf wiederholt. Der systemd-Timer startet täglich um 03:15 in der Zeitzone des Hosts, mit zufälliger Verzögerung bis 30 Minuten. Standardmäßig bleiben die letzten sieben erfolgreichen Backups erhalten; `CRAFTCLOUD_BACKUP_KEEP` in der Konfiguration ändert die Anzahl. Kontrollieren Sie regelmäßig Timer, Exitcodes, freien Speicher und das Backup-Alter in der Betreiberansicht. Ein angelegter Timer allein beweist keine erfolgreiche Sicherung.

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

## E-Mail-Versand (AC-09)

Ohne `CRAFTCLOUD_MAIL_TRANSPORT` bleibt der Versand abgeschaltet. Einladungs- und Resetlinks werden dann wie bisher einmalig zum Kopieren angezeigt; das Restrisiko aus ADR-0017 bleibt in diesem Modus bestehen. Mit SMTP liefert `/api/v1/info` das Merkmal `feature_flags.email_delivery=true`. Einladende erhalten keine geheimen Links mehr. Bereits vorhandene unbestätigte Konten werden nicht automatisch als bestätigt markiert.

Die Installation verwendet optionale Compose-Dateien `compose.smtp.yaml` und bei Passwortanmeldung zusätzlich `compose.smtp-auth.yaml`. `install.sh` und `craftcloud-ctl` wählen diese anhand von `/etc/craftcloud/craftcloud.env` automatisch. Der SMTP-Testcontainer gehört ausschließlich zu Entwicklungstests und wird nie installiert. Fügen Sie beispielsweise diese nicht geheimen Einstellungen in die geschützte Konfigurationsdatei ein:

```dotenv
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_HOST=mail.example.org
CRAFTCLOUD_SMTP_PORT=587
CRAFTCLOUD_SMTP_TLS=starttls
CRAFTCLOUD_SMTP_FROM=CraftCloud <craftcloud@example.org>
```

Der Server benötigt weiterhin `CRAFTCLOUD_SECRET_KEY_FILE` und `CRAFTCLOUD_PUBLIC_URL`. `starttls` erzwingt das TLS-Upgrade; `tls` verwendet implizites TLS, typischerweise auf Port 465. Die Zertifikatsprüfung bleibt immer aktiv. Der konfigurierte Host muss zum Zertifikat passen. Die Produktionskonfiguration hat keinen Schalter für unsichere Zertifikate und keine Option für Clientzertifikate; ein Relay mit dieser Anforderung muss durch einen separat verwalteten lokalen SMTP-Relay vermittelt werden.

Für einen Versanddienst mit SMTP-Passwort ergänzen Sie:

```dotenv
CRAFTCLOUD_SMTP_USERNAME=craftcloud-smtp-user
CRAFTCLOUD_SMTP_PASSWORD_SOURCE=/etc/craftcloud/smtp-password
```

Legen Sie die Passwortdatei mit Eigentümer root und Modus 0600 an, ohne das Passwort als Befehlsargument oder Umgebungsvariable zu verwenden. `secrets-init` kopiert sie mit Modus 0400 in das ausschließlich vom Server gelesene Secret-Verzeichnis; der Server erhält `CRAFTCLOUD_SMTP_PASSWORD_FILE=/run/secrets/smtp_password`. Benutzername und Passwort müssen gemeinsam vorhanden sein. Bei direktem Betrieb ohne Compose setzen Sie `CRAFTCLOUD_SMTP_PASSWORD_FILE` auf eine nur vom Serverkonto lesbare Datei; `CRAFTCLOUD_SMTP_PASSWORD_SOURCE` ist nur eine Compose-Einstellung. Verwenden Sie die Host-, Port-, Absender- und Zugangsdaten Ihres SMTP-Dienstes; richten Sie SPF, DKIM und DMARC nach dessen Anleitung ein. CraftCloud signiert DKIM nicht selbst.

Für den eigenen Postfix oder ein anderes Relay können Benutzername und Passwort beide entfallen, wenn das Relay ausschließlich die freigegebene Ausgangs-IP akzeptiert. Kein offenes Relay betreiben. Auch dieser Weg benötigt ein gültiges Serverzertifikat und TLS.

Für Microsoft 365 verwenden Sie einen für Ihre Ausgangs-IP freigegebenen Eingangsconnector, den MX-Endpunkt Ihrer akzeptierten Domain als SMTP-Host, Port 25 und `starttls`, ohne Benutzername oder Passwort. Die Absenderdomain muss im Tenant akzeptiert sein. Port 25 muss ausgehend erreichbar sein. Ein zertifikatsbasierter Connector benötigt einen vorgeschalteten Relay mit passendem Clientzertifikat. Microsoft Graph und OAuth werden erst in AC-09b behandelt. Siehe [Microsofts Connector-Anleitung](https://learn.microsoft.com/en-us/exchange/mail-flow-best-practices/how-to-set-up-a-multifunction-device-or-application-to-send-email-using-microsoft-365-or-office-365).

Für Google Workspace konfigurieren Sie den SMTP-Relay-Dienst mit Ihrer freigegebenen Ausgangs-IP und TLS-Pflicht. Verwenden Sie `smtp-relay.gmail.com`, Port 587, `starttls` und einen zulässigen Absender Ihrer Domain; bei IP-Freigabe entfallen beide Zugangsdaten. Beachten Sie die [Google-Relay-Konfiguration](https://knowledge.workspace.google.com/admin/gmail/advanced/route-outgoing-smtp-relay-messages-through-google).

Das SMTP-Overlay verbindet nur den Server zusätzlich mit einem Netz mit Ausgangsroute. Beschränken Sie den ausgehenden Verkehr auf dem Host auf den Relay und dessen Port. Die internen Datenbanknetze bleiben intern. Bei manueller Compose-Nutzung hängen Sie `-f compose.smtp.yaml` und gegebenenfalls `-f compose.smtp-auth.yaml` an die bisherigen Compose-Dateien an. Nach Änderungen verwenden Sie in einem Wartungsfenster `craftcloud-ctl stop` und danach `craftcloud-ctl start`; so werden auch ausgetauschte Passwortdateien erneut verteilt. `craftcloud-ctl` lässt die SMTP-Overlays mit einer Warnung weg, wenn ein Release (etwa bei einem Rückwechsel auf einen Stand vor AC-09) sie nicht enthält; dann läuft der Server ohne Mailversand. Prüfen Sie im Bereich **System** den Versandstatus und senden Sie eine Testmail an Ihre eigene Adresse. Der Status zeigt nur Konfiguration, letzten erfolgreichen Versand, Fehleranzahl und die Zahl unterdrückter Sicherheitshinweise, niemals Mailinhalte.

Der Worker versucht vorübergehende Fehler höchstens achtmal innerhalb von 24 Stunden mit exponentiellen Abständen. Eine SMTP-Annahme bestätigt die Übergabe an den Relay, nicht die Zustellung im Postfach. Prüfen Sie bei ausbleibender Mail auch Spamordner und Relay-Protokolle. Permanente Fehler oder ausgeschöpfte Versuche werden auditiert. Resends sind auf einmal pro Minute begrenzt; zusätzlich gelten fünf Mails pro Empfänger und Vorlage pro Stunde sowie dreißig pro auslösender Person. Bei einem Absturz zwischen SMTP-Annahme und Datenbankbestätigung kann eine Mail doppelt eintreffen.

Ein Sicherheitshinweis blockiert die gemeldete Aktion nie. Ist das Limit von fünf Hinweisen pro Empfänger und Stunde ausgeschöpft, werden Anmeldung, Passwortänderung, 2FA-Änderung oder Adressänderung trotzdem durchgeführt; der Hinweis wird nicht versendet, sondern in derselben Transaktion als unterdrückt gezählt. Der Bereich **System** zeigt die Zahl der unterdrückten Hinweise der letzten sieben Tage. Eine auffällige Zahl deutet auf viele Anmeldungen oder Änderungen in kurzer Zeit hin und sollte geprüft werden. Mails mit Link (Einladung, Bestätigung, Adressänderung, Reset) behalten ihre Limits und werden bei Überschreitung abgelehnt. Bestätigungsmails haben, auch bei der Massenversendung durch den Operator, ein eigenes Kontingent von dreißig pro Stunde, getrennt von Hinweisen und Testmails des Operators.

Abgeschlossene Queue-Einträge (gesendet, fehlgeschlagen, abgelaufen, unterdrückt) löscht die Wartung nach sieben Tagen; die jüngste erfolgreiche Zustellung bleibt für die Statusanzeige erhalten. Ebenso entfernt die Wartung abgelaufene Bestätigungstokens und abgelaufene Adressänderungen. Die Fehleranzahl im Status umfasst daher die letzten sieben Tage.

Bestätigungslinks verwenden URL-Fragmente und kurzlebige, einmalige Tokens. Die Outbox speichert Tokens nur verschlüsselt mit dem AC-02-Serverschlüssel und löscht sie nach erfolgreichem Versand oder Ablauf. Queue und offene Adressänderungen gehören nicht ins Backup; nach Restore werden alte Mails nicht nachgesendet. Schützen und rotieren Sie den Serverschlüssel wie für 2FA beschrieben. Alte Einladungen müssen nach Restore weiterhin geprüft und gegebenenfalls widerrufen werden. Für bestehende unbestätigte Konten kann der Operator im Bereich **System** Bestätigungsmails anstoßen; Versandlimits gelten auch dabei.

Bei vielen unbestätigten Konten arbeitet die Oberfläche in begrenzten Gruppen. Verwenden Sie nach einer angezeigten Wartezeit „Weitere Bestätigungslinks senden…“, um am zurückgegebenen Fortsetzungspunkt weiterzuarbeiten. Bereits früher kopierte Einladungslinks gelten nach Aktivierung von SMTP nicht als Adressnachweis; senden Sie solche Einladungen erneut. Der neue Link ersetzt den alten.

Die nur auf dem Serverhost verfügbare Passwort-Wiederherstellung bleibt ein Notfallweg: Sie gibt weiterhin den ausdrücklich als geheim gekennzeichneten Resetlink aus. Verwenden Sie diesen Befehl interaktiv und schützen Sie seine Ausgabe; die normalen Webpanel-Aktionen versenden den Link bei aktiviertem SMTP ausschließlich per E-Mail.

## Releases lokal vorbereiten

Auf einem Entwicklerrechner mit Git, der Rust-Toolchain des Projekts und Python ab 3.11 erzeugt `cargo xtask release 0.1.0` aus einem sauberen Git-Stand das Archiv, die SBOM und Prüfsummen unter `dist/`. Ungültige SemVer-Versionen sowie geänderte oder unversionierte Dateien werden abgelehnt. Die Quelldateien stammen aus `git archive`; zusätzlich enthält das Archiv `CRAFTCLOUD-RELEASE.json` mit Version und Commit. `craftcloud-server --version` zeigt die Build-Identität. Ein Tag `v0.1.0`, eine Signatur oder Veröffentlichung entstehen durch diesen Befehl nicht.
