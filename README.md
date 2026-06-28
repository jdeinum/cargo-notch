# Notch

Notch is designed to be an ultra simple build tool that versions against a
ground source branch for your repository. The goal is to provide a simple
interface that allows you to version your new releases, generate a
[changelog](https://git-cliff.org/), open a PR, and create a git tags for new
releases.

If you have ideas, please create an issue!

## Roadmap

NOTE: I am still working on the alpha, please do not use this yet!

✅ Initial prototype - [`39d5eea`](https://github.com/jdeinum/notch/commit/39d5eea1943a79ad88419e876f41917d15ed906f)
☑️ Github action that creates tags from merged PRs matching specs
☑️ CI/CD stuff
☑️ Move hardcoded stuff to TOML config
☑️ Working CLI
☑️ Working TUI
☑️ Release v1.0.0
☑️ Auto versioning option
☑️ Auto merge option

## Example

```bash
# create a feature branch
git checkout -b feature/add_two

# do some stuff...
# nvim src/main.rs

# commit
git commit -m "feat: added the add_two function"

# run cargo notch
cargo notch

# merge PR on github or from cli (not required if auto-merge is used)
gh pr merge <pr_number>

# notch gha notices the PR, sees version differences, creates tags

# build happens as normal
```

## Inspiration

Notch takes heavy inspiration from tools like
[release-plz](https://github.com/release-plz/release-plz), but rather than
versioning against git tags or crates.io, it versions exclusively against your
production branch (likely `origin/master` or `origin/main`). Additionally, it
keeps version management as a manual step, making this usable even if you are
not using conventional commits.

This allows the codebase to remain super simple, as all we need to do is compare
our current HEAD against what's on the production branch.

The tradeoff is that you need to meet certain criteria to get value out of
notch. First, you need to have a branch that always has the up to date, working
version of the software. This means following the [not rocket science
rule](https://matklad.github.io/2024/03/22/basic-things.html#Not-Rocket-Science-Rule),
which is often implemented through the following items:

1. No direct push to your production branch, i.e PRs to update it
2. PR branches required to be up to date to merch
3. PR branches have status checks pass to merge (i.e fmt, tests, etc)
4. Disable force pushes
5. Branch off of origin/master rather than local master

Some other considerations is that notch does not strive to verify your public
API changes, nor does it automatically decide your next version, instead it
shows you all of the commits included, grouped by type, and lets you decide.

## Installation

See [INSTALL.md](./INSTALL.md)

## Configuration

See [CONFIGURATION.md](./CONFIGURATION.md)

## Changelog

See [CHANGELOG.md](./crates/notch/CHANGELOG.md)
