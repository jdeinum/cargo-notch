use crate::error::{Error, Result};
use anyhow::Context;
use cargo_metadata::semver::Version;
use serde::Deserialize;

// determine the tag to be created
pub fn tag(old: Option<Version>, new: Option<Version>) -> Result<Option<Version>> {
    match (old, new) {
        // if there is no old version, but a new version, return
        (None, Some(n)) => Ok(Some(n)),

        // if there is both an old and new version, and the new is newer than the old, return new
        (Some(o), Some(n)) if o < n => Ok(Some(n)),

        // if there is both an old and new version, but the older version is higher than the new
        // one, we return an error for that
        (Some(o), Some(n)) if o < n => Err(Error::msg("New version is older than the old version")),

        // otherwise, we don't release anything
        _ => Ok(None),
    }
}

#[derive(Deserialize)]
pub struct Manifest {
    version: String,
}

pub fn parse_version(v: String) -> Result<Version> {
    let x: Manifest = toml::from_str(&v).context("parse manifest")?;
    let version: Version = Version::try_from(x.version).context("parse version")?;
    Ok(version)
}
