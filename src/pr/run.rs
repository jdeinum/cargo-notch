use crate::cmd::run_command;
use crate::config;
use crate::error::{Error, Result};
use crate::package::Package;
use crate::pr::assign::{WorktreeCommitAssigner, prior_bump_sections};
use crate::pr::auto;
use crate::pr::git::{commit_changes, open_pr, push_current_branch};
use crate::pr::packages::CargoPackager;
use crate::pr::traits::{CommitInfo, PackageCommits, Packages};
use crate::pr::tui;
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{Repository, Status};
use secrecy::ExposeSecret;
use std::collections::HashMap;
use tracing::info;

/// Run notch PR:
///
/// 1. finds the packages in the project and, per package, the commits attributed to it since the
///    last notch bump commit (or since diverging from upstream, if there isn't one yet)
/// 2. for a package with prior, still-unmerged bump sections on this branch, poaches those
///    sections' commits into this run's — a `fix` landing after an already-staged `feat` bump
///    doesn't need its own patch bump on top, since nothing has actually released yet, but it
///    still needs to end up in the changelog (see the `poached_commits` comment below)
/// 3. picks each (possibly poached) package's bump — automatically from conventional commits
///    with `--auto`, otherwise interactively via the tui — then commits and pushes the result
pub fn run(auto: bool) -> Result<()> {
    // load the config
    let config = config::load().context("load notch.toml")?;

    // validate we have a github PAT to work with
    // NOTE: This is better than requiring the token as a cli arg because we can pass it as an env
    // override, which avoids poluting your shell history.
    let Some(token) = config.repo.token.clone() else {
        return Err(Error::msg("No token provided"));
    };

    // get our packages
    let cargo_packages = CargoPackager::new(".".to_string());
    let packages = cargo_packages.get().context("get packages")?;

    // keep a copy: rebuilding the PR body later needs to map crate names from prior bump
    // trailers back to their packages
    let all_packages = packages.clone();

    // get commits for each package
    let repo: Repository = Repository::init(".").context("open repo")?;
    let mut worktree_assigner = WorktreeCommitAssigner::new(repo);
    let (changed_packages_with_commits, repo, changelog_range) = worktree_assigner
        .get(&config.release, packages)
        .context("get commits for packages")?;

    // bumps that previous, still-unmerged runs already committed on this branch, recovered from
    // their trailers. Computed before commit_changes so this run's own bump commit isn't in the
    // walked range.
    let prior_bumps = prior_bump_sections(&repo, &config.release, &all_packages)
        .context("collect prior bump sections")?;

    // each package's prior sections' commits, oldest first, keyed by package name — since
    // nothing on this branch has actually released yet, a package that already has a staged bump
    // needs its whole not-yet-released history considered when deciding whether fresh commits
    // require escalating past it, and that history needs to end up in the regenerated changelog
    // entry rather than being silently dropped. `baselines` remembers each such package's version
    // *before* its first still-unmerged bump (the oldest section, since `prior_bumps` is oldest
    // first) — bumps must be applied there, not to the package's current version, or a fresh
    // commit no more severe than what's already staged would double-bump it (see `Bump::apply_to`).
    let mut poached_commits: HashMap<String, Vec<CommitInfo>> = HashMap::new();
    let mut baselines: HashMap<String, Version> = HashMap::new();
    for section in &prior_bumps {
        baselines
            .entry(section.package.name.clone())
            .or_insert_with(|| section.package.version.clone());
        poached_commits
            .entry(section.package.name.clone())
            .or_default()
            .extend(section.commits.iter().cloned());
    }

    // fold each package's poached commits in ahead of its fresh ones, and pair it with its
    // baseline — see `poached_commits`/`baselines` above. A package with no prior sections has no
    // history to poach and its baseline is just its own current version.
    let changed_packages_with_commits: HashMap<Package, (Version, Vec<CommitInfo>)> =
        changed_packages_with_commits
            .into_iter()
            .map(|(package, fresh)| {
                let baseline = baselines
                    .get(&package.name)
                    .cloned()
                    .unwrap_or_else(|| package.version.clone());
                let mut commits = poached_commits
                    .get(&package.name)
                    .cloned()
                    .unwrap_or_default();
                commits.extend(fresh);
                (package, (baseline, commits))
            })
            .collect();

    // nothing to do, just return
    if changed_packages_with_commits.is_empty() {
        println!("No packages to update, not creating commits or a release pr");
        return Ok(());
    }

    // pick each package's bump: from its conventional commits with --auto,
    // otherwise interactively via the tui
    let res = if auto {
        let res = auto::select(changed_packages_with_commits, &config.bumps);
        // every changed package's commits can match the skip list, in which
        // case there's nothing left to release
        if res.is_empty() {
            println!("No packages to update, not creating commits or a release pr");
            return Ok(());
        }
        res
    } else {
        let Some(res) = tui::run(changed_packages_with_commits).context("select version bumps")?
        else {
            info!("Cancelled, no changes made");
            return Ok(());
        };
        res
    };

    // actually update the package: one with poached prior sections gets its changelog entry
    // regenerated over the whole branch (its old entry no longer describes what's about to ship,
    // see `update_package`'s `poached` argument); one bumped for the first time on this branch
    // only needs the range since the last notch commit
    for updated_package in &res {
        let poached = poached_commits.contains_key(&updated_package.package.name);
        let range = if poached {
            config.release.commit_range()
        } else {
            changelog_range.clone()
        };
        update_package(&repo, updated_package, &range, poached).context("update the package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    // commit changes
    commit_changes(&repo, &res).context("commit changes to the repo")?;

    // push to the remote
    push_current_branch(&repo, &config.release).context("push current branch")?;

    // the PR describes every bump on the branch: prior runs' sections for packages this run
    // didn't touch (oldest first), then this run's — a package this run did touch had its prior
    // sections poached into its new one above, so its old standalone sections are dropped here to
    // avoid describing the same commits twice
    let mut all_updated: Vec<UpdatedCrate> = prior_bumps
        .into_iter()
        .filter(|section| !res.iter().any(|r| r.package.name == section.package.name))
        .collect();
    all_updated.extend(res);

    // open the PR
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("spawn runtime")?;

    rt.block_on(open_pr(&repo, &config, token.expose_secret(), &all_updated))
        .context("open PR on runtime")?;

    Ok(())
}

/// update the lockfile for the current project
#[inline]
pub fn generate_lockfile() -> Result<()> {
    run_command(&["cargo", "generate-lockfile"]).context("call cargo generate-lockfile")?;
    Ok(())
}

/// Generate the changelog for the provided commit range
#[inline]
fn generate_changelog(tag: &str, crate_path: &str, commit_range: &str) -> Result<()> {
    run_command(&[
        "git",
        "cliff",
        "--tag",
        tag,
        "--prepend",
        &format!("{crate_path}/CHANGELOG.md"),
        commit_range,
    ])
    .context("generate changelog using git cliff")?;
    Ok(())
}

/// Removes the `## [<version>]` section (from its heading up to, but not including, the next
/// `## [` heading, or up to the end of the file) from changelog content, if present. Used before
/// regenerating a poached package's changelog entry, so the superseded entry for its previously
/// staged version doesn't linger alongside the new consolidated one — see `update_package`.
fn strip_changelog_section(content: &str, version: &Version) -> String {
    let heading = format!("## [{version}]");
    let Some(heading_start) = content
        .match_indices(&heading)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || content.as_bytes()[i - 1] == b'\n')
    else {
        return content.to_string();
    };

    let after_heading = heading_start + heading.len();
    let section_end = content[after_heading..]
        .find("\n## [")
        .map_or(content.len(), |rel| after_heading + rel + 1);

    let mut result = content[..heading_start].to_string();
    result.push_str(&content[section_end..]);
    result
}

pub struct UpdatedCrate {
    pub package: Package,
    pub new_version: Version,
    pub commits: Vec<CommitInfo>,
}

#[inline]
pub fn backup_config(original_location: &str, backup_location: &str) -> Result<()> {
    run_command(&["cp", original_location, backup_location]).context("create cargo.toml backup")?;
    Ok(())
}

/// Restore the original cargo.toml
#[inline]
pub fn restore_backup_config(original_location: &str, backup_location: &str) -> Result<()> {
    run_command(&["mv", backup_location, original_location])
        .context("restore backup cargo.toml")?;
    Ok(())
}

#[inline]
pub fn update_package_version(updated_crate: &UpdatedCrate, original_location: &str) -> Result<()> {
    // update the current one in place, with the bumped version
    // cowboying this for now
    // TODO: Use a proper crate for updating this
    // WARN: If you have a dep with this version before the package version, ur cooked
    let s = std::fs::read_to_string(original_location).context("read Cargo.toml")?;
    let s = s.replacen(
        &format!("version = \"{}\"", updated_crate.package.version),
        &format!("version = \"{}\"", updated_crate.new_version),
        1,
    );
    std::fs::write(original_location, s).context("write updated back")?;
    Ok(())
}

#[inline]
fn is_file_dirty(repo: &Repository, path: &std::path::Path) -> Result<bool> {
    let status = repo.status_file(path)?;

    // "Dirty" = any change in the index or working tree
    Ok(status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::WT_NEW
            | Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_RENAMED
            | Status::WT_TYPECHANGE,
    ))
}

pub fn update_package(
    repo: &Repository,
    updated_crate: &UpdatedCrate,
    commit_range: &str,
    poached: bool,
) -> Result<()> {
    // create a backup of the current Cargo.toml
    let real_file = updated_crate.package.join("Cargo.toml");
    let tmp_file = updated_crate.package.join("Cargo.toml.bak");

    // check if either the changelog or the Cargo.toml has changes not yet commited, if so, we
    // return an error. We don't update anything for consistency, its an all or nothing
    let changelog_path = updated_crate.package.join("CHANGELOG.md");

    if is_file_dirty(repo, real_file.as_ref()).context("check if Cargo.toml dirty")? {
        return Err(Error::msg(
            "Cargo.toml is dirty, please commit the changes and try again",
        ));
    }

    if is_file_dirty(repo, changelog_path.as_ref()).context("check if Cargo.toml dirty")? {
        return Err(Error::msg(
            "Cargo.toml is dirty, please commit the changes and try again",
        ));
    }

    // create a backup
    backup_config(&real_file, &tmp_file).context("backup config")?;

    // update the package version in cargo.toml
    match update_package_version(updated_crate, &real_file) {
        Ok(()) => {
            // delete the temp file
            std::fs::remove_file(tmp_file).context("delete temp file")?;
        }
        Err(e) => {
            // move the backup to the OG location
            restore_backup_config(&real_file, &tmp_file).context("restore backup cargo.toml")?;
            return Err(e);
        }
    }

    // this package's changelog entry is being regenerated over its whole (poached) history, not
    // just this run's delta — strip the entry that superseded version left behind so it doesn't
    // linger duplicated alongside the new consolidated one
    if poached {
        let changelog_path = updated_crate.package.join("CHANGELOG.md");
        let existing = std::fs::read_to_string(&changelog_path)
            .context("read changelog to strip its superseded section")?;
        let stripped = strip_changelog_section(&existing, &updated_crate.package.version);
        std::fs::write(&changelog_path, stripped).context("write stripped changelog")?;
    }

    // generate the changelog entry using git cliff,
    generate_changelog(
        &updated_crate.new_version.to_string(),
        &updated_crate.package.path,
        commit_range,
    )
    .context("generate changelog")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn strips_the_only_section() {
        let content = "# Changelog\n\n## [1.3.0] - 2026-01-01\n\n### Features\n\n- add x\n";
        assert_eq!(
            strip_changelog_section(content, &v("1.3.0")),
            "# Changelog\n\n"
        );
    }

    #[test]
    fn strips_only_the_matching_section_and_keeps_others() {
        let content = "\
# Changelog

## [1.3.0] - 2026-01-02

### Features

- add y

## [1.2.0] - 2026-01-01

### Features

- add x
";
        let stripped = strip_changelog_section(content, &v("1.3.0"));
        assert!(!stripped.contains("1.3.0"));
        assert!(stripped.contains("## [1.2.0] - 2026-01-01"));
        assert!(stripped.contains("- add x"));
    }

    #[test]
    fn leaves_content_unchanged_when_version_not_present() {
        let content = "# Changelog\n\n## [1.2.0] - 2026-01-01\n\n- add x\n";
        assert_eq!(strip_changelog_section(content, &v("1.3.0")), content);
    }

    #[test]
    fn strips_a_section_at_the_end_of_the_file_with_no_trailing_heading() {
        let content = "# Changelog\n\n## [2.0.0] - 2026-01-01\n\n- breaking change\n";
        assert_eq!(
            strip_changelog_section(content, &v("2.0.0")),
            "# Changelog\n\n"
        );
    }

    #[test]
    fn does_not_match_a_heading_that_is_not_at_the_start_of_a_line() {
        // a body line that happens to contain the heading text mid-line must not be treated as
        // the section's own heading
        let content =
            "# Changelog\n\n## [1.3.0] - 2026-01-01\n\n- mentions ## [1.3.0] in passing\n";
        let stripped = strip_changelog_section(content, &v("1.3.0"));
        assert_eq!(stripped, "# Changelog\n\n");
    }
}
