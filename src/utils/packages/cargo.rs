use crate::error::Result;
use crate::utils::package::Package;
use crate::utils::packages::traits::Packages;
use anyhow::Context;
use cargo_metadata::MetadataCommand;
use std::collections::HashSet;
use std::path::Path;
use tracing::debug;

pub struct CargoPackager {
    dir: String,
}

impl CargoPackager {
    pub const fn new(dir: String) -> Self {
        Self { dir }
    }
}

impl Packages for CargoPackager {
    fn get(&self) -> Result<HashSet<Package>> {
        let dir = Path::new(&self.dir);
        let metadata = MetadataCommand::new()
            .current_dir(dir)
            .exec()
            .context("run cargo metadata")?;
        let members = metadata.workspace_members;
        let packages = metadata.packages;
        debug!("Members: {members:?}");

        // Strip against the workspace root cargo itself reports, not the caller's
        // `dir`: cargo always emits member ids as absolute paths, so a relative or
        // non-canonical `dir` (like the "." run() passes) would never prefix-match
        // and every member would silently keep its absolute path.
        let root = metadata.workspace_root.as_str();

        // clean up the members
        let cleaned_members: HashSet<Package> = members
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
                    .strip_prefix(root)
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

        let members = CargoPackager::new(dir.to_str().unwrap().to_string()).get();
        fs::remove_dir_all(&dir).unwrap();
        let members: Vec<Package> = members.unwrap().into_iter().collect();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
        assert_eq!(members[0].name, "root-test-crate");
    }

    // `run()` invokes this with the relative dir "." — the caller's `dir` never
    // literally prefixes the absolute member paths cargo reports, so the
    // normalization must not depend on it. A non-canonical dir (`{tmp}/.`)
    // reproduces the same mismatch without having to chdir the test process.
    #[test]
    fn non_canonical_dir_still_normalizes_root_to_dot() {
        let dir = std::env::temp_dir().join(format!(
            "notch-package-test-relative-{}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"root-test-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let dot = dir.join(".");
        let members = CargoPackager::new(dot.to_str().unwrap().to_string()).get();
        fs::remove_dir_all(&dir).unwrap();
        let members: Vec<Package> = members.unwrap().into_iter().collect();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
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

        let members = CargoPackager::new(dir.to_str().unwrap().to_string()).get();
        fs::remove_dir_all(&dir).unwrap();
        let members: Vec<Package> = members.unwrap().into_iter().collect();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, "crate-a");
        assert_eq!(members[0].name, "crate-a");
    }
}
