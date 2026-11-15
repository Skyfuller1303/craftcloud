---
title: Troubleshooting Craft Cloud
description: What the Craft Cloud app and its agent report when something is wrong, what each message means and what to do.
---

# Troubleshooting

This page lists the states that the app and the agent report, in the words they use. Most problems show on the
**Overview** tab, in the Sync cell. A line in red is followed by **Details**, which holds the agent's full message and can
be selected and copied, for example into a message to the person who runs your server. The footer always shows the state
in one line, such as "Online · Up to date", and the tray icon says "Needs your attention" when something needs you.

## The server cannot be reached

The tag says **Offline** and the line reads "Can't reach cloud.example.org" followed by "trying again". The agent tries
again with growing pauses, up to five minutes apart. Your files stay where they are, and what you change meanwhile goes up
when the connection is back. Check your internet connection, then that the server is running and that its address still
works. A different message, "Syncing failed", means that the server answered but the pass failed; it also tries again.

## Paused, and why

The line says why the agent stopped, and **Details** holds the message. These are the states the app reports.

- **"Paused: nothing syncs until you resume".** You paused it. Click **Resume** on the Overview or in the tray menu.
- **"The server is full. Uploads wait until its admin frees space".** The server's disk or capacity has run out, which is
  not your own limit. Nothing is lost. The agent looks again every ten minutes and goes on once the person who runs the
  server has freed space.
- **A message that the account's storage on the server is full.** You have used your own storage limit. The message gives
  the bytes used and the limit. Delete files you no longer need, or ask the administrator to allow more. The Storage cell
  shows what you use.
- **The folder or its marker is missing.** The agent cannot find the synced folder, or the `.craft-cloud` marker inside it.
  This happens when a drive is not mounted yet. It looks again every minute and goes on by itself once the folder is back.
  If you moved or deleted the folder on purpose, sign out and join again.
- **"Not signed in".** This computer has no account. Join again from the first-start screens.

## This computer was removed

The app shows "This computer was removed from your cloud" and a **Join again** button. Another computer, or an
administrator, removed this one from the account. The files stay on this computer. Click **Join again** and sign in
as in [Getting started](getting-started.md).

## The agent is not running

If the app cannot start the background service, it shows "Craft Cloud's background service didn't start". If the service
goes away while the window is open, it shows "Craft Cloud's background service stopped". Click **Try again**. The
**Details** end with "Its log:" and the path of the log. The agent writes what it says to `run.log` in its data folder, and
moves the log to `run.log.1` when it grows past 10 MiB. The data folder is `%LOCALAPPDATA%\craft-cloud-agent` on Windows,
`~/Library/Application Support/craft-cloud-agent` on macOS and `~/.local/share/craft-cloud-agent` on Linux. If you chose
**Quit** in the tray, nothing syncs until you open the app or log in again.

If the Sync cell says "This computer won't sync after a restart: starting at login couldn't be turned on", the app is
working now but will not start by itself. Click **Try again** under it.

## A read-only role

A team space where you are a viewer, or one that was archived, shows a banner on its page. "You can read this space but
not change it. What you change here stays on this computer and doesn't go up." means your role is **Viewer**, and the
files you change or add are kept here and not uploaded. Ask an owner of the space for the **Editor** role. "This space is
archived: everyone, owners included, can only read it. Nothing is deleted." means an administrator archived the space,
and an owner or administrator can bring it back.

## A sync that would delete many files

When a lot of files are deleted at once on one computer, Craft Cloud holds the deletions back until you approve them.
Nothing is lost while they wait. By default this applies to more than 500 files in one pass, or to more than 20 percent of
the synced files once at least 10 are deleted. This protects you from a drive that was emptied or unmounted by mistake.

For a team space, its page has a **Held deletes** card that lists what waits. Click **Approve deletes** and confirm
"Approve the held deletes?", or choose **Keep them held**. Each computer approves its own side. The Overview does not show
held deletions for your own folder, and the app has no button for them. Run `craft-cloud-agent status` to list them, and
`craft-cloud-agent sync --approve-deletes` to approve exactly what it listed. See [the agent](../agent.md) for the details.

## A space is full, gone or not available

- "The space is full: uploads are refused until something is removed or the limit is raised." Ask an owner or an
  administrator to raise the limit, or remove files.
- "You no longer have access to this space. Your local copy is kept as it is and no longer syncs." Someone removed you from
  the space. The files stay on this computer.
- "Update Craft Cloud on this computer to use team spaces" means the app and its agent are older than each other. Install
  the same version of both, as described in [installing and updating](install-update.md).
- "The server doesn't support team spaces yet. Ask its administrator to update it." Your own files sync as before.
- "This space is being changed. Try again in a moment."

The Shared tab's State column also says **Syncing**, **Synced**, **Paused**, **Retrying**, **Server full**, **Over the
limit**, **Read only**, **Needs you** or **Unavailable**. **Needs you** means a message is waiting on the space's page.

## Joining and signing in

- "This code doesn't work any more. Make a new one on a signed-in computer." A join code works once and for ten minutes.
  On the signed-in computer, click **Make a new code** (the page says "This code expired: make a new one.").
- "This folder already has files in it." Choose **Sync them** to add them to your cloud as copies where they differ, or
  **Choose another folder**.
- "This sign-in can't go on any more (it took too long or had too many wrong codes). Sign in again." Two-factor sign-in
  allows five wrong codes. A recovery code works once. Use **Use a recovery code** if you lost your phone.
- When a sign-out fails because the server cannot be reached, the footer shows the error and **Sign out on this computer
  only**, which forgets the account here and keeps the files.

## Linux has no tray icon

The icon needs a StatusNotifierItem host, such as the tray of waybar, KDE, or GNOME with the AppIndicator extension. Without
one, open Craft Cloud from your app menu. The agent keeps syncing either way.
