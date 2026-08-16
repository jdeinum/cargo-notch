use crate::{config::Config, error::Result};
use anyhow::Context;
use std::{io::Write, path::PathBuf};
use tracing::warn;

pub struct NotchAction {
    val: String,
}

impl Default for NotchAction {
    fn default() -> Self {
        let val = r#"name: Tag crate versions

on:
  push:
    branches: [master]
  workflow_call:
    inputs:
      version:
        required: false
        type: string
        default: latest
        description: >-
          cargo-notch version to install (e.g. v0.1.25). Defaults to
          latest, which floats on whatever release GitHub currently marks
          as latest — pin this once you depend on stable tagging behavior.
    secrets:
      release_pat:
        required: false
        description: >-
          PAT with contents:write on the calling repo. Pushes made with the
          default GITHUB_TOKEN don't trigger other workflow runs, so callers
          that want the pushed tag to fire their own tag-triggered workflows
          need to pass a PAT here. Falls back to GITHUB_TOKEN when omitted.

permissions:
  contents: write

jobs:
  tag-versions:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0
          token: ${{ secrets.release_pat || secrets.GITHUB_TOKEN }}

      - uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo-notch binary
        id: notch-cache
        uses: actions/cache@v6
        with:
          path: ~/.cargo/bin/cargo-notch
          key: cargo-notch-${{ runner.os }}-${{ inputs.version }}-${{ github.sha }}

      - name: Install cargo-notch
        if: steps.notch-cache.outputs.cache-hit != 'true'
        shell: bash
        env:
          NOTCH_VERSION: ${{ inputs.version }}
        run: |
          if [ -z "$NOTCH_VERSION" ] || [ "$NOTCH_VERSION" = "latest" ]; then
            url="https://github.com/jdeinum/cargo-notch/releases/latest/download/cargo-notch-installer.sh"
          else
            url="https://github.com/jdeinum/cargo-notch/releases/download/$NOTCH_VERSION/cargo-notch-installer.sh"
          fi
          curl --proto '=https' --tlsv1.2 -LsSf "$url" | sh

      - name: Compute new tags
        run: |
          cargo notch tag \
            --old ${{ github.event.before }} \
            --new ${{ github.sha }} \
            > tags.txt
          echo "--- tags to create ---"
          cat tags.txt

      - name: Create and push tags
        run: |
          git config user.name "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"

          while IFS= read -r tag; do
            [ -z "$tag" ] && continue
            if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
              echo "tag $tag already exists, skipping"
              continue
            fi
            git tag -a "$tag" -m "Release $tag" ${{ github.sha }}
            git push origin "refs/tags/$tag"
          done < tags.txt
"#.to_string();
        Self { val }
    }
}

impl NotchAction {
    /// Writes the github actions file to the correct location
    pub(crate) fn write_to_default_file(&self) -> Result<()> {
        let workflow_dir = PathBuf::from(".github/workflows");
        let workflow_path = {
            let mut d = workflow_dir.clone();
            d.push("notch_tag.yaml");
            d
        };

        // if the file already exists, warn and return
        if std::path::Path::exists(&workflow_path) {
            warn!(
                "{} already exists, not writing default!",
                workflow_path.to_string_lossy()
            );
            return Ok(());
        }

        // otherwise we create the directory
        std::fs::create_dir_all(&workflow_dir).context("create workflow dir")?;

        // and the file
        let mut f = std::fs::File::create(&workflow_path).context("create notch github action")?;
        f.write_all(self.val.as_bytes())
            .context("write action bytes")?;

        Ok(())
    }
}

/// Creates the github action and the config file for the user according to the current defaults
pub fn run() -> Result<()> {
    // create the config
    let config = Config::default();
    config
        .write_to_default_file()
        .context("write default config")?;

    // create the action
    let action = NotchAction::default();
    action
        .write_to_default_file()
        .context("write notch action")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, V0Style};
    use std::sync::Mutex;

    // `run()`/`write_to_default_file()` both resolve hardcoded paths relative to the process's
    // current directory rather than taking one as an argument, so exercising them means actually
    // chdir-ing — which is process-global state. This lock keeps these tests from stepping on
    // each other when the test binary runs them concurrently; `CwdGuard` below restores the
    // original directory (even on panic) so it doesn't leak into whichever test runs next.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct CwdGuard {
        original: PathBuf,
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        fn enter(name: &str) -> Self {
            let lock = CWD_LOCK.lock().unwrap();
            let dir =
                std::env::temp_dir().join(format!("notch-init-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(&dir).unwrap();
            Self {
                original,
                dir,
                _lock: lock,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn init_on_fresh_repo_works() {
        let _guard = CwdGuard::enter("fresh-repo");

        run().unwrap();

        assert!(std::path::Path::new("notch.toml").exists());
        assert!(
            std::path::Path::new(".github/workflows/notch_tag.yaml").exists(),
            "workflow file should be created alongside the config"
        );
    }

    #[test]
    fn does_not_overwrite_config_if_exists() {
        let _guard = CwdGuard::enter("existing-config");

        std::fs::write("notch.toml", "# a user's own config\n").unwrap();

        Config::default().write_to_default_file().unwrap();

        assert_eq!(
            std::fs::read_to_string("notch.toml").unwrap(),
            "# a user's own config\n"
        );
    }

    #[test]
    fn does_not_overwrite_action_if_exists() {
        let _guard = CwdGuard::enter("existing-action");

        std::fs::create_dir_all(".github/workflows").unwrap();
        std::fs::write(
            ".github/workflows/notch_tag.yaml",
            "# a user's own workflow\n",
        )
        .unwrap();

        NotchAction::default().write_to_default_file().unwrap();

        assert_eq!(
            std::fs::read_to_string(".github/workflows/notch_tag.yaml").unwrap(),
            "# a user's own workflow\n"
        );
    }

    #[test]
    fn config_matches_expected_default() {
        let _guard = CwdGuard::enter("config-default");

        Config::default().write_to_default_file().unwrap();

        let written = std::fs::read_to_string("notch.toml").unwrap();
        let config: Config = toml::from_str(&written).unwrap();

        assert_eq!(config.repo.owner, None);
        assert_eq!(config.repo.name, None);
        assert_eq!(config.release.default_branch, "master");
        assert_eq!(config.release.remote, "origin");
        assert_eq!(config.release.tag_format, "{name}-v{version}");
        assert_eq!(config.bumps.v0, V0Style::Cargo);
        assert_eq!(config.bumps.major, Vec::<String>::new());
        assert_eq!(config.bumps.minor, vec!["feat".to_string()]);
        assert_eq!(
            config.bumps.patch,
            ["fix", "chore", "refactor", "docs"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(config.bumps.skip, Vec::<String>::new());
    }

    #[test]
    fn action_matches_expected_default() {
        let _guard = CwdGuard::enter("action-default");

        NotchAction::default().write_to_default_file().unwrap();

        let written = std::fs::read_to_string(".github/workflows/notch_tag.yaml").unwrap();
        assert_eq!(written, NotchAction::default().val);
    }
}
