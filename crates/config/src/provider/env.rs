use anyhow::Context;

use super::Provider;

const DEFAULT_PREFIX: &str = "SPIN_APP";

/// A config Provider that uses environment variables.
pub struct EnvProvider {
    prefix: String,
}

impl EnvProvider {
    /// Creates a new EnvProvider.
    pub fn new(prefix: String) -> Self {
        Self { prefix }
    }
}

impl Default for EnvProvider {
    fn default() -> Self {
        Self {
            prefix: DEFAULT_PREFIX.to_string(),
        }
    }
}

impl Provider for EnvProvider {
    fn get(&self, path: &crate::appconfig::Path) -> anyhow::Result<Option<String>> {
        let key = format!("{}_{}", self.prefix, path.to_env_var());
        match std::env::var(&key) {
            Err(std::env::VarError::NotPresent) => Ok(None),
            other => other
                .map(Some)
                .with_context(|| format!("failed to resolve env var {}", &key)),
        }
    }
}

