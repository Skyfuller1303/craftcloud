# Security policy

Craft Cloud stores people's files on servers they run themselves. A weakness in it can expose or lose those files, so reports are welcome and are handled before other work.

## Reporting a vulnerability

Please report privately, not in a public issue or pull request.

Use GitHub's private vulnerability reporting for this repository: on the repository page open **Security**, then **Report a vulnerability**. The report is visible only to you and the maintainer.

If that page does not offer the button, private reporting is not turned on for the repository yet. In that case open an issue that says only that you have a security report and asks for a private channel. Do not put any detail of the weakness in it.

A useful report says:

- what an attacker can do, and what they need for it (an account, a role in a team space, access to the network, access to the computer);
- the version or commit, and whether the server ran from a binary or from the Docker setup;
- the steps to reproduce it, with made-up accounts and files.

Please do not include real passwords, tokens, join strings, host names or other people's files, and do not test against a server that is not yours.

## What to expect

This is a small project maintained in spare time. The maintainer aims to answer a report within seven days, to say whether it is accepted, and to keep you informed until a fix is published. You are credited in the fix unless you prefer not to be.

## Supported versions

Craft Cloud is in early development and has one line of development. Fixes go into the main branch and the next release. Older releases are not patched.

## Scope

In scope: the server and its admin pages, the agent and its local API, the Craft Cloud app, the installers, the Docker setup in `packaging/docker/`, and the libraries that apps link.

Out of scope: weaknesses that need an administrator of the server or a user who already controls the computer to act against themselves, and the configuration of a reverse proxy, database or storage service that the operator runs beside Craft Cloud.

[docs/security.md](docs/security.md) describes what Craft Cloud protects and how, and what it does not protect.
