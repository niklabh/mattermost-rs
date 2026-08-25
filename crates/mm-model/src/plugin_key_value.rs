//! Port of `model/plugin_key_value.go` — one row of a plugin's KV store.
//!
//! # The `db:` tags are not the `json:` tags
//!
//! `Key` is stored in the column **`PKey`** and `Value` in **`PValue`** — `key` and `value` are
//! reserved words in enough dialects that Mattermost renamed the columns. A query written from
//! the JSON tags returns rows this type cannot fill.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult};

/// Port of `model.KeyValuePluginIdMaxRunes` (plugin_key_value.go:11).
pub const KEY_VALUE_PLUGIN_ID_MAX_RUNES: usize = 190;
/// Port of `model.KeyValueKeyMaxRunes` (plugin_key_value.go:12).
pub const KEY_VALUE_KEY_MAX_RUNES: usize = 150;

/// Port of `model.PluginKeyValue` (plugin_key_value.go:15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginKeyValue {
    #[serde(rename = "plugin_id")]
    pub plugin_id: String,

    /// Column `PKey`.
    #[serde(rename = "key")]
    pub key: String,

    /// Column `PValue`. base64 on the wire — see [`crate::go_bytes`].
    #[serde(rename = "value", with = "crate::go_bytes")]
    pub value: Option<Vec<u8>>,

    /// Epoch milliseconds; `0` means never.
    #[serde(rename = "expire_at")]
    pub expire_at: i64,
}

impl PluginKeyValue {
    /// Port of `(*PluginKeyValue).IsValid` (plugin_key_value.go:22).
    ///
    /// Two things a reader would tidy up and break:
    ///
    /// - the **plugin id** branch reports `"Max": KeyValueKeyMaxRunes` — 150, the *key's* limit —
    ///   while actually enforcing 190. That is Go's bug and it is on the wire, inside the i18n
    ///   params of the error body.
    /// - both branches put `key=` in `detailed_error`, so a bad plugin id is reported with the
    ///   key, not the id.
    pub fn is_valid(&self) -> AppResult {
        if self.plugin_id.is_empty()
            || self.plugin_id.chars().count() > KEY_VALUE_PLUGIN_ID_MAX_RUNES
        {
            return Err(self.err("plugin_id"));
        }

        if self.key.is_empty() || self.key.chars().count() > KEY_VALUE_KEY_MAX_RUNES {
            return Err(self.err("key"));
        }

        Ok(())
    }

    fn err(&self, field: &str) -> Box<AppError> {
        let mut params = std::collections::HashMap::new();
        // Go passes KeyValueKeyMaxRunes in *both* branches — see the note on `is_valid`.
        params.insert(
            "Max".to_string(),
            serde_json::Value::from(KEY_VALUE_KEY_MAX_RUNES as i64),
        );
        params.insert("Min".to_string(), serde_json::Value::from(0));
        Box::new(AppError::new(
            "PluginKeyValue.IsValid",
            format!("model.plugin_key_value.is_valid.{field}.app_error"),
            Some(params),
            format!("key={}", self.key),
            400,
        ))
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn plugin_key_value_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginKeyValue, "plugin_key_value");
    }
}
