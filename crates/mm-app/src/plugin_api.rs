//! Port of `PluginAPI` (app/plugin_api.go): the server API each plugin is served, one instance per
//! plugin (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! Every method is a thin wrapper over an app function a route already uses, so the unit is the
//! method, and what it owes Go is the answer on the wire: the value, and the `*model.AppError`
//! when there is one. A method not overridden here answers Go's `API <Name> called but not
//! implemented.` through `mm_plugin`'s default.
//!
//! # What is ported
//!
//! The logging four, the nine `KV*` methods (`crate::plugin_key_value_store`), `GetServerVersion`,
//! `GetDiagnosticId` and `GetSystemInstallDate`. Not yet `GetConfig`, `GetUnsanitizedConfig` or
//! `GetLicense`: each answers a gob `model.Config` or `model.License`, hundreds of pointer fields
//! this server holds only as JSON, and the conversion is its own unit ([D-990]).
//!
//! # An error crosses translated, without its wrapped cause
//!
//! `NewAppError` translates at construction with the server's locale (`i18n.T`), and gob carries
//! only exported fields, so the plugin receives `Id`, the translated `Message`, `DetailedError`,
//! `StatusCode` and `Where` — and not the driver error `Wrap` attached. See [`wire_app_error`].
//!
//! # Logging
//!
//! Go's `LogDebug` and friends go through `mlog.Sugar` with the plugin id attached
//! (`a.Log().Sugar(mlog.String("plugin_id", id))`), whose `argsToFields` pairs the arguments up
//! ([`log_fields`]). The pairs arrive as strings (the Go SDK formats them with `%+v` before
//! sending), but nothing on the wire enforces it, so a key that is not a string, and a dangling
//! last argument, are logged as the complaints Go logs. The record itself is a `tracing` event,
//! not an `mlog` line: this server's log format is not Go's anywhere, so what is ported is the
//! level, the message, the plugin id and the fields, in order.

use gobwire::Interface;
use mm_model::manifest::Manifest;
use mm_model::utils::AppError;
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model::AppError as WireAppError;
use mm_plugin::wire::plugin::{
    Z_GetDiagnosticIdArgs, Z_GetDiagnosticIdReturns, Z_GetServerVersionArgs,
    Z_GetServerVersionReturns, Z_GetSystemInstallDateArgs, Z_GetSystemInstallDateReturns,
    Z_KVCompareAndDeleteArgs, Z_KVCompareAndDeleteReturns, Z_KVCompareAndSetArgs,
    Z_KVCompareAndSetReturns, Z_KVDeleteAllArgs, Z_KVDeleteAllReturns, Z_KVDeleteArgs,
    Z_KVDeleteReturns, Z_KVGetArgs, Z_KVGetReturns, Z_KVListArgs, Z_KVListReturns, Z_KVSetArgs,
    Z_KVSetReturns, Z_KVSetWithExpiryArgs, Z_KVSetWithExpiryReturns, Z_KVSetWithOptionsArgs,
    Z_KVSetWithOptionsReturns, Z_LogDebugArgs, Z_LogDebugReturns, Z_LogErrorArgs,
    Z_LogErrorReturns, Z_LogInfoArgs, Z_LogInfoReturns, Z_LogWarnArgs, Z_LogWarnReturns,
};

use crate::App;

/// Port of `PluginAPI` (app/plugin_api.go:24): the app, and the plugin it serves.
///
/// Go also holds a `request.CTX`, which only `KVCompareAndDelete` reads, for its logger; the
/// `tracing` span stands in for it.
pub struct AppPluginApi {
    app: App,
    id: String,
    /// The manifest the plugin was activated with. Go keeps the pointer; the environment hands
    /// this factory a borrow, so each API keeps its own copy.
    manifest: Manifest,
}

impl AppPluginApi {
    /// Port of `NewPluginAPI` (app/plugin_api.go:32).
    pub fn new(app: App, manifest: &Manifest) -> Self {
        Self {
            app,
            id: manifest.id.clone(),
            manifest: manifest.clone(),
        }
    }

    /// The plugin this API serves.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn wire(&self, err: Box<AppError>) -> Option<Box<WireAppError>> {
        Some(wire_app_error(
            err,
            &self.app.config().default_server_locale,
        ))
    }

    fn log(&self, level: tracing::Level, msg: &str, pairs: &[Option<Interface>]) {
        let (fields, complaints) = log_fields(pairs);
        for complaint in complaints {
            tracing::error!(plugin_id = %self.id, detail = %complaint.detail, "{}", complaint.message);
        }
        let fields = render_fields(&fields);
        match level {
            tracing::Level::ERROR => {
                tracing::error!(plugin_id = %self.id, fields = %fields, "{msg}")
            }
            tracing::Level::WARN => tracing::warn!(plugin_id = %self.id, fields = %fields, "{msg}"),
            tracing::Level::INFO => tracing::info!(plugin_id = %self.id, fields = %fields, "{msg}"),
            _ => tracing::debug!(plugin_id = %self.id, fields = %fields, "{msg}"),
        }
    }
}

/// A `[]byte` as gob delivered it: an empty slice was omitted on the wire, so it is Go's nil.
fn bytes(value: &[u8]) -> Option<&[u8]> {
    (!value.is_empty()).then_some(value)
}

/// A `*model.AppError` as gob carries it to the plugin: translated at construction with the
/// server's locale, as `NewAppError` does with `i18n.T`, and only its exported fields — the error
/// `Wrap` attached is unexported, so it stays behind and `DetailedError` is what it was.
pub fn wire_app_error(mut err: Box<AppError>, default_server_locale: &str) -> Box<WireAppError> {
    if let Some(bundle) = crate::i18n::loaded() {
        bundle.translate_app_error(bundle.server_locale(default_server_locale), &mut err);
    }
    Box::new(WireAppError {
        id: err.id,
        message: err.message,
        detailed_error: err.detailed_error,
        request_id: err.request_id,
        status_code: i64::from(err.status_code),
        r#where: err.where_,
        skip_translation: err.skip_translation,
    })
}

/// One complaint `argsToFields` logs at error level instead of a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogComplaint {
    /// The complaint's message.
    pub message: &'static str,
    /// Its one field: `arg=<value>` or `pos=<index>`.
    pub detail: String,
}

/// Port of logr's `Sugar.argsToFields` (logr/v2 sugar.go:166) over the pairs a plugin sent.
///
/// Pairs are read two at a time: a string key and any value. A last argument with no partner
/// ends the walk with `invalid key/value pair` (`arg`); a key that is not a string drops the pair
/// with `invalid key for key/value pair` (`pos`, the key's index) and the walk goes on after its
/// value. A `logr.Field` is taken whole in Go, but none can cross gob, so that arm is not here.
pub fn log_fields(pairs: &[Option<Interface>]) -> (Vec<(String, String)>, Vec<LogComplaint>) {
    let mut fields = Vec::new();
    let mut complaints = Vec::new();
    let mut i = 0;
    while i < pairs.len() {
        if i == pairs.len() - 1 {
            complaints.push(LogComplaint {
                message: "invalid key/value pair",
                detail: format!("arg={}", render_any(pairs[i].as_ref())),
            });
            break;
        }
        match pairs[i].as_ref().filter(|key| key.name == "string") {
            Some(key) => fields.push((
                key.downcast::<String>().unwrap_or_default(),
                render_any(pairs[i + 1].as_ref()),
            )),
            None => complaints.push(LogComplaint {
                message: "invalid key for key/value pair",
                detail: format!("pos={i}"),
            }),
        }
        i += 2;
    }
    (fields, complaints)
}

/// `logr.Any`'s text for a value that crossed gob: a string as it is, the scalar kinds Go
/// registers by name as Go prints them, a nil interface as `<nil>`, and anything else by its
/// registered type name.
fn render_any(value: Option<&Interface>) -> String {
    let Some(value) = value else {
        return "<nil>".to_owned();
    };
    match value.name.as_str() {
        "string" => value.downcast::<String>().unwrap_or_default(),
        "bool" => value.downcast::<bool>().unwrap_or_default().to_string(),
        "int" | "int8" | "int16" | "int32" | "int64" => {
            value.downcast::<i64>().unwrap_or_default().to_string()
        }
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" => {
            value.downcast::<u64>().unwrap_or_default().to_string()
        }
        "float32" | "float64" => value.downcast::<f64>().unwrap_or_default().to_string(),
        other => other.to_owned(),
    }
}

/// The fields as one `key="value"` list, in the order the plugin sent them.
fn render_fields(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={v:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl mm_plugin::rpc::PluginApi for AppPluginApi {
    /// Port of `PluginAPI.LogDebug` (app/plugin_api.go:1271).
    async fn log_debug(&self, args: Z_LogDebugArgs) -> Result<Z_LogDebugReturns, NotImplemented> {
        self.log(tracing::Level::DEBUG, &args.a, &args.b);
        Ok(Z_LogDebugReturns {})
    }

    /// Port of `PluginAPI.LogInfo` (app/plugin_api.go:1275).
    async fn log_info(&self, args: Z_LogInfoArgs) -> Result<Z_LogInfoReturns, NotImplemented> {
        self.log(tracing::Level::INFO, &args.a, &args.b);
        Ok(Z_LogInfoReturns {})
    }

    /// Port of `PluginAPI.LogError` (app/plugin_api.go:1279).
    async fn log_error(&self, args: Z_LogErrorArgs) -> Result<Z_LogErrorReturns, NotImplemented> {
        self.log(tracing::Level::ERROR, &args.a, &args.b);
        Ok(Z_LogErrorReturns {})
    }

    /// Port of `PluginAPI.LogWarn` (app/plugin_api.go:1283).
    async fn log_warn(&self, args: Z_LogWarnArgs) -> Result<Z_LogWarnReturns, NotImplemented> {
        self.log(tracing::Level::WARN, &args.a, &args.b);
        Ok(Z_LogWarnReturns {})
    }

    /// Port of `PluginAPI.KVSet` (app/plugin_api.go:1208). An empty value deletes the key.
    async fn kv_set(&self, args: Z_KVSetArgs) -> Result<Z_KVSetReturns, NotImplemented> {
        let result = self
            .app
            .set_plugin_key(&self.id, &args.a, bytes(&args.b))
            .await;
        Ok(Z_KVSetReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVSetWithExpiry` (app/plugin_api.go:1220).
    async fn kv_set_with_expiry(
        &self,
        args: Z_KVSetWithExpiryArgs,
    ) -> Result<Z_KVSetWithExpiryReturns, NotImplemented> {
        let result = self
            .app
            .set_plugin_key_with_expiry(&self.id, &args.a, bytes(&args.b), args.c)
            .await;
        Ok(Z_KVSetWithExpiryReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVSetWithOptions` (app/plugin_api.go:1204).
    async fn kv_set_with_options(
        &self,
        args: Z_KVSetWithOptionsArgs,
    ) -> Result<Z_KVSetWithOptionsReturns, NotImplemented> {
        let options = mm_model::plugin_kvset_options::PluginKVSetOptions {
            atomic: args.c.atomic,
            old_value: bytes(&args.c.old_value).map(<[u8]>::to_vec),
            expire_in_seconds: args.c.expire_in_seconds,
        };
        let answer = match self
            .app
            .set_plugin_key_with_options(&self.id, &args.a, bytes(&args.b), &options)
            .await
        {
            Ok(set) => Z_KVSetWithOptionsReturns { a: set, b: None },
            Err(e) => Z_KVSetWithOptionsReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVCompareAndSet` (app/plugin_api.go:1212).
    async fn kv_compare_and_set(
        &self,
        args: Z_KVCompareAndSetArgs,
    ) -> Result<Z_KVCompareAndSetReturns, NotImplemented> {
        let answer = match self
            .app
            .compare_and_set_plugin_key(&self.id, &args.a, bytes(&args.b), bytes(&args.c))
            .await
        {
            Ok(set) => Z_KVCompareAndSetReturns { a: set, b: None },
            Err(e) => Z_KVCompareAndSetReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVCompareAndDelete` (app/plugin_api.go:1216).
    async fn kv_compare_and_delete(
        &self,
        args: Z_KVCompareAndDeleteArgs,
    ) -> Result<Z_KVCompareAndDeleteReturns, NotImplemented> {
        let answer = match self
            .app
            .compare_and_delete_plugin_key(&self.id, &args.a, bytes(&args.b))
            .await
        {
            Ok(deleted) => Z_KVCompareAndDeleteReturns {
                a: deleted,
                b: None,
            },
            Err(e) => Z_KVCompareAndDeleteReturns {
                a: false,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVGet` (app/plugin_api.go:1224). A missing key is nil, which gob
    /// sends as an omitted field.
    async fn kv_get(&self, args: Z_KVGetArgs) -> Result<Z_KVGetReturns, NotImplemented> {
        let answer = match self.app.get_plugin_key(&self.id, &args.a).await {
            Ok(value) => Z_KVGetReturns {
                a: value.unwrap_or_default(),
                b: None,
            },
            Err(e) => Z_KVGetReturns {
                a: Vec::new(),
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.KVDelete` (app/plugin_api.go:1228).
    async fn kv_delete(&self, args: Z_KVDeleteArgs) -> Result<Z_KVDeleteReturns, NotImplemented> {
        let result = self.app.delete_plugin_key(&self.id, &args.a).await;
        Ok(Z_KVDeleteReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVDeleteAll` (app/plugin_api.go:1232): this plugin's keys only.
    async fn kv_delete_all(
        &self,
        _: Z_KVDeleteAllArgs,
    ) -> Result<Z_KVDeleteAllReturns, NotImplemented> {
        let result = self.app.delete_all_keys_for_plugin(&self.id).await;
        Ok(Z_KVDeleteAllReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.KVList` (app/plugin_api.go:1236).
    async fn kv_list(&self, args: Z_KVListArgs) -> Result<Z_KVListReturns, NotImplemented> {
        let answer = match self.app.list_plugin_keys(&self.id, args.a, args.b).await {
            Ok(keys) => Z_KVListReturns { a: keys, b: None },
            Err(e) => Z_KVListReturns {
                a: Vec::new(),
                b: self.wire(e),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetServerVersion` (app/plugin_api.go:152): `model.CurrentVersion`.
    async fn get_server_version(
        &self,
        _: Z_GetServerVersionArgs,
    ) -> Result<Z_GetServerVersionReturns, NotImplemented> {
        Ok(Z_GetServerVersionReturns {
            a: mm_model::version::CURRENT_VERSION.to_owned(),
        })
    }

    /// Port of `PluginAPI.GetDiagnosticId` (app/plugin_api.go:160): the server id.
    async fn get_diagnostic_id(
        &self,
        _: Z_GetDiagnosticIdArgs,
    ) -> Result<Z_GetDiagnosticIdReturns, NotImplemented> {
        Ok(Z_GetDiagnosticIdReturns {
            a: self.app.server_id().await,
        })
    }

    /// Port of `PluginAPI.GetSystemInstallDate` (app/plugin_api.go:156).
    async fn get_system_install_date(
        &self,
        _: Z_GetSystemInstallDateArgs,
    ) -> Result<Z_GetSystemInstallDateReturns, NotImplemented> {
        let answer = match self.app.get_system_install_date().await {
            Ok(date) => Z_GetSystemInstallDateReturns { a: date, b: None },
            Err(e) => Z_GetSystemInstallDateReturns {
                a: 0,
                b: self.wire(e),
            },
        };
        Ok(answer)
    }
}

impl mm_plugin::rpc::PluginApiStreams for AppPluginApi {}
impl mm_plugin::rpc::PluginApiHttp for AppPluginApi {}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Option<Interface> {
        Some(Interface::string(text))
    }

    #[test]
    fn pairs_become_fields_in_order() {
        let (fields, complaints) = log_fields(&[s("a"), s("1"), s("b"), s("2")]);
        assert_eq!(
            fields,
            vec![("a".into(), "1".into()), ("b".into(), "2".into())]
        );
        assert!(complaints.is_empty());
        assert_eq!(log_fields(&[]), (vec![], vec![]));
    }

    /// The last argument alone ends the walk; the pairs before it are kept.
    #[test]
    fn a_dangling_argument_is_a_complaint_not_a_field() {
        let (fields, complaints) = log_fields(&[s("a"), s("1"), s("dangling")]);
        assert_eq!(fields, vec![("a".into(), "1".into())]);
        assert_eq!(
            complaints,
            vec![LogComplaint {
                message: "invalid key/value pair",
                detail: "arg=dangling".into(),
            }]
        );
    }

    /// A key that is not a string drops its pair and names the key's index; the walk goes on.
    #[test]
    fn a_key_that_is_not_a_string_is_skipped_with_its_value() {
        let (fields, complaints) = log_fields(&[
            Some(Interface::int(7)),
            s("lost"),
            s("b"),
            None,
            None,
            s("x"),
        ]);
        assert_eq!(fields, vec![("b".into(), "<nil>".into())]);
        assert_eq!(
            complaints,
            vec![
                LogComplaint {
                    message: "invalid key for key/value pair",
                    detail: "pos=0".into(),
                },
                LogComplaint {
                    message: "invalid key for key/value pair",
                    detail: "pos=4".into(),
                },
            ]
        );
        let (fields, _) = log_fields(&[
            s("n"),
            Some(Interface::int(-3)),
            s("t"),
            Some(Interface::bool(true)),
        ]);
        assert_eq!(
            fields,
            vec![("n".into(), "-3".into()), ("t".into(), "true".into())]
        );
    }

    #[test]
    fn an_empty_slice_is_go_nil() {
        assert_eq!(bytes(b""), None);
        assert_eq!(bytes(b"x"), Some(&b"x"[..]));
    }

    /// Only exported fields cross: the wrapped cause is not `DetailedError`, and the status is
    /// widened, not reinterpreted.
    #[test]
    fn an_app_error_crosses_without_its_wrapped_cause() {
        let err = Box::new(
            AppError::new(
                "ListPluginKeys",
                "app.plugin_store.list.app_error",
                None,
                "d",
                500,
            )
            .wrap(std::io::Error::other("driver")),
        );
        let wire = wire_app_error(err, "en");
        assert_eq!(wire.id, "app.plugin_store.list.app_error");
        assert_eq!(wire.detailed_error, "d");
        assert_eq!(wire.status_code, 500);
        assert_eq!(wire.r#where, "ListPluginKeys");
        assert!(!wire.skip_translation);
    }
}
