use crate::error::Result;
use crate::pr::traits::{Package, Packages};
use anyhow::Context;
use cargo_metadata::MetadataCommand;
use std::collections::HashSet;
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
    fn get(&self) -> Result<HashSet<super::traits::Package>> {
        let metadata = MetadataCommand::new()
            .current_dir(&self.dir)
            .exec()
            .context("run cargo metadata")?;
        let members = metadata.workspace_members;
        let packages = metadata.packages;
        debug!("Members: {members:?}");

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
                    .strip_prefix(&self.dir)
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
