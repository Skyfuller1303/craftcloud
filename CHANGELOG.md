# Changelog

All notable changes to CraftCloud are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow Semantic Versioning.

## [0.1.0] - Unreleased

### Added

- Initial self-hosted CraftCloud release tooling and operator installation support.
- The Compose stack, `cargo xtask compose-secrets` and the installer provision the two-factor encryption key (`CC_SECRET_TWO_FACTOR_KEY`, served as `CRAFTCLOUD_SECRET_KEY_FILE`); it is kept on reruns and upgrades and is not part of backups.

### Fixed

- A failed first install can be retried, a failed upgrade leaves no partial release directory, and `uninstall` works when systemd units were never installed.
- Release archives are reproducible; `cargo xtask release` finds Python 3.11 or newer (`python3`, `python`, `py -3`).
- `craftcloud-ctl upgrade` refreshes shipped Caddy files and keeps operator edits as `<name>.new`; backup, upgrade and rollback are serialized with `flock`.
