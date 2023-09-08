//! Spin core execution engine
//!
//! This crate provides low-level Wasm and WASI functionality required by Spin.
//! Most of this functionality consists of wrappers around [`wasmtime`] and
//! [`wasi_common`] that narrows the flexibility of `wasmtime` to the set of
//! features used by Spin (such as only supporting `wasmtime`'s async calling style).

#![deny(missing_docs)]

mod host_component;
mod io;
mod limits;
mod preview1;
mod store;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use crossbeam_channel::Sender;
use tracing::instrument;
use wasmtime::{InstanceAllocationStrategy, PoolingAllocationConfig};
use wasmtime_wasi::preview2::Table;

use self::host_component::{HostComponents, HostComponentsBuilder};

pub use async_trait::async_trait;
pub use wasmtime::{
    self,
    component::{Component, Instance},
    Instance as ModuleInstance, Module, Trap,
};
pub use wasmtime_wasi::preview2::I32Exit;

pub use host_component::{
    AnyHostComponentDataHandle, HostComponent, HostComponentDataHandle, HostComponentsData,
};
pub use io::OutputBuffer;
pub use store::{Store, StoreBuilder, Wasi, WasiVersion};

/// The default [`EngineBuilder::epoch_tick_interval`].
pub const DEFAULT_EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
const WASM_PAGE_SIZE: u64 = 64 * 1024;

/// Global configuration for `EngineBuilder`.
///
/// This is currently only used for advanced (undocumented) use cases.
pub struct Config {
    inner: wasmtime::Config,
}

impl Config {
    /// Borrow the inner wasmtime::Config mutably.
    /// WARNING: This is inherently unstable and may break at any time!
    #[doc(hidden)]
    pub fn wasmtime_config(&mut self) -> &mut wasmtime::Config {
        &mut self.inner
    }

    /// Enable the Wasmtime compilation cache. If `path` is given it will override
    /// the system default path.
    ///
    /// For more information, see the [Wasmtime cache config documentation][docs].
    ///
    /// [docs]: https://docs.wasmtime.dev/cli-cache.html
    pub fn enable_cache(&mut self, config_path: &Option<PathBuf>) -> Result<()> {
        match config_path {
            Some(p) => self.inner.cache_config_load(p)?,
            None => self.inner.cache_config_load_default()?,
        };

        Ok(())
    }

    /// Disable the pooling instance allocator.
    pub fn disable_pooling(&mut self) -> &mut Self {
        self.inner
            .allocation_strategy(wasmtime::InstanceAllocationStrategy::OnDemand);
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut inner = wasmtime::Config::new();
        inner.async_support(true);
        inner.epoch_interruption(true);
        inner.wasm_component_model(true);

        // By default enable the pooling instance allocator in Wasmtime. This
        // drastically reduces syscall/kernel overhead for wasm execution,
        // especially in async contexts where async stacks must be allocated.
        // The general goal here is that the default settings here rarely, if
        // ever, need to be modified. As a result there aren't fine-grained
        // knobs for each of these settings just yet and instead they're
        // generally set to defaults. Environment-variable-based fallbacks are
        // supported though as an escape valve for if this is a problem.
        //
        // NB: much of this will change in Wasmtime 13 as the settings are
        // different. Ping @alexcrichton for assistance in updating this if
        // needed (and delete this comment after the 13 update).
        let mut pooling_config = PoolingAllocationConfig::default();
        pooling_config
            .instance_count(env("SPIN_WASMTIME_INSTANCE_COUNT", 1_000))
            .instance_size(env("SPIN_WASMTIME_INSTANCE_SIZE", (10 * MB) as u32) as usize)
            .instance_tables(env("SPIN_WASMTIME_INSTANCE_TABLES", 2))
            .instance_table_elements(env("SPIN_WASMTIME_INSTANCE_TABLE_ELEMENTS", 30_000))
            .instance_memories(env("SPIN_WASMTIME_INSTANCE_MEMORIES", 1))
            // Nothing is lost from allowing the maximum size of memory for
            // all instance as it's still limited through other the normal
            // `StoreLimitsAsync` accounting method too.
            // .instance_memory_pages(4 * GB / WASM_PAGE_SIZE)
            // These numbers are completely arbitrary at something above 0.
            .linear_memory_keep_resident((2 * MB) as usize)
            .table_keep_resident((MB / 2) as usize);
        inner.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling_config));

        return Self { inner };

        fn env(name: &str, default: u32) -> u32 {
            match std::env::var(name) {
                Ok(val) => val
                    .parse()
                    .unwrap_or_else(|e| panic!("failed to parse env var `{name}={val}`: {e}")),
                Err(_) => default,
            }
        }
    }
}

/// Host state data associated with individual [Store]s and [Instance]s.
pub struct Data<T> {
    inner: T,
    wasi: Wasi,
    host_components_data: HostComponentsData,
    store_limits: limits::StoreLimitsAsync,
    table: Table,
}

impl<T> Data<T> {
    /// Get the amount of memory in bytes consumed by instances in the store
    pub fn memory_consumed(&self) -> u64 {
        self.store_limits.memory_consumed()
    }
}

impl<T> AsRef<T> for Data<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl<T> AsMut<T> for Data<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Send> wasmtime_wasi::preview2::WasiView for Data<T> {
    fn table(&self) -> &wasmtime_wasi::preview2::Table {
        &self.table
    }

    fn table_mut(&mut self) -> &mut wasmtime_wasi::preview2::Table {
        &mut self.table
    }

    fn ctx(&self) -> &wasmtime_wasi::preview2::WasiCtx {
        match &self.wasi {
            Wasi::Preview1(_) => panic!("using WASI Preview 1 functions with Preview 2 store"),
            Wasi::Preview2(ctx) => ctx,
        }
    }

    fn ctx_mut(&mut self) -> &mut wasmtime_wasi::preview2::WasiCtx {
        match &mut self.wasi {
            Wasi::Preview1(_) => panic!("using WASI Preview 1 functions with Preview 2 store"),
            Wasi::Preview2(ctx) => ctx,
        }
    }
}

/// An alias for [`wasmtime::Linker`] specialized to [`Data`].
pub type ModuleLinker<T> = wasmtime::Linker<Data<T>>;

/// An alias for [`wasmtime::component::Linker`] specialized to [`Data`].
pub type Linker<T> = wasmtime::component::Linker<Data<T>>;

/// A builder interface for configuring a new [`Engine`].
///
/// A new [`EngineBuilder`] can be obtained with [`Engine::builder`].
pub struct EngineBuilder<T> {
    engine: wasmtime::Engine,
    linker: Linker<T>,
    module_linker: ModuleLinker<T>,
    host_components_builder: HostComponentsBuilder,
    epoch_tick_interval: Duration,
    epoch_ticker_thread: bool,
}

impl<T: Send + Sync> EngineBuilder<T> {
    fn new(config: &Config) -> Result<Self> {
        let engine = wasmtime::Engine::new(&config.inner)?;

        let mut linker: Linker<T> = Linker::new(&engine);
        wasmtime_wasi::preview2::wasi::command::add_to_linker(&mut linker)?;

        let mut module_linker = ModuleLinker::new(&engine);
        wasmtime_wasi::tokio::add_to_linker(&mut module_linker, |data| match &mut data.wasi {
            Wasi::Preview1(ctx) => ctx,
            Wasi::Preview2(_) => panic!("using WASI Preview 2 functions with Preview 1 store"),
        })?;

        Ok(Self {
            engine,
            linker,
            module_linker,
            host_components_builder: HostComponents::builder(),
            epoch_tick_interval: DEFAULT_EPOCH_TICK_INTERVAL,
            epoch_ticker_thread: true,
        })
    }

    /// Adds definition(s) to the built [`Engine`].
    ///
    /// This method's signature is meant to be used with
    /// [`wasmtime::component::bindgen`]'s generated `add_to_linker` functions, e.g.:
    ///
    /// ```ignore
    /// use spin_core::my_interface;
    /// // ...
    /// let mut builder: EngineBuilder<my_interface::MyInterfaceData> = Engine::builder();
    /// builder.link_import(my_interface::add_to_linker)?;
    /// ```
    pub fn link_import(
        &mut self,
        f: impl FnOnce(&mut Linker<T>, fn(&mut Data<T>) -> &mut T) -> Result<()>,
    ) -> Result<()> {
        f(&mut self.linker, Data::as_mut)
    }

    /// Adds a [`HostComponent`] to the built [`Engine`].
    ///
    /// Returns a [`HostComponentDataHandle`] which can be passed to
    /// [`HostComponentsData`] to access or set associated
    /// [`HostComponent::Data`] for an instance.
    pub fn add_host_component<HC: HostComponent + Send + Sync + 'static>(
        &mut self,
        host_component: HC,
    ) -> Result<HostComponentDataHandle<HC>> {
        self.host_components_builder
            .add_host_component(&mut self.linker, host_component)
    }

    /// Sets the epoch tick internal for the built [`Engine`].
    ///
    /// This is used by [`Store::set_deadline`] to calculate the number of
    /// "ticks" for epoch interruption, and by the default epoch ticker thread.
    /// The default is [`DEFAULT_EPOCH_TICK_INTERVAL`].
    ///
    /// See [`EngineBuilder::epoch_ticker_thread`] and
    /// [`wasmtime::Config::epoch_interruption`](https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html#method.epoch_interruption).
    pub fn epoch_tick_interval(&mut self, interval: Duration) {
        self.epoch_tick_interval = interval;
    }

    /// Configures whether the epoch ticker thread will be spawned when this
    /// [`Engine`] is built.
    ///
    /// Enabled by default; if disabled, the user must arrange to call
    /// `engine.as_ref().increment_epoch()` every `epoch_tick_interval` or
    /// interrupt-based features like `Store::set_deadline` will not work.
    pub fn epoch_ticker_thread(&mut self, enable: bool) {
        self.epoch_ticker_thread = enable;
    }

    fn maybe_spawn_epoch_ticker(&self) -> Option<Sender<()>> {
        if !self.epoch_ticker_thread {
            return None;
        }
        let engine = self.engine.clone();
        let interval = self.epoch_tick_interval;
        let (send, recv) = crossbeam_channel::bounded(0);
        std::thread::spawn(move || loop {
            match recv.recv_timeout(interval) {
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => (),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                res => panic!("unexpected epoch_ticker_signal: {res:?}"),
            }
            engine.increment_epoch();
        });
        Some(send)
    }

    /// Builds an [`Engine`] from this builder.
    pub fn build(self) -> Engine<T> {
        let epoch_ticker_signal = self.maybe_spawn_epoch_ticker();

        let host_components = self.host_components_builder.build();

        Engine {
            inner: self.engine,
            linker: self.linker,
            module_linker: self.module_linker,
            host_components,
            epoch_tick_interval: self.epoch_tick_interval,
            _epoch_ticker_signal: epoch_ticker_signal,
        }
    }
}

/// An `Engine` is a global context for the initialization and execution of
/// Spin components.
pub struct Engine<T> {
    inner: wasmtime::Engine,
    linker: Linker<T>,
    module_linker: ModuleLinker<T>,
    host_components: HostComponents,
    epoch_tick_interval: Duration,
    // Matching receiver closes on drop
    _epoch_ticker_signal: Option<Sender<()>>,
}

impl<T: Send + Sync> Engine<T> {
    /// Creates a new [`EngineBuilder`] with the given [`Config`].
    pub fn builder(config: &Config) -> Result<EngineBuilder<T>> {
        EngineBuilder::new(config)
    }

    /// Creates a new [`StoreBuilder`].
    pub fn store_builder(&self, wasi_version: WasiVersion) -> StoreBuilder {
        StoreBuilder::new(
            self.inner.clone(),
            self.epoch_tick_interval,
            &self.host_components,
            wasi_version,
        )
    }

    /// Creates a new [`InstancePre`] for the given [`Component`].
    #[instrument(skip_all)]
    pub fn instantiate_pre(&self, component: &Component) -> Result<InstancePre<T>> {
        let inner = Arc::new(self.linker.instantiate_pre(component)?);
        Ok(InstancePre { inner })
    }

    /// Creates a new [`ModuleInstancePre`] for the given [`Module`].
    #[instrument(skip_all)]
    pub fn module_instantiate_pre(&self, module: &Module) -> Result<ModuleInstancePre<T>> {
        let inner = Arc::new(self.module_linker.instantiate_pre(module)?);
        Ok(ModuleInstancePre { inner })
    }

    /// Find the [`HostComponentDataHandle`] for a [`HostComponent`] if configured for this engine.
    pub fn find_host_component_handle<HC: HostComponent>(
        &self,
    ) -> Option<HostComponentDataHandle<HC>> {
        self.host_components.find_handle()
    }
}

impl<T> AsRef<wasmtime::Engine> for Engine<T> {
    fn as_ref(&self) -> &wasmtime::Engine {
        &self.inner
    }
}

/// A pre-initialized instance that is ready to be instantiated.
///
/// See [`wasmtime::component::InstancePre`] for more information.
pub struct InstancePre<T> {
    inner: Arc<wasmtime::component::InstancePre<Data<T>>>,
}

impl<T: Send + Sync> InstancePre<T> {
    /// Instantiates this instance with the given [`Store`].
    #[instrument(skip_all)]
    pub async fn instantiate_async(&self, store: &mut Store<T>) -> Result<Instance> {
        self.inner.instantiate_async(store).await
    }
}

impl<T> Clone for InstancePre<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> AsRef<wasmtime::component::InstancePre<Data<T>>> for InstancePre<T> {
    fn as_ref(&self) -> &wasmtime::component::InstancePre<Data<T>> {
        &self.inner
    }
}

/// A pre-initialized module instance that is ready to be instantiated.
///
/// See [`wasmtime::InstancePre`] for more information.
pub struct ModuleInstancePre<T> {
    inner: Arc<wasmtime::InstancePre<Data<T>>>,
}

impl<T: Send + Sync> ModuleInstancePre<T> {
    /// Instantiates this instance with the given [`Store`].
    #[instrument(skip_all)]
    pub async fn instantiate_async(&self, store: &mut Store<T>) -> Result<ModuleInstance> {
        self.inner.instantiate_async(store).await
    }
}

impl<T> Clone for ModuleInstancePre<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> AsRef<wasmtime::InstancePre<Data<T>>> for ModuleInstancePre<T> {
    fn as_ref(&self) -> &wasmtime::InstancePre<Data<T>> {
        &self.inner
    }
}
