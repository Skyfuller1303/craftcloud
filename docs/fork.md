# About this fork

This repository, [Skyfuller1303/craftcloud](https://github.com/Skyfuller1303/craftcloud), is a maintained fork of [itrycodinganditsbad/craft-cloud](https://github.com/itrycodinganditsbad/craft-cloud). This page says where the code comes from, how the fork relates to the original, which rules it follows, and what would have to change if it were ever proposed to the organisation that maintains the Craft apps.

## Where the code comes from

Craft Cloud was created by Fabio Lussi ([itrycodinganditsbad](https://github.com/itrycodinganditsbad)). The original repository holds the design and the first implementation: the sync engine, the server, the agent, the Craft Cloud app, live documents with presence, and the app kit. The co-working features of this fork are built on that work.

The fork adds what was written afterwards and proposed to the original repository as pull requests: the admin API and the admin pages, the audit log, two-factor sign-in, trash, history and usage, storage tools, the SFTP blob store and backups, the Docker deployment, the redesigned screens of the app, team spaces, and live documents in team spaces.

The history of the original is kept. The fork's main line is built by merging onto the original `main`, never by squashing or rewriting, so every original commit keeps its author. Commits in the fork that build on the original author's code name him as co-author.

## Relation to the original repository

- The original stays a remote of this repository. Changes are still proposed there as pull requests, so that the original can take them whenever its maintainer wishes.
- The fork does not claim to be the original and makes no statement on its maintainer's behalf.
- The licence is unchanged: MIT or Apache-2.0, at your option. The copyright lines in `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE` are kept as they are; `NOTICE` adds one line that names the fork.

## The predecessor in this repository

Before the fork line was imported, this repository held an earlier prototype with the same aim and a different design: one server image beside PostgreSQL instead of the sync engine, agent and app described here. It is kept unchanged under the tag [`archive/prototype-2026-11-03`](https://github.com/Skyfuller1303/craftcloud/tree/archive/prototype-2026-11-03), and its commits remain in the history of `main`, because the import was a merge.

The prototype had parts that the current code does not have. They are listed here so that nobody assumes they still exist:

- organisations and roles within them
- single sign-on and SCIM provisioning
- e-mail (invitations and notifications sent by the server)
- shares and file locks
- seats
- storage tiers
- the setup wizard
- the admin interface written in React

The current code covers some of the same ground differently: accounts with member and admin roles, team spaces with owner, editor and viewer roles, invitations as join strings, and server-rendered admin pages. See [the architecture](architecture.md) for what exists now.

## Independence and names

Craft Cloud is an independent project built for the Craft App Suite. It is not made, sponsored or endorsed by the team that makes the suite, and it is not part of the storytold organisation. The product and its documents use no name, logo or mark of that team as branding; the only reference is the plain statement of what the software is built for.

The Craft apps each carry a `docs/brand/LICENSE-brand.txt` that explains the rules for their marks. Craft Cloud has no such file and needs none, because it ships none of those marks. If a later version ever bundled one, that file and the permission behind it would have to come first.

## The house rules

The top of [`AGENTS.md`](../AGENTS.md) carries two generated blocks, the core house rules and the Rust house rules, copied from `storytold/craft-contrib` and pinned to a commit of that repository with a SHA-256 digest of the text. They replace the references to `craftrules`, which is retired. The blocks are not edited here.

Craft Cloud takes these rules as its quality bar although it is not bound by them. It follows them with the exceptions below. Each exception is a fact about the repository today, not a claim that the rule does not matter.

| House rule | What Craft Cloud does | Why |
|---|---|---|
| Core 8: `main` changes only through a pull request, enforced by branch protection. | The rule is followed as a working rule. Branch protection on the fork's `main` is a repository setting that the maintainer has to turn on. | Settings are not part of the source. |
| Core 11: CI calls the shared workflows of `storytold/craft-common-actions`. | The repository has its own workflow, `.github/workflows/ci.yml`: formatting, licences, clippy with warnings as errors, the tests on Linux, Windows and macOS, the Postgres, S3 and SFTP back ends, the minimum Rust version and the shell tests of the Docker files. | An independent repository cannot rely on another organisation's workflows. See "If Craft Cloud were proposed to the organisation" below. |
| Rust 3: font files live in `storytold/craft-fonts`, not in the app repository. | The Craft Cloud app bundles Archivo, Inter and Geist Mono (OFL-1.1, unmodified) in `apps/craft-cloud-desktop/assets/fonts/`, each with its licence text and a row in `ATTRIBUTION.md`. | The app must build without a dependency on another organisation's repository. |
| Licence allowlist: a repository may shorten the list, not add to it. | `cargo xtask licenses` allows, beyond the list, `Zlib`, `Unicode-DFS-2016` and, for named crates only, `MPL-2.0` (`im`, `bitmaps`, `sized-chunks`, which the Loro CRDT needs), `BSL-1.0` (`xxhash-rust`, `clipboard-win`, `error-code`) and `CDLA-Permissive-2.0` (root certificate data in `webpki-roots` and `webpki-root-certs`). | Loro and the window toolkit bring these crates. They are used as published and never modified. Section 2 of `AGENTS.md` and `xtask/src/licenses.rs` name every exception. |
| Rust 6: contributor credits only through `contributors/people.toml`. | The repository has no such file. The original author is credited in the README, in this page and in the architecture document. | The credit is owed to the author of the original work and is a condition of maintaining this fork. |
| Core 15: a pull request does not say that an agent wrote it. | Followed for new work. Some commits of the original history carry an agent co-author trailer; they are kept as they are because history is not rewritten. | Rewriting history would remove the original authorship. |

The rules that Craft Cloud follows without exception include never crash (the `deny` attribute on every production crate root and on `xtask`, `unsafe_code = "forbid"`), the clean room, attribution of every asset in the same change, Rust only, and no secrets or personal details in the repository, logs or requests.

Rust 4 forbids handwritten JavaScript and webview shells in native apps. The Craft Cloud app is native egui. The server's admin pages are HTML rendered by the server in Rust and contain no script.

## The name `craft-cloud`

The suite's plan for cloud documents (RFC-0024 in `storytold/craft-contrib`) names a vendor-neutral contract crate `craft-cloud` with a trait `CloudBackend`. Apps are to depend on that contract and never on one provider. That crate is proposed for the shared library repository and is not this project.

The crates here are `craft-cloud-api`, `craft-cloud-proto`, `craft-cloud-app`, `craft-cloud-kit`, `craft-cloud-egui` and so on. The shared prefix invites confusion, and a crate named `craft-cloud` in a dependency list would say nothing about which of the two is meant.

The proposed way out keeps both names and makes the roles clear:

- The contract crate keeps the name `craft-cloud`. Apps depend on it only.
- Craft Cloud becomes one provider behind that contract: an adapter crate that implements `CloudBackend` by calling the agent's local API through `craft-cloud-app`. A name such as `craft-cloud-provider` says what it is.
- The crates of this repository keep their names. None of them is ever published or imported as plain `craft-cloud`.

[integration.md](integration.md) describes how far the present interfaces are from that adapter. The adapter does not exist yet.

## If Craft Cloud were proposed to the organisation

Craft Cloud stays independent for now. Joining the organisation is the maintainers' decision and would go through their process for a new repository or app, a written proposal with a comment period. Nothing in this repository assumes that outcome. For the record, these are the things that would have to change or be decided:

1. CI would call the shared workflows (the house-rules check of the two blocks in `AGENTS.md`, the check of git dependency sources, the gate that runs `cargo xtask ci`), and feature pull requests would stop changing CI.
2. The licence exceptions above would need the maintainers' approval, or the dependencies would have to go.
3. The bundled fonts would move to the shared fonts repository.
4. The account model would have to be reconciled with the suite's plan for accounts and tokens (see [integration.md](integration.md#how-this-fits-the-suites-plans)).
5. The provider adapter described above would have to exist and pass the contract's acceptance tests, among them the refusal to overwrite a newer remote version.
6. The user guide in [`docs/user/`](user/overview.md) would move to the suite's documentation site. It already has the five pages that site asks for.
7. Releases would follow the suite's release workflow instead of `cargo xtask package` run by hand.
