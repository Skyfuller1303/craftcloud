# Security model

This document describes what Craft Cloud protects and how. Every statement was checked against the code of this repository; the files that do the work are named so that you can read them yourself. It is a description of the current behaviour, not a promise about future releases. The last section lists what the code does not do, so that you can decide whether it is enough for your server. For operating instructions see [`server.md`](server.md) and [`operations.md`](operations.md); for how the parts fit together see [`architecture.md`](architecture.md).

Craft Cloud is self-hosted. There is no hosted service, no account with the authors and no telemetry. The people who can read your files are the people who can reach your server's disk, its database and its backups, plus the accounts you give access to.

## Passwords and tokens

Passwords are hashed with Argon2id (`apps/craft-cloud-server/src/auth.rs`, using the library's default parameters). A password is 8 to 1024 bytes long; the upper bound keeps an oversized input from costing hashing time. A sign-in for a name that does not exist, or for an account that has no password yet because it was made by an invite, still spends a full Argon2id check against a dummy hash, so the time of the answer does not tell a visitor which names exist. The server runs at most four password checks at once; a sign-in beyond that is answered `429 rate_limited` at once rather than queued (`apps/craft-cloud-server/src/state.rs`).

A device token is 32 random bytes from the system's random source, sent as 43 characters of URL-safe base64. The server stores only the BLAKE3 hash of the token text and finds the device by it, so a copy of the database does not contain a usable token. Tokens are sent only in the `Authorization: Bearer` header and are never logged. Each computer or app has a token of its own, and each can be revoked on its own: an administrator with `device revoke` or `device revoke-all` on the command line, an account holder from the account's devices, or an administrator through the admin API. Changing or resetting a password signs out the account's other devices by default, and deletes the account's open invite and add-a-computer codes, so that a stolen token or code does not outlive the old password. A token does not expire by itself; revocation is the way to end it.

The role of a signed-in account is read from the database on every request and never cached, so a demoted administrator loses access with the next request (`require_admin` in the same file). Every administrator change asks for a fresh confirmation in the request, the acting administrator's own password and, when they use two-factor sign-in, a code, because a token that never expires should not be enough on its own.

Sign-ins are rate limited per account and per address (10 per account and 30 per address in five minutes, constants in `apps/craft-cloud-server/src/api.rs`), and the address counted is the one a configured trusted proxy reports (`apps/craft-cloud-server/src/ratelimit.rs`). The server refuses to run behind a proxy without naming it in `trusted_proxies`, unless you explicitly set `ignore_forwarded_for`; see [`server.md`](server.md#running-the-server).

## Two-factor sign-in

An account can add a TOTP second factor (RFC 6238: six digits, 30-second steps, `apps/craft-cloud-server/src/totp.rs`) and ten recovery codes. With it on, a correct password does not sign anyone in; joining a computer with an invite or an administrator's reset code asks for the code too. The authenticator secret is stored only encrypted with AES-256-GCM, bound to its account, under a key held in a separate file named by `[two_factor] key_file` (`apps/craft-cloud-server/src/secretbox.rs`). Recovery codes are stored as hashes. A code is accepted once: the server remembers the last step used for an account, so a code that was seen cannot be replayed within its window.

Repeated wrong codes lock the second factor for a while, with a delay that doubles up to 24 hours and resets on a right code; the lock is never permanent, so someone who only knows a password cannot lock an account out for good. The server fails closed: if the database holds two-factor secrets and the key is missing or cannot open them all, `serve` refuses to start. The server never creates the key itself; `init` and the Docker setup do. Keep a copy of the key apart from your database backups, because a backup without it cannot be used for two-factor secrets. An administrator can take a lost second factor away with `user reset-2fa` on the command line or through the admin API, which also signs every device of that account out. The details are in [`server.md`](server.md#two-factor-sign-in).

A computer that is added with an add-a-computer code (a pairing) is not asked for a second factor, because only a computer that is already signed in can make such a code. Turning two-factor sign-in on signs the account's other devices out.

## The admin pages in the browser

The server-rendered admin pages under `/web` are off by default (`[web_admin] enabled`). The configuration is refused at start when they are enabled without `behind_proxy = true` and an `https://` `public_url` without a path (`apps/craft-cloud-server/src/config.rs`). Only administrators can sign in, and every refusal at the password step reads the same whether the name exists or not.

The code in `apps/craft-cloud-server/src/web/mod.rs` implements these protections:

- The session is a random 32-byte value in the cookie `__Host-craft_cloud_session`, marked `Secure`, `HttpOnly` and `SameSite=Strict`, with no `Domain`. The database keeps only its BLAKE3 hash. A session ends at sign-out, after `idle_minutes` of disuse, after `absolute_hours`, and whenever the account changes (password, role, disabling, two-factor changes).
- Every form carries a CSRF token derived from the session token under a key-derivation context of its own, compared in constant time. The sign-in form has no token, because there is no session yet; the origin check covers it.
- Every request that is not `GET` or `HEAD` must carry an `Origin` equal to the origin of `public_url` (or, without `Origin`, a `Referer` from that origin). Anything else, including `Origin: null`, is answered `403`. No `GET` changes anything.
- Answers carry a content security policy that allows no script and no other host, `frame-ancestors 'none'`, `nosniff`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. The pages contain no script at all.
- The pages know only the cookie and the JSON admin routes know only the bearer token; a request with both is refused, so a client cannot be confused about which credential acted.
- Every change asks again for the administrator's password, and a code when two-factor sign-in is on.

Strict Transport Security is not sent by the server; the Docker deployment's Caddy configuration sends it for real certificates.

## The agent's local API

The sync agent offers a local HTTP API to the Craft apps and to its own command line (`apps/craft-cloud-agent/src/local_api.rs`). It binds only to `127.0.0.1` on a random port, never to another address. At each start the agent makes a random token and writes the port, the token and its process ID to `local-api.json` in its data folder, owner-only on Unix and removed when the agent stops. Every request is checked before anything else:

- The `Host` header must be exactly `127.0.0.1:<port>`, which stops a web page that rebinds a name to the loopback address.
- Any request that carries an `Origin` header is refused, because browsers send one with every cross-origin request and every WebSocket. A web page therefore cannot use the API even if it finds the port.
- The token must be sent as `Authorization: Bearer`, never in the query string, and is compared in constant time. It is never logged.

An app that holds the token acts as the signed-in person on this computer. It can read the status, sign in or out, join, pause and resume, quit the agent, manage this account's devices and two-factor settings with the person's passwords and codes, read and write live documents and blobs through the agent, and use the server's admin calls if the signed-in account is an administrator. It cannot ask the agent for the device token: the API has no route that returns it, and the debug output of every request and response type that holds a secret hides it. The server still decides every request: the agent checks no role and keeps no confirmation, so an administrator call still needs the administrator's password in the request, and a team space call is still checked against the account's role. Any program that can read the agent's data folder as the same operating system user can read the token file, so the local API protects against web pages and other users of the machine, not against other programs run by the same user.

The agent keeps one instance per data folder with operating system file locks, and refuses a lock file that is a link.

## Where the agent stores the account credential

The agent's `account.json` holds the server address, the user, the device and the device token. It is written atomically (temporary file, flush, rename). There is no operating system keychain integration; the code says so explicitly.

- **Linux:** `~/.local/share/craft-cloud-agent`. The data folder is created with mode 0700 and the files in it with mode 0600.
- **macOS:** `~/Library/Application Support/craft-cloud-agent`, with the same modes.
- **Windows:** `%LOCALAPPDATA%\craft-cloud-agent`. Windows has no mode bits here; the folder inherits the access rules of the user's profile, which only that user and administrators can read. A `--data-dir` outside `%LOCALAPPDATA%` is refused on Windows, because it would get another folder's access rules, which might let other users read the token.

The agent talks to the server over HTTPS only, except to a loopback address, ignores `HTTP_PROXY` and similar variables so that the token is never handed to a proxy, and caps everything it reads from the network before buffering it (`apps/craft-cloud-agent/src/http.rs`).

## Team spaces

A team space is a shared tree with three roles: `viewer` (reads, history and trash), `editor` (also writes) and `owner` (also manages members, removes from the trash, renames and archives). The role is checked in three places.

- **Per route.** Every space-scoped route declares the least role it needs (`apps/craft-cloud-server/src/api.rs`, the table of `space_routes`), and the layer in `apps/craft-cloud-server/src/space.rs` reads the account's role from the database on every request and refuses before the handler runs. A request names its space with the `craft-space` header; without it the space is the account's personal one. An account with no access gets `404 not_found`, the same answer as for a space that does not exist, so membership cannot be probed. A member whose role is too low gets `403 read_only`.
- **Again inside the write.** A write in a team space re-reads the role inside its database transaction after locking the space's row (`crates/meta/src/spaces.rs`), so a role lowered between the check and the commit is respected.
- **Per live frame.** The live connection to a team space is served on live protocol 3 only. Each frame needs a role: `create`, `update` and `snapshot` need an editor, `subscribe` needs a viewer; a viewer's write is refused with `read_only` and changes nothing (`apps/craft-cloud-server/src/live.rs`). A connection re-reads the account's access at every ping, and a removal, a role change, an archived space or a disabled account ends it with `access_changed`. Presence is stamped by the server, not taken from the client.

An administrator has no access to a team space's files merely by being an administrator. Access comes from membership only (`space_access` in `crates/meta/src/spaces.rs`). An administrator can create, list, archive and name an owner for a space, set its limit, and add themselves as a member, which is audited and visible to the other members. The server's command line can also add members, because whoever runs it has the files on disk anyway; see the known limits below.

## Path confinement and names

File and folder names on the wire are single path segments. `Name` in `crates/api/src/name.rs` normalises to NFC and rejects empty names, names over 255 bytes, `.` and `..`, control characters (which include NUL), the characters Windows forbids (`/ \ < > : " | ? *`), a trailing dot or space, Windows device names such as `CON` or `COM1` with or without an extension, and the agent's own `.craft-cloud` folder name. Two names that differ only by case or normalisation cannot share a directory. A server-side path never comes from a client: blobs are stored under their hash, not under a name. On the client the agent's disk layer works with symbolic links explicitly (`apps/craft-cloud-agent/src/disk.rs`): only the synced root itself may be a link, chosen by the user.
## Request size limits and blob verification

Everything from the network is treated as hostile, including requests from signed-in accounts. The server sets a body limit per route (`apps/craft-cloud-server/src/api.rs`, limits in `crates/api/src/limits.rs`): chunks at 4 MiB, manifests at about 36 MiB, commit batches at 1 MiB and 1000 operations, sign-in and small JSON bodies at 1 MiB, account and two-factor bodies at 4 KiB, live updates at 1 MiB and live presence at 4 KiB. A body that is over the limit is answered `413`; one that arrives too slowly is answered `408` by the server itself (60 seconds plus 30 seconds per MiB the route allows), so a stalled client only holds its own slot. Each account may send at most eight chunk or manifest bodies at once and have four manifests checked at once. The agent applies matching caps to what it reads from the server.

Every blob is checked against its hash. A blob's ID is the BLAKE3 hash of its plain bytes, and the store (`crates/store/src/lib.rs`) refuses a write whose hash does not match, whatever backend it goes to, and checks the hash again when it reads, returning an error instead of serving a damaged blob. Compression does not change this: the hash is taken over the plain bytes, and a recompression run verifies before and after.

## The audit log

The server records security events in its own database: sign-ins and failed sign-ins, devices signed out, password and two-factor changes, administrator changes to accounts, roles, quotas, invites and devices, team space changes, and what the command line does to an account. A row has a time, the IDs of the account and device that acted and of the target, an event name from a closed list, success or failure, and a few typed values. It has no IP addresses, no file or device names, no names of accounts that do not exist, and no passwords, codes or tokens (`crates/meta/migrations/sqlite/0009_audit_events.sql` documents the columns). Failed sign-ins are one row per account and minute with a count. Events are kept 365 days by default (`[audit] retention_days`, 1 to 3650), and `audit prune` deletes older ones. Because it holds account IDs and times, treat it as personal data about your users and tell them what you keep it for. See [`server.md`](server.md#the-security-audit-log).

The log is append-only in the server's code, not enforced by the database. Anyone who can write to the database can change it, so keep the database file and the Postgres role private.

## What is logged, and what never is

The server writes its log to standard error with `tracing`, at `info` unless `RUST_LOG` says otherwise (`apps/craft-cloud-server/src/main.rs`). It logs start and stop, the listen address, warnings such as a full store or `ignore_forwarded_for`, and counts for command line actions such as how many devices a reset signed out. Passwords, tokens, recovery codes, two-factor challenges, keys, cookies, CSRF tokens, file contents and file names are not logged at `info` or above; invite codes are printed only to the administrator's terminal and never logged. The user name of an invite is kept to `debug` level only.

The Docker deployment's Caddy proxy writes no access log, because URLs contain invite codes (`/join/<code>`). Its error log removes the request URI, the headers and the fields that name the client. A few messages from Go's HTTP library, such as a failed TLS handshake, can still contain a client address in their text. See [`server.md`](server.md#tls-modes).

## No telemetry and the update check

Neither the server nor the agent sends usage data, and the server makes no outbound request to any service of the authors. The server never contacts the network for updates: it only reads a small status file if one is configured (`apps/craft-cloud-server/src/updates.rs` reads a regular file of at most 4096 bytes and refuses links).

The update check is a separate optional script, `packaging/docker/update-check.sh`, run by the Docker `update-check` service. It is off by default; `install.sh` asks and the answer defaults to no. When you turn it on it asks the GitHub API for the latest release (or the latest commit of a branch) of the configured repository every 12 hours, after a random start delay of up to 30 minutes. The request is a plain `GET` with the generic User-Agent `Craft-Cloud/<version>` and an `Accept` header. It sends no account name, domain, address or installation identifier, though GitHub sees the address of your host as it would for any request. The result is written to a status file that the server reads to show a banner on the admin overview; nothing updates the server automatically.

## The Docker deployment

The deployment in `packaging/docker/` is hardened as follows (see [`server.md`](server.md#docker) for the full description).

- **Secrets.** `.env` is written with mode 0600 and holds random database passwords. The one-shot `secrets-init` service has no network; it renders `config.toml` into a volume (mode 0400, owner uid 65532) and makes the two-factor key once (mode 0600), never replacing an existing one. No secret appears in an environment variable or a Compose file, so `docker inspect` shows none.
- **Containers.** Each service has a read-only root filesystem, no capabilities, `no-new-privileges`, memory and process limits and a `noexec` tmpfs for `/tmp`, with the exceptions explained in the Compose files. The server runs as uid 65532 on a distroless image, and base images are pinned by digest.
- **Networks.** Only Caddy publishes ports (80 and 443). The server is reachable from Caddy on an internal network, and Postgres, when used, is on an internal backend network that Caddy is not attached to. The server has no outbound network access in the default setup.
- **TLS.** Caddy terminates TLS (`packaging/docker/caddy/Caddyfile`). The modes are a public certificate by ACME, Caddy's internal CA for tests, and your own certificate. HSTS is sent for the first and last. Caddy has its management API turned off, sets a 10-second request header timeout, redirects plain HTTP to the public URL and passes only requests that the server's `/api/info` health check accepts. The server's own listen address speaks plain HTTP and must never be published.
- **Postgres.** The connection between the server and Postgres is not encrypted; it stays on the internal network.

## Known limits

These are things the code does not do. They are listed so that you can judge them against your own needs.

- **No end-to-end encryption.** The server can read your files. Blobs are stored in the store's own format, optionally compressed with zstd, but never encrypted (the format byte reserves encryption, and values other than plain and compressed are refused). The database is not encrypted by the server either. Protect the disk with your operating system's disk encryption if you need data at rest protected.
- **Backups.** `backup` seals the database dump and authenticates the manifest with a key you must keep apart, but the blobs it copies to the backup server are not encrypted, so the backup server's operator can read file contents. See [`operations.md`](operations.md).
- **No keychain for the agent.** The device token is a file protected by file permissions (Unix) or by the profile's access rules (Windows), not by an operating system keychain. Anyone who can read it as that user can act as that device until it is revoked.
- **The local API token is a file too.** Any program run by the same operating system user can read it.
- **Device tokens do not expire.** Revoke lost devices; there is no automatic expiry.
- **Command line has full power.** `craft-cloud-server` on the host enforces neither the confirmation password nor the last-administrator rule, and can add anyone to a team space. Whoever can run it, or read the database and the blob directory, has everything.
- **The audit log can be changed by whoever can write to the database.** It is not tamper-evident.
- **Plain HTTP inside.** The server and Postgres do not speak TLS; TLS is the proxy's job and a Postgres over TLS is not covered.
