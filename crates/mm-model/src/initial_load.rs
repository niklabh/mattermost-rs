//! Port of `model/initial_load.go` — the legacy bootstrap payload.
//!
//! Seven keys, none with `omitempty`, so an anonymous load still writes `"user":null` and four
//! `null` collections. `Preferences` is the named slice type from `preference.rs`, not a bare
//! `Vec`, which is what keeps its `null`-when-nil behaviour.

use serde::{Deserialize, Serialize};

use crate::preference::Preferences;
use crate::team::Team;
use crate::team_member::TeamMember;
use crate::user::User;
use crate::utils::StringMap;

/// Port of `model.InitialLoad` (initial_load.go:3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitialLoad {
    #[serde(rename = "user")]
    pub user: Option<Box<User>>,

    #[serde(rename = "team_members")]
    pub team_members: Option<Vec<TeamMember>>,

    #[serde(rename = "teams")]
    pub teams: Option<Vec<Team>>,

    #[serde(rename = "preferences")]
    pub preferences: Preferences,

    /// The client-visible slice of the server config.
    #[serde(rename = "client_cfg")]
    pub client_cfg: Option<StringMap>,

    #[serde(rename = "license_cfg")]
    pub license_cfg: Option<StringMap>,

    #[serde(rename = "no_accounts")]
    pub no_accounts: bool,
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
    fn initial_load_round_trips_the_fixture() {
        assert_fixture_round_trips!(InitialLoad, "initial_load");
    }
}
