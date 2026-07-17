use crate::error::{Error, Result};
use anyhow::Context;
use config::{Environment, File, FileFormat};
use git2::Repository;
use secrecy::SecretString;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub repo: RepoConfig,
    pub release: ReleaseConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    /// Overrides the owner detected from the `origin` remote.
    pub owner: Option<String>,
    /// Overrides the repo name detected from the `origin` remote.
    pub name: Option<String>,
    /// Github token
    pub token: Option<SecretString>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReleaseConfig {
    pub default_branch: String,
    pub remote: String,
    pub tag_format: String,
}

impl Default for ReleaseConfig {
    fn default() -> Self {
        Self {
            default_branch: "master".to_string(),
            remote: "origin".to_string(),
            tag_format: "{name}-v{version}".to_string(),
        }
    }
}

impl ReleaseConfig {
    #[must_use]
    #[allow(clippy::literal_string_with_formatting_args)]
    pub fn format_tag(&self, name: &str, version: &str) -> String {
        self.tag_format
            .replace("{name}", name)
            .replace("{version}", version)
    }

    #[must_use]
    pub fn commit_range(&self) -> String {
        format!("{}/{}..HEAD", self.remote, self.default_branch)
    }
}

/// Loads config from `notch.toml` in `dir` (if present), then applies
/// `NOTCH__`-prefixed environment variable overrides, e.g.
/// `NOTCH__RELEASE__DEFAULT_BRANCH=main` overrides `[release] default_branch`.
pub fn load(dir: &Path) -> Result<Config> {
    let raw = config::Config::builder()
        .add_source(
            File::from(dir.join("notch.toml"))
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(
            Environment::with_prefix("NOTCH")
                .prefix_separator("__")
                .separator("__"),
        )
        .build()
        .context("build notch config")?;

    raw.try_deserialize().context("parse notch config")
}

/// Resolves the GitHub owner/repo, preferring explicit `notch.toml` overrides
/// and falling back to parsing the `origin` remote's URL.
pub fn resolve_owner_repo(repo: &Repository, config: &RepoConfig) -> Result<(String, String)> {
    if let (Some(owner), Some(name)) = (&config.owner, &config.name) {
        return Ok((owner.clone(), name.clone()));
    }

    let remote = repo.find_remote("origin").context("find origin remote")?;
    let url = remote
        .url()
        .context("origin remote has no valid utf-8 url")?;
    let (detected_owner, detected_name) = parse_github_owner_repo(url)
        .ok_or_else(|| Error::msg(format!("could not parse owner/repo from remote url: {url}")))?;

    Ok((
        config.owner.clone().unwrap_or(detected_owner),
        config.name.clone().unwrap_or(detected_name),
    ))
}

/// Parses `owner/repo` out of a GitHub remote URL, handling both the SSH
/// (`git@github.com:owner/repo.git`) and HTTPS forms.
fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim_end_matches(".git");
    let path = trimmed
        .strip_prefix("git@github.com:")
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("https://github.com/"))
        .or_else(|| trimmed.strip_prefix("http://github.com/"))?;

    let (owner, name) = path.split_once('/')?;

    if owner.is_empty() || name.is_empty() {
        return None;
    }

    Some((owner.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_notch_toml_parses_to_defaults() {
        let config = load(Path::new(".")).expect("shipped notch.toml must load and parse");
        assert_eq!(config.repo.owner, None);
        assert_eq!(config.repo.name, None);
        assert_eq!(config.release.default_branch, "master");
        assert_eq!(config.release.remote, "origin");
        assert_eq!(config.release.tag_format, "v{version}");
    }

    #[test]
    fn parses_ssh_url() {
        assert_eq!(
            parse_github_owner_repo("git@github.com:jdeinum/notch.git"),
            Some(("jdeinum".to_string(), "notch".to_string()))
        );
    }

    #[test]
    fn parses_https_url() {
        assert_eq!(
            parse_github_owner_repo("https://github.com/jdeinum/notch"),
            Some(("jdeinum".to_string(), "notch".to_string()))
        );
    }

    #[test]
    fn rejects_unknown_host() {
        assert_eq!(
            parse_github_owner_repo("https://gitlab.com/jdeinum/notch"),
            None
        );
    }
}
