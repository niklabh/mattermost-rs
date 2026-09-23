//! The plugin API's configuration and licence answers (app/plugin_api.go:50-145): what
//! `GetConfig`, `GetUnsanitizedConfig`, `GetPluginConfig`, `LoadPluginConfiguration` and
//! `GetLicense` hand a plugin, and what `SavePluginConfig` writes.
//!
//! # A `*model.Config` is built from its JSON, generically
//!
//! Go's `GetConfig` and `GetUnsanitizedConfig` answer `Config().Clone()`, and `Clone` is a JSON
//! round trip (config.go:4243). So what gob sends is exactly what `encoding/json` makes of the
//! document — a JSON `{}` is an empty non-nil map, which gob **sends**, and a `null` is a nil
//! one, which it omits — and this server holds that document already (`mm_model::config`, which
//! `GET /config` serves byte for byte). [`config_to_gob`] walks the JSON against the gob shape
//! of `model.Config` — taken from the generated wire struct, so it has every field Go's has —
//! and builds a [`gobwire::Value`], served whole through `mm_plugin::rpc::PluginApiDynamic`.
//!
//! Two facts make the walk generic, and `fixtures/plugin/idl.json` pins both (tests below):
//! every JSON key under `model.Config` is the gob field name (its only three `json:` tags are
//! `,omitempty` without a rename), and there is one embedded struct, which JSON flattens into its
//! parent and gob sends as a field named for its type ([`EMBEDDED`]).
//!
//! # A `*model.License` is mapped by hand
//!
//! Its 74 `json:` tags are all renames, and the four structs are small, so [`license_to_wire`]
//! maps `mm_model::license` field by field. gob omits a zero pointer as it omits a nil one, so a
//! `Some(false)` and a `None` feature cross identically; a nil `Customer`, `Features` or
//! `Limits` does not cross at all, where an empty one crosses as an empty struct.

use std::sync::{Arc, OnceLock};

use gobwire::{Dynamic, Interface, StructType, Type, Value};
use mm_model::license::License;
use mm_model::manifest::Manifest;
use mm_model::utils::{AppError, StringInterface};
use mm_plugin::wire::model as wire;
use serde_json::{Map, Value as Json};

use crate::App;

/// The embedded struct fields reachable from `model.Config`, as (struct, field): JSON flattens
/// them into the enclosing object, gob sends each as one field named for its type.
pub const EMBEDDED: [(&str, &str); 1] =
    [("ContentFlaggingSettings", "ContentFlaggingSettingsBase")];

/// Why a configuration document could not be put into gob's shape.
#[derive(Debug, thiserror::Error)]
pub enum ConfigGobError {
    /// A JSON value of the wrong kind for its gob field.
    #[error("{path}: JSON {found} where gob wants {wanted}")]
    Mismatch {
        path: String,
        wanted: &'static str,
        found: &'static str,
    },
    /// A gob kind this walk has no JSON reading for.
    #[error("{path}: no JSON reading for a gob {what}")]
    Unsupported { path: String, what: &'static str },
    /// `model.Config`'s gob type could not be described.
    #[error("the gob type of model.Config: {0}")]
    Describe(String),
}

/// The gob shape of `model.Config`: the generated wire struct, described and read back as a
/// dynamic type, so every field Go sends is here with its wire kind.
pub fn config_type() -> Result<&'static Type, ConfigGobError> {
    static TYPE: OnceLock<Result<Type, String>> = OnceLock::new();
    TYPE.get_or_init(|| {
        Interface::new("Config", &wire::Config::default())
            .map(|i| i.ty)
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| ConfigGobError::Describe(e.clone()))
}

fn kind(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "bool",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

/// The walk over one document: the struct types it is inside (for [`Type::Ref`]), and the JSON
/// keys that matched no gob field.
#[derive(Default)]
struct Walk {
    structs: Vec<Arc<StructType>>,
    unmatched: Vec<String>,
}

impl Walk {
    fn resolve(&self, ty: &Type) -> Type {
        match ty {
            Type::Ref(n) => self
                .structs
                .len()
                .checked_sub(n + 1)
                .and_then(|i| self.structs.get(i))
                .map_or_else(|| ty.clone(), |st| Type::Struct(Arc::clone(st))),
            other => other.clone(),
        }
    }

    /// `json` as a value of `ty`. `None` is a field gob omits; only a struct field can be one,
    /// so an `element` (of a slice or map) that is JSON `null` is the type's zero value — which
    /// is what gob sends for a nil map or interface element.
    ///
    /// A struct field that is `null` or absent is taken for a nil pointer and omitted. Go would
    /// send an empty struct for a **value** struct there, which gob's type does not distinguish;
    /// the documents this is given are `mm_model::config::Config` serialised, which always
    /// writes its value structs.
    fn value(
        &mut self,
        ty: &Type,
        json: &Json,
        path: &str,
        element: bool,
    ) -> Result<Option<Value>, ConfigGobError> {
        let ty = self.resolve(ty);
        if ty == Type::Bytes {
            return Ok(Some(raw_message(json)));
        }
        if json.is_null() {
            return Ok(element.then(|| zero(&ty)));
        }
        let mismatch = |wanted: &'static str| ConfigGobError::Mismatch {
            path: path.to_owned(),
            wanted,
            found: kind(json),
        };
        Ok(Some(match &ty {
            Type::Bool => Value::Bool(json.as_bool().ok_or_else(|| mismatch("bool"))?),
            Type::Int => Value::Int(json.as_i64().ok_or_else(|| mismatch("int"))?),
            Type::Uint => Value::Uint(json.as_u64().ok_or_else(|| mismatch("uint"))?),
            Type::Float => Value::Float(json.as_f64().ok_or_else(|| mismatch("float"))?),
            Type::String => Value::String(json.as_str().ok_or_else(|| mismatch("string"))?.into()),
            Type::Bytes => raw_message(json),
            Type::Interface => {
                Value::Interface(mm_plugin::rpc::json_to_interface(json).map(Box::new))
            }
            Type::Slice(elem) => {
                let items = json.as_array().ok_or_else(|| mismatch("slice"))?;
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.extend(self.value(elem, item, &format!("{path}[{i}]"), true)?);
                }
                Value::Slice(out)
            }
            Type::Array(elem, len) => {
                let items = json.as_array().ok_or_else(|| mismatch("array"))?;
                if items.len() != *len {
                    return Err(mismatch("array"));
                }
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.extend(self.value(elem, item, &format!("{path}[{i}]"), true)?);
                }
                Value::Array(out)
            }
            Type::Map(key, elem) => {
                let object = json.as_object().ok_or_else(|| mismatch("map"))?;
                if **key != Type::String {
                    return Err(ConfigGobError::Unsupported {
                        path: path.to_owned(),
                        what: "map with a key that is not a string",
                    });
                }
                let mut pairs = Vec::with_capacity(object.len());
                for (k, v) in object {
                    if let Some(v) = self.value(elem, v, &format!("{path}[{k}]"), true)? {
                        pairs.push((Value::String(k.clone()), v));
                    }
                }
                Value::Map(pairs)
            }
            Type::Struct(st) => {
                let object = json.as_object().ok_or_else(|| mismatch("struct"))?;
                return self.structure(st, object, path, true).map(Some);
            }
            Type::Complex | Type::Marshaler(..) | Type::Ref(_) => {
                return Err(ConfigGobError::Unsupported {
                    path: path.to_owned(),
                    what: "complex, marshaler or unresolved type",
                });
            }
        }))
    }

    /// A struct from `object`, field by gob name; an [`EMBEDDED`] field reads the same object.
    /// `report` is false for an embedded struct, whose parent accounts for the keys.
    fn structure(
        &mut self,
        st: &Arc<StructType>,
        object: &Map<String, Json>,
        path: &str,
        report: bool,
    ) -> Result<Value, ConfigGobError> {
        self.structs.push(Arc::clone(st));
        let mut fields = Vec::with_capacity(st.fields.len());
        // Owned: an embedded struct's type is resolved into a temporary.
        let mut claimed: Vec<String> = Vec::new();
        for (name, ty) in &st.fields {
            let field_path = format!("{path}.{name}");
            let embedded = EMBEDDED.contains(&(st.name.as_str(), name.as_str()));
            let value = match (embedded, self.resolve(ty)) {
                (true, Type::Struct(inner)) => {
                    claimed.extend(inner.fields.iter().map(|(n, _)| n.clone()));
                    Some(self.structure(&inner, object, &field_path, false)?)
                }
                _ => {
                    claimed.push(name.clone());
                    match object.get(name) {
                        Some(json) => self.value(ty, json, &field_path, false)?,
                        // A `json.RawMessage` the document lacks is still `null` after `Clone`.
                        None if self.resolve(ty) == Type::Bytes => Some(raw_message(&Json::Null)),
                        None => None,
                    }
                }
            };
            fields.push(value);
        }
        if report {
            self.unmatched.extend(
                object
                    .keys()
                    .filter(|k| !claimed.contains(k))
                    .map(|k| format!("{path}.{k}")),
            );
        }
        self.structs.pop();
        Ok(Value::Struct(fields))
    }
}

/// A `json.RawMessage` after `Config.Clone`: the value's JSON text, compacted and HTML-escaped
/// as `json.Marshal` writes a raw message — and never nil, since a `null` (or an absent key)
/// comes back as the four bytes `null`. The only `[]byte` fields under `model.Config` are the two
/// `AdvancedLoggingJSON`s, both raw messages (the IDL test holds it to that).
///
/// `mm_model` holds the value as a `serde_json::Value`, whose object keys are sorted, so a stored
/// object whose keys are out of order comes out in a different order than Go's copy keeps.
fn raw_message(json: &Json) -> Value {
    Value::Bytes(mm_model::utils::go_json_escape(&json.to_string()).into_bytes())
}

/// The zero value of a gob type, which is what gob sends for a nil element.
fn zero(ty: &Type) -> Value {
    match ty {
        Type::Bool => Value::Bool(false),
        Type::Int => Value::Int(0),
        Type::Uint => Value::Uint(0),
        Type::Float => Value::Float(0.0),
        Type::Complex => Value::Complex(0.0, 0.0),
        Type::Bytes => Value::Bytes(Vec::new()),
        Type::String => Value::String(String::new()),
        Type::Interface => Value::Interface(None),
        Type::Slice(_) => Value::Slice(Vec::new()),
        Type::Array(elem, len) => Value::Array(vec![zero(elem); *len]),
        Type::Map(..) => Value::Map(Vec::new()),
        Type::Struct(st) => Value::Struct(vec![None; st.fields.len()]),
        Type::Marshaler(..) => Value::Marshaled(Vec::new()),
        Type::Ref(_) => Value::Struct(Vec::new()),
    }
}

/// A `model.Config` document as the gob value of `model.Config`, and the JSON keys that matched
/// no gob field (none, for a document this server serialised; the tests hold it to that).
pub fn config_to_gob(json: &Json) -> Result<(Value, Vec<String>), ConfigGobError> {
    let ty = config_type()?;
    let mut walk = Walk::default();
    let value = walk
        .value(ty, json, "Config", false)?
        .unwrap_or_else(|| zero(ty));
    Ok((value, walk.unmatched))
}

/// A one-field `Z_<Method>Returns` struct around `value` (`None` omits the field: a nil `A`).
fn returns(name: &str, ty: Type, value: Option<Value>) -> Dynamic {
    Dynamic {
        ty: Type::Struct(Arc::new(StructType {
            name: name.to_owned(),
            fields: vec![("A".to_owned(), ty)],
        })),
        value: Value::Struct(vec![value]),
    }
}

/// `Z_GetConfigReturns` or `Z_GetUnsanitizedConfigReturns` (`name`) carrying `config`, or a nil
/// config when there is none.
pub fn config_returns(name: &str, config: Option<&Json>) -> Result<Dynamic, ConfigGobError> {
    let ty = config_type()?.clone();
    let value = match config {
        Some(json) => {
            let (value, unmatched) = config_to_gob(json)?;
            if !unmatched.is_empty() {
                tracing::warn!(?unmatched, "configuration keys with no gob field");
            }
            Some(value)
        }
        None => None,
    };
    Ok(returns(name, ty, value))
}

/// Port of the lookup in `PluginAPI.GetPluginConfig` (app/plugin_api.go:119) as gob sends it:
/// the plugin's entry — a nil one (`null` in the document) is omitted — or, when there is no
/// entry at all, `map[string]any{}`, which is empty, non-nil, and **sent**.
pub fn plugin_config_returns(plugins: Option<&PluginsMap>, id: &str) -> Dynamic {
    let ty = Type::Map(Arc::new(Type::String), Arc::new(Type::Interface));
    let value = match plugins.and_then(|p| p.get(id)) {
        None => Some(Value::Map(Vec::new())),
        Some(None) => None,
        Some(Some(settings)) => Some(any_map(settings)),
    };
    returns("Z_GetPluginConfigReturns", ty, value)
}

/// `model.Config.PluginSettings.Plugins`.
pub type PluginsMap = std::collections::BTreeMap<String, Option<StringInterface>>;

/// A `map[string]any` after `encoding/json`: every value as `json.Unmarshal` leaves it.
fn any_map(settings: &StringInterface) -> Value {
    Value::Map(
        settings
            .iter()
            .map(|(k, v)| {
                (
                    Value::String(k.clone()),
                    Value::Interface(mm_plugin::rpc::json_to_interface(v).map(Box::new)),
                )
            })
            .collect(),
    )
}

/// What `SavePluginConfig` was sent, as the JSON Go's document will hold: `null` for a nil map
/// (the field absent from the arguments), an object otherwise.
pub fn plugin_config_from_args(args: &Dynamic) -> Result<Json, SavePluginConfigError> {
    let (Type::Struct(st), Value::Struct(values)) = (&args.ty, &args.value) else {
        return Err(SavePluginConfigError::Shape);
    };
    let Some(index) = st.fields.iter().position(|(name, _)| name == "A") else {
        return Ok(Json::Null);
    };
    match values.get(index) {
        None | Some(None) => Ok(Json::Null),
        Some(Some(Value::Map(pairs))) => {
            let mut object = Map::new();
            for (key, value) in pairs {
                let Value::String(key) = key else {
                    return Err(SavePluginConfigError::Shape);
                };
                let value = match value {
                    Value::Interface(None) => Json::Null,
                    Value::Interface(Some(i)) => mm_plugin::wire::interface_to_json(i)
                        .ok_or_else(|| SavePluginConfigError::NotJson(key.clone()))?,
                    _ => return Err(SavePluginConfigError::Shape),
                };
                object.insert(key.clone(), value);
            }
            Ok(Json::Object(object))
        }
        Some(Some(_)) => Err(SavePluginConfigError::Shape),
    }
}

/// Why a `SavePluginConfig` argument could not become the document's JSON.
#[derive(Debug, thiserror::Error)]
pub enum SavePluginConfigError {
    /// Not a `Z_SavePluginConfigArgs` with a `map[string]any`.
    #[error("the arguments are not a map[string]any")]
    Shape,
    /// A value `json.Marshal` would refuse, so Go's save fails on it.
    #[error("the value of {0} has no JSON form")]
    NotJson(String),
}

/// Port of `PluginAPI.LoadPluginConfiguration` (app/plugin_api.go:50) with the host half of
/// `apiRPCServer.LoadPluginConfiguration` (client_rpc.go:398): the manifest's defaults under
/// their lower-cased keys (top-level settings, then each section's), overridden by the stored
/// settings, also lower-cased — and the bytes `json.Marshal` makes of that `map[string]any`.
///
/// The unsanitised settings: a secret reaches its own plugin. Keys that differ only in case end
/// up as one, the last written winning; Go ranges the stored map in random order, this in key
/// order, so two stored spellings of one key are the one divergence.
pub fn plugin_configuration(manifest: &Manifest, settings: Option<&StringInterface>) -> Vec<u8> {
    use mm_model::utils::go_to_lower;
    let mut config = Map::new();
    if let Some(schema) = manifest.settings_schema.as_ref() {
        for setting in schema.settings.iter().flatten() {
            config.insert(go_to_lower(&setting.key), setting.default.clone());
        }
        for section in schema.sections.iter().flatten() {
            for setting in section.settings.iter().flatten() {
                config.insert(go_to_lower(&setting.key), setting.default.clone());
            }
        }
    }
    for (key, value) in settings.into_iter().flatten() {
        config.insert(go_to_lower(key), value.clone());
    }
    let mut out = String::new();
    go_marshal_any(&Json::Object(config), &mut out);
    mm_model::utils::go_json_escape(&out).into_bytes()
}

/// `json.Marshal` of what `json.Unmarshal` into an `any` left: every number a float64 (so Go's
/// float formatting), every object a map with sorted keys. Escaping of `<`, `>`, `&` and the two
/// line separators is left to `go_json_escape` over the whole text.
fn go_marshal_any(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            // A JSON number is finite, so Go's formatter always has an answer.
            out.push_str(&mm_model::utils::go_json_format_float(f).unwrap_or_else(|| "0".into()));
        }
        Json::String(s) => out.push_str(&Json::String(s.clone()).to_string()),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                go_marshal_any(item, out);
            }
            out.push(']');
        }
        Json::Object(fields) => {
            // `serde_json::Map` is ordered by key, byte for byte, as Go sorts a map's keys.
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Json::String(key.clone()).to_string());
                out.push(':');
                go_marshal_any(item, out);
            }
            out.push('}');
        }
    }
}

/// `model.License` as gob sends it, field for field (license.go:79).
pub fn license_to_wire(license: &License) -> wire::License {
    wire::License {
        id: license.id.clone(),
        issued_at: license.issued_at,
        starts_at: license.starts_at,
        expires_at: license.expires_at,
        customer: license.customer.as_ref().map(|c| {
            Box::new(wire::Customer {
                id: c.id.clone(),
                name: c.name.clone(),
                email: c.email.clone(),
                company: c.company.clone(),
            })
        }),
        features: license.features.as_ref().map(|f| {
            Box::new(wire::Features {
                users: f.users,
                ldap: f.ldap,
                ldap_groups: f.ldap_groups,
                mfa: f.mfa,
                google_o_auth: f.google_oauth,
                office365_o_auth: f.office365_oauth,
                open_id: f.open_id,
                compliance: f.compliance,
                cluster: f.cluster,
                metrics: f.metrics,
                mhpns: f.mhpns,
                saml: f.saml,
                elasticsearch: f.elasticsearch,
                announcement: f.announcement,
                theme_management: f.theme_management,
                email_notification_contents: f.email_notification_contents,
                data_retention: f.data_retention,
                message_export: f.message_export,
                custom_permissions_schemes: f.custom_permissions_schemes,
                custom_terms_of_service: f.custom_terms_of_service,
                guest_accounts: f.guest_accounts,
                guest_accounts_permissions: f.guest_accounts_permissions,
                id_loaded_push_notifications: f.id_loaded_push_notifications,
                lock_teammate_name_display: f.lock_teammate_name_display,
                enterprise_plugins: f.enterprise_plugins,
                advanced_logging: f.advanced_logging,
                cloud: f.cloud,
                shared_channels: f.shared_channels,
                remote_cluster_service: f.remote_cluster_service,
                outgoing_o_auth_connections: f.outgoing_oauth_connections,
                auto_translation: f.auto_translation,
                future_features: f.future_features,
            })
        }),
        sku_name: license.sku_name.clone(),
        sku_short_name: license.sku_short_name.clone(),
        is_trial: license.is_trial,
        is_gov_sku: license.is_gov_sku,
        is_non_production: license.is_non_production,
        is_seat_count_enforced: license.is_seat_count_enforced,
        extra_users: license.extra_users,
        signup_jwt: license.signup_jwt.clone(),
        limits: license.limits.as_ref().map(|l| {
            Box::new(wire::LicenseLimits {
                post_history: l.post_history,
                board_cards: l.board_cards,
                playbook_runs: l.playbook_runs,
                call_duration_seconds: l.call_duration_seconds,
                agents_prompts: l.agents_prompts,
                push_notifications: l.push_notifications,
            })
        }),
    }
}

impl App {
    /// Port of `PluginAPI.SavePluginConfig` (app/plugin_api.go:127): the plugin's entry in
    /// `PluginSettings.Plugins` replaced by `settings` (`null` for a nil map), and the document
    /// saved.
    ///
    /// Go clones its configuration, sets the entry and calls `SaveConfig`. This server saves
    /// through the Go server instead (`crate::peer_config`), as a `PATCH /config` naming only this
    /// entry — `patchConfig` keeps every other plugin's entry (api4/config.go:344) and
    /// `config.Merge` replaces a map it is given whole, so the result is Go's. The one thing the
    /// detour changes is whose `ConfigurationWillBeSaved` hooks run: the Go server's, not this
    /// host's ([D-1000]). Any failure is Go's `app.save_config.app_error`, the error `SaveConfig`
    /// wraps every store failure in.
    pub async fn save_plugin_config(&self, id: &str, settings: Json) -> Result<(), Box<AppError>> {
        let failed = |detail: String| {
            tracing::warn!(plugin_id = %id, %detail, "SavePluginConfig failed");
            AppError::boxed("saveConfig", "app.save_config.app_error", None, "", 500)
        };
        let mut plugins = Map::new();
        plugins.insert(id.to_owned(), settings);
        let patch = serde_json::json!({ "PluginSettings": { "Plugins": plugins } });
        let Some(peer) = self.peer_config() else {
            return Err(failed(
                "no Go server to save the configuration through".into(),
            ));
        };
        peer.patch_config(&patch)
            .await
            .map_err(|e| failed(e.to_string()))?;
        if let Err(err) = self.refresh_config().await {
            tracing::warn!(error = %err, "could not reload the configuration after SavePluginConfig");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Json {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(path).expect("the fixture")).expect("JSON")
    }

    /// Every struct field of a value, recursively: the paths of those the walk left `None`.
    fn missing(ty: &Type, value: &Value, path: &str, out: &mut Vec<String>) {
        if let (Type::Struct(st), Value::Struct(fields)) = (ty, value) {
            for ((name, fty), field) in st.fields.iter().zip(fields) {
                match field {
                    Some(v) => missing(fty, v, &format!("{path}.{name}"), out),
                    None => out.push(format!("{path}.{name}")),
                }
            }
        }
    }

    /// `fixtures/config.json` is Go's `model.Config` with every field distinctive and non-zero,
    /// so the walk must place every key and fill every field: a key with no gob field, or a gob
    /// field no key reached, is a naming assumption that broke.
    #[test]
    fn every_key_of_the_go_fixture_lands_in_a_gob_field() {
        let json = fixture("config.json");
        let (value, unmatched) = config_to_gob(&json).expect("converts");
        assert!(unmatched.is_empty(), "{unmatched:?}");
        let mut holes = Vec::new();
        missing(config_type().unwrap(), &value, "Config", &mut holes);
        assert!(holes.is_empty(), "{holes:?}");
    }

    /// The same document through this server's own model: what `GetConfig` actually converts.
    #[test]
    fn the_model_config_converts_without_a_stray_key() {
        let config: mm_model::config::Config =
            serde_json::from_value(fixture("config.json")).expect("the model reads it");
        let json = serde_json::to_value(&config).expect("serialises");
        let (_, unmatched) = config_to_gob(&json).expect("converts");
        assert!(unmatched.is_empty(), "{unmatched:?}");
    }

    /// The converted value decodes into the generated struct with the fixture's values in it —
    /// including the embedded struct, which JSON flattened.
    #[test]
    fn the_gob_value_is_the_documents() {
        let json = fixture("config.json");
        let dynamic = config_returns("Z_GetConfigReturns", Some(&json)).expect("converts");
        let typed: mm_plugin::wire::plugin::Z_GetConfigReturns = Interface {
            name: String::new(),
            ty: dynamic.ty,
            value: dynamic.value,
        }
        .downcast()
        .expect("decodes");
        let config = typed.a.expect("a config");
        assert_eq!(
            config.service_settings.site_url.as_deref(),
            json["ServiceSettings"]["SiteURL"].as_str()
        );
        let flagging = &config
            .content_flagging_settings
            .content_flagging_settings_base;
        assert_eq!(
            flagging.enable_content_flagging,
            json["ContentFlaggingSettings"]["EnableContentFlagging"].as_bool()
        );
        let plugins = &config.plugin_settings.plugins;
        let first = json["PluginSettings"]["Plugins"]
            .as_object()
            .and_then(|m| m.iter().next())
            .expect("a plugin entry");
        assert_eq!(plugins[first.0].len(), first.1.as_object().unwrap().len());
    }

    /// `{}` is an empty non-nil map, which gob sends; `null` and an absent key are nil, omitted.
    /// A `null` **element** of a map is its zero value, since an element cannot be omitted.
    #[test]
    fn an_empty_map_is_sent_and_a_null_one_is_not() {
        let config =
            |plugins: Json| serde_json::json!({ "PluginSettings": { "Plugins": plugins } });
        let plugins_field = |json: &Json| -> Option<Value> {
            let (value, _) = config_to_gob(json).unwrap();
            let Type::Struct(st) = config_type().unwrap() else {
                unreachable!()
            };
            let Value::Struct(fields) = value else {
                unreachable!()
            };
            let at = st
                .fields
                .iter()
                .position(|(n, _)| n == "PluginSettings")
                .unwrap();
            let (Type::Struct(ps), Some(Value::Struct(inner))) = (&st.fields[at].1, &fields[at])
            else {
                unreachable!()
            };
            let at = ps.fields.iter().position(|(n, _)| n == "Plugins").unwrap();
            inner[at].clone()
        };
        assert_eq!(
            plugins_field(&config(serde_json::json!({}))),
            Some(Value::Map(vec![]))
        );
        assert_eq!(plugins_field(&config(Json::Null)), None);
        assert_eq!(
            plugins_field(&serde_json::json!({ "PluginSettings": {} })),
            None
        );
        assert_eq!(
            plugins_field(&config(serde_json::json!({ "a": null, "b": {} }))),
            Some(Value::Map(vec![
                (Value::String("a".into()), Value::Map(vec![])),
                (Value::String("b".into()), Value::Map(vec![])),
            ]))
        );
    }

    /// A raw message is its compact, HTML-escaped text, and `null` — present or absent — is the
    /// four bytes `null`, never nil (measured against Go's `Clone`).
    #[test]
    fn advanced_logging_json_crosses_as_its_text() {
        let raw = |json: Json| -> Option<Value> {
            let (value, _) = config_to_gob(&json).unwrap();
            let Value::Struct(fields) = value else {
                unreachable!()
            };
            let Type::Struct(st) = config_type().unwrap() else {
                unreachable!()
            };
            let at = st
                .fields
                .iter()
                .position(|(n, _)| n == "LogSettings")
                .unwrap();
            let (Type::Struct(ls), Some(Value::Struct(inner))) = (&st.fields[at].1, &fields[at])
            else {
                unreachable!()
            };
            let at = ls
                .fields
                .iter()
                .position(|(n, _)| n == "AdvancedLoggingJSON")
                .unwrap();
            inner[at].clone()
        };
        let text = |t: &str| Some(Value::Bytes(t.as_bytes().to_vec()));
        assert_eq!(raw(serde_json::json!({ "LogSettings": {} })), text("null"));
        assert_eq!(
            raw(serde_json::json!({ "LogSettings": { "AdvancedLoggingJSON": null } })),
            text("null")
        );
        assert_eq!(
            raw(serde_json::json!({ "LogSettings": { "AdvancedLoggingJSON": {} } })),
            text("{}")
        );
        assert_eq!(
            raw(serde_json::json!({ "LogSettings": { "AdvancedLoggingJSON": { "a": "<x>" } } })),
            text(r#"{"a":"\u003cx\u003e"}"#)
        );
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_named() {
        let err =
            config_to_gob(&serde_json::json!({ "ServiceSettings": { "SiteURL": 3 } })).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Config.ServiceSettings.SiteURL: JSON number where gob wants string"
        );
    }

    fn map_of(d: &Dynamic) -> Option<Value> {
        match &d.value {
            Value::Struct(fields) => fields[0].clone(),
            _ => unreachable!(),
        }
    }

    /// No entry is `map[string]any{}` (sent); a `null` entry is nil (omitted); a stored one is
    /// its values as `any`.
    #[test]
    fn get_plugin_config_tells_missing_from_nil() {
        let mut plugins = PluginsMap::new();
        plugins.insert("nil".into(), None);
        let mut stored = StringInterface::new();
        stored.insert("n".into(), serde_json::json!(2));
        plugins.insert("set".into(), Some(stored));

        assert_eq!(
            map_of(&plugin_config_returns(Some(&plugins), "absent")),
            Some(Value::Map(vec![]))
        );
        assert_eq!(
            map_of(&plugin_config_returns(None, "absent")),
            Some(Value::Map(vec![]))
        );
        assert_eq!(map_of(&plugin_config_returns(Some(&plugins), "nil")), None);
        assert_eq!(
            map_of(&plugin_config_returns(Some(&plugins), "set")),
            Some(Value::Map(vec![(
                Value::String("n".into()),
                Value::Interface(Some(Box::new(Interface::float64(2.0)))),
            )]))
        );
    }

    fn args(a: Option<Value>) -> Dynamic {
        returns(
            "Z_SavePluginConfigArgs",
            Type::Map(Arc::new(Type::String), Arc::new(Type::Interface)),
            a,
        )
    }

    #[test]
    fn save_plugin_config_keeps_nil_and_empty_apart() {
        assert_eq!(plugin_config_from_args(&args(None)).unwrap(), Json::Null);
        assert_eq!(
            plugin_config_from_args(&args(Some(Value::Map(vec![])))).unwrap(),
            serde_json::json!({})
        );
        let one = Value::Map(vec![
            (
                Value::String("k".into()),
                Value::Interface(Some(Box::new(Interface::int(7)))),
            ),
            (Value::String("nil".into()), Value::Interface(None)),
            // An empty map an interface holds is a non-nil map once decoded: `{}`, not `null`.
            (
                Value::String("empty".into()),
                Value::Interface(
                    mm_plugin::rpc::json_to_interface(&serde_json::json!({})).map(Box::new),
                ),
            ),
        ]);
        assert_eq!(
            plugin_config_from_args(&args(Some(one))).unwrap(),
            serde_json::json!({ "k": 7, "nil": null, "empty": {} })
        );
    }

    fn manifest(schema: Json) -> Manifest {
        serde_json::from_value(serde_json::json!({ "id": "p", "settings_schema": schema }))
            .expect("a manifest")
    }

    /// Defaults under lower-cased keys, sections after the top level, stored values over both,
    /// and Go's number formatting and escaping — `1e20`, which serde_json writes `1e+20` and Go
    /// in full, is the value that tells the two apart.
    #[test]
    fn load_plugin_configuration_layers_defaults_under_settings() {
        let m = manifest(serde_json::json!({
            "settings": [
                { "key": "Shared", "default": "top" },
                { "key": "Number", "default": 1e21 },
                { "key": "Large", "default": 1e20 },
                { "key": "Tiny", "default": 1e-7 },
                { "key": "NoDefault" }
            ],
            "sections": [{ "key": "s", "settings": [
                { "key": "SHARED", "default": "section" },
                { "key": "Flag", "default": true }
            ]}]
        }));
        let text = |settings: Option<&StringInterface>| {
            String::from_utf8(plugin_configuration(&m, settings)).unwrap()
        };
        assert_eq!(
            text(None),
            r#"{"flag":true,"large":100000000000000000000,"nodefault":null,"number":1e+21,"shared":"section","tiny":1e-7}"#
        );
        let mut stored = StringInterface::new();
        stored.insert("Shared".into(), serde_json::json!("<stored>"));
        stored.insert("Extra".into(), serde_json::json!([1.5, 100]));
        assert_eq!(
            text(Some(&stored)),
            r#"{"extra":[1.5,100],"flag":true,"large":100000000000000000000,"nodefault":null,"number":1e+21,"shared":"\u003cstored\u003e","tiny":1e-7}"#
        );
        assert_eq!(
            String::from_utf8(plugin_configuration(&manifest(Json::Null), None)).unwrap(),
            "{}"
        );
    }

    /// The Go IDL's struct types in package `model`, by name: each field's gob name, its type,
    /// its JSON tag and whether it is embedded.
    fn idl_structs() -> std::collections::HashMap<String, Vec<(String, String, String, bool)>> {
        let idl = fixture("plugin/idl.json");
        let prefix = "github.com/mattermost/mattermost/server/public/model.";
        let mut out = std::collections::HashMap::new();
        for (name, ty) in idl["types"].as_object().expect("types") {
            let Some(short) = name.strip_prefix(prefix) else {
                continue;
            };
            if ty["kind"] != "struct" {
                continue;
            }
            let fields = ty["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    (
                        f["name"].as_str().unwrap_or_default().to_owned(),
                        f["type"].as_str().unwrap_or_default().to_owned(),
                        f["json"].as_str().unwrap_or_default().to_owned(),
                        f["embedded"].as_bool().unwrap_or(false),
                    )
                })
                .collect();
            out.insert(short.to_owned(), fields);
        }
        out
    }

    /// The two facts the walk rests on, read from the Go IDL over every struct reachable from
    /// `model.Config`: no `json:` tag renames a field, and [`EMBEDDED`] is the whole list of
    /// embedded fields.
    #[test]
    fn the_idl_confirms_json_names_are_gob_names_under_config() {
        let structs = idl_structs();
        let prefix = "github.com/mattermost/mattermost/server/public/model.";
        let mut seen = std::collections::HashSet::new();
        let mut queue = vec!["Config".to_owned()];
        let mut embedded = Vec::new();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            for (field, ty, json, is_embedded) in &structs[&name] {
                assert!(
                    !ty.ends_with("[]uint8") || ty == "encoding/json.RawMessage",
                    "{name}.{field} is a []byte that is not a json.RawMessage"
                );
                let tag_name = json.split(',').next().unwrap_or_default();
                assert!(
                    tag_name.is_empty() || tag_name == field,
                    "{name}.{field} is tagged {json:?}"
                );
                if *is_embedded {
                    embedded.push((name.clone(), field.clone()));
                }
                let mut referenced = ty.as_str();
                for token in ["*", "[]", "map[string]"] {
                    referenced = referenced.trim_start_matches(token);
                }
                for part in referenced.split(']') {
                    let part = part.trim_start_matches(['*', '[']).trim_start_matches("[]");
                    if let Some(short) = part.strip_prefix(prefix) {
                        if structs.contains_key(short) {
                            queue.push(short.to_owned());
                        }
                    }
                }
            }
        }
        assert!(seen.len() > 50, "walked {} structs", seen.len());
        let expected: Vec<(String, String)> = EMBEDDED
            .iter()
            .map(|(s, f)| ((*s).to_owned(), (*f).to_owned()))
            .collect();
        assert_eq!(embedded, expected);
    }

    /// Renders a gob value with the IDL's JSON names, so it can be compared with Go's JSON.
    fn as_json(
        structs: &std::collections::HashMap<String, Vec<(String, String, String, bool)>>,
        ty: &Type,
        value: &Value,
    ) -> Json {
        match (ty, value) {
            (Type::Struct(st), Value::Struct(fields)) => {
                let idl = &structs[&st.name];
                let mut out = Map::new();
                for ((name, fty), field) in st.fields.iter().zip(fields) {
                    let json = idl
                        .iter()
                        .find(|(n, ..)| n == name)
                        .map(|(_, _, j, _)| j.split(',').next().unwrap_or_default().to_owned())
                        .unwrap_or_default();
                    let key = if json.is_empty() { name.clone() } else { json };
                    let rendered = match field {
                        Some(v) => as_json(structs, fty, v),
                        None => Json::Null,
                    };
                    out.insert(key, rendered);
                }
                Json::Object(out)
            }
            (_, Value::Bool(b)) => Json::Bool(*b),
            (_, Value::Int(i)) => serde_json::json!(i),
            (_, Value::String(s)) => Json::String(s.clone()),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Go's fully populated `model.License` fixture, through [`license_to_wire`], rendered under
    /// the IDL's JSON names, is the fixture again: every one of the 74 tags maps its own field.
    #[test]
    fn the_licence_maps_every_field_to_its_own() {
        let json = fixture("license.json");
        let license: License = serde_json::from_value(json.clone()).expect("licence");
        let wire = Interface::new("License", &license_to_wire(&license)).expect("encodes");
        let rendered = as_json(&idl_structs(), &wire.ty, &wire.value);
        assert_eq!(rendered, json);
    }

    /// Every boolean of the fixture is `true`, so two swapped flags would still render it. So
    /// each flag is also set alone, the others `false`, and must come back where it went in.
    #[test]
    fn each_licence_flag_maps_to_its_own_field() {
        let structs = idl_structs();
        let fixture = fixture("license.json");
        let one_hot = |object: &Json, key: &str| -> Json {
            let mut out = object.clone();
            for (k, v) in out.as_object_mut().expect("an object") {
                if v.is_boolean() {
                    *v = Json::Bool(k == key);
                }
            }
            out
        };
        let round_trip = |json: &Json| -> Json {
            let license: License = serde_json::from_value(json.clone()).expect("licence");
            let wire = Interface::new("License", &license_to_wire(&license)).expect("encodes");
            as_json(&structs, &wire.ty, &wire.value)
        };
        let flags = |object: &Json| -> Vec<String> {
            object
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, v)| v.is_boolean())
                .map(|(k, _)| k.clone())
                .collect()
        };
        let features = fixture["features"].clone();
        assert!(flags(&features).len() > 30);
        for key in flags(&features) {
            let mut json = fixture.clone();
            json["features"] = one_hot(&features, &key);
            // gob omits a false pointer as it omits a nil one; render both as false.
            let mut rendered = round_trip(&json);
            for v in rendered["features"].as_object_mut().unwrap().values_mut() {
                if v.is_null() {
                    *v = Json::Bool(false);
                }
            }
            assert_eq!(rendered["features"], json["features"], "features.{key}");
        }
        for key in flags(&fixture) {
            let json = one_hot(&fixture, &key);
            let mut rendered = round_trip(&json);
            for (k, v) in rendered.as_object_mut().unwrap() {
                if v.is_null() && json[k].is_boolean() {
                    *v = Json::Bool(false);
                }
            }
            assert_eq!(rendered, json, "{key}");
        }
    }

    /// Field for field, including the three renamed features and a nil struct staying nil.
    #[test]
    fn the_licence_maps_field_for_field() {
        let license: License = serde_json::from_value(fixture("license.json")).expect("licence");
        let wire = license_to_wire(&license);
        let features = license.features.as_ref().expect("features");
        let wire_features = wire.features.as_ref().expect("features");
        assert_eq!(wire_features.google_o_auth, features.google_oauth);
        assert_eq!(wire_features.office365_o_auth, features.office365_oauth);
        assert_eq!(
            wire_features.outgoing_o_auth_connections,
            features.outgoing_oauth_connections
        );
        assert_eq!(
            wire.limits.as_ref().map(|l| l.call_duration_seconds),
            license.limits.as_ref().map(|l| l.call_duration_seconds)
        );
        assert_eq!(
            wire.customer.as_ref().map(|c| c.company.clone()),
            license.customer.as_ref().map(|c| c.company.clone())
        );
        assert_eq!(wire.signup_jwt, license.signup_jwt);
        assert_eq!(wire.extra_users, license.extra_users);
        let bare = license_to_wire(&License::default());
        assert!(bare.customer.is_none() && bare.features.is_none() && bare.limits.is_none());
    }
}
