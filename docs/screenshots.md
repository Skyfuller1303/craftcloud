# Screenshots

The pictures on this page are not taken by hand. They are the expected images of the Craft Cloud app's snapshot tests, drawn by the tests with a CPU renderer from a made-up account, and kept in `apps/craft-cloud-desktop/tests/snapshots/`. A picture here therefore shows what the code draws today: when a screen changes, its test fails until the picture is redrawn. [development.md](development.md) says how the tests run and how `UPDATE_SNAPSHOTS=1` redraws them. `ATTRIBUTION.md` lists the folder.

There are 264 pictures, for every screen in light and dark and at small window sizes. This page shows one per screen.

## The Craft Cloud app

### First start and sign-in

| The server step | A join string | Adding a computer |
|---|---|---|
| ![The first step asks for the server](../apps/craft-cloud-desktop/tests/snapshots/step-server.png) | ![Joining with a join string](../apps/craft-cloud-desktop/tests/snapshots/join-string.png) | ![A code for another computer](../apps/craft-cloud-desktop/tests/snapshots/adding-a-computer.png) |

### Overview and files

| Overview | Recent files | Conflict copies |
|---|---|---|
| ![The overview](../apps/craft-cloud-desktop/tests/snapshots/overview-folders.png) | ![Recent files of every app](../apps/craft-cloud-desktop/tests/snapshots/files-loaded.png) | ![Conflict copies](../apps/craft-cloud-desktop/tests/snapshots/conflicts-list.png) |

### Versions and trash

| Versions of a file | Trash |
|---|---|
| ![Earlier versions](../apps/craft-cloud-desktop/tests/snapshots/versions-loaded.png) | ![The trash](../apps/craft-cloud-desktop/tests/snapshots/trash-loaded.png) |

### Sharing with a team

| Team spaces and invitations | A team space, as owner | A team space made from a folder |
|---|---|---|
| ![The Shared tab](../apps/craft-cloud-desktop/tests/snapshots/shared-list.png) | ![A team space with its members](../apps/craft-cloud-desktop/tests/snapshots/shared-space-owner.png) | ![Making a team space from a folder](../apps/craft-cloud-desktop/tests/snapshots/make-space-done.png) |

### Computers and security

| Computers | Two-factor sign-in, setting up | Two-factor sign-in, on |
|---|---|---|
| ![The computers of the account](../apps/craft-cloud-desktop/tests/snapshots/computers-light.png) | ![Setting up two-factor sign-in](../apps/craft-cloud-desktop/tests/snapshots/security-setup.png) | ![Security with two-factor sign-in on](../apps/craft-cloud-desktop/tests/snapshots/security-on.png) |

### The administrator's pages in the app

| Accounts | Storage | Audit log |
|---|---|---|
| ![Accounts](../apps/craft-cloud-desktop/tests/snapshots/admin-users.png) | ![Storage](../apps/craft-cloud-desktop/tests/snapshots/admin-storage.png) | ![The audit log](../apps/craft-cloud-desktop/tests/snapshots/audit-log.png) |

### Dark

| Signed in | Recent files |
|---|---|
| ![Signed in, dark](../apps/craft-cloud-desktop/tests/snapshots/signed-in-dark.png) | ![Recent files, dark](../apps/craft-cloud-desktop/tests/snapshots/files-loaded-dark.png) |

## The pieces an app shows

`craft-cloud-egui` gives an app the account line, the shared recent files, the live badge and the people badge. The app's own gallery test draws them together:

![The kit's pieces, light](../apps/craft-cloud-desktop/tests/snapshots/kit-gallery-light.png)

Pictures of these pieces inside a Craft app belong to that app's repository and its own snapshot tests, not here.

## The admin pages in the browser

[web-admin.md](web-admin.md) shows each page. Those pictures were taken from a test server with made-up accounts and are kept outside the main branch. Unlike the app's pictures they are not produced by a test: the tests of the admin pages (`apps/craft-cloud-server/tests/web.rs` and the unit tests in `src/web/`) check the HTML as text and draw nothing. They can therefore go out of date without any test failing.

## What has no picture yet

Every screen has at least one picture. Some short-lived states have none; a test that draws them would go next to the existing ones in `apps/craft-cloud-desktop/src/ui/`:

- adding a computer while the code is being made, and after it expired (`ui/add_computer.rs`);
- the versions list while loading, after an error, and with more versions to load (`ui/versions.rs`);
- the trash while loading and after an error (`ui/trash.rs`);
- renaming a team space, and the invitation form with an error (`ui/spaces.rs`);
- the confirmation before a computer is removed (`ui/computers.rs`);
- the panel shown when signing out fails (`ui/shell.rs`).
