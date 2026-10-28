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

Reserviert und derzeit **abgelehnt** („not supported in this build yet“): `CRAFTCLOUD_DATABASE_URL`, `CRAFTCLOUD_STORAGE_BACKEND`, `CRAFTCLOUD_STORAGE_ROOT`, `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE`, `CRAFTCLOUD_SESSION_SECRET_FILE`. Sie werden mit späteren Meilensteinen eingeführt (Secret-Dateien: absoluter Pfad, reguläre Datei, höchstens 4 KiB, unter Unix nicht für Gruppe/andere lesbar).

```dotenv
# EXAMPLE — NEVER commit real values
CRAFTCLOUD_PUBLIC_URL=https://cloud.example.org
CRAFTCLOUD_BIND=127.0.0.1:8080
CRAFTCLOUD_TRUSTED_PROXIES=127.0.0.1/32
CRAFTCLOUD_LOG_FORMAT=json
```

Absolute URLs entstehen ausschließlich aus `CRAFTCLOUD_PUBLIC_URL`, nie aus `Host` oder `X-Forwarded-*`. `X-Forwarded-For` wird nur von konfigurierten Proxys ausgewertet.

**Bewusst kein `docker-compose.yml` als scheinbar lauffertiges Ergebnis:** erst erstellen, wenn Server-Image, Migrationspfad, Healthcheck, Secret Ownership und Volume-Permissions real implementiert sind.

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
