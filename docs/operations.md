# Operating a server

This guide is for the person who keeps a Craft Cloud server running: backing it up, restoring it, upgrading it, looking after its storage and noticing when something is wrong. It is a set of procedures that match the command line of `craft-cloud-server` and the scripts in `packaging/docker/`. The long reference material stays in [`server.md`](server.md), and each section below links to the part that explains the details. How the server protects your data is described in [`security.md`](security.md).

Commands on this page are written for a plain install, with the configuration at `/var/lib/craft-cloud/config.toml`. Every command that needs the configuration takes `--config <path>`; the option may come before or after the action. In the Docker deployment the same commands run inside a short-lived container, with the configuration at `/etc/craft-cloud/config.toml`:

```sh
docker compose run --rm --no-deps server verify --config /etc/craft-cloud/config.toml
```

Run that from `packaging/docker`, where `.env` and the Compose files live. The image's entry point is `craft-cloud-server`, so the arguments are the ones shown here.

## What needs backing up

A server consists of four things, and a backup that lacks one of them may be useless.

- **The database** (`meta.db` with SQLite, or a Postgres database). It holds accounts, devices, the tree of every space, history, the trash and the audit log.
- **The blobs** (`blobs/` on a disk store, or your S3 bucket or SFTP directory). They hold the content of every file. Blobs never change once written, which is what makes incremental copies cheap.
- **The two-factor key** (`two-factor.key`). Without it, the two-factor secrets in a database backup cannot be opened and the server refuses to start with them.
- **The configuration** (`config.toml`). It holds secrets, such as database passwords and storage keys.

Keep the last two apart from the backups themselves. If you use the built-in `backup` command, a fifth item, the backup key (`backup.key`), decides whether a backup can be restored at all.

## Backing up to an SFTP server

`craft-cloud-server backup` copies the database and the blobs to an SFTP server. It runs while the server runs, does not stop it, and is safe to repeat: a repeated backup sends only the blobs the backup server does not have yet.

```sh
craft-cloud-server backup --config /var/lib/craft-cloud/config.toml \
  --to sftp://you@backup.example.org/craft-cloud-backup \
  --key-file /etc/craft-cloud/backup-key --host-key 'SHA256:…'
```

The options, all of which exist on the command (`craft-cloud-server backup --help` lists them), are:

- `--to` is the destination, `sftp://user@host[:port]/directory`, or `/~/directory` for a directory in the account's home. It can come from `[backup] to` in the configuration instead.
- `--key-file` is the private key (Ed25519 or ECDSA) to sign in with, or `[backup] key_file`, or the environment variable `CRAFT_CLOUD_SFTP_KEY_FILE`. Signing in is by key only.
- `--host-key` takes a `SHA256:` fingerprint or a public key line, and `--known-hosts` takes a `known_hosts` file. One of them is required; there is no mode that skips host key checking.
- `--keep` is the number of database generations to keep on the backup server (default 7). Blobs are never removed.
- `--backup-key-file` names the backup key file (default `backup.key` beside the configuration, made on the first backup if missing).
- `--work-dir` is where the database dump is made before it is sent (default: the system's temporary directory). It needs room for the whole database.

**What is included.** First a consistent copy of the SQLite database, sealed with AES-256-GCM before it leaves the machine, then every blob the backup server does not have yet, then an inventory of the blobs and a manifest carrying a MAC under the backup key. A backup without its manifest is never offered by `restore`. Each blob is checked against its ID on the way out, and a blob that cannot be read stops the backup with an error that names it.

**What is not included.** The configuration, `two-factor.key` and `backup.key` are deliberately not copied. Keep copies of all three somewhere other than the machine and the backup server. Without `backup.key` a backup cannot be restored and nothing can be done about it, and without `two-factor.key` the two-factor secrets in the database stay sealed. The blobs on the backup server are in the store's own format and are not encrypted, so whoever operates the backup server can read file contents; see the known limits in [`security.md`](security.md#known-limits).

**Postgres is not covered.** The command refuses a Postgres server with a message that says so, and it does not copy the blobs of such a server either. (The text printed by `backup --help` mentions `pg_dump`, but the command does not run it; this is a known wording problem.) For Postgres, run `pg_dump --format=custom` yourself and keep the dump with a copy of the blobs. Section "Backing up to an SFTP server" in [`server.md`](server.md#backing-up-to-an-sftp-server) explains the format, the backup key and the choices for backing up to other kinds of storage.

**Scheduling.** The server has no scheduler. Run `backup` from cron, a systemd timer or the task scheduler, and read the result now and then: a backup nobody reads the result of may have stopped long ago. [`server.md`](server.md#backing-up-to-an-sftp-server) has an example systemd unit and timer.

### The Docker update script's backups

The Docker deployment does not run `backup`. Its script `packaging/docker/update.sh` makes a different kind of backup, once per update. After the new image has been built, it stops the server, dumps Postgres with `pg_dump` into `database.sql` when the Postgres overlay is in use, and copies the whole `data` volume (the database file for SQLite, `blobs/` and `two-factor.key`) into `backups/<UTC time>/` at the repository root. Every such backup is a full copy, blobs included.

After an update that succeeded, the script keeps the newest three backups, the one it just made included, and removes older ones whose names match its own pattern. Change the number with `--keep N` or the environment variable `CC_BACKUP_KEEP`; `0` keeps every backup. A failed update, a skipped backup (`--skip-backup`) and `--keep 0` remove nothing. Three backups of a large volume take three times its size, so check the free space and move any backup you want to keep elsewhere. These backups do not contain `.env` or the rendered configuration; keep your own copy of `.env`.

## Restoring

Restoring is a drill worth doing once before you need it, into a scratch directory.

```sh
craft-cloud-server restore \
  --from sftp://you@backup.example.org/craft-cloud-backup \
  --key-file /etc/craft-cloud/backup-key --host-key 'SHA256:…' \
  --backup-key-file /safe/place/backup.key --dir /var/lib/craft-cloud-restored
```

`--dir` must be empty or missing. `restore` takes the newest complete generation, or the one named with `--generation` (a stamp such as `20261107T031500Z`), and checks the manifest's MAC under the backup key before it believes anything in it. A wrong or missing key restores nothing. It then checks the sealed dump and the blob inventory against their hashes, fetches exactly the listed blobs with each one checked against its ID, and moves the finished result into place only when everything is there, so a failed or killed restore never leaves a database with part of its blobs. After a kill, delete the leftover `<dir>.restore-<id>` directory.

Four limits protect your disk from a broken or hostile backup server and can be raised when your data really is larger: `--max-db-bytes` (default 8 GiB), `--max-total-bytes` (default 1 TiB), `--max-objects` (default 50,000,000) and `--min-free-bytes` (default 1 GiB, checked before and during the restore). `--config` lets `restore` take `[backup] backup_key_file` and the sign-in settings from a configuration. `--allow-unauthenticated` exists only for test data from before backups were authenticated and should not be used otherwise.

A restore always writes `meta.db` and a disk tree `blobs/`, whatever kind of storage the original server used, so the restored server must be configured with `kind = "disk"`, `path = "<dir>/blobs"` and `url = "sqlite:<dir>/meta.db"`. To return to S3 or SFTP, copy the contents of `blobs/` into the bucket or directory first and check the count before you change the configuration back. Details are in [`server.md`](server.md#backing-up-to-an-sftp-server).

**Before you start the restored server**, with the server stopped:

1. Put your `config.toml` and `two-factor.key` next to the restored files.
2. Run `craft-cloud-server verify --config <path>` to check the database and the blobs against each other.
3. Run `craft-cloud-server reset-epoch --config <path> --all`. Every account then starts a new epoch, so each synced computer reconciles with the restored state instead of trusting its own change counter. `reset-epoch` also takes a username for one account and `--space <id>` for one space.
4. Start `serve`.

[`server.md`](server.md#restoring-a-backup) describes this step. For a Docker install restored from an `update.sh` backup, stop the server, put the backup's files back into the `data` volume (and load `database.sql` into Postgres if you use it), run `reset-epoch --all` as above, and start the stack with `docker compose up -d --wait`. The script prints the exact situation if an update failed after the database may have been migrated.

## Upgrading

The server applies its database migrations itself when it opens the database, and refuses a database that a newer server has migrated ("the database was created by a newer Craft Cloud"). Going back to an older version therefore means restoring a backup taken before the upgrade, so always take one first. A configuration with a section that a newer release added is refused by an older server, so remove such sections before going back. Upgrade the server first, then the desktop packages and agents; the Upgrading section in [`server.md`](server.md#upgrading) lists what each release changed and the one known exception for agents older than two-factor sign-in.

**Checking the version.** `craft-cloud-server --version` prints the server's version, and `craft-cloud-agent` has the same flag. In a source checkout the version lives only in `[workspace.package]` of the root `Cargo.toml`, and `cargo xtask version` prints it. In Docker, `update.sh` records the version and the Git revision in `.env`, and the image carries the revision as the label `org.opencontainers.image.revision`.

**Binary install.** The Linux tarball and the Windows setup install the Craft Cloud app and its agent, not the server. The server is built from source (`cargo build --release -p craft-cloud-server`). To upgrade it:

1. Back up as described above.
2. Stop the server.
3. Replace the binary with the new build.
4. Start it. It migrates the database when it opens it.
5. Run `craft-cloud-server verify --config <path>` once. After upgrading from a release that did not yet have the manifest reference index, also run `craft-cloud-server index build --config <path>` once; it only writes that derived index, can be stopped and run again, and can run beside the server.

**Docker.** From the repository root of a Docker checkout, run:

```sh
packaging/docker/update.sh [--yes] [--skip-backup] [--keep N]
```

The script refuses to run without `packaging/docker/.env`, on a detached HEAD or with local changes. It fetches, compares the upstream commit with the revision the running image was built from, and asks before changing anything unless you pass `--yes`. It then fast-forwards to the commit it showed, builds the new image while the old server keeps running (a failed build leaves the server as it was), stops the server, makes the backup described above, and starts the new server with `docker compose up -d --wait`. If a step after the merge fails, its message says what has already changed and how to go back to the old source with `git reset --hard <old commit>`. A migrated database is never rolled back automatically; going back needs the backup. `--skip-backup` is an explicit override and prints a warning. See [`server.md`](server.md#data-upgrading-and-backups).

**Update notices.** The admin overview can show a banner when a newer release exists, from a status file written by an optional checker that is off by default; the server itself never contacts the network. See [`security.md`](security.md#no-telemetry-and-the-update-check).

## Moving between SQLite and Postgres

The code does not support it. There is no command that copies a database from one kind to the other, no export or import, and the backup commands do not convert. The migrations exist for both databases, but a server uses the kind named by `[database] url` for its whole life. The restore described above always produces SQLite. If you need to change database kind, treat it as a new server: create it, have people join it again with invites, and let the agents upload their folders afresh. Plan the choice before you start; see "Postgres" in [`server.md`](server.md#postgres) for the Docker overlay.

## Storage maintenance

All of the following run on the server's host with the same `--config`. Commands that only use the database (`user`, `device`, `audit`, `reset-epoch` and the like) do not open the blob store, so they work while a remote store is unreachable. The commands in this section open it.

**`verify`** checks the database and the blob store against each other: the reference index of every manifest, the manifests and chunks the database names against the objects in the store, and what names what. It changes nothing and exits with an error when it finds a problem. By default it reads every chunk object, which takes as long as reading all your data; `--skip-chunk-reads` leaves that out, and `--json` prints the report as JSON. It opens an existing SQLite database without creating anything.

**`store estimate`** samples stored chunks (read only) and reports what each zstd level would save and how fast it compresses. `--sample` sets how many chunks to sample (default 100) and `--levels` the levels to try (default `1,3,9,19`). The result is remembered so the admin overview can show it.

**`store recompress --level N`** rewrites stored blobs at a higher zstd level (1 to 19) where that saves more. Each blob is checked against its ID, compressed, decompressed and checked again before the object is replaced atomically, and nothing is deleted. It is safe while the server runs. `--dry-run` only says what would change, and `--limit-bytes` stops after reading that many bytes so you can work in slices; stopping it with Ctrl-C is safe and the next run continues. It refuses to run when the configuration says `compression = "none"`. Expect CPU load, and traffic if the store is remote. Set the level for new uploads with `compression_level` in `[storage]`, or from the admin storage page.

**`store measure`** fills in the stored size of blobs written before sizes were tracked. It asks the store for each object's length and reads no content.

**`store clean-temp`** applies to an SFTP store only. It removes temporary files of uploads that were cut off, older than `--older-than-hours` (default 24, at least 1). A finished or running upload is never touched. It walks every shard directory, so on a big store it takes a while.

**`index build`** completes the reference index of manifests stored before the index existed (see Upgrading above). `--batch-rows` sets the number of rows written per transaction.

**Garbage collection.** Deleting stored data that nothing references is not implemented. `craft-cloud-server gc --dry-run --config <path>` inventories the store and prints what a collector would consider (`--json` for a machine-readable report); it changes no data on SQLite or Postgres and exits with an error when it finds a problem. Run without `--dry-run`, the command refuses with "deleting is not implemented yet". Until a collector exists, disk usage only grows, apart from what you reclaim with recompression. Plan capacity accordingly, and consider `refuse_when_full` and `capacity` in `[storage]` so that a full disk produces a clear `507 server_full` instead of a failed upload; see "A full server" in [`server.md`](server.md#a-full-server).

**The trash and retention.** A deleted file or folder stays in the trash, and a signed-in device can bring it back or take it out of the trash. Taking it out only marks it: no version or chunk is freed. The only setting is `[retention] trash_days` (1 to 3650, default 30), which sets the `purge_after_ms` clients are shown and removes nothing yet. Every version of every file is kept. Nothing a person uploaded is deleted except by an explicit action. See [`server.md`](server.md#the-trash).

**Limits on what accounts store.** `stored_bytes_per_user` and per-account overrides (through the admin API or pages) cap what an account may store, and `live_bytes_per_user` and `team_live_bytes` cap live documents. If an app's recent-files list of one account is full, `craft-cloud-server live reset-doc <user> <app> --config <path>` empties it, and every computer sends its own copy up again, so nothing is lost.

## Routine housekeeping

Two tables grow without a background task to trim them, so run these now and then, for example daily from cron or the task scheduler:

```sh
craft-cloud-server audit prune --config /var/lib/craft-cloud/config.toml
craft-cloud-server web-sessions prune --config /var/lib/craft-cloud/config.toml
```

`audit prune` deletes events older than `[audit] retention_days` (default 365), and `web-sessions prune` deletes browser sessions that are over; both take `--dry-run` to only count. The server also removes a few at a time when it writes, but not while nobody uses it. `audit list` shows the newest events (`--user`, `--action`, `--limit` from 1 to 200). `craft-cloud-server two-factor rekey` rotates the two-factor key; follow the order in [`server.md`](server.md#two-factor-sign-in).

## Health checks and monitoring

- **`GET /api/info`** answers 200 while the server runs and says which protocol versions it speaks. Reverse proxies and monitors can poll it through the public address.
- **`craft-cloud-server healthcheck [--addr 127.0.0.1:8080]`** sends that request to the address, prints nothing and exits 0 if the server answers 200, and otherwise exits 1 with one line on standard error. It is for places without a shell or `curl`. The Docker image runs it as its `HEALTHCHECK`, and the Compose file passes the same check to the `server` service.
- **The admin overview and `GET /admin/server`** show how the server is: accounts, devices, stored bytes, the kinds of database and storage, whether the store answered within two seconds (`store_ok`) and whether uploads are being refused because the server is full (`refusing_uploads`). The storage page also shows the capacity and free space.
- **Docker.** `docker compose ps` shows the health state of each service, and `docker compose logs -f server` shows the log.

**Logs.** The server writes its log to standard error, at `info` level unless the environment variable `RUST_LOG` sets another filter (for example `RUST_LOG=warn`). Under systemd, `journalctl -u <your unit>` shows it; in Docker, `docker compose logs server`. Look for warnings about a full store, `ignore_forwarded_for`, SFTP retries and failed starts, and for the message that the server is running at startup. The log does not contain passwords, tokens, file names or file contents; the audit log in the database, not the process log, is the place for who did what. The Docker proxy writes no access log by design. See [`security.md`](security.md#what-is-logged-and-what-never-is).

**Troubleshooting.** The Docker troubleshooting list in [`server.md`](server.md#troubleshooting) covers the common start-up failures. If the server refuses to start after a restore or upgrade, read the first error line: it names the configuration setting, the missing two-factor key or the newer database that is the cause.
