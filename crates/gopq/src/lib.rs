//! A port of github.com/lib/pq's `database/sql/driver` connection.
//!
//! It exists for callers that must answer exactly as a Go program using lib/pq answers: the same
//! protocol messages (simple query without arguments, an unnamed prepare-and-execute with them),
//! the same result formats (binary only for `bytea`, `int2/4/8` and `uuid` columns), the same
//! decoded values, and the same error texts, down to `driver: bad connection` after a fatal error.
//!
//! No TLS: `sslmode` must be `disable`, `allow` or `prefer`, and the connection is always plain.

mod config;
mod conn;
mod encode;
mod error;
pub mod oid;
mod scram;
mod value;

pub use config::Config;
pub use conn::{Conn, ExecResult, FieldDesc, Rows, RowsHeader, Stmt};
pub use error::{Error, PqError};
pub use value::{NamedValue, Value};
