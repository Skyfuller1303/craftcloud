<!-- craft-house-rules:start core source=ce8827d4b89aa16d0236a825059b4557d29d7058 sha256=540e6cea75d12d0710c3dca89658328e81e508ecef102446ca5bb95b7c0b92bb -->
## House rules

These apply in every Craft repository. A later rule may be stricter, not weaker. The source is `standards/house-rules-core.md` in `storytold/craft-contrib`. Propose changes there, not in a copy.

1. **A person is accountable.** Whoever runs an agent answers for what it opens and for its review comments.
2. **Check, then claim.** Before writing code, read the issue comments, search pull requests for `#N` and the keywords, and check recent commits on `main`. Comment on the issue first. Release a claim you will not finish.
3. **Verify, don't guess.** Open a definition before using it. Confirm the cause in the code, even when someone names a location.
4. **Keep fixes narrow and tested.** Fix what the issue describes, where the bug is. Add a test that fails without the fix.
5. **Review the diff before opening a pull request.** Identifiers exist as used, the test fails without the fix, nothing unrelated changed, and the description is true.
6. **Look for a better pull request.** If another fixes the same thing, close yours when it is better or equal, and say why.
7. **Report only what ran.** Say what you tested and what you could not run. Without a local build or test, say so and rely on CI before marking ready.
8. **`main` changes through a pull request.** Do not push to it. Branch protection enforces this. There is no exception.
9. **Stop and ask only about** licensing outside the allowlist, publishing, releases, a new remote, secrets, spending money, destructive git, and anything else hard to undo. Otherwise keep working. `plan/` is a gitignored notebook, not rules. Without it, do not ask for it and do not stop.
10. **Keep secrets and personal details out** of the repository, logs and requests.
11. **CI stays shared.** Workflows call `storytold/craft-common-actions`. Do not change CI in a feature or fix pull request. If CI cannot cover a change, open an issue there.
12. **Guards say why.** A check that looks removable says what it prevents. Read that before changing it.
13. **Instructions must be readable by everyone.** No notes about one machine, and no links to files a reader cannot open. `craftrules` is retired. Do not link to it.
14. **Keep pull requests moving.** Concurrent pull requests are fine; unanswered actionable feedback is not. Respond in 7 days, a reminder at 14, closure eligible at 30, from when an actionable request waits on the author. Suggestions and unconfirmed bot findings do not start the clock. A fix, explanation or agreed plan resets it; unrelated activity does not. Waiting on a reviewer or maintainer pauses it and does not count against the author. A maintainer decides closure; it is never automatic. Reuse search results, do not poll, and stop at a rate limit.
15. **No agent attribution.** A pull request does not say an agent wrote it. A repository rule may not require that.
<!-- craft-house-rules:end -->
<!-- craft-house-rules:start rust source=ce8827d4b89aa16d0236a825059b4557d29d7058 sha256=642f46b4ac38cc9e4d10d1ccde19d68cb6bc03d20e4c6236a7535f3047566262 -->
## Rust Craft repositories

These apply to native Craft apps and to shared Rust libraries. Select them by repository type. A repository that merely contains some Rust does not take this block. Rules later in a repository's file may be stricter, but not weaker. The source is `standards/house-rules-rust.md` in [storytold/craft-contrib](https://github.com/storytold/craft-contrib).

1. **Never crash.** Non-test code does not panic and has no `unsafe`, except where the repository file names an exception. Return errors, treat input as hostile, and bound recursion and allocations. New production crate roots copy the `deny` attribute from an existing crate: `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`, `clippy::unimplemented`, `clippy::todo`, `clippy::unreachable`. Every crash fix comes with a test that panicked before the fix.
2. **Clean room.** Learn from observed behaviour and public specifications. Do not copy another product's code, assets, strings or artwork. Check a dependency's licence before adding it.
3. **Assets and fonts.** Every asset is original or suitably licensed, and is recorded in `ATTRIBUTION.md` in the same change. Font files live in [storytold/craft-fonts](https://github.com/storytold/craft-fonts), not in the app repository.
4. **Rust only for native Craft apps.** No handwritten JavaScript or TypeScript, and no Tauri, Electron or webview shell, in those apps. Shared tooling follows its documented technical scope and maintainer-approved exceptions. Craft PR Review's existing Tauri and TypeScript UI is one such exception. It does not relax this rule for native apps. Preserve any existing web build.
5. **Time goes through `chrono`.** Do not hand-roll calendar conversion. For dates beyond what `chrono` can represent, follow [standards/time.md](https://github.com/storytold/craft-contrib/blob/main/standards/time.md).
6. **Contributor credits.** Add or edit only your own person's entry in `contributors/people.toml`, and only after asking them. Never guess a name or hand-edit `contributors.json`.

## Licence allowlist

Dependencies and assets use SPDX identifiers. An expression is allowed when every identifier is in this list. A repository may name a shorter list. It may not add an identifier here by editing its own copy of these rules.

- `ISC`
- `MIT`
- `BSD-2-Clause`
- `BSD-3-Clause`
- `Apache-2.0`
- `Unicode-3.0`
- `OFL-1.1`
- `Ubuntu-font-1.0`
- `Bitstream-Vera`
- `CC0-1.0`
- `CC-BY-4.0`
- `CC-BY-SA-4.0`
- `LicenseRef-Public-Domain`

`LicenseRef-Storyteller-Trademark` is not in this list. It covers Storyteller brand marks only, where a repository already says so.
<!-- craft-house-rules:end -->

# AGENTS.md: guide for AI agents and contributors

Craft Cloud is a **self-hosted** sync and storage server for the **Craft App Suite** (PhotoCraft,
LightCraft, FilmCraft, DesignCraft, VectorCraft, PdfCraft, EffectCraft, WordCraft, DeckCraft,
GridCraft, CADCraft, SoundCraft). People run it on their own machine, NAS or VPS; there is no
hosted service, no account with us, and nothing phones home. Your files, settings and asset
libraries follow you across machines, and (later) can be shared with a team.

It is written in **Rust only**. Siblings with the same conventions: `../craft-launcher`,
`../photocraft`, `../vectorcraft`, `../filmcraft` and the other Craft apps. The standards shared across
the Craft apps are the house rules at the top of this file, copied from `storytold/craft-contrib`
(`standards/house-rules-core.md` and `standards/house-rules-rust.md`). Do not edit the two blocks: propose
changes in that repository. Contribute reusable learnings there, never code: repos don't share code.

This repository is a maintained fork of `itrycodinganditsbad/craft-cloud` and is independent of the
storytold organisation. It takes the house rules as its quality bar. Where it cannot follow one yet (its
own CI, the fonts it bundles, the wider licence list in section 2), [docs/fork.md](docs/fork.md) names the
rule, the difference and the reason. Rules in this file that are stricter than a house rule stay in force.

## Naming

The product name is **Craft Cloud** in user-facing text. Machine names are lowercase:
`craft-cloud-*` crates and binaries. This repository is not an ArtCraft Team project: never use the
ArtCraft name, logo or marks in the product, its icon, its binaries or its docs, beyond saying in
plain text that it is built for the Craft App Suite. Every Craft app's `docs/brand/LICENSE-brand.txt`
explains why.

## 1. Workspace map

The design lives in `docs/design/sync.md`; the live protocols (the server's live endpoint, the agent's local API) in
`docs/design/live.md`.

```text
crates/
  api       craft-cloud-api       what apps need of the protocol, with no postcard: IDs, names, limits, the agent's local API (`local.rs`, `/v1/blobs` too), error codes, the account's devices and the admin API's bodies (`api.rs`)
  proto     craft-cloud-proto     wire types: hashes, manifests, commit ops, API bodies, join codes and join strings (`join.rs`), the live protocol (`live.rs`); re-exports `craft-cloud-api` under the same paths
  chunker   craft-cloud-chunker   FastCDC `fastcdc-v1` + BLAKE3, frozen by golden vectors
  store     craft-cloud-store     blob store: disk, S3, SFTP (feature `sftp`, `sftp.rs`), memory; checks every blob in and out
  meta      craft-cloud-meta      metadata on SQLite or Postgres; the batch commit; the change feed; invites and pairings (`join.rs`); live documents' logs and snapshots (`live.rs`)
  sync      craft-cloud-sync      the sync engine over `Fs`/`Transport` traits; `MemFs`; scenario tests and the simulation
  kit       craft-cloud-kit       the app kit (`docs/kit.md`): an app's model live through the agent; the `LiveDocument` trait and `Session` (`session.rs`: open, the `local_change` hook, `poll`, undo through Loro's undo manager, reconnect and the 64 MiB queue, presence, `people()`, viewers, `blobs()` bound to the document's space, saves), the change writer that cuts commits under 512 KiB (`writer.rs`), the mappings (`tree.rs`, `text.rs`, `table.rs`, `timeline.rs`, `opaque.rs`, `list.rs`), values, chunk references and blob stores (`value.rs`)
  egui      craft-cloud-egui      Craft Cloud's piece of an app's window: the account line, the shared recents, a live document's badge and the people badge, and `CloudWatch` that keeps them current
  app       craft-cloud-app       what apps link: a blocking client of the agent's local API (status, sign-in, joining, computers, pause, autostart, recents, live documents at the level of bytes: `live.rs`, the admin calls: `admin.rs`, team spaces: `spaces.rs`)
apps/
  craft-cloud-server              HTTP API (`/api/v1`), the live service (`/api/v1/live`, `live.rs`: logs, documents by import, snapshots and compaction, presence, and in team spaces the space from the upgrade, a role per frame, stamped presence, revocation; `tests/team_live.rs`), join codes (`join.rs`: pairings, join, the `/join/{code}` page), the space layer (`space.rs`: the `craft-space` header, `require_space`, a role per route, `read_only`), the team-space routes (`spaces.rs`: `/spaces…`, `/admin/spaces…`, invitations, members), auth, admin pages (`web/`: server-rendered HTML with browser sessions, off unless `[web_admin]` is on), admin CLI (`init`, `serve`, `user` with `invite`, `device`, `reset-epoch`, `live`, `web-sessions`, `store` with `recompress`, `estimate`, `measure`: `storage_tools.rs`, `backup` and `restore` to an SFTP server: `backup.rs`)
  craft-cloud-agent               the sync agent: real disk `Fs`, HTTP `Transport` over a `Connection`, saved account and state, root marker, CLI (`login`, `join`, `run`, `sync`, `status`, `logout`, `recents`, `doc` (`doc.rs`), `add-computer`, `devices`, `pause`, `resume`, `quit`, `autostart`), the `run` loop, local API (`local_api.rs`, including server admin calls in `admin.rs`), team spaces (`spaces.rs`: discovery by `GET /spaces`, a folder and an engine per synced space, losing access), making a team space from a folder (`from_folder.rs`), live hub (`live/`: a document's Loro copy, recents document, who has a recent file open (`open_now.rs`), replica (recents and documents of files), store (with the documents' journals), link, hub, the local API's docs socket), start at login (`autostart/`: Windows, macOS, Linux), background build (`src/bin/bg.rs`, logging to `runlog.rs`)
    tests/team_spaces.rs          several spaces against a real server: invitations, the team folder both ways, viewer, removal, rename, folder clashes, the echo, an old server
    tests/network/                the simulation over a simulated network (turmoil): `main.rs` histories and scenarios, `harness.rs` server and agent hosts, `live.rs` live documents over WebSockets on it
    tests/recents_sim.rs          the recents convergence simulation: replicas against the server's live sessions
    tests/documents_sim.rs        the documents convergence simulation (shares `tests/docs_common/` with `documents.rs` and `live_speed.rs`)
  craft-cloud-desktop             the Craft Cloud app (`craft-cloud`, eframe/egui): the pure model (`model.rs`, the admin pages' part of it in `admin.rs`, the Shared tab's in `spaces.rs`, "Make a team space from this folder" in `make_space.rs`), the agent behind a trait and calls on threads (`agent.rs`), starting the agent (`start_agent.rs`), one app per user (`single_instance.rs`), the window (`ui/`, `app.rs`), the tray (`tray/`: tray-icon in the window's process on Windows and macOS; on Linux a StatusNotifierItem over zbus in a process of its own, `craft-cloud --tray`, `tray/process.rs`, since Wayland can't hide a window), the app's own start at login (`autostart.rs`), its icons (`assets/`, drawn by `cargo xtask icons`)
    tests/snapshots/              every screen as kittest draws it (`UPDATE_SNAPSHOTS=1` redraws them)
  craft-cloud-setup               the Windows setup (`craft-cloud-setup.exe`): install, upgrade, move from a hand-made install, `--uninstall`; the exes built in by `cargo xtask package`
  craft-cloud-kit-demo            the app kit's demo (never packaged or installed): a story and a layer tree, live through the kit; its model and adapter (`model.rs`), its window (`ui.rs`); tests drive two windows on one fake hub, and time the kit on a big document
packaging/linux/                  the Linux tarball's `install.sh` and `uninstall.sh` (into `~/.local`)
xtask/                            cargo xtask ci | licenses | icons | package | version
docs/                             the architecture (`architecture.md`), the interfaces for apps (`integration.md`), the security model, operating a server, the self-hosting guide, the design documents (`design/`), the user guide (`user/`) and the notes on this fork (`fork.md`)
```

Layering: `api` and `chunker` depend on nothing else in the workspace; `proto` depends on `api` only. What apps link
(`app`, `kit`, `egui`) depends on `api`, never on `proto`, so it brings no crate a Craft app doesn't already lock but
Loro's own (`cargo xtask suite-deps <the apps' Cargo.lock files>` checks). `store` and `meta` depend on `proto`.
The server depends on all three; nothing depends on the server outside tests. `sync` depends on `proto` and `chunker`
(its tests also use `meta` and `store` as an in-process server). `app` depends on `api` only and has no async
runtime. The agent depends on `proto`, `sync` and `app` (its `recents` commands are clients like an app); its
tests also run the server on a loopback port, and `tests/network/` runs the server's router and agents over
`turmoil` (a dev-dependency: the shipped binaries never link it). That test target includes sync's test harness
(`crates/sync/tests/common/`) by `#[path]`, so the agent's dev-dependencies mirror sync's test ones, and a change
there can break the agent's tests: run both (`cargo xtask ci` does). The app (`craft-cloud-desktop`) depends on
`app` and `proto` only, never on the agent: it reaches the agent through the local API and starts its binary. The
setup (`craft-cloud-setup`) depends on nothing in the workspace: it stops and moves the agent through the agent's own
local API (`POST /v1/quit`, then its process is waited for and, after a minute, ended) and command line
(`autostart on`).

Backend tests (Postgres, S3) skip unless their environment variables are set; see `.github/workflows/ci.yml`
for the variables and the test containers. Set `CRAFT_CLOUD_REQUIRE_BACKENDS=1` to make a skip a failure.

## 2. Clean room

Craft Cloud reimplements the *idea* of desktop cloud sync. It is not compatible with, and never
talks to, any vendor's cloud.

- Behaviour comes from public documentation, published papers (for example the FastCDC paper for
  content-defined chunking) and black-box use of products with synthetic test files and throwaway
  accounts. Never capture real personal data, account details or someone else's files.
- **Never reverse engineer another sync service's private protocol or API.** No packet captures of
  their clients, no decompiling their apps, no copying their wire formats. Our protocol is our own and documented in `docs/design/`.
- Never copy, port or paraphrase code from GPL, AGPL or LGPL projects (Nextcloud, ownCloud,
  Seafile, and similar). Don't read their source while working on the matching feature here.
  MPL-2.0 code (Syncthing) is not copied either.
- Dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0, CDLA-Permissive-2.0 (data only, e.g. root certificates)).
  Named exceptions, for no other crate: for what Loro (the live documents' CRDT) needs, MPL-2.0 for `im`, `bitmaps`
  and `sized-chunks`, used as published and never modified or copied from, and BSL-1.0 (the Boost licence) for
  `xxhash-rust`; for the Craft Cloud app's window (eframe), BSL-1.0 for `clipboard-win` and `error-code` (the
  Windows clipboard), and OFL-1.1 and the Ubuntu Font Licence (data only) for the fonts in `epaint_default_fonts`; OFL-1.1 font data
  is also bundled, unmodified, in the Craft Cloud app: Archivo, Inter and Geist Mono (`apps/craft-cloud-desktop/assets/fonts/`,
  their OFL texts beside them and in `LICENSES.txt`).
  Adding one means checking its license and every transitive license. `cargo xtask licenses` (part of
  `cargo xtask ci` and CI) checks every crate in the build against this list; a data-only licence is allowed
  only for the crates named in `xtask/src/licenses.rs`.
- Never copy vendor icons, artwork, UI text or documentation wording. Feature names are fine.

## 3. Golden rules

### Never crash (outranks feature work)

A bad request, a truncated upload, a corrupt chunk, a full disk or a broken config file must
produce an error the user or admin can act on, never a panic. A crashed server is a whole studio
that can't save. Shared standard: rule 1 ("Never crash") of the Rust house rules above.

- **Non-test code never panics.** No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!` or
  `unimplemented!`. Return the crate's error type and propagate with `?`.
- Every production crate root (`lib.rs`, each binary's `main.rs`, and `xtask`) carries
  `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]`.
- **No `unsafe`.** The workspace sets `unsafe_code = "forbid"`.
- **Everything from the network is hostile**, including requests from signed-in users. Use `get()`
  instead of indexing, checked arithmetic for sizes and offsets, and cap request bodies, chunk
  sizes, JSON sizes, path lengths and list lengths.

### Never lose data

- Nothing a user uploaded is deleted except by an explicit user action or the admin's retention
  policy. Garbage collection only removes chunks that no version references, and is tested for it.
- Every chunk is verified against its hash when written and when read. A mismatch is an error, never
  silently served.
- Writes are atomic: write to a temporary file, flush, then rename. A crash at any point leaves the
  old state or the new state, never a mix.
- A sync conflict keeps both copies. The client never overwrites a local change it hasn't uploaded.

### Security

- Passwords are hashed with Argon2id. Access tokens are random, stored only as hashes, and can be
  revoked per device.
- Never log passwords, tokens, file contents or file names at info level or above.
- Every request is checked against the signed-in user's permissions on the server; never trust a
  path, ID or flag from the client to decide access.
- Paths from clients are normalised and confined; `..`, absolute paths, drive letters, NUL bytes and
  reserved Windows names are rejected.
- No telemetry, analytics or update checks that send anything about the user or their server.

### Other rules

1. **Tests are the gate.** Protocol, chunking, storage and sync logic get unit tests. The sync
   engine gets tests for conflicts, interrupted transfers and offline edits.
2. **Assets need attribution.** Every bundled non-code asset gets a row in `ATTRIBUTION.md` (path,
   title, author, source, license) in the same change.
3. When fetching anything, use a generic `Craft-Cloud/<version>` User-Agent and never put a
   person's name, email or other personal details in requests.

## 4. Before you finish a task

```sh
cargo xtask ci              # fmt --check, licenses, clippy -D warnings, test
```

Or piecemeal: `cargo test -p <crate>`, `cargo clippy -p <crate> --all-targets -- -D warnings`.
The app's Linux tray tests run a private D-Bus and need `dbus-daemon`; without it they skip, and CI
(`CRAFT_CLOUD_REQUIRE_BACKENDS=1`) fails them.

## 5. Versions

The version lives only in `[workspace.package] version` in the root `Cargo.toml`
(`cargo xtask version`, `cargo xtask version set X.Y.Z[-pre]`).

## 6. Parallel agents

- Use your own target dir (`CARGO_TARGET_DIR=target/agent-<name>`) and edit only the files you own.
- Keep every `Cargo.toml` valid at all times: the `crates/*` and `apps/*` globs mean one broken
  manifest breaks everyone's build. Write manifests to a temp file and `mv` them into place.
- If someone else's in-progress edit breaks the build, wait and retry; don't fix their files.
