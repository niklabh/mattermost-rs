//! Port of `model/metrics.go` — client performance telemetry.
//!
//! # Labels are normalised against allow-lists, never rejected
//!
//! `processLabel` lower-cases the value and, if it is not in the accepted set, silently
//! substitutes a default — `other` for platform and agent, but **`Login`** for the network
//! request group, which is neither a neutral value nor lower-case. An unknown label is therefore
//! indistinguishable from a real one in the metrics, by design.

use serde::{Deserialize, Serialize};

use crate::manifest::StrictVersion;
use crate::serde_helpers::is_empty_map;
use crate::utils::{StringMap, get_millis, go_to_lower};

/// Port of `model.MetricType` (metrics.go:11) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricType(pub String);

impl MetricType {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for MetricType {
    fn from(s: &str) -> Self {
        MetricType(s.to_string())
    }
}

// Web client metrics. The values are the wire form; several are acronyms and stay upper-case.
pub const CLIENT_TIME_TO_FIRST_BYTE: &str = "TTFB";
pub const CLIENT_TIME_TO_LAST_BYTE: &str = "TTLB";
pub const CLIENT_TIME_TO_DOM_INTERACTIVE: &str = "dom_interactive";
pub const CLIENT_SPLASH_SCREEN_END: &str = "splash_screen";
pub const CLIENT_FIRST_CONTENTFUL_PAINT: &str = "FCP";
pub const CLIENT_LARGEST_CONTENTFUL_PAINT: &str = "LCP";
pub const CLIENT_INTERACTION_TO_NEXT_PAINT: &str = "INP";
pub const CLIENT_CUMULATIVE_LAYOUT_SHIFT: &str = "CLS";
pub const CLIENT_LONG_TASKS: &str = "long_tasks";
pub const CLIENT_PAGE_LOAD_DURATION: &str = "page_load";
pub const CLIENT_CHANNEL_SWITCH_DURATION: &str = "channel_switch";
pub const CLIENT_TEAM_SWITCH_DURATION: &str = "team_switch";
pub const CLIENT_RHS_LOAD_DURATION: &str = "rhs_load";
pub const CLIENT_GLOBAL_THREADS_LOAD_DURATION: &str = "global_threads_load";

// Mobile client metrics.
pub const MOBILE_CLIENT_LOAD_DURATION: &str = "mobile_load";
pub const MOBILE_CLIENT_CHANNEL_SWITCH_DURATION: &str = "mobile_channel_switch";
pub const MOBILE_CLIENT_TEAM_SWITCH_DURATION: &str = "mobile_team_switch";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_AVERAGE_SPEED: &str =
    "mobile_network_requests_average_speed";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_EFFECTIVE_LATENCY: &str =
    "mobile_network_requests_effective_latency";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_ELAPSED_TIME: &str =
    "mobile_network_requests_elapsed_time";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_LATENCY: &str = "mobile_network_requests_latency";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_TOTAL_COMPRESSED_SIZE: &str =
    "mobile_network_requests_total_compressed_size";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_TOTAL_PARALLEL_REQUESTS: &str =
    "mobile_network_requests_total_parallel_requests";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_TOTAL_REQUESTS: &str =
    "mobile_network_requests_total_requests";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_TOTAL_SEQUENTIAL_REQUESTS: &str =
    "mobile_network_requests_total_sequential_requests";
pub const MOBILE_CLIENT_NETWORK_REQUESTS_TOTAL_SIZE: &str = "mobile_network_requests_total_size";

// Desktop client metrics.
pub const DESKTOP_CLIENT_CPU_USAGE: &str = "desktop_cpu";
pub const DESKTOP_CLIENT_MEMORY_USAGE: &str = "desktop_memory";

/// Plugin web-app performance metrics.
pub const PLUGIN_WEBAPP_PERF: &str = "plugin_webapp_perf";

/// Port of `performanceReportTTLMilliseconds` (metrics.go:52) — five minutes. A report whose
/// `end` is older than this is rejected.
pub const PERFORMANCE_REPORT_TTL_MILLISECONDS: i64 = 300 * 1000;

/// Port of `performanceReportVersion` (metrics.go:55) — the schema the server understands.
pub const PERFORMANCE_REPORT_VERSION: &str = "0.1.0";

/// Port of `acceptedPlatforms` (metrics.go:56).
pub const ACCEPTED_PLATFORMS: [&str; 6] = ["linux", "macos", "ios", "android", "windows", "other"];
/// Port of `acceptedAgents` (metrics.go:57).
pub const ACCEPTED_AGENTS: [&str; 6] = ["desktop", "firefox", "chrome", "safari", "edge", "other"];
/// Port of `model.AcceptedInteractions` (metrics.go:59).
pub const ACCEPTED_INTERACTIONS: [&str; 3] = ["keyboard", "pointer", "other"];
/// Port of `model.AcceptedLCPRegions` (metrics.go:60).
pub const ACCEPTED_LCP_REGIONS: [&str; 10] = [
    "post",
    "post_textbox",
    "channel_sidebar",
    "team_sidebar",
    "channel_header",
    "global_header",
    "announcement_bar",
    "center_channel",
    "modal_content",
    "other",
];
/// Port of `model.AcceptedTrueFalseLabels` (metrics.go:72).
pub const ACCEPTED_TRUE_FALSE_LABELS: [&str; 2] = ["true", "false"];
/// Port of `model.AcceptedSplashScreenOrigins` (metrics.go:73).
pub const ACCEPTED_SPLASH_SCREEN_ORIGINS: [&str; 2] = ["root", "team_controller"];

/// Port of `model.AcceptedNetworkRequestGroups` (metrics.go:74).
///
/// **These are the only accepted values that are not lower-case** — and `processLabel`
/// lower-cases the incoming value before comparing, so *no* incoming value can ever match one of
/// them and every network-request-group label falls back to the default `Login`. That is Go's
/// behaviour as written; it is reproduced rather than repaired.
pub const ACCEPTED_NETWORK_REQUEST_GROUPS: [&str; 12] = [
    "Cold Start",
    "Cold Start Deferred",
    "DeepLink",
    "DeepLink Deferred",
    "Login",
    "Login Deferred",
    "Notification",
    "Notification Deferred",
    "Server Switch",
    "Server Switch Deferred",
    "WebSocket Reconnect",
    "WebSocket Reconnect Deferred",
];

/// Port of `processLabel` (metrics.go:148).
///
/// Missing key → default. Present → lower-cased, then checked against the accepted set; a miss is
/// the default again. `strings.ToLower` is [`go_to_lower`], not `str::to_lowercase`.
pub fn process_label(
    labels: Option<&StringMap>,
    name: &str,
    accepted_values: &[&str],
    default_value: &str,
) -> String {
    let Some(value) = labels.and_then(|l| l.get(name)) else {
        return default_value.to_string();
    };

    let value = go_to_lower(value);

    if accepted_values.contains(&value.as_str()) {
        value
    } else {
        default_value.to_string()
    }
}

/// Port of `model.MetricSample` (metrics.go:89).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricSample {
    #[serde(rename = "metric")]
    pub metric: MetricType,

    #[serde(rename = "value")]
    pub value: f64,

    #[serde(rename = "labels", skip_serializing_if = "is_empty_map")]
    pub labels: StringMap,
}

impl MetricSample {
    /// Port of `(*MetricSample).GetLabelValue` (metrics.go:95).
    pub fn get_label_value(
        &self,
        name: &str,
        accepted_values: &[&str],
        default_value: &str,
    ) -> String {
        process_label(Some(&self.labels), name, accepted_values, default_value)
    }
}

/// Port of `model.PerformanceReport` (metrics.go:100) — a batch of samples from one client.
///
/// `Start` and `End` are **`float64` epoch milliseconds**, not `i64` — the browser's
/// `performance.now()` origin is fractional, and the type is what the validity window is measured
/// against.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceReport {
    #[serde(rename = "version")]
    pub version: String,

    #[serde(rename = "client_id")]
    pub client_id: String,

    #[serde(rename = "labels")]
    pub labels: Option<StringMap>,

    #[serde(rename = "start")]
    pub start: f64,

    #[serde(rename = "end")]
    pub end: f64,

    #[serde(rename = "counters")]
    pub counters: Option<Vec<MetricSample>>,

    #[serde(rename = "histograms")]
    pub histograms: Option<Vec<MetricSample>>,
}

impl PerformanceReport {
    /// Port of `(*PerformanceReport).IsValid` (metrics.go:111).
    ///
    /// The version rule is **not** "same version": the major must match and the report's minor
    /// must be **≤** the server's, so an older client is accepted and a newer one is not. The
    /// version is parsed leniently (`semver.NewVersion`), so `0.1` and `v0.1.0` are accepted.
    pub fn is_valid(&self) -> Result<(), MetricsError> {
        let Some(report_version) = StrictVersion::parse_lenient(&self.version) else {
            return Err(MetricsError::UnparseableVersion(self.version.clone()));
        };

        let Some(server_version) = StrictVersion::parse(PERFORMANCE_REPORT_VERSION) else {
            return Err(MetricsError::UnparseableVersion(
                PERFORMANCE_REPORT_VERSION.to_string(),
            ));
        };

        if report_version.major != server_version.major
            || report_version.minor > server_version.minor
        {
            return Err(MetricsError::UnsupportedVersion {
                server: PERFORMANCE_REPORT_VERSION.to_string(),
                report: self.version.clone(),
            });
        }

        if self.start > self.end {
            return Err(MetricsError::TimestampsReversed {
                start: self.start,
                end: self.end,
            });
        }

        let now = get_millis();
        if self.end < (now - PERFORMANCE_REPORT_TTL_MILLISECONDS) as f64 {
            return Err(MetricsError::Outdated {
                end: self.end,
                ttl: PERFORMANCE_REPORT_TTL_MILLISECONDS,
            });
        }

        Ok(())
    }

    /// Port of `(*PerformanceReport).ProcessLabels` (metrics.go:137) — the four labels attached
    /// to every sample in the report.
    ///
    /// `desktop_app_version` is passed through **raw**, with no allow-list and no lower-casing —
    /// the only unvalidated label, and therefore the only one with unbounded cardinality.
    pub fn process_labels(&self) -> StringMap {
        let mut out = StringMap::new();
        out.insert(
            "platform".to_string(),
            process_label(
                self.labels.as_ref(),
                "platform",
                &ACCEPTED_PLATFORMS,
                "other",
            ),
        );
        out.insert(
            "agent".to_string(),
            process_label(self.labels.as_ref(), "agent", &ACCEPTED_AGENTS, "other"),
        );
        out.insert(
            "desktop_app_version".to_string(),
            self.labels
                .as_ref()
                .and_then(|l| l.get("desktop_app_version"))
                .cloned()
                .unwrap_or_default(),
        );
        out.insert(
            "network_request_group".to_string(),
            process_label(
                self.labels.as_ref(),
                "network_request_group",
                &ACCEPTED_NETWORK_REQUEST_GROUPS,
                "Login",
            ),
        );
        out
    }
}

/// The errors `metrics.go` returns. Go builds them with `fmt.Errorf`; `%f` renders six decimal
/// places, which `{:.6}` reproduces.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MetricsError {
    #[error("could not parse semver version: {0}")]
    UnparseableVersion(String),
    #[error("report version is not supported: server version: {server}, report version: {report}")]
    UnsupportedVersion { server: String, report: String },
    #[error(
        "report timestamps are erroneous: start_timestamp {start:.6} is greater than end_timestamp {end:.6}"
    )]
    TimestampsReversed { start: f64, end: f64 },
    #[error("report is outdated: end_time {end:.6} is past {ttl} ms from now")]
    Outdated { end: f64, ttl: i64 },
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
    fn metric_sample_round_trips_the_fixture() {
        assert_fixture_round_trips!(MetricSample, "metric_sample");
    }
    #[test]
    fn performance_report_round_trips_the_fixture() {
        assert_fixture_round_trips!(PerformanceReport, "performance_report");
    }
}
