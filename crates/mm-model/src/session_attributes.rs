//! Port of `model/session_attributes.go` — the built-in session-attribute schema that ABAC
//! policies evaluate against a live session.
//!
//! Session attributes are property fields with `object_type = session`, all system-scoped and
//! sysadmin-gated. Each carries a TTL and a grace period in its `attrs`, in **seconds**, grouped
//! into three tiers: network identity (15s), posture (60s) and identity (300s).
//!
//! # `SAField` shadows `attrs` exactly like `CPAField`
//!
//! Same trick, same reason, same hand-written codec — see `custom_profile_attributes.rs`. The
//! typed shape here is [`SAAttrs`].
//!
//! # Two Go quirks preserved in the schema builders
//!
//! - `sessionAttributeFieldAttrs` sets `enabled: false` on **every** built-in field, so the whole
//!   schema ships switched off and an admin turns fields on individually.
//! - `sessionAttributeField` leaves `ID` empty — unlike `native_attributes.go`, which mints a
//!   synthetic one. These fields *are* persisted, so the store assigns the id.

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::native_attributes::NATIVE_ATTRIBUTE_ATTR_OPERATORS;
use crate::property_field::{
    PROPERTY_FIELD_ATTRIBUTE_OPTIONS, PROPERTY_FIELD_OBJECT_TYPE_SESSION,
    PROPERTY_FIELD_TARGET_LEVEL_SYSTEM, PermissionLevel, PluginPropertyOption, PropertyField,
    PropertyFieldType, PropertyOption, PropertyOptions,
};
use crate::serde_helpers::is_empty_str;
use crate::utils::StringInterface;

/// Port of `model.SessionAttributesPropertyGroupName` (session_attributes.go:13).
pub const SESSION_ATTRIBUTES_PROPERTY_GROUP_NAME: &str = "session_attributes";

pub const SESSION_ATTRIBUTE_PLATFORM_DESKTOP: &str = "desktop";
pub const SESSION_ATTRIBUTE_PLATFORM_MOBILE: &str = "mobile";
pub const SESSION_ATTRIBUTE_PLATFORM_BROWSER: &str = "browser";

pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_IP_ADDRESS: &str = "client_ip_address";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_NETWORK_INTERFACE_TYPE: &str = "network_interface_type";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_VPN_ACTIVE: &str = "vpn_active";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_SSID: &str = "ssid";
/// The constant is `TLSDDeviceID` (two Ds); the value is `tls_device_id`.
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_TLSD_DEVICE_ID: &str = "tls_device_id";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_DEVICE_ID: &str = "client_device_id";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_MDM_ENROLLED: &str = "mdm_enrolled";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_HARDWARE_ID: &str = "hardware_id";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_OS_PLATFORM: &str = "os_platform";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_OS_VERSION: &str = "os_version";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_VERSION: &str = "client_version";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_JAILBREAK_DETECTED: &str = "jailbreak_detected";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_SERVER_FQDN: &str = "server_fqdn";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_FQDN: &str = "client_fqdn";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_PLATFORM: &str = "user_agent_platform";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_OS: &str = "user_agent_os";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_NAME: &str =
    "user_agent_browser_name";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_VERSION: &str =
    "user_agent_browser_version";
pub const SESSION_ATTRIBUTES_PROPERTY_FIELD_IP_ADDRESS: &str = "ip_address";

pub const SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_IP_ADDRESS: &str = "Client IP address";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_NETWORK_INTERFACE_TYPE: &str = "Network interface type";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_VPN_ACTIVE: &str = "VPN active";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_SSID: &str = "SSID";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_TLSD_DEVICE_ID: &str = "TLS device ID";
/// The field is `client_device_id`; its label is just **`Device ID`**.
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_DEVICE_ID: &str = "Device ID";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_MDM_ENROLLED: &str = "MDM enrolled";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_HARDWARE_ID: &str = "Hardware ID";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_OS_PLATFORM: &str = "OS platform";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_OS_VERSION: &str = "OS version";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_VERSION: &str = "Client version";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_JAILBREAK_DETECTED: &str = "Jailbreak detected";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_SERVER_FQDN: &str = "Server FQDN";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_FQDN: &str = "Client FQDN";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_PLATFORM: &str = "User agent platform";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_OS: &str = "User agent OS";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_BROWSER_NAME: &str = "User agent browser name";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_BROWSER_VERSION: &str =
    "User agent browser version";
pub const SESSION_ATTRIBUTES_DISPLAY_NAME_IP_ADDRESS: &str = "IP address";

pub const SA_ATTR_ENABLED: &str = "enabled";
pub const SA_ATTR_PLATFORMS: &str = "platforms";
pub const SA_ATTR_TTL_SECONDS: &str = "ttl_seconds";
pub const SA_ATTR_GRACE_PERIOD_SECONDS: &str = "grace_period_seconds";
pub const SA_ATTR_DISPLAY_NAME: &str = "display_name";

/// Seconds. Network-identity attributes go stale fastest.
pub const SESSION_ATTRIBUTE_DEFAULT_TTL_NETWORK_IDENTITY: i64 = 15;
pub const SESSION_ATTRIBUTE_DEFAULT_TTL_POSTURE: i64 = 60;
pub const SESSION_ATTRIBUTE_DEFAULT_TTL_IDENTITY: i64 = 300;

/// The grace periods mirror the TTLs one for one.
pub const SESSION_ATTRIBUTE_DEFAULT_GRACE_NETWORK_IDENTITY: i64 = 15;
pub const SESSION_ATTRIBUTE_DEFAULT_GRACE_POSTURE: i64 = 60;
pub const SESSION_ATTRIBUTE_DEFAULT_GRACE_IDENTITY: i64 = 300;

/// The header a client sends its own attributes in.
pub const SESSION_ATTRIBUTE_HEADER_CLIENT_ATTRIBUTES: &str = "X-MM-Session-Attributes";
/// The header a TLS-terminating proxy injects the device id in. Note the **different prefix**:
/// `X-Mattermost-…`, not `X-MM-…`.
pub const SESSION_ATTRIBUTE_HEADER_PROXY_DEVICE_ID: &str =
    "X-Mattermost-Session-Attribute-Device-Id";

pub const SESSION_OPERATOR_IN_CIDR: &str = "inCIDR";
pub const SESSION_OPERATOR_VERSION_EQ: &str = "versionEQ";
pub const SESSION_OPERATOR_VERSION_GT: &str = "versionGT";
pub const SESSION_OPERATOR_VERSION_GTE: &str = "versionGTE";
pub const SESSION_OPERATOR_VERSION_LT: &str = "versionLT";
pub const SESSION_OPERATOR_VERSION_LTE: &str = "versionLTE";

/// Port of `sessionStringOperators` (session_attributes.go:99).
pub const SESSION_STRING_OPERATORS: [&str; 6] =
    ["==", "!=", "in", "startsWith", "endsWith", "contains"];

/// Port of `sessionVersionOperators` (session_attributes.go:101).
pub const SESSION_VERSION_OPERATORS: [&str; 5] = [
    SESSION_OPERATOR_VERSION_EQ,
    SESSION_OPERATOR_VERSION_GT,
    SESSION_OPERATOR_VERSION_GTE,
    SESSION_OPERATOR_VERSION_LT,
    SESSION_OPERATOR_VERSION_LTE,
];

/// Port of `sessionOperators` (session_attributes.go:109) — the six string operators plus any
/// extras, under the **`operators`** key that `native_attributes.go` also uses.
fn session_operators(extra: &[&str]) -> StringInterface {
    let mut list: Vec<serde_json::Value> = SESSION_STRING_OPERATORS
        .iter()
        .map(|o| serde_json::Value::String((*o).to_string()))
        .collect();
    list.extend(
        extra
            .iter()
            .map(|o| serde_json::Value::String((*o).to_string())),
    );

    let mut attrs = StringInterface::new();
    attrs.insert(
        NATIVE_ATTRIBUTE_ATTR_OPERATORS.to_string(),
        serde_json::Value::Array(list),
    );
    attrs
}

/// Port of `model.SessionAttributesRequestDerivedFieldNames` (session_attributes.go:115) — the
/// five attributes the **server** derives from the request rather than trusting the client for.
pub const SESSION_ATTRIBUTES_REQUEST_DERIVED_FIELD_NAMES: [&str; 5] = [
    SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_PLATFORM,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_OS,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_NAME,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_VERSION,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_IP_ADDRESS,
];

/// Port of `model.SessionAttributesDeviceIDFieldNames` (session_attributes.go:123).
pub const SESSION_ATTRIBUTES_DEVICE_ID_FIELD_NAMES: [&str; 3] = [
    SESSION_ATTRIBUTES_PROPERTY_FIELD_TLSD_DEVICE_ID,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_DEVICE_ID,
    SESSION_ATTRIBUTES_PROPERTY_FIELD_HARDWARE_ID,
];

/// Port of `model.SessionAttributesClusterPayload` (session_attributes.go:129) — what one node
/// tells the others when a session's attributes change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAttributesClusterPayload {
    #[serde(rename = "session_id")]
    pub session_id: String,

    #[serde(rename = "attrs")]
    pub attrs: Option<StringInterface>,

    /// Epoch milliseconds.
    #[serde(rename = "timestamp")]
    pub timestamp: i64,
}

/// Port of `model.SAAttrs` (session_attributes.go:140).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SAAttrs {
    /// Every built-in field ships with this **false** — see the module docs.
    #[serde(rename = "enabled")]
    pub enabled: bool,

    /// Which client platforms may report this attribute.
    #[serde(rename = "platforms")]
    pub platforms: Option<Vec<String>>,

    #[serde(rename = "ttl_seconds")]
    pub ttl_seconds: i64,

    #[serde(rename = "grace_period_seconds")]
    pub grace_period_seconds: i64,

    #[serde(rename = "display_name", skip_serializing_if = "is_empty_str")]
    pub display_name: String,
}

/// Port of `model.SAField` (session_attributes.go:135) — a `PropertyField` with typed `attrs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SAField {
    pub property_field: PropertyField,
    pub attrs: SAAttrs,
}

impl Serialize for SAField {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut object = match serde_json::to_value(&self.property_field) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return Err(serde::ser::Error::custom("PropertyField is not an object")),
        };
        let typed = serde_json::to_value(&self.attrs).map_err(serde::ser::Error::custom)?;
        object.insert("attrs".to_string(), typed);
        object.serialize(s)
    }
}

impl<'de> Deserialize<'de> for SAField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        let attrs: SAAttrs = match value.get("attrs") {
            Some(raw) => serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?,
            None => SAAttrs::default(),
        };
        let property_field: PropertyField =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(SAField {
            property_field,
            attrs,
        })
    }
}

impl SAField {
    /// Port of `model.SAFieldFromPropertyField` (session_attributes.go:159).
    ///
    /// An **empty** attrs map short-circuits to the zero `SAAttrs` without a round trip, which is
    /// why a field with no attrs is not an error.
    pub fn from_property_field(field: &PropertyField) -> Result<Self, SessionAttributeError> {
        let mut sa = SAField {
            property_field: field.clone(),
            attrs: SAAttrs::default(),
        };

        let Some(map) = &field.attrs else {
            return Ok(sa);
        };
        if map.is_empty() {
            return Ok(sa);
        }

        sa.attrs = serde_json::from_value(serde_json::Value::Object(map.clone()))
            .map_err(|e| SessionAttributeError::Attrs(e.to_string()))?;

        Ok(sa)
    }

    /// Port of `(*SAField).EnabledForPlatform` (session_attributes.go:179).
    ///
    /// Both halves matter: the field must be enabled **and** list the platform. Go's nil receiver
    /// returns false; that state is unrepresentable on `&self`.
    pub fn enabled_for_platform(&self, platform: &str) -> bool {
        if !self.attrs.enabled {
            return false;
        }
        self.attrs
            .platforms
            .as_ref()
            .is_some_and(|p| p.iter().any(|x| x == platform))
    }
}

/// Port of `model.SessionAttributeManifestEntry` (session_attributes.go:148) — the schema as
/// advertised to clients so they know what to collect and how often.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAttributeManifestEntry {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "ttl_seconds")]
    pub ttl_seconds: i64,

    #[serde(rename = "grace_period_seconds")]
    pub grace_period_seconds: i64,

    #[serde(rename = "platforms")]
    pub platforms: Option<Vec<String>>,

    #[serde(rename = "display_name", skip_serializing_if = "is_empty_str")]
    pub display_name: String,
}

/// Port of `model.IsValidSessionAttributeValue` (session_attributes.go:187).
///
/// Only two field types are accepted at all — `text` and `select`; everything else, including
/// `multiselect` and `date`, is invalid. A `select` value matches on **either** an option's name
/// or its id, and a field with no `options` attr rejects every value.
pub fn is_valid_session_attribute_value(field: &PropertyField, value: &serde_json::Value) -> bool {
    if value.is_null() {
        return false;
    }

    match field.type_.as_str() {
        PropertyFieldType::TEXT => value.as_str().is_some_and(|s| !s.is_empty()),
        PropertyFieldType::SELECT => {
            let Some(str_value) = value.as_str() else {
                return false;
            };
            if str_value.is_empty() {
                return false;
            }

            let Some(raw_options) = field.get_attr(PROPERTY_FIELD_ATTRIBUTE_OPTIONS) else {
                return false;
            };

            let Ok(options) =
                PropertyOptions::<PluginPropertyOption>::from_field_attrs(raw_options)
            else {
                return false;
            };

            options
                .0
                .iter()
                .any(|option| option.get_name() == str_value || option.get_id() == str_value)
        }
        _ => false,
    }
}

/// Port of `sessionAttributeFieldAttrs` (session_attributes.go:225).
fn session_attribute_field_attrs(platforms: &[&str], ttl: i64, grace: i64) -> StringInterface {
    let mut attrs = StringInterface::new();
    attrs.insert(SA_ATTR_ENABLED.to_string(), serde_json::Value::Bool(false));
    attrs.insert(
        SA_ATTR_PLATFORMS.to_string(),
        serde_json::Value::Array(
            platforms
                .iter()
                .map(|p| serde_json::Value::String((*p).to_string()))
                .collect(),
        ),
    );
    attrs.insert(SA_ATTR_TTL_SECONDS.to_string(), serde_json::json!(ttl));
    attrs.insert(
        SA_ATTR_GRACE_PERIOD_SECONDS.to_string(),
        serde_json::json!(grace),
    );
    attrs
}

/// Port of `sessionAttributeField` (session_attributes.go:234).
#[allow(clippy::too_many_arguments)]
fn session_attribute_field(
    group_id: &str,
    name: &str,
    display_name: &str,
    field_type: PropertyFieldType,
    platforms: &[&str],
    ttl: i64,
    grace: i64,
    extra_attrs: Option<StringInterface>,
) -> PropertyField {
    let mut attrs = session_attribute_field_attrs(platforms, ttl, grace);
    attrs.insert(
        SA_ATTR_DISPLAY_NAME.to_string(),
        serde_json::Value::String(display_name.to_string()),
    );
    if let Some(extra) = extra_attrs {
        for (key, value) in extra {
            attrs.insert(key, value);
        }
    }

    PropertyField {
        group_id: group_id.to_string(),
        name: name.to_string(),
        type_: field_type,
        object_type: PROPERTY_FIELD_OBJECT_TYPE_SESSION.to_string(),
        target_type: PROPERTY_FIELD_TARGET_LEVEL_SYSTEM.to_string(),
        target_id: String::new(),
        permission_field: Some(PermissionLevel::SYSADMIN.into()),
        permission_values: Some(PermissionLevel::SYSADMIN.into()),
        permission_options: Some(PermissionLevel::SYSADMIN.into()),
        attrs: Some(attrs),
        ..Default::default()
    }
}

/// A `select` field's options as Go writes them: a list of `{"name": …}` objects with **no id**.
/// The store mints ids later via `PropertyField::ensure_option_ids`.
fn name_options(names: &[&str]) -> StringInterface {
    let mut attrs = StringInterface::new();
    attrs.insert(
        PROPERTY_FIELD_ATTRIBUTE_OPTIONS.to_string(),
        serde_json::Value::Array(
            names
                .iter()
                .map(|n| serde_json::json!({ "name": *n }))
                .collect(),
        ),
    );
    attrs
}

/// Port of `model.SessionAttributeSystemFields` (session_attributes.go:253) — the 19 built-in
/// fields, in Go's order (network identity, then posture, then identity).
pub fn session_attribute_system_fields(group_id: &str) -> Vec<PropertyField> {
    let all_platforms = [
        SESSION_ATTRIBUTE_PLATFORM_DESKTOP,
        SESSION_ATTRIBUTE_PLATFORM_MOBILE,
        SESSION_ATTRIBUTE_PLATFORM_BROWSER,
    ];
    let clients_only = [
        SESSION_ATTRIBUTE_PLATFORM_DESKTOP,
        SESSION_ATTRIBUTE_PLATFORM_MOBILE,
    ];
    let desktop_browser = [
        SESSION_ATTRIBUTE_PLATFORM_DESKTOP,
        SESSION_ATTRIBUTE_PLATFORM_BROWSER,
    ];
    let desktop_only = [SESSION_ATTRIBUTE_PLATFORM_DESKTOP];
    let mobile_only = [SESSION_ATTRIBUTE_PLATFORM_MOBILE];

    let bool_select_options = || name_options(&["true", "false"]);
    let text = || PropertyFieldType::from(PropertyFieldType::TEXT);
    let select = || PropertyFieldType::from(PropertyFieldType::SELECT);

    let network_ttl = SESSION_ATTRIBUTE_DEFAULT_TTL_NETWORK_IDENTITY;
    let network_grace = SESSION_ATTRIBUTE_DEFAULT_GRACE_NETWORK_IDENTITY;
    let posture_ttl = SESSION_ATTRIBUTE_DEFAULT_TTL_POSTURE;
    let posture_grace = SESSION_ATTRIBUTE_DEFAULT_GRACE_POSTURE;
    let identity_ttl = SESSION_ATTRIBUTE_DEFAULT_TTL_IDENTITY;
    let identity_grace = SESSION_ATTRIBUTE_DEFAULT_GRACE_IDENTITY;

    vec![
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_IP_ADDRESS,
            SESSION_ATTRIBUTES_DISPLAY_NAME_IP_ADDRESS,
            text(),
            &all_platforms,
            network_ttl,
            network_grace,
            Some(session_operators(&[SESSION_OPERATOR_IN_CIDR])),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_IP_ADDRESS,
            SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_IP_ADDRESS,
            text(),
            &clients_only,
            network_ttl,
            network_grace,
            Some(session_operators(&[SESSION_OPERATOR_IN_CIDR])),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_NETWORK_INTERFACE_TYPE,
            SESSION_ATTRIBUTES_DISPLAY_NAME_NETWORK_INTERFACE_TYPE,
            select(),
            &clients_only,
            network_ttl,
            network_grace,
            Some(name_options(&[
                "wifi",
                "ethernet",
                "cellular",
                "vpn",
                "bluetooth",
                "other",
            ])),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_VPN_ACTIVE,
            SESSION_ATTRIBUTES_DISPLAY_NAME_VPN_ACTIVE,
            select(),
            &clients_only,
            network_ttl,
            network_grace,
            Some(bool_select_options()),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_SSID,
            SESSION_ATTRIBUTES_DISPLAY_NAME_SSID,
            text(),
            &clients_only,
            network_ttl,
            network_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_MDM_ENROLLED,
            SESSION_ATTRIBUTES_DISPLAY_NAME_MDM_ENROLLED,
            select(),
            &clients_only,
            posture_ttl,
            posture_grace,
            Some(bool_select_options()),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_JAILBREAK_DETECTED,
            SESSION_ATTRIBUTES_DISPLAY_NAME_JAILBREAK_DETECTED,
            select(),
            &mobile_only,
            posture_ttl,
            posture_grace,
            Some(bool_select_options()),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_OS_PLATFORM,
            SESSION_ATTRIBUTES_DISPLAY_NAME_OS_PLATFORM,
            select(),
            &clients_only,
            posture_ttl,
            posture_grace,
            Some(name_options(&[
                "macos", "windows", "linux", "ios", "android",
            ])),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_OS_VERSION,
            SESSION_ATTRIBUTES_DISPLAY_NAME_OS_VERSION,
            text(),
            &clients_only,
            posture_ttl,
            posture_grace,
            Some(session_operators(&SESSION_VERSION_OPERATORS)),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_VERSION,
            SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_VERSION,
            text(),
            &clients_only,
            posture_ttl,
            posture_grace,
            Some(session_operators(&SESSION_VERSION_OPERATORS)),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_PLATFORM,
            SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_PLATFORM,
            text(),
            &all_platforms,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_OS,
            SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_OS,
            text(),
            &all_platforms,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_NAME,
            SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_BROWSER_NAME,
            text(),
            &all_platforms,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_USER_AGENT_BROWSER_VERSION,
            SESSION_ATTRIBUTES_DISPLAY_NAME_USER_AGENT_BROWSER_VERSION,
            text(),
            &all_platforms,
            identity_ttl,
            identity_grace,
            Some(session_operators(&SESSION_VERSION_OPERATORS)),
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_TLSD_DEVICE_ID,
            SESSION_ATTRIBUTES_DISPLAY_NAME_TLSD_DEVICE_ID,
            text(),
            &desktop_browser,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_DEVICE_ID,
            SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_DEVICE_ID,
            text(),
            &mobile_only,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_HARDWARE_ID,
            SESSION_ATTRIBUTES_DISPLAY_NAME_HARDWARE_ID,
            text(),
            &desktop_only,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_SERVER_FQDN,
            SESSION_ATTRIBUTES_DISPLAY_NAME_SERVER_FQDN,
            text(),
            &clients_only,
            identity_ttl,
            identity_grace,
            None,
        ),
        session_attribute_field(
            group_id,
            SESSION_ATTRIBUTES_PROPERTY_FIELD_CLIENT_FQDN,
            SESSION_ATTRIBUTES_DISPLAY_NAME_CLIENT_FQDN,
            text(),
            &desktop_only,
            identity_ttl,
            identity_grace,
            None,
        ),
    ]
}

/// The one non-`AppError` failure in this file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionAttributeError {
    #[error("{0}")]
    Attrs(String),
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
    fn session_attributes_cluster_payload_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            SessionAttributesClusterPayload,
            "session_attributes_cluster_payload"
        );
    }
    #[test]
    fn sa_field_round_trips_the_fixture() {
        assert_fixture_round_trips!(SAField, "sa_field");
    }
    #[test]
    fn sa_attrs_round_trips_the_fixture() {
        assert_fixture_round_trips!(SAAttrs, "sa_attrs");
    }
    #[test]
    fn session_attribute_manifest_entry_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            SessionAttributeManifestEntry,
            "session_attribute_manifest_entry"
        );
    }
}
