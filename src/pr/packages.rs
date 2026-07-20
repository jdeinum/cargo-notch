use crate::error::Result;
use crate::package::{Package, get_cleaned_members};
use crate::pr::traits::Packages;
use std::collections::HashSet;
use std::path::Path;

pub struct CargoPackager {
    dir: String,
}

impl CargoPackager {
    pub const fn new(dir: String) -> Self {
        Self { dir }
    }
}

impl Packages for CargoPackager {
    fn get(&self) -> Result<HashSet<Package>> {
        Ok(get_cleaned_members(Path::new(&self.dir))?
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::semver::Version;
    use std::fs;

    // Thin sanity check that `CargoPackager::get()` actually delegates to
    // `package::get_cleaned_members` and collects into the `HashSet` the `Packages`
    // trait requires — the path-cleaning logic itself is tested once in `package.rs`.
    #[test]
    fn get_delegates_to_get_cleaned_members() {
        let dir =
            std::env::temp_dir().join(format!("notch-cargo-packager-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"root-test-crate\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let packages = CargoPackager::new(dir.to_str().unwrap().to_string()).get();
        fs::remove_dir_all(&dir).unwrap();
        let packages = packages.unwrap();

        assert_eq!(
            packages,
            HashSet::from([Package {
                path: ".".to_string(),
                name: "root-test-crate".to_string(),
                version: Version::new(0, 2, 0),
            }])
        );
    }
}
