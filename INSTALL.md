# Installation

## Step 1: Install Binary

### Install from Release

Run

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jdeinum/cargo-notch/releases/latest/download/cargo-notch-installer.sh | sh
```

### Install from Source (requires cargo)

```bash
cargo install --git https://github.com/jdeinum/cargo-notch cargo-notch
```

## Step 2: Initialize Notch

```bash
cargo notch init
```

## Step 3 (optional): Create a PAT

The tagging workflow falls back to the default `GITHUB_TOKEN` when no PAT is
provided. However, tags pushed with `GITHUB_TOKEN` won't trigger other
workflows — if you have tag-triggered workflows (e.g. Docker builds), create a
PAT with `contents: write` on the repo:

1. Go to repository
2. -> Settings
3. -> Secrets and Variables
4. -> Actions
5. Create a `RELEASE_PAT` secret, setting it to the PAT

## Step 4: Use!

That's it, see the example in the [README](/README.md) on usage.
