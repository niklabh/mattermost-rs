//! The two constraint grammars and the config-path equality `noticeMatchesConditions`
//! (channels/app/product_notices.go:30) judges a product notice with:
//!
//! - `github.com/Masterminds/semver/v3` v3.5.0 — [`new_version`] (the **coercing** parser the
//!   library defaults to, which accepts `7.9`, `v11` and `01.2.3` where its strict parser would
//!   not), [`Constraints`] and their check;
//! - `github.com/reflog/dateconstraints` v0.2.1 — [`DateConstraints`], the same grammar over
//!   RFC 3339 instants;
//! - `config.GetValueByPath` plus the typed `==` of `validateConfigEntry` — [`config_entry_matches`].
//!
//! Each is a port of the library source, regular expressions included verbatim, and each is
//! pinned to `fixtures/behaviour_notice_conditions.json`, which `reference/dump` produces by
//! calling the libraries themselves over the corpora in `behaviour_notice_conditions.go`.
//!
//! # Two things a reader would get wrong
//!
//! **The date grammar's hyphen range is broken in the library**, and stays broken here: its
//! `rewriteRange` was copied from the semver package and reads the second date out of capture
//! group 11, which in the date regex is a fragment of that date rather than the whole of it; the
//! rewritten constraint then fails validation. A notice with `"2026-09-01T00:00:00Z -
//! 2026-09-30T00:00:00Z"` is therefore an error on both servers, not a range.
//!
//! **`1 == 1.0` is false for a config entry.** A notice's `serverConfig` values arrive from JSON,
//! so a number is a `float64`; the setting it is compared with is an `int` or `int64` pointer,
//! and Go's interface equality compares dynamic types before values. Only strings, booleans and
//! `null` can ever match.

use std::cmp::Ordering;
use std::sync::LazyLock;

use chrono::{DateTime, FixedOffset, Utc};
use regex::Regex;

// ---------------------------------------------------------------------------------------------
// Masterminds/semver: versions
// ---------------------------------------------------------------------------------------------

/// `MaxVersionLen` (version.go:60).
const MAX_VERSION_LEN: usize = 256;

/// `looseSemVerRegex` (version.go:71), anchored as `looseVersionRegex` is.
static LOOSE_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^v?([0-9]+)(\.[0-9]+)?(\.[0-9]+)?(-([0-9A-Za-z\-]+(\.[0-9A-Za-z\-]+)*))?(\+([0-9A-Za-z\-]+(\.[0-9A-Za-z\-]+)*))?$",
    )
    .expect("a literal regular expression")
});

/// Port of `semver.Version`: the three numbers, the prerelease and the build metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: String,
    pub metadata: String,
}

/// Port of `semver.NewVersion` with `CoerceNewVersion == true` (version.go:176, :252): the loose
/// regex, `strconv.ParseUint` on each segment (so a number past `u64` is an error), and the
/// prerelease and metadata validators. `None` is any error.
pub fn new_version(v: &str) -> Option<Version> {
    if v.len() > MAX_VERSION_LEN {
        return None;
    }
    let m = LOOSE_VERSION_REGEX.captures(v)?;
    let segment = |i: usize| -> Option<u64> {
        match m.get(i) {
            Some(part) => part.as_str().trim_start_matches('.').parse::<u64>().ok(),
            None => Some(0),
        }
    };
    let version = Version {
        major: segment(1)?,
        minor: segment(2)?,
        patch: segment(3)?,
        pre: m.get(5).map(|p| p.as_str().to_owned()).unwrap_or_default(),
        metadata: m.get(8).map(|p| p.as_str().to_owned()).unwrap_or_default(),
    };
    if !version.pre.is_empty() && !valid_prerelease(&version.pre) {
        return None;
    }
    if !version.metadata.is_empty() && !valid_metadata(&version.metadata) {
        return None;
    }
    Some(version)
}

/// `validatePrerelease` (version.go:745): no empty part, and a numeric part has no leading
/// zero. The character class is the regex's already.
fn valid_prerelease(p: &str) -> bool {
    p.split('.').all(|part| {
        if part.is_empty() {
            return false;
        }
        if part.bytes().all(|b| b.is_ascii_digit()) {
            return !(part.len() > 1 && part.starts_with('0'));
        }
        part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// `validateMetadata` (version.go:766): no empty part.
fn valid_metadata(m: &str) -> bool {
    m.split('.').all(|part| {
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

impl Version {
    /// Port of `Version.Compare` (version.go:525): the three numbers, then a release outranks
    /// any prerelease, then the prerelease parts. Metadata is ignored.
    pub fn compare(&self, o: &Version) -> Ordering {
        self.major
            .cmp(&o.major)
            .then(self.minor.cmp(&o.minor))
            .then(self.patch.cmp(&o.patch))
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_prerelease(&self.pre, &o.pre),
            })
    }

    pub fn less_than(&self, o: &Version) -> bool {
        self.compare(o) == Ordering::Less
    }

    pub fn equal(&self, o: &Version) -> bool {
        self.compare(o) == Ordering::Equal
    }
}

/// `comparePrerelease` (version.go:638): part by part, a missing part being `""`.
fn compare_prerelease(v: &str, o: &str) -> Ordering {
    let sparts: Vec<&str> = v.split('.').collect();
    let oparts: Vec<&str> = o.split('.').collect();
    for i in 0..sparts.len().max(oparts.len()) {
        let d = compare_pre_part(
            sparts.get(i).copied().unwrap_or(""),
            oparts.get(i).copied().unwrap_or(""),
        );
        if d != Ordering::Equal {
            return d;
        }
    }
    Ordering::Equal
}

/// `comparePrePart` (version.go:680): numbers before strings, numbers numerically, strings
/// bytewise, and the empty part below any present one — **except** that two absent parts
/// compare equal on the fast path only when both are `""`, which the caller's loop bound makes
/// unreachable.
fn compare_pre_part(s: &str, o: &str) -> Ordering {
    if s == o {
        return Ordering::Equal;
    }
    if s.is_empty() {
        return if o.is_empty() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if o.is_empty() {
        return Ordering::Greater;
    }
    match (s.parse::<u64>(), o.parse::<u64>()) {
        (Err(_), Err(_)) => {
            if s > o {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Ok(si), Ok(oi)) => {
            if si > oi {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Masterminds/semver: constraints
// ---------------------------------------------------------------------------------------------

/// `MaxConstraintLen` and `MaxConstraintGroups` (constraints.go).
const MAX_CONSTRAINT_LEN: usize = 512;
const MAX_CONSTRAINT_GROUPS: usize = 32;

/// `cvRegex` (constraints.go): the version shape a constraint may carry, wildcards included.
const CV_REGEX: &str = r"v?([0-9|x|X|\*]+)(\.[0-9|x|X|\*]+)?(\.[0-9|x|X|\*]+)?(-([0-9A-Za-z\-]+(\.[0-9A-Za-z\-]+)*))?(\+([0-9A-Za-z\-]+(\.[0-9A-Za-z\-]+)*))?";
/// The operator alternation, empty branch included — `=||!=` is Go's spelling and it is what
/// lets a bare `1.2.3` parse with the empty operator.
const OPS: &str = r"=||!=|>|<|>=|=>|<=|=<|~|~>|\^";

static CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^\s*({OPS})\s*({CV_REGEX})\s*$")).expect("a literal regular expression")
});
static CONSTRAINT_RANGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"\s*({CV_REGEX})\s+-\s+({CV_REGEX})\s*"))
        .expect("a literal regular expression")
});
static FIND_CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"({OPS})\s*({CV_REGEX})")).expect("a literal regular expression")
});
static VALID_CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^(\s*({OPS})\s*({CV_REGEX})\s*)((?:\s+|,\s*)({OPS})\s*({CV_REGEX})\s*)*$"
    ))
    .expect("a literal regular expression")
});

/// One `constraint` (constraints.go): the parsed version, the operator and the three wildcard
/// flags. Go's `orig` text feeds only the error strings `Check` discards, so it is not kept.
#[derive(Debug, Clone)]
struct Constraint {
    con: Version,
    op: String,
    minor_dirty: bool,
    dirty: bool,
    patch_dirty: bool,
}

/// Port of `semver.Constraints`: groups joined by `||`, each a conjunction.
#[derive(Debug, Clone)]
pub struct Constraints {
    groups: Vec<Vec<Constraint>>,
    contains_pre: Vec<bool>,
}

fn is_x(x: &str) -> bool {
    matches!(x, "x" | "*" | "X")
}

/// `rewriteRange` (constraints.go): `A - B` becomes `>= A, <= B `, first occurrence of each
/// match, in order.
fn rewrite_range(input: &str) -> String {
    let mut out = input.to_owned();
    for m in CONSTRAINT_RANGE_REGEX.captures_iter(input) {
        let whole = m.get(0).map(|w| w.as_str()).unwrap_or_default();
        let low = m.get(1).map(|w| w.as_str()).unwrap_or_default();
        let high = m.get(11).map(|w| w.as_str()).unwrap_or_default();
        out = out.replacen(whole, &format!(">= {low}, <= {high} "), 1);
    }
    out
}

/// `parseConstraint` (constraints.go:240).
fn parse_constraint(c: &str) -> Option<Constraint> {
    if c.is_empty() {
        return Some(Constraint {
            con: Version {
                major: 0,
                minor: 0,
                patch: 0,
                pre: String::new(),
                metadata: String::new(),
            },
            op: String::new(),
            minor_dirty: false,
            dirty: true,
            patch_dirty: false,
        });
    }
    let m = CONSTRAINT_REGEX.captures(c)?;
    let group = |i: usize| m.get(i).map(|g| g.as_str()).unwrap_or_default();
    let (op, whole, major, minor, patch, pre) =
        (group(1), group(2), group(3), group(4), group(5), group(6));
    let mut minor_dirty = false;
    let mut patch_dirty = false;
    let mut dirty = false;
    let ver = if is_x(major) || major.is_empty() {
        dirty = true;
        format!("0.0.0{pre}")
    } else if is_x(minor.trim_start_matches('.')) || minor.is_empty() {
        minor_dirty = true;
        dirty = true;
        format!("{major}.0.0{pre}")
    } else if is_x(patch.trim_start_matches('.')) || patch.is_empty() {
        dirty = true;
        patch_dirty = true;
        format!("{major}{minor}.0{pre}")
    } else {
        whole.to_owned()
    };
    let con = new_version(&ver)?;
    Some(Constraint {
        con,
        op: op.to_owned(),
        minor_dirty,
        dirty,
        patch_dirty,
    })
}

/// Port of `semver.NewConstraint` (constraints.go:34). `None` is any of its errors: too long,
/// too many `||` groups, a group the validating regex refuses, a member the parser refuses.
pub fn new_constraint(c: &str) -> Option<Constraints> {
    if c.len() > MAX_CONSTRAINT_LEN {
        return None;
    }
    let c = rewrite_range(c);
    let ors: Vec<&str> = c.split("||").collect();
    if ors.len() > MAX_CONSTRAINT_GROUPS {
        return None;
    }
    let mut groups = Vec::with_capacity(ors.len());
    let mut contains_pre = Vec::with_capacity(ors.len());
    for v in ors {
        if !VALID_CONSTRAINT_REGEX.is_match(v) {
            return None;
        }
        let mut members: Vec<&str> = FIND_CONSTRAINT_REGEX
            .find_iter(v)
            .map(|m| m.as_str())
            .collect();
        if members.is_empty() {
            members.push(v);
        }
        let mut group = Vec::with_capacity(members.len());
        let mut has_pre = false;
        for s in members {
            let pc = parse_constraint(s)?;
            if !pc.con.pre.is_empty() {
                has_pre = true;
            }
            group.push(pc);
        }
        groups.push(group);
        contains_pre.push(has_pre);
    }
    Some(Constraints {
        groups,
        contains_pre,
    })
}

impl Constraints {
    /// Port of `Constraints.Check` (constraints.go:86): any group all of whose members pass.
    /// `IncludePrerelease` is never set by the caller, so a prerelease version passes only a
    /// group that itself names a prerelease.
    pub fn check(&self, v: &Version) -> bool {
        self.groups
            .iter()
            .zip(&self.contains_pre)
            .any(|(group, &has_pre)| group.iter().all(|c| c.check(v, has_pre)))
    }
}

impl Constraint {
    /// `constraintOps[c.origfunc](v, c, includePre)`.
    fn check(&self, v: &Version, include_pre: bool) -> bool {
        if !v.pre.is_empty() && !include_pre {
            return false;
        }
        match self.op.as_str() {
            "" | "=" => self.tilde_or_equal(v),
            "!=" => self.not_equal(v),
            ">" => self.greater_than(v),
            "<" => v.less_than(&self.con),
            ">=" | "=>" => v.compare(&self.con) != Ordering::Less,
            "<=" | "=<" => self.less_than_equal(v),
            "~" | "~>" => self.tilde(v),
            "^" => self.caret(v),
            _ => false,
        }
    }

    /// `constraintNotEqual`.
    fn not_equal(&self, v: &Version) -> bool {
        if self.dirty {
            if self.con.major != v.major {
                return true;
            }
            if self.con.minor != v.minor && !self.minor_dirty {
                return true;
            } else if self.minor_dirty {
                return false;
            } else if self.con.patch != v.patch && !self.patch_dirty {
                return true;
            } else if self.patch_dirty {
                if !v.pre.is_empty() || !self.con.pre.is_empty() {
                    return compare_prerelease(&v.pre, &self.con.pre) != Ordering::Equal;
                }
                return false;
            }
        }
        !v.equal(&self.con)
    }

    /// `constraintGreaterThan`.
    #[allow(
        clippy::if_same_then_else,
        reason = "Go's branch order, kept so each branch is a separate mutation target"
    )]
    fn greater_than(&self, v: &Version) -> bool {
        if !self.dirty {
            return v.compare(&self.con) == Ordering::Greater;
        }
        if v.major > self.con.major {
            return true;
        } else if v.major < self.con.major {
            return false;
        } else if self.minor_dirty {
            return false;
        } else if self.patch_dirty {
            return v.minor > self.con.minor;
        }
        v.compare(&self.con) == Ordering::Greater
    }

    /// `constraintLessThanEqual`.
    #[allow(
        clippy::if_same_then_else,
        reason = "Go's branch order, kept so each branch is a separate mutation target"
    )]
    fn less_than_equal(&self, v: &Version) -> bool {
        if !self.dirty {
            return v.compare(&self.con) != Ordering::Greater;
        }
        if v.major > self.con.major {
            return false;
        } else if v.major == self.con.major && v.minor > self.con.minor && !self.minor_dirty {
            return false;
        }
        true
    }

    /// `constraintTilde`.
    fn tilde(&self, v: &Version) -> bool {
        if v.less_than(&self.con) {
            return false;
        }
        if self.con.major == 0
            && self.con.minor == 0
            && self.con.patch == 0
            && !self.minor_dirty
            && !self.patch_dirty
        {
            return true;
        }
        if v.major != self.con.major {
            return false;
        }
        if v.minor != self.con.minor && !self.minor_dirty {
            return false;
        }
        true
    }

    /// `constraintTildeOrEqual`.
    fn tilde_or_equal(&self, v: &Version) -> bool {
        if self.dirty {
            return self.tilde(v);
        }
        v.equal(&self.con)
    }

    /// `constraintCaret`.
    fn caret(&self, v: &Version) -> bool {
        if v.less_than(&self.con) {
            return false;
        }
        if self.con.major > 0 || self.minor_dirty {
            return v.major == self.con.major;
        }
        if self.con.major == 0 && v.major > 0 {
            return false;
        }
        if self.con.minor > 0 || self.patch_dirty {
            return v.minor == self.con.minor;
        }
        if self.con.minor == 0 && v.minor > 0 {
            return false;
        }
        self.con.patch == v.patch
    }
}

// ---------------------------------------------------------------------------------------------
// reflog/dateconstraints
// ---------------------------------------------------------------------------------------------

/// `cvRegex` (dateconstraints/constraints.go).
const DATE_CV_REGEX: &str =
    r"\d{4}(-\d\d(-\d\d(T\d\d:\d\d(:\d\d)?(\.\d+)?(([+-]\d\d:\d\d)|Z)?)?)?)?";
/// The eight operators. Go builds this alternation from a map, in an order that differs per
/// process; every order yields the same match, because no shorter operator can complete a match
/// that a longer one would.
const DATE_OPS: &str = r"!=|=|>|<|>=|=>|<=|=<";

static DATE_CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^\s*({DATE_OPS})\s*({DATE_CV_REGEX})\s*$"))
        .expect("a literal regular expression")
});
static DATE_CONSTRAINT_RANGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"\s*({DATE_CV_REGEX})\s+-\s+({DATE_CV_REGEX})\s*"))
        .expect("a literal regular expression")
});
static DATE_FIND_CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"({DATE_OPS})\s*({DATE_CV_REGEX})")).expect("a literal regular expression")
});
static DATE_VALID_CONSTRAINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^(\s*({DATE_OPS})\s*({DATE_CV_REGEX})\s*\,?)+$"))
        .expect("a literal regular expression")
});

#[derive(Debug, Clone)]
struct DateConstraint {
    con: DateTime<FixedOffset>,
    op: String,
}

/// Port of `date_constraints.Constraints`.
#[derive(Debug, Clone)]
pub struct DateConstraints {
    groups: Vec<Vec<DateConstraint>>,
}

/// The library's `rewriteRange`, group 11 and all — see the module docs.
fn rewrite_date_range(input: &str) -> String {
    let mut out = input.to_owned();
    for m in DATE_CONSTRAINT_RANGE_REGEX.captures_iter(input) {
        let whole = m.get(0).map(|w| w.as_str()).unwrap_or_default();
        let low = m.get(1).map(|w| w.as_str()).unwrap_or_default();
        let high = m.get(11).map(|w| w.as_str()).unwrap_or_default();
        out = out.replacen(whole, &format!(">= {low}, <= {high}"), 1);
    }
    out
}

/// `parseConstraint`: the operator and an RFC 3339 instant — a bare date or year is a parser
/// error even though the regex admits it.
fn parse_date_constraint(c: &str) -> Option<DateConstraint> {
    if c.is_empty() {
        return None;
    }
    let m = DATE_CONSTRAINT_REGEX.captures(c)?;
    let op = m.get(1).map(|g| g.as_str()).unwrap_or_default().to_owned();
    let text = m.get(2).map(|g| g.as_str()).unwrap_or_default();
    let con = DateTime::parse_from_rfc3339(text).ok()?;
    Some(DateConstraint { con, op })
}

/// Port of `date_constraints.NewConstraint`.
pub fn new_date_constraint(c: &str) -> Option<DateConstraints> {
    let c = rewrite_date_range(c);
    let mut groups = Vec::new();
    for v in c.split("||") {
        if !DATE_VALID_CONSTRAINT_REGEX.is_match(v) {
            return None;
        }
        let mut members: Vec<&str> = DATE_FIND_CONSTRAINT_REGEX
            .find_iter(v)
            .map(|m| m.as_str())
            .collect();
        if members.is_empty() {
            members.push(v);
        }
        let mut group = Vec::with_capacity(members.len());
        for s in members {
            group.push(parse_date_constraint(s)?);
        }
        groups.push(group);
    }
    Some(DateConstraints { groups })
}

impl DateConstraints {
    /// Port of `Constraints.Check`: instant comparisons, so the zone of either side is
    /// immaterial.
    pub fn check(&self, v: DateTime<Utc>) -> bool {
        self.groups.iter().any(|group| {
            group.iter().all(|c| {
                let con = c.con.with_timezone(&Utc);
                match c.op.as_str() {
                    "!=" => v != con,
                    "=" => v == con,
                    ">" => v > con,
                    "<" => v < con,
                    ">=" | "=>" => v >= con,
                    "<=" | "=<" => v <= con,
                    _ => false,
                }
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// config.GetValueByPath + validateConfigEntry
// ---------------------------------------------------------------------------------------------

/// Port of `validateConfigEntry` (product_notices.go:201) over the configuration as JSON.
///
/// `GetValueByPath` walks the `model.Config` struct field by field name, which is the JSON key
/// for every section (the config structs carry no `json:` renames); a path that runs out
/// before a leaf, or runs past one, is "not found" and the entry fails. What it finds is a
/// pointer: nil matches only a `null` expectation, and a value matches only an expectation of
/// the **same dynamic type** — string to string, bool to bool, and never a number, since the
/// expectation is a `float64` and every numeric setting an `int` (see the module docs).
///
/// The two map-valued sections (`PluginSettings.Plugins`, `PluginSettings.PluginStates`) take a
/// prefix walk in Go that this JSON walk cannot tell apart from a struct's; the two agree for
/// every plugin id without a dot in it.
pub fn config_entry_matches(
    config: &serde_json::Value,
    path: &str,
    expected: &serde_json::Value,
) -> bool {
    let mut current = config;
    for segment in path.split('.') {
        let Some(object) = current.as_object() else {
            return false;
        };
        let Some(next) = object.get(segment) else {
            return false;
        };
        current = next;
    }
    match (current, expected) {
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_notice_conditions.json"
        ))
        .unwrap()
    }

    #[test]
    fn versions_parse_as_the_coercing_parser_parses_them() {
        let rows = oracle()["semver_parse"].as_array().unwrap().clone();
        assert!(rows.len() >= 40);
        for row in rows {
            let input = row["in"].as_str().unwrap();
            let parsed = new_version(input);
            assert_eq!(parsed.is_some(), row["ok"].as_bool().unwrap(), "{input:?}");
            if let Some(v) = parsed {
                assert_eq!(v.major, row["major"].as_u64().unwrap(), "{input:?}");
                assert_eq!(v.minor, row["minor"].as_u64().unwrap(), "{input:?}");
                assert_eq!(v.patch, row["patch"].as_u64().unwrap(), "{input:?}");
                assert_eq!(v.pre, row["pre"].as_str().unwrap(), "{input:?}");
                assert_eq!(v.metadata, row["metadata"].as_str().unwrap(), "{input:?}");
            }
        }
    }

    #[test]
    fn constraints_check_as_masterminds_checks_them() {
        let rows = oracle()["semver_check"].as_array().unwrap().clone();
        assert!(rows.len() > 2000);
        let mut checked = 0;
        for row in rows {
            let text = row["constraint"].as_str().unwrap();
            let parsed = new_constraint(text);
            assert_eq!(parsed.is_some(), row["ok"].as_bool().unwrap(), "{text:?}");
            let Some(cs) = parsed else { continue };
            let version = new_version(row["version"].as_str().unwrap()).unwrap();
            assert_eq!(
                cs.check(&version),
                row["check"].as_bool().unwrap(),
                "{text:?} against {:?}",
                row["version"]
            );
            checked += 1;
        }
        assert!(checked > 2000);
    }

    #[test]
    fn date_constraints_check_as_the_library_checks_them() {
        let rows = oracle()["date_check"].as_array().unwrap().clone();
        assert!(rows.len() > 100);
        for row in rows {
            let text = row["constraint"].as_str().unwrap();
            let parsed = new_date_constraint(text);
            assert_eq!(parsed.is_some(), row["ok"].as_bool().unwrap(), "{text:?}");
            let Some(cs) = parsed else { continue };
            let at = DateTime::parse_from_rfc3339(row["date"].as_str().unwrap())
                .unwrap()
                .to_utc();
            assert_eq!(
                cs.check(at),
                row["check"].as_bool().unwrap(),
                "{text:?} at {at}"
            );
        }
    }

    #[test]
    fn config_entries_compare_by_dynamic_type() {
        // The configuration the oracle pinned: defaults plus five settings.
        let config = serde_json::json!({
            "ServiceSettings": {
                "CollapsedThreads": "always_on",
                "EnableLinkPreviews": true,
                "PostEditTimeLimit": -1
            },
            "FileSettings": { "MaxFileSize": 1048576 },
            "ImageProxySettings": { "ImageProxyType": "atmos/camo" },
            "LdapSettings": { "LoginIdAttribute": null }
        });
        let rows = oracle()["config_entry"].as_array().unwrap().clone();
        assert_eq!(rows.len(), 18);
        for row in rows {
            let path = row["path"].as_str().unwrap();
            let expected = &row["expected"];
            let want = row["match"].as_bool().unwrap();
            // `BleveSettings` is absent from the document above and present-but-unset in Go's;
            // both are "not found" for the walk, which the oracle records as no match.
            assert_eq!(
                config_entry_matches(&config, path, expected),
                want,
                "{path} vs {expected}"
            );
        }
    }
}
