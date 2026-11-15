# Integrating an app

This page describes the interfaces that an app uses to work with Craft Cloud: saving to the cloud, sharing files with a team, and working on a document together. For each it gives the purpose, the calls and types, the security model and a short example. The last section says how these interfaces relate to the suite's own plans for cloud documents, and where they do not fit yet.

The detailed references are elsewhere and are not repeated here: [kit-integration.md](kit-integration.md) (adding the kit to a Rust app), [kit.md](kit.md) (the kit's API), [protocol-reference.md](protocol-reference.md) (the local API for other languages) and [agent.md](agent.md) (the agent). [architecture.md](architecture.md) shows how the parts fit together.

## The one rule

An app talks to the agent on its own computer and to nothing else. It does not know the server's address, holds no password and no token of the account, and opens no connection to the network for Craft Cloud. Everything below goes through the agent's local API:

- The agent listens on `127.0.0.1` on a port it chooses, and writes the port and a token to `local-api.json` in its data directory. Only the user can read that file.
- Every call carries `Authorization: Bearer <token>`. The agent requires the exact `Host` header of its own address and refuses any request that has an `Origin` header, so a web page cannot reach it.
- The API has a version (`Status.api`, currently 1). A call that an older agent does not know is reported as `agent_too_old`.

A Rust app does not write any of this itself. `craft_cloud_app::Client::connect()` finds the agent and returns a blocking client; no async runtime is needed.

```rust
let client = craft_cloud_app::Client::connect()?;
let status = client.status()?;
if !status.signed_in {
    // Show "not signed in" and keep working with ordinary files.
}
```

If the agent is not running or nobody is signed in, the app keeps working with local files. Nothing in these interfaces is required for an app to open and save.

## Cloud saving

**Purpose.** Files that an app saves into the synced folder are stored on the user's server with their history, appear on the user's other computers, and can be restored to an earlier version.

**How saving works.** There is no save call. The app writes an ordinary file into the synced folder (`Status.folder`, by default `~/Craft Cloud`), atomically, as it should anywhere. The agent syncs it. This is why the files stay plain files that backups and other tools can read, and why saving works offline.

**Calls** (`craft-cloud-app`, with the local API path):

| Purpose | Client call | Local API |
|---|---|---|
| Account, folder, sync state | `status()` | `GET /v1/status` |
| The app's recent files, shared across computers | `recents(app)`, `add_recent(app, entry)`, `watch_recents(app)` | `GET /v1/recents` (WebSocket) |
| The versions of a file | `file_history(path, cursor, limit)` | `/v1/files/history` |
| Restore an earlier version | `restore_file(path, version, expected_version)` | `/v1/files/restore` |
| The trash | `trash(cursor, limit)`, `restore_from_trash(id)`, `remove_from_trash(id)` | `/v1/trash`, `/v1/trash/restore`, `/v1/trash/remove` |
| Conflict copies in the synced folder | `conflicts()` | `/v1/conflicts` |
| Storage in use | `usage()`, `folder_usage()` | `/v1/usage`, `/v1/usage/folders` |
| Large binary content of a document | `blobs().put(bytes)`, `blobs().get(hash, len)` | `PUT /v1/blobs`, `GET /v1/blobs/<hash>` |

**Versions and restore.** The server keeps every version of a file for the retention time the administrator sets. A restore names the version to bring back and the version the caller believes is current (`expected_version`). If the file has changed in the meantime the restore is refused instead of overwriting the newer version, and the caller reads the history again. A restore makes a new version; it deletes none.

```rust
let history = client.file_history("Notes/story.craft", None, Some(20))?;
// Show history.versions to the user, then restore the chosen one:
// client.restore_file("Notes/story.craft", chosen, current)?;
```

**Checkpoints.** There is no separate checkpoint object in this code. A version is created by each sync of a saved file, and for a live document a save record ties the saved file to a point in the document (see Co-working below). An app that wants named checkpoints has to keep the names itself for now.

**Security model.** The agent confines every path to the synced folder or to a synced team folder; `..`, absolute paths outside those folders, drive letters and reserved names are refused. The server checks each request against the account's permissions again; a path or ID from a client never decides access. Every chunk is verified against its hash on the way in and on the way out.

## File sharing

**Purpose.** A folder that several accounts on the same server share: a team space. Each member has a role, and the folder syncs to each member's computer like the personal one.

**Model.**

- An administrator creates a team space and names its first owner. Being administrator gives no access to the files of a space; only membership does.
- Roles are **viewer** (read), **editor** (read and write) and **owner** (read, write, and manage members). A space always keeps at least one owner.
- An owner invites another account and chooses its role. The invited person accepts or declines. A member can leave; an owner can change a role or remove a member.
- A member chooses, per computer, whether a space syncs there and into which folder.
- An administrator can make a team space from an existing folder of their personal folder. The agent copies the files into the new space, checks on the server that every file arrived, and only then removes the old folder, which goes to the trash and can be restored.

**Calls** (`craft-cloud-app`, module `spaces`):

| Purpose | Client call |
|---|---|
| The spaces and invitations of the account | `spaces()` |
| Accept or decline an invitation | `accept_space(id)`, `decline_space(id)` |
| Sync a space on this computer, or stop | `set_space_sync(id, request)` |
| Members and roles | `space_members(id)`, `add_space_member(id, request)`, `change_space_member(id, user, request)`, `remove_space_member(id, user)`, `leave_space(id)` |
| Make a space from a folder | `make_space_from_folder(request)` |
| Storage used by a space | `space_usage(id)` |
| Recent files of a space | `recents_in(app, space)`, `watch_recents_in(app, space)` |
| Blobs that belong to a file in a space | `blobs().for_path(path)` |

On the server these are the routes under `/api/v1/spaces` and, for administrators, `/api/v1/admin/spaces`. An app does not call them; the agent does.

**A file in a team space** is an ordinary file in that space's synced folder. An app opens it by its path there. For a viewer the agent does not upload local changes: the file can be opened and read, and a write to a live document is answered with `read_only`.

```rust
let list = client.spaces()?;
// For each space: its name, the account's role, whether it syncs here and where.
// For each invitation: who invited, and the offered role.
```

**Security model.** Every server route that touches files or documents names its space and is checked against the caller's role for that route; a role too low is answered with `read_only` or a refusal, never served. Removing a member or archiving a space takes effect on the next request and closes that member's live connections to the space's documents. Blobs are bound to a space: content uploaded for a team document is stored in that space, so that the other members can read it and nobody else can.

**What is not here.** Sharing is between accounts on one server. There is no access for people without an account (guest access, guest grants and their expiry, rotation and revocation are not part of this code), no sharing by link, and no sharing between servers.

## Co-working

**Purpose.** Several people, or one person on several computers, edit the same document at once. Each sees the others' changes as they happen and sees who else is there.

**The interface is the app kit**, `craft-cloud-kit`. The app keeps its own model, file format and user interface. It tells the kit how its model maps onto shared structures, and the kit keeps the model live.

- **`LiveDocument`** is the trait an app implements once for its document type: the name of the app, a schema number, the mappings of the model's parts (a tree, text, a table, a timeline, or an opaque value), how to apply a remote change, and how to repair a state that a merge made invalid.
- **`Session`** is one open live document: `open`, `local_change(&old, &new)` after each command, `poll()` once per frame for remote changes, `undo()` and `redo()` that leave other people's edits alone, `save_point()` and `record(point, bytes)` when the file is saved, `people()` and presence for who is here, and `status()` for the badge.

```rust
match Session::<MyApp>::open(&Agent::local(), path, model, Some(file_bytes))? {
    Open::Live(session) => { /* edit through the session */ }
    Open::NotLive(_) => { /* an ordinary file: edit as before */ }
}
```

[kit-integration.md](kit-integration.md) walks through the whole adapter with the demo app's code.

**Presence and people.** Presence is a small value (at most 4 KiB) that an app sends for its user, such as a cursor or a selection. It is relayed to the others and never stored. With identity turned on, each incoming presence carries the sender's account, and `people()` lists who is in the document with a name and a colour from a fixed palette.

**Roles.** In a team space the role applies to the live document too. The server checks the role for every frame it receives, not only when the connection opens, so a member whose role is lowered loses the ability to write at once. A viewer's session is read-only: the kit refuses local changes with `KitError::ViewOnly`, and a role change reaches the open session as an event.

**Offline.** Changes made without a connection stay in the agent's journal and in the kit's queue (up to 64 MiB) and are sent when the connection returns. A document also becomes read-only when a newer version of the app wrote it with a higher schema number.

**An app that is not written in Rust** uses the docs WebSocket of the local API directly. [protocol-reference.md](protocol-reference.md) gives the frames.

**Security model.** The server stores a document's log and snapshots and relays its frames; it does not interpret the content of a frame. Access is decided per frame by membership and role in the document's space. The app never authenticates to the server; the agent's device token does. Sizes are capped at every step (a commit under 512 KiB from the kit's writer, the presence limit, the queue).

## How this fits the suite's plans

The organisation that maintains the Craft apps has accepted three plans that touch this ground. They are RFCs in `storytold/craft-contrib`. Craft Cloud is not mentioned in any of them and is not part of that organisation; this section only compares.

### Cloud documents (RFC-0024)

The plan: apps depend on a vendor-neutral contract crate, `craft-cloud`, with the traits `CloudBackend` and `ShareBackend`, and never on one provider. Remote storage must never require running one particular service. Every adapter must pass an acceptance test in which it refuses to overwrite a newer remote version and keeps both copies.

Where Craft Cloud fits: as one provider among several, behind that contract.

Where it does not fit yet:

- **There is no adapter.** Nothing in this repository implements `CloudBackend`. Today an app that uses Craft Cloud links `craft-cloud-app` directly, which is exactly the dependency on one provider that the plan rules out. The contract crate itself was still a pull request when this was written, so its final shape is not known here.
- **The model differs.** The contract is about opening and saving remote documents through the app. Craft Cloud syncs a folder: the app saves a local file and the agent does the rest. An adapter would present the synced folder as the remote, map "open" and "save" to local file operations plus sync state, and map versions to `file_history` and `restore_file`.
- **Refusing to overwrite.** The pieces exist: a restore names the version it expects, a conflict keeps both copies, and the agent never overwrites a local change it has not uploaded. Whether they satisfy the contract's acceptance test is not shown, because the test has not been run against Craft Cloud.
- **The name.** The contract crate and this project share the name `craft-cloud`. [fork.md](fork.md#the-name-craft-cloud) proposes how to keep them apart.

### Accounts and tokens (RFC-0025)

The plan: secrets live in the operating system's keychain through a shared crate, the list of accounts is a file in a suite folder, and no secret is ever written to a file or an environment variable in plain form.

What Craft Cloud does: the agent holds the account. The device token is in `account.json` in the agent's data directory, written so that only the user can read it, and the token for the local API is in `local-api.json` beside it. Apps hold nothing.

The gap is real: a token in a file that the user can read is not a token in the keychain, and the plan does not allow it. The two designs agree on the more important point, that an app should not hold credentials. Moving the agent's device token into the keychain would close the gap without changing any interface on this page. That change is not made.

### Live collaboration (RFC-0020)

The plan: presence, invitations, chat and concurrent text as vendor-neutral contracts beside the cloud contract, with a mature CRDT behind the text interface, a document served by one relay, frames that the relay does not interpret, and features that are simply absent when there is no back end.

Where Craft Cloud fits: it uses Loro, a CRDT of the kind the plan names. One server serves a document. The server relays frames without interpreting them. An app without an agent keeps working with ordinary files.

Where it does not fit yet: the kit's `LiveDocument` and `Session` are Craft Cloud's own interfaces, not the plan's contracts, and they cover more than text (trees, tables, timelines). There is no chat. Invitations here are invitations to a team space for an existing account, not the one-time keys of the plan. Whether the plan's rule about replica identities matches how the agent assigns Loro peer IDs has not been checked.

### In short

Craft Cloud is a working provider with its own interfaces. To sit behind the suite's contracts it needs an adapter crate for cloud documents, the device token in the keychain, and a mapping of the kit onto the collaboration contracts or an agreement that the kit is the richer interface for apps that want it. None of the three exists in this repository, and none is claimed.
