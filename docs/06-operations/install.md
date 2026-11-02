# CraftCloud auf einem eigenen Linux-Server installieren

Diese Anleitung richtet sich an Administratoren, die CraftCloud erstmals betreiben. CraftCloud speichert Konten, Organisationen und Dateien; PostgreSQL und der HTTPS-Proxy Caddy laufen im mitgelieferten Docker-Stack. Auf dem Host müssen Sie weder PostgreSQL noch Rust oder Node.js installieren. CraftCloud selbst läuft in **einem** Image (`craftcloud:<version>`), das Server, Admin-Oberfläche und den FigCraft-Server enthält; PostgreSQL bleibt ein eigener Container oder ein vorhandener Server. Es werden keine fertigen Images aus einer Registry geladen: Entweder baut der Installationsbefehl das Image aus dem Quellarchiv, oder Sie laden das mit dem Release gelieferte Image-Archiv (siehe [Vorgebautes Image](#vorgebautes-image-statt-lokalem-build)).

Drei Wege stehen zur Wahl, die Sie unten finden: [Weg A](#weg-a-mitgelieferter-caddy-standard) mit dem mitgelieferten Caddy (Standard), [Weg B](#weg-b-vorhandener-nginx-oder-apache-mit-certbot) mit einem vorhandenen nginx oder Apache und [Weg C](#weg-c-ein-einzelner-container-mit-docker-run) mit einem einzelnen `docker run`. Die [Datenbankwahl](#datenbank-wählen) gilt für alle drei.

Der Stand ist ein lokaler Prototyp. Ein externer Betreiber muss die unten beschriebene Abnahme auf einer echten VM noch durchführen. Insbesondere sind eine Docker-Desktop-Probe und ein funktionierender Healthcheck keine Freigabe für einen Mehrmandanten-Produktivbetrieb. SMTP und E-Mail-Verifikation müssen für diesen Betrieb ausdrücklich eingerichtet und geprüft werden.

## Host, Software und Netzwerk vorbereiten

Vorgesehene Plattformen sind Debian 13 (trixie) und Ubuntu 24.04 LTS auf x86_64 oder arm64. Benötigt werden Docker Engine mindestens 25, das Docker-Compose-Plugin mindestens 2.24.4 sowie die üblichen Linux-Werkzeuge `sh`, `tar`, `sha256sum`, `curl`, `ss`, `getent`, `df` und `install`. Verwenden Sie gepflegte Pakete aus einer vertrauenswürdigen Quelle und kontrollieren Sie `docker version` und `docker compose version`. Der Benutzer muss den Docker-Daemon erreichen können. Docker-Zugriff verleiht praktisch Root-Rechte; geben Sie ihn nur Administratoren.

Mindestens 2 GiB RAM und 10 GiB freier Speicher sind erforderlich. Für den lokalen Build und kleine Installationen empfehlen wir 4 CPU-Kerne, 8 GiB RAM und mindestens 30 GiB freien SSD-Speicher, zusätzlich zum Platz für Dateien, Versionen, Datenbank und sieben vollständige Backups. Die Mindestwerte garantieren keinen erfolgreichen Build bei zusätzlicher Last. Das Datenverzeichnis muss auf einem lokalen Linux-Dateisystem mit Hard-Links und Unix-Dateirechten liegen, beispielsweise ext4 oder XFS.

Richten Sie einen DNS-A-Eintrag und, nur bei funktionierendem IPv6, einen AAAA-Eintrag für beispielsweise `cloud.example.org` auf den Server ein. Öffnen Sie TCP 80 und 443; UDP 443 ist für HTTP/3 optional. PostgreSQL und der interne Server-Port werden nicht veröffentlicht (bei Weg B nur auf `127.0.0.1`). Der Build braucht ausgehenden Zugriff auf die in Dockerfile und Lockfiles genannten Paketquellen; Caddy braucht Zugriff auf die Zertifizierungsstelle. Schützen Sie SSH, halten Sie Host und Docker aktuell und synchronisieren Sie die Uhr.

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

## Weg A: mitgelieferter Caddy (Standard)

Sind die Ports 80 und 443 frei und läuft auf dem Server kein nginx oder Apache, richtet der Installer den mitgelieferten Caddy mit automatischem Zertifikat (Let's Encrypt) ein. Das ist der Befehl aus dem Abschnitt „Release beziehen und prüfen“. `--proxy caddy` wählt ihn ausdrücklich.

## Weg B: vorhandener nginx oder Apache (mit certbot)

Läuft auf dem Server schon ein Webserver auf Port 80 und 443, darf CraftCloud nicht mit einem eigenen Caddy kollidieren. Der Installer erkennt einen laufenden nginx oder Apache (`--proxy auto`) und schlägt ihn vor; Sie können ihn auch ausdrücklich wählen:

```sh
sudo sh craftcloud-0.1.0/deploy/install/install.sh --proxy nginx \
  --domain cloud.example.org --acme-email admin@example.org \
  --non-interactive ./craftcloud-0.1.0.tar.gz
```

Mit `--proxy nginx`, `--proxy apache` oder `--proxy none` (eigener Proxy) startet **kein** Caddy. Der CraftCloud-Server veröffentlicht nur `127.0.0.1:8080` (anderer Port mit `--local-port`) und der FigCraft-Server nur `127.0.0.1:47900` (`--local-figcraft-port`). Die Ports 80 und 443 prüft der Installer dann nicht; sie gehören Ihrem Webserver.

Der Installer **schreibt nur Vorlagen** nach `/etc/craftcloud/proxy/` (`craftcloud.nginx.conf` und `craftcloud.apache.conf`, mit Ihrer Domain, den Ports und dem Größenlimit) und nennt die nächsten Schritte. Er ändert keine Webserver-Konfiguration, solange Sie `--proxy-install` nicht angeben. Die Schritte von Hand für nginx:

```sh
sudo apt install certbot python3-certbot-nginx   # nur wenn Sie certbot noch nicht haben
sudo certbot certonly --nginx -d cloud.example.org
sudo cp /etc/craftcloud/proxy/craftcloud.nginx.conf /etc/nginx/conf.d/craftcloud.conf
sudo nginx -t && sudo systemctl reload nginx
```

Für Apache: `sudo a2enmod proxy proxy_http proxy_wstunnel headers rewrite ssl`, `sudo certbot certonly --apache -d cloud.example.org`, die Vorlage nach `/etc/apache2/sites-available/craftcloud.conf` kopieren, `sudo a2ensite craftcloud`, dann `sudo apachectl configtest && sudo systemctl reload apache2`.

Mit `--proxy-install` erledigt der Installer das selbst, vorsichtig: Er sichert die eine Datei, die er ändert (nach `/etc/craftcloud/proxy/backup/`), prüft die Konfiguration mit `nginx -t` bzw. `apachectl configtest`, lädt nur bei Erfolg neu und stellt bei einem fehlgeschlagenen Test den vorherigen Stand wieder her. Bei Apache prüft er die Module und nennt fehlende; aktivieren muss sie der Administrator. Das Zertifikat kommt entweder aus eigenen Dateien (`--proxy-cert DATEI --proxy-key DATEI`) oder von certbot (`--proxy-install --certbot`). certbot wird **nie** automatisch installiert; fehlt es, nennt der Installer die Pakete für Debian und Ubuntu. Mit `--certbot` richtet er zuerst eine minimale Port-80-Konfiguration ein, ruft `certbot certonly --nginx` (bzw. `--apache`) für die Domain auf und aktiviert danach die vollständige Konfiguration; nicht interaktiv braucht das `--acme-email`, und certbot erhält `--agree-tos`. Die Erneuerung bleibt beim certbot-Timer Ihres Systems (`systemctl list-timers | grep certbot`). Mit eigenem Zertifikat kümmern Sie sich selbst um die Erneuerung.

Was die Vorlagen sicherstellen: HTTPS mit Weiterleitung von HTTP, HSTS, WebSocket-Weiterleitung für den FigCraft-Livepfad `/figcraft/`, Upload ohne Zwischenspeichern (große Dateien bis zum Dateigrößenlimit, Standard 5 GiB; Apache kann über 2 GiB nicht begrenzen, dort erzwingt der Server das Limit), lange Zeitlimits für Uploads und Livesitzungen, `/metrics` gesperrt. Beide Vorlagen schreiben keine URLs in Protokolle: nginx hat in beiden Servern kein Zugriffsprotokoll und ein eigenes Fehlerprotokoll `/var/log/nginx/craftcloud-error.log` auf Stufe `crit` (das Verzeichnis muss existieren, wie bei Debian, Ubuntu und den nginx.org-Paketen); bei Apache hat jeder virtuelle Host ein eigenes `CustomLog` nach `/dev/null` (so erbt er die Zugriffsprotokolle des Servers nicht) und ein eigenes Fehlerprotokoll mit `LogLevel crit` (Proxy-Fehler wie AH00898 nennen sonst den Pfad; der erste Integrationslauf fand ein Token im Fehlerprotokoll). `X-Content-Type-Options: nosniff` und `Referrer-Policy: same-origin` ergänzen die Vorlagen nur, wenn CraftCloud sie nicht selbst setzt (wie der mitgelieferte Caddy). Bei Apache aktiviert der Installer die Seite (`a2ensite`), prüft die Konfiguration und lädt neu in einem Schritt; scheitert etwas, nimmt er alles zurück (`a2dissite`, gesicherte Datei) und prüft, dass Apache mit dem vorherigen Stand weiterläuft. Mit `--certbot` ist die HTTP-Seite aktiv und geladen, bevor `certbot certonly` läuft. Der Proxy **überschreibt** `X-Forwarded-For` mit der Adresse des Clients und setzt `X-Forwarded-Proto: https`; `X-Forwarded-Host` und `Forwarded` leitet er nie weiter. Das ist sicherheitsrelevant: CraftCloud vertraut `X-Forwarded-For` nur von der Docker-Gateway-Adresse des Proxy-Netzes (`CRAFTCLOUD_TRUSTED_PROXIES`, Standard `172.29.206.1/32`, das Teilnetz lässt sich mit `CRAFTCLOUD_PROXY_SUBNET` und `CRAFTCLOUD_PROXY_GATEWAY` in `craftcloud.env` ändern, falls es mit einem vorhandenen Netz kollidiert), und Rate-Limit und Audit-Log sehen sonst nur diese Adresse. Steht vor Ihrem nginx noch ein weiterer Proxy oder ein CDN, müssen Sie dessen Adresse im nginx mit `set_real_ip_from` und `real_ip_header` bekannt machen, sonst sieht CraftCloud die Adresse des CDN. Das Docker-Netz `proxy` (nur Server und FigCraft-Server) ist nicht `internal`, weil Docker nur dort Ports veröffentlicht; die beiden Container haben dadurch ausgehenden Netzzugriff. Begrenzen Sie ihn bei Bedarf mit der Host-Firewall. Beachten Sie außerdem: Der Server vertraut `X-Forwarded-For` von jeder Verbindung, die über den veröffentlichten Loopback-Port ankommt. Wer auf dem Server selbst Konten hat, kann also die Client-Adresse fälschen, indem er `127.0.0.1:8080` direkt anspricht. Betreiben Sie den Server deshalb nicht auf einem Host mit nicht vertrauenswürdigen lokalen Benutzern, oder schränken Sie den Zugriff auf die Loopback-Ports mit der Host-Firewall ein.

`--proxy none` schreibt die Vorlagen zum Anpassen und gibt die Eckdaten aus (Loopback-Ports, Pfad `/figcraft/`, die Header-Regeln), für Apache-fremde und nginx-fremde Proxys wie HAProxy oder Traefik.

## Weg C: ein einzelner Container mit `docker run`

Für Tests und kleine Installationen startet `all-in-one` den CraftCloud-Server und den FigCraft-Server in **einem** Container (ein kleiner Supervisor startet ein beendetes Programm mit wachsender Wartezeit neu und gibt das Stoppsignal an beide weiter). Das ersetzt `install.sh` nicht: Es gibt keine Datensicherung per Zeitplan, kein Upgrade-Werkzeug und keinen Proxy. Compose zusammen mit `install.sh` bleibt der empfohlene Weg.

Voraussetzungen: ein Image (`docker load` des Image-Archivs oder `docker build`), ein erreichbarer PostgreSQL 16 bis 18 mit den beiden Rollen `craftcloud_owner` und `craftcloud_runtime` (die SQL-Befehle liefert der Installer, siehe [Datenbank wählen](#datenbank-wählen), oder `deploy/compose/db-init.sh` als Vorlage) und Geheimnisdateien, die dem Benutzer 65532 gehören und Modus 0400 haben:

```sh
sudo install -d -m 0500 -o 65532 -g 65532 /etc/craftcloud-run
# Je Datei: Inhalt über die Standardeingabe, nie als Argument (hier eine Datei mit 64 Hex-Zeichen als Beispiel)
od -An -N32 -tx1 /dev/urandom | tr -d ' \n' | sudo install -m 0400 -o 65532 -g 65532 /dev/stdin /etc/craftcloud-run/figcraft_key
head -c 32 /dev/urandom | base64 | tr -d '\n' | sudo install -m 0400 -o 65532 -g 65532 /dev/stdin /etc/craftcloud-run/two_factor_key
# db_owner_url und db_runtime_url enthalten die Datenbank-URLs mit den Rollen (postgres://craftcloud_owner:...@host:5432/craftcloud?sslmode=verify-full), ebenso mit Modus 0400.

# Schema anlegen (einmalig und nach jedem Upgrade):
docker run --rm -v /etc/craftcloud-run:/run/secrets:ro \
  -e CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE=/run/secrets/db_owner_url \
  -e CRAFTCLOUD_DATABASE_URL_FILE=/run/secrets/db_runtime_url \
  -e CRAFTCLOUD_DATABASE_RUNTIME_ROLE=craftcloud_runtime \
  craftcloud:0.1.0 migrate

# Server und FigCraft-Server starten:
docker run -d --name craftcloud --restart unless-stopped --stop-timeout 35 \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:47900:47900 \
  -e CRAFTCLOUD_PUBLIC_URL=https://cloud.example.org \
  -e CRAFTCLOUD_TRUSTED_PROXIES=172.17.0.1/32 \
  -e CRAFTCLOUD_DATABASE_URL_FILE=/run/secrets/db_runtime_url \
  -e CRAFTCLOUD_SECRET_KEY_FILE=/run/secrets/two_factor_key \
  -e CRAFTCLOUD_FIGCRAFT_KEY_FILE=/run/secrets/figcraft_key \
  -v /etc/craftcloud-run:/run/secrets:ro \
  -v craftcloud-store:/var/lib/craftcloud/store \
  -v craftcloud-figcraft:/var/lib/craftcloud/figcraft \
  craftcloud:0.1.0 all-in-one
```

`172.17.0.1` ist das Gateway des Standard-Docker-Netzes (`docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'`); von dort kommt die Verbindung Ihres Proxys auf dem Host an. Ein vorhandener nginx oder Apache verwendet dieselbe Vorlage wie bei Weg B (mit `--local-port 8080`). Die erste Einrichtung (Bootstrap-Geheimnis `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE`) beschreibt [deployment.md](deployment.md). `docker stop craftcloud` sendet SIGTERM, beide Programme beenden sich innerhalb der 35 Sekunden. Die öffentliche FigCraft-Adresse leitet der Container aus `CRAFTCLOUD_PUBLIC_URL` ab (`wss://<Domain>/figcraft/`), wenn Sie `CRAFTCLOUD_FIGCRAFT_PUBLIC_URL` nicht setzen.

## Datenbank wählen

CraftCloud unterstützt ausschließlich **PostgreSQL 16, 17 und 18** (Debian 13 liefert 17, Ubuntu 24.04 liefert 16). Ältere und andere Versionen lehnt der Server beim Start mit der Meldung `unsupported PostgreSQL version N` ab. MySQL, MariaDB, SQLite und SQL Server werden nicht unterstützt. PostgreSQL ist nie Teil des CraftCloud-Images, damit Upgrades, Backups und Ausfälle der Datenbank getrennt behandelbar bleiben.

`--db auto` (Standard) prüft, ob auf dem Server schon PostgreSQL läuft (Port 5432, Prozess, systemd-Unit, `pg_lsclusters`, `psql --version`), und meldet die Version. Interaktiv fragt der Installer, ob Sie sie verwenden möchten. Nicht interaktiv verwendet er eine gefundene Datenbank **nie** ohne ausdrückliches `--db external` und installiert stattdessen den mitgelieferten PostgreSQL-Container (`--db bundled`). Findet er MySQL oder MariaDB, erklärt er, dass sie nicht unterstützt werden, und bietet den mitgelieferten Container an.

Für eine vorhandene Datenbank (`--db external`):

1. Legen Sie eine Datei mit einer Zeile an, **ohne** Benutzer und Passwort, zum Beispiel `/root/craftcloud-db.url` mit dem Inhalt `postgres://db.example.org:5432/craftcloud?sslmode=verify-full`. Erlaubt sind nur `sslmode` und `sslrootcert` (absoluter Pfad zum Zertifikat Ihrer privaten Zertifizierungsstelle). Eine Datenbank auf einem **anderen Host** verlangt `sslmode=verify-full`. Für eine Datenbank **auf diesem Server** (`localhost`, `127.0.0.1`) genügt ohne `verify-full` nur ausdrücklich `--db-allow-insecure-local`; die Container erreichen sie als `host.docker.internal`, PostgreSQL muss dafür auf der Docker-Gateway-Adresse lauschen und `pg_hba.conf` das Docker-Teilnetz erlauben. Unix-Sockets gehen mit Containern nicht.
2. Starten Sie den Installer mit `--db external --database-url-file /root/craftcloud-db.url`. Er erzeugt die Passwörter der beiden Rollen `craftcloud_owner` (besitzt die Tabellen, nur für die Migration) und `craftcloud_runtime` (nur Datenrechte, für den Server), aber **kein** Administratorpasswort, und schreibt die SQL-Befehle in `/etc/craftcloud/external-db-roles.sql` (Modus 0600, enthält die erzeugten Rollenpasswörter, wird nie ausgegeben). Danach hält er an.
3. Führen Sie die Datei als Datenbankadministrator aus (`psql -v ON_ERROR_STOP=1 -f /etc/craftcloud/external-db-roles.sql postgres`) und starten Sie denselben Installerbefehl noch einmal mit `--db-roles-ready`. Die Datei wird danach gelöscht. Wer dem Installer die Administratorverbindung geben will, übergibt `--db-admin-url-file DATEI` (eine Zeile `postgres://admin:PASSWORT@host/postgres?sslmode=verify-full`, Modus 0600): Dann führt der Installer die Befehle selbst in einem Container des PostgreSQL-Images aus, prüft dabei auch die Version, und gibt weder die Datei noch deren Inhalt aus. Die Datei folgt denselben Regeln wie die Datenbank-URL: gleicher Host und Port, bei entfernten Servern `sslmode=verify-full`, bei einem Server auf diesem Host `verify-full` oder `--db-allow-insecure-local`. Das Passwort darf `@ : / ? # %` und `\\` nicht enthalten, weil es nicht prozentdekodiert wird. Das Passwort steht in keinem Prozessargument, sondern nur in einer Datei auf einem tmpfs des Hilfs-Containers. Der Datenbankname besteht aus Kleinbuchstaben, Ziffern und Unterstrich (höchstens 63 Zeichen, nicht mit einer Ziffer beginnend, keine reservierten SQL-Wörter, nicht `postgres` oder `template*`). `sslmode=verify-full` mit `localhost` oder `127.0.0.1` ist nicht möglich (der Container erreicht den Host als `host.docker.internal`): Stellen Sie das Zertifikat auf `host.docker.internal` aus und tragen Sie diesen Namen in die URL ein. Die SQL-Datei schaltet in einer Superuser-Sitzung das Protokollieren von Anweisungen für die Sitzung ab, bevor die Rollenpasswörter gesetzt werden; ohne Superuser zeigt der Installer eine Warnung, und die Passwörter könnten im Serverprotokoll stehen (dann danach ändern). Bei einem Server auf diesem Host muss `pg_hba.conf` das Docker-Teilnetz erlauben: `172.29.206.0/24` bei eigenem Proxy (Netz `proxy`), `172.29.207.0/24` beim mitgelieferten Caddy (Netz `dbegress`).

Backups, `craftcloud-ctl backup` und die Wiederherstellung funktionieren unverändert; die Datenbank selbst sichern Sie mit den Werkzeugen Ihres PostgreSQL-Servers zusätzlich.

## Vorgebautes Image statt lokalem Build

`cargo xtask release <version>` erzeugt neben dem Quellarchiv je Architektur ein Image-Archiv `craftcloud-<version>-image-amd64.tar.zst` (und `-arm64`, soweit der Builder es kann) und trägt es in `SHA256SUMS` ein. Legen Sie es neben das Quellarchiv und `SHA256SUMS`:

```sh
sudo sh craftcloud-0.1.0/deploy/install/install.sh --non-interactive --domain cloud.example.org \
  --image-archive ./craftcloud-0.1.0-image-amd64.tar.zst ./craftcloud-0.1.0.tar.gz
sudo sh /opt/craftcloud/current/deploy/install/craftcloud-ctl upgrade ./craftcloud-0.1.1.tar.gz \
  --image-archive ./craftcloud-0.1.1-image-amd64.tar.zst
```

Der Installer prüft die Prüfsumme, den Dateinamen (Version und Architektur dieses Servers) und die Architektur des geladenen Images und baut dann nichts mehr: Der lange Build auf dem Server entfällt, ohne dass eine Registry nötig ist. Docker muss `.zst` lesen können; sonst installieren Sie das Paket `zstd` (Debian und Ubuntu). Ohne `--image-archive` baut der Installer wie bisher aus dem Quellarchiv.

## FigCraft-Live (standardmäßig aktiv)

Seit 2026-10-31 richtet der Installer den FigCraft-Begleitdienst (AC-06) standardmäßig mit ein, damit Personen rund um die Uhr live an gemeinsamen FigCraft-Dateien arbeiten können, ohne dass jemand einen eigenen FigCraft-Server startet. `craftcloud.env` enthält dafür `CRAFTCLOUD_FIGCRAFT=enabled`, den Pfad `CRAFTCLOUD_FIGCRAFT_SECRET_FILE` und die öffentliche Adresse `CRAFTCLOUD_FIGCRAFT_PUBLIC_URL` (`wss://<Domain>[:<HTTPS-Port>]/figcraft/`). Den Verwaltungsschlüssel (64 zufällige Hex-Zeichen) erzeugt der Installer einmalig als `/etc/craftcloud/figcraft.key` mit Modus 0600; er wird nie ausgegeben, nie überschrieben und ist **nicht** im Backup. Bewahren Sie wie beim 2FA-Schlüssel eine getrennte, geschützte Kopie auf. Der FigCraft-Server ist Teil des CraftCloud-Images (Rolle `figcraft`, ein eigener Container aus demselben Image); beim lokalen Build wird er zusätzlich aus dem gepinnten Commit gebaut (Netzwerkzugriff auf `codeload.github.com`, zusätzliche Bauzeit und Speicher), mit dem Image-Archiv entfällt das. Er läuft nur im internen Compose-Netz; Caddy reicht `/figcraft/` weiter. Bei Weg B veröffentlicht er `127.0.0.1:47900` für Ihren Webserver. Alle `docker compose`-Aufrufe von `install.sh` und `craftcloud-ctl` verwenden gemeinsam `compose.yaml`, `compose.install.yaml`, `compose.figcraft.yaml` (und während des Setups `compose.setup.yaml`).

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

## Systemprüfung des Einrichtungsassistenten

Die folgenden Abschnitte helfen, Hinweise der Systemprüfung einzuordnen. Die Prüfung selbst ändert weder Serverkonfiguration noch Infrastruktur. Ein Warnhinweis verhindert nicht, dass der Betreiber die Einrichtung fortsetzt.

### Public URL

Vergleichen Sie die öffentliche Adresse einschließlich Schema und Port mit `CRAFTCLOUD_PUBLIC_URL`. Browser und Server müssen dieselbe Origin verwenden; Pfade gehören nicht in diesen Wert.

### Reverse proxy

Nutzen Sie für den öffentlichen Zugriff HTTPS. Wenn ein vorgeschalteter Proxy TLS beendet, darf der Server weitergeleitete Angaben nur von den ausdrücklich konfigurierten vertrauenswürdigen Proxys übernehmen. Beachten Sie die Abschnitte zu den Proxy-Varianten oben.

### PostgreSQL

Prüfen Sie Version, Erreichbarkeit und Laufzeitrolle der Datenbank anhand des Abschnitts „Datenbank wählen“. Eine Warnung über eine nicht unterstützte Version erfordert eine passende Datenbankinstallation und ein geprüftes Backup vor einem Upgrade.

### Storage

Kontrollieren Sie den freien Platz auf dem Datenträger des konfigurierten Speicherverzeichnisses. Datenbank und Dateispeicher benötigen eigene Kapazitätsplanung und Sicherungen.

### FigCraft

Prüfen Sie bei aktiviertem FigCraft-Live Dienstzustand, interne Erreichbarkeit und öffentliche Adresse wie im Abschnitt „FigCraft-Live“ beschrieben. Eine konfigurierte Adresse allein beweist keine Erreichbarkeit.

### Time synchronization

Synchronisieren Sie Server und Datenbankhost mit einem vertrauenswürdigen Zeitdienst. Größere Abweichungen beeinträchtigen unter anderem Sitzungslaufzeiten, Zwei-Faktor-Anmeldung und Wartung.

### Maintenance

Prüfen Sie, ob Wartung aktiviert ist und regelmäßig erfolgreich läuft. Ohne Wartung werden unter anderem abgelaufene Daten und freizugebender Speicher nicht automatisch bereinigt.

### Archive

Der Assistent zeigt den Archivstatus und verweist auf die [Archiv-Betriebsanleitung](archive-storage.md). Er konfiguriert den Archivspeicher nicht; die bestehende Startsperre bleibt bis zur Freigabe von AC-12 bestehen.

## Erstes Konto und Betrieb

Nach dem Image-Build, der Datenbankinitialisierung und den Migrationen wartet das Skript auf `/health/ready`. Es nennt anschließend die Setup-URL und den Pfad zur Bootstrap-Geheimnisdatei, niemals das Geheimnis. Öffnen Sie die ausgegebene URL im Browser, lesen Sie den Token nur in einer privaten lokalen Administratorsitzung und tragen Sie ihn in das Setup ein. Legen Sie Ihr Betreiberkonto, ein eigenes Passwort und die erste Organisation an. Speichern Sie den Token nicht in Tickets, Logs, Shell-Argumenten oder Bildschirmaufzeichnungen.

Nach der Kontoerstellung führt der Setup-Assistent durch Systemprüfung, Zwei-Faktor-Anmeldung, E-Mail, Organisation, Arbeitsbereich, Einladungen und optionale Dienste. Der gespeicherte Fortschritt lässt sich später fortsetzen. Prüfen Sie Warnungen anhand der Hinweise zur Systemprüfung und senden Sie nach der E-Mail-Konfiguration eine Testmail. Die Oberfläche meldet zunächst das Einreihen zur Zustellung; prüfen Sie den tatsächlichen Eingang und bei Bedarf den Versandstatus. Mailänderungen benötigen eine frische Anmeldung und gegebenenfalls den eingerichteten zweiten Faktor. In der Umgebung festgelegte Werte haben Vorrang und können in der Oberfläche nicht überschrieben werden. Die Archivkonfiguration bleibt gesperrt.

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

Das Datenbankbackup enthält auch die verschlüsselten Geheimnisse der Instanzeinstellungen. Nach einer Wiederherstellung lassen sie sich nur mit demselben Serverschlüssel entschlüsseln; bewahren Sie diesen Schlüssel getrennt und geschützt auf und prüfen Sie ihn bei Restore-Proben.

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

**Windows Server** ist kein Ziel dieser Installation. Betreiben Sie CraftCloud auf Windows-Infrastruktur in einer Linux-VM (Debian 13 oder Ubuntu 24.04) auf Hyper-V und folgen Sie dieser Anleitung in der VM. Gründe: CraftCloud und seine Images sind Linux-Container; Windows-Container können sie nicht ausführen. Docker Desktop mit WSL 2 ist eine Entwicklungsumgebung und auf Servern nicht empfohlen (kein Dienstmanager für den Start beim Hochfahren ohne angemeldete Person, andere Dateirechte, Netzwerk über eine zusätzliche Schicht). Eine spätere native Unterstützung bräuchte eine gesonderte Entscheidung samt vollständigem Installations- und Restore-Test.

Auf einer echten Debian-VM bleiben für OPS-02 folgende Prüfungen offen, weil sie in den automatischen Tests nicht möglich sind: ein echtes Zertifikat von Let's Encrypt über `certbot` mit Erneuerungsprobe (`certbot renew --dry-run`), die Host-Firewall (nur 22, 80 und 443; die Loopback-Ports 8080 und 47900 dürfen nicht von außen erreichbar sein), eine Datenbank auf einem anderen Host mit `sslmode=verify-full` und privater Zertifizierungsstelle, eine PostgreSQL 16 oder 17 des Pakets (`apt install postgresql`) mit `listen_addresses` auf der Docker-Gateway-Adresse sowie ein `arm64`-Server mit dem arm64-Image-Archiv.

## E-Mail-Versand (AC-09)

### AC-09b: Microsoft Graph und SMTP mit OAuth

Die drei Versandarten verwenden dieselbe Warteschlange, Vorlagen und Limits. Graph benötigt ausgehendes HTTPS; SMTP mit OAuth zusätzlich den SMTP-Port. Zugriffs-Tokens bleiben nur im Arbeitsspeicher. Die produktiven Endpunkte sind fest vorgegeben; Weiterleitungen und unsichere Zertifikate sind nicht zugelassen. Nationale Microsoft-Clouds werden nicht unterstützt.

#### Entra-App und Zertifikat vorbereiten

1. Melden Sie sich als Administrator im Microsoft Entra Admin Center an. Öffnen Sie **App-Registrierungen > Neue Registrierung**, Name „CraftCloud Mail“, nur Konten dieses Verzeichnisses; eine Redirect-URI ist nicht erforderlich. Notieren Sie die **Verzeichnis-/Mandanten-ID** und **Anwendungs-/Client-ID**.
2. Unter **Unternehmensanwendungen > CraftCloud Mail > Übersicht** notieren Sie zusätzlich die **Objekt-ID des Dienstprinzipals**. Die Objekt-ID der App-Registrierung ist hierfür falsch.
3. Erzeugen Sie auf dem geschützten Server ein RSA-Zertifikat und einen unverschlüsselten PEM-Schlüssel. Der Schlüssel ist ausschließlich durch Dateirechte geschützt; CraftCloud unterstützt keine interaktive Passphrase:

```sh
sudo install -d -m 0700 /etc/craftcloud/mail
sudo sh -c 'umask 077; openssl req -x509 -newkey rsa:3072 -noenc -sha256 -days 365 -subj /CN=CraftCloud-Mail -keyout /etc/craftcloud/mail/client.key -out /etc/craftcloud/mail/client.crt'
```

4. Laden Sie ausschließlich `client.crt` unter **App-Registrierungen > CraftCloud Mail > Zertifikate und Geheimnisse > Zertifikate** hoch. Kontrollieren Sie Ablaufdatum und Fingerabdruck. Der private Schlüssel bleibt auf dem Server. CraftCloud prüft Laufzeit und Übereinstimmung des Schlüssels und verwendet eine signierte Client-Assertion. Alternativ erstellen Sie dort ein Client-Geheimnis und speichern dessen **Wert**, nicht dessen ID, mit einem lokalen Editor in einer geschützten Datei. Zertifikat/Schlüssel und Client-Geheimnis dürfen nicht gleichzeitig konfiguriert sein. Keine Geheimnisse in Shellargumente, Umgebungswerte oder Tickets kopieren.

Die konkrete Zertifikatsanmeldung beschreibt [Microsoft](https://learn.microsoft.com/en-us/entra/identity-platform/certificate-credentials).

#### Graph auf genau ein Absenderpostfach beschränken

Ein global in Entra gewährtes `Mail.Send` darf grundsätzlich aus jedem Postfach des Tenants senden. Die folgende Variante verwendet deshalb ausschließlich **Application RBAC** mit der Rolle `Application Mail.Send`; gewähren Sie dieser App keine zusätzliche unbeschränkte Entra-Anwendungsberechtigung `Mail.Send`. Bereits bestehende globale Admin-Consents müssen in den Berechtigungen der Unternehmensanwendung widerrufen werden, nicht nur aus der konfigurierten Berechtigungsliste verschwinden. Globale Grants und RBAC gelten additiv.

Installieren Sie auf einem Administratorrechner das Microsoft-Modul und verbinden Sie sich mit Exchange Online. Ersetzen Sie die drei Beispielwerte durch die zuvor notierten IDs und das vorhandene Absenderpostfach:

```powershell
Install-Module ExchangeOnlineManagement -Scope CurrentUser
Connect-ExchangeOnline
$appId = '11111111-1111-1111-1111-111111111111'
$enterpriseObjectId = '22222222-2222-2222-2222-222222222222'
$sender = 'craftcloud@example.org'
New-ServicePrincipal -AppId $appId -ObjectId $enterpriseObjectId -DisplayName 'CraftCloud Mail'
$principal = Get-ServicePrincipal -Identity $enterpriseObjectId
New-ManagementScope -Name 'CraftCloudSender' -RecipientRestrictionFilter "PrimarySmtpAddress -eq '$sender'"
New-ManagementRoleAssignment -Name 'CraftCloudGraphSend' -Role 'Application Mail.Send' -App $principal.Identity -CustomResourceScope 'CraftCloudSender'
Test-ServicePrincipalAuthorization -Identity $principal.Identity -Resource $sender
Test-ServicePrincipalAuthorization -Identity $principal.Identity -Resource 'anderes-postfach@example.org'
```

Die erste Prüfung muss `InScope=True`, die zweite für ein tatsächlich existierendes fremdes Postfach `False` ergeben. Änderungen können verzögert wirksam werden. Testen Sie danach die tatsächliche Zustellung und den verweigerten Zugriff mit einem frischen Token. Eine bestehende Application Access Policy ist eine alternative Beschränkung globaler Entra-Grants, wird hier aber nicht mit RBAC vermischt. Hintergrund und Grant-Grenzen: [Exchange Application RBAC](https://learn.microsoft.com/en-us/exchange/permissions-exo/application-rbac).

Für direkten Serverbetrieb setzen Sie diese nicht geheimen Werte und Pfade:

```dotenv
CRAFTCLOUD_MAIL_TRANSPORT=graph
CRAFTCLOUD_GRAPH_TENANT_ID=11111111-1111-1111-1111-111111111111
CRAFTCLOUD_GRAPH_CLIENT_ID=22222222-2222-2222-2222-222222222222
CRAFTCLOUD_GRAPH_SENDER=craftcloud@example.org
CRAFTCLOUD_GRAPH_CERT_FILE=/etc/craftcloud/mail/client.crt
CRAFTCLOUD_GRAPH_KEY_FILE=/etc/craftcloud/mail/client.key
CRAFTCLOUD_GRAPH_SAVE_TO_SENT=false
```

Bei Client-Geheimnis ersetzen Sie beide Zertifikatspfade durch `CRAFTCLOUD_GRAPH_CLIENT_SECRET_FILE`. `SAVE_TO_SENT` ist standardmäßig `false`; `true` legt gesendete Nachrichten im Absenderpostfach ab. Der Token-Scope lautet `https://graph.microsoft.com/.default`. Die bestehende Datenbankkonfiguration, öffentliche URL und der AC-02-Serverschlüssel bleiben erforderlich.

#### Microsoft SMTP mit XOAUTH2

Verwenden Sie eine getrennte App, wenn deren Berechtigungen unabhängig von Graph verwaltet werden sollen. Wählen Sie unter **API-Berechtigungen > Berechtigung hinzufügen > Von meiner Organisation verwendete APIs > Office 365 Exchange Online > Anwendungsberechtigungen** `SMTP.SendAsApp`, anschließend **Administratorzustimmung erteilen**. Registrieren Sie deren Dienstprinzipal wie oben in Exchange und geben Sie ausschließlich das Absenderpostfach frei:

```powershell
Add-MailboxPermission -Identity 'craftcloud@example.org' -User $principal.Identity -AccessRights FullAccess
Add-RecipientPermission -Identity 'craftcloud@example.org' -Trustee $principal.Identity -AccessRights SendAs
Set-CASMailbox -Identity 'craftcloud@example.org' -SmtpClientAuthenticationDisabled $false
```

Prüfen Sie die tenantweiten Sicherheits- und SMTP-AUTH-Richtlinien vor einer Änderung; die Postfachfreigabe allein kann eine tenantweite Sperre nicht aufheben. Verwenden Sie für `$principal` die SMTP-App, nicht versehentlich die Graph-App. [Microsofts SMTP-OAuth-Anleitung](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth) erläutert Dienstprinzipal und Postfachbindung.

```dotenv
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_HOST=smtp.office365.com
CRAFTCLOUD_SMTP_PORT=587
CRAFTCLOUD_SMTP_TLS=starttls
CRAFTCLOUD_SMTP_FROM=CraftCloud <craftcloud@example.org>
CRAFTCLOUD_SMTP_USERNAME=craftcloud@example.org
CRAFTCLOUD_SMTP_AUTH=xoauth2
CRAFTCLOUD_SMTP_OAUTH_PROVIDER=microsoft
CRAFTCLOUD_SMTP_OAUTH_TENANT_ID=11111111-1111-1111-1111-111111111111
CRAFTCLOUD_SMTP_OAUTH_CLIENT_ID=22222222-2222-2222-2222-222222222222
CRAFTCLOUD_SMTP_OAUTH_CERT_FILE=/etc/craftcloud/mail/client.crt
CRAFTCLOUD_SMTP_OAUTH_KEY_FILE=/etc/craftcloud/mail/client.key
```

Alternativ verwenden Sie `CRAFTCLOUD_SMTP_OAUTH_CLIENT_SECRET_FILE` statt Zertifikat und Schlüssel. `SMTP_PASSWORD_FILE` bleibt entfernt. Benutzername und tatsächliche Adresse in `SMTP_FROM` müssen übereinstimmen. Der Scope ist `https://outlook.office365.com/.default`.

#### Google Workspace SMTP mit domainweiter Delegierung

1. Erstellen Sie in der Google Cloud Console ein Projekt und unter **IAM und Verwaltung > Dienstkonten** ein Dienstkonto für CraftCloud. Aktivieren Sie dessen domainweite Delegierung; notieren Sie die numerische OAuth-Client-ID. Erstellen Sie unter **Schlüssel > Schlüssel hinzufügen > Neuen Schlüssel erstellen > JSON** eine Schlüsseldatei, sofern die Organisationsrichtlinie dies erlaubt.
2. Ein Workspace-Superadministrator öffnet **Admin-Konsole > Sicherheit > Zugriff und Datenkontrolle > API-Steuerung > Domainweite Delegierung verwalten**. Fügen Sie die numerische Client-ID und genau den Scope `https://mail.google.com/` hinzu. Diese Delegierung ist weitreichend; schützen Sie das Dienstkonto entsprechend und verwenden Sie es ausschließlich für den vorgesehenen Versand.
3. Legen Sie die JSON-Datei mit Modus 0600 im geschützten Mailverzeichnis ab. Verwenden Sie ein vorhandenes Workspace-Postfach als `SMTP_USERNAME` und identische Absenderadresse. CraftCloud setzt dieses Postfach als JWT-`sub`; es meldet sich keine Person interaktiv an.

```dotenv
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_HOST=smtp.gmail.com
CRAFTCLOUD_SMTP_PORT=587
CRAFTCLOUD_SMTP_TLS=starttls
CRAFTCLOUD_SMTP_FROM=CraftCloud <craftcloud@example.org>
CRAFTCLOUD_SMTP_USERNAME=craftcloud@example.org
CRAFTCLOUD_SMTP_AUTH=xoauth2
CRAFTCLOUD_SMTP_OAUTH_PROVIDER=google
CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_FILE=/etc/craftcloud/mail/google-service-account.json
```

Microsoft-Credentialfelder und `SMTP_PASSWORD_FILE` bleiben entfernt. Siehe [Google-Dienstkonten und Delegierung](https://developers.google.com/identity/protocols/oauth2/service-account) sowie [Gmail XOAUTH2](https://developers.google.com/workspace/gmail/imap/xoauth2-protocol).

#### Betrieb und Prüfung

Bei Compose/Installation verwenden Sie für die geheimen Hostdateien die entsprechenden `*_SOURCE`-Einstellungen statt `*_FILE`. Die gewählten Overlays binden diese ein; `secrets-init` verteilt sie ausschließlich in das servereigene Secret-Verzeichnis. Für direkten Betrieb müssen die Dateien dem Serverkonto gehören und nur für dieses lesbar sein. Die Zertifikatsdatei ist öffentlich; sie wird wegen konsistenter Verteilung ebenfalls als Datei geführt. Nach einem Konfigurationswechsel verwenden Sie `craftcloud-ctl stop` und `craftcloud-ctl start` im Wartungsfenster.

Unter **System** sehen Sie Transport, Absender, Zertifikatsablauf, letzten erfolgreichen Versand und letzten kontrollierten Providerfehler. Senden Sie die Testmail an die eigene Adresse. Prüfen Sie außerdem Spamordner, Gesendet-Ordner gemäß Einstellung und den tatsächlichen Absender. `invalid_client` deutet auf App-/Credentialkonfiguration, `ErrorAccessDenied` beziehungsweise `provider_forbidden` auf Berechtigungen; bei `429` berücksichtigt die Queue `Retry-After`. Zertifikate und Secrets werden vor Ablauf ersetzt, neue öffentliche Zertifikate zuerst in Entra hochgeladen und erst danach die Dateien gewechselt. Behalten Sie das alte Zertifikat bis zur erfolgreichen Prüfung des neuen bei.

Echte Microsoft-/Google-Tenants gehören zur Betreiberabnahme; lokale Tests beweisen keine Tenantberechtigung. Beim ersten Rollout muss auch der Zugriff auf ein unzulässiges Postfach tatsächlich verweigert werden. Tokens, Schlüssel, JSON-Dateien oder Mailinhalte dürfen nicht in Diagnosepakete aufgenommen werden.

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

Für Microsoft 365 verwenden Sie einen für Ihre Ausgangs-IP freigegebenen Eingangsconnector, den MX-Endpunkt Ihrer akzeptierten Domain als SMTP-Host, Port 25 und `starttls`, ohne Benutzername oder Passwort. Die Absenderdomain muss im Tenant akzeptiert sein. Port 25 muss ausgehend erreichbar sein. Ein zertifikatsbasierter Connector benötigt einen vorgeschalteten Relay mit passendem Clientzertifikat. Microsoft Graph und SMTP mit OAuth sind oben unter AC-09b beschrieben. Siehe [Microsofts Connector-Anleitung](https://learn.microsoft.com/en-us/exchange/mail-flow-best-practices/how-to-set-up-a-multifunction-device-or-application-to-send-email-using-microsoft-365-or-office-365).

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
