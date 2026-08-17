//! Port of `server/public/model/version.go`.
//!
//! The release table plus the four functions that read it. There are no `json:` tags in this
//! file, so there is no serialization fixture; everything here is pinned instead by
//! `fixtures/behaviour_version.json`, which records Go's own answers over a corpus.
//!
//! Three things about this file are easy to get wrong:
//!
//! 1. **`versions` and `versionsWithoutHotFixes` are unexported in Go**, so [`VERSIONS`] is a
//!    transcription. It is not an unchecked one: the oracle extracts the literal out of
//!    `version.go` with `go/parser` and [`go_parity::versions_match_go`] compares it entry for
//!    entry, so an upstream release bump fails a test rather than leaving the port stale.
//!
//! 2. **`SplitVersion` throws away every `strconv.ParseInt` error**, and Go's `ParseInt` does
//!    not fail the way Rust's `str::parse::<i64>` does. On a range error Go returns the
//!    *clamped* bound alongside the error, and since the error is discarded that clamped value
//!    is the answer: `SplitVersion("99999999999999999999.0.0")` yields `9223372036854775807`,
//!    not `0`. See [`parse_int64_go`].
//!
//! 3. **Only the major and minor components are ever looked up.** Every function reduces its
//!    input to `"<major>.<minor>.0"` first, so `11.11.5`, `11.11` and `11.11.0.1` are all the
//!    same query, and the five hotfix entries in [`VERSIONS`] (`4.8.1`, `4.7.2`, `4.7.1`,
//!    `1.2.1`, `0.7.1`) are invisible to all of them.

use std::num::IntErrorKind;
use std::sync::LazyLock;

/// Port of `versions` (version.go:15) — every shipped release, newest first.
///
/// Unexported in Go; public here because [`CURRENT_VERSION`] is derived from it and the
/// oracle pins it. Extracted from the Go source rather than typed by hand.
pub const VERSIONS: &[&str] = &[
    "11.11.0", "11.10.0", "11.9.0", "11.8.0", "11.7.0", //
    "11.6.0", "11.5.0", "11.4.0", "11.3.0", "11.2.0", //
    "11.1.0", "11.0.0", "10.12.0", "10.11.0", "10.10.0", //
    "10.9.0", "10.8.0", "10.7.0", "10.6.0", "10.5.0", //
    "10.4.0", "10.3.0", "10.2.0", "10.1.0", "10.0.0", //
    "9.11.0", "9.10.0", "9.9.0", "9.8.0", "9.7.0", //
    "9.6.0", "9.5.0", "9.4.0", "9.3.0", "9.2.0", //
    "9.1.0", "9.0.0", "8.1.0", "8.0.0", "7.11.0", //
    "7.10.0", "7.9.0", "7.8.0", "7.7.0", "7.6.0", //
    "7.5.0", "7.4.0", "7.3.0", "7.2.0", "7.1.0", //
    "7.0.0", "6.7.0", "6.6.0", "6.5.0", "6.4.0", //
    "6.3.0", "6.2.0", "6.1.0", "6.0.0", "5.39.0", //
    "5.38.0", "5.37.0", "5.36.0", "5.35.0", "5.34.0", //
    "5.33.0", "5.32.0", "5.31.0", "5.30.0", "5.29.0", //
    "5.28.0", "5.27.0", "5.26.0", "5.25.0", "5.24.0", //
    "5.23.0", "5.22.0", "5.21.0", "5.20.0", "5.19.0", //
    "5.18.0", "5.17.0", "5.16.0", "5.15.0", "5.14.0", //
    "5.13.0", "5.12.0", "5.11.0", "5.10.0", "5.9.0", //
    "5.8.0", "5.7.0", "5.6.0", "5.5.0", "5.4.0", //
    "5.3.0", "5.2.0", "5.1.0", "5.0.0", "4.10.0", //
    "4.9.0", "4.8.1", "4.8.0", "4.7.2", "4.7.1", //
    "4.7.0", "4.6.0", "4.5.0", "4.4.0", "4.3.0", //
    "4.2.0", "4.1.0", "4.0.0", "3.10.0", "3.9.0", //
    "3.8.0", "3.7.0", "3.6.0", "3.5.0", "3.4.0", //
    "3.3.0", "3.2.0", "3.1.0", "3.0.0", "2.2.0", //
    "2.1.0", "2.0.0", "1.4.0", "1.3.0", "1.2.1", //
    "1.2.0", "1.1.0", "1.0.0", "0.7.1", "0.7.0", //
    "0.6.0", "0.5.0",
];

/// Port of `model.CurrentVersion` (version.go:155) — `versions[0]`.
///
/// A `var` in Go only because the expression is not compile-time constant; nothing in the model
/// package reassigns it and it is not injected by `-ldflags` the way the `Build*` values below
/// are, so a `const` is faithful.
///
/// Every `Etag` in the tree prefixes this string, which is why `utils` re-exports it.
pub const CURRENT_VERSION: &str = VERSIONS[0];

/// Port of `versionsWithoutHotFixes` (version.go:161), built by `init()` at version.go:163.
///
/// Each entry of [`VERSIONS`] is reduced to `"<major>.<minor>.0"` and the **first** occurrence
/// wins, so `4.8.1` claims the slot and the later `4.8.0` is dropped as a duplicate. This is
/// the list all three lookup functions actually search — [`VERSIONS`] itself is never consulted
/// after startup.
pub static VERSIONS_WITHOUT_HOTFIXES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut out: Vec<String> = Vec::with_capacity(VERSIONS.len());
    for version in VERSIONS {
        let key = version_key(version);
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out
});

/// Port of `model.BuildNumber` (version.go:156).
///
/// Go leaves these empty and the build injects them with
/// `-ldflags "-X ...model.BuildNumber=$(BUILD_NUMBER)"`. Rust has no link-time string
/// injection, so they read a build-time environment variable instead and fall back to Go's
/// zero value. The `MM_` prefix is ours; the Go build system has no such variable.
pub const BUILD_NUMBER: &str = match option_env!("MM_BUILD_NUMBER") {
    Some(v) => v,
    None => "",
};

/// Port of `model.BuildDate` (version.go:157). See [`BUILD_NUMBER`].
pub const BUILD_DATE: &str = match option_env!("MM_BUILD_DATE") {
    Some(v) => v,
    None => "",
};

/// Port of `model.BuildHash` (version.go:158). See [`BUILD_NUMBER`].
pub const BUILD_HASH: &str = match option_env!("MM_BUILD_HASH") {
    Some(v) => v,
    None => "",
};

/// Port of `model.BuildHashEnterprise` (version.go:159). See [`BUILD_NUMBER`].
pub const BUILD_HASH_ENTERPRISE: &str = match option_env!("MM_BUILD_HASH_ENTERPRISE") {
    Some(v) => v,
    None => "",
};

/// Port of `model.BuildEnterpriseReady` (version.go:160). See [`BUILD_NUMBER`].
///
/// A string, not a bool — Go compares it against `"true"` at the call sites.
pub const BUILD_ENTERPRISE_READY: &str = match option_env!("MM_BUILD_ENTERPRISE_READY") {
    Some(v) => v,
    None => "",
};

/// `strconv.ParseInt(s, 10, 64)` with the error discarded, which is how version.go calls it.
///
/// The discarded error is the whole point. Go returns `(MaxInt64, ErrRange)` for an input too
/// large and `(MinInt64, ErrRange)` for one too small, so throwing the error away leaves the
/// **saturated** bound, not zero. Rust's `parse::<i64>()` reports the same two conditions as
/// `PosOverflow`/`NegOverflow`, and both parsers scan left to right and stop at the first
/// problem, so they agree even on `"99999999999999999999abc"` — overflow at digit 20 is
/// reached before the `a`, and both answer `MaxInt64` rather than "syntax error".
///
/// Every other failure (empty, non-digit, whitespace, `0x`/`0b` prefixes, `_` separators —
/// which `ParseInt` accepts only when base is 0 — and non-ASCII digits) is `ErrSyntax` in Go
/// and yields `0` in both.
fn parse_int64_go(s: &str) -> i64 {
    match s.parse::<i64>() {
        Ok(v) => v,
        Err(e) => match e.kind() {
            IntErrorKind::PosOverflow => i64::MAX,
            IntErrorKind::NegOverflow => i64::MIN,
            _ => 0,
        },
    }
}

/// Port of `model.SplitVersion` (version.go:178).
///
/// Splits on `.` and parses at most the first three parts; anything beyond the third is
/// discarded and a missing part is `0`. Nothing here can fail — see [`parse_int64_go`] for
/// what Go does with the parse errors it ignores.
///
/// Note `"".split('.')` yields one empty part in both languages, so an empty version is
/// `(0, 0, 0)` rather than a special case.
pub fn split_version(version: &str) -> (i64, i64, i64) {
    let mut parts = version.split('.');
    let major = parts.next().map_or(0, parse_int64_go);
    let minor = parts.next().map_or(0, parse_int64_go);
    let patch = parts.next().map_or(0, parse_int64_go);
    (major, minor, patch)
}

/// `fmt.Sprintf("%v.%v.0", major, minor)` — the lookup key the three functions below share.
///
/// Go formats `int64` with `%v`, which agrees with Rust's `Display` including for negatives,
/// so `"-1.-1.0"` reduces to `"-1.-1.0"` and simply misses.
fn version_key(version: &str) -> String {
    let (major, minor, _) = split_version(version);
    format!("{major}.{minor}.0")
}

/// Port of `model.GetPreviousVersion` (version.go:200).
///
/// Returns the release immediately older than `version` in [`VERSIONS_WITHOUT_HOTFIXES`], or
/// `""` when the version is unknown *or* is the oldest entry. The two cases are not
/// distinguished — `get_previous_version("0.5.0")` and `get_previous_version("garbage")` both
/// return `""`.
///
/// Because the lookup goes through [`version_key`], a hotfix returns the predecessor of its
/// base release: `4.7.2` yields `4.6.0`, not `4.7.1`.
pub fn get_previous_version(version: &str) -> &'static str {
    let key = version_key(version);
    let list: &'static Vec<String> = &VERSIONS_WITHOUT_HOTFIXES;
    for (index, v) in list.iter().enumerate() {
        if *v == key && list.len() > index + 1 {
            return &list[index + 1];
        }
    }
    ""
}

/// Port of `model.IsCurrentVersion` (version.go:213).
///
/// Compares major and minor only, so every patch of the current release is "current" — and so
/// is `"11.11"`, which has no patch component at all.
pub fn is_current_version(version_to_check: &str) -> bool {
    let (current_major, current_minor, _) = split_version(CURRENT_VERSION);
    let (to_check_major, to_check_minor, _) = split_version(version_to_check);
    to_check_major == current_major && to_check_minor == current_minor
}

/// Port of `model.IsPreviousVersionsSupported` (version.go:223).
///
/// True for the current release and the three before it. Go writes this as four unrolled
/// comparisons against `versionsWithoutHotFixes[0..3]`, which **panics** if the table ever has
/// fewer than four entries; `take(4)` is the same answer without that edge. The entries are
/// distinct, so at most one can match.
pub fn is_previous_versions_supported(version_to_check: &str) -> bool {
    let key = version_key(version_to_check);
    VERSIONS_WITHOUT_HOTFIXES
        .iter()
        .take(4)
        .any(|supported| *supported == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_the_head_of_the_table() {
        assert_eq!(CURRENT_VERSION, VERSIONS[0]);
        assert_eq!(CURRENT_VERSION, "11.11.0");
    }

    #[test]
    fn build_metadata_defaults_to_gos_zero_value() {
        // Nothing sets the MM_BUILD_* variables in a plain `cargo test`, so these must be the
        // empty strings Go declares — a non-empty default would leak into any response that
        // reports the build.
        assert_eq!(BUILD_NUMBER, "");
        assert_eq!(BUILD_DATE, "");
        assert_eq!(BUILD_HASH, "");
        assert_eq!(BUILD_HASH_ENTERPRISE, "");
        assert_eq!(BUILD_ENTERPRISE_READY, "");
    }

    #[test]
    fn hotfixes_claim_the_slot_ahead_of_their_base_release() {
        let list = &*VERSIONS_WITHOUT_HOTFIXES;
        assert!(list.contains(&"4.8.0".to_string()));
        assert!(!list.contains(&"4.8.1".to_string()));
        // 137 releases, five of which are hotfixes of a release already in the table.
        assert_eq!(VERSIONS.len(), 137);
        assert_eq!(list.len(), 132);
    }

    #[test]
    fn the_derived_table_has_no_duplicates() {
        let list = &*VERSIONS_WITHOUT_HOTFIXES;
        let mut sorted = list.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), list.len());
    }

    #[test]
    fn parse_int64_go_saturates_where_rust_would_fail() {
        assert_eq!(parse_int64_go("42"), 42);
        assert_eq!(parse_int64_go("+42"), 42);
        assert_eq!(parse_int64_go("-42"), -42);
        assert_eq!(parse_int64_go(""), 0);
        assert_eq!(parse_int64_go("x"), 0);
        assert_eq!(parse_int64_go("9223372036854775807"), i64::MAX);
        assert_eq!(parse_int64_go("9223372036854775808"), i64::MAX);
        assert_eq!(parse_int64_go("-9223372036854775809"), i64::MIN);
        // Overflow is detected before the trailing garbage is reached, in both languages.
        assert_eq!(parse_int64_go("99999999999999999999abc"), i64::MAX);
        // ...but garbage *before* the overflow is a syntax error, so zero.
        assert_eq!(parse_int64_go("abc99999999999999999999"), 0);
    }

    #[test]
    fn get_previous_version_walks_the_whole_table() {
        // Chaining from the newest release must reach the oldest, one step per entry.
        let mut current = CURRENT_VERSION.to_string();
        let mut steps = 0;
        loop {
            let next = get_previous_version(&current);
            if next.is_empty() {
                break;
            }
            current = next.to_string();
            steps += 1;
            assert!(steps <= VERSIONS.len(), "get_previous_version cycled");
        }
        assert_eq!(current, "0.5.0");
        assert_eq!(steps, VERSIONS_WITHOUT_HOTFIXES.len() - 1);
    }

    #[test]
    fn the_support_window_is_four_releases_wide() {
        assert!(is_previous_versions_supported("11.11.0"));
        assert!(is_previous_versions_supported("11.10.0"));
        assert!(is_previous_versions_supported("11.9.0"));
        assert!(is_previous_versions_supported("11.8.0"));
        assert!(!is_previous_versions_supported("11.7.0"));
    }
}

/// Parity tests driven by `fixtures/behaviour_version.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_version.json")).unwrap()
    }

    /// The transcription check. [`VERSIONS`] is typed into Rust because Go does not export it;
    /// the oracle reads the literal back out of `version.go` with `go/parser`, so this fails on
    /// a typo *and* on an upstream release bump.
    #[test]
    fn versions_match_go() {
        let oracle = oracle();
        let go: Vec<&str> = oracle["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            VERSIONS,
            &go[..],
            "version.go's release list moved; regenerate VERSIONS from the oracle"
        );
    }

    #[test]
    fn current_version_matches_go() {
        assert_eq!(
            CURRENT_VERSION,
            oracle()["current_version"].as_str().unwrap()
        );
    }

    /// Go's `versionsWithoutHotFixes` is unexported, so the oracle observes it by chaining
    /// `GetPreviousVersion` from `CurrentVersion` rather than re-deriving it.
    #[test]
    fn versions_without_hotfixes_matches_go() {
        let oracle = oracle();
        let go: Vec<&str> = oracle["versions_without_hotfixes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let ours: Vec<&str> = VERSIONS_WITHOUT_HOTFIXES
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(ours, go);
    }

    #[test]
    fn split_version_matches_go() {
        let oracle = oracle();
        let cases = oracle["split_version"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["in"].as_str().unwrap();
            let want = (
                case["major"].as_i64().unwrap(),
                case["minor"].as_i64().unwrap(),
                case["patch"].as_i64().unwrap(),
            );
            assert_eq!(split_version(input), want, "split_version({input:?})");
        }
    }

    #[test]
    fn get_previous_version_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_previous_version"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                get_previous_version(input),
                want.as_str().unwrap(),
                "get_previous_version({input:?})"
            );
        }
    }

    #[test]
    fn is_current_version_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_current_version"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                is_current_version(input),
                want.as_bool().unwrap(),
                "is_current_version({input:?})"
            );
        }
    }

    #[test]
    fn is_previous_versions_supported_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_previous_versions_supported"]
            .as_object()
            .unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                is_previous_versions_supported(input),
                want.as_bool().unwrap(),
                "is_previous_versions_supported({input:?})"
            );
        }
    }
}
