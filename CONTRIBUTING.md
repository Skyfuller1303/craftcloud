# Contributing

Thank you for wanting to improve Craft Cloud. This page is short on purpose. The rules themselves are in [`AGENTS.md`](AGENTS.md), which applies to people and to coding agents alike; read it first.

## Before you start

- Look for an existing issue or pull request on the same subject, here and in the original repository, [itrycodinganditsbad/craft-cloud](https://github.com/itrycodinganditsbad/craft-cloud). If someone is already working on it, say so there instead of starting a second attempt.
- For anything larger than a fix, open an issue first and describe what you want to change and why.
- A security problem is reported privately: see [`SECURITY.md`](SECURITY.md).

## Making a change

1. Work on a branch of your own. The main branch changes only through pull requests.
2. Keep the change narrow: fix what the issue describes, where the fault is. A fix comes with a test that fails without it.
3. Follow the golden rules in `AGENTS.md`: no panics and no `unsafe` in non-test code, nothing a user stored is lost, everything from the network is treated as hostile, and no secret or personal detail in code, logs, tests or requests.
4. Follow the clean room: learn from public documentation and observed behaviour, never copy code, assets or wording from another product, and check the licence of a dependency before adding it. `cargo xtask licenses` checks every crate in the build.
5. Every new file that is not code (an image, a font, a test fixture) gets a row in [`ATTRIBUTION.md`](ATTRIBUTION.md) in the same change.
6. Do not change the CI workflow in a pull request that adds a feature or fixes a bug.

## Before you open a pull request

Run the gate, and say in the pull request what you ran and what you could not run:

```sh
cargo xtask ci    # fmt --check, licences, icons --check, clippy with warnings as errors, the tests
```

[docs/development.md](docs/development.md) explains how to build and test single crates, the back-end tests that need Postgres, S3 or SFTP, and how the screenshot tests of the app are refreshed.

Read your own diff once more before you open the pull request: every name exists as you use it, nothing unrelated changed, and the description is true.

## Credit and licence

Your contribution is licensed under MIT or Apache-2.0, at your option, like the rest of the repository.

Craft Cloud was created by Fabio Lussi. This fork keeps his history: branches are merged, never squashed or rewritten in a way that would remove an author. A commit that builds on the original author's code names him as co-author with the line

```text
Co-authored-by: Fabio Lussi <198260595+itrycodinganditsbad@users.noreply.github.com>
```

A pull request does not say that a coding agent wrote it, and commits carry no agent attribution. The person who opens the pull request answers for it.
