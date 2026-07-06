use crate::error::Result;
use anyhow::Context;
use cargo_metadata::{MetadataCommand, semver::Version};
use std::path::Path;
use tracing::info;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Crate {
    pub name: String,
    pub version: MyVersion,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct MyVersion {
    pub version: Version,
}

impl MyVersion {
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

// get the list of crates in the workspace rooted at `dir`
pub fn get_cleaned_members(dir: &Path) -> Result<Vec<Crate>> {
    let metadata = MetadataCommand::new()
        .current_dir(dir)
        .exec()
        .context("run cargo metadata")?;
    let members = metadata.workspace_members;
    let packages = metadata.packages;
    info!("Members: {members:?}");

    // clean up the members
    let cleaned_members: Vec<Crate> = members
        .iter()
        .map(|s| {
            let x: String = s
                .repr
                .replace("path+file://", "")
                .replace(&format!("{}/", dir.to_str().unwrap()), "")
                .split('#')
                .next()
                .unwrap()
                .to_string();

            let v = packages
                .iter()
                .find(|p| p.id == *s)
                .unwrap()
                .version
                .clone();
            Crate {
                name: x,
                version: MyVersion { version: v },
            }
        })
        .collect();

    info!("cleaned members: {cleaned_members:?}");
    Ok(cleaned_members)
}
