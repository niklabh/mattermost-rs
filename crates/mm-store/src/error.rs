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

    /// A store function refused its arguments before building a query.
    ///
    /// Go writes these as a bare `errors.New` inside the store — `SqlTeamStore.GetMembersByIds`
    /// on an empty id list is the first one ported — and the app layer wraps them into the same
    /// **500** it gives a driver failure, because `errors.As` finds no `*store.ErrNotFound`. So
    /// this is deliberately *not* a 400: the guard exists for a caller that bypassed the api4
    /// handler's own bounds check, and reproducing Go means reproducing its status too.
    #[error("{entity}: {detail}")]
    Argument {
        entity: &'static str,
        detail: &'static str,
    },

    /// Port of `store.NewErrOutOfBounds(limit)` (store/errors.go).
    ///
    /// A store function refusing a page size *before* querying. Kept apart from
    /// [`StoreError::Argument`] because the app layer branches on it: `GetAuditsPage`
    /// (app/audit.go:62) does `errors.As(err, &outErr)` and answers **400**, where every other
    /// store failure on that path is a 500. Folding the two together would turn a client's
    /// oversized `per_page` into a server error.
    #[error("limit exceeds the store's maximum: {limit}")]
    OutOfBounds { limit: i64 },

    /// Port of `store.NewErrConflict(resource, err, details)` (store/errors.go).
    ///
    /// A unique-constraint violation the app layer must tell apart by **which** constraint:
    /// `App.UpdateUser` answers `app.user.save.username_exists.app_error` when `Resource` is
    /// `"Username"` and `app.user.save.email_exists.app_error` for anything else. Folding the two
    /// together would tell a client their email was taken when their username was.
    #[error("{resource} already exists")]
    Conflict {
        resource: &'static str,
        #[source]
        source: sqlx::Error,
    },

    /// Port of `store.NewErrInvalidInput(entity, field, value)` (store/errors.go).
    ///
    /// Distinct from [`StoreError::Argument`] because the app layer answers **400** to this one
    /// and 500 to that one: `App.UpdateUser` maps it to `app.user.update.find.app_error`. Go
    /// raises it for a row that vanished between the caller's read and the store's, and for an
    /// LDAP user whose username or email the caller tried to change.
    #[error("invalid {entity}.{field}: {value}")]
    InvalidInput {
        entity: &'static str,
        field: &'static str,
        value: String,
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

    /// True when a unique constraint rejected the write. The `resource` says which.
    pub fn conflict_resource(&self) -> Option<&'static str> {
        match self {
            StoreError::Conflict { resource, .. } => Some(resource),
            _ => None,
        }
    }

    /// True when a store function refused its input, which the app layer answers 400 to.
    pub fn is_invalid_input(&self) -> bool {
        matches!(self, StoreError::InvalidInput { .. })
    }

    /// True when a store function refused its page size, which the app layer answers 400 to.
    pub fn is_out_of_bounds(&self) -> bool {
        matches!(self, StoreError::OutOfBounds { .. })
    }
}
