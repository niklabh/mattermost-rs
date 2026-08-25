//! Port of `model/packet_metadata.go` — the `metadata.yaml` inside a Support Packet.
//!
//! # This file is YAML-only
//!
//! Every tag is a `yaml:` tag; there is not one `json:` tag. The struct is serialised into the
//! archive customers send to Mattermost staff, so the key names are the wire format even though
//! no HTTP route carries them. `ParsePacketMetadata` — which sniffs the `version` key, then
//! decodes and validates — needs a YAML parser this crate does not have, and is deferred with
//! `manifest.go`'s `FindManifest`.

use crate::license::License;
use crate::manifest::StrictVersion;
use crate::utils::{CURRENT_VERSION, get_millis, go_quote, is_valid_id};

/// Port of `model.CurrentMetadataVersion` (packet_metadata.go:15).
pub const CURRENT_METADATA_VERSION: i64 = 1;
/// Port of `model.PacketMetadataFileName` (packet_metadata.go:19).
pub const PACKET_METADATA_FILE_NAME: &str = "metadata.yaml";

/// Port of `model.PacketType` (packet_metadata.go:11) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketType(pub String);

impl PacketType {
    pub const SUPPORT_PACKET: &'static str = "support-packet";
    pub const PLUGIN_PACKET: &'static str = "plugin-packet";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PacketType {
    fn from(s: &str) -> Self {
        PacketType(s.to_string())
    }
}

impl std::fmt::Display for PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Port of `model.PacketMetadata` (packet_metadata.go:24).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PacketMetadata {
    /// `yaml:"version"`. Must be ≥ 1.
    pub version: i64,
    /// `yaml:"type"`.
    pub type_: PacketType,
    /// `yaml:"generated_at"` — epoch milliseconds.
    pub generated_at: i64,
    /// `yaml:"server_version"`.
    pub server_version: String,
    /// `yaml:"server_id"` — the telemetry id, which is a 26-character id.
    pub server_id: String,

    /// `yaml:"license_id"`. Optional.
    pub license_id: String,
    /// `yaml:"customer_id"`. Optional.
    pub customer_id: String,
    /// `yaml:"extras,omitempty"` — the only field that is omitted when empty.
    pub extras: Option<crate::utils::StringInterface>,
}

impl PacketMetadata {
    /// Port of `(*PacketMetadata).Validate` (packet_metadata.go:41).
    ///
    /// Note the version message says "should be greater than 1" while the check is `< 1`, so
    /// version 1 — the only version that exists — passes despite what the text says.
    pub fn validate(&self) -> Result<(), PacketMetadataError> {
        if self.version < 1 {
            return Err(PacketMetadataError::BadVersion);
        }

        match self.type_.as_str() {
            PacketType::SUPPORT_PACKET | PacketType::PLUGIN_PACKET => {}
            other => return Err(PacketMetadataError::UnknownType(other.to_string())),
        }

        if self.generated_at <= 0 {
            return Err(PacketMetadataError::BadGeneratedAt);
        }

        if StrictVersion::parse_lenient(&self.server_version).is_none() {
            return Err(PacketMetadataError::BadServerVersion(
                self.server_version.clone(),
            ));
        }

        if !is_valid_id(&self.server_id) {
            return Err(PacketMetadataError::BadServerId(self.server_id.clone()));
        }

        if !self.license_id.is_empty() && !is_valid_id(&self.license_id) {
            return Err(PacketMetadataError::BadLicenseId(self.license_id.clone()));
        }

        if !self.customer_id.is_empty() && !is_valid_id(&self.customer_id) {
            return Err(PacketMetadataError::BadCustomerId(self.customer_id.clone()));
        }

        Ok(())
    }
}

/// Port of `model.GeneratePacketMetadata` (packet_metadata.go:104).
///
/// **Go dereferences `license.Customer` unguarded**, so a licence with no customer panics. Here
/// a missing customer leaves `customer_id` empty, which is the safe direction; every licence Go
/// survives produces the same metadata.
pub fn generate_packet_metadata(
    packet_type: PacketType,
    telemetry_id: &str,
    license: Option<&License>,
    extra: Option<crate::utils::StringInterface>,
) -> Result<PacketMetadata, PacketMetadataError> {
    let mut md = PacketMetadata {
        version: CURRENT_METADATA_VERSION,
        type_: packet_type,
        generated_at: get_millis(),
        server_version: CURRENT_VERSION.to_string(),
        server_id: telemetry_id.to_string(),
        // Go replaces a nil map with an empty one, so `extras` is always present.
        extras: Some(extra.unwrap_or_default()),
        ..Default::default()
    };

    if let Some(license) = license {
        md.license_id = license.id.clone();
        if let Some(customer) = &license.customer {
            md.customer_id = customer.id.clone();
        }
    }

    md.validate()?;

    Ok(md)
}

/// The errors `packet_metadata.go` returns, with Go's message text. `%q` is [`go_quote`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketMetadataError {
    #[error("metadata version should be greater than 1")]
    BadVersion,
    #[error("unrecognized packet type: {0}")]
    UnknownType(String),
    #[error("generated_at should be a positive number")]
    BadGeneratedAt,
    #[error("could not parse server version: {0}")]
    BadServerVersion(String),
    #[error("server id is not a valid id {}", go_quote(.0))]
    BadServerId(String),
    #[error("license id is not a valid id {}", go_quote(.0))]
    BadLicenseId(String),
    #[error("customer id is not a valid id {}", go_quote(.0))]
    BadCustomerId(String),
}
