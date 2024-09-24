use anyhow::Result;
use spin_core::{async_trait, wasmtime::component::Resource};
use spin_world::v1::postgres as v1;
use spin_world::v1::rdbms_types as v1_types;
use spin_world::v2::postgres::{self as v2, Connection};
use spin_world::v2::rdbms_types;
use spin_world::v2::rdbms_types::{ParameterValue, RowSet};
use tracing::instrument;
use tracing::Level;

use crate::client::Client;
use crate::InstanceState;

impl<C: Client> InstanceState<C> {
    async fn open_connection(&mut self, address: &str) -> Result<Resource<Connection>, v2::Error> {
        self.connections
            .push(
                C::build_client(address)
                    .await
                    .map_err(|e| v2::Error::ConnectionFailed(format!("{e:?}")))?,
            )
            .map_err(|_| v2::Error::ConnectionFailed("too many connections".into()))
            .map(Resource::new_own)
    }

    async fn get_client(&mut self, connection: Resource<Connection>) -> Result<&C, v2::Error> {
        self.connections
            .get(connection.rep())
            .ok_or_else(|| v2::Error::ConnectionFailed("no connection found".into()))
    }

    async fn is_address_allowed(&self, address: &str) -> Result<bool> {
        let Ok(config) = address.parse::<tokio_postgres::Config>() else {
            return Ok(false);
        };
        for (i, host) in config.get_hosts().iter().enumerate() {
            match host {
                tokio_postgres::config::Host::Tcp(address) => {
                    let ports = config.get_ports();
                    // The port we use is either:
                    // * The port at the same index as the host
                    // * The first port if there is only one port
                    let port =
                        ports
                            .get(i)
                            .or_else(|| if ports.len() == 1 { ports.get(1) } else { None });
                    let port_str = port.map(|p| format!(":{}", p)).unwrap_or_default();
                    let url = format!("{address}{port_str}");
                    if !self.allowed_hosts.check_url(&url, "postgres").await? {
                        return Ok(false);
                    }
                }
                #[cfg(unix)]
                tokio_postgres::config::Host::Unix(_) => return Ok(false),
            }
        }
        Ok(true)
    }
}

#[async_trait]
impl<C: Send + Sync + Client> v2::Host for InstanceState<C> {}

#[async_trait]
impl<C: Send + Sync + Client> v2::HostConnection for InstanceState<C> {
    #[instrument(name = "spin_outbound_pg.open_connection", skip(self), err(level = Level::INFO), fields(otel.kind = "client", db.system = "postgresql"))]
    async fn open(&mut self, address: String) -> Result<Resource<Connection>, v2::Error> {
        if !self
            .is_address_allowed(&address)
            .await
            .map_err(|e| v2::Error::Other(e.to_string()))?
        {
            return Err(v2::Error::ConnectionFailed(format!(
                "address {address} is not permitted"
            )));
        }
        self.open_connection(&address).await
    }

    #[instrument(name = "spin_outbound_pg.execute", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "postgresql", otel.name = statement))]
    async fn execute(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<u64, v2::Error> {
        Ok(self
            .get_client(connection)
            .await?
            .execute(statement, params)
            .await?)
    }

    #[instrument(name = "spin_outbound_pg.query", skip(self, connection), err(level = Level::INFO), fields(otel.kind = "client", db.system = "postgresql", otel.name = statement))]
    async fn query(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<RowSet, v2::Error> {
        Ok(self
            .get_client(connection)
            .await?
            .query(statement, params)
            .await?)
    }

    async fn drop(&mut self, connection: Resource<Connection>) -> anyhow::Result<()> {
        self.connections.remove(connection.rep());
        Ok(())
    }
}

impl<C: Send> rdbms_types::Host for InstanceState<C> {
    fn convert_error(&mut self, error: v2::Error) -> Result<v2::Error> {
        Ok(error)
    }
}

/// Delegate a function call to the v2::HostConnection implementation
macro_rules! delegate {
    ($self:ident.$name:ident($address:expr, $($arg:expr),*)) => {{
        if !$self.is_address_allowed(&$address).await.map_err(|e| v2::Error::Other(e.to_string()))? {
            return Err(v1::PgError::ConnectionFailed(format!(
                "address {} is not permitted", $address
            )));
        }
        let connection = match $self.open_connection(&$address).await {
            Ok(c) => c,
            Err(e) => return Err(e.into()),
        };
        <Self as v2::HostConnection>::$name($self, connection, $($arg),*)
            .await
            .map_err(|e| e.into())
    }};
}

#[async_trait]
impl<C: Send + Sync + Client> v1::Host for InstanceState<C> {
    async fn execute(
        &mut self,
        address: String,
        statement: String,
        params: Vec<v1_types::ParameterValue>,
    ) -> Result<u64, v1::PgError> {
        delegate!(self.execute(
            address,
            statement,
            params.into_iter().map(Into::into).collect()
        ))
    }

    async fn query(
        &mut self,
        address: String,
        statement: String,
        params: Vec<v1_types::ParameterValue>,
    ) -> Result<v1_types::RowSet, v1::PgError> {
        delegate!(self.query(
            address,
            statement,
            params.into_iter().map(Into::into).collect()
        ))
        .map(Into::into)
    }

    fn convert_pg_error(&mut self, error: v1::PgError) -> Result<v1::PgError> {
        Ok(error)
    }
}