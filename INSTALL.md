# Installation

Install `cargo-notch` directly from git:

```bash
cargo install --git https://github.com/jdeinum/notch cargo-notch
```

This installs the `cargo notch` subcommand, which you can then run from within
any repository:

```bash
cargo notch pr --token <github-token>
```

## CI: creating tags

`cargo notch pr` only bumps versions, updates changelogs, and opens a release
PR — it doesn't create tags. That part runs in CI, after the release PR
merges, via a reusable workflow hosted in this repo
(`.github/workflows/tag.yaml`). Call it from your own repo instead of copying
the file:

```yaml
# .github/workflows/tag.yaml
name: Tag crate versions

on:
  push:
    branches: [master]

permissions:
  contents: write

jobs:
  tag:
    uses: jdeinum/notch/.github/workflows/tag.yaml@master
    permissions:
      contents: write
    secrets:
      release_pat: ${{ secrets.RELEASE_PAT }}
```

Notes:

- `@master` floats on notch's `master` branch. Once notch has tagged releases
  you trust, pin to one instead (e.g. `@cargo-notch-v0.1.12`) so a notch
  change can't silently alter your CI.
- `release_pat` must be a PAT (not the default `GITHUB_TOKEN`) with
  `contents: write` on your repo, added as a secret named `RELEASE_PAT`.
  This is required, not optional, if you have anything downstream that
  triggers on tag pushes (e.g. a build workflow on `push: tags:`) — GitHub
  does not let pushes authenticated with the default `GITHUB_TOKEN` trigger
  other workflow runs, so without a real PAT here the tags will be created
  but your tag-triggered workflow will never fire.
