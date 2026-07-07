# Build Tool

## Configuration

`notch.toml` in the repo root is optional; every field has a default matching
the previous hardcoded behavior. Only set what you need to override:

```toml
[repo]
# owner = "..."   # defaults to the owner parsed from the origin remote's URL
# name = "..."    # defaults to the repo name parsed from the origin remote's URL

[release]
default_branch = "master"          # branch releases are diffed/opened against
remote = "origin"
tag_format = "{name}-v{version}"   # {name} is the Cargo package name, not the workspace directory
```

## Assumptions

`<remote>/<default_branch>` (`origin/master` by default) ALWAYS contains the
up to date working code. This is the thing we diff against.

This means that we need to have the following enabled in github for the repo

- branch must be up to date in order to merge
- no force push
- status checks pass

The files that change tell us which crate within the workspace needs to have
their version bumped, while the commits we have locally and not on origin will
be used to build the changelog, which we can edit before commiting to the changelog.

## General Flow

1. Get the list of all commits from HEAD to origin/master
2. Find what services changed in those commits
3. Show you the list of commits, the suggested bump version, and the ability to
   override it
4. Generate a changelog for that service for that new version
5. Allow you to edit the changelog
6. Commit the changelog and version bumps to the current branch
