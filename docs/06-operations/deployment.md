# Self-hosting-Deployment (Design, keine ausführbare Vorlage)

## Minimaler MVP-Stack

- `craftcloud-server`: Rust service (axum, siehe ADR-0010), listens only in private network/loopback; runs as non-root.
- `postgres`: single supported primary production database; persistent volume, backups.
- `craftcloud-data`: persistent local blob volume, securely mounted.
- Reverse proxy: Caddy/nginx with valid TLS. `PUBLIC_URL=https://cloud.example.org`, known trusted proxy IPs, explicit headers.
- `craftcloud-admin`: static React build served by proxy/same-origin where feasible; CORS/CSRF simple.

## Umgebungsvariablen (Konfiguration nur über Env, siehe ADR-0011)

Alle Schlüssel tragen das Präfix `CRAFTCLOUD_`. Unbekannte `CRAFTCLOUD_*`-Variablen (Tippfehler) und noch nicht unterstützte Schlüssel führen zu einem Startfehler mit Exit-Code 2; es wird nichts gebunden.

| Schlüssel | Standard | Regel |
| --- | --- | --- |
| `CRAFTCLOUD_BIND` | `127.0.0.1:8080` | `<ip>:<port>` |
| `CRAFTCLOUD_PUBLIC_URL` | nicht gesetzt | absolute URL, `https`; `http` nur für Loopback (`localhost`, `127.0.0.0/8`, `::1`); keine Userinfo, kein Query, kein Fragment; abschließender `/` wird entfernt |
| `CRAFTCLOUD_TRUSTED_PROXIES` | leer (niemandem wird vertraut) | kommagetrennte IPs/CIDRs, höchstens 64; `0.0.0.0/0` und `::/0` werden abgelehnt |
| `CRAFTCLOUD_MAX_BODY_BYTES` | `65536` | 1024 bis 16777216 |
| `CRAFTCLOUD_REQUEST_TIMEOUT_SECS` | `10` | 1 bis 300 |
| `CRAFTCLOUD_SHUTDOWN_GRACE_SECS` | `10` | 1 bis 120 |
| `CRAFTCLOUD_LOG_FORMAT` | `text` | `text` oder `json` |
| `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE` | nicht gesetzt (Setup deaktiviert) | absoluter Pfad einer Secret-Datei mit dem Bootstrap-Token (mindestens 32 Zeichen nach dem Trimmen); erfordert `CRAFTCLOUD_DATABASE_URL_FILE` |
| `CRAFTCLOUD_SESSION_IDLE_TIMEOUT_MINUTES` | `480` (8 h) | 5 bis 4320; nicht größer als die absolute Dauer |
| `CRAFTCLOUD_SESSION_ABSOLUTE_TIMEOUT_HOURS` | `168` (7 Tage) | 1 bis 720 |
| `CRAFTCLOUD_TRASH_RETENTION_DAYS` | `30` | 1 bis 3650; so lange bleiben in den Papierkorb verschobene Projekte und Dateien wiederherstellbar (ADR-0014). Es gibt noch keine automatische endgültige Löschung (CC-P02-06): abgelaufene Einträge bleiben in der Datenbank, sind aber nicht mehr wiederherstellbar |
| `CRAFTCLOUD_STORAGE_DIR` | nicht gesetzt (Dateispeicher nicht verfügbar) | absoluter Pfad eines Verzeichnisses für Dateiinhalte. Beim Start geprüft: vorhanden, Verzeichnis, beschreibbar, kein Symlink/Reparse-Point; `objects` und `tmp` darunter werden angelegt und müssen auf demselben Dateisystem liegen (Probe mit Anlegen und Hard-Link; das Dateisystem muss Hard-Links unterstützen, z. B. NTFS, ext4, XFS, nicht FAT/exFAT). Bei einem Fehler beendet sich der Server mit Exit 1 |
| `CRAFTCLOUD_MAX_FILE_SIZE_BYTES` | `5368709120` (5 GiB) | 1048576 bis 1099511627776; größte erlaubte Dateigröße (ADR-0014) |
| `CRAFTCLOUD_UPLOAD_TTL_HOURS` | `24` | 1 bis 168; so lange bleibt eine Upload-Sitzung offen. Danach wird sie beim nächsten Zugriff (oder durch den Bereinigungsjob) geschlossen und gibt ihre Quota-Reservierung frei |
| `CRAFTCLOUD_DATABASE_URL_FILE` | nicht gesetzt (Prototypmodus ohne Datenbank) | absoluter Pfad einer Secret-Datei mit der PostgreSQL-URL |
| `CRAFTCLOUD_DATABASE_MAX_CONNECTIONS` | `10` | 1 bis 100 |
| `CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS` | `5` | 1 bis 60 |

Die Datenbank-URL enthält ein Passwort und steht deshalb **nur** in der Secret-Datei (siehe ADR-0012); `CRAFTCLOUD_DATABASE_URL` selbst wird abgelehnt und verweist auf `CRAFTCLOUD_DATABASE_URL_FILE`. Für entfernte Datenbanken in der URL `sslmode=verify-full` verwenden (bei privater CA zusätzlich `sslrootcert`). Die URL wird nie geloggt.

Schema-Migrationen werden **nie implizit** angewendet: vor dem ersten Start und nach jedem Upgrade zuerst ein Backup erstellen, dann `craftcloud-server migrate` ausführen (wendet ausstehende Migrationen unter einem Advisory Lock an, nur vorwärts). Der normale Start prüft nur, ob das Schema vollständig und unverändert migriert ist, und endet sonst mit Exit-Code 1.

Reserviert und derzeit **abgelehnt** („not supported in this build yet“): `CRAFTCLOUD_DATABASE_URL`, `CRAFTCLOUD_STORAGE_BACKEND`, `CRAFTCLOUD_STORAGE_ROOT`, `CRAFTCLOUD_SESSION_SECRET_FILE`. Sie werden mit späteren Meilensteinen eingeführt (Secret-Dateien: absoluter Pfad, reguläre Datei, höchstens 4 KiB, unter Unix nicht für Gruppe/andere lesbar).

```dotenv
# EXAMPLE — NEVER commit real values
CRAFTCLOUD_PUBLIC_URL=https://cloud.example.org
CRAFTCLOUD_BIND=127.0.0.1:8080
CRAFTCLOUD_TRUSTED_PROXIES=127.0.0.1/32
CRAFTCLOUD_LOG_FORMAT=json
```

### Anmeldung, Sitzungen und CSRF

- **`CRAFTCLOUD_PUBLIC_URL` ist für Browser-Betrieb Pflicht.** Daraus ergeben sich die erlaubte `Origin` (CSRF-Prüfung) und die Reset-Links. Ohne gesetzte URL lehnt der Server jede Anfrage mit `Origin`-Header (also jeden Browser) ab und warnt beim Start; Clients ohne `Origin` (curl, Skripte) funktionieren weiter.
- **Cookie:** `__Host-craftcloud_session` mit `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`, ohne `Domain`. Nur wenn `CRAFTCLOUD_PUBLIC_URL` ein `http://`-Loopback-Host ist (lokale Entwicklung), heißt das Cookie `craftcloud_session` und hat kein `Secure`, weil Browser `__Host-`/`Secure` über http nicht zuverlässig akzeptieren. Produktiv immer `https` über den vertrauenswürdigen Proxy.
- **Laufzeit:** 8 h Inaktivität und 7 Tage absolut (konfigurierbar, siehe Tabelle). Die letzte Aktivität wird höchstens einmal pro Minute gespeichert. Abgelaufene und widerrufene Zeilen werden bei Anmeldungen opportunistisch gelöscht.
- **Gespeichert wird nur der SHA-256-Hash** des Session-Tokens. Das CSRF-Token wird aus dem Session-Token abgeleitet (SHA-256 mit Domänentrenner), nicht gespeichert und über `GET /api/v1/auth/session` erneut abrufbar. Es wird keine IP-Adresse und kein User-Agent gespeichert (ADR-0016).
- **Ratenbegrenzung** (im Speicher, nur für eine Instanz): (1) Anfragen pro Minute und Client-Adresse für Login, Setup und Reset-Abschluss — je Route eine eigene Tabelle mit eigener Obergrenze, IPv4 exakt, IPv6 nach /64 zusammengefasst, 10 pro Minute; (2) für Logins werden nur **fehlgeschlagene** Versuche gezählt: 5 pro 15 Minuten je Konto und Adresse, 50 pro 15 Minuten je Konto über alle Adressen; ein erfolgreicher Login setzt den Zähler seiner Adresse zurück; unbekannte E-Mail-Adressen verhalten sich identisch (kein Hinweis auf Existenz); (3) Passwortänderungen: 5 pro Minute und Konto. Antwort `429 RATE_LIMITED` mit `Retry-After`. Die Fehlversuche aller Logins (bestehende und unbekannte E-Mail-Adressen) liegen in **einer** Tabelle, geschlüsselt nach normalisierter E-Mail-Adresse, mit einer Regel: Zähler mit laufendem Fenster werden nie verdrängt, abgelaufene Einträge werden zuerst entfernt, ist die Tabelle danach noch voll, werden **neue** Adressen mit 429 abgewiesen (fail closed). Die Obergrenze liegt bei 250 000 Adressen: Sie zu füllen bräuchte mehr als 270 fehlschlagende Prüfungen pro Sekunde über 15 Minuten, die Passwortprüfung (Argon2, höchstens vier gleichzeitig) schafft nur etwa 100 pro Sekunde. Speicher: typisch rund 50 MB, im ungünstigsten Fall etwa 400 MB bei voller Tabelle. Ein Login-Versuch wird beim Prüfen atomar **reserviert** (zählt sofort mit); so können gleichzeitige Versuche das Limit nicht alle vor dem ersten Eintrag passieren. Die Reservierung wird zum Fehlversuch, bei Erfolg oder bei Abbruch (Timeout) freigegeben. Restrisiko: Etwa zehn Adressen (oder ein IPv6-/56 mit 256 verschiedenen /64) genügen, um die globale Obergrenze eines Kontos (50 Fehlversuche in 15 Minuten) zu erreichen und den Besitzer bis zu 15 Minuten auszusperren; es gibt kein hartes Sperren, ein Neustart des Servers hebt alle Sperren auf. Die Zähler leben im Prozessspeicher: Ein Neustart des Servers hebt alle Sperren auf (Abhilfe für den Betreiber), mehrere Instanzen hätten getrennte Zähler. Die Client-Adresse stammt aus der vertrauenswürdigen Proxy-Kette (`CRAFTCLOUD_TRUSTED_PROXIES`); ohne konfigurierte Proxys zählt die Peer-Adresse, hinter einem Proxy ohne Eintrag also alle Nutzer gemeinsam. Abgelaufene Datenbankzeilen werden höchstens alle 10 Minuten je Prozess bei Anmeldungen bereinigt.
- **Passwort-Reset ohne SMTP:** Der Operator erzeugt mit `POST /api/v1/admin/users/{user_id}/password-reset` einen Link (gültig 1 Stunde, einmalig, nur als Hash gespeichert, einmalig angezeigt) und übergibt ihn selbst. Das Token steht im URL-Fragment (`/reset-password#token=...`), damit es nicht an Server oder Referer geht. Das **Ausstellen** eines Links beendet sofort alle Sitzungen des Kontos und macht ältere offene Links ungültig (ADR-0013, Reaktion auf eine mögliche Kompromittierung; stellt der Operator den Link für sich selbst aus, ist auch seine eigene Sitzung betroffen). `DELETE /api/v1/admin/users/{user_id}/password-reset` macht offene Links ungültig, ohne Sitzungen zu beenden. Für deaktivierte Konten wird kein Link ausgestellt (409 `ACCOUNT_DISABLED`), und vorhandene Links funktionieren dann nicht mehr. Eine Passwortänderung des Kontos macht offene Links ebenfalls ungültig. Das Einlösen (`POST /api/v1/auth/password-reset`) setzt das Passwort und beendet alle Sitzungen des Kontos. Bis PR-006 darf nur der im Setup festgelegte Instanz-Operator Links erzeugen.
- **Offline-Reset auf dem Server:** `craftcloud-server admin issue-password-reset --email <adresse>` (braucht `CRAFTCLOUD_DATABASE_URL_FILE` und `CRAFTCLOUD_PUBLIC_URL`, prüft das Schema, ändert nichts am Passwort selbst) gibt den Link nur auf stdout aus, nie ins Log, und warnt auf stderr, dass der Link ein Geheimnis ist (auch hier werden Sitzungen des Kontos beendet und ältere Links ungültig; deaktivierte Konten werden abgelehnt). **Interaktiv ausführen** (zum Beispiel `docker exec -it <container> craftcloud-server admin issue-password-reset --email <adresse>`): Wird die Ausgabe von einem Job, CI oder einem Log-Treiber aufgezeichnet, steht der Link dort im Klartext. Kein Passwort über Argumente oder stdin; die Person setzt es über den Link.

Absolute URLs entstehen ausschließlich aus `CRAFTCLOUD_PUBLIC_URL`, nie aus `Host` oder `X-Forwarded-*`. `X-Forwarded-For` wird nur von konfigurierten Proxys ausgewertet.

**Bewusst kein `docker-compose.yml` als scheinbar lauffertiges Ergebnis:** erst erstellen, wenn Server-Image, Migrationspfad, Healthcheck, Secret Ownership und Volume-Permissions real implementiert sind.

## Erstes Setup (Bootstrap des Instanz-Operators)

Der Server erzeugt und druckt nie einen Token (ADR-0013). Der Operator legt ihn selbst als Datei an:

```sh
umask 077
openssl rand -base64 48 > /run/secrets/craftcloud_bootstrap   # Rechte 0600, nur der Serverbenutzer darf lesen
```

1. Datenbank-URL-Datei bereitstellen, `craftcloud-server migrate` ausführen (erzeugt auch die Tabelle `instance_setup`).
2. Server mit `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE=/run/secrets/craftcloud_bootstrap` starten; `GET /api/v1/setup/status` liefert `{"setup_required":true}`.
3. `POST /api/v1/setup/complete` mit `bootstrap_token`, `email`, `display_name`, `password` (12 bis 128 Zeichen), `organization.name` und `organization.slug` aufrufen (nur über TLS am vertrauenswürdigen Proxy, nie über unverschlüsseltes Netz). Erfolg: 201 mit `user_id` und `organization_id`; es wird noch keine Sitzung erzeugt (Login folgt mit PR-005).
4. **Danach die Bootstrap-Secret-Datei löschen und die Variable entfernen.** Der Server warnt bei jedem Start, solange die Datei konfiguriert ist, obwohl das Setup abgeschlossen ist. Ein gültiger Token liefert nach Abschluss nur noch 409, ein falscher immer 403. Ist keine Bootstrap-Datei konfiguriert, antwortet `POST /api/v1/setup/complete` ebenfalls 403 `INVALID_BOOTSTRAP_TOKEN` (anonyme Aufrufer sollen nicht erkennen, ob ein Secret existiert); der eigentliche Grund steht als Warnung im Server-Log. Läuft die Anfrage in den Timeout (503 `TIMEOUT`) oder bricht die Verbindung während des Commits ab, kann das Setup trotzdem erfolgreich gewesen sein: vor einem erneuten Versuch `GET /api/v1/setup/status` prüfen. Das Setup ist vom allgemeinen Timeout nicht ausgenommen.

Das Setup hat keine eigene Ratenbegrenzung: Der Token hat mindestens 32 Zeichen, Raten ist praktisch aussichtslos; ein allgemeiner Limiter folgt mit den Sitzungen (PR-005). Der Passwort-Hash (Argon2id, 19 MiB, 2 Iterationen) wird nur nach gültigem Token berechnet, höchstens vier gleichzeitig.

### Offline-Wiederherstellung (nur per Datenbankzugriff)

Ist der Operator (oder ein anderes Konto) ausgesperrt, genügt in der Regel `craftcloud-server admin issue-password-reset --email <adresse>` auf dem Server (siehe oben). Es gibt bewusst **keinen** HTTP-Endpunkt zum Zurücksetzen ohne Anmeldung ohne Link. Nur wenn auch das Setup selbst neu geöffnet werden muss, kann ein Betreiber mit Datenbankzugriff es nach einem **Backup** wieder öffnen:

1. Server stoppen, Datenbank sichern.
2. `DELETE FROM instance_setup;` ausführen. Das bestehende Operator-Konto und die Organisation bleiben unverändert erhalten (alle Fremdschlüssel sind `ON DELETE RESTRICT`).
3. Bootstrap-Secret-Datei mit einem **neuen** Token anlegen und den Server starten. Das erneute Setup braucht eine andere E-Mail-Adresse und einen anderen Org-Slug als die bestehenden Datensätze; sonst antwortet es mit 422.
4. Danach wie oben die Secret-Datei entfernen. Alte Konten sind anschließend durch den Betreiber zu bereinigen.

Dieser Ablauf ist getestet (`deleting_the_marker_row_reopens_setup_without_touching_existing_data`), aber eine Notfallmaßnahme, kein Produktmerkmal.

### Organisation ohne aktivierten Owner (Break-glass)

Wird das Konto des letzten aktivierten Owners deaktiviert (das ist nie blockiert), meldet `POST /api/v1/admin/users/{id}/disable` die betroffenen Organisationen in `orphaned_organization_ids`, und der Server schreibt eine Warnung und ein Audit-Ereignis `organization.orphaned`. Der Operator hat bewusst keinen API-Zugriff auf Organisationsdaten. Wiederherstellung durch den Hostbetreiber, ohne HTTP-Endpunkt:

```sh
craftcloud-server admin set-org-owner --org <slug> --email <adresse>
```

Das Konto muss existieren und aktiviert sein; es wird als Owner hinzugefügt oder befördert (eine vorhandene Zeile mit Status `invited`/`disabled` wird aktiviert). Der Befehl braucht `CRAFTCLOUD_DATABASE_URL_FILE` (Hostzugriff), prüft das Schema, schreibt ein Audit-Ereignis `org.break_glass_set_owner` mit Kennungen (keine E-Mail-Adresse) und gibt das Ergebnis auf stdout aus. Alternativ kann der ausgesperrte Owner mit `POST /api/v1/admin/users/{id}/enable` wieder aktiviert werden.

## Einladungen ohne SMTP (PR-007)

- Mitglieder ohne Konto werden per Link eingeladen: `POST /api/v1/organizations/{org_id}/invitations` liefert einmalig `invite_url` (`<CRAFTCLOUD_PUBLIC_URL>/invite#token=<token>`). Der Link wird **nicht per E-Mail versendet**, der Einladende gibt ihn selbst weiter. Er ist 72 Stunden gültig, einmalig verwendbar und widerrufbar (`DELETE /api/v1/organizations/{org_id}/invitations/{id}`); gespeichert wird nur der SHA-256 des Tokens. Ohne `CRAFTCLOUD_PUBLIC_URL` antwortet die Route 503 `AUTH_UNAVAILABLE`.
- Wer den Link besitzt, kann die Einladung annehmen (bis eine E-Mail-Verifikation existiert, gilt Linkbesitz als Nachweis der Adresse). Link wie ein Passwort behandeln und nur über einen vertrauenswürdigen Kanal weitergeben. Für bestehende Konten muss der Annehmende als genau dieses Konto angemeldet sein.
- Der Organisationsschalter `workspace_admin_invites_enabled` (Standard `true`, `PATCH /api/v1/organizations/{org_id}`, Owner/Admin) bestimmt, ob Workspace-Admins in ihren Workspace einladen dürfen. Höchstens 1000 offene Einladungen je Organisation; Anlegen ist je Nutzer ratenbegrenzt (Standard 30 je Fenster, Zähler im Speicher, eine Instanz).
- **Account-Squatting (Restrisiko):** Wer einen Einladungslink für eine fremde Adresse erzeugt und selbst annimmt, legt ein Konto für diese Adresse an; die echte Person erhält später `LOGIN_REQUIRED`. Der Operator sieht Herkunft und Einlader mit `GET /api/v1/admin/users?unverified=true` (Organisation, einladender Nutzer, Einladung; `email_verified` ist bis zu einer Verifikation für alle Konten `false`) und im Audit-Log (`account.created_via_invitation`, nur IDs). Abhilfe: das Konto mit `POST /api/v1/admin/users/{user_id}/disable` sperren; anschließend mit der echten Person eine Wiederherstellung vereinbaren (Break-glass: Passwort-Reset-Link des Operators für das Konto, nachdem der Operator die Identität außerhalb des Systems geprüft hat, dann Konto wieder aktivieren). Vor einem Mehrmandanten-Produktivbetrieb ist E-Mail-Verifikation nötig.
- Migration 0006 muss vor dem Start der neuen Version mit `craftcloud-server migrate` eingespielt werden (nur vorwärts).

## Dateispeicher (PR-009)

- Das Speicherverzeichnis gehört dem Serverbenutzer (unix: Modus 0700 empfohlen; der Server legt Dateien mit 0600 an, veröffentlichte Objekte sind schreibgeschützt). Es muss auf einem lokalen Dateisystem liegen, das atomare Hard-Links innerhalb des Verzeichnisses kann (kein Netzwerkdateisystem mit schwacher Semantik); `objects` und `tmp` dürfen nicht auf verschiedene Dateisysteme zeigen (z. B. per Mount).
- Struktur: `objects/<2 Hex>/<2 Hex>/<32 Hex>` für veröffentlichte Objekte, `tmp/<32 Hex>.part` für laufende Schreibvorgänge. Schlüssel sind zufällig; Dateinamen, Organisationen oder Nutzer tauchen in Pfaden nie auf. Identische Inhalte werden nicht dedupliziert.
- Nach einem Absturz können `.part`-Dateien in `tmp` liegen bleiben; sie sind nie sichtbar und werden mit dem Abstimmungsjob (CC-P02-07) entfernt. Sie dürfen bei gestopptem Server gefahrlos gelöscht werden.
- Backups: Datenbank und Speicherverzeichnis gemeinsam sichern (zuerst die Datenbank, danach das Verzeichnis, damit jeder Datenbankeintrag ein Objekt hat; überzählige Objekte sind harmlos). Symlinks im Baum werden vom Server abgelehnt; beim Wiederherstellen keine Links anlegen.
- Plattenplatz: ein volles Gerät führt zu typisierten Fehlern (`Full`, später HTTP 507); der Server bleibt nutzbar. Quoten und Überwachung folgen mit CC-F10/CC-F15.

## Uploads und Quota (PR-010)

- Der Inhalt wird mit `PUT /api/v1/uploads/{id}/content` als roher Body gestreamt. Für diese Route gelten weder das globale Body-Limit (`CRAFTCLOUD_MAX_BODY_BYTES`) noch das Handler-Zeitlimit; stattdessen gilt die deklarierte Größe als hartes Limit, und ein Lesevorgang, der 60 Sekunden stillsteht, bricht den Upload ab. Ein Reverse-Proxy muss für diese Route große Bodys, Streaming ohne Pufferung (`proxy_request_buffering off` bei nginx) und lange Zeitlimits erlauben.
- Ohne `CRAFTCLOUD_STORAGE_DIR` antworten Upload-Routen 503 `STORAGE_UNAVAILABLE`. Ein volles Gerät führt zu 507 `INSUFFICIENT_STORAGE`; die Sitzung wird beendet und die Reservierung freigegeben.
- Quotas: der Operator setzt die Organisations-Quota (`PUT /api/v1/admin/organizations/{id}/quota`), Organisations-Owner/Admins die Workspace-Quota (`PUT /api/v1/workspaces/{id}/quota`, höchstens die Organisations-Quota); `null` bedeutet unbegrenzt (Standard, ADR-0014). Eine niedrigere Quota löscht nie Daten, sie verhindert nur neue Reservierungen. Nutzung: `GET /api/v1/organizations/{id}/usage`.
- Langsame Uploads: nach einer Schonfrist von 5 Minuten wird ein Upload abgebrochen, dessen Durchschnitt unter 16 KiB/s liegt; jeder Upload endet spätestens, wenn die Sitzung abläuft (`CRAFTCLOUD_UPLOAD_TTL_HOURS`). Das Quota-Limit eines Workspaces ist das Minimum aus Workspace- und Organisations-Quota; eine niedrigere Organisations-Quota ändert Workspace-Quotas nicht.
- Je Nutzer höchstens 10 und je Organisation höchstens 200 offene Upload-Sitzungen (409 `UPLOAD_LIMIT`).
- Verwaiste Blobs (Sitzung abgebrochen, abgelaufen oder fehlgeschlagen) bleiben bis zum Bereinigungsjob (CC-P02-06/07) im Speicher und in der Tabelle `blobs` mit Status `orphaned`.
- Migration 0008 muss mit `craftcloud-server migrate` eingespielt werden (nur vorwärts).

## Versionen und Downloads (PR-011)

- Downloads (`GET /api/v1/files/{id}/content`, `.../versions/{vid}/content`) streamen mit begrenztem Speicher; die Antwort wird erst durch den Body-Fortschritt bestimmt, das Handler-Zeitlimit gilt nur bis zum Antwortkopf. Ein Reverse-Proxy sollte Antwortpufferung für diese Routen abschalten (`proxy_buffering off` bei nginx) und lange Lese-Zeitlimits erlauben. Der Server setzt `Cache-Control: no-store`; Proxys dürfen Dateiinhalte nicht cachen.
- Der Download prüft beim vollständigen Streamen den SHA-256 und hält den letzten Block bis zum Abgleich zurück: bei Abweichung wird die Verbindung abgebrochen, der Blob als `corrupt` markiert und später mit 500 `BLOB_CORRUPT` beantwortet (die Zeile in `blobs` und die Datei bleiben als Beweis, die Bytes zählen weiter zur Quota). Wiederherstellung: Objekt aus einem Backup zurückspielen und den Blob-Status per SQL auf `committed` setzen, oder die Datei aus einer anderen Version neu committen. Bereichsabrufe (Range) werden nicht geprüft.
- Alte Versionen werden aufbewahrt; es gibt noch keine Löschung (CC-P02-06). Der Speicherverbrauch wächst mit jeder Version, ein Restore teilt sich den Blob mit der alten Version und braucht keinen zusätzlichen Platz.
- Migration 0009 muss mit `craftcloud-server migrate` eingespielt werden (nur vorwärts).

## Setup-Runbook

1. Host patchen, Docker/Podman gemäß Betreiberregel, DNS und TLS einrichten.
2. Persistente Volumes getrennt mit passenden UIDs/GIDs anlegen; Backups außerhalb Host planen.
3. DB secret und Bootstrap secret über sichere Dateien bereitstellen, keine Secrets im Repo.
4. Mit einem nur lokal erreichbaren Admin-Setup starten, Tests durchführen, dann proxy remote freischalten.
5. Ersten Admin über one-shot setup anlegen und bootstrap key vernichten.
6. TLS cookie, CSRF, Host/proxy, upload limits, readiness, logs und Backup prüfen.
7. Backup/Restore-Test auf separatem Host durchführen, bevor Produktivdaten aufgenommen werden.

## Self-hosted vs managed

Dieselbe versionierte API und Storage-Schicht; Auth und Enterprise-Policies dürfen deployed mode differenzieren, aber Projekte/Dateien sollen exportierbar bleiben. Keine versteckten Vendor-Pflichten.
