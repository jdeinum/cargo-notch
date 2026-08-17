# cargo-notch

[![Current tag](https://img.shields.io/github/v/tag/jdeinum/cargo-notch?style=for-the-badge&logo=semanticrelease&color=C9CBFF&logoColor=D9E0EE&labelColor=302D41&sort=semver)](https://github.com/jdeinum/cargo-notch/tags)
[![Check](https://img.shields.io/github/actions/workflow/status/jdeinum/cargo-notch/check.yaml?style=for-the-badge&label=Check&logo=githubactions&logoColor=D9E0EE&labelColor=302D41)](https://github.com/jdeinum/cargo-notch/actions/workflows/check.yaml)
[![Test](https://img.shields.io/github/actions/workflow/status/jdeinum/cargo-notch/test.yaml?style=for-the-badge&label=Test&logo=githubactions&logoColor=D9E0EE&labelColor=302D41)](https://github.com/jdeinum/cargo-notch/actions/workflows/test.yaml)
[![Audit](https://img.shields.io/github/actions/workflow/status/jdeinum/cargo-notch/audit.yaml?style=for-the-badge&label=Audit&logo=githubactions&logoColor=D9E0EE&labelColor=302D41)](https://github.com/jdeinum/cargo-notch/actions/workflows/audit.yaml)

Notch is designed to be an ultra simple build tool for rust that versions
against a ground source branch for your repository. The goal is to provide a
simple interface that allows you to version your new releases, generate a
[changelog](https://git-cliff.org/), open a PR, and create git tags for new
releases.

If you have ideas, please create an issue!

## Contents

- [Who is Notch for?](#who-is-notch-for)
- [Installation](#installation)
- [Usage](#usage)
- [Example](#example)
- [Inspiration](#inspiration)
- [Development](#development)
- [Configuration](#configuration)
- [Changelog](#changelog)

## Who is Notch for?

Notch is ideal for teams already practicing trunk based development that
want to retain control of deciding version bumps manually (with an optional
conventional-commit based auto mode). To have notch
automatically start builds, you also need a way of calling notch with the
current HEAD and previous HEAD so it can find which packages in your project
actually changed.

I built Notch to help speed up my builds for Annona, a service based project
where most workspace members end up as a docker image to be consumed by
downstream consumers.

## Installation

See [INSTALL.md](./INSTALL.md)

## Usage

Every command is invoked as `cargo notch <command>`. `-v`/`--verbose` is a
flag on `notch` itself rather than on the individual commands, so it goes
*before* the command name (`cargo notch --verbose pr`, not
`cargo notch pr --verbose`) and turns on debug logging for whichever command
runs.

```bash
# list all commands and global flags
cargo notch --help

# list the flags for one command
cargo notch pr --help
```

### `cargo notch init`

Scaffolds a repo for notch: writes a default `notch.toml` and a
`.github/workflows/notch_tag.yaml` workflow that runs `cargo notch tag` on
every push to your default branch. Never overwrites either file if it's
already there, so it's safe to re-run.

### `cargo notch commit`

Bumps the version and updates the changelog for every package with attributed
commits, then commits the result locally — no push, no PR. If the branch
already has a prior notch commit, it's dropped first so re-running always
starts from a clean slate.

By default this opens an interactive TUI, preselecting each package's
suggested bump so you can confirm or override it. Pass `--auto` to skip the
TUI entirely and derive every bump from conventional commits instead (see
`[bumps]` in [CONFIGURATION.md](./CONFIGURATION.md)).

### `cargo notch pr`

Does everything `cargo notch commit` does, then pushes the branch and opens a
release PR on GitHub. Requires a GitHub token — see
[CONFIGURATION.md](./CONFIGURATION.md) for how to set
`NOTCH__REPO__TOKEN`. Also accepts `--auto`.

### `cargo notch tag`

Takes `--old <commit>` and `--new <commit>`, diffs workspace member versions
between the two, and prints (one per line) the tags that should be created
for whichever packages changed. This is what the generated GitHub Action runs
after a release PR merges — you won't normally call it by hand.

## Example

```bash
git checkout -b feature/add_two

# ...make your changes...
# nvim src/main.rs

# commit your changes
git commit -m "feat: added the add_two function"

# bump versions, update changelogs, and open a release PR for changed crates
# (the token is read from config/env, not passed on the command line — see CONFIGURATION.md)
NOTCH__REPO__TOKEN=<github-token> cargo notch pr

# merge the PR on github or from the cli
gh pr merge <pr_number>

# the generated GitHub Action notices the merge, diffs versions, and pushes
# tags for whatever changed — your build triggers off those tags as usual
```

## Inspiration

Notch takes heavy inspiration from tools like
[release-plz](https://github.com/release-plz/release-plz), but rather than
versioning against git tags or crates.io, it versions exclusively against your
production branch (likely `origin/master` or `origin/main`). Additionally, it
keeps version management as a manual step by default, making this usable even
if you are not using conventional commits. If you do use conventional commits,
`cargo notch pr --auto` (added in v0.1.24) can derive the bumps for you.

This allows the codebase to remain super simple, as all we need to do is compare
our current HEAD against what's on the production branch.

The tradeoff is that you need to meet certain criteria to get value out of
notch. First, you need to have a branch that always has the up to date, working
version of the software. This means following the [not rocket science
rule](https://matklad.github.io/2024/03/22/basic-things.html#Not-Rocket-Science-Rule),
which is often implemented through the following items:

1. No direct push to your production branch, i.e. PRs to update it
2. PR branches required to be up to date to merge
3. PR branches have status checks pass to merge (i.e. fmt, tests, etc)
4. Disable force pushes
5. Branch off of origin/master rather than local master

Some other considerations are that notch does not strive to verify your public
API changes. By default it also doesn't decide your next version — it shows
you all of the commits included, grouped by type, and lets you decide. Since
v0.1.24 you can opt into automatic bumps with `cargo notch pr --auto`, which
maps conventional commits to bump levels via the `[bumps]` config section.

## Development

```sh
# Install git hooks (run once after cloning)
git config core.hooksPath .githooks
```

The pre-push hook runs `cargo fmt --check`, `cargo clippy`, and `cargo deny
check` before every push.

## Configuration

See [CONFIGURATION.md](./CONFIGURATION.md)

## Changelog

See [CHANGELOG.md](./CHANGELOG.md)
