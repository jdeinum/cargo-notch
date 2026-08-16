use crate::commit::UpdatedCrate;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::utils::command::run_command;
use crate::utils::commits::{fetch_remote, get_commits};
use anyhow::Context;
use git2::{Commit, Cred, CredentialType, PushOptions, RemoteCallbacks, Repository, Signature};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
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

// Shared by every remote operation (fetch, push) that needs to authenticate — SSH remotes rely
// on the caller already having an ssh-agent with the right key loaded, HTTPS remotes on the
// GitHub token from config (the same one the PR API uses).
pub fn remote_credentials(token: Option<SecretString>) -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_url, username, allowed| {
        if allowed.contains(CredentialType::SSH_KEY) {
            return Cred::ssh_key_from_agent(username.unwrap_or("git"));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            return token.as_ref().map_or_else(
                || {
                    Err(git2::Error::from_str(
                        "authenticating to an https remote needs the github token (NOTCH__REPO__TOKEN)",
                    ))
                },
                |token| Cred::userpass_plaintext("x-access-token", token.expose_secret()),
            );
        }
        Err(git2::Error::from_str(
            "remote offered no supported auth method (ssh-agent for ssh remotes, token for https)",
        ))
    });
    callbacks
}

// `crate@old->new` pairs for every crate this run bumped, recorded as a trailer so the commit is
// self-describing and, combined with its identity, recognizable as notch's own — see
// `is_notch_commit`.
pub fn build_bump_trailer(updated: &[UpdatedCrate]) -> String {
    let pairs = updated
        .iter()
        .map(|c| {
            format!(
                "{}@{}->{}",
                c.package.name, c.package.version, c.new_version
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{NOTCH_TRAILER_KEY}: {pairs}")
}

// A commit is notch's own release commit only if both its identity and the trailer match — either
// alone could plausibly be forged or coincidental, but not both together. Checked via the
// *author*, not the committer: a rebase, cherry-pick, or `commit --amend` stamps a new committer
// (whoever ran the operation) while preserving the original author, so committer-based detection
// silently stops recognizing a notch commit the moment it's replayed onto a new base — which is
// routine for a bump commit sitting on an unmerged branch.
pub fn is_notch_commit(commit: &Commit) -> bool {
    let author_matches = commit
        .author()
        .email()
        .is_ok_and(|e| e == NOTCH_COMMITTER_EMAIL);
    let has_trailer = commit.message().is_ok_and(|m| {
        m.lines()
            .any(|l| l.starts_with(&format!("{NOTCH_TRAILER_KEY}:")))
    });
    author_matches && has_trailer
}

// `commits` is oldest-first, so the last match is the most recent bump — the one a rerun should
// diff against.
pub fn find_last_notch_commit<'a>(commits: &[Commit<'a>]) -> Option<Commit<'a>> {
    commits.iter().rev().find(|c| is_notch_commit(c)).cloned()
}

/// Drops the branch's most recent notch commit, if one exists, by replaying every commit after it
/// onto its own parent — the standard rebase idiom for removing a single commit from history.
/// Meant to run before anything else in `commit::commit`, so changed-package detection, commit
/// attribution, and the FSMs all see a clean slate instead of having to route around a stale
/// notch commit sitting in the branch's history.
pub fn drop_prior_notch_commit(repo: &Repository, config: &Config) -> Result<()> {
    // a stale local upstream ref would throw off both which commit range we search and, if we do
    // rebase, what we rebase onto — see `fetch_remote`
    fetch_remote(repo, config).context("fetch remote")?;

    let commits = get_commits(repo, &config.release).context("get commits")?;
    let Some(notch_commit) = find_last_notch_commit(&commits) else {
        return Ok(());
    };

    run_command(&[
        "git",
        "rebase",
        "--onto",
        &format!("{}^", notch_commit.id()),
        &notch_commit.id().to_string(),
    ])
    .context("rebase away the prior notch commit")?;

    Ok(())
}

/// Commits the changes made by notch, typically just updating `Cargo.toml` and lockfiles, as well
/// as the changelogs.
pub fn commit_changes(repo: &Repository, updated: &[UpdatedCrate]) -> Result<()> {
    let mut index = repo.index().context("get index for repo")?;

    // add the lock file, which is created when we generate our changelog
    index
        .add_path(Path::new("Cargo.lock"))
        .context("add Cargo.lock to the index")?;

    for package in updated {
        // add the cargo.toml
        let cargo_path = package.package.join("Cargo.toml");
        index
            .add_path(cargo_path.as_ref())
            .context("add cargo.toml to index")?;

        // add the changelog
        let changelog_path = package.package.join("CHANGELOG.md");
        index
            .add_path(changelog_path.as_ref())
            .context("add changelog to index")?;
    }

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

pub fn push_current_branch(repo: &Repository, config: &Config) -> Result<()> {
    let release = &config.release;
    let mut branch = git2::Branch::wrap(repo.head().context("get head")?);
    let refname = branch.get().name().context("get head name")?.to_string();

    let mut remote = repo.find_remote(&release.remote).context("get remote")?;

    debug!("Found remote {}", release.remote);

    let mut opts = PushOptions::new();
    opts.remote_callbacks(remote_credentials(config.repo.token.clone()));

    // refspec: local:remote — creates the branch on the remote if it doesn't exist yet
    let refspec = format!("{refname}:{refname}");
    remote.push(&[&refspec], Some(&mut opts))?;

    // `git push -u` equivalent: a branch whose remote ref the push above just created has no
    // upstream configured, and open_pr resolves the upstream — without this it fails on any
    // branch that wasn't already pushed manually with `-u`.
    if branch.upstream().is_err() {
        let short = branch
            .name()
            .context("get local branch name")?
            .ok_or_else(|| Error::msg("branch name is not valid utf-8"))?
            .to_string();
        branch
            .set_upstream(Some(&format!("{}/{short}", release.remote)))
            .context("set upstream for pushed branch")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::package::Package;
    use cargo_metadata::semver::Version;
    use git2::Oid;
    use std::fs;
    use std::path::Path;

    // Builds a throwaway repo with a single commit authored by `sig`, so
    // `is_notch_commit`/`find_last_notch_commit` can be exercised against a
    // real `git2::Commit` rather than a hand-built struct.
    fn repo_with_commit(dir_suffix: &str, message: &str, sig: &Signature) -> (Repository, Oid) {
        let dir = std::env::temp_dir().join(format!(
            "notch-git-test-{dir_suffix}-{}",
            std::process::id()
        ));
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
            "Notch-Bump: foo@1.0.0->1.1.0,bar@0.4.0->0.5.0"
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

    // Regression test: a rebase, cherry-pick, or amend stamps a new committer (whoever performed
    // the operation) while preserving the original author — routine for a bump commit sitting on
    // an unmerged branch that gets rebased. Detection must survive that.
    #[test]
    fn notch_commit_is_still_recognized_after_its_committer_changes() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));

        let dir = std::env::temp_dir().join(format!(
            "notch-git-test-rebased-committer-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        fs::write(dir.join("file.txt"), "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let author = notch_signature().unwrap();
        let rebaser = Signature::now("github-actions[bot]", "someone@example.com").unwrap();
        let oid = repo
            .commit(Some("HEAD"), &author, &rebaser, &message, &tree, &[])
            .unwrap();

        let commit = repo.find_commit(oid).unwrap();
        assert!(is_notch_commit(&commit));

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
}
