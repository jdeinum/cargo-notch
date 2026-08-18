use crate::error::{Error, Result};
use anyhow::Context;
use config::{Environment, File, FileFormat};
use git2::Repository;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Write, path::Path};
use tracing::warn;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub repo: RepoConfig,
    pub release: ReleaseConfig,
    pub bumps: BumpsConfig,
    pub tracking: TrackingConfig,
}

impl Config {
    pub(crate) fn write_to_default_file(&self) -> Result<()> {
        // if the file already exists, warn and return
        if std::path::Path::exists(Path::new("notch.toml")) {
            warn!("notch.toml already exists, not writing default!");
            return Ok(());
        }

        let mut f = std::fs::File::create(Path::new("notch.toml")).context("create notch.toml")?;

        let s = toml::to_string(self).context("convert config to toml")?;

        f.write_all(&s.into_bytes())
            .context("write config to file")?;

        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RepoConfig {
    /// Overrides the owner detected from the `origin` remote.
    pub owner: Option<String>,
    /// Overrides the repo name detected from the `origin` remote.
    pub name: Option<String>,
    /// Github token
    /// We opt to skip serializing this field, the only time we serialize the config is through
    /// init, and we dont have a token anyways. Deserializing must still work, though — it's how
    /// the `NOTCH__REPO__TOKEN` env var override reaches this field.
    #[serde(skip_serializing)]
    pub token: Option<SecretString>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

    // x..HEAD gets you anything present in head that cannot be reached from x
    #[must_use]
    pub fn commit_range(&self) -> String {
        format!("{}/{}..HEAD", self.remote, self.default_branch)
    }
}

/// How `--auto` versions crates still below 1.0.0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// Narrows which files count as a change to a package, on top of what the
/// [`Ecosystem`](crate::utils::packages::Ecosystem) already worked out from the workspace layout.
/// The ecosystem can subtract a nested package from the one containing it, because that's
/// derivable; it can't know that you don't want a `benches/` edit to cut a release. That's intent,
/// and it has to be stated.
///
/// Every pattern is a path **relative to the package's own directory**, not to the repo. That's
/// what lets `exclude = ["benches"]` mean "each package's own benches directory" without any glob
/// syntax, keeping matching a plain component-wise prefix test (see
/// [`PathSpec`](crate::utils::package::PathSpec)) rather than dragging in a glob matcher.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TrackingConfig {
    /// Applied to every package.
    pub exclude: Vec<String>,
    /// Puts a path back that a broader `exclude` covers — `exclude = ["tests"]` alongside
    /// `include = ["tests/compat"]`. Not an allowlist: a package tracks its own directory by
    /// default, so listing paths here narrows nothing on its own.
    pub include: Vec<String>,
    /// Applied to one package, keyed by the name its manifest declares. Adds to the lists above
    /// rather than replacing them — those are a baseline, and a per-package override that silently
    /// dropped them would be a trap.
    pub packages: HashMap<String, PackageTracking>,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            // notch writes this file itself during a bump. Its own commit is already filtered out
            // of attribution by `is_notch_commit`, but a hand-edited changelog is not, and
            // "editing the changelog releases a new version" is a surprise nobody wants.
            exclude: vec!["CHANGELOG.md".to_string()],
            include: Vec::new(),
            packages: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PackageTracking {
    pub exclude: Vec<String>,
    pub include: Vec<String>,
}

/// Loads config from `notch.toml` in `dir` (if present), then applies
/// `NOTCH__`-prefixed environment variable overrides, e.g.
/// `NOTCH__RELEASE__DEFAULT_BRANCH=main` overrides `[release] default_branch`.
pub fn load() -> Result<Config> {
    // check if the config exists, it it doesn't, we'll warn the user but supply the config default
    let notch_path = Path::new("notch.toml");

    let raw = config::Config::builder();

    let raw = if notch_path.exists() {
        raw.add_source(File::from(notch_path).format(FileFormat::Toml))
    } else {
        warn!("notch.toml does not exist, using the default!");
        // TODO: There is probably a way to skip serializing here and just pass the config object
        // directly
        let s = toml::to_string(&Config::default()).context("serialize default config")?;
        raw.add_source(File::from_str(s.as_str(), FileFormat::Toml))
    };
    let raw = raw
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
    use secrecy::ExposeSecret;

    // `load()` serializes `Config::default()` to TOML and reads it back whenever no `notch.toml`
    // exists, and `init` writes the same string to disk. TOML puts every table after every plain
    // value, so a config struct that mixes an array in among the tables — or a `[tracking]` table
    // whose own `exclude` array is declared after its `packages` sub-table — fails to serialize at
    // all, taking down every run without a config file.
    #[test]
    fn the_default_config_survives_a_toml_round_trip() {
        let serialized = toml::to_string(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&serialized).unwrap();

        assert_eq!(parsed.tracking.exclude, vec!["CHANGELOG.md".to_string()]);
        assert!(parsed.tracking.packages.is_empty());
    }

    #[test]
    fn per_package_tracking_deserializes_from_a_named_sub_table() {
        let parsed: TrackingConfig = toml::from_str(
            "exclude = [\"benches\"]\n\
             \n\
             [packages.crate-a]\n\
             exclude = [\"fixtures\"]\n",
        )
        .unwrap();

        assert_eq!(parsed.exclude, vec!["benches".to_string()]);
        assert_eq!(
            parsed.packages["crate-a"].exclude,
            vec!["fixtures".to_string()]
        );
    }

    #[test]
    fn an_include_deserializes_alongside_the_exclude_it_carves_back_out_of() {
        let parsed: TrackingConfig = toml::from_str(
            "exclude = [\"tests\"]\n\
             include = [\"tests/compat\"]\n",
        )
        .unwrap();

        assert_eq!(parsed.exclude, vec!["tests".to_string()]);
        assert_eq!(parsed.include, vec!["tests/compat".to_string()]);
    }

    // Regression test for a bug where `token` was `#[serde(skip)]`, which skips both directions —
    // it wasn't just kept out of the serialized `notch.toml` (the intent), it also meant `token`
    // could never be populated at all, including via the `NOTCH__REPO__TOKEN` env override that
    // `load()` relies on. `#[serde(skip_serializing)]` keeps the former without breaking the latter.
    #[test]
    fn repo_token_deserializes_even_though_it_is_not_serialized() {
        let config: RepoConfig = toml::from_str("token = \"abc123\"\n").unwrap();
        assert_eq!(
            config.token.unwrap().expose_secret(),
            "abc123",
            "token must still deserialize"
        );

        let config = RepoConfig {
            token: Some(SecretString::from("abc123")),
            ..RepoConfig::default()
        };
        let serialized = toml::to_string(&config).unwrap();
        assert!(
            !serialized.contains("token"),
            "token must not be written back out: {serialized}"
        );
    }

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
