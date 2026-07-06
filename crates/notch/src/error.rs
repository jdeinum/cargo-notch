// If we decide to change our error type, we can later on
pub type Error = anyhow::Error;
pub type Result<T> = std::result::Result<T, Error>;
