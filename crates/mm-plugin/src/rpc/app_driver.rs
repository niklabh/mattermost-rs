//! Go's `AppDriver` (driver.go:64) and the supervisor's `driverForPlugin` (supervisor.go:36):
//! what the host's database driver offers beyond the RPC surface, and the per-plugin view each
//! plugin is served.

use std::future::Future;
use std::sync::Arc;

use super::{Driver, NotImplemented};
use crate::wire::plugin;

/// Port of `plugin.AppDriver`: a [`Driver`] that also opens connections on a plugin's behalf, so
/// that it can close whatever that plugin left open when the plugin stops.
pub trait AppDriver: Driver {
    /// Go: `ConnWithPluginID(isMaster bool, pluginID string) (string, error)`. Not reachable over
    /// RPC; [`DriverForPlugin`] answers a plugin's `Conn` with it. The default opens an
    /// untracked connection through [`Driver::conn`].
    fn conn_with_plugin_id(
        &self,
        is_master: bool,
        plugin_id: &str,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        let _ = plugin_id;
        self.conn(is_master)
    }

    /// Go: `ShutdownConns(pluginID string)`: close every connection `plugin_id` opened and did not
    /// close. The environment calls it after the plugin's process is stopped. The default has
    /// nothing to close.
    fn shutdown_conns(&self, plugin_id: &str) -> impl Future<Output = ()> + Send {
        let _ = plugin_id;
        async {}
    }
}

/// Port of `driverForPlugin` (supervisor.go:36): the host's driver as one plugin sees it. `Conn`
/// becomes `ConnWithPluginID` under that plugin's id; every other method passes through.
pub struct DriverForPlugin<D> {
    inner: Arc<D>,
    plugin_id: String,
}

impl<D> DriverForPlugin<D> {
    pub fn new(inner: Arc<D>, plugin_id: impl Into<String>) -> Self {
        Self {
            inner,
            plugin_id: plugin_id.into(),
        }
    }
}

impl<D: AppDriver> Driver for DriverForPlugin<D> {
    fn conn(
        &self,
        args: bool,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        self.inner.conn_with_plugin_id(args, &self.plugin_id)
    }

    fn conn_close(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.conn_close(args)
    }

    fn conn_exec(
        &self,
        args: plugin::Z_DbConnArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbResultContErrReturn, NotImplemented>> + Send {
        self.inner.conn_exec(args)
    }

    fn conn_ping(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.conn_ping(args)
    }

    fn conn_query(
        &self,
        args: plugin::Z_DbConnArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        self.inner.conn_query(args)
    }

    fn rows_close(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.rows_close(args)
    }

    fn rows_column_type_database_type_name(
        &self,
        args: plugin::Z_DbRowsColumnArg,
    ) -> impl Future<Output = Result<String, NotImplemented>> + Send {
        self.inner.rows_column_type_database_type_name(args)
    }

    fn rows_column_type_precision_scale(
        &self,
        args: plugin::Z_DbRowsColumnArg,
    ) -> impl Future<Output = Result<plugin::Z_DbRowsColumnTypePrecisionScaleReturn, NotImplemented>>
    + Send {
        self.inner.rows_column_type_precision_scale(args)
    }

    fn rows_columns(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbStrSliceReturn, NotImplemented>> + Send {
        self.inner.rows_columns(args)
    }

    fn rows_has_next_result_set(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbBoolReturn, NotImplemented>> + Send {
        self.inner.rows_has_next_result_set(args)
    }

    fn rows_next(
        &self,
        args: plugin::Z_DbRowScanArg,
    ) -> impl Future<Output = Result<plugin::Z_DbRowScanReturn, NotImplemented>> + Send {
        self.inner.rows_next(args)
    }

    fn rows_next_result_set(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.rows_next_result_set(args)
    }

    fn stmt(
        &self,
        args: plugin::Z_DbStmtArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        self.inner.stmt(args)
    }

    fn stmt_close(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.stmt_close(args)
    }

    fn stmt_exec(
        &self,
        args: plugin::Z_DbStmtQueryArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbResultContErrReturn, NotImplemented>> + Send {
        self.inner.stmt_exec(args)
    }

    fn stmt_num_input(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbIntReturn, NotImplemented>> + Send {
        self.inner.stmt_num_input(args)
    }

    fn stmt_query(
        &self,
        args: plugin::Z_DbStmtQueryArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        self.inner.stmt_query(args)
    }

    fn tx(
        &self,
        args: plugin::Z_DbTxArgs,
    ) -> impl Future<Output = Result<plugin::Z_DbStrErrReturn, NotImplemented>> + Send {
        self.inner.tx(args)
    }

    fn tx_commit(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.tx_commit(args)
    }

    fn tx_rollback(
        &self,
        args: String,
    ) -> impl Future<Output = Result<plugin::Z_DbErrReturn, NotImplemented>> + Send {
        self.inner.tx_rollback(args)
    }
}
