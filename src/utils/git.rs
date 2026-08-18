use crate::commit::UpdatedCrate;
use crate::config::{Config, ReleaseConfig};
use crate::error::{Error, Result};
use crate::utils::command::run_command_in;
use crate::utils::commits::{fetch_remote, get_commits};
use crate::utils::package::Package;
use crate::utils::packages::Ecosystem;
use anyhow::Context;
use git2::{
    BranchType, Commit, Cred, CredentialType, PushOptions, RemoteCallbacks, Repository, Signature,
    Worktree, WorktreePruneOptions, build::CheckoutBuilder,
};
use secrecy::{ExposeSecret, SecretString};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

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

/// Reads each package as it would be if this branch's prior notch commit had never happened,
/// leaving the caller's branch exactly as it found it.
///
/// The problem: package discovery reads versions off disk, so a prior notch bump sitting in the
/// working tree becomes the package's "current" version and the next run bumps on top of it —
/// stacking one bump per invocation instead of producing one net bump against the default branch.
/// [`drop_prior_notch_commit`] fixes that by rebasing the commit away, but rewriting history is
/// not something *deciding* what to release should be doing.
///
/// So the rebase happens in a throwaway worktree instead. What comes back is "this branch without
/// notch's own bump", which is the baseline that actually matters: every other commit survives, so
/// a version someone set by hand still counts. Reading the merge base instead would be simpler and
/// would silently discard it.
///
/// Returns `None` when the branch has no notch commit — then the working tree already *is* the
/// baseline, and no worktree is created.
pub fn packages_without_prior_notch_commit(
    repo: &Repository,
    config: &Config,
    ecosystem: &dyn Ecosystem,
) -> Result<Option<Vec<Package>>> {
    {
        let commits = get_commits(repo, &config.release).context("get commits")?;
        if !commits.iter().any(|c| is_notch_commit(c)) {
            return Ok(None);
        }
    }

    let head = repo
        .head()
        .context("get head")?
        .peel_to_commit()
        .context("peel head to commit")?
        .id();

    // keyed by head so concurrent runs, or one abandoned by a crash, don't collide on the name
    let name = format!("notch-baseline-{head}");
    let path = std::env::temp_dir().join(&name);
    let worktree = repo
        .worktree(&name, &path, None)
        .context("create baseline worktree")?;

    let packages = (|| -> Result<Vec<Package>> {
        let worktree_repo =
            Repository::open_from_worktree(&worktree).context("open baseline worktree")?;
        worktree_repo
            .set_head_detached(head)
            .context("detach baseline worktree head")?;
        worktree_repo
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .context("check out head in baseline worktree")?;

        // Detached, so this moves nothing anyone else can see, and it's the same drop `apply`
        // performs in place — a conflict fails inside the worktree, which the cleanup below
        // discards, rather than leaving the user's branch sitting mid-rebase.
        drop_notch_commits(&worktree_repo, &path, &config.release)
            .context("drop notch commits in the baseline worktree")?;

        ecosystem
            .packages(&path)
            .context("read packages from the baseline worktree")
    })();

    prune_worktree(repo, &worktree, &name);

    packages.map(Some)
}

/// Discards a throwaway worktree and the scratch branch `Repository::worktree` created alongside
/// it. Best-effort and never fails the caller: the work it was created for has either already
/// succeeded or already failed with a more useful error, and the only cost of a leaked worktree is
/// a stale temp directory.
pub fn prune_worktree(repo: &Repository, worktree: &Worktree, name: &str) {
    let mut opts = WorktreePruneOptions::new();
    opts.valid(true).working_tree(true);

    if let Err(e) = worktree.prune(Some(&mut opts)) {
        warn!("could not prune worktree {name}: {e}");
    }
    if let Ok(mut branch) = repo.find_branch(name, BranchType::Local)
        && let Err(e) = branch.delete()
    {
        warn!("could not delete scratch branch {name}: {e}");
    }
}

/// Rewinds the branch to the state notch found it in, by rebasing away the bump commits it left
/// behind on a previous run. Meant to run at the start of `apply` — the writing half — because it
/// rewrites history; `plan` gets the same view without touching anything, via
/// [`packages_without_prior_notch_commit`].
pub fn drop_prior_notch_commit(repo: &Repository, config: &Config) -> Result<()> {
    // a stale local upstream ref would throw off both which commit range we search and, if we do
    // rebase, what we rebase onto — see `fetch_remote`
    fetch_remote(repo, config).context("fetch remote")?;

    drop_notch_commits(repo, Path::new("."), &config.release)
}

/// Rebases away *every* notch commit on the branch, in `dir`.
///
/// Every one, not just the most recent: a run interrupted between two of them leaves both, and
/// dropping only the last would leave the first's bump standing and then bump on top of it —
/// double-counting the release. One pass drops one commit, so the loop runs until none are left.
///
/// Shared by the in-place drop and the throwaway-worktree one precisely so those two can't drift:
/// `plan` predicts what `apply` will do only for as long as they agree on what "without notch's
/// prior bump" means.
fn drop_notch_commits(repo: &Repository, dir: &Path, release: &ReleaseConfig) -> Result<()> {
    // Bounded by how many are there to begin with. Each pass should remove exactly one, so this is
    // only ever a backstop — but it's the difference between a rebase that silently no-ops and an
    // infinite loop.
    let mut passes = {
        let commits = get_commits(repo, release).context("get commits")?;
        commits.iter().filter(|c| is_notch_commit(c)).count()
    };

    while passes > 0 {
        let target = {
            let commits = get_commits(repo, release).context("get commits")?;
            let Some(commit) = find_last_notch_commit(&commits) else {
                return Ok(());
            };
            commit.id()
        };

        run_command_in(
            dir,
            &[
                "git",
                "rebase",
                "--onto",
                &format!("{target}^"),
                &target.to_string(),
            ],
        )
        .context("rebase away a prior notch commit")?;

        passes -= 1;
    }

    Ok(())
}

/// Commits the changes made by notch: the manifests and lockfile the ecosystem reported writing,
/// plus the changelogs. `touched` is repo-relative paths, exactly as `Ecosystem::set_versions`
/// and `Ecosystem::refresh_lock` returned them — staging what those steps actually wrote, rather
/// than re-deriving cargo-shaped filenames here, is what keeps this function ecosystem-agnostic.
pub fn commit_changes(
    repo: &Repository,
    updated: &[UpdatedCrate],
    touched: &[PathBuf],
) -> Result<()> {
    let mut index = repo.index().context("get index for repo")?;

    for path in touched {
        index
            .add_path(path)
            .with_context(|| format!("add {} to the index", path.display()))?;
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
    use crate::utils::packages::CargoEcosystem;
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

    // Commits `file` onto `parent` (or as a root commit), so a test can build the short branch
    // `prior_notch_state` walks: `origin/master..HEAD`.
    fn commit_onto(
        repo: &Repository,
        sig: &Signature,
        message: &str,
        file: &str,
        parent: Option<Oid>,
    ) -> Oid {
        fs::write(repo.workdir().unwrap().join(file), message).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let parents: Vec<Commit> = parent
            .into_iter()
            .map(|p| repo.find_commit(p).unwrap())
            .collect();
        let parent_refs: Vec<&Commit> = parents.iter().collect();

        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parent_refs)
            .unwrap()
    }

    // A repo with one human commit that `origin/master` points at, so anything committed after it
    // falls inside the range notch scans.
    fn repo_with_upstream(dir_suffix: &str) -> (Repository, Oid) {
        let dir = std::env::temp_dir().join(format!(
            "notch-git-test-{dir_suffix}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();

        let human = Signature::now("a human", "human@example.com").unwrap();
        let base = commit_onto(&repo, &human, "feat: a thing", "a.txt", None);
        repo.reference("refs/remotes/origin/master", base, true, "test")
            .unwrap();

        (repo, base)
    }

    // Commits whatever is already on disk under `paths` — unlike `commit_onto`, which writes the
    // commit message into the file it then commits, and so can't be used to stage a real manifest.
    fn commit_paths(
        repo: &Repository,
        sig: &Signature,
        message: &str,
        paths: &[&str],
        parent: Option<Oid>,
    ) -> Oid {
        let mut index = repo.index().unwrap();
        for path in paths {
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let parents: Vec<Commit> = parent
            .into_iter()
            .map(|p| repo.find_commit(p).unwrap())
            .collect();
        let parent_refs: Vec<&Commit> = parents.iter().collect();

        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parent_refs)
            .unwrap()
    }

    // A minimal but real crate, so `cargo metadata` can resolve it inside the baseline worktree.
    fn write_crate(dir: &Path, version: &str) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"baseline-test-crate\"\nversion = \"{version}\"\nedition = \"2021\"\n"
            ),
        )
        .unwrap();
    }

    fn notch_commit_message(name: &str, from: (u64, u64, u64), to: Version) -> String {
        let updated = vec![updated_crate(name, from, to)];
        format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated))
    }

    fn updated_crate(name: &str, from: (u64, u64, u64), to: Version) -> UpdatedCrate {
        UpdatedCrate {
            package: Package::new(
                name.to_string(),
                name.to_string(),
                Version::new(from.0, from.1, from.2),
                PathBuf::from(format!("{name}/Cargo.toml")),
            ),
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

    // The core of the non-destructive plan: the branch still carries notch's bump to 1.1.0, but
    // the baseline read has to come back with 1.0.0 — and the branch has to be exactly where it
    // was afterwards, which is what separates this from `drop_prior_notch_commit`.
    #[test]
    fn the_baseline_read_sees_past_a_prior_bump_without_moving_the_branch() {
        let (repo, base) = repo_with_upstream("baseline-worktree");
        let workdir = repo.workdir().unwrap().to_path_buf();
        let human = Signature::now("a human", "human@example.com").unwrap();

        write_crate(&workdir, "1.0.0");
        let released = commit_paths(
            &repo,
            &human,
            "feat: a thing",
            &["Cargo.toml", "src/lib.rs"],
            Some(base),
        );

        // notch's own bump, exactly as `apply` would leave it: manifest at 1.1.0 on disk
        write_crate(&workdir, "1.1.0");
        let message = notch_commit_message("baseline-test-crate", (1, 0, 0), Version::new(1, 1, 0));
        let bumped = commit_paths(
            &repo,
            &notch_signature().unwrap(),
            &message,
            &["Cargo.toml"],
            Some(released),
        );

        let packages =
            packages_without_prior_notch_commit(&repo, &Config::default(), &CargoEcosystem)
                .unwrap()
                .expect("a notch commit is on the branch");

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, Version::new(1, 0, 0));

        // the branch, and the working tree it points at, are untouched
        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), bumped);
        let on_disk = fs::read_to_string(workdir.join("Cargo.toml")).unwrap();
        assert!(on_disk.contains("version = \"1.1.0\""), "got: {on_disk}");

        let _ = fs::remove_dir_all(&workdir);
    }

    // No notch commit means the working tree already is the baseline, so no worktree is created
    // and the caller is told to read it directly.
    #[test]
    fn the_baseline_read_declines_when_there_is_no_prior_bump() {
        let (repo, base) = repo_with_upstream("baseline-none");
        let workdir = repo.workdir().unwrap().to_path_buf();
        let human = Signature::now("a human", "human@example.com").unwrap();

        write_crate(&workdir, "1.0.0");
        commit_paths(
            &repo,
            &human,
            "feat: a thing",
            &["Cargo.toml", "src/lib.rs"],
            Some(base),
        );

        let packages =
            packages_without_prior_notch_commit(&repo, &Config::default(), &CargoEcosystem)
                .unwrap();

        assert!(packages.is_none());

        let _ = fs::remove_dir_all(&workdir);
    }

    // Two notch commits stack up when a run is interrupted between them. Dropping only the most
    // recent would leave the first bump standing and then bump on top of it, double-counting the
    // release — so every notch commit has to go, not just the one `find_last_notch_commit` names.
    #[test]
    fn every_stacked_notch_commit_is_dropped_not_just_the_most_recent() {
        let (repo, base) = repo_with_upstream("drop-stacked");
        let sig = notch_signature().unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();

        let first = notch_commit_message("foo", (1, 0, 0), Version::new(1, 1, 0));
        let first = commit_onto(&repo, &sig, &first, "b.txt", Some(base));
        let second = notch_commit_message("foo", (1, 1, 0), Version::new(1, 2, 0));
        commit_onto(&repo, &sig, &second, "c.txt", Some(first));

        drop_notch_commits(&repo, &workdir, &ReleaseConfig::default()).unwrap();

        let commits = get_commits(&repo, &ReleaseConfig::default()).unwrap();
        assert!(
            !commits.iter().any(is_notch_commit),
            "notch commits left on the branch: {commits:?}"
        );

        let _ = fs::remove_dir_all(&workdir);
    }

    // The other half of the same contract: a branch notch has never touched must come back
    // untouched, without a stray rebase moving commits around for no reason.
    #[test]
    fn a_branch_with_no_notch_commit_is_left_exactly_as_it_was() {
        let (repo, base) = repo_with_upstream("drop-none");
        let human = Signature::now("a human", "human@example.com").unwrap();
        let head = commit_onto(&repo, &human, "fix: something", "b.txt", Some(base));
        let workdir = repo.workdir().unwrap().to_path_buf();

        drop_notch_commits(&repo, &workdir, &ReleaseConfig::default()).unwrap();

        assert_eq!(repo.head().unwrap().peel_to_commit().unwrap().id(), head);

        let _ = fs::remove_dir_all(&workdir);
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
