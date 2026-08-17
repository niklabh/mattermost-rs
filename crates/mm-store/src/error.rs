//! Store errors.
//!
//! Go's store layer returns `*store.ErrNotFound` for a miss and a `pkg/errors`-wrapped driver
//! error for everything else, and callers branch on the former with `errors.As`. The split
//! matters at the API edge: a missing session is a 401, a broken query is a 500. So not-found is
//! its own variant rather than an error string a caller has to match on.

use thiserror::Error;

/// Port of the error surface of `server/channels/store` as far as this crate uses it.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Port of `store.NewErrNotFound(entity, criteria)` (store/errors.go).
    ///
    /// `criteria` reproduces Go's habit of embedding the lookup key in the message, e.g.
    /// `sessionIdOrToken=abc`. It is not the raw value on its own, because a bare token in a log
    /// line is a credential.
    #[error("{entity} not found: {criteria}")]
    NotFound {
        entity: &'static str,
        criteria: String,
    },

    /// Any driver-level failure. `context` says what was being attempted, matching the
    /// `errors.Wrapf(err, "failed to find Sessions with ...")` convention in the Go store.
    #[error("database error: {context}")]
    Db {
        context: String,
        #[source]
        source: sqlx::Error,
    },

    /// A model type rejected the value before it reached the database.
    ///
    /// Go's store returns `*model.AppError` straight out of `Save` when `IsValid` fails, and the
    /// app layer passes it through with `errors.As`. Carrying the `AppError` rather than a
    /// message keeps the error id and status code intact all the way to the client — the whole
    /// point of the type. Boxed because `AppError` is much larger than the other variants.
    #[error("{entity} failed validation: {app_error}")]
    Invalid {
        entity: &'static str,
        app_error: Box<mm_model::utils::AppError>,
    },

    /// A `jsonb` column held something the model type cannot represent.
    ///
    /// Go decodes these columns into `model.StringMap` with `encoding/json` and surfaces a
    /// scan error; there is no silent-default path on either side.
    #[error("{entity}.{column} held JSON that does not decode into the model type")]
    Decode {
        entity: &'static str,
        column: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl StoreError {
    /// True when the query ran fine and matched nothing.
    ///
    /// The API edge needs this distinction to choose between 401 and 500, and asking it to match
    /// on a message would be exactly the stringly-typed error handling `CLAUDE.md` forbids.
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::NotFound { .. })
    }
}
