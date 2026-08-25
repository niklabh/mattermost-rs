//! Port of `model/plugin_kvset_options.go` — how a value is written to the plugin KV store.

use crate::plugin_key_value::PluginKeyValue;
use crate::utils::{AppError, AppResult, get_millis};

/// Port of `model.PluginKVSetOptions` (plugin_kvset_options.go:11). No `json:` tags — this is an
/// argument bag, not a wire type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginKVSetOptions {
    /// Write only if the stored value still equals [`old_value`](Self::old_value).
    pub atomic: bool,
    /// Consulted only when `atomic`. `None` is Go's nil, which is a *meaningful* value here: it
    /// means "compare against absent", i.e. create-if-missing.
    pub old_value: Option<Vec<u8>>,
    /// Seconds; `0` means no expiry.
    pub expire_in_seconds: i64,
}

impl PluginKVSetOptions {
    /// Port of `(*PluginKVSetOptions).IsValid` (plugin_kvset_options.go:18).
    ///
    /// The single rule: an `old_value` without `atomic` is a caller mistake, because the compare
    /// would be silently ignored. Note it tests `!= nil`, so `Some(vec![])` — an explicit empty
    /// comparand — is also rejected without `atomic`.
    pub fn is_valid(&self) -> AppResult {
        if !self.atomic && self.old_value.is_some() {
            return Err(Box::new(AppError::new(
                "PluginKVSetOptions.IsValid",
                "model.plugin_kvset_options.is_valid.old_value.app_error",
                None,
                "",
                400,
            )));
        }

        Ok(())
    }
}

/// Port of `model.NewPluginKeyValueFromOptions` (plugin_kvset_options.go:32).
///
/// Go's signature returns `(*PluginKeyValue, *AppError)` but the error is **always nil** — it
/// never validates `opt`. Reproduced: this cannot fail, so it returns the value directly rather
/// than a `Result` that a caller would have to pretend to handle.
pub fn new_plugin_key_value_from_options(
    plugin_id: impl Into<String>,
    key: impl Into<String>,
    value: Option<Vec<u8>>,
    opt: &PluginKVSetOptions,
) -> PluginKeyValue {
    let expire_at = if opt.expire_in_seconds != 0 {
        get_millis() + (opt.expire_in_seconds * 1000)
    } else {
        0
    };

    PluginKeyValue {
        plugin_id: plugin_id.into(),
        key: key.into(),
        value,
        expire_at,
    }
}
