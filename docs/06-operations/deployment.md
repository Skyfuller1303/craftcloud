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

Es gibt bewusst **keinen** HTTP-Endpunkt zum Zurücksetzen. Ist der Operator ausgesperrt, bevor die CLI-Wiederherstellung (PR-005, ADR-0013 Punkt 6) existiert, kann ein Betreiber mit Datenbankzugriff das Setup nach einem **Backup** wieder öffnen:

1. Server stoppen, Datenbank sichern.
2. `DELETE FROM instance_setup;` ausführen. Das bestehende Operator-Konto und die Organisation bleiben unverändert erhalten (alle Fremdschlüssel sind `ON DELETE RESTRICT`).
3. Bootstrap-Secret-Datei mit einem **neuen** Token anlegen und den Server starten. Das erneute Setup braucht eine andere E-Mail-Adresse und einen anderen Org-Slug als die bestehenden Datensätze; sonst antwortet es mit 422.
4. Danach wie oben die Secret-Datei entfernen. Alte Konten sind anschließend durch den Betreiber zu bereinigen.

Dieser Ablauf ist getestet (`deleting_the_marker_row_reopens_setup_without_touching_existing_data`), aber eine Notfallmaßnahme, kein Produktmerkmal.

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
