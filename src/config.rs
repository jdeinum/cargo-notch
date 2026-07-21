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
    pub bumps: BumpsConfig,
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

/// How `--auto` versions crates still below 1.0.0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum V0Style {
    /// Cargo's interpretation of 0.x versions: a breaking change bumps
    /// minor, everything else bumps patch.
    #[default]
    Cargo,
    /// Apply the mapped bump as-is, like any post-1.0 crate.
    Semver,
}

/// Maps conventional commits to bump levels for `cargo notch pr --auto`.
/// Each list holds patterns of the form `type` (any scope) or `type(scope)`
/// (that scope only); a scoped pattern beats a bare-type one. A breaking
/// change (`!` header marker or `BREAKING CHANGE:` footer) always means a
/// major bump, commits matching `skip` contribute no bump at all, and
/// commits matching nothing fall back to patch.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BumpsConfig {
    pub v0: V0Style,
    pub major: Vec<String>,
    pub minor: Vec<String>,
    pub patch: Vec<String>,
    pub skip: Vec<String>,
}

impl Default for BumpsConfig {
    fn default() -> Self {
        Self {
            v0: V0Style::default(),
            major: Vec::new(),
            minor: vec!["feat".to_string()],
            patch: ["fix", "chore", "refactor", "docs"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            skip: Vec::new(),
        }
    }
}

/// Loads config from `notch.toml` in `dir` (if present), then applies
/// `NOTCH__`-prefixed environment variable overrides, e.g.
/// `NOTCH__RELEASE__DEFAULT_BRANCH=main` overrides `[release] default_branch`.
pub fn load() -> Result<Config> {
    let raw = config::Config::builder()
        .add_source(
            File::from(Path::new("notch.toml"))
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
        let config = Config::default();
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
