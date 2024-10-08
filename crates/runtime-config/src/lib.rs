use std::path::{Path, PathBuf};

use anyhow::Context as _;
use spin_common::ui::quoted_path;
use spin_factor_key_value::runtime_config::spin::{self as key_value};
use spin_factor_key_value::{DefaultLabelResolver as _, KeyValueFactor};
use spin_factor_llm::{spin as llm, LlmFactor};
use spin_factor_outbound_http::OutboundHttpFactor;
use spin_factor_outbound_mqtt::OutboundMqttFactor;
use spin_factor_outbound_mysql::OutboundMysqlFactor;
use spin_factor_outbound_networking::runtime_config::spin::SpinTlsRuntimeConfig;
use spin_factor_outbound_networking::OutboundNetworkingFactor;
use spin_factor_outbound_pg::OutboundPgFactor;
use spin_factor_outbound_redis::OutboundRedisFactor;
use spin_factor_sqlite::SqliteFactor;
use spin_factor_variables::{spin_cli as variables, VariablesFactor};
use spin_factor_wasi::WasiFactor;
use spin_factors::runtime_config::toml::GetTomlValue as _;
use spin_factors::{
    runtime_config::toml::TomlKeyTracker, FactorRuntimeConfigSource, RuntimeConfigSourceFinalizer,
};
use spin_key_value_spin::{SpinKeyValueRuntimeConfig, SpinKeyValueStore};
use spin_sqlite as sqlite;
use spin_trigger::cli::UserProvidedPath;
use toml::Value;

/// The default state directory for the trigger.
pub const DEFAULT_STATE_DIR: &str = ".spin";

/// A runtime configuration which has been resolved from a runtime config source.
///
/// Includes other pieces of configuration that are used to resolve the runtime configuration.
pub struct ResolvedRuntimeConfig<T> {
    /// The resolved runtime configuration.
    pub runtime_config: T,
    /// The resolver used to resolve key-value stores from runtime configuration.
    pub key_value_resolver: key_value::RuntimeConfigResolver,
    /// The resolver used to resolve sqlite databases from runtime configuration.
    pub sqlite_resolver: sqlite::RuntimeConfigResolver,
    /// The fully resolved state directory.
    ///
    /// `None` is used for an "unset" state directory which each factor will treat differently.
    pub state_dir: Option<PathBuf>,
    /// The fully resolved log directory.
    ///
    /// `None` is used for an "unset" log directory.
    pub log_dir: Option<PathBuf>,
    /// The input TOML, for informational summaries.
    pub toml: toml::Table,
}

impl<T> ResolvedRuntimeConfig<T> {
    pub fn summarize(&self, runtime_config_path: Option<&Path>) {
        let summarize_labeled_typed_tables = |key| {
            let mut summaries = vec![];
            if let Some(tables) = self.toml.get(key).and_then(Value::as_table) {
                for (label, config) in tables {
                    if let Some(ty) = config.get("type").and_then(Value::as_str) {
                        summaries.push(format!("[{key}.{label}: {ty}]"))
                    }
                }
            }
            summaries
        };

        let mut summaries = vec![];
        // [key_value_store.<label>: <type>]
        summaries.extend(summarize_labeled_typed_tables("key_value_store"));
        // [sqlite_database.<label>: <type>]
        summaries.extend(summarize_labeled_typed_tables("sqlite_database"));
        // [llm_compute: <type>]
        if let Some(table) = self.toml.get("llm_compute").and_then(Value::as_table) {
            if let Some(ty) = table.get("type").and_then(Value::as_str) {
                summaries.push(format!("[llm_compute: {ty}"));
            }
        }
        if !summaries.is_empty() {
            let summaries = summaries.join(", ");
            let from_path = runtime_config_path
                .map(|path| format!("from {}", quoted_path(path)))
                .unwrap_or_default();
            println!("Using runtime config {summaries} {from_path}");
        }
    }
}

impl<T> ResolvedRuntimeConfig<T>
where
    T: for<'a, 'b> TryFrom<TomlRuntimeConfigSource<'a, 'b>>,
    for<'a, 'b> <T as TryFrom<TomlRuntimeConfigSource<'a, 'b>>>::Error: Into<anyhow::Error>,
{
    /// Creates a new resolved runtime configuration from a runtime config source TOML file.
    ///
    /// `provided_state_dir` is the explicitly provided state directory, if any.
    pub fn from_file(
        runtime_config_path: Option<&Path>,
        local_app_dir: Option<PathBuf>,
        provided_state_dir: UserProvidedPath,
        provided_log_dir: UserProvidedPath,
        use_gpu: bool,
    ) -> anyhow::Result<Self> {
        let toml = match runtime_config_path {
            Some(runtime_config_path) => {
                let file = std::fs::read_to_string(runtime_config_path).with_context(|| {
                    format!(
                        "failed to read runtime config file '{}'",
                        runtime_config_path.display()
                    )
                })?;
                toml::from_str(&file).with_context(|| {
                    format!(
                        "failed to parse runtime config file '{}' as toml",
                        runtime_config_path.display()
                    )
                })?
            }
            None => Default::default(),
        };
        let toml_resolver =
            TomlResolver::new(&toml, local_app_dir, provided_state_dir, provided_log_dir);

        Self::new(toml_resolver, runtime_config_path, use_gpu)
    }

    /// Creates a new resolved runtime configuration from a TOML table.
    pub fn new(
        toml_resolver: TomlResolver<'_>,
        runtime_config_path: Option<&Path>,
        use_gpu: bool,
    ) -> anyhow::Result<Self> {
        let runtime_config_dir = runtime_config_path
            .and_then(Path::parent)
            .map(ToOwned::to_owned);
        let tls_resolver = runtime_config_dir.clone().map(SpinTlsRuntimeConfig::new);
        let key_value_config_resolver =
            key_value_config_resolver(runtime_config_dir, toml_resolver.state_dir()?);
        let sqlite_config_resolver = sqlite_config_resolver(toml_resolver.state_dir()?)
            .context("failed to resolve sqlite runtime config")?;

        let source = TomlRuntimeConfigSource::new(
            toml_resolver.clone(),
            &key_value_config_resolver,
            tls_resolver.as_ref(),
            &sqlite_config_resolver,
            use_gpu,
        );
        let runtime_config: T = source.try_into().map_err(Into::into)?;

        Ok(Self {
            runtime_config,
            key_value_resolver: key_value_config_resolver,
            sqlite_resolver: sqlite_config_resolver,
            state_dir: toml_resolver.state_dir()?,
            log_dir: toml_resolver.log_dir()?,
            toml: toml_resolver.toml(),
        })
    }

    /// Set initial key-value pairs supplied in the CLI arguments in the default store.
    pub async fn set_initial_key_values(
        &self,
        initial_key_values: impl IntoIterator<Item = &(String, String)>,
    ) -> anyhow::Result<()> {
        // We don't want to unnecessarily interact with the default store
        let mut iter = initial_key_values.into_iter().peekable();
        if iter.peek().is_none() {
            return Ok(());
        }

        let store = self
            .key_value_resolver
            .default(DEFAULT_KEY_VALUE_STORE_LABEL)
            .expect("trigger was misconfigured and lacks a default store")
            .get(DEFAULT_KEY_VALUE_STORE_LABEL)
            .await
            .expect("trigger was misconfigured and lacks a default store");
        for (key, value) in iter {
            store
                .set(key, value.as_bytes())
                .await
                .context("failed to set key-value pair")?;
        }
        Ok(())
    }

    /// The fully resolved state directory.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.state_dir.clone()
    }

    /// The fully resolved state directory.
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.log_dir.clone()
    }
}

#[derive(Clone, Debug)]
/// Resolves runtime configuration from a TOML file.
pub struct TomlResolver<'a> {
    table: TomlKeyTracker<'a>,
    /// The local app directory.
    local_app_dir: Option<PathBuf>,
    /// Explicitly provided state directory.
    state_dir: UserProvidedPath,
    /// Explicitly provided log directory.
    log_dir: UserProvidedPath,
}

impl<'a> TomlResolver<'a> {
    /// Create a new TOML resolver.
    pub fn new(
        table: &'a toml::Table,
        local_app_dir: Option<PathBuf>,
        state_dir: UserProvidedPath,
        log_dir: UserProvidedPath,
    ) -> Self {
        Self {
            table: TomlKeyTracker::new(table),
            local_app_dir,
            state_dir,
            log_dir,
        }
    }

    /// Get the configured state_directory.
    ///
    /// Errors if the path cannot be converted to an absolute path.
    pub fn state_dir(&self) -> std::io::Result<Option<PathBuf>> {
        let mut state_dir = self.state_dir.clone();
        // If the state_dir is not explicitly provided, check the toml.
        if matches!(state_dir, UserProvidedPath::Default) {
            let from_toml =
                self.table
                    .get("state_dir")
                    .and_then(|v| v.as_str())
                    .map(|toml_value| {
                        if toml_value.is_empty() {
                            // If the toml value is empty, treat it as unset.
                            UserProvidedPath::Unset
                        } else {
                            // Otherwise, treat the toml value as a provided path.
                            UserProvidedPath::Provided(PathBuf::from(toml_value))
                        }
                    });
            // If toml value is not provided, use the original value after all.
            state_dir = from_toml.unwrap_or(state_dir);
        }

        match (state_dir, &self.local_app_dir) {
            (UserProvidedPath::Provided(p), _) => Ok(Some(std::path::absolute(p)?)),
            (UserProvidedPath::Default, Some(local_app_dir)) => {
                Ok(Some(local_app_dir.join(".spin")))
            }
            (UserProvidedPath::Default | UserProvidedPath::Unset, _) => Ok(None),
        }
    }

    /// Get the configured log directory.
    ///
    /// Errors if the path cannot be converted to an absolute path.
    pub fn log_dir(&self) -> std::io::Result<Option<PathBuf>> {
        let mut log_dir = self.log_dir.clone();
        // If the log_dir is not explicitly provided, check the toml.
        if matches!(log_dir, UserProvidedPath::Default) {
            let from_toml = self
                .table
                .get("log_dir")
                .and_then(|v| v.as_str())
                .map(|toml_value| {
                    if toml_value.is_empty() {
                        // If the toml value is empty, treat it as unset.
                        UserProvidedPath::Unset
                    } else {
                        // Otherwise, treat the toml value as a provided path.
                        UserProvidedPath::Provided(PathBuf::from(toml_value))
                    }
                });
            // If toml value is not provided, use the original value after all.
            log_dir = from_toml.unwrap_or(log_dir);
        }

        match log_dir {
            UserProvidedPath::Provided(p) => Ok(Some(std::path::absolute(p)?)),
            UserProvidedPath::Default => Ok(self.state_dir()?.map(|p| p.join("logs"))),
            UserProvidedPath::Unset => Ok(None),
        }
    }

    /// Validate that all keys in the TOML file have been used.
    pub fn validate_all_keys_used(&self) -> spin_factors::Result<()> {
        self.table.validate_all_keys_used()
    }

    fn toml(&self) -> toml::Table {
        self.table.as_ref().clone()
    }
}

/// The TOML based runtime configuration source Spin CLI.
pub struct TomlRuntimeConfigSource<'a, 'b> {
    toml: TomlResolver<'b>,
    key_value: &'a key_value::RuntimeConfigResolver,
    tls: Option<&'a SpinTlsRuntimeConfig>,
    sqlite: &'a sqlite::RuntimeConfigResolver,
    use_gpu: bool,
}

impl<'a, 'b> TomlRuntimeConfigSource<'a, 'b> {
    pub fn new(
        toml_resolver: TomlResolver<'b>,
        key_value: &'a key_value::RuntimeConfigResolver,
        tls: Option<&'a SpinTlsRuntimeConfig>,
        sqlite: &'a sqlite::RuntimeConfigResolver,
        use_gpu: bool,
    ) -> Self {
        Self {
            toml: toml_resolver,
            key_value,
            tls,
            sqlite,
            use_gpu,
        }
    }
}

impl FactorRuntimeConfigSource<KeyValueFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(
        &mut self,
    ) -> anyhow::Result<Option<spin_factor_key_value::RuntimeConfig>> {
        self.key_value.resolve_from_toml(Some(&self.toml.table))
    }
}

impl FactorRuntimeConfigSource<OutboundNetworkingFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(
        &mut self,
    ) -> anyhow::Result<Option<<OutboundNetworkingFactor as spin_factors::Factor>::RuntimeConfig>>
    {
        let Some(tls) = self.tls else {
            return Ok(None);
        };
        tls.config_from_table(&self.toml.table)
    }
}

impl FactorRuntimeConfigSource<VariablesFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(
        &mut self,
    ) -> anyhow::Result<Option<<VariablesFactor as spin_factors::Factor>::RuntimeConfig>> {
        Ok(Some(variables::runtime_config_from_toml(&self.toml.table)?))
    }
}

impl FactorRuntimeConfigSource<OutboundPgFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<OutboundMysqlFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<LlmFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<spin_factor_llm::RuntimeConfig>> {
        llm::runtime_config_from_toml(&self.toml.table, self.toml.state_dir()?, self.use_gpu)
    }
}

impl FactorRuntimeConfigSource<OutboundRedisFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<WasiFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<OutboundHttpFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<OutboundMqttFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<()>> {
        Ok(None)
    }
}

impl FactorRuntimeConfigSource<SqliteFactor> for TomlRuntimeConfigSource<'_, '_> {
    fn get_runtime_config(&mut self) -> anyhow::Result<Option<spin_factor_sqlite::RuntimeConfig>> {
        self.sqlite.resolve_from_toml(&self.toml.table)
    }
}

impl RuntimeConfigSourceFinalizer for TomlRuntimeConfigSource<'_, '_> {
    fn finalize(&mut self) -> anyhow::Result<()> {
        Ok(self.toml.validate_all_keys_used()?)
    }
}

const DEFAULT_KEY_VALUE_STORE_LABEL: &str = "default";

/// The key-value runtime configuration resolver.
///
/// Takes a base path that all local key-value stores which are configured with
/// relative paths will be relative to. It also takes a default store base path
/// which will be used as the directory for the default store.
pub fn key_value_config_resolver(
    local_store_base_path: Option<PathBuf>,
    default_store_base_path: Option<PathBuf>,
) -> key_value::RuntimeConfigResolver {
    let mut key_value = key_value::RuntimeConfigResolver::new();

    // Register the supported store types.
    // Unwraps are safe because the store types are known to not overlap.
    key_value
        .register_store_type(spin_key_value_spin::SpinKeyValueStore::new(
            local_store_base_path.clone(),
        ))
        .unwrap();
    key_value
        .register_store_type(spin_key_value_redis::RedisKeyValueStore::new())
        .unwrap();
    key_value
        .register_store_type(spin_key_value_azure::AzureKeyValueStore::new())
        .unwrap();

    // Add handling of "default" store.
    let default_store_path = default_store_base_path.map(|p| p.join(DEFAULT_SPIN_STORE_FILENAME));
    // Unwraps are safe because the store is known to be serializable as toml.
    key_value
        .add_default_store::<SpinKeyValueStore>(
            DEFAULT_KEY_VALUE_STORE_LABEL,
            SpinKeyValueRuntimeConfig::new(default_store_path),
        )
        .unwrap();

    key_value
}

/// The default filename for the SQLite database.
const DEFAULT_SPIN_STORE_FILENAME: &str = "sqlite_key_value.db";

/// The sqlite runtime configuration resolver.
///
/// Takes a path to the directory where the default database should be stored.
/// If the path is `None`, the default database will be in-memory.
fn sqlite_config_resolver(
    default_database_dir: Option<PathBuf>,
) -> anyhow::Result<sqlite::RuntimeConfigResolver> {
    let local_database_dir =
        std::env::current_dir().context("failed to get current working directory")?;
    Ok(sqlite::RuntimeConfigResolver::new(
        default_database_dir,
        local_database_dir,
    ))
}
