# Changelog

All notable changes to CraftCloud are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow Semantic Versioning.

## [0.1.0] - Unreleased

### Added

- Initial self-hosted CraftCloud release tooling and operator installation support.
- The Compose stack, `cargo xtask compose-secrets` and the installer provision the two-factor encryption key (`CC_SECRET_TWO_FACTOR_KEY`, served as `CRAFTCLOUD_SECRET_KEY_FILE`); it is kept on reruns and upgrades and is not part of backups.

- One image, `craftcloud:<version>`, holds the server, the admin UI and the FigCraft server; the first argument selects the role (`serve`, `figcraft`, `migrate`, `backup`, `restore`, `healthcheck`, `all-in-one`). The separate `craftcloud-figcraft` image is gone; `CRAFTCLOUD_FIGCRAFT_IMAGE` stays as an alias for one release. `all-in-one` runs both servers under a small supervisor for `docker run` installations.
- The server refuses PostgreSQL versions other than 16, 17 and 18 at connect time. `cargo xtask db-test --postgres 16|17|18` tests against another major version.
- The installer detects a PostgreSQL server on the host (`--db auto|bundled|external`, `--database-url-file`, `--db-admin-url-file`) and explains that MySQL and MariaDB are not supported. For an existing database it writes the SQL for the two roles privately and never prints credentials or the database address.
- The installer can use your own nginx or Apache instead of Caddy (`--proxy auto|caddy|nginx|apache|none`, `--proxy-install`, `--certbot`): templates in `/etc/craftcloud/proxy`, a backup and a configuration test before any change, and a published loopback port instead of public ports. The proxy replaces `X-Forwarded-For`; CraftCloud trusts only the gateway of its own `proxy` network.
- `cargo xtask release` also builds `craftcloud-<version>-image-<arch>.tar.zst` with a `SHA256SUMS` entry (`--no-image`, `--image-arch`); `install.sh --image-archive` and `craftcloud-ctl upgrade --image-archive` load it instead of building on the server.

### Changed

- The default image name is `craftcloud:<version>` (an existing installation keeps its saved prefix). The installer needs Docker Compose 2.24.4 or newer.

### Fixed

- A failed first install can be retried, a failed upgrade leaves no partial release directory, and `uninstall` works when systemd units were never installed.
- Release archives are reproducible; `cargo xtask release` finds Python 3.11 or newer (`python3`, `python`, `py -3`).
- `craftcloud-ctl upgrade` refreshes shipped Caddy files and keeps operator edits as `<name>.new`; backup, upgrade and rollback are serialized with `flock`.
