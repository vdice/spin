use crate::appconfig::Path;

/// Environment variable based provider.
pub mod env;

/// A config provider.
pub trait Provider {
    /// Returns the value at the given config path, if it exists.
    fn get(&self, path: &Path) -> anyhow::Result<Option<String>>;
}
