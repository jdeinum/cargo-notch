use crate::error::{Error, Result};
use anyhow::Context;
use fd_lock::RwLock;
use git2::Repository;
use std::fs::File;

/// Guards against two notch processes (`commit` or `pr`, which runs `commit` itself) operating
/// on the same repo at once: `commit` fetches, walks history to find the prior notch commit, and
/// rewrites `HEAD` via an external `git rebase` — none of that is safe to interleave with another
/// notch process doing the same thing in the same working directory, since there's no other
/// synchronization between the two (see `drop_prior_notch_commit`). Acquired via a real OS file
/// lock, so it's automatically released if the process crashes or is killed — no stale lock file
/// to clean up by hand.
///
/// Deliberately leaked rather than returned as a guard: notch never explicitly unlocks mid-run
/// (e.g. `pr::run` keeps mutating the repo — pushing — well after `commit` returns), so the lock
/// is meant to live for the rest of the process anyway. The OS releases it when the process exits.
pub fn acquire_repo_lock(repo: &Repository) -> Result<()> {
    let lock_path = repo.path().join("notch.lock");
    let file = File::create(&lock_path).context("create notch lock file")?;

    let mut lock = RwLock::new(file);
    let guard = lock.try_write().map_err(|_| {
        Error::msg(
            "another notch process is already running against this repo, try again once it finishes",
        )
    })?;

    // leak the guard and the lock it borrows from so the underlying fd — and thus the OS-level
    // lock it holds — stays open for the rest of the process instead of being released on drop
    std::mem::forget(guard);
    std::mem::forget(lock);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo(name: &str) -> Repository {
        let dir =
            std::env::temp_dir().join(format!("notch-lock-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Repository::init(&dir).unwrap()
    }

    fn cleanup(repo: &Repository) {
        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn a_second_lock_on_the_same_repo_is_rejected() {
        let repo = init_repo("contended");

        // first process claims the lock and never releases it — same as the real caller, which
        // leaks it for the rest of its own lifetime rather than dropping it early
        acquire_repo_lock(&repo).unwrap();

        // a second attempt against the same repo, from a fresh file handle (standing in for a
        // second notch process), must not be able to also acquire it
        assert!(acquire_repo_lock(&repo).is_err());

        cleanup(&repo);
    }

    #[test]
    fn locking_one_repo_does_not_affect_another() {
        let repo_a = init_repo("independent-a");
        let repo_b = init_repo("independent-b");

        acquire_repo_lock(&repo_a).unwrap();
        assert!(acquire_repo_lock(&repo_b).is_ok());

        cleanup(&repo_a);
        cleanup(&repo_b);
    }
}
