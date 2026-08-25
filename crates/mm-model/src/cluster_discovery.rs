//! Port of `model/cluster_discovery.go` — the row a node writes so its peers can find it.
//!
//! # `Port` is dead but still on the wire
//!
//! `Port` is documented in Go as "Deperacted: Port is unused. It's only kept for backwards
//! compatibility" (typo theirs). It has no `omitempty`, so it is always emitted as `0`. Dropping
//! it would be a wire break.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id};

/// Port of `model.CDSOfflineAfterMillis` (cluster_discovery.go:12) — 30 minutes.
pub const CDS_OFFLINE_AFTER_MILLIS: i64 = 1000 * 60 * 30;
/// Port of `model.CDSTypeApp` (cluster_discovery.go:13).
pub const CDS_TYPE_APP: &str = "mattermost_app";

/// Port of `model.ClusterDiscovery` (cluster_discovery.go:16).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterDiscovery {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "cluster_name")]
    pub cluster_name: String,

    /// Despite the name this holds an **IP address** whenever `AutoFillIPAddress` ran — see
    /// [`ClusterDiscovery::auto_fill_ip_address`].
    #[serde(rename = "hostname")]
    pub hostname: String,

    #[serde(rename = "gossip_port")]
    pub gossip_port: i32,

    /// Deprecated and unused; kept because removing it changes the wire format.
    #[serde(rename = "port")]
    pub port: i32,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    /// Epoch milliseconds.
    #[serde(rename = "last_ping_at")]
    pub last_ping_at: i64,
}

impl ClusterDiscovery {
    /// Port of `(*ClusterDiscovery).PreSave` (cluster_discovery.go:27).
    ///
    /// `last_ping_at` is set **only** inside the `create_at == 0` branch, so a row that already
    /// has a creation time keeps whatever ping time it arrived with.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
            self.last_ping_at = self.create_at;
        }
    }

    /// Port of `(*ClusterDiscovery).AutoFillIPAddress` (cluster_discovery.go:46).
    ///
    /// **Divergence:** Go falls back to `GetServerIPAddress(iface)`, which walks the host's
    /// network interfaces. That is not a wire format and `mm-model` does no I/O, so the caller
    /// resolves the address and passes it in; an empty `ip_address` leaves `hostname` untouched
    /// rather than probing. `AutoFillHostname` — `os.Hostname()` — is deferred for the same
    /// reason.
    pub fn auto_fill_ip_address(&mut self, ip_address: &str) {
        if self.hostname.is_empty() && !ip_address.is_empty() {
            self.hostname = ip_address.to_string();
        }
    }

    /// Port of `(*ClusterDiscovery).IsEqual` (cluster_discovery.go:57).
    ///
    /// Compares **three** fields only — type, cluster name, hostname. Not id, not the ports, not
    /// the timestamps: this answers "is this the same node", not "is this the same row".
    pub fn is_equal(&self, other: &ClusterDiscovery) -> bool {
        self.type_ == other.type_
            && self.cluster_name == other.cluster_name
            && self.hostname == other.hostname
    }

    /// Port of `(*ClusterDiscovery).IsValid` (cluster_discovery.go:87).
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id"));
        }

        if self.cluster_name.is_empty() {
            return Err(err("name"));
        }

        if self.type_.is_empty() {
            return Err(err("type"));
        }

        if self.hostname.is_empty() {
            return Err(err("hostname"));
        }

        if self.create_at == 0 {
            return Err(err("create_at"));
        }

        if self.last_ping_at == 0 {
            return Err(err("last_ping_at"));
        }

        Ok(())
    }
}

/// The error ids are `model.cluster.is_valid.<field>.app_error` — note **`cluster`**, not
/// `cluster_discovery`, and note that the `ClusterName` branch reports `name`.
fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "ClusterDiscovery.IsValid",
        format!("model.cluster.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.FilterClusterDiscovery` (cluster_discovery.go:75).
///
/// Go returns a non-nil empty slice when nothing matches, which marshals as `[]` rather than
/// `null`; `Vec::new()` has the same property here.
pub fn filter_cluster_discovery(
    vs: &[ClusterDiscovery],
    f: impl Fn(&ClusterDiscovery) -> bool,
) -> Vec<ClusterDiscovery> {
    vs.iter().filter(|v| f(v)).cloned().collect()
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
    fn cluster_discovery_round_trips_the_fixture() {
        assert_fixture_round_trips!(ClusterDiscovery, "cluster_discovery");
    }
}
