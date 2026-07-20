use crate::error::Result;
use anyhow::Context;
use cargo_metadata::{MetadataCommand, semver::Version};
use std::path::Path;
use tracing::debug;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Package {
    /// Workspace-relative directory containing the package's `Cargo.toml`.
    pub path: String,
    /// The package's actual `[package] name` from its `Cargo.toml`.
    pub name: String,
    pub version: Version,
}

impl Package {
    pub fn bump_patch(&self) -> Version {
        let mut new = self.version.clone();
        new.patch += 1;
        new
    }

    pub fn bump_minor(&self) -> Version {
        let mut new = self.version.clone();
        new.minor += 1;
        new
    }

    pub fn bump_major(&self) -> Version {
        let mut new = self.version.clone();
        new.major += 1;
        new
    }
}

// get the list of packages in the workspace rooted at `dir`
pub fn get_cleaned_members(dir: &Path) -> Result<Vec<Package>> {
    let metadata = MetadataCommand::new()
        .current_dir(dir)
        .exec()
        .context("run cargo metadata")?;
    let members = metadata.workspace_members;
    let packages = metadata.packages;
    debug!("Members: {members:?}");

    let dir_str = dir.to_str().unwrap();

    // clean up the members
    let cleaned_members: Vec<Package> = members
        .iter()
        .map(|s| {
            let raw_path = s
                .repr
                .replace("path+file://", "")
                .split('#')
                .next()
                .unwrap()
                .to_string();

            // strip the workspace root prefix to get a repo-relative path;
            // a member whose manifest sits at the root itself (e.g. a
            // single, non-workspace crate) has no trailing slash to strip
            // against, so it's normalized to "." rather than left as an
            // absolute path (which would never prefix-match the relative
            // paths in a git diff)
            let relative = raw_path
                .strip_prefix(dir_str)
                .map_or(raw_path.as_str(), |rest| rest.trim_start_matches('/'));
            let path = if relative.is_empty() { "." } else { relative }.to_string();

            let package = packages.iter().find(|p| p.id == *s).unwrap();
            Package {
                path,
                name: package.name.to_string(),
                version: package.version.clone(),
            }
        })
        .collect();

    debug!("cleaned members: {cleaned_members:?}");
    Ok(cleaned_members)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // A package whose manifest sits at the workspace root (e.g. a single,
    // non-workspace crate) has a `cargo_metadata` repr with no trailing
    // slash before the `#` anchor, so naively stripping "{dir}/" leaves the
    // raw absolute path untouched. That absolute path can never prefix-match
    // the repo-relative paths in a git diff, so changed-package detection
    // silently found nothing for repos shaped like this one.
    #[test]
    fn root_crate_path_is_normalized_to_dot() {
        let dir = std::env::temp_dir().join(format!("notch-package-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"root-test-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let members = get_cleaned_members(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
        assert_eq!(members[0].name, "root-test-crate");
    }

    // A workspace member nested under `dir` (rather than sitting at the root
    // itself) should resolve to a path relative to `dir`, not the raw
    // absolute manifest path `cargo_metadata` reports.
    #[test]
    fn nested_workspace_member_path_is_relative_to_dir() {
        let dir =
            std::env::temp_dir().join(format!("notch-package-test-nested-{}", std::process::id()));
        fs::create_dir_all(dir.join("crate-a/src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate-a\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nversion = \"0.3.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("crate-a/src/lib.rs"), "").unwrap();

        let members = get_cleaned_members(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, "crate-a");
        assert_eq!(members[0].name, "crate-a");
    }
}
