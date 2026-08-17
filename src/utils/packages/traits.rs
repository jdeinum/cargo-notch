use crate::error::Result;
use crate::utils::package::Package;
use cargo_metadata::semver::Version;
use std::path::{Path, PathBuf};

/// Everything that varies per language ecosystem: how packages are discovered, how a new version
/// is written back to a manifest, and how the lockfile is refreshed afterwards. The thing that
/// knows how to *read* a package's version is necessarily the thing that knows how to *write it
/// back*, so both live here rather than only discovery being abstracted and the write side being
/// hardcoded against cargo.
///
/// Deliberately not the axis [`PackageCommits`](crate::utils::commits::PackageCommits) sits on:
/// attributing commits to packages varies by VCS, not by ecosystem, and is identical whether the
/// packages came from cargo or npm. It also consumes the packages this trait produces, so it sits
/// downstream of it rather than beside it.
///
/// Every package path this trait accepts or returns is repo-relative, because those paths are
/// handed straight to git2 (`status_file`, `Index::add_path`), which rejects absolute ones.
/// `root` is the exception: it's the caller's handle on where the repo actually lives, and is
/// what the repo-relative paths resolve against.
pub trait Ecosystem {
    /// Discovers every package under `root`.
    fn packages(&self, root: &Path) -> Result<Vec<Package>>;

    /// Writes each package's new version to its manifest, all-or-nothing: if any package can't be
    /// updated, none of them are left modified. Batched rather than called once per package
    /// precisely so that guarantee is expressible — bumping five packages through five
    /// independent calls leaves the first three written when the fourth fails, which is a
    /// half-released workspace the user then has to unpick by hand.
    ///
    /// Returns the repo-relative paths it modified, so the caller can stage exactly those without
    /// knowing which files this ecosystem keeps versions in.
    fn set_versions(&self, root: &Path, bumps: &[(Package, Version)]) -> Result<Vec<PathBuf>>;

    /// Refreshes the lockfile after [`set_versions`](Ecosystem::set_versions), returning the
    /// repo-relative paths it touched. Defaults to doing nothing, for ecosystems that have no
    /// lockfile to refresh.
    fn refresh_lock(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let _ = root;
        Ok(Vec::new())
    }
}
