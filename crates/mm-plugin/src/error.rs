//! The errors that cross the plugin RPC (client_rpc.go, `encodableError` and `decodableError`).
//!
//! An `error` field is a gob interface, so only a registered type can travel in one. Go wraps
//! anything else in an `ErrorString`, keeping its message and, for a handful of sentinels from
//! `database/sql`, a code that names it on the far side.

use gobwire::Interface;

use crate::wire::model::AppError;
use crate::wire::plugin::ErrorString;
use crate::wire::{pq, registered};

/// An error a plugin or host sent, as the far side reads it (client_rpc.go, `decodableError`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PluginError {
    /// Mattermost's own error, which crosses whole.
    App(Box<AppError>),
    /// A Postgres error, which crosses whole.
    Postgres(Box<pq::Error>),
    /// One of the `database/sql` sentinels, named by the code `encodableError` gave it.
    Sentinel(Sentinel),
    /// Any other error: its message survived, its type did not.
    Message(String),
    /// A registered type this crate does not know.
    Other(Box<Interface>),
}

/// The errors `encodableError` gives a code, so that `decodableError` can name them again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentinel {
    /// `io.EOF`.
    Eof,
    /// `sql.ErrNoRows`.
    NoRows,
    /// `sql.ErrConnDone`.
    ConnDone,
    /// `sql.ErrTxDone`.
    TxDone,
    /// `driver.ErrSkip`.
    Skip,
    /// `driver.ErrBadConn`.
    BadConn,
    /// `driver.ErrRemoveArgument`.
    RemoveArgument,
}

impl Sentinel {
    /// The code `encodableError` writes into `ErrorString.Code`.
    pub fn code(self) -> i64 {
        match self {
            Sentinel::Eof => 1,
            Sentinel::NoRows => 2,
            Sentinel::ConnDone => 3,
            Sentinel::TxDone => 4,
            Sentinel::Skip => 5,
            Sentinel::BadConn => 6,
            Sentinel::RemoveArgument => 7,
        }
    }

    /// The code as `decodableError` reads it. Any other code is not a sentinel.
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            1 => Sentinel::Eof,
            2 => Sentinel::NoRows,
            3 => Sentinel::ConnDone,
            4 => Sentinel::TxDone,
            5 => Sentinel::Skip,
            6 => Sentinel::BadConn,
            7 => Sentinel::RemoveArgument,
            _ => return None,
        })
    }

    /// Go's own message for the sentinel, which `encodableError` copies into `ErrorString.Err`.
    pub fn message(self) -> &'static str {
        match self {
            Sentinel::Eof => "EOF",
            Sentinel::NoRows => "sql: no rows in result set",
            Sentinel::ConnDone => "sql: connection is already closed",
            Sentinel::TxDone => "sql: transaction has already been committed or rolled back",
            Sentinel::Skip => "driver: skip fast-path; continue as if unimplemented",
            Sentinel::BadConn => "driver: bad connection",
            Sentinel::RemoveArgument => "driver: remove argument from query",
        }
    }
}

impl PluginError {
    /// The error's `Error()` text as Go renders it, which is what a status or a log shows.
    ///
    /// `*model.AppError` joins `Where`, `Message` and `DetailedError` (utils.go, `Error`), leaving
    /// out a message that is `<untranslated>`; `*pq.Error` prefixes `pq: `.
    pub fn go_error(&self) -> String {
        match self {
            PluginError::App(e) => {
                let untranslated = e.message == "<untranslated>";
                let mut text = String::new();
                if !e.r#where.is_empty() {
                    text.push_str(&e.r#where);
                    text.push_str(": ");
                }
                if !untranslated {
                    text.push_str(&e.message);
                }
                if !e.detailed_error.is_empty() {
                    if !untranslated {
                        text.push_str(", ");
                    }
                    text.push_str(&e.detailed_error);
                }
                text
            }
            PluginError::Postgres(e) => format!("pq: {}", e.message),
            PluginError::Sentinel(s) => s.message().to_owned(),
            PluginError::Message(m) => m.clone(),
            PluginError::Other(v) => v.name.clone(),
        }
    }
}

/// What the far side made of an error field (client_rpc.go, `decodableError`).
///
/// `None` is a nil error. An `ErrorString` whose code names a sentinel comes back as that
/// sentinel, which is how Go's driver recognises `driver.ErrBadConn` and retries.
pub fn decodable_error(value: Option<&Interface>) -> Option<PluginError> {
    let value = value?;
    Some(match value.name.as_str() {
        registered::APP_ERROR => PluginError::App(Box::new(value.downcast().ok()?)),
        registered::PQ_ERROR => PluginError::Postgres(Box::new(value.downcast().ok()?)),
        registered::ERROR_STRING => {
            let error: ErrorString = value.downcast().ok()?;
            match Sentinel::from_code(error.code) {
                Some(sentinel) => PluginError::Sentinel(sentinel),
                None => PluginError::Message(error.err),
            }
        }
        _ => PluginError::Other(Box::new(value.clone())),
    })
}

/// An error as it must cross (client_rpc.go, `encodableError`).
///
/// An `AppError` and a `pq.Error` travel whole; everything else becomes an `ErrorString`, with a
/// code when it is one of the sentinels.
pub fn encodable_error(error: Option<&PluginError>) -> Option<Interface> {
    let error = error?;
    let wrap = |code: i64, message: &str| {
        Interface::new(
            registered::ERROR_STRING,
            &ErrorString {
                code,
                err: message.to_owned(),
            },
        )
        .ok()
    };
    match error {
        PluginError::App(app) => Interface::new(registered::APP_ERROR, &**app).ok(),
        PluginError::Postgres(error) => Interface::new(registered::PQ_ERROR, &**error).ok(),
        PluginError::Sentinel(sentinel) => wrap(sentinel.code(), sentinel.message()),
        PluginError::Message(message) => wrap(0, message),
        // Go keeps a registered type it was handed; only an unregistered one is wrapped.
        PluginError::Other(value) => Some((**value).clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// utils.go, `AppError.Error`: each part only when present, and an untranslated message
    /// drops out with its separator.
    #[test]
    fn go_error_renders_an_app_error_as_go_does() {
        let app = |r#where: &str, message: &str, detailed: &str| {
            PluginError::App(Box::new(AppError {
                r#where: r#where.into(),
                message: message.into(),
                detailed_error: detailed.into(),
                ..Default::default()
            }))
            .go_error()
        };
        assert_eq!(app("W", "m", "d"), "W: m, d");
        assert_eq!(app("", "m", ""), "m");
        assert_eq!(app("W", "<untranslated>", "d"), "W: d");
        assert_eq!(app("", "<untranslated>", ""), "");
        assert_eq!(PluginError::Message("x".into()).go_error(), "x");
    }

    #[test]
    fn a_sentinel_survives_the_round_trip() {
        for sentinel in [
            Sentinel::Eof,
            Sentinel::NoRows,
            Sentinel::ConnDone,
            Sentinel::TxDone,
            Sentinel::Skip,
            Sentinel::BadConn,
            Sentinel::RemoveArgument,
        ] {
            let sent = encodable_error(Some(&PluginError::Sentinel(sentinel)));
            assert_eq!(
                decodable_error(sent.as_ref()),
                Some(PluginError::Sentinel(sentinel)),
                "{sentinel:?}"
            );
        }
    }

    #[test]
    fn a_plain_message_keeps_its_text_and_loses_its_type() {
        let sent = encodable_error(Some(&PluginError::Message("boom".into())));
        let error: ErrorString = sent.as_ref().unwrap().downcast().unwrap();
        assert_eq!(error.code, 0, "an unnamed error has no code");
        assert_eq!(
            decodable_error(sent.as_ref()),
            Some(PluginError::Message("boom".into()))
        );
    }

    #[test]
    fn an_app_error_crosses_whole() {
        let app = AppError {
            id: "api.context.invalid_param.app_error".into(),
            message: "invalid parameter".into(),
            status_code: 400,
            ..Default::default()
        };
        let sent = encodable_error(Some(&PluginError::App(Box::new(app.clone()))));
        assert_eq!(sent.as_ref().unwrap().name, registered::APP_ERROR);
        assert_eq!(
            decodable_error(sent.as_ref()),
            Some(PluginError::App(Box::new(app)))
        );
    }

    #[test]
    fn a_nil_error_stays_nil() {
        assert_eq!(encodable_error(None), None);
        assert_eq!(decodable_error(None), None);
    }
}
