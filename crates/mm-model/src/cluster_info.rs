//! Port of `model/cluster_info.go` — whole file, six strings and no methods.
//!
//! # `ipaddress` is one word
//!
//! Five tags are unremarkable; `IPAddress` is tagged `ipaddress`, not `ip_address`. That is the
//! only thing in this file that can drift, which is precisely why it gets a fixture rather than a
//! glance.

use serde::{Deserialize, Serialize};

/// Port of `model.ClusterInfo` (cluster_info.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterInfo {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "version")]
    pub version: String,

    #[serde(rename = "schema_version")]
    pub schema_version: String,

    #[serde(rename = "config_hash")]
    pub config_hash: String,

    /// `ipaddress` — one word, unlike `schema_version` and `config_hash` beside it.
    #[serde(rename = "ipaddress")]
    pub ip_address: String,

    #[serde(rename = "hostname")]
    pub hostname: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_parity_with_the_fixture() {
        let raw = include_str!("../../../fixtures/cluster_info.json");
        let info: ClusterInfo = serde_json::from_str(raw).unwrap();
        let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&info).unwrap(), expected);
    }

    /// No `omitempty`, so the zero value is six keys.
    #[test]
    fn the_zero_value_is_six_keys_in_gos_order() {
        assert_eq!(
            serde_json::to_string(&ClusterInfo::default()).unwrap(),
            r#"{"id":"","version":"","schema_version":"","config_hash":"","ipaddress":"","hostname":""}"#
        );
    }

    /// The one tag that can drift.
    #[test]
    fn the_ip_address_tag_is_one_word() {
        let doc = serde_json::to_string(&ClusterInfo::default()).unwrap();
        assert!(doc.contains(r#""ipaddress""#));
        assert!(!doc.contains(r#""ip_address""#), "Go tags this as one word");
    }
}
