use crate::error::{Error, Result};
use crate::utils::command::run_command_in;
use crate::utils::package::Package;
use crate::utils::packages::traits::Ecosystem;
use anyhow::Context;
use cargo_metadata::MetadataCommand;
use cargo_metadata::semver::{Version, VersionReq};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};
use tracing::{debug, warn};

/// The cargo ecosystem: workspace members discovered via `cargo metadata`, versions written back
/// to their `Cargo.toml`, and a single workspace-root `Cargo.lock` refreshed afterwards.
pub struct CargoEcosystem;

impl Ecosystem for CargoEcosystem {
    fn packages(&self, root: &Path) -> Result<Vec<Package>> {
        let metadata = MetadataCommand::new()
            .current_dir(root)
            .exec()
            .context("run cargo metadata")?;
        let members = metadata.workspace_members;
        let packages = metadata.packages;
        debug!("Members: {members:?}");

        // Strip against the workspace root cargo itself reports, not the caller's
        // `root`: cargo always emits member ids as absolute paths, so a relative or
        // non-canonical `root` (like the "." commit::commit passes) would never prefix-match
        // and every member would silently keep its absolute path.
        let workspace_root = metadata.workspace_root.as_str();

        // clean up the members
        let mut cleaned_members: Vec<Package> = members
            .iter()
            .map(|s| {
                let raw_path = s
                    .repr
                    .replace("path+file://", "")
                    .split('#')
                    .next()
                    .unwrap()
                    .to_string();

                // strip the workspace root prefix to get a repo-relative path;
                // a member whose manifest sits at the root itself (e.g. a
                // single, non-workspace crate) has no trailing slash to strip
                // against, so it's normalized to "." rather than left as an
                // absolute path (which would never prefix-match the relative
                // paths in a git diff)
                let relative = raw_path
                    .strip_prefix(workspace_root)
                    .map_or(raw_path.as_str(), |rest| rest.trim_start_matches('/'));
                let path = if relative.is_empty() { "." } else { relative }.to_string();

                let package = packages.iter().find(|p| p.id == *s).unwrap();
                let manifest = PathBuf::from(join_path(&path, "Cargo.toml"));
                Package::new(
                    path,
                    package.name.to_string(),
                    package.version.clone(),
                    manifest,
                )
            })
            .collect();

        // Subtract each member from any member whose directory contains it. Without this a
        // workspace root that is *also* a package claims every file in the repo — including its
        // own members' — so touching one member bumps the root alongside it. The same goes,
        // rarely, for a member nested inside another member's directory. Cargo is what knows
        // this layout, so cargo is what has to say it; a bare list of directories can't.
        let dirs: Vec<String> = cleaned_members.iter().map(|p| p.path.clone()).collect();
        for member in &mut cleaned_members {
            for other in dirs.iter().filter(|d| contains_package(&member.path, d)) {
                debug!("{} excludes nested package {other}", member.path);
                member.paths.exclude(other.as_str());
            }
        }

        debug!("cleaned members: {cleaned_members:?}");
        Ok(cleaned_members)
    }

    fn set_versions(&self, root: &Path, bumps: &[(Package, Version)]) -> Result<Vec<PathBuf>> {
        let bumped: HashMap<&str, &Version> = bumps
            .iter()
            .map(|(package, new)| (package.name.as_str(), new))
            .collect();
        let by_manifest: HashMap<&Path, (&Package, &Version)> = bumps
            .iter()
            .map(|(package, new)| (package.manifest.as_path(), (package, new)))
            .collect();

        // Every manifest that could mention a bumped package, not just the ones being bumped: a
        // sibling that depends on one has to have its requirement widened in the same batch, or
        // the workspace is left referring to a version that no longer exists. The workspace root
        // is included even when it isn't a member, because that's where `[workspace.dependencies]`
        // lives and a virtual manifest declares no package of its own.
        let mut manifests: Vec<PathBuf> = self
            .packages(root)
            .context("find the manifests that may depend on a bumped package")?
            .into_iter()
            .map(|package| package.manifest)
            .collect();
        manifests.push(PathBuf::from("Cargo.toml"));
        manifests.sort();
        manifests.dedup();

        // Phase 1: read every manifest and compute its replacement in memory. Nothing reaches
        // disk until all of them have validated, so a package whose manifest has drifted out of
        // sync aborts the whole batch instead of leaving every package before it in the list
        // already bumped.
        let mut staged: Vec<(PathBuf, String, String)> = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            let path = root.join(&manifest);
            if !path.exists() {
                continue;
            }

            let original = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let mut doc = original
                .parse::<DocumentMut>()
                .with_context(|| format!("parse {}", manifest.display()))?;

            let bumped_itself = match by_manifest.get(manifest.as_path()) {
                Some((package, new)) => {
                    set_package_version(&mut doc, &manifest, &package.version, new)?;
                    true
                }
                None => false,
            };
            // not `||` — the dependent pass has to run even when this manifest is being bumped,
            // since a package can depend on a sibling that's in the same batch
            let changed = update_dependents(&mut doc, &manifest, &bumped) | bumped_itself;

            if changed {
                staged.push((manifest, original, doc.to_string()));
            }
        }

        // Phase 2: write. A failure here (permissions, a full disk) is past the point where it
        // can be predicted, so roll back to the originals phase 1 is still holding rather than
        // leaving a half-bumped workspace behind. Restoring is best-effort — if the write that
        // failed was caused by something that also breaks the rollback, the original error is
        // the more useful one to surface.
        for (i, (manifest, _, updated)) in staged.iter().enumerate() {
            if let Err(e) = std::fs::write(root.join(manifest), updated) {
                for (done, original, _) in &staged[..i] {
                    let _ = std::fs::write(root.join(done), original);
                }
                return Err(Error::from(e).context(format!("write bumped {}", manifest.display())));
            }
        }

        Ok(staged
            .into_iter()
            .map(|(manifest, _, _)| manifest)
            .collect())
    }

    fn refresh_lock(&self, root: &Path) -> Result<Vec<PathBuf>> {
        run_command_in(root, &["cargo", "generate-lockfile"])
            .context("call cargo generate-lockfile")?;
        // a cargo workspace has exactly one lockfile, at its root
        Ok(vec![PathBuf::from("Cargo.lock")])
    }
}

// Rewrites just the `[package] version` key, leaving the rest of the document — key order,
// spacing, comments — exactly as the author wrote it.
//
// This used to be a `replacen` of `version = "<old>"` across the raw file, which rewrites whichever
// occurrence comes first rather than the one that means the package's own version. A root manifest
// that declares `[workspace.dependencies]` above `[package]` puts a dependency's version string
// first, so the bump landed on the dependency and the package kept its old version.
fn set_package_version(
    doc: &mut DocumentMut,
    manifest: &Path,
    current: &Version,
    new: &Version,
) -> Result<()> {
    let package = doc
        .get_mut("package")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::msg(format!("{} has no [package] table", manifest.display())))?;

    let version = package.get_mut("version").ok_or_else(|| {
        Error::msg(format!(
            "{} declares no `[package] version` to bump",
            manifest.display()
        ))
    })?;

    // `version.workspace = true`: the real version lives in the root's `[workspace.package]` and is
    // shared by every member, so "bump this one package" isn't a thing that can be expressed. Worth
    // saying outright — the old code failed here with a confusing "out of sync" error instead.
    if version.get("workspace").is_some() {
        return Err(Error::msg(format!(
            "{} inherits its version from `[workspace.package]`; notch can't bump a single member \
             of a workspace that shares one version across all of them",
            manifest.display()
        )));
    }

    let value = version
        .as_value_mut()
        .ok_or_else(|| Error::msg(format!("{}'s version is not a value", manifest.display())))?;

    let on_disk = value.as_str().ok_or_else(|| {
        Error::msg(format!(
            "{}'s `[package] version` is not a string",
            manifest.display()
        ))
    })?;

    if on_disk != current.to_string() {
        return Err(Error::msg(format!(
            "expected {} to be at version `{current}`, but it's `{on_disk}` on disk — \
             the version notch is bumping from is out of sync with what's actually there",
            manifest.display()
        )));
    }

    // swap the string in place, keeping the original decor so `version = "1.0.0"` doesn't come
    // back as `version="1.0.0"` and any trailing comment survives
    let decor = value.decor().clone();
    *value = Value::from(new.to_string());
    *value.decor_mut() = decor;

    Ok(())
}

// Where cargo lets one package depend on another. `[workspace.dependencies]` and the
// `[target.'cfg(...)']` tables are reached separately, since they nest these same names.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

// Widens this manifest's requirements on any package the same batch is bumping, so a workspace
// isn't left depending on a version that no longer exists. Only requirements the new version
// stops satisfying are touched — `bar = "0.3"` against a 0.3.0 -> 0.3.1 bump is already correct,
// and rewriting it would churn the manifest for nothing.
fn update_dependents(
    doc: &mut DocumentMut,
    manifest: &Path,
    bumped: &HashMap<&str, &Version>,
) -> bool {
    let mut changed = false;

    for table in DEPENDENCY_TABLES {
        if let Some(deps) = doc.get_mut(table).and_then(Item::as_table_like_mut) {
            changed |= update_dependency_table(deps, manifest, bumped);
        }
    }

    // a workspace member writing `bar.workspace = true` inherits the requirement from here, so
    // this is the only place that kind of dependency can be corrected
    if let Some(deps) = doc
        .get_mut("workspace")
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(Item::as_table_like_mut)
    {
        changed |= update_dependency_table(deps, manifest, bumped);
    }

    // [target.'cfg(unix)'.dependencies] and friends
    if let Some(targets) = doc.get_mut("target").and_then(Item::as_table_like_mut) {
        for (_, target) in targets.iter_mut() {
            for table in DEPENDENCY_TABLES {
                if let Some(deps) = target.get_mut(table).and_then(Item::as_table_like_mut) {
                    changed |= update_dependency_table(deps, manifest, bumped);
                }
            }
        }
    }

    changed
}

fn update_dependency_table(
    deps: &mut dyn toml_edit::TableLike,
    manifest: &Path,
    bumped: &HashMap<&str, &Version>,
) -> bool {
    let mut changed = false;

    for (key, item) in deps.iter_mut() {
        // a renamed dependency (`mybar = { package = "bar", ... }`) is keyed by the rename, so the
        // real package name has to come from `package` where it's present
        let name = item
            .as_table_like()
            .and_then(|t| t.get("package"))
            .and_then(Item::as_str)
            .unwrap_or_else(|| key.get())
            .to_string();

        let Some(new) = bumped.get(name.as_str()) else {
            continue;
        };

        // `bar = "0.3"` puts the requirement directly on the key; anything else keeps it under a
        // `version` field, and a path- or git-only dependency has none at all to update
        let value = if item.is_str() {
            item.as_value_mut()
        } else {
            item.as_table_like_mut()
                .and_then(|t| t.get_mut("version"))
                .and_then(Item::as_value_mut)
        };
        let Some(value) = value else { continue };

        let Some(existing) = value.as_str() else {
            continue;
        };
        let Some(replacement) = bump_requirement(existing, new) else {
            continue;
        };

        debug!("{}: {name} {existing} -> {replacement}", manifest.display());
        let decor = value.decor().clone();
        *value = Value::from(replacement);
        *value.decor_mut() = decor;
        changed = true;
    }

    changed
}

// Rewrites a version requirement so it admits `new`, keeping the shape the author wrote it in:
// the comparison operator, and how many components they bothered to spell out. Returns `None`
// when there's nothing to do or nothing safe to do.
fn bump_requirement(existing: &str, new: &Version) -> Option<String> {
    let Ok(req) = VersionReq::parse(existing) else {
        warn!("could not parse `{existing}` as a version requirement, leaving it alone");
        return None;
    };

    // already admits the new version — `^0.3` covers a 0.3.0 -> 0.3.1 bump
    if req.matches(new) {
        return None;
    }

    let existing = existing.trim();
    let operator_len = existing.len() - existing.trim_start_matches(['^', '~', '=']).len();
    let (operator, rest) = existing.split_at(operator_len);

    // A multi-comparator requirement (">=1, <2") has no single obvious rewrite, and `>`/`<` bounds
    // aren't bumped so much as re-authored. Guessing at either risks silently loosening what the
    // author pinned, so leave them and say so.
    if rest.contains(',') || rest.contains('*') || rest.starts_with(['>', '<']) {
        warn!("`{existing}` doesn't admit {new} and is too ambiguous to rewrite, leaving it alone");
        return None;
    }

    // A prerelease can't be expressed in a truncated requirement, so those always get spelled out
    let components = rest.trim().split('.').count();
    let bumped = if new.pre.is_empty() && components == 1 {
        new.major.to_string()
    } else if new.pre.is_empty() && components == 2 {
        format!("{}.{}", new.major, new.minor)
    } else {
        new.to_string()
    };

    Some(format!("{operator}{bumped}"))
}

// Mirrors `Package::join`: a root-level package is normalized to ".", so naive concatenation
// would produce a leading "./" that libgit2 rejects.
// Whether the package directory `outer` contains the package directory `inner`. The workspace
// root is spelled "." rather than "", so it needs saying explicitly: it contains everything except
// itself.
fn contains_package(outer: &str, inner: &str) -> bool {
    if outer == inner {
        return false;
    }
    outer == "." || Path::new(inner).starts_with(outer)
}

fn join_path(path: &str, file: &str) -> String {
    if path == "." {
        file.to_string()
    } else {
        format!("{path}/{file}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "notch-package-test-{suffix}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_workspace(dir: &Path, members: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        let members = members
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            dir.join("Cargo.toml"),
            format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"),
        )
        .unwrap();
    }

    fn write_crate(dir: &Path, name: &str, version: &str) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();
    }

    // A package whose manifest sits at the workspace root (e.g. a single,
    // non-workspace crate) has a `cargo_metadata` repr with no trailing
    // slash before the `#` anchor, so naively stripping "{dir}/" leaves the
    // raw absolute path untouched. That absolute path can never prefix-match
    // the repo-relative paths in a git diff, so changed-package detection
    // silently found nothing for repos shaped like this one.
    #[test]
    fn root_crate_path_is_normalized_to_dot() {
        let dir = scratch_dir("root");
        write_crate(&dir, "root-test-crate", "0.1.0");

        let members = CargoEcosystem.packages(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
        assert_eq!(members[0].name, "root-test-crate");
        // no leading "./" — libgit2 rejects that
        assert_eq!(members[0].manifest, PathBuf::from("Cargo.toml"));
    }

    // A root crate that is *also* the workspace root owns every path in the repo, so without
    // subtracting its members every change to one of them would bump the root alongside it. Only
    // cargo knows the layout, so only cargo can say this.
    #[test]
    fn a_root_package_excludes_the_members_nested_inside_it() {
        let dir = scratch_dir("root-with-members");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n\
             members = [\"crates/a\"]\n\
             resolver = \"2\"\n\
             \n\
             [package]\n\
             name = \"root-test-crate\"\n\
             version = \"0.1.0\"\n\
             edition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();
        write_crate(&dir.join("crates/a"), "crate-a", "0.1.0");

        let members = CargoEcosystem.packages(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        let root = members.iter().find(|m| m.path == ".").unwrap();
        let a = members.iter().find(|m| m.path == "crates/a").unwrap();

        assert!(root.tracks(Path::new("src/lib.rs")));
        assert!(!root.tracks(Path::new("crates/a/src/lib.rs")));
        // and the member still owns its own tree
        assert!(a.tracks(Path::new("crates/a/src/lib.rs")));
    }

    // A member is not nested in a sibling that merely shares a name prefix, so it must not be
    // subtracted from it: `Path::starts_with` compares components, a string prefix would not.
    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_excluded() {
        let dir = scratch_dir("prefix-siblings");
        write_workspace(&dir, &["crates/a", "crates/ab"]);
        write_crate(&dir.join("crates/a"), "crate-a", "0.1.0");
        write_crate(&dir.join("crates/ab"), "crate-ab", "0.1.0");

        let members = CargoEcosystem.packages(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        let a = members.iter().find(|m| m.path == "crates/a").unwrap();

        assert!(a.tracks(Path::new("crates/a/src/lib.rs")));
        assert!(!a.tracks(Path::new("crates/ab/src/lib.rs")));
    }

    // `commit::commit` invokes this with the relative dir "." — the caller's `root` never
    // literally prefixes the absolute member paths cargo reports, so the normalization must not
    // depend on it. A non-canonical dir (`{tmp}/.`) reproduces the same mismatch without having
    // to chdir the test process.
    #[test]
    fn non_canonical_dir_still_normalizes_root_to_dot() {
        let dir = scratch_dir("relative");
        write_crate(&dir, "root-test-crate", "0.1.0");

        let members = CargoEcosystem.packages(&dir.join("."));
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, ".");
    }

    // A workspace member nested under `root` (rather than sitting at the root
    // itself) should resolve to a path relative to `root`, not the raw
    // absolute manifest path `cargo_metadata` reports.
    #[test]
    fn nested_workspace_member_path_is_relative_to_root() {
        let dir = scratch_dir("nested");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate-a\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");

        let members = CargoEcosystem.packages(&dir);
        fs::remove_dir_all(&dir).unwrap();
        let members = members.unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].path, "crate-a");
        assert_eq!(members[0].name, "crate-a");
        assert_eq!(members[0].manifest, PathBuf::from("crate-a/Cargo.toml"));
    }

    #[test]
    fn set_versions_writes_every_manifest_and_reports_what_it_touched() {
        let dir = scratch_dir("set-versions");
        write_workspace(&dir, &["crate-a", "crate-b"]);
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");
        write_crate(&dir.join("crate-b"), "crate-b", "1.0.0");

        let a = package("crate-a", "0.3.0");
        let b = package("crate-b", "1.0.0");
        let touched = CargoEcosystem
            .set_versions(
                &dir,
                &[
                    (a, Version::parse("0.4.0").unwrap()),
                    (b, Version::parse("1.0.1").unwrap()),
                ],
            )
            .unwrap();

        let a_toml = fs::read_to_string(dir.join("crate-a/Cargo.toml")).unwrap();
        let b_toml = fs::read_to_string(dir.join("crate-b/Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(a_toml.contains("version = \"0.4.0\""));
        assert!(b_toml.contains("version = \"1.0.1\""));
        assert_eq!(
            touched,
            vec![
                PathBuf::from("crate-a/Cargo.toml"),
                PathBuf::from("crate-b/Cargo.toml"),
            ]
        );
    }

    // The reason this is one batched call rather than one call per package: a package whose
    // manifest has drifted out of sync used to fail *after* the packages before it in the list
    // had already been written, leaving a half-bumped workspace to unpick by hand.
    #[test]
    fn a_package_that_cannot_be_bumped_leaves_every_other_manifest_untouched() {
        let dir = scratch_dir("set-versions-atomic");
        write_workspace(&dir, &["crate-a", "crate-b"]);
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");
        // on disk as 1.0.0, but the Package we hand set_versions claims 9.9.9
        write_crate(&dir.join("crate-b"), "crate-b", "1.0.0");

        let a = package("crate-a", "0.3.0");
        let stale = package("crate-b", "9.9.9");
        let err = CargoEcosystem
            .set_versions(
                &dir,
                &[
                    (a, Version::parse("0.4.0").unwrap()),
                    (stale, Version::parse("9.9.10").unwrap()),
                ],
            )
            .unwrap_err();

        let a_toml = fs::read_to_string(dir.join("crate-a/Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(err.to_string().contains("out of sync"), "got: {err}");
        // crate-a comes first in the batch, so a per-package loop would already have bumped it
        assert!(
            a_toml.contains("version = \"0.3.0\""),
            "crate-a was left bumped: {a_toml}"
        );
    }

    fn package(path: &str, version: &str) -> Package {
        Package::new(
            path.to_string(),
            path.to_string(),
            Version::parse(version).unwrap(),
            PathBuf::from(format!("{path}/Cargo.toml")),
        )
    }

    // Regression: `set_versions` used to `replacen` the raw string `version = "<old>"`, which
    // rewrites whichever occurrence appears first in the file rather than the package's own. A
    // root manifest declaring `[workspace.dependencies]` above `[package]` — an ordinary layout —
    // put a dependency's identical version string first, so the bump landed on the dependency and
    // the package itself was never bumped.
    #[test]
    fn a_dependency_sharing_the_version_string_is_not_bumped_instead_of_the_package() {
        let dir = scratch_dir("set-versions-dep-first");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace.dependencies]\n\
             helper = { version = \"0.3.0\", path = \"helper\" }\n\
             \n\
             [package]\n\
             name = \"root\"\n\
             version = \"0.3.0\"\n\
             edition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let root_package = Package::new(
            ".".to_string(),
            "root".to_string(),
            Version::parse("0.3.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );

        CargoEcosystem
            .set_versions(&dir, &[(root_package, Version::parse("0.4.0").unwrap())])
            .unwrap();

        let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(
            toml.contains("helper = { version = \"0.3.0\", path = \"helper\" }"),
            "the dependency was bumped instead: {toml}"
        );
        assert!(
            toml.contains("version = \"0.4.0\""),
            "the package was not bumped: {toml}"
        );
    }

    // Bumping a version shouldn't reformat the file around it or drop the author's comments.
    #[test]
    fn everything_around_the_version_survives_the_bump() {
        let dir = scratch_dir("set-versions-formatting");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\n\
             name = \"root\"\n\
             version = \"0.3.0\" # keep me\n\
             edition = \"2021\"\n\
             \n\
             # a section comment\n\
             [dependencies]\n\
             anyhow = \"1\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let root_package = Package::new(
            ".".to_string(),
            "root".to_string(),
            Version::parse("0.3.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );

        CargoEcosystem
            .set_versions(&dir, &[(root_package, Version::parse("0.4.0").unwrap())])
            .unwrap();

        let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(toml.contains("version = \"0.4.0\" # keep me"), "{toml}");
        assert!(toml.contains("# a section comment"), "{toml}");
        assert!(toml.contains("anyhow = \"1\""), "{toml}");
    }

    // A workspace that shares one version across every member can't have a single member bumped.
    // Saying so beats the old behaviour, which failed with a confusing "out of sync" error.
    #[test]
    fn a_workspace_inherited_version_is_refused_with_a_clear_error() {
        let dir = scratch_dir("set-versions-inherited");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\n\
             members = []\n\
             resolver = \"2\"\n\
             \n\
             [workspace.package]\n\
             version = \"0.3.0\"\n\
             \n\
             [package]\n\
             name = \"member\"\n\
             version.workspace = true\n\
             edition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let member = Package::new(
            ".".to_string(),
            "member".to_string(),
            Version::parse("0.3.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );

        let err = CargoEcosystem
            .set_versions(&dir, &[(member, Version::parse("0.4.0").unwrap())])
            .unwrap_err();
        fs::remove_dir_all(&dir).unwrap();

        assert!(
            err.to_string().contains("[workspace.package]"),
            "got: {err}"
        );
    }

    // The other half of the `replacen` bug: bumping a package used to leave every sibling still
    // requiring the version that no longer exists, so the workspace stopped resolving.
    #[test]
    fn a_sibling_depending_on_a_bumped_package_has_its_requirement_widened() {
        let dir = scratch_dir("set-versions-dependents");
        write_workspace(&dir, &["crate-a", "crate-b"]);
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");
        write_crate(&dir.join("crate-b"), "crate-b", "1.0.0");
        fs::write(
            dir.join("crate-b/Cargo.toml"),
            "[package]\n\
             name = \"crate-b\"\n\
             version = \"1.0.0\"\n\
             edition = \"2021\"\n\
             \n\
             [dependencies]\n\
             crate-a = { version = \"0.3\", path = \"../crate-a\" }\n",
        )
        .unwrap();

        let touched = CargoEcosystem
            .set_versions(
                &dir,
                &[(
                    package("crate-a", "0.3.0"),
                    Version::parse("0.4.0").unwrap(),
                )],
            )
            .unwrap();

        let a = fs::read_to_string(dir.join("crate-a/Cargo.toml")).unwrap();
        let b = fs::read_to_string(dir.join("crate-b/Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(a.contains("version = \"0.4.0\""), "{a}");
        // "0.3" is `^0.3`, which 0.4.0 no longer satisfies; the two-component shape is kept
        assert!(
            b.contains("crate-a = { version = \"0.4\", path = \"../crate-a\" }"),
            "{b}"
        );
        // crate-b itself is not being released, so its own version stays put
        assert!(b.contains("version = \"1.0.0\""), "{b}");
        assert!(
            touched.contains(&PathBuf::from("crate-b/Cargo.toml")),
            "the dependent was rewritten but not reported: {touched:?}"
        );
    }

    // A patch bump inside the range a requirement already covers needs no rewrite; doing one
    // anyway churns manifests that had nothing wrong with them.
    #[test]
    fn a_requirement_that_already_admits_the_new_version_is_left_alone() {
        let dir = scratch_dir("set-versions-no-churn");
        write_workspace(&dir, &["crate-a", "crate-b"]);
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");
        write_crate(&dir.join("crate-b"), "crate-b", "1.0.0");
        fs::write(
            dir.join("crate-b/Cargo.toml"),
            "[package]\n\
             name = \"crate-b\"\n\
             version = \"1.0.0\"\n\
             edition = \"2021\"\n\
             \n\
             [dependencies]\n\
             crate-a = { version = \"0.3\", path = \"../crate-a\" }\n",
        )
        .unwrap();

        let touched = CargoEcosystem
            .set_versions(
                &dir,
                &[(
                    package("crate-a", "0.3.0"),
                    Version::parse("0.3.1").unwrap(),
                )],
            )
            .unwrap();

        let b = fs::read_to_string(dir.join("crate-b/Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(b.contains("version = \"0.3\""), "{b}");
        assert_eq!(touched, vec![PathBuf::from("crate-a/Cargo.toml")]);
    }

    #[test]
    fn bump_requirement_keeps_the_operator_and_component_count() {
        let new = Version::parse("2.0.0").unwrap();

        assert_eq!(bump_requirement("1", &new).as_deref(), Some("2"));
        assert_eq!(bump_requirement("1.5", &new).as_deref(), Some("2.0"));
        assert_eq!(bump_requirement("1.5.2", &new).as_deref(), Some("2.0.0"));
        assert_eq!(bump_requirement("^1.5.2", &new).as_deref(), Some("^2.0.0"));
        assert_eq!(bump_requirement("~1.5.2", &new).as_deref(), Some("~2.0.0"));
        assert_eq!(bump_requirement("=1.5.2", &new).as_deref(), Some("=2.0.0"));
    }

    #[test]
    fn bump_requirement_leaves_alone_what_it_cannot_rewrite_safely() {
        let new = Version::parse("2.0.0").unwrap();

        // already satisfied
        assert_eq!(
            bump_requirement("^1.5", &Version::parse("1.6.0").unwrap()),
            None
        );
        // no single obvious rewrite
        assert_eq!(bump_requirement(">=1, <2", &new), None);
        assert_eq!(bump_requirement(">1.0", &new), None);
        assert_eq!(bump_requirement("*", &new), None);
        // not a requirement at all
        assert_eq!(bump_requirement("not-a-req", &new), None);
    }

    // A truncated requirement can't express a prerelease, so those are always spelled out in full.
    #[test]
    fn a_prerelease_is_always_written_out_in_full() {
        let new = Version::parse("2.0.0-rc.1").unwrap();

        assert_eq!(bump_requirement("1.5", &new).as_deref(), Some("2.0.0-rc.1"));
    }

    // A path-only dependency declares no requirement, so there's nothing that can go stale and
    // nothing to rewrite — this is the case `cargo update` genuinely does handle on its own. The
    // `version` key is what makes a path dep publishable, and only its presence creates a
    // requirement that a bump can invalidate.
    #[test]
    fn a_path_only_dependency_has_no_requirement_to_widen() {
        let dir = scratch_dir("set-versions-path-only");
        write_workspace(&dir, &["crate-a", "crate-b"]);
        write_crate(&dir.join("crate-a"), "crate-a", "0.3.0");
        write_crate(&dir.join("crate-b"), "crate-b", "1.0.0");
        fs::write(
            dir.join("crate-b/Cargo.toml"),
            "[package]\n\
             name = \"crate-b\"\n\
             version = \"1.0.0\"\n\
             edition = \"2021\"\n\
             \n\
             [dependencies]\n\
             crate-a = { path = \"../crate-a\" }\n",
        )
        .unwrap();

        let touched = CargoEcosystem
            .set_versions(
                &dir,
                &[(
                    package("crate-a", "0.3.0"),
                    Version::parse("0.4.0").unwrap(),
                )],
            )
            .unwrap();

        let b = fs::read_to_string(dir.join("crate-b/Cargo.toml")).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        assert!(b.contains("crate-a = { path = \"../crate-a\" }"), "{b}");
        assert_eq!(touched, vec![PathBuf::from("crate-a/Cargo.toml")]);
    }
}
