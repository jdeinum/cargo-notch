# Installation

## Step 1: Install Binary

### Install from Release

Run

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jdeinum/cargo-notch/releases/download/latest/cargo-notch-installer.sh | sh
```

### Install from Source (requires cargo)

```bash
cargo install --git https://github.com/jdeinum/cargo-notch cargo-notch
```

## Step 2: Initialize Notch

```bash
cargo notch init
```

## Step 3: Create the required PAT

1. Go to repository
2. -> Settings
3. -> Secrets and Variables
4. -> Actions
5. Create RELEASE_PAT secret, setting it to a valid github token

## Step 4: Use!

That's it, see the example in the [README](/README.md) on usage.
