# Architecture

This page is the map of Craft Cloud: which programs and libraries exist, what each is responsible for, and how a file or an edit travels between them. It stays at the level of the map. The designs behind it are in [design/sync.md](design/sync.md), [design/live.md](design/live.md) and [design/collaboration.md](design/collaboration.md), and the workspace layout with every source file is in [`AGENTS.md`](../AGENTS.md).

## The idea

Craft Cloud does three things, and each is an interface that an app can use without knowing how the others work:

1. **Self-hosted storage.** A person runs the server on a computer, NAS or VPS of their own. There is no hosted service, no account with anyone else, and nothing reports back to anyone.
2. **Integration into tools.** A Craft app saves and opens ordinary files. It talks to a small agent on the same computer and never to the server, so it holds no credentials and has no network code for the cloud.
3. **Interfaces for saving, sharing and working together.** Saving with versions and restore, sharing a folder with a team, and editing a document together, each with its own surface (see [integration.md](integration.md)).

## The parts

```text
                      one computer                                     a machine you run
┌──────────────────────────────────────────────────────┐        ┌──────────────────────────────┐
│  Craft app ──► craft-cloud-app ─┐                    │        │  craft-cloud-server          │
│     │          craft-cloud-kit ─┤  local API         │        │   /api/v1        sync, account│
│     │          craft-cloud-egui ┤  127.0.0.1, token  │  HTTPS │   /api/v1/live   live service │
│     ▼                           ▼                    │◄──────►│   /web           admin pages  │
│  files in  ◄──────────  craft-cloud-agent            │        │   /join/{code}   join page    │
│  ~/Craft Cloud          (sync, live hub, account)    │        │        │              │       │
│                                 ▲                    │        │  craft-cloud-meta  craft-cloud-store
│  Craft Cloud app ───────────────┘                    │        │  SQLite or Postgres  disk, S3 or SFTP
│  (craft-cloud-desktop: window and tray)              │        └──────────────────────────────┘
└──────────────────────────────────────────────────────┘
```

### The server (`apps/craft-cloud-server`)

One binary, `craft-cloud-server`, with an HTTP API under `/api/v1` and a command line for the administrator.

- **Sync API.** The tree, the change feed, commits, and chunk upload and download (`/tree`, `/changes`, `/commit`, `/chunks/missing`, `/chunks/claim`). A commit is a batch of operations that the server applies atomically or refuses.
- **Accounts and devices.** Sign-in with a password and an optional second factor, a token per device that can be revoked, join codes and pairings for adding a computer (`/auth/login`, `/devices`, `/pairings`, `/join`).
- **History and trash.** Earlier versions of a file and their restore, and the trash (`/nodes/{id}/history`, `/nodes/{id}/restore`, `/trash`).
- **Team spaces.** A second kind of space beside the personal one, with members, roles and invitations (`/spaces…`, `/admin/spaces…`). Every route that touches files or documents names its space in the `craft-space` header and is checked against the caller's role (`space.rs`).
- **The live service.** A WebSocket endpoint, `/api/v1/live`, that keeps the log and the snapshots of live documents, relays presence, and in a team space checks the role for every frame (`live.rs`).
- **The admin API and the admin pages.** Accounts, invitations, devices, storage, the audit log and server health as an API (`/admin/…`) and as server-rendered HTML pages under `/web`, which are off until the administrator turns them on.
- **The administrator's command line.** `init`, `serve`, `user`, `store`, `backup`, `restore` and more. [server.md](server.md) is the guide, [operations.md](operations.md) covers backups and upgrades.

The server stores two kinds of data through two libraries. `craft-cloud-meta` holds the metadata (accounts, devices, the tree, versions, spaces, the audit log, live documents' logs) in SQLite or Postgres. `craft-cloud-store` holds the content as blobs on disk, in S3 or on an SFTP server, and checks every blob against its hash when it is written and when it is read.

### The agent (`apps/craft-cloud-agent`)

The agent runs in the background on every computer and is the only program there that talks to the server.

- **Sync.** It watches the synced folder, cuts files into content-defined chunks (`craft-cloud-chunker`: FastCDC and BLAKE3), uploads what the server lacks and applies what others changed. The engine, `craft-cloud-sync`, is written against two traits, a file system and a transport, so the same code runs on a real disk and in the simulation that tests it.
- **The local API.** An HTTP and WebSocket API on the loopback interface, protected by a token in a file that only the user can read. Apps and the Craft Cloud app use it for everything: status, sign-in, recent files, history, trash, team spaces, live documents and blobs.
- **The live hub.** For each live document the agent keeps a copy and a journal on disk, so edits survive a restart and continue offline, and it holds the one connection to the server's live service.
- **The account.** The agent keeps the server address and the device token. Apps never see them.
- **Team spaces.** It discovers the spaces the account belongs to and syncs each chosen one into a folder of its own.

### The Craft Cloud app (`apps/craft-cloud-desktop`)

The window and the tray icon: the overview, recent files, shared spaces and invitations, computers, security, and the administrator's pages. It is a client of the agent's local API like any other app, it starts the agent, and it never talks to the server itself. `apps/craft-cloud-setup` is the Windows installer.

### What a Craft app links

Three libraries, none of which brings an async runtime or a network stack of its own:

| Crate | What it is |
|---|---|
| `craft-cloud-app` | A blocking client of the agent's local API: status, sign-in, recent files, history and restore, trash, team spaces, blobs, and live documents at the level of bytes. |
| `craft-cloud-kit` | The app kit. An app implements `LiveDocument` for its model, and a `Session` keeps that model live: local changes, remote changes, undo, presence, people, save records. |
| `craft-cloud-egui` | Ready-made pieces for an egui window: the account line, the shared recent files, the live badge and the people badge. |

All three depend on `craft-cloud-api`, the part of the protocol that apps need, and never on `craft-cloud-proto`, the wire types between agent and server. `cargo xtask suite-deps` checks that linking them adds no crate that a Craft app does not already lock, apart from the Loro CRDT's own. `apps/craft-cloud-kit-demo` is a small app that shows the kit in use.

## How a saved file travels

1. An app saves a file into the synced folder, as it would anywhere.
2. The agent notices the change, cuts the file into chunks, asks the server which chunks it lacks, uploads those, and commits a new version of the file.
3. The server checks every chunk against its hash, records the version, and adds the change to the change feed of the space.
4. The agents of the other computers read the feed, fetch the chunks they lack and write the file, atomically: a temporary file, then a rename.

Earlier versions stay on the server for the retention time the administrator sets. If two computers change the same file, both copies are kept. A sync that would delete many files stops and asks first.

## How a live edit travels

1. An app opens a file through the kit. If the file is live, the app receives the document's state from the agent. If it was never live, the app imports its model once.
2. Each local command becomes a small change. The kit hands it to the agent over the local API's docs socket, and the agent keeps it in the document's journal on disk and acknowledges it.
3. The agent sends the change to the server's live service, which appends it to the document's log and relays it to the other agents that have the document open.
4. Their apps receive the change through the kit and rebuild the touched parts of their model.

Presence (who is here, and where) takes the same path but is never stored. Saving still writes an ordinary file; a save record ties the file to the point in the live document it corresponds to.

## Co-working: where it comes from

Live documents, presence and the app kit are the work of Fabio Lussi, the author of the original Craft Cloud ([itrycodinganditsbad/craft-cloud](https://github.com/itrycodinganditsbad/craft-cloud)): the live protocol, the server's live service, the agent's live hub with its journals, the mapping of an app's model onto a CRDT, and the `LiveDocument` and `Session` interfaces. This fork builds on that foundation. Team spaces, roles per frame, people with names and colours, and read-only documents for viewers were added on top of it. See [fork.md](fork.md).

## What is not here

The map above describes the code in this repository. Some things that a reader may expect are not built:

- **No end-to-end encryption.** The server can read the files it stores. [security.md](security.md) says what is protected and how.
- **No guest access.** Sharing is between accounts on the same server, through team spaces. Access for people without an account is not in this code.
- **No sharing by link, no comments, no locks.** They are drafted in [design/later-phases.md](design/later-phases.md) as proposals.
- **No provider adapter for the suite's vendor-neutral contract.** [integration.md](integration.md#how-this-fits-the-suites-plans) says what is missing.
- **No hosted service and no federation between servers.** One server, the people who have accounts on it.
