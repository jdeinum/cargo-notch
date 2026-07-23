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
  release PR, and to authenticate git fetch/push when the remote is an HTTPS
  URL (SSH remotes authenticate through your ssh-agent instead). There's no
  CLI flag for this and it shouldn't go in `notch.toml` either, since that
  file is normally committed — set it via the `NOTCH__REPO__TOKEN`
  environment variable instead (see below).

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

## `[bumps]`

Used only by `cargo notch pr --auto` (available since v0.1.24), which skips
the interactive TUI and derives each changed crate's version bump from its
conventional commits: every attributed commit is mapped to a bump level and
the biggest one wins.

The `major`/`minor`/`patch`/`skip` lists hold patterns of two forms: a bare
type (`"chore"` — matches any scope) or a scoped type (`"chore(release)"` —
matches that exact scope only). When both forms match a commit, the scoped one
wins, so `patch = ["chore"]` plus `skip = ["chore(release)"]` gives every
chore a patch bump except release chores, which are skipped.

- `v0` (default: `"cargo"`) — how crates still below `1.0.0` are versioned:
  - `"cargo"` — cargo's interpretation of 0.x versions, where the leading zero
    shifts everything down: a breaking change bumps minor, everything else
    bumps patch.
  - `"semver"` — apply the mapped bump as-is, like any post-1.0 crate.
- `major` (default: `[]`) — patterns that map to a major bump. A breaking
  change always maps to major regardless of any list (even `skip`), whether
  declared via the header's `!` marker (`feat(api)!: …`) or a
  `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer in the commit body.
- `minor` (default: `["feat"]`) — patterns that map to a minor bump.
- `patch` (default: `["fix", "chore", "refactor", "docs"]`) — patterns that
  map to a patch bump. Any commit matching no list at all — including ones
  that aren't conventional commits — already falls back to a patch bump,
  since the crate did change; list a type here only to anchor a bare fallback
  for scoped overrides.
- `skip` (default: `[]`) — patterns whose commits contribute no bump. A crate
  whose every attributed commit is skipped is dropped from the release
  entirely — no version bump, no changelog entry, no PR section.

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

[bumps]
v0 = "cargo"
major = []
minor = ["feat"]
patch = ["fix", "chore", "refactor", "docs"]
skip = []
```
