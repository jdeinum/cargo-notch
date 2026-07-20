use crate::config::{self, Config, ReleaseConfig};
use crate::error::{Error, Result};
use crate::pr::run::UpdatedCrate;
use anyhow::Context;
use git2::{Commit, Cred, IndexAddOption, PushOptions, RemoteCallbacks, Repository, Signature};
use octocrab::Octocrab;
use tracing::debug;

// Notch identity
const NOTCH_COMMIT_MESSAGE: &str = "chore(notch): changelog + version bump";
const NOTCH_COMMITTER_NAME: &str = "notch";
const NOTCH_COMMITTER_EMAIL: &str = "notch@noreply.notch-release";
const NOTCH_TRAILER_KEY: &str = "Notch-Bump";

// Fixed identity for notch's own commits, so they're recognizable in `git log`/`git blame` and,
// combined with the `Notch-Bump` trailer, can't be mistaken for a human commit that happens to
// start the same way.
pub fn notch_signature<'a>() -> Result<Signature<'a>> {
    Signature::now(NOTCH_COMMITTER_NAME, NOTCH_COMMITTER_EMAIL).context("build notch signature")
}

// Shared by every remote operation (fetch, push) that needs to authenticate — relies on the
// caller already having an SSH agent with the right key loaded, since notch has no other way
// to get credentials.
pub fn ssh_credentials() -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username, _allowed| {
        Cred::ssh_key_from_agent(username.unwrap_or("git"))
    });
    callbacks
}

// `crate@version` pairs for every crate this run bumped, recorded as a trailer so a later run can
// recover exactly what a prior bump commit did without needing a second source of truth.
pub fn build_bump_trailer(updated: &[UpdatedCrate]) -> String {
    let pairs = updated
        .iter()
        .map(|c| format!("{}@{}", c.package.name, c.new_version))
        .collect::<Vec<_>>()
        .join(",");
    format!("{NOTCH_TRAILER_KEY}: {pairs}")
}

// A commit is notch's own release commit only if both the committer identity and the trailer match
// — either alone could plausibly be forged or coincidental, but not both together.
pub fn is_notch_commit(commit: &Commit) -> bool {
    let committer_matches = commit
        .committer()
        .email()
        .is_ok_and(|e| e == NOTCH_COMMITTER_EMAIL);
    let has_trailer = commit.message().is_ok_and(|m| {
        m.lines()
            .any(|l| l.starts_with(&format!("{NOTCH_TRAILER_KEY}:")))
    });
    committer_matches && has_trailer
}

// The range git-cliff should scan: since the last notch bump if one exists in this branch's
// history, otherwise the same upstream-relative range used everywhere else.
pub fn changelog_range(release: &ReleaseConfig, last_notch_commit: Option<&Commit>) -> String {
    last_notch_commit.map_or_else(|| release.commit_range(), |c| format!("{}..HEAD", c.id()))
}

// Commits current changes for this repo
// TODO: We should only commit the files notch actually touches, and maybe check if they are already
// dirty and prevent usage if dirty?
pub fn commit_changes(repo: &Repository, updated: &[UpdatedCrate]) -> Result<()> {
    // commit the changelog and version bumps
    let mut index = repo.index().context("get index for repo")?;
    index
        .add_all(std::iter::once(&"."), IndexAddOption::DEFAULT, None)
        .context("add all files to the index")?;
    index.write().context("write index to disk")?;
    let sig = notch_signature()?;
    let tree = repo
        .find_tree(index.write_tree().context("write tree for index")?)
        .context("find tree")?;
    let parent = repo
        .head()
        .context("get head of branch")?
        .peel_to_commit()
        .context("convert ref commit")?;

    let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(updated));

    repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&parent])
        .context("create the commit")?;

    Ok(())
}

pub fn push_current_branch(repo: &Repository, release: &ReleaseConfig) -> Result<()> {
    let head = repo.head().context("get head")?;
    let branch = head.name().context("get head name")?;

    let mut remote = repo.find_remote(&release.remote).context("get remote")?;

    debug!("Found remote {}", release.remote);

    let mut opts = PushOptions::new();
    opts.remote_callbacks(ssh_credentials());

    // refspec: local:remote
    let refspec = format!("{branch}:{branch}");
    remote.push(&[&refspec], Some(&mut opts))?;

    Ok(())
}

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

    let pr = octocrab
        .pulls(owner, repo_name)
        .create(title, name, &config.release.default_branch)
        .body(body)
        .send()
        .await
        .context("create PR")?;

    println!("Opened PR #{}: {}", pr.number, pr.html_url.unwrap());
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
    use crate::package::Package;
    use crate::pr::assign::find_last_notch_commit;
    use crate::pr::traits::CommitInfo;
    use cargo_metadata::semver::Version;
    use git2::Oid;
    use std::fs;
    use std::path::Path;

    // Builds a throwaway repo with a single commit authored by `sig`, so
    // `is_notch_commit`/`find_last_notch_commit` can be exercised against a
    // real `git2::Commit` rather than a hand-built struct.
    fn repo_with_commit(dir_suffix: &str, message: &str, sig: &Signature) -> (Repository, Oid) {
        let dir =
            std::env::temp_dir().join(format!("notch-pr-test-{dir_suffix}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let repo = Repository::init(&dir).unwrap();
        fs::write(dir.join("file.txt"), "hello").unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let oid = repo
            .commit(Some("HEAD"), sig, sig, message, &tree, &[])
            .unwrap();
        drop(tree);
        (repo, oid)
    }

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
    fn build_bump_trailer_lists_every_crate_updated() {
        let updated = vec![
            updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0)),
            updated_crate("bar", (0, 4, 0), Version::new(0, 5, 0)),
        ];

        assert_eq!(
            build_bump_trailer(&updated),
            "Notch-Bump: foo@1.1.0,bar@0.5.0"
        );
    }

    #[test]
    fn notch_commit_is_identified_by_identity_and_trailer_together() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));

        let (repo, oid) = repo_with_commit("notch", &message, &notch_signature().unwrap());
        let commit = repo.find_commit(oid).unwrap();
        assert!(is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn human_commit_with_the_same_wording_is_not_mistaken_for_notchs() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));
        let human_sig = Signature::now("a human", "human@example.com").unwrap();

        let (repo, oid) = repo_with_commit("human", &message, &human_sig);
        let commit = repo.find_commit(oid).unwrap();
        assert!(!is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn notch_identity_without_the_trailer_is_not_mistaken_for_a_bump_commit() {
        let (repo, oid) = repo_with_commit(
            "no-trailer",
            "chore(notch): unrelated",
            &notch_signature().unwrap(),
        );
        let commit = repo.find_commit(oid).unwrap();
        assert!(!is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn find_last_notch_commit_picks_the_one_closest_to_head() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));
        let sig = notch_signature().unwrap();

        // Two notch commits with a human commit in between — oldest-first,
        // as `get_commits` returns them.
        let (repo, first_oid) = repo_with_commit("multi", &message, &sig);
        let human_sig = Signature::now("a human", "human@example.com").unwrap();
        let human_commit = repo.find_commit(first_oid).unwrap();
        let mut index = repo.index().unwrap();
        fs::write(repo.workdir().unwrap().join("file2.txt"), "again").unwrap();
        index.add_path(Path::new("file2.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let second_oid = repo
            .commit(
                Some("HEAD"),
                &human_sig,
                &human_sig,
                "feat: add a thing",
                &tree,
                &[&human_commit],
            )
            .unwrap();

        let second_notch_commit = repo.find_commit(second_oid).unwrap();
        fs::write(repo.workdir().unwrap().join("file.txt"), "changed").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let third_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &message,
                &tree,
                &[&second_notch_commit],
            )
            .unwrap();

        let commits = vec![
            repo.find_commit(first_oid).unwrap(),
            repo.find_commit(second_oid).unwrap(),
            repo.find_commit(third_oid).unwrap(),
        ];

        let found = find_last_notch_commit(&commits).unwrap();
        assert_eq!(found.id(), third_oid);

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn changelog_range_falls_back_to_upstream_range_without_a_marker() {
        let release = ReleaseConfig::default();
        assert_eq!(changelog_range(&release, None), "origin/master..HEAD");
    }

    #[test]
    fn changelog_range_uses_the_marker_commit_when_present() {
        let (repo, oid) =
            repo_with_commit("range", NOTCH_COMMIT_MESSAGE, &notch_signature().unwrap());
        let commit = repo.find_commit(oid).unwrap();
        let release = ReleaseConfig::default();

        assert_eq!(
            changelog_range(&release, Some(&commit)),
            format!("{oid}..HEAD")
        );

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
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
            },
            CommitInfo {
                summary: "fix: fix a thing".to_string(),
                sha1: "abcdef1234567890".to_string(),
            },
        ];
        let (_title, body) = get_pr_title_and_description(&[updated]).unwrap();

        assert!(body.contains("- 1234567 feat: add a thing"));
        assert!(body.contains("- abcdef1 fix: fix a thing"));
    }
}
