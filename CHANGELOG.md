# Changelog

All notable changes to CraftCloud are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow Semantic Versioning.

## [Unreleased]

## [0.1.0-alpha.2] - 2026-11-03

Second local test release. Adds AC-14 (version thinning), AC-15 (per-file chunk deduplication and delta upload) and AC-16 (zstd chunk compression). Migrations 0024 to 0027 run on upgrade.

### Added

- Version thinning: overwritten versions are kept on a UTC grid (all of the last 24 hours, one per day for 7 days, one per ISO week for 4 weeks) before the existing age limit applies; `versions.keep_all_hours`, `versions.daily_days`, `versions.weekly_weeks` and the optional cap `versions.max_per_file` are instance settings with environment overrides. Versions can be pinned ("keep") in the admin console, via `PATCH /api/v1/files/{file_id}/versions/{version_id}` and with the SDK's `pin_version`; the head, pinned and published versions are never thinned (ADR-0031).
- Per-file chunk deduplication: new versions are stored as content-defined chunks (FastCDC, 1 MiB target) and only chunks new to that file are stored; nothing is shared across files. A second version of a 300 MiB file with a 1 % change used 1.46 % additional storage in the acceptance test. Admin usage reports `physical_bytes` next to the unchanged logical quota. `craftcloud-server admin storage rechunk --file <id>|--all [--dry-run]` converts existing whole versions (ADR-0032).
- Optional delta upload: when `/api/v1/info` reports `chunked_upload`, the SDK sends a chunk manifest and uploads only missing chunks; older clients keep using the full upload.
- Chunk compression: new chunks are stored zstd-compressed when that saves at least 5 %, otherwise raw. Level 3 by default, `storage.compression_level` 0 to 19 (`CRAFTCLOUD_STORAGE_COMPRESSION_LEVEL` takes precedence, `0` turns compression off for new chunks). Downloads, `Range`, `ETag` and `Repr-Digest` are byte-for-byte unchanged (ADR-0033).

### Changed

- Backups use format v4 (chunk mappings and encoding metadata); v1 to v3 backups stay readable.

### Upgrade notes

- Take a backup with the current version before upgrading. Once chunks are stored compressed, an older server cannot read them and cannot restore a v4 backup; rolling back requires the pre-upgrade backup.
- Existing versions stay whole and uncompressed until `admin storage rechunk` converts them. Setting the compression level to `0` does not decompress existing chunks.

## [0.1.0-alpha.1] - 2026-11-02

First local test release (prototype, not production-ready). Includes AC-01 to AC-13 and OPS-01/OPS-02; archive storage remains locked until AC-12c.

### Added

- Initial self-hosted CraftCloud release tooling and operator installation support.
- The Compose stack, `cargo xtask compose-secrets` and the installer provision the two-factor encryption key (`CC_SECRET_TWO_FACTOR_KEY`, served as `CRAFTCLOUD_SECRET_KEY_FILE`); it is kept on reruns and upgrades and is not part of backups.

- One image, `craftcloud:<version>`, holds the server, the admin UI and the FigCraft server; the first argument selects the role (`serve`, `figcraft`, `migrate`, `backup`, `restore`, `healthcheck`, `all-in-one`). The separate `craftcloud-figcraft` image is gone; `CRAFTCLOUD_FIGCRAFT_IMAGE` stays as an alias for one release. `all-in-one` runs both servers under a small supervisor for `docker run` installations.
- The server refuses PostgreSQL versions other than 16, 17 and 18 at connect time. `cargo xtask db-test --postgres 16|17|18` tests against another major version.
- The installer detects a PostgreSQL server on the host (`--db auto|bundled|external`, `--database-url-file`, `--db-admin-url-file`) and explains that MySQL and MariaDB are not supported. For an existing database it writes the SQL for the two roles privately and never prints credentials or the database address.
- The installer can use your own nginx or Apache instead of Caddy (`--proxy auto|caddy|nginx|apache|none`, `--proxy-install`, `--certbot`): templates in `/etc/craftcloud/proxy`, a backup and a configuration test before any change, and a published loopback port instead of public ports. The proxy replaces `X-Forwarded-For`; CraftCloud trusts only the gateway of its own `proxy` network.
- `cargo xtask release` also builds `craftcloud-<version>-image-<arch>.tar.zst` with a `SHA256SUMS` entry (`--no-image`, `--image-arch`); `install.sh --image-archive` and `craftcloud-ctl upgrade --image-archive` load it instead of building on the server.
- With an external database and your own proxy the server has exactly one routable network; all routable Compose subnets are pinned. The administrator URL follows the same TLS and host rules as the database URL and its password never appears in a process argument. Apache site activation, configuration test and reload are one guarded change, and the Apache templates keep URLs out of all logs.

### Changed

- The default image name is `craftcloud:<version>` (an existing installation keeps its saved prefix). The installer needs Docker Compose 2.24.4 or newer.

### Fixed

- A failed first install can be retried, a failed upgrade leaves no partial release directory, and `uninstall` works when systemd units were never installed.
- Release archives are reproducible; `cargo xtask release` finds Python 3.11 or newer (`python3`, `python`, `py -3`).
- `craftcloud-ctl upgrade` refreshes shipped Caddy files and keeps operator edits as `<name>.new`; backup, upgrade and rollback are serialized with `flock`.
