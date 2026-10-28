# Self-hosting-Deployment (Design, keine ausführbare Vorlage)

## Minimaler MVP-Stack

- `craftcloud-server`: Rust service (HTTP framework not yet decided, see ADR-0009 open points), listens only in private network/loopback; runs as non-root.
- `postgres`: single supported primary production database; persistent volume, backups.
- `craftcloud-data`: persistent local blob volume, securely mounted.
- Reverse proxy: Caddy/nginx with valid TLS. `PUBLIC_URL=https://cloud.example.org`, known trusted proxy IPs, explicit headers.
- `craftcloud-admin`: static React build served by proxy/same-origin where feasible; CORS/CSRF simple.

## Proposed env template (illustrative only)

```dotenv
# EXAMPLE — NEVER commit real values
CRAFTCLOUD_PUBLIC_URL=https://cloud.example.org
CRAFTCLOUD_BIND=127.0.0.1:8080
CRAFTCLOUD_DATABASE_URL=postgresql://craftcloud:<secret>@db:5432/craftcloud
CRAFTCLOUD_STORAGE_BACKEND=filesystem
CRAFTCLOUD_STORAGE_ROOT=/data/blobs
CRAFTCLOUD_TRUSTED_PROXIES=127.0.0.1/32
CRAFTCLOUD_BOOTSTRAP_SECRET_FILE=/run/secrets/bootstrap_token
CRAFTCLOUD_SESSION_SECRET_FILE=/run/secrets/session_secret
```

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
