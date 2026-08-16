use crate::error::Result;
use crate::utils::package::Package;
use std::collections::HashSet;

/// Defines our packages in our system
pub trait Packages {
    fn get(&self) -> Result<HashSet<Package>>;
}
