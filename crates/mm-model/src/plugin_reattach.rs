//! Port of `model/plugin_reattach.go` — the serialisable form of go-plugin's `ReattachConfig`,
//! used when the server re-attaches to an already-running plugin process (local development).
//!
//! # Not ported
//!
//! `NewPluginReattachConfig` and `ToHashicorpPluginReattachmentConfig` convert to and from
//! `github.com/hashicorp/go-plugin`'s own type. The Rust counterpart is `goplugin::ReattachConfig`,
//! which this crate cannot name; `mm_plugin::environment` converts.
//!
//! # The wire form
//!
//! None of these structs has a `json:` tag, so the keys are Go's field names, matched
//! case-insensitively: `{"manifest":…,"pluginreattachconfig":{"pid":1}}` is a request.
//! [`PluginReattachRequest::from_json`] is `json.NewDecoder(r.Body).Decode(&req)` in
//! `reattachPlugin` (api4/plugin_local.go:30).

use serde::Deserialize;

use crate::go_json::{GoFields, remap_object_keys};
use crate::manifest::Manifest;
use crate::utils::{AppError, AppResult};

/// Port of `net.UnixAddr` as `PluginReattachConfig` uses it — a path plus a network name
/// (`unix`, `unixgram`, `unixpacket`). Not a Mattermost type; declared here because Rust's
/// `std::os::unix::net` has no address type that carries the network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UnixAddr {
    /// `Name` in Go — the socket path.
    #[serde(rename = "Name")]
    pub name: String,
    /// `Net` in Go.
    #[serde(rename = "Net")]
    pub net: String,
}

/// Port of `model.PluginReattachConfig` (plugin_reattach.go:12). No `json:` tags: Go relies on
/// the field names, so the wire keys are `Protocol`, `ProtocolVersion`, `Addr`, `Pid`, `Test`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PluginReattachConfig {
    #[serde(rename = "Protocol")]
    pub protocol: String,
    #[serde(rename = "ProtocolVersion")]
    pub protocol_version: i64,
    #[serde(rename = "Addr")]
    pub addr: UnixAddr,
    #[serde(rename = "Pid")]
    pub pid: i64,
    #[serde(rename = "Test")]
    pub test: bool,
}

/// Port of `model.PluginReattachRequest` (plugin_reattach.go:45).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PluginReattachRequest {
    #[serde(rename = "Manifest")]
    pub manifest: Option<Manifest>,
    #[serde(rename = "PluginReattachConfig")]
    pub plugin_reattach_config: Option<PluginReattachConfig>,
}

const UNIX_ADDR_FIELDS: GoFields = GoFields {
    names: &["Name", "Net"],
    nested: &[],
};

const CONFIG_FIELDS: GoFields = GoFields {
    names: &["Protocol", "ProtocolVersion", "Addr", "Pid", "Test"],
    nested: &[("Addr", &UNIX_ADDR_FIELDS)],
};

const MANIFEST_SERVER_FIELDS: GoFields = GoFields {
    names: &["executables", "executable"],
    nested: &[],
};

const MANIFEST_WEBAPP_FIELDS: GoFields = GoFields {
    names: &["bundle_path"],
    nested: &[],
};

/// `model.Manifest`'s top level and its two component sections — the parts `Reattach` reads.
/// The settings schema below them keeps serde's exact matching.
const MANIFEST_FIELDS: GoFields = GoFields {
    names: &[
        "id",
        "name",
        "description",
        "homepage_url",
        "support_url",
        "release_notes_url",
        "icon_path",
        "version",
        "min_server_version",
        "server",
        "webapp",
        "settings_schema",
        "props",
    ],
    nested: &[
        ("server", &MANIFEST_SERVER_FIELDS),
        ("webapp", &MANIFEST_WEBAPP_FIELDS),
    ],
};

const REQUEST_FIELDS: GoFields = GoFields {
    names: &["Manifest", "PluginReattachConfig"],
    nested: &[
        ("Manifest", &MANIFEST_FIELDS),
        ("PluginReattachConfig", &CONFIG_FIELDS),
    ],
};

/// One struct level as Go's decoder sees it: `null` members skipped (Go leaves the field alone,
/// which on a fresh struct is the zero value; serde refuses `null` for a `String`), and anything
/// but an object or `null` refused (serde's derive would also take a struct as a sequence).
fn struct_level(value: Option<&mut serde_json::Value>) -> Result<(), serde_json::Error> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::Object(object)) => {
            object.retain(|_, v| !v.is_null());
            Ok(())
        }
        Some(_) => Err(<serde_json::Error as serde::de::Error>::custom(
            "json: cannot unmarshal a non-object into a struct",
        )),
    }
}

impl PluginReattachRequest {
    /// `json.NewDecoder(body).Decode(&req)` (api4/plugin_local.go:30): the first JSON value in
    /// the body, keys folded as Go folds them. `Err` is the decoder's failure, which the handler
    /// turns into its 400.
    ///
    /// A `null` body decodes to the empty request, as in Go, so `IsValid` refuses it rather than
    /// the decoder. `null` members are skipped on the request, the config, its address, and the
    /// manifest's top level and `server`/`webapp`; deeper in the manifest they reach serde, which
    /// refuses one where Go would not.
    pub fn from_json(body: &[u8]) -> Result<Self, serde_json::Error> {
        let mut value: serde_json::Value = crate::utils::decode_one_from_json(body)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        remap_object_keys(&mut value, &REQUEST_FIELDS);
        struct_level(Some(&mut value))?;
        if let Some(object) = value.as_object_mut() {
            if let Some(config) = object.get_mut("PluginReattachConfig") {
                struct_level(Some(config))?;
                struct_level(config.get_mut("Addr"))?;
            }
            if let Some(manifest) = object.get_mut("Manifest") {
                struct_level(Some(manifest))?;
                struct_level(manifest.get_mut("server"))?;
                struct_level(manifest.get_mut("webapp"))?;
            }
        }
        serde_json::from_value(value)
    }

    /// Port of `(*PluginReattachRequest).IsValid` (plugin_reattach.go:50).
    ///
    /// The error ids are `plugin_reattach_request.is_valid.*` — **no `model.` prefix**, unlike
    /// every other validator in the package.
    pub fn is_valid(&self) -> AppResult {
        if self.manifest.is_none() {
            return Err(err("manifest"));
        }
        if self.plugin_reattach_config.is_none() {
            return Err(err("plugin_reattach_config"));
        }

        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "PluginReattachRequest.IsValid",
        format!("plugin_reattach_request.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_fold_and_nulls_are_skipped_as_go_does() {
        let r = PluginReattachRequest::from_json(
            br#"{"MANIFEST":{"ID":"x","Name":null,"SERVER":{"Executable":null}},
                "pluginreattachconfig":{"PROTOCOL":"netrpc","protocolversion":1,
                "addr":{"name":"/tmp/p","NET":"unix"},"Pid":7,"test":true}} trailing"#,
        )
        .unwrap();
        let m = r.manifest.unwrap();
        assert_eq!(m.id, "x");
        assert!(m.has_server());
        assert_eq!(
            r.plugin_reattach_config.unwrap(),
            PluginReattachConfig {
                protocol: "netrpc".into(),
                protocol_version: 1,
                addr: UnixAddr {
                    name: "/tmp/p".into(),
                    net: "unix".into()
                },
                pid: 7,
                test: true,
            }
        );
    }

    #[test]
    fn the_decoder_and_validation_refuse_as_go_does() {
        for bad in [
            &b""[..],
            b"[]",
            b"\"x\"",
            br#"{"Manifest":[]}"#,
            br#"{"manifest":{"server":[]}}"#,
            br#"{"PluginReattachConfig":{"Addr":["a","unix"]}}"#,
            br#"{"PluginReattachConfig":{"Pid":1.5}}"#,
            b"{",
        ] {
            assert!(PluginReattachRequest::from_json(bad).is_err(), "{bad:?}");
        }
        let id = |body: &[u8]| {
            PluginReattachRequest::from_json(body)
                .unwrap()
                .is_valid()
                .err()
                .map(|e| e.id.clone())
        };
        assert_eq!(
            id(b"null").as_deref(),
            Some("plugin_reattach_request.is_valid.manifest.app_error")
        );
        assert_eq!(
            id(br#"{"Manifest":null,"PluginReattachConfig":{}}"#).as_deref(),
            Some("plugin_reattach_request.is_valid.manifest.app_error")
        );
        assert_eq!(
            id(br#"{"Manifest":{}}"#).as_deref(),
            Some("plugin_reattach_request.is_valid.plugin_reattach_config.app_error")
        );
        assert_eq!(id(br#"{"Manifest":{},"PluginReattachConfig":{}}"#), None);
    }
}
