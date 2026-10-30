# Backup, Restore und Upgrades – Betreiber-Runbook (CC-F14, CC-P03-08)

Für eine Installation aus einem Release-Archiv beschreibt [install.md](install.md) die Befehle `craftcloud-ctl backup`, `upgrade` und `rollback` sowie den systemd-Zeitplan. Die folgenden manuellen Compose-Beispiele beziehen sich auf die Entwicklungs-Vorlage; bei OPS-01 gehören zusätzlich `--env-file /etc/craftcloud/craftcloud.env` und `-f compose.install.yaml` sowie die installierten Datenpfade dazu. Ein Programm-Rollback ersetzt keinen Datenbank-Restore.

Dieses Runbook beschreibt, was `craftcloud-server admin backup create | verify | restore` (PR-019) tut, wie Sie es mit Docker Compose betreiben und was gemessen wurde. Es ist ein Entwicklungsprototyp: Das Verfahren wurde mit kleinen Datenmengen auf einem Entwicklungsrechner geprüft (siehe „Gemessene Werte“), nicht mit großen Beständen oder auf einem Linux-Produktivhost.

## Was gesichert wird

Ein Backup ist ein Verzeichnis. Es enthält zusammengehörig:

| Inhalt | Datei | Hinweis |
| --- | --- | --- |
| Persistente Tabellen laut `crates/craftcloud-backup/src/tables.rs`, einschließlich Freigaben, verschlüsselter TOTP-Geheimnisse und gehashter Wiederherstellungscodes | `tables/<Tabelle>.copy` (PostgreSQL-`COPY`-Textformat) | aus **einem** Snapshot (`REPEATABLE READ`, nur lesend) |
| die Bytes aller `committed` Blobs | `blobs/<xx>/<Schlüssel>` | jeder Blob wird beim Kopieren gegen Größe und SHA-256 aus der Datenbank geprüft |
| Blobliste | `blobs.jsonl` | eine Zeile je Blob, aufsteigend nach Schlüssel |
| Manifest | `manifest.json`, `manifest.sha256` | Format- und Migrationsversion, Serverversion, Zeitpunkt des Snapshots (Recovery-Punkt), je Tabelle Zeilenzahl, Größe, SHA-256, Quell-Speicherkennung; Prüfsumme des Manifests |

**Nicht** im Backup (mit Absicht):

- Browser- und Geräte-Sitzungen, Refresh-Historie, kurzlebige Zwei-Faktor-Challenges und Bearbeitungsleases: Nach einem Restore sind **alle Sitzungen ungültig**, alle Personen melden sich neu an. Ein gestohlenes Backup enthält so keine verwendbaren Sitzungen.
- `password_reset_tokens`: Einmal-Links; nach dem Restore neue ausstellen (`admin issue-password-reset`).
- `storage_binding` und `maintenance_runs`: Die wiederhergestellte Instanz bekommt eine **neue Speicherkennung** (Marker-Datei und Datenbankzeile, Epoche 0) und baut ihren Wartungsverlauf neu auf.
- Blobs, die zum Snapshot nicht `committed` waren (laufende Uploads, `orphaned`, `corrupt`, `deleting`). Eine zum Backupzeitpunkt **laufende Upload-Sitzung** wird beim Restore auf `aborted` gesetzt (ihr Blob auf `orphaned`; die Reservierung bleibt für die Bytes dieser verwaisten Blobs bestehen, bis der nächste Wartungslauf sie einsammelt und freigibt, alle anderen Reservierungen sind 0); der Client lädt neu hoch. Gibt es Versionen, deren Blob nicht `committed` war (zum Beispiel `corrupt`), warnt `backup create` (`unavailable_version_blobs` im Manifest).
- **Geheimnisse:** Datenbankpasswörter, Bootstrap-Token, Metrik-Token (`.env` und Volume `secrets` des Compose-Stacks), TLS-Zertifikate (`caddy_data`) sowie die Dateien `CRAFTCLOUD_SECRET_KEY_FILE` und `CRAFTCLOUD_SECRET_KEY_OLD_FILE`. Sitzungs- und Reset-Token liegen nur als Hash in der Datenbank. Wer die Compose-Instanz neu aufbaut, erzeugt neue Infrastruktur-Secrets (`cargo xtask compose-secrets`). **Für vorhandene TOTP-Geheimnisse muss dagegen derselbe passende Entschlüsselungsschlüssel wieder bereitgestellt werden.** Bewahren Sie ihn getrennt und geschützt auf; ein neuer zufälliger Schlüssel entschlüsselt das Backup nicht. `caddy_data` sichern Sie getrennt, sonst drohen Rate-Limits der CA.

Das Backup enthält **alles, was ein Datenbankadministrator lesen kann**: Passwort-Hashes (Argon2), E-Mail-Adressen, Namen, das Audit-Log und sämtliche Dateiinhalte im Klartext. Es wird mit Modus `0700` (Verzeichnisse) und `0600` (Dateien) auf unix geschrieben, ist aber **nicht verschlüsselt** (siehe „Off-site-Kopie und Verschlüsselung“). Das Manifest enthält keine Zugangsdaten.

## Backup erstellen

Bei aktivem 2FA ist die Wiederherstellbarkeit von [Schlüsseldateien und Rotation](deployment.md#backup-und-restore-mit-2fa) Teil jeder Restore-Probe. Nach einem produktiven Restore sind auch Wiederherstellungscodes wieder im Zustand des Backupzeitpunkts; lassen Sie sie neu erzeugen. Ein verlorener Entschlüsselungsschlüssel darf nicht durch Abschalten der Login-Prüfung umgangen werden; der gezielte Operator-Reset verlangt eine erneute Einrichtung.

Voraussetzungen: Datenbank migriert, der Server lief mindestens einmal (Datenbank und Speicherverzeichnis sind aneinander gebunden, siehe „Wartungsjob“ in `deployment.md`), `CRAFTCLOUD_DATABASE_URL_FILE` und `CRAFTCLOUD_STORAGE_DIR` gesetzt. Das Zielverzeichnis darf nicht existieren oder muss leer sein; ein Backup schreibt nie in vorhandene Daten. Ein vorhandenes leeres Verzeichnis wird unter unix auf Modus 0700 gesetzt (es enthält später alles). Scheitert ein Backup, werden die von ihm geschriebenen Einträge entfernt: ein neu angelegtes Verzeichnis ganz, ein vorhandenes leeres bleibt leer bestehen.

Mit Docker Compose (einmaliger Dienst `backup` im Profil `tools`; er bindet `store` und `backups` ein, der Server läuft währenddessen weiter):

```sh
cd deploy/compose
TS=$(date -u +%Y%m%dT%H%M%SZ)
docker compose run --rm backup admin backup create --out /backups/$TS
```

Ohne Compose: `craftcloud-server admin backup create --out <Verzeichnis>` mit denselben Umgebungsvariablen wie der Server. Unter Git Bash auf Windows verändert die Pfadumwandlung Argumente wie `/backups/...`; vorher `export MSYS_NO_PATHCONV=1` setzen.

Ausgabe (nur Zähler, keine Namen): Recovery-Punkt, Server- und Migrationsversion, Tabellen und Zeilen, Blobs und Bytes, Dauer und die **SHA-256 des Manifests**. Notieren Sie diese Prüfsumme **an einem anderen Ort als das Backup** (Ticket, Passwortmanager, Monitoring): Sie ist der Anker gegen Manipulation (siehe „Prüfen“). Exit 0 nur, wenn das Backup vollständig ist **und** das Audit-Ereignis `backup.created` geschrieben wurde; sonst 1.

Was dabei passiert:

1. Prüfung, dass das Schema zu dieser Version passt und das Speicherverzeichnis zur Datenbank gehört (sonst Abbruch, nichts wird geschrieben).
2. Das Backup nimmt den **Wartungs-Lock** (denselben PostgreSQL-Advisory-Lock wie der Wartungsjob). Solange es ihn hält, **überspringen** Wartungsläufe (Papierkorb-Löschung, Versionsaufbewahrung, Garbage Collection, Abstimmung) ihren Lauf; es kann also kein vom Snapshot referenzierter Blob verschwinden. Läuft gerade ein Wartungslauf, wartet das Backup bis zu 120 Sekunden darauf und bricht sonst mit „the maintenance job (or another backup) is running“ ab. Der Lock gehört zur Datenbankverbindung: Stürzt der Backup-Prozess ab oder reißt die Verbindung, gibt PostgreSQL ihn frei, ein abgebrochenes Backup blockiert die Garbage Collection also nicht dauerhaft.
3. Alle Tabellen werden in **einer** Transaktion (`REPEATABLE READ READ ONLY`) mit `COPY … TO STDOUT` gestreamt; die Blobliste stammt aus demselben Snapshot. Danach wird die Transaktion geschlossen (keine langen Transaktionen während des Kopierens).
4. Die Blobs werden aus dem Speicher kopiert (committed Blobs sind unveränderlich), jeder gegen Größe und SHA-256 geprüft. Fehlt ein Objekt oder stimmt seine Prüfsumme nicht, bricht das Backup ab (`SourceDamaged`) und entfernt das von ihm angelegte Verzeichnis; ein Teilbackup wird nie als vollständig gemeldet.
5. Zuletzt entstehen `manifest.json` und `manifest.sha256`. Ein Verzeichnis ohne Manifest ist unvollständig und wird von `verify` und `restore` abgelehnt.
6. Audit: `backup.created` (Ergebnis `success` oder `failed`, nur Zähler, ohne Akteur: Host-Zugriff ist die Autorität), im Audit-Log der Instanz.

**Speicher und Last:** Tabellen, Blobliste und Blobs laufen als Datenstrom in Blöcken von höchstens 256 KiB; nichts wird vollständig in den Speicher geladen. Während des Backups hält es zwei Datenbankverbindungen (Lock und Snapshot; `CRAFTCLOUD_DATABASE_MAX_CONNECTIONS` mindestens 2, Standard 10). Der Speicherplatz des Zielvolumes muss für Datenbank-Export plus alle committed Blobs reichen (keine Prüfung vorab; bei vollem Datenträger bricht das Backup ab und räumt auf).

**Backup-Alter:** Der letzte erfolgreiche Lauf erscheint als `backup.last_success_age_seconds` in `GET /api/v1/admin/health` und in der Ansicht „System“. Die Zeit stammt aus dem Audit-Ereignis `backup.created`; löscht die Audit-Aufbewahrung (`CRAFTCLOUD_AUDIT_RETENTION_DAYS`) das Ereignis, verschwindet das Alter, die Aufbewahrung muss also länger sein als das Backup-Intervall (Standard 365 Tage).

**Zeitplan:** Das Serverbinary selbst hat keinen Backup-Zeitplan. OPS-01 installiert einen systemd-Timer für täglich 03:15 mit bis zu 30 Minuten zufälliger Verzögerung und bewahrt standardmäßig sieben erfolgreiche Backups auf (`CRAFTCLOUD_BACKUP_KEEP`, siehe [install.md](install.md)). Bei manueller Compose-Nutzung richten Sie Cron oder einen systemd-Timer und die Rotation selbst ein. Die verschlüsselte Kopie außer Haus bleibt in beiden Fällen Ihre Aufgabe.

## Prüfen

```sh
docker compose run --rm backup admin backup verify /backups/$TS --manifest-sha256 <Prüfsumme aus der Ausgabe von create>
```

`verify` braucht weder Datenbank noch Konfiguration und liest nur das Verzeichnis. Es berechnet **alle** Prüfsummen neu und lehnt ab:

- ein fehlendes, zusätzliches, verändertes oder in der Größe abweichendes Tabellen- oder Blobfile, eine abweichende Zeilenzahl,
- eine nicht sortierte oder doppelte Blobliste, überlange Zeilen, unbekannte Felder,
- ein Manifest, das nicht zu `manifest.sha256` (oder zur mit `--manifest-sha256` angegebenen Prüfsumme) passt,
- ein unbekanntes Format (`format_version` ≠ 1) und eine **Migrationsversion, die dieser Server nicht kennt** (Backup eines neueren Servers: mit der Version restoren, die es erstellt hat, oder einer neueren),
- jede Datei oder jedes Verzeichnis, das das Manifest nicht aufführt, und Symlinks.

Wichtig: `manifest.sha256` liegt neben dem Manifest und erkennt **Beschädigung**, aber keine absichtliche Fälschung (wer die Dateien ändern kann, ändert auch die Prüfsumme). Gegen Manipulation hilft nur die getrennt aufbewahrte Prüfsumme (`--manifest-sha256`) oder eine Signatur außerhalb dieses Werkzeugs.

## Off-site-Kopie und Verschlüsselung

Die Verschlüsselung ruhender Backups ist **Ihre Entscheidung** und wird von CraftCloud nicht umgesetzt. Üblich: das Verzeichnis außerhalb des Hosts verschlüsseln und dann kopieren, zum Beispiel mit `age` oder `gpg`:

```sh
# Auf dem Host, Pfad des Volumes mit `docker volume inspect <Projekt>_backups` ermitteln
tar -C /var/lib/docker/volumes/craftcloud_backups/_data -cf - $TS | age -r <öffentlicher Schlüssel> > $TS.tar.age
# Danach auf ein anderes System/anderen Anbieter kopieren (rclone, rsync, Objektspeicher) und die Prüfsumme des Manifests getrennt ablegen.
```

Den Schlüssel zum Entschlüsseln bewahren Sie getrennt vom Backup auf (Schlüssel-Escrow ist Ihre Aufgabe: geht er verloren, ist das Backup wertlos). Löschen Sie lokale Kopien nach der Übertragung, wenn Ihr Konzept das verlangt: Endgültig gelöschte Daten bleiben in älteren Backups, bis diese ablaufen (Datenschutz, Löschkonzept).

## Restore auf einer separaten Instanz

Ein Restore schreibt nur in eine **leere** Datenbank und ein **leeres** Speicherverzeichnis und überschreibt nie etwas. Er läuft so:

1. `verify` über das ganze Verzeichnis (vor dem ersten Schreibzugriff). Danach nimmt der Restore den Wartungs-Lock und hält ihn bis zum Ende: Ein zweiter Restore, ein Backup oder ein Wartungslauf gleichzeitig scheitert oder überspringt mit „… holds the lock“ (`Busy`) und berührt nichts von dem, was dieser Restore schreibt.
2. Prüfung, dass das Ziel leer ist: kein Objekt, keine Marker-Datei, keine temporäre Datei im Speicher; eine Datenbank ohne Tabellen **oder** genau auf die Migrationsversion des Backups migriert und mit leeren Tabellen (auch `sessions`, `storage_binding`, `audit_events`). Sonst Abbruch mit „the target … is not empty“. Welche Tabellen erwartet werden, richtet sich nach der **Migrationsversion des Backups** (jede Tabelle kennt die Version, die sie eingeführt hat); Backups vor Migration 11 (ohne Audit-Log) werden mit einer klaren Meldung abgelehnt. Die Datenbankverbindung muss die **Owner-Rolle** sein (Eigentümerin von `audit_events`): Eine andere Rolle würde durch den Audit-Schutz (Migration 0012) Zeitstempel und Zähler der wiederhergestellten Ereignisse stillschweigend überschreiben, deshalb bricht der Restore vorher mit „the table owner is …“ ab. Im Ein-Rollen-Betrieb ist die einzige Rolle der Owner.
3. Eine leere Datenbank wird bis zur Migrationsversion **des Backups** migriert (und der Runtime-Rolle werden die Datenrechte gegeben).
4. Die Blobs werden unter ihren ursprünglichen Schlüsseln in den Speicher geschrieben; jedes Objekt wird beim Schreiben gegen SHA-256 und Größe geprüft. Die Blobliste wird beim Lesen gehasht und mit Prüfsumme, Anzahl und Gesamtgröße des Manifests verglichen: Eine nach `verify` ausgetauschte Liste (und damit ein Umgehen von `--manifest-sha256`) führt zum Abbruch.
5. **Eine** Transaktion lädt die Tabellen mit `COPY … FROM STDIN` in Fremdschlüssel-Reihenfolge (Zyklen `users` ↔ `invitations` und `files` ↔ `file_versions` werden über eine nachträglich gesetzte Spalte aufgelöst), setzt unfertige Uploads auf `aborted` und schreibt das Audit-Ereignis `backup.restored`. Jede Tabellendatei wird dabei gehasht (Länge und SHA-256) und vor dem Commit mit dem Manifest verglichen („changed after verification“). In derselben Transaktion entsteht die Zeile `storage_binding` mit der neuen Kennung; die Marker-Datei wird unmittelbar vor dem Commit geschrieben und entfernt, wenn der Commit scheitert. Scheitert irgendetwas bis dahin, bleiben weder Zeilen noch Binding, Marker oder Objekte (die geschriebenen Objekte werden anhand der Liste entfernt, die der Restore selbst geführt hat, nicht aus der Backup-Datei neu gelesen). Nach dem Commit gibt es keinen weiteren Schritt, ein erfolgreicher Restore ist also vollständig. Einzige Ausnahme: Lässt sich die Marker-Datei nach einem gescheiterten Commit nicht entfernen, nennt die Fehlermeldung die Datei `craftcloud-store.json`; löschen Sie sie von Hand, bevor Sie es erneut versuchen. Hat ein anderer Prozess (etwa ein Server) währenddessen Binding oder Marker angelegt, scheitert der Restore und lässt dessen Marker unangetastet.
6. Das Paar aus Datenbank und Speicher bekommt dabei eine **neue Kennung** (Marker-Datei und `storage_binding`, Epoche 0). Es gibt keine Verwechslung mit der Quelle und keinen Epochen-Konflikt.

Die Datenbank erhält keine Sequenzen zum Zurücksetzen (keine seriellen Spalten in den gesicherten Tabellen; ein Test prüft das und dass jede Tabelle entweder gesichert oder mit Begründung ausgenommen ist).

### Ablauf mit Compose (neues Projekt, andere öffentliche URL)

Auf dem Zielhost (oder mit einem anderen Projektnamen, `-p craftcloud-restore`, und anderen Ports auf demselben Host). Das Backup-Verzeichnis (entschlüsselt) muss im Volume `backups` des Zielprojekts liegen.

```sh
cd deploy/compose
cargo xtask compose-secrets --domain restore.example.org      # eigene .env mit eigenen Secrets und CRAFTCLOUD_PUBLIC_URL
docker compose -p craftcloud-restore build migrate            # baut das Image (oder CRAFTCLOUD_IMAGE auf ein vorhandenes setzen)
docker compose -p craftcloud-restore create restore           # legt das Volume backups an
# Backup (entschlüsselt) in das Volume bringen; Beispiel: Verzeichnis ./b1 auf dem Host
docker run --rm -v craftcloud-restore_backups:/backups -v "$PWD/b1":/in:ro postgres:18.6 sh -c 'cp -a /in /backups/b1 && chown -R 65532:65532 /backups/b1'
# Datenbank und Rollen vorbereiten, aber den Server noch NICHT starten
docker compose -p craftcloud-restore run --rm db-init
docker compose -p craftcloud-restore run --rm restore admin backup restore /backups/b1 --manifest-sha256 <Prüfsumme>
# Jetzt starten: migrate bringt ein älteres Backup auf den aktuellen Stand, der Server bindet Datenbank und Speicher
docker compose -p craftcloud-restore up -d
```

Wichtig:

- **Den Server erst nach dem Restore starten.** Ein gestarteter Server bindet den Speicher (legt Marker und Binding an); dann ist der Speicher nicht mehr leer und der Restore lehnt ab. Das ist gewollt.
- Der Dienst `restore` benutzt die Owner-Rolle (`CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE`), weil er das Schema anlegen kann und weil das Audit-Log nur dann seine ursprünglichen Zeitstempel behält. Mit der Runtime-Rolle bricht der Restore ab (siehe oben), statt das Protokoll zu verändern.
- **Öffentliche URL:** In der Datenbank steht keine URL (Einladungs- und Reset-Links entstehen beim Ausstellen aus `CRAFTCLOUD_PUBLIC_URL`). Eine Test-Instanz mit **anderer** `CRAFTCLOUD_PUBLIC_URL` funktioniert deshalb ohne Eingriff; ausgestellte, noch offene Einladungen bleiben gültig, ihre Links müssen aber mit der neuen URL neu zugestellt werden, weil nur der Hash gespeichert ist (Einladung widerrufen und neu anlegen). **Einladungen:** Eine Einladung, die **nach dem Recovery-Punkt widerrufen** wurde, ist nach dem Restore wieder gültig (bis zu ihrem Ablauf), und wer den alten Link besitzt, kann ihn einlösen. Prüfen Sie nach einem Restore einer Produktivinstanz alle offenen Einladungen und widerrufen Sie, was nicht mehr gelten soll (Einladungsliste der Organisation oder die API); einen Befehl, der alle offenen Einladungen widerruft, gibt es noch nicht (`docs/gaps.md`). Das Instanz-Setup gilt als abgeschlossen (`instance_setup` wird wiederhergestellt); der Bootstrap-Token öffnet nichts mehr.
- Ein Backup einer **älteren** Version wird auf deren Schema wiederhergestellt; der Ablauf ist **Restore, dann Migrate** (`docker compose up -d` führt `migrate` aus, oder `craftcloud-server migrate`). Ein Backup einer **neueren** Version lehnt dieser Server ab.

### Nach dem Restore prüfen

1. `docker compose -p craftcloud-restore logs migrate server` ohne Fehler; `https://<domain>/health/ready` antwortet `200`.
2. Anmelden (alle müssen sich neu anmelden); Mitglieder, Workspaces und Rollen stimmen; ein Mitglied eines anderen Workspaces sieht fremde Projekte nicht (404).
3. Einige Dateien aus mehreren Workspaces herunterladen und mit den Quelldaten vergleichen (SHA-256); eine neue Version hochladen.
4. `docker compose exec server /usr/local/bin/craftcloud-server admin maintenance --once --dry-run` ohne Fehler.
5. Der Zustand entspricht dem **Recovery-Punkt** des Manifests. Alles, was danach geschah, fehlt: neue Uploads, Rollenänderungen, **Deaktivierungen und entfernte Mitglieder tauchen wieder auf**, Passwörter sind die vom Backupzeitpunkt. Prüfen Sie nach einem Restore einer Produktivinstanz Konten und Mitgliedschaften und stellen Sie Sperren wieder her.
6. Test beendet? `docker compose -p craftcloud-restore down -v` entfernt die Testinstanz mit allen Daten.

### Restore auf der Produktivinstanz (Notfall)

Wie oben, aber zuerst den Server stoppen und die Volumes `pgdata` und `store` bewusst durch leere ersetzen (ein Restore überschreibt nie; Daten, die Sie noch brauchen könnten, sichern Sie vorher). Es gibt keinen „Merge“-Modus.

## Restore-Drill (Abnahme)

Der automatische Test `restore_drill_two_users_two_workspaces_and_a_version_history` (`cargo xtask db-test`) legt zwei Besitzer in zwei Organisationen mit je einem Workspace, ein weiteres Mitglied mit Rolle `editor`, ein Projekt, eine Datei mit vier Versionen (150 000, 70 001, 5 und 1 Byte) und eine zweite Datei an, sichert, stellt in eine **getrennte frische Datenbank und ein getrenntes Speicherverzeichnis** wieder her und prüft: Zeilen jeder Tabelle identisch (Audit-Log zusätzlich `backup.restored`), jedes Blob byte-identisch mit gleichem SHA-256, Zugriff über die HTTP-API der wiederhergestellten Instanz (Besitzer und Editor lesen alle Versionen, Fremde erhalten 404, der Editor kann keine Mitglieder verwalten, alte Sitzungen sind ungültig, eine neue Version lässt sich hochladen), neue Speicherkennung und Marker. Weitere Tests: Manipulation (veränderte Tabelle, verändertes, fehlendes, zu großes Blob, zusätzliche Dateien, gefälschtes Manifest, unbekannte Format- und Migrationsversion), Restore in nicht leere Ziele, Wartungslauf während eines Backups, Upload während eines Backups, Backup-Alter in der Gesundheitsansicht, Kommandozeile Ende zu Ende.

Den Drill auf Ihrem Host wiederholen Sie vor produktivem Betrieb und nach jedem Upgrade mit **Ihren** Daten auf einer getrennten Instanz (Abschnitt oben).

## Upgrade

Schema-Migrationen sind nur vorwärts gerichtet (kein Down-Skript, ADR-0012): Das Zurückspielen eines Backups ist der Rollback-Pfad.

1. **Vorher:** `backup create`, `backup verify` mit der notierten Prüfsumme, **Restore-Probe** auf einer getrennten Instanz mit der **aktuellen** Version (Abschnitt oben, bis zur erfolgreichen Anmeldung und einem Download). Erst wenn die Probe gelingt, ist das Backup ein Rollback-Pfad. Release Notes und Migrationen lesen.
2. Quellen aktualisieren, `docker compose up -d --build` (`migrate` wendet nur ausstehende Migrationen an).
3. **Nachher:** `/health/ready` = 200, Ansicht „System“ ohne Warnungen, Anmeldung, Projektliste, Download einer bekannten Datei mit Prüfsumme, Upload einer neuen Version, `admin maintenance --once --dry-run`, Backup-Alter (jetzt neues Backup anlegen, wenn die Migration das Schema verändert hat).
4. **Rollback:** Den Stack auf die alte Version zurücksetzen, **leere** Volumes für Datenbank und Speicher verwenden und das Backup von vor dem Upgrade restoren (die alte Version kennt die neue Migrationsversion nicht). Das verwirft alles seit dem Backup.
5. Keine stillen Downgrades über nicht rückwärtskompatible Migrationen.
6. **Quota nach dem Abbruch:** Migration 0014 berechnet die Reservierungen neu (offene Sitzungen plus `orphaned` Blobs; nur Daten, keine Tabellenänderung). Seitdem zählt der Inhalt einer abgebrochenen oder abgelaufenen, bereits gespeicherten (`stored`) Upload-Sitzung weiter gegen die Quota, bis die Garbage Collection den Blob entfernt hat, also etwa Upload-TTL plus eine Stunde (Standard rund 25 Stunden) nach dem Anlegen des Blobs, zuzüglich des Wartungsintervalls. Das ist gewollt (der Inhalt liegt bis dahin auf dem Datenträger); ein Wartungslauf nach dem Upgrade ist nicht nötig.

## Fehlerbilder

| Meldung (Auszug) | Ursache | Abhilfe |
| --- | --- | --- |
| `the database and CRAFTCLOUD_STORAGE_DIR are not bound to each other` | falsches Speicherverzeichnis, Server lief noch nie, Binding fehlt | Verzeichnis prüfen; Server einmal starten; bewusst `admin storage-bind --force` |
| `the maintenance job (or another backup) is running` | Wartungslauf oder zweites Backup hält den Lock länger als 120 s | später wiederholen; `docker compose exec server … admin maintenance --once` nicht gleichzeitig |
| `object <Schlüssel> is missing from the store` / `does not match its recorded checksum` | Speicher beschädigt oder unvollständig (zum Beispiel nur die Datenbank zurückgespielt) | Speicher aus passendem Stand herstellen; Wartungsmeldung `missing_objects` prüfen; **kein** Teilbackup wird angelegt |
| `the output directory is not empty` | Zielverzeichnis enthält Daten | neues Verzeichnis wählen |
| `… does not match its checksum`, `has the wrong size`, `is missing`, `unexpected file` | Backup beschädigt oder verändert | anderes Backup verwenden, Quelle des Transports prüfen |
| `manifest.json does not match the expected manifest digest` | Manifest nicht das, das `create` ausgegeben hat | Backup nicht verwenden, Herkunft klären |
| `the backup has migration version N but this server only knows up to M` | Backup eines neueren Servers | Server auf mindestens diese Version bringen |
| `the target store already has a marker file / objects` | Speicher nicht leer (zum Beispiel ein Server wurde gestartet) | leeres Volume/Verzeichnis verwenden |
| `the target database is not empty (table … has rows)` | Datenbank nicht leer | leere Datenbank verwenden (`down -v` für Testinstanzen) |
| `another restore, a backup or a maintenance run holds the lock` | ein zweiter Restore oder ein Wartungslauf läuft gleichzeitig | warten, dann wiederholen |
| `the restore is connected as role X, but the table owner is Y` | Restore mit der Runtime-Rolle | Dienst `restore` (Owner-URL) verwenden |
| `… changed after verification` | Backup-Datei wurde während des Restores verändert | Backup nicht verwenden, Herkunft und Zugriffe klären |
| `migration version N is older than the first one with backup support` | Backup einer Version vor Migration 11 | nicht unterstützt |
| `the target database is not at the backup's migration version N` | Ziel ist migriert, aber auf einem anderen Stand | leere Datenbank ohne Tabellen verwenden, der Restore migriert selbst |

## Gemessene Werte (RPO/RTO)

Die folgenden Werte sind **gemessen**, gelten nur für den genannten Aufbau und sind keine Zusage.

| Aufbau | Datenmenge | Backup | Restore |
| --- | --- | --- | --- |
| Test `restore_drill_two_users_two_workspaces_and_a_version_history` (Debug-Build, Windows 11, PostgreSQL 18.6 in Docker Desktop mit `fsync=off`, lokale Platte, zwei Läufe) | 53 Zeilen, 5 Blobs, 260 007 Bytes | 53 und 56 ms | 180 und 202 ms (mit Verifikation und Migration der leeren Datenbank) |
| Compose-Stack `craftcloud-pr019` und ein zweites Projekt (Release-Image, Docker Desktop/WSL2, PostgreSQL 18.6, Volumes auf lokaler Platte) | 25 Zeilen, 3 Blobs (24 MiB, 3 MiB und 4 KiB), 28 315 665 Bytes | 0,1 s im Programm, 2,3 s mit Containerstart | 0,2 s im Programm (mit Verifikation, Migration und Rollenvergabe), 3,0 s mit Containerstart |

Gemessener Umfang: **kleine Datenmenge (unter 30 MiB), lokale Platte, ein Entwicklungsrechner, ein Lauf je Zeile bzw. zwei Läufe im Test**. Nicht gemessen: große Bestände, Netzlaufwerke, entfernte Datenbank, Linux-Host, parallele Last durch Benutzer, der Weg des Backups außer Haus.

**RPO** (Datenverlust im Fehlerfall) ist der Abstand zwischen zwei erfolgreichen Backups plus die Dauer des Transports außer Haus; Sie legen ihn durch Ihren Zeitplan fest (kein Point-in-Time-Recovery, keine inkrementellen Backups). **RTO** ist die Zeit für Beschaffen und Entschlüsseln des Backups, Bereitstellen eines Hosts, Restore und Prüfung. Für große Bestände sind die Werte nicht gemessen; sie wachsen mit der Datenmenge etwa linear (Hashen und Kopieren aller Bytes, zweimal beim Restore: einmal in `verify`, einmal beim Schreiben). Planen Sie Ihre Zahl aus einem Drill mit Ihren Daten.

## Grenzen

- Nur der lokale Dateispeicher (`craftcloud-storage`) existiert; es gibt noch keinen S3-Adapter, also auch kein Backup dafür.
- Keine Verschlüsselung, Signatur, Kompression oder Aufbewahrung durch das Werkzeug; keine inkrementellen oder differenziellen Backups; kein Point-in-Time-Recovery über WAL.
- Das Backup ist vollständig, nicht dateiweise: Es gibt keinen Restore einzelner Organisationen oder Dateien.
- Der Backup-Prozess nutzt die Datenbankverbindung des Servers (Runtime-Rolle, Leserechte auf alle Tabellen); wer den Befehl ausführen kann, kann alle Daten lesen: Zugriff auf den Host, den Docker-Socket und die Secrets ist die Autorität.
