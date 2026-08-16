use crate::commit::UpdatedCrate;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use anyhow::Context;
use git2::Repository;
use octocrab::Octocrab;
use octocrab::params::State;
use tracing::debug;

// Runs only on a current-thread tokio runtime (see run()), so the future is
// never sent across threads despite git2 types not being Send.
#[allow(clippy::future_not_send)]
pub async fn open_pr(
    repo: &Repository,
    config: &Config,
    token: &str,
    updated_crates: &[UpdatedCrate],
) -> Result<()> {
    let (owner, repo_name) =
        config::resolve_owner_repo(repo, &config.repo).context("resolve owner/repo")?;

    let head = repo.head().context("get branch head")?;
    let branch = git2::Branch::wrap(head);
    let name = branch
        .name()
        .context("get local name")?
        .ok_or_else(|| Error::msg("No branch name"))?;

    let upstream = branch.upstream().context("get upstream branch")?;
    let upstream_branch_name = upstream
        .name()
        .context("get branch name")?
        .ok_or_else(|| Error::msg("No branch name"))?;
    debug!(
        "Creating PR from {upstream_branch_name} into {}",
        config.release.default_branch
    );

    let octocrab = Octocrab::builder()
        .personal_token(token)
        .build()
        .context("build octocrab")?;

    let (title, body) =
        get_pr_title_and_description(updated_crates).context("get title pr and description")?;

    let head_filter = format!("{owner}:{name}");
    let pulls = octocrab.pulls(owner, repo_name);

    // a rerun on a branch that already has an open release PR must refresh that PR's title/body
    // instead of trying to create a second one (GitHub rejects the duplicate with a 422)
    let existing = pulls
        .list()
        .state(State::Open)
        .head(head_filter)
        .per_page(1)
        .send()
        .await
        .context("list open PRs for branch")?
        .items
        .into_iter()
        .next();

    if let Some(pr) = existing {
        let pr = pulls
            .update(pr.number)
            .title(title)
            .body(body)
            .send()
            .await
            .context("update existing PR")?;
        println!("Updated PR #{}: {}", pr.number, pr.html_url.unwrap());
    } else {
        let pr = pulls
            .create(title, name, &config.release.default_branch)
            .body(body)
            .send()
            .await
            .context("create PR")?;
        println!("Opened PR #{}: {}", pr.number, pr.html_url.unwrap());
    }
    Ok(())
}

fn get_pr_title_and_description(updated_crates: &[UpdatedCrate]) -> Result<(String, String)> {
    fn bump_line(c: &UpdatedCrate) -> String {
        format!(
            "chore: bumping {} from {} to {}",
            c.package.name, c.package.version, c.new_version
        )
    }

    fn commit_list(c: &UpdatedCrate) -> String {
        c.commits
            .iter()
            .map(|commit| format!("- {} {}", commit.short_id(), commit.summary))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let title = match updated_crates {
        [] => return Err(Error::msg("No updated crates, shouldn't be creating a PR!")),
        [c] => bump_line(c),
        // several sections for the same crate means one crate bumped across several runs —
        // title it with the full journey rather than the generic multi-crate line
        [first, .., last]
            if updated_crates
                .iter()
                .all(|c| c.package.name == first.package.name) =>
        {
            format!(
                "chore: bumping {} from {} to {}",
                first.package.name, first.package.version, last.new_version
            )
        }
        _ => "chore: bumping package versions".to_string(),
    };

    let body = updated_crates
        .iter()
        .map(|c| format!("{}\n{}\n", bump_line(c), commit_list(c)))
        .collect::<Vec<_>>()
        .join("\n");

    Ok((title, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::commits::CommitInfo;
    use crate::utils::package::Package;
    use cargo_metadata::semver::Version;

    fn updated_crate(name: &str, from: (u64, u64, u64), to: Version) -> UpdatedCrate {
        UpdatedCrate {
            package: Package {
                path: name.to_string(),
                name: name.to_string(),
                version: Version::new(from.0, from.1, from.2),
            },
            new_version: to,
            commits: vec![],
        }
    }

    #[test]
    fn errors_when_no_updated_crates() {
        let result = get_pr_title_and_description(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn single_crate_title_is_the_bump_line() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let (title, body) = get_pr_title_and_description(&updated).unwrap();

        assert_eq!(title, "chore: bumping foo from 1.0.0 to 1.1.0");
        assert!(body.contains("chore: bumping foo from 1.0.0 to 1.1.0"));
    }

    #[test]
    fn repeated_bumps_of_one_crate_get_a_cumulative_title() {
        let updated = vec![
            updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0)),
            updated_crate("foo", (1, 1, 0), Version::new(1, 2, 0)),
        ];
        let (title, _body) = get_pr_title_and_description(&updated).unwrap();

        assert_eq!(title, "chore: bumping foo from 1.0.0 to 1.2.0");
    }

    #[test]
    fn multiple_crates_get_a_generic_title() {
        let updated = vec![
            updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0)),
            updated_crate("bar", (0, 4, 0), Version::new(0, 5, 0)),
        ];
        let (title, _body) = get_pr_title_and_description(&updated).unwrap();

        assert_eq!(title, "chore: bumping package versions");
    }

    #[test]
    fn body_lists_each_crates_commits_by_short_id() {
        let mut updated = updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0));
        updated.commits = vec![
            CommitInfo {
                summary: "feat: add a thing".to_string(),
                sha1: "1234567890abcdef".to_string(),
                breaking: false,
            },
            CommitInfo {
                summary: "fix: fix a thing".to_string(),
                sha1: "abcdef1234567890".to_string(),
                breaking: false,
            },
        ];
        let (_title, body) = get_pr_title_and_description(&[updated]).unwrap();

        assert!(body.contains("- 1234567 feat: add a thing"));
        assert!(body.contains("- abcdef1 fix: fix a thing"));
    }
}
