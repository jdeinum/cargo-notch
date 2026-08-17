mod cargo;
mod traits;

use crate::config::TrackingConfig;
use crate::utils::package::Package;
use tracing::warn;

pub use cargo::CargoEcosystem;
pub use traits::Ecosystem;

/// Applies the user's `[tracking]` excludes on top of what the ecosystem already worked out from
/// the workspace layout.
///
/// Kept off [`Ecosystem`] deliberately: `packages` takes no `Config` and shouldn't start to. An
/// ecosystem reports what the workspace *is*, which is derivable from the manifests; which parts
/// of it are worth cutting a release for is intent, which isn't. Folding the two together would
/// have every future ecosystem re-implementing the same config plumbing to no benefit.
///
/// Patterns are relative to each package's own directory, so a single `exclude = ["benches"]`
/// narrows every package's own `benches`. [`Package::join`] resolves them, which also normalises
/// the root package's `"."` away — a leading `./` is something libgit2 rejects outright.
pub fn narrow_to_tracked(packages: &mut [Package], tracking: &TrackingConfig) {
    for package in packages {
        let per_package = tracking.packages.get(&package.name);

        // Resolved before either is applied, and applied in no meaningful order:
        // `PathSpec::matches` settles overlaps by which prefix is more specific, not by which
        // list was built first, so `include` needn't be sequenced after the `exclude` it carves
        // back out of.
        let excluded = resolve(
            package,
            &tracking.exclude,
            per_package.map(|p| p.exclude.as_slice()),
        );
        let tracked = resolve(
            package,
            &tracking.include,
            per_package.map(|p| p.include.as_slice()),
        );

        for path in excluded {
            package.paths.exclude(path);
        }
        for path in tracked {
            package.paths.track(path);
        }
    }
}

// Resolves one list of package-relative patterns against `package`, with the global list acting as
// a baseline the per-package one adds to rather than replaces.
fn resolve(package: &Package, global: &[String], per_package: Option<&[String]>) -> Vec<String> {
    global
        .iter()
        .chain(per_package.unwrap_or_default())
        .filter_map(|pattern| normalize(pattern, &package.name))
        .map(|pattern| package.join(&pattern))
        .collect()
}

// A pattern that resolves to the package's own directory is meaningless in both lists and harmful
// in one: as an exclude it pins the package at its current version with no indication why, and as
// an include it does nothing, since the package already tracks its own directory. `""`, `"."`,
// `"/"` and `"./"` all land there once joined, so they're refused loudly rather than honoured
// literally.
fn normalize(pattern: &str, package: &str) -> Option<String> {
    let trimmed = pattern.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        warn!("ignoring tracking pattern {pattern:?} for {package}: it matches the whole package");
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PackageTracking;
    use cargo_metadata::semver::Version;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_global_exclude_resolves_against_each_packages_own_directory() {
        let mut packages = [package("crates/a"), package("crates/b")];

        narrow_to_tracked(&mut packages, &tracking(&["benches"], HashMap::new()));

        assert!(!packages[0].tracks(Path::new("crates/a/benches/throughput.rs")));
        assert!(!packages[1].tracks(Path::new("crates/b/benches/throughput.rs")));
        // one package's excluded directory is not another's
        assert!(packages[0].tracks(Path::new("crates/a/src/lib.rs")));
    }

    #[test]
    fn a_per_package_exclude_applies_only_to_that_package() {
        let mut packages = [package("crates/a"), package("crates/b")];
        let per_package =
            HashMap::from([("crates/a".to_string(), package_tracking(&["fixtures"], &[]))]);

        narrow_to_tracked(&mut packages, &tracking(&[], per_package));

        assert!(!packages[0].tracks(Path::new("crates/a/fixtures/big.json")));
        assert!(packages[1].tracks(Path::new("crates/b/fixtures/big.json")));
    }

    // The global list is a baseline, not a default to be overwritten: a package that names its own
    // excludes still gets the shared ones.
    #[test]
    fn a_per_package_exclude_adds_to_the_global_one_rather_than_replacing_it() {
        let mut packages = [package("crates/a")];
        let per_package =
            HashMap::from([("crates/a".to_string(), package_tracking(&["fixtures"], &[]))]);

        narrow_to_tracked(&mut packages, &tracking(&["benches"], per_package));

        assert!(!packages[0].tracks(Path::new("crates/a/benches/throughput.rs")));
        assert!(!packages[0].tracks(Path::new("crates/a/fixtures/big.json")));
    }

    // "." joined onto a package is the package itself, which would freeze it forever.
    #[test]
    fn a_pattern_matching_the_whole_package_is_ignored() {
        let mut packages = [package("crates/a")];

        narrow_to_tracked(&mut packages, &tracking(&[".", "", "/"], HashMap::new()));

        assert!(packages[0].tracks(Path::new("crates/a/src/lib.rs")));
    }

    #[test]
    fn excludes_resolve_against_the_root_package_without_a_leading_dot_slash() {
        let mut packages = [package(".")];

        narrow_to_tracked(&mut packages, &tracking(&["benches"], HashMap::new()));

        assert!(!packages[0].tracks(Path::new("benches/throughput.rs")));
        assert!(packages[0].tracks(Path::new("src/lib.rs")));
    }

    // The `except tests/compat/` half. `include` is not an allowlist — it only ever wins back
    // ground a broader `exclude` took, by being the more specific prefix.
    #[test]
    fn an_include_carves_a_subdirectory_back_out_of_a_broader_exclude() {
        let mut packages = [package("crates/a")];
        let config = TrackingConfig {
            exclude: strings(&["tests"]),
            include: strings(&["tests/compat"]),
            packages: HashMap::new(),
        };

        narrow_to_tracked(&mut packages, &config);

        assert!(!packages[0].tracks(Path::new("crates/a/tests/unit.rs")));
        assert!(packages[0].tracks(Path::new("crates/a/tests/compat/v1.rs")));
        assert!(packages[0].tracks(Path::new("crates/a/src/lib.rs")));
    }

    // Both lists resolve against the package, so a per-package `include` can win back ground the
    // *global* `exclude` took — the two lists don't have to come from the same scope.
    #[test]
    fn a_per_package_include_overrides_a_global_exclude() {
        let mut packages = [package("crates/a"), package("crates/b")];
        let per_package = HashMap::from([(
            "crates/a".to_string(),
            package_tracking(&[], &["benches/regression"]),
        )]);

        narrow_to_tracked(&mut packages, &tracking(&["benches"], per_package));

        assert!(packages[0].tracks(Path::new("crates/a/benches/regression/slow.rs")));
        assert!(!packages[0].tracks(Path::new("crates/a/benches/throughput.rs")));
        // the package that asked for nothing keeps the global exclude whole
        assert!(!packages[1].tracks(Path::new("crates/b/benches/regression/slow.rs")));
    }

    fn tracking(exclude: &[&str], packages: HashMap<String, PackageTracking>) -> TrackingConfig {
        TrackingConfig {
            exclude: strings(exclude),
            include: Vec::new(),
            packages,
        }
    }

    fn package_tracking(exclude: &[&str], include: &[&str]) -> PackageTracking {
        PackageTracking {
            exclude: strings(exclude),
            include: strings(include),
        }
    }

    fn strings(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|s| (*s).to_string()).collect()
    }

    // named after its path so the per-package config in these tests has something to key on
    fn package(path: &str) -> Package {
        Package::new(
            path.to_string(),
            path.to_string(),
            Version::new(1, 0, 0),
            PathBuf::from(format!("{path}/Cargo.toml")),
        )
    }
}
