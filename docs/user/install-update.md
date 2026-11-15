---
title: Installing and updating Craft Cloud
description: Install the Craft Cloud app on Windows, macOS and Linux, remove it again, and update the app and the server.
---

# Installing and updating

The Craft Cloud app comes with its sync agent, and it installs for you alone: it needs no administrator rights and puts
nothing on the desktop. The package for your system is built with `cargo xtask package`, which writes it to
`target/package/` for the system it runs on. Every package carries a `LICENSES.txt` that is installed next to the programs.

## Installing the app

### Windows

Run `craft-cloud-setup.exe`. It installs into `%LOCALAPPDATA%\Programs\Craft Cloud`, adds Craft Cloud to the Start menu
and to "Apps & features", and at the end asks whether to open Craft Cloud now. Two options change that:

- `--no-start-menu` leaves the Start menu entry out.
- `--quiet` installs without asking anything. Only the exit code tells how it went: 0 for done, 1 for failed.

An earlier install made by hand in `%LOCALAPPDATA%\Programs\craft-cloud` is taken over, and its files are left alone.

### macOS

Unzip `craft-cloud-<version>-macos-<arch>.zip` and move `Craft Cloud.app` to Applications. The build is not signed, so
the first time, open it from its context menu with **Open** to tell macOS that it is fine.

### Linux

Unpack `craft-cloud-<version>-linux-x86_64.tar.gz` and run `./install.sh` in the unpacked folder. It installs into
`~/.local/lib/craft-cloud`, links `craft-cloud` and `craft-cloud-agent` into `~/.local/bin`, and adds Craft Cloud to the
app menu. `--no-menu` leaves the menu entry out.

### After installing

Open Craft Cloud and follow [Getting started](getting-started.md) to sign in. A computer that was signed in before,
with the agent's `login` command, shows its account at once.

The tray icon says whether everything is synced, syncing, offline, paused or needs you, and its menu has **Open Craft
Cloud**, **Open folder**, **Pause** (or **Resume**) and **Quit**. Quit stops syncing until the next login. On Windows and
macOS, closing the window leaves the app in the tray. On Linux the tray icon is a small process of its own, because
Wayland cannot hide a window: closing the window ends it, and **Open Craft Cloud** in the tray brings it back. The icon
needs a StatusNotifierItem host, for example the tray of waybar, KDE, or GNOME with the AppIndicator extension. Without
one, the window is the way in, and the agent keeps syncing either way.

## Uninstalling

Uninstalling keeps your synced folder and the agent's data folder, which holds the sign-in.

- **Windows:** uninstall Craft Cloud in "Apps & features". `craft-cloud-setup.exe --uninstall` does the same from a
  command line, and adding `--quiet` skips the questions.
- **macOS:** quit Craft Cloud from the tray, then move `Craft Cloud.app` to the Bin. To stop the agent starting at login
  first, run `autostart off` with the agent inside the app, at
  `/Applications/Craft Cloud.app/Contents/MacOS/craft-cloud-agent`.
- **Linux:** run `./uninstall.sh` from the unpacked folder.

The agent's data folder is `%LOCALAPPDATA%\craft-cloud-agent` on Windows, `~/Library/Application Support/craft-cloud-agent`
on macOS and `~/.local/share/craft-cloud-agent` on Linux. Remove it only if you want to forget the sign-in on this
computer. To sign out properly instead, use **Sign out** in the footer of the app while it is installed. The files stay on
the computer either way.

## Updating the app

Update the app after the server, never before. The app and its agent must be the same version: if they are not, the app
says that "The Craft Cloud program on this computer is older than this window" and asks you to install the same version
of both. The **Computers** tab marks a computer that runs an older app with **Older**.

- **Windows:** run the new `craft-cloud-setup.exe`. The agent finishes what it is doing, stops, and starts again with the
  new files.
- **macOS:** quit Craft Cloud from the tray and replace `Craft Cloud.app` in Applications with the new one.
- **Linux:** unpack the new tarball and run `./install.sh` again. A running agent finishes its pass, stops and starts again
  from the new copy.

## Updating the server

An update can change the database, and the server migrates it when it starts. A database that a newer server has migrated
cannot be opened by an older one, so rolling back means restoring a backup. Take a backup before every update, update the
server first, and update the apps afterwards. [Running the server](../server.md#data-upgrading-and-backups) describes the
backups, and [operations](../operations.md) covers running a server day to day.

**With a binary.** Stop the server, back up the database and the blobs (for SQLite and local disk, that is `meta.db*`,
`blobs/` and `two-factor.key` together), replace the binary with the new build, and start the server again. Read the
release notes first.

**With Docker.** In the checkout you installed from, run:

```sh
cd /opt/craft-cloud && ./packaging/docker/update.sh
```

The script shows what it is about to change and asks before it does anything. It builds the new image while the old server
keeps running, then stops the server, copies the data volume into `backups/`, and starts the new server. The browser admin
overview can show a notice when a newer version exists, but only if you turn the update check on, and it is off by
default. The server never updates itself. The details are in [the Docker section](../server.md#docker) of the server
guide.
