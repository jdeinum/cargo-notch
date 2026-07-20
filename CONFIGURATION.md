# Configuration

`cargo-notch` reads an optional `notch.toml` from the repository root. Every
field has a default matching the tool's built-in behavior, so the file itself
is optional — add one only to override what you need.

## `[repo]`

- `owner` (optional) — GitHub owner/org the PR is opened against. Defaults to
  the owner parsed from the `origin` remote's URL.
- `name` (optional) — GitHub repository name the PR is opened against.
  Defaults to the repo name parsed from the `origin` remote's URL.
- `token` (required for `cargo notch pr`) — GitHub token used to open the
  release PR. There's no CLI flag for this and it shouldn't go in `notch.toml`
  either, since that file is normally committed — set it via the
  `NOTCH__REPO__TOKEN` environment variable instead (see below).

Only set `owner`/`name` when `origin` doesn't point at the GitHub repo you
actually want (e.g. a fork or a mirror). Both SSH
(`git@github.com:owner/repo.git`) and HTTPS (`https://github.com/owner/repo`)
remote URLs are understood.

## `[release]`

- `default_branch` (default: `"master"`) — the branch releases are diffed
  against and PRs are opened into.
- `remote` (default: `"origin"`) — the git remote used for diffing and
  pushing.
- `tag_format` (default: `"{name}-v{version}"`) — template for generated tags.
  `{name}` is the crate's actual Cargo package name (the `name` under
  `[package]` in its `Cargo.toml`) — **not** the workspace-relative directory
  it lives in. These can differ (a crate at `services/user` might be named
  `user_service`), and anything downstream that matches on the tag (e.g. a
  Docker build workflow triggering on a `*_service-v*` glob) needs the real
  package name to line up.

## Environment variable overrides

Every field can also be set (or overridden) with a `NOTCH__`-prefixed
environment variable, using `__` to separate the section from the key, e.g.
`NOTCH__RELEASE__DEFAULT_BRANCH=main` overrides `[release] default_branch`.
This is the only way to set `repo.token`.

## Example

```toml
[repo]
# owner = "my-org"
# name = "my-repo"
# token is a secret — set it via NOTCH__REPO__TOKEN instead of here

[release]
default_branch = "master"
remote = "origin"
tag_format = "{name}-v{version}"
```
