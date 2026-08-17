//! Port of `model/utils.go` (utils.go:1–938).
//!
//! Covers ID generation, the timestamp helpers, the identifier validators, and
//! `AppError` — the error envelope every API response uses.
//!
//! # Deliberately not translated here
//!
//! - `IsValidHTTPURL` (utils.go:790) delegates to Go's `net/url`. Reproducing RFC 3986
//!   acceptance exactly is a parser port, not a translation, and an approximation would
//!   silently accept or reject URLs the Go server does not. It gets its own session.
//! - `ParseHashtags` (utils.go:750) is post-rendering logic and belongs with `post.go`.
//! - `Scan`/`Value` (utils.go:100–216) are `database/sql` driver impls — they belong in
//!   `mm-store`, expressed as sqlx traits, not here.
//! - The `io.Reader` JSON helpers (utils.go:495–614) exist because Go lacks serde. Callers
//!   use `serde_json` directly.
//! - `GetServerIPAddress` (network interfaces), `IsCloud` (env var), `NewTestPassword`
//!   (test-only), `SliceToMapKey` (panics by design).
//! - `NewRandomTeamName` depends on `IsReservedTeamName`, which lives in `team.go`; it follows
//!   that file. `Etag` needed `CurrentVersion` from `version.go` and now has it — see
//!   [`CURRENT_VERSION`].
//!
//! Go's `StringMap`/`StringArray`/`StringSet`/`StringInterface` methods (`Has`, `Add`,
//! `Contains`, `Equals`, `Remove`, `CopyStringMap`) are all std operations in Rust —
//! `HashSet::contains`, `Vec::contains`, `==`, `Vec::retain`, `Clone` — so they are aliased
//! rather than wrapped.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Utc};
use rand::RngCore;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::go_url::parse_request_uri;

/// Port of `model.StringMap` — serialises as a plain JSON object.
///
/// A `BTreeMap`, **not** a `HashMap`, for the same wire reason [`StringInterface`] is a
/// `serde_json::Map`: Go's `encoding/json` sorts map keys by byte value when marshalling, while
/// a `HashMap` emits iteration order — which is not merely unsorted but *unstable between runs*,
/// so one process could serialise the same value two different ways. See [D-027].
///
/// Note: a **nil** Go map marshals to `null`, not `{}`. Struct fields that Go can leave nil
/// must therefore be `Option<StringMap>`, or the wire format drifts.
pub type StringMap = BTreeMap<String, String>;

/// Port of `model.StringInterface` (utils.go:48).
///
/// A `serde_json::Map`, **not** a `HashMap`, and that is a wire decision rather than a taste
/// one. Go's `encoding/json` sorts map keys by byte value when marshalling; a `HashMap` emits
/// iteration order, which is neither sorted nor stable between runs. Since `serde_json` builds
/// `Map` on a `BTreeMap` (absent the `preserve_order` feature) it is sorted for free, so
/// `Post.Props` and `Channel.Props` serialise byte-for-byte like Go's. See [D-027].
pub type StringInterface = serde_json::Map<String, serde_json::Value>;

/// Compares two `serde_json::Value`s the way Go compares two values that came out of
/// `encoding/json`.
///
/// The difference that matters is **numbers**. Go decodes every JSON number into a `float64`,
/// so `1` and `1.0` are the same value and compare equal. serde_json keeps them apart —
/// `Number(PosInt(1))` and `Number(Float(1.0))` are unequal — so a plain `==` on two decoded
/// documents disagrees with Go on any integral number written with a decimal point or an
/// exponent (`1e2` vs `100`).
///
/// Used by every `Equals` that compares an `any`/`map[string]any` field:
/// `MessageAttachment.Timestamp`, `MessageAttachmentField.Value` and
/// `PostAction.Integration.Context`.
pub fn json_values_equal_like_go(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(a, b)| json_values_equal_like_go(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_values_equal_like_go(v, w)))
        }
        _ => a == b,
    }
}

/// Go's **`encoding/json`** rendering of a `float64` — which is not [`go_format_float`], and not
/// serde_json's either.
///
/// There are three renderings of a float in play and they disagree on most values:
///
/// | value | `encoding/json` (this) | `%v` ([`go_format_float`]) | serde_json |
/// |---|---|---|---|
/// | `1.0` | `1` | `1` | `1.0` |
/// | `1234567.0` | `1234567` | `1.234567e+06` | `1234567.0` |
/// | `1e-6` | `0.000001` | `1e-06` | `1e-6` |
/// | `1e-7` | `1e-7` | `1e-07` | `1e-7` |
/// | `9.999999999999999e20` | `999999999999999900000` | `9.999999999999999e+20` | `9.999999999999999e+20` |
///
/// Go's encoder is (`encoding/json/encode.go`, `floatEncoder`):
///
/// ```text
/// fmt := byte('f')
/// if abs := math.Abs(f); abs != 0 && (abs < 1e-6 || abs >= 1e21) { fmt = 'e' }
/// b = strconv.AppendFloat(b, f, fmt, -1, 64)
/// if fmt == 'e' { rewrite a trailing "e-09" to "e-9" }
/// ```
///
/// So the thresholds are `1e-6` and `1e21` — not `%g`'s `1e-4`/`1e6` — and the **negative**
/// exponent has one leading zero stripped while the positive one keeps it: `1e-7` and `1e+21`.
///
/// `None` for `NaN`, `+Inf` and `-Inf`: Go's `json.Marshal` returns
/// `json: unsupported value: NaN` and emits **nothing**, so one bad value fails the whole
/// document rather than degrading to `null`. Callers must propagate that, not substitute.
///
/// Measured against Go over 29 values spanning both thresholds from each side; serde_json
/// disagrees on 11 of them.
pub fn go_json_format_float(f: f64) -> Option<String> {
    if f.is_nan() || f.is_infinite() {
        return None;
    }

    // Go spells this `abs < 1e-6 || abs >= 1e21`; the half-open range is the same test, and
    // clippy prefers it.
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        return Some(strip_exponent_zero(&format_exponential(f)));
    }
    Some(format_decimal(f))
}

/// `strconv.FormatFloat(f, 'e', -1, 64)`: `d.ddde±dd`, shortest round-tripping digits, exponent
/// signed and at least two digits.
///
/// Rust's `LowerExp` gives the same digits in the same normalised form but writes the exponent
/// as `e-7`/`e21` — no `+`, no zero padding — so only the exponent needs rebuilding.
fn format_exponential(f: f64) -> String {
    let sci = format!("{f:e}");
    let Some((mantissa, exp_str)) = sci.split_once('e') else {
        return sci;
    };
    let Ok(exp) = exp_str.parse::<i32>() else {
        return sci;
    };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exp.abs())
}

/// `strconv.FormatFloat(f, 'f', -1, 64)`: positional, shortest round-tripping digits, never an
/// exponent and never a trailing `.0`.
///
/// Rust's `Display` is positional too and agrees on the digits, but writes `-0` for negative zero
/// exactly as Go does — so this is a thin wrapper today. It exists as a named function because
/// the two could drift and because the call site should say which `strconv` mode it means.
fn format_decimal(f: f64) -> String {
    format!("{f}")
}

/// Go's fixup for `'e'` output: `1e-07` becomes `1e-7`.
///
/// It fires only on a **negative** two-digit exponent whose first digit is zero, which is why
/// `1e+21` keeps its two digits and `1e-07` does not. Reproduced as the same narrow rewrite Go
/// performs rather than as general zero-stripping — `1e-107` must not become `1e-17`.
fn strip_exponent_zero(s: &str) -> String {
    let bytes = s.as_bytes();
    let n = bytes.len();
    if n >= 4 && bytes[n - 4] == b'e' && bytes[n - 3] == b'-' && bytes[n - 2] == b'0' {
        let mut out = String::with_capacity(n - 1);
        out.push_str(&s[..n - 2]);
        out.push(s.as_bytes()[n - 1] as char);
        return out;
    }
    s.to_string()
}

/// Go's `fmt.Sprintf("%v", f)` for a `float64`, which is `strconv.FormatFloat(f, 'g', -1, 64)`.
///
/// Not substitutable by Rust formatting. Rust's `Display` for `f64` never uses exponent form
/// (`1e21` prints as twenty-one digits) and its `LowerExp` always does; Go switches between them
/// on the **scientific exponent**: exponent form when `exp < -4 || exp >= 6`. So Go prints
/// `1000000.0` as `1e+06` and `100000.0` as `100000`, and the exponent always carries a sign and
/// at least two digits.
///
/// Measured against Go over 36 values, chosen from what `encoding/json` can produce — which is
/// the only source that matters, since a decoded JSON number is always a `float64`.
///
/// Negative zero is the one case not in the corpus; `-0` follows Go's `strconv`.
pub fn go_format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.to_string();
    }

    // Rust's LowerExp gives the shortest round-tripping digits in normalised form (`d.ddde±n`),
    // which is the same digit string Go's shortest mode computes. Only the layout differs.
    let sci = format!("{f:e}");
    let Some((mantissa, exp_str)) = sci.split_once('e') else {
        return sci;
    };
    let Ok(exp) = exp_str.parse::<i32>() else {
        return sci;
    };

    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();

    let body = if !(-4..6).contains(&exp) {
        let mut m = String::from(&digits[..1]);
        if digits.len() > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else if exp >= 0 {
        let point = exp as usize + 1;
        if digits.len() > point {
            format!("{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("{digits}{}", "0".repeat(point - digits.len()))
        }
    } else {
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    };

    if negative { format!("-{body}") } else { body }
}

/// Go's `fmt.Sprintf("%v", v)` for a value that came out of `encoding/json`.
///
/// Go's `%v` is not JSON: a string prints bare (so `"a b"` and two elements are
/// indistinguishable), a nil prints `<nil>`, a slice prints `[a b]` space-separated, and a map
/// prints `map[k:v]` with **sorted** keys. `StringifyMessageAttachmentFieldValue` writes this
/// into a post's props, so it is stored bytes, not a debug rendering.
///
/// Numbers route through [`go_format_float`] because `encoding/json` decodes every JSON number
/// into a `float64` — so Go prints `123456789` as `1.23456789e+08`, not as its digits.
pub fn go_format_v(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "<nil>".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => go_format_float(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(go_format_v).collect();
            format!("[{}]", parts.join(" "))
        }
        // serde_json::Map is a BTreeMap, so iteration is already in Go's sorted order.
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}:{}", go_format_v(v)))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// Port of `*multierror.Error` from `github.com/hashicorp/go-multierror`.
///
/// Not a Mattermost type — a Go library one, in the same category as [`go_time`] and
/// [`go_json_marshal`]. It is here because `integration_action.go` and `message_attachment.go`
/// validate with it, and their `IsValid` methods therefore behave unlike every other `IsValid`
/// in the tree: they **accumulate every failure** instead of returning the first, and they
/// return a plain `error` rather than an `*AppError`.
///
/// Two observables have to match Go, and both are measured rather than assumed:
///
/// - **The message order**, which is the order of the checks in the validator.
/// - **The `Error()` layout**, which is not a plain join and which changes wording between one
///   error and several: `"1 error occurred:\n\t* x\n\n"` versus
///   `"2 errors occurred:\n\t* x\n\t* y\n\n"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiError {
    errors: Vec<String>,
}

impl MultiError {
    pub fn new() -> Self {
        Self::default()
    }

    /// Go's `multierror.Append`.
    pub fn push(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    /// Go's `multierror.Prefix`. Applied to a `*multierror.Error` it prefixes **each** contained
    /// error and keeps them flat — it does not nest — so two option failures become two
    /// prefixed messages in the parent, not one. The separator is a single space, and the
    /// callers supply a prefix already ending in `:`.
    #[must_use]
    pub fn prefixed(self, prefix: &str) -> Self {
        Self {
            errors: self
                .errors
                .into_iter()
                .map(|e| format!("{prefix} {e}"))
                .collect(),
        }
    }

    /// Merges another error list in, preserving order.
    pub fn extend(&mut self, other: MultiError) {
        self.errors.extend(other.errors);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// The accumulated messages, in the order the validator produced them.
    pub fn messages(&self) -> &[String] {
        &self.errors
    }

    /// Go's `ErrorOrNil`.
    pub fn into_result(self) -> Result<(), MultiError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl std::fmt::Display for MultiError {
    /// Go's `multierror.ListFormatFunc`, reproduced exactly — including the trailing blank
    /// line and the singular/plural split.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.errors.len() == 1 {
            return write!(f, "1 error occurred:\n\t* {}\n\n", self.errors[0]);
        }
        write!(f, "{} errors occurred:\n\t", self.errors.len())?;
        let joined: Vec<String> = self.errors.iter().map(|e| format!("* {e}")).collect();
        write!(f, "{}\n\n", joined.join("\n\t"))
    }
}

impl std::error::Error for MultiError {}

/// Port of `model.ArrayToJSON` (utils.go:530).
///
/// A **nil** slice is `"null"`, not `"[]"` — which matters because `Post::is_valid` measures
/// this string's rune count against a cap. Go discards the marshal error and returns
/// `string(nil)`, i.e. `""`; `unwrap_or_default` is the same answer.
pub fn array_to_json(objmap: Option<&[String]>) -> String {
    match objmap {
        None => "null".to_string(),
        Some(a) => go_json_marshal(&a).unwrap_or_default(),
    }
}

/// Port of `model.StringInterfaceToJSON` (utils.go:585).
///
/// A **nil** map is `"null"`. Go's HTML escaping applies, so a single `<` in a value costs six
/// runes against `Post::is_valid`'s 800,000-rune props cap rather than one.
pub fn string_interface_to_json(objmap: Option<&StringInterface>) -> String {
    match objmap {
        None => "null".to_string(),
        Some(m) => go_json_marshal(m).unwrap_or_default(),
    }
}

/// Port of `model.StringArray` (utils.go:52).
pub type StringArray = Vec<String>;

pub const LOWERCASE_LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
pub const UPPERCASE_LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
pub const NUMBERS: &str = "0123456789";
pub const SYMBOLS: &str = " !\"\\#$%&'()*+,-./:;<=>?@[]^_`|~";
pub const BINARY_PARAM_KEY: &str = "MM_BINARY_PARAMETERS";
pub const NO_TRANSLATION: &str = "<untranslated>";
pub const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";
pub const MAX_PROP_SIZE_BYTES: usize = 1024 * 1024;

/// Port of `reservedName` (utils.go:685). Unexported in Go but consumed by
/// `IsReservedTeamName` in `team.go`.
pub const RESERVED_NAMES: [&str; 17] = [
    "admin",
    "api",
    "channel",
    "claim",
    "error",
    "files",
    "help",
    "landing",
    "login",
    "mfa",
    "oauth",
    "plug",
    "plugins",
    "post",
    "signup",
    "boards",
    "playbooks",
];

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// The z-base-32 alphabet from `utils.go:378`.
///
/// The doc comment on Go's `NewId` claims the result is `[A-Z0-9]`. It is not — this
/// alphabet is lowercase, and omits `l`, `v`, `2` and `0`. Do not "fix" a validator to
/// match that comment.
const ID_ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

/// Length of a Mattermost ID: 16 bytes of UUID, z-base-32 encoded without padding.
pub const ID_LENGTH: usize = 26;

/// Port of `model.NewId` (utils.go:383).
///
/// A random UUIDv4 (16 bytes) encoded as 26 z-base-32 characters. The RFC 4122 version and
/// variant bits are set, exactly as `uuid.NewRandom` does, so the character distribution at
/// offsets 9 and 12 matches Go's.
pub fn new_id() -> String {
    let mut raw = [0u8; 16];
    rand::rng().fill_bytes(&mut raw);
    raw[6] = (raw[6] & 0x0f) | 0x40; // version 4
    raw[8] = (raw[8] & 0x3f) | 0x80; // RFC 4122 variant
    zbase32_encode(&raw)
}

/// Port of `model.NewUsername` (utils.go:388). Prefixed so the result is a valid username,
/// which may not start with a digit.
pub fn new_username() -> String {
    format!("a{}", new_id())
}

/// Port of `model.NewRandomString` (utils.go:403).
///
/// Mirrors Go's sizing exactly — `1 + length*5/8` random bytes, encoded, then truncated to
/// `length` — so the entropy per character (5 bits) is identical.
pub fn new_random_string(length: usize) -> String {
    let mut data = vec![0u8; 1 + (length * 5 / 8)];
    rand::rng().fill_bytes(&mut data);
    let encoded = zbase32_encode(&data);
    encoded.chars().take(length).collect()
}

/// z-base-32, no padding — the encoding half of `base32.NewEncoding(...)` at utils.go:378.
fn zbase32_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;

    for &byte in input {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(ID_ALPHABET[index] as char);
            buffer &= (1 << bits) - 1;
        }
    }

    if bits > 0 {
        // Trailing partial group is left-aligned and zero-filled, matching NoPadding.
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(ID_ALPHABET[index] as char);
    }

    out
}

/// Port of `unicode.IsLetter` — Unicode general category `L`.
///
/// Not `char::is_alphabetic()`: that is the Alphabetic *property*, which also includes
/// `Other_Alphabetic` (combining marks such as U+0345), so it accepts identifiers Go
/// rejects. The behavioural oracle caught this; see `is_valid_id_matches_go`.
fn is_go_letter(c: char) -> bool {
    use unicode_general_category::GeneralCategory::*;
    matches!(
        unicode_general_category::get_general_category(c),
        UppercaseLetter | LowercaseLetter | TitlecaseLetter | ModifierLetter | OtherLetter
    )
}

/// Port of `unicode.IsNumber` — Unicode general category `N`.
fn is_go_number(c: char) -> bool {
    use unicode_general_category::GeneralCategory::*;
    matches!(
        unicode_general_category::get_general_category(c),
        DecimalNumber | LetterNumber | OtherNumber
    )
}

/// Port of `model.IsValidHTTPURL` (utils.go:790).
///
/// Go is two checks: a literal `http://` or `https://` prefix, then `net/url.ParseRequestURI`
/// succeeding with a non-empty `Scheme` and `Host`. This is now those two lines, delegating to
/// [`crate::go_url::parse_request_uri`].
///
/// It was not always. [D-003] shipped a hand-written predicate reproducing the same grammar,
/// because no parser existed; the corpus that verified it — 136 hand-picked inputs, 2,881
/// generated ones and four exhaustive 0..127 byte sweeps — now verifies the *parser* instead,
/// which is a much stronger use of the same 3,529 cases. The behaviour below is unchanged and
/// every one of them still passes.
///
/// Measured against Go over 136 hand-picked inputs, a 2,881-case generated corpus and four
/// exhaustive 0..127 byte sweeps. The traps, each of which a plausible port gets wrong:
///
/// - **The prefix test is case- and position-sensitive** (`strings.Index(...) != 0`), so
///   `HTTP://x` and `" http://x"` are both rejected.
/// - **`ParseRequestURI` does not strip a `#fragment`**, unlike `Parse`. So `http://x#f` is
///   *invalid* — the `#` lands in the host, where it is not a legal byte — while
///   `http://x/#f` is fine, because there the `#` is in the path.
/// - **Three positions have three different rules.** The host is validated against a character
///   class; the path is checked only for well-formed `%` escapes; the query is not checked at
///   all, so `?q=%zz` is valid. Control bytes are rejected everywhere, by a check over the
///   whole raw string before parsing starts.
/// - **`Host` includes the port**, so `http://:1` and even `http://:` are valid: the host name
///   is empty but `Host` is not.
/// - **A bracketed host must be a real IPv6 address.** `[::1]` and `[::ffff:1.2.3.4]` pass;
///   `[abc]`, `[]` and `[1.2.3.4]` do not.
/// - **A `%` escape in the host is rejected unless it encodes a byte >= 0x80, or is `%25`.**
///   That is the reverse of the usual intuition: `%80` is fine and `%41` is not.
pub fn is_valid_http_url(raw_url: &str) -> bool {
    if !raw_url.starts_with("http://") && !raw_url.starts_with("https://") {
        return false;
    }
    match parse_request_uri(raw_url) {
        Ok(url) => !url.scheme.is_empty() && !url.host.is_empty(),
        Err(_) => false,
    }
}

/// Port of `model.IsValidId` (utils.go:802).
///
/// Note this does **not** check the z-base-32 alphabet: Go accepts any 26-byte string whose
/// runes are all Unicode letters or numbers. The length test is on **bytes**, so a 26-byte
/// multi-byte string has fewer than 26 runes and still passes the length check.
pub fn is_valid_id(value: &str) -> bool {
    if value.len() != ID_LENGTH {
        return false;
    }
    value.chars().all(|c| is_go_letter(c) || is_go_number(c))
}

// ---------------------------------------------------------------------------
// Time — Go stores epoch milliseconds as i64 everywhere on the wire.
// ---------------------------------------------------------------------------

/// Port of `model.GetMillis` (utils.go:448).
pub fn get_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// Port of `model.GetMillisForTime` (utils.go:453).
pub fn get_millis_for_time<Tz: TimeZone>(this_time: &DateTime<Tz>) -> i64 {
    this_time.timestamp_millis()
}

/// Port of `model.GetTimeForMillis` (utils.go:458).
///
/// **Returns local time, not UTC.** Go's `time.UnixMilli` attaches `time.Local`, and callers
/// that read calendar fields off the result — `GetStartOfDayMillis`, `GetEndOfDayMillis` —
/// therefore produce answers that depend on the server's timezone. The behavioural oracle
/// caught this: for `1700000000000` the Go server (UTC+05:30) reports the start of Nov 15,
/// where a UTC reading gives Nov 14.
///
/// This is faithful to Go, not to good sense. Call `.with_timezone(&Utc)` when an absolute
/// instant is what you want.
///
/// Returns `None` outside chrono's representable range; Go's `time.UnixMilli` cannot fail,
/// so that is a widening of the contract, not a behavioural difference for real timestamps.
pub fn get_time_for_millis(millis: i64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(millis).map(|t| t.with_timezone(&Local))
}

/// Port of `model.PadDateStringZeros` (utils.go:463).
///
/// **The length test is in bytes, not characters.** Go's `len(part) == 1` counts bytes, so a
/// single Arabic-Indic digit — two bytes — is left unpadded and the date goes on to fail its
/// parse. An earlier version of this function counted `chars()` and padded it, which is the one
/// input where the two disagree; the `pad_date_string_zeros` section of
/// `fixtures/behaviour_search_params.json` is what caught it.
pub fn pad_date_string_zeros(date_string: &str) -> String {
    date_string
        .split('-')
        .map(|part| {
            if part.len() == 1 {
                format!("0{part}")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Port of `model.GetStartOfDayMillis` (utils.go:475).
///
/// The calendar date is taken from `this_time` **in its own zone** and then reinterpreted at
/// `tz_offset_seconds`, which is what Go's `time.Date(y, m, d, ..., fixedZone)` does.
///
/// **The offset is not range-checked, deliberately.** `time.FixedZone` accepts any `int` of
/// seconds, and `SearchParams.TimeZoneOffset` arrives from a client, so `86400` and `1000000`
/// are reachable values that Go answers for. An earlier version of this function built a
/// `chrono::FixedOffset`, which stops at ±86399 and returned `None` for all of them; the
/// `day_millis` section of `fixtures/behaviour_search_params.json` is what caught it. The
/// arithmetic below has no such limit.
pub fn get_start_of_day_millis<Tz: TimeZone>(
    this_time: &DateTime<Tz>,
    tz_offset_seconds: i64,
) -> Option<i64> {
    day_millis_at(this_time, 0, tz_offset_seconds)
}

/// Port of `model.GetEndOfDayMillis` (utils.go:482).
///
/// Go builds 23:59:59.999999999; `UnixMilli` truncates the nanoseconds, so the result is exactly
/// `GetStartOfDayMillis` plus 86,399,999 — a fixed zone has no DST, so no day is a different
/// length. Measured across the whole `day_millis` corpus, including pre-epoch dates where the
/// truncation direction is easy to get wrong.
pub fn get_end_of_day_millis<Tz: TimeZone>(
    this_time: &DateTime<Tz>,
    tz_offset_seconds: i64,
) -> Option<i64> {
    day_millis_at(this_time, MILLIS_TO_END_OF_DAY, tz_offset_seconds)
}

/// 23:59:59.999 after midnight, in milliseconds.
const MILLIS_TO_END_OF_DAY: i64 = 86_399_999;

/// The shared body of the two helpers above: midnight UTC on `this_time`'s calendar date, moved
/// `into_day` milliseconds forward, then shifted by the offset.
///
/// Returns `None` only for a date outside chrono's range or on arithmetic overflow. Go cannot
/// fail here, so that is a widening of the contract rather than a behavioural difference — no
/// reachable `SearchParams` produces one.
fn day_millis_at<Tz: TimeZone>(
    this_time: &DateTime<Tz>,
    into_day: i64,
    tz_offset_seconds: i64,
) -> Option<i64> {
    let midnight_utc =
        NaiveDate::from_ymd_opt(this_time.year(), this_time.month(), this_time.day())?
            .and_hms_opt(0, 0, 0)?
            .and_utc()
            .timestamp_millis();

    midnight_utc
        .checked_add(into_day)?
        .checked_sub(tz_offset_seconds.checked_mul(1_000)?)
}

// ---------------------------------------------------------------------------
// Identifier validation
// ---------------------------------------------------------------------------

/// Compiles a pattern that is a compile-time constant.
///
/// Returns `None` only if the pattern is malformed — which `regexes_compile` asserts cannot
/// happen. Callers then fail closed (validation returns `false`) instead of panicking, which
/// keeps library code free of `expect` per CLAUDE.md.
fn compile(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

static VALID_ALPHA_NUM: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^[a-z0-9]+([a-z\-0-9]+|(__)?)[a-z0-9]+$"));
static VALID_ALPHA_NUM_HYPHEN_UNDERSCORE: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^[a-z0-9]+([a-z\-_0-9]+|(__)?)[a-z0-9]+$"));
static VALID_SIMPLE_ALPHA_NUM: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^[a-z0-9]+([a-z\-_0-9]+|(__)?)[a-z0-9]*$"));
static VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^[a-zA-Z0-9\-_]+$"));
static VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE_PLUS: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^[a-zA-Z0-9+_-]+$"));

/// Port of `model.validHashtag` (utils.go:744). Go: `^(#\pL[\pL\d\-_.]*[\pL\d])$`.
///
/// `\d` is spelled `[0-9]` here on purpose — see [`is_valid_hashtag`].
static VALID_HASHTAG: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"^(#\p{L}[\p{L}0-9\-_.]*[\p{L}0-9])$"));

/// Port of `model.hashtagStart` (utils.go:746). Go: `^#{2,}`.
static HASHTAG_START: LazyLock<Option<Regex>> = LazyLock::new(|| compile(r"^#{2,}"));

fn matches(re: &LazyLock<Option<Regex>>, s: &str) -> bool {
    re.as_ref().is_some_and(|re| re.is_match(s))
}

/// Port of `model.validHashtag`'s only use (utils.go:763) — is this word a hashtag?
///
/// **Go's `\d` is ASCII and Rust's is not**, so the pattern is transcribed with `[0-9]`. Go's
/// `regexp` implements the Perl classes over ASCII only, while the `regex` crate's `\d` is
/// `\p{Nd}` — which would make `#a٣` a valid hashtag here and not in Go. Measured over 169
/// codepoints in `fixtures/behaviour_search_params.json`; see [`crate::search_params`] for the
/// same trap in the two term-trimming patterns.
///
/// Requires at least three characters: `#`, a letter, and a final letter or digit. `#a` is
/// **not** a hashtag, which is why a one-letter tag searches as a plain term.
pub fn is_valid_hashtag(word: &str) -> bool {
    matches(&VALID_HASHTAG, word)
}

/// Port of `hashtagStart.ReplaceAllString(word, "#")` (utils.go:761) — collapses a run of two or
/// more leading `#` into one. Anchored, so at most one replacement happens.
pub fn collapse_leading_hashes(word: &str) -> Cow<'_, str> {
    match HASHTAG_START.as_ref() {
        Some(re) => re.replace(word, "#"),
        None => Cow::Borrowed(word),
    }
}

/// Port of `model.isValidAlphaNum` (utils.go:717). Unexported in Go; used by `team.go`.
pub fn is_valid_alpha_num(s: &str) -> bool {
    matches(&VALID_ALPHA_NUM, s)
}

/// Port of `model.IsValidAlphaNumHyphenUnderscore` (utils.go:721).
///
/// `with_format = true` selects the stricter pattern that also constrains the first and last
/// character; `false` selects the plain character-class check.
pub fn is_valid_alpha_num_hyphen_underscore(s: &str, with_format: bool) -> bool {
    if with_format {
        matches(&VALID_ALPHA_NUM_HYPHEN_UNDERSCORE, s)
    } else {
        matches(&VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE, s)
    }
}

/// Port of `model.IsValidAlphaNumHyphenUnderscorePlus` (utils.go:728).
pub fn is_valid_alpha_num_hyphen_underscore_plus(s: &str) -> bool {
    matches(&VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE_PLUS, s)
}

/// Port of the `validSimpleAlphaNum` half of `model.IsValidChannelIdentifier`
/// (utils.go:705). See [`crate::channel::is_valid_channel_identifier`] for the whole check.
pub fn is_valid_simple_alpha_num(s: &str) -> bool {
    matches(&VALID_SIMPLE_ALPHA_NUM, s)
}

// ---------------------------------------------------------------------------
// Email
// ---------------------------------------------------------------------------

/// RFC 5322 `atext`, extended per RFC 6532.
///
/// Go's `net/mail` treats any non-ASCII rune as valid atext, so `日本@example.com` and even
/// `a\u{00A0}b@x.com` (non-breaking space) are accepted addresses. Measured, not assumed.
fn is_atext(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '/'
                | '='
                | '?'
                | '^'
                | '_'
                | '`'
                | '{'
                | '|'
                | '}'
                | '~'
        )
        || !c.is_ascii()
}

/// `dot-atom` — one or more non-empty atoms joined by single dots.
///
/// Rejects a leading dot, a trailing dot, and consecutive dots, because each produces an
/// empty atom.
fn is_dot_atom(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|atom| !atom.is_empty() && atom.chars().all(is_atext))
}

/// Port of `model.IsValidEmail` (utils.go:655).
///
/// Go composes three checks, and the combination is far narrower than RFC 5322:
///
/// 1. `isLower` — the input must already equal its own lowercasing.
/// 2. `mail.ParseAddress` must succeed **and** return an `Address` equal to the input
///    verbatim. That single equality does most of the work: it rejects display names, angle
///    brackets, comments, and every quoted local part, because the parser normalises those
///    and the result then differs from the input.
/// 3. At most one `@`.
///
/// What survives is exactly `dot-atom "@" ( dot-atom / "[" ip "]" )`. Note that the bracketed
/// domain form is validated as an **IP address**, not as free `dtext`: `a@[::1]` is accepted
/// while `a@[abc]` and `a@[1.2.3]` are not. `a@[fe80::1%eth0]` is rejected too — Go's parser
/// does not take a zone. The `IPv6:` prefix Go also accepts is unreachable here, since its
/// uppercase fails check 1.
///
/// Every claim above is asserted against Go's own answers over a 128-case corpus; see
/// `go_parity::is_valid_email_matches_go`.
pub fn is_valid_email(input: &str) -> bool {
    // `strings.ToLower`, not `str::to_lowercase` — see [`go_to_lower`].
    if go_to_lower(input) != input {
        return false;
    }

    // splitn(3) distinguishes "no @", "one @" and "more than one @" in a single pass.
    let parts: Vec<&str> = input.splitn(3, '@').collect();
    if parts.len() != 2 {
        return false;
    }
    let (local, domain) = (parts[0], parts[1]);

    is_dot_atom(local) && is_email_domain(domain)
}

fn is_email_domain(domain: &str) -> bool {
    if let Some(literal) = domain.strip_prefix('[').and_then(|d| d.strip_suffix(']')) {
        return literal.parse::<std::net::IpAddr>().is_ok();
    }
    is_dot_atom(domain)
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Port of `model.ClearMentionTags` (utils.go:784).
pub fn clear_mention_tags(post: &str) -> String {
    post.replace("<mention>", "").replace("</mention>", "")
}

/// Port of `model.RemoveDuplicateStrings` (utils.go:818). Sorts, then de-duplicates.
pub fn remove_duplicate_strings(input: &mut Vec<String>) {
    input.sort();
    input.dedup();
}

/// Port of `model.RemoveDuplicateStringsNonSort` (utils.go:838). Preserves first-seen order.
pub fn remove_duplicate_strings_non_sort(input: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(input.len());
    input
        .iter()
        .filter(|item| seen.insert(item.as_str()))
        .cloned()
        .collect()
}

/// Port of `model.GetPreferredTimezone` (utils.go:850).
///
/// A missing key yields `""`, matching Go's zero value for an absent map entry.
pub fn get_preferred_timezone(timezone: &StringMap) -> &str {
    let key = if timezone.get("useAutomaticTimezone").map(String::as_str) == Some("true") {
        "automaticTimezone"
    } else {
        "manualTimezone"
    };
    timezone.get(key).map_or("", String::as_str)
}

// ---------------------------------------------------------------------------
// Etag
// ---------------------------------------------------------------------------

/// `model.CurrentVersion` (version.go:155), which is `versions[0]` — the newest entry in the
/// release list at version.go:15.
///
/// Re-exported from [`crate::version`], which now owns it. It was transcribed here first
/// because every `Etag` in the tree prefixes it and `Etag` is the whole content of
/// `channel_list.go`; the alias stays so the `utils::CURRENT_VERSION` path keeps working, but
/// there is only one definition.
pub use crate::version::CURRENT_VERSION;

/// Port of `model.Etag` (utils.go:732).
///
/// Go is variadic over `any` and renders each part with `%v`; Rust takes a slice of
/// `Display`, which agrees with `%v` for every type a call site actually passes — strings,
/// integers and bools (`true`/`false` in both). It would *not* agree for floats, where Go's
/// `%v` is `%g`; no call site passes one.
///
/// Note the parts are joined with `.` and nothing is escaped, so a part containing a dot
/// silently changes the component count. `Team::etag` on a zero team yields `11.11.0..0` — an
/// empty component is normal, not a bug.
pub fn etag(parts: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::from(CURRENT_VERSION);
    for part in parts {
        out.push('.');
        out.push_str(&part.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// Go's encoding/json, for the cases where its *output* is load-bearing
// ---------------------------------------------------------------------------

/// The `map[string]string` case of `model.ToJSON` (utils.go:611), which is
/// `json.Marshal` with the error discarded.
///
/// `serde_json::to_string` is **not** a drop-in substitute when the result is measured rather
/// than transmitted, and `ChannelMember`'s 800,000-rune notify-props check measures it. Three
/// differences, all verified against Go:
///
/// 1. **Key order.** Go sorts map keys by byte value; a `HashMap` iterates arbitrarily.
/// 2. **HTML escaping is on.** `<`, `>` and `&` become `<`, `>`, `&` — one rune
///    becomes six, so an adversarial notify-props value counts six times over in Go and once
///    in serde_json.
/// 3. **U+2028 / U+2029 are escaped**, though both are legal raw in JSON.
///
/// A nil map marshals to `null`, not `{}`, hence the `Option`.
pub fn go_json_marshal_string_map(map: Option<&StringMap>) -> String {
    let Some(map) = map else {
        return "null".to_string();
    };

    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable(); // `str: Ord` is byte-wise, matching Go's sort.Strings.

    let mut out = String::from("{");
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        go_json_quote(key, &mut out);
        out.push(':');
        // The key came from the map, so the lookup cannot miss.
        go_json_quote(map.get(*key).map_or("", String::as_str), &mut out);
    }
    out.push('}');
    out
}

/// Go's `encodeState.string` with `escapeHTML` left at its default of true.
fn go_json_quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // The HTML trio and the two separators are the escapes serde_json does not make.
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                // Remaining C0 controls have no shorthand; Go writes \u00XX lowercase.
                let n = c as u32;
                out.push_str("\\u00");
                out.push(HEX_LOWER[(n >> 4) as usize]);
                out.push(HEX_LOWER[(n & 0xf) as usize]);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

const HEX_LOWER: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// `json.Marshal` for any `Serialize` value, with Go's default HTML escaping.
///
/// [`go_json_marshal_string_map`] above solves this for one shape (`map[string]string`) by
/// building the JSON itself. That does not generalise to structs, and it needs to: a marshalled
/// `CustomStatus` is **stored** in `User.Props["customStatus"]`, in a column the Go server reads
/// and writes too. If we write `<b>` where Go writes `<b>`, the two servers disagree
/// byte-for-byte about a value that is later compared as a string.
///
/// serde_json and Go's `encoding/json` differ on exactly five characters — `<`, `>`, `&`,
/// U+2028 and U+2029 — so rather than reimplementing a serializer this re-escapes serde_json's
/// output. Every other escape (`\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`, and `\u00XX` in
/// lower-case hex for the remaining C0 controls) already agrees.
///
/// **Structs only — do not pass a `HashMap`.** This adjusts escaping, never key order. Struct
/// fields serialize in declaration order in both languages, so those agree. Go sorts *map* keys
/// by byte value, and a `HashMap` serializes in iteration order, which is neither sorted nor
/// stable across runs. Use [`go_json_marshal_string_map`] for a [`StringMap`]; it sorts. A
/// `BTreeMap` or a `serde_json::Map` would also be safe, since both are already ordered.
pub fn go_json_marshal<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    Ok(go_json_escape(&serde_json::to_string(value)?))
}

/// Port of Go's `strings.ToLower`.
///
/// **Not the same function as `str::to_lowercase`**, which is why this exists. Go applies
/// Unicode's *simple* (1:1) lowercase mapping to each rune; Rust applies the *full* (1:many)
/// mapping and implements the Final_Sigma context rule. Measured, they disagree twice:
///
/// | input | Go | `str::to_lowercase` |
/// |---|---|---|
/// | `İ` (U+0130) | `i` | `i` + U+0307 |
/// | `ΟΔΟΣ` | `οδοσ` | `οδος` |
///
/// Taking the first character of Rust's full mapping reproduces the simple mapping, and the
/// character-level API has no context so it cannot apply Final_Sigma. Pinned over 30 inputs by
/// `go_parity::go_to_lower_matches_go`.
///
/// Anywhere Go calls `strings.ToLower` on user input that is later **stored or compared**, this
/// is the function to use — a team slug or an emoji name that lowercases differently in the two
/// servers is a divergence on a shared database.
pub fn go_to_lower(s: &str) -> String {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Applies Go's five extra string escapes to already-serialized JSON.
///
/// Walks the document tracking whether it is inside a string literal, so a `<` appearing in
/// structural position — which valid JSON has no way to produce — is never touched, and a
/// backslash-escaped character is never re-interpreted.
pub fn go_json_escape(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut after_backslash = false;

    for c in json.chars() {
        if !in_string {
            in_string = c == '"';
            out.push(c);
            continue;
        }
        if after_backslash {
            after_backslash = false;
            out.push(c);
            continue;
        }
        match c {
            '\\' => {
                after_backslash = true;
                out.push(c);
            }
            '"' => {
                in_string = false;
                out.push(c);
            }
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Port of `model.SanitizeUnicode` (utils.go:859).
///
/// Drops the W3C "characters not suitable for use with markup" blocklist.
pub fn sanitize_unicode(s: &str) -> String {
    s.chars().filter(|c| !is_blocklisted(*c)).collect()
}

/// Port of `model.filterBlocklist` (utils.go:865).
fn is_blocklisted(r: char) -> bool {
    matches!(r,
        '\u{0340}' | '\u{0341}'                                     // deprecated grave/acute clones
        | '\u{17A3}' | '\u{17D3}'                                   // deprecated Khmer
        | '\u{2028}' | '\u{2029}'                                   // line/paragraph separators
        | '\u{202A}'..='\u{202E}'                                   // BIDI embedding controls
        | '\u{206A}'..='\u{206F}'                                   // deprecated format controls
        | '\u{FFF9}'..='\u{FFFB}'                                   // interlinear annotation
        | '\u{FEFF}'                                                // byte order mark
        | '\u{FFFC}'                                                // object replacement
        | '\u{1D173}'..='\u{1D17A}'                                 // musical notation scoping
        | '\u{E0000}'..='\u{E007F}'                                 // language tag code points
    )
}

/// Port of `model.LimitRunes` (utils.go:922). Returns the string and whether it was cut.
pub fn limit_runes(s: &str, max_runes: usize) -> (String, bool) {
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(max_runes).collect();
    // Anything left in the iterator means we cut the string short.
    (out, chars.next().is_some())
}

/// Port of `model.LimitBytes` (utils.go:933).
///
/// **Deliberate divergence:** Go slices at exactly `max_bytes`, which can split a multi-byte
/// character and yield a `string` holding invalid UTF-8. A Rust `String` cannot represent
/// that, so this truncates at the nearest char boundary at or below `max_bytes`. For ASCII
/// input — every caller in the Go tree today — the results are identical.
pub fn limit_bytes(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    (truncate_at_boundary(s, max_bytes).to_string(), true)
}

fn truncate_at_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// AppError
// ---------------------------------------------------------------------------

const MAX_ERROR_LENGTH: usize = 1024;

/// The result type Go's `*AppError`-returning validators map to.
///
/// The error is boxed deliberately: `AppError` carries five `String`s and is ~200 bytes, so an
/// unboxed `Result<(), AppError>` would make every success path pay for the failure path. A
/// validation failure is the exceptional branch, so the allocation belongs there.
pub type AppResult<T = ()> = Result<T, Box<AppError>>;

fn is_zero_i32(n: &i32) -> bool {
    *n == 0
}

/// Port of `model.AppError` (utils.go:232) — the JSON error envelope for every API response.
///
/// `where_` and `skip_translation` carry `json:"-"` in Go and are `#[serde(skip)]` here.
/// `request_id` and `status_code` carry `omitempty`; per CLAUDE.md they stay concrete types
/// with a skip predicate rather than becoming `Option`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AppError {
    #[serde(rename = "id")]
    pub id: String,

    /// Shown to the end user, without debugging information.
    #[serde(rename = "message")]
    pub message: String,

    /// Internal detail for developers.
    #[serde(rename = "detailed_error")]
    pub detailed_error: String,

    #[serde(
        rename = "request_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub request_id: String,

    #[serde(rename = "status_code", default, skip_serializing_if = "is_zero_i32")]
    pub status_code: i32,

    /// `Struct.Func` where the error occurred. Never serialised (`json:"-"`).
    #[serde(skip)]
    pub where_: String,

    /// Never serialised (`json:"-"`).
    #[serde(skip)]
    pub skip_translation: bool,

    /// i18n interpolation params. Unexported in Go.
    #[serde(skip)]
    pub params: Option<HashMap<String, serde_json::Value>>,

    #[serde(skip)]
    wrapped: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl AppError {
    /// Port of `model.NewAppError` (utils.go:365).
    ///
    /// Go calls `Translate(translateFunc)` here. With no i18n bundle registered — the state
    /// before `AppErrorInit` runs — `Translate` sets `Message = Id`, which is what this
    /// reproduces. Translation is a server concern and lands with the i18n layer.
    pub fn new(
        where_: impl Into<String>,
        id: impl Into<String>,
        params: Option<HashMap<String, serde_json::Value>>,
        details: impl Into<String>,
        status: i32,
    ) -> Self {
        let id = id.into();
        Self {
            message: id.clone(), // Go: Message starts as Id, then Translate overwrites it.
            id,
            params,
            where_: where_.into(),
            detailed_error: details.into(),
            status_code: status,
            skip_translation: false,
            request_id: String::new(),
            wrapped: None,
        }
    }

    /// Port of `(*AppError).Wrap` (utils.go:334).
    #[must_use]
    pub fn wrap(mut self, err: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.wrapped = Some(Box::new(err));
        self
    }

    /// Port of `(*AppError).WipeDetailed` (utils.go:339).
    pub fn wipe_detailed(&mut self) {
        self.wrapped = None;
        self.detailed_error.clear();
    }

    /// Port of `(*AppError).wrappedToDetailed` (utils.go:318) — the folded value only, so
    /// `to_json` need not mutate and restore `self` the way Go does.
    fn detailed_with_wrapped(&self) -> Option<String> {
        let wrapped = self.wrapped.as_ref()?;
        if self.detailed_error.is_empty() {
            Some(wrapped.to_string())
        } else {
            Some(format!("{}, {}", self.detailed_error, wrapped))
        }
    }

    /// Port of `(*AppError).ToJSON` (utils.go:305).
    ///
    /// The wrapped error is folded into `detailed_error` for the wire, exactly as Go does
    /// before marshalling. Built from this type's own `Serialize` so the two cannot drift.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        if let (Some(object), Some(detailed)) =
            (value.as_object_mut(), self.detailed_with_wrapped())
        {
            object.insert(
                "detailed_error".to_string(),
                serde_json::Value::String(detailed),
            );
        }
        serde_json::to_string(&value)
    }
}

impl fmt::Display for AppError {
    /// Port of `(*AppError).Error` (utils.go:246).
    ///
    /// Truncation is at the nearest char boundary at or below 1024 bytes; Go cuts at exactly
    /// 1024, which can split a multi-byte character.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();

        if !self.where_.is_empty() {
            out.push_str(&self.where_);
            out.push_str(": ");
        }

        let translated = self.message != NO_TRANSLATION;
        if translated {
            out.push_str(&self.message);
        }

        if !self.detailed_error.is_empty() {
            if translated {
                out.push_str(", ");
            }
            out.push_str(&self.detailed_error);
        }

        if let Some(wrapped) = &self.wrapped {
            out.push_str(", ");
            out.push_str(&wrapped.to_string());
        }

        if out.len() > MAX_ERROR_LENGTH {
            write!(f, "{}...", truncate_at_boundary(&out, MAX_ERROR_LENGTH))
        } else {
            f.write_str(&out)
        }
    }
}

impl std::error::Error for AppError {
    /// Port of `(*AppError).Unwrap` (utils.go:330).
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.wrapped.as_ref().map(|e| e.as_ref() as _)
    }
}

// ---------------------------------------------------------------------------
// go_time — Go's `time.Time` JSON encoding, and its calendar arithmetic
// ---------------------------------------------------------------------------

/// Go's `time.Time` as `encoding/json` writes and reads it, plus the two `time` functions that
/// do calendar arithmetic in a named zone ([`go_time::date_in_zone`], [`go_time::add_date_days`]).
///
/// Almost every timestamp in the model package is an `int64` of epoch milliseconds, but a
/// handful of types (`CustomStatus.ExpiresAt` first among them) hold a real `time.Time`, which
/// marshals to RFC 3339. This module is a port of `(Time).MarshalJSON` and
/// `(Time).UnmarshalJSON`, not of any single Mattermost source file — it lives here for the
/// same reason [`go_json_marshal_string_map`] does: it is `encoding/json` behaviour that more
/// than one model type needs.
///
/// **chrono's own serde impl is not a substitute.** Four things differ:
///
/// | | Go | chrono default |
/// |---|---|---|
/// | `.5` seconds | `.5` | `.500` (`SecondsFormat::AutoSi` pads to 3/6/9) |
/// | zero fraction | omitted | omitted |
/// | zero offset | `Z` | `+00:00` for `FixedOffset` |
/// | `null` | leaves the value untouched | error |
///
/// The offset is preserved rather than normalised: Go re-emits whatever zone the value holds,
/// so `12:00:00+05:30` round-trips as `+05:30` and **not** as `06:30:00Z`. That is why the
/// Rust type is `DateTime<FixedOffset>` and not `DateTime<Utc>`.
///
/// Every claim here is pinned by `fixtures/behaviour_custom_status.json` (`time_marshal`,
/// `time_unmarshal`) over 51 cases.
pub mod go_time {
    use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike};
    use serde::de::{Error as DeError, Unexpected};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::LazyLock;

    /// Go's zero `time.Time`: January 1 of year 1, UTC.
    ///
    /// Built from a `const` naive value, so the fallible constructors are resolved at compile
    /// time — an impossible date would fail the build rather than panic at runtime.
    pub static ZERO: LazyLock<DateTime<FixedOffset>> =
        LazyLock::new(|| ZERO_NAIVE.and_utc().fixed_offset());

    const ZERO_NAIVE: NaiveDateTime = match NaiveDate::from_ymd_opt(1, 1, 1) {
        Some(date) => match date.and_hms_opt(0, 0, 0) {
            Some(dt) => dt,
            None => panic!("00:00:00 is a valid time"),
        },
        None => panic!("0001-01-01 is a valid date"),
    };

    /// Port of `(Time).IsZero` — true only for the exact zero value, offset included.
    pub fn is_zero(t: &DateTime<FixedOffset>) -> bool {
        t.naive_utc() == ZERO_NAIVE && t.offset().local_minus_utc() == 0
    }

    /// Port of `(Time).MarshalJSON` — the string body, without the surrounding quotes.
    ///
    /// `None` is Go's `"Time.MarshalJSON: year outside of range [0,9999]"`. The bound is on the
    /// year in the value's *own* zone, and it is exclusive at the top: 9999 marshals, 10000
    /// does not, and so does no negative year.
    pub fn format(t: &DateTime<FixedOffset>) -> Option<String> {
        let year = t.year();
        if !(0..10000).contains(&year) {
            return None;
        }

        let mut out = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            year,
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );

        // RFC3339Nano's ".999999999" means: drop trailing zeros, and drop the point too when
        // nothing survives. Go's own leap-second representation (nanosecond >= 1e9) cannot be
        // constructed by the parser, so `nanosecond()` is taken modulo a second the way Go's
        // formatter reads it.
        let nanos = t.nanosecond() % 1_000_000_000;
        if nanos != 0 {
            let frac = format!("{nanos:09}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }

        let offset = t.offset().local_minus_utc();
        if offset == 0 {
            out.push('Z');
        } else {
            let sign = if offset < 0 { '-' } else { '+' };
            let abs = offset.abs();
            out.push(sign);
            out.push_str(&format!("{:02}:{:02}", abs / 3600, (abs % 3600) / 60));
        }
        Some(out)
    }

    /// Port of `time.Parse(time.RFC3339, …)` as `(Time).UnmarshalJSON` reaches it.
    ///
    /// The grammar is strict and deliberately narrower than RFC 3339 itself:
    /// `YYYY-MM-DDTHH:MM:SS` with an optional `.` and one or more digits, then `Z` or `±HH:MM`.
    /// Measured against Go over 38 inputs:
    ///
    /// - **`T` and `Z` must be uppercase.** `2026-08-14t12:00:00z` is rejected, although RFC
    ///   3339 says the separators are case-insensitive.
    /// - **More than nine fractional digits is accepted and truncated**, not rounded and not
    ///   an error: `.1234567891` becomes `.123456789`.
    /// - **A zone offset must be `±HH:MM`** with `HH <= 23` and `MM <= 59`; `+0530` and
    ///   `+99:99` are both rejected, `+23:59` is accepted, and `+00:00`/`-00:00` collapse to
    ///   UTC so they re-marshal as `Z`.
    /// - **Calendar validity is enforced**: `2023-02-29` and `2026-02-30` are rejected,
    ///   `2024-02-29` is not. So is the leap second `23:59:60`.
    pub fn parse(s: &str) -> Option<DateTime<FixedOffset>> {
        let b = s.as_bytes();
        if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
            return None;
        }
        if b[13] != b':' || b[16] != b':' {
            return None;
        }

        let year = digits(&b[0..4])?;
        let month = digits(&b[5..7])?;
        let day = digits(&b[8..10])?;
        let hour = digits(&b[11..13])?;
        let minute = digits(&b[14..16])?;
        let second = digits(&b[17..19])?;

        let mut rest = &b[19..];

        // The fraction is only taken when the point is followed by at least one digit; a bare
        // "." falls through and then fails the zone check, which is how Go rejects "…00.Z".
        let mut nanos = 0u32;
        if rest.first() == Some(&b'.') && rest.get(1).is_some_and(u8::is_ascii_digit) {
            let run = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
            // Truncate to nanosecond precision, then scale a short run up.
            let taken = run.min(9);
            nanos = digits(&rest[1..=taken])?;
            for _ in taken..9 {
                nanos *= 10;
            }
            rest = &rest[1 + run..];
        }

        let offset_seconds = match rest {
            b"Z" => 0,
            [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
                let hours = digits(&[*h1, *h2])?;
                let minutes = digits(&[*m1, *m2])?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                let magnitude = (hours * 3600 + minutes * 60) as i32;
                if *sign == b'-' { -magnitude } else { magnitude }
            }
            _ => return None,
        };

        let date = NaiveDate::from_ymd_opt(year as i32, month, day)?;
        let naive = date.and_hms_nano_opt(hour, minute, second, nanos)?;
        let offset = FixedOffset::east_opt(offset_seconds)?;
        naive.and_local_timezone(offset).single()
    }

    /// Parses an exact-width ASCII digit run. Rejects signs, so `-026-08-14T…` fails on the
    /// year the way Go does.
    fn digits(b: &[u8]) -> Option<u32> {
        if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let mut out: u32 = 0;
        for c in b {
            out = out.checked_mul(10)?.checked_add(u32::from(c - b'0'))?;
        }
        Some(out)
    }

    /// `#[serde(with = "…::go_time")]` serializer.
    pub fn serialize<S: Serializer>(t: &DateTime<FixedOffset>, s: S) -> Result<S::Ok, S::Error> {
        match format(t) {
            Some(rendered) => s.serialize_str(&rendered),
            None => Err(serde::ser::Error::custom(
                "Time.MarshalJSON: year outside of range [0,9999]",
            )),
        }
    }

    /// `#[serde(with = "…::go_time")]` deserializer.
    ///
    /// Go's `UnmarshalJSON` returns early on `null` **without touching the receiver**, which a
    /// struct-level deserialize cannot express — there is no prior value to keep. The nearest
    /// faithful behaviour is the zero value, which is what a freshly allocated Go struct would
    /// have held. Noted in `docs/TECH_DEBT.md` as D-023.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<FixedOffset>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        let Some(raw) = raw else {
            return Ok(*ZERO);
        };
        parse(&raw)
            .ok_or_else(|| D::Error::invalid_value(Unexpected::Str(&raw), &"an RFC 3339 timestamp"))
    }

    // -- calendar arithmetic in a named zone ---------------------------------------------

    /// Port of `time.Date` — specifically its **normalisation**, which is the part that has no
    /// specification and therefore cannot be reasoned about.
    ///
    /// Turning a wall clock into an instant is ambiguous twice a year. Go's doc says only that
    /// "the choice of time zone, and therefore the time, is not guaranteed", so what a port has
    /// to reproduce is the implementation:
    ///
    /// ```text
    /// unix := <the wall clock read as if it were UTC>
    /// _, offset, start, end, _ := loc.lookup(unix)
    /// if offset != 0 {
    ///     switch utc := unix - int64(offset); {
    ///     case utc < start: _, offset, _, _, _ = loc.lookup(start - 1)
    ///     case utc >= end:  _, offset, _, _, _ = loc.lookup(end)
    ///     }
    ///     unix -= int64(offset)
    /// }
    /// ```
    ///
    /// The `start`/`end` boundaries are not reachable through any timezone crate, but they are
    /// not needed. `utc < start` says `utc` sits in the interval *before* the one holding
    /// `unix`, so `lookup(start - 1)` is `lookup(utc)`; `utc >= end` says it sits in the one
    /// *after*, so `lookup(end)` is `lookup(utc)` again. Both branches therefore reduce to "the
    /// offset at `utc`", and the whole function collapses to:
    ///
    /// ```text
    /// o0 = offset(unix); cand = unix - o0; o1 = offset(cand)
    /// if o1 == o0 { cand } else { unix - o1 }
    /// ```
    ///
    /// which also subsumes the `offset != 0` guard, since `o0 == 0` makes `cand == unix` and
    /// `o1 == o0`. The reduction assumes at most one transition between `unix` and `cand` —
    /// they are at most 26 hours apart, and no zone in tzdata changes offset twice inside a day.
    ///
    /// **`chrono`'s `LocalResult` is not a substitute**, and mapping its three arms onto a rule
    /// is where a plausible port goes wrong. There is no rule: because Go looks the offset up on
    /// the wall clock *read as a UTC instant*, which side of a transition it lands on is decided
    /// by the sign of the zone's own offset. Measured over 280 probes in
    /// `fixtures/behaviour_scheduled_post_recurrence.json`:
    ///
    /// | | negative-offset zone | positive-offset zone |
    /// |---|---|---|
    /// | skipped local time (`LocalResult::None`) | resolves **backwards**, before the gap | resolves **forwards**, after it |
    /// | repeated local time (`LocalResult::Ambiguous`) | the **earlier** instant | the **later** instant |
    ///
    /// So `Ambiguous(a, _) => a` — the mapping that reads as obviously right — is wrong for
    /// Europe/London, and `None` has no `LocalResult` answer to map at all.
    ///
    /// Returns `None` only on arithmetic that leaves `chrono`'s representable range.
    pub fn date_in_zone<T: chrono::TimeZone>(wall: NaiveDateTime, tz: &T) -> Option<DateTime<T>> {
        // Go's `unix`: the wall clock's calendar fields read as though they were UTC. It is not
        // an instant, and the arithmetic below is the only thing that turns it into one.
        let pseudo = wall.and_utc();

        let offset_at = |t: &DateTime<chrono::Utc>| {
            chrono::Offset::fix(&tz.offset_from_utc_datetime(&t.naive_utc())).local_minus_utc()
        };

        let o0 = offset_at(&pseudo);
        let candidate = pseudo.checked_sub_signed(chrono::TimeDelta::seconds(o0.into()))?;

        let o1 = offset_at(&candidate);
        let utc = if o1 == o0 {
            candidate
        } else {
            pseudo.checked_sub_signed(chrono::TimeDelta::seconds(o1.into()))?
        };

        Some(utc.with_timezone(tz))
    }

    /// Port of `(Time).AddDate(0, 0, days)`.
    ///
    /// Go's `AddDate` is `Date(y, m, d+days, h, mi, s, ns, loc)` — it adds **calendar** days and
    /// keeps the wall clock, then re-resolves through [`date_in_zone`]. That is not the same as
    /// adding `days * 86_400` seconds: across a DST boundary the two differ by the size of the
    /// shift, and across a skipped local hour the wall clock does not survive at all.
    pub fn add_date_days<T: chrono::TimeZone>(t: &DateTime<T>, days: u64) -> Option<DateTime<T>> {
        let wall = t.naive_local().checked_add_days(chrono::Days::new(days))?;
        date_in_zone(wall, &t.timezone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- encoding / IDs ----------------------------------------------------

    #[test]
    fn zbase32_encodes_known_vectors() {
        // 128 zero bits -> 26 groups all index 0.
        assert_eq!(zbase32_encode(&[0u8; 16]), "y".repeat(26));
        // 128 one bits -> 25 full groups at index 31 ('9'), then 3 bits left-aligned
        // to 0b11100 = 28 ('h').
        assert_eq!(
            zbase32_encode(&[0xffu8; 16]),
            format!("{}h", "9".repeat(25))
        );
    }

    #[test]
    fn zbase32_alphabet_is_the_go_one() {
        // Guards against transcription drift in the alphabet itself.
        assert_eq!(
            std::str::from_utf8(ID_ALPHABET),
            Ok("ybndrfg8ejkmcpqxot1uwisza345h769")
        );
        assert_eq!(ID_ALPHABET.len(), 32);
    }

    #[test]
    fn new_id_has_go_length_and_alphabet() {
        for _ in 0..100 {
            let id = new_id();
            assert_eq!(id.len(), ID_LENGTH, "id {id} not 26 bytes");
            assert!(
                id.bytes().all(|b| ID_ALPHABET.contains(&b)),
                "id {id} escaped the z-base-32 alphabet"
            );
        }
    }

    #[test]
    fn new_id_is_not_uppercase_despite_the_go_doc_comment() {
        // utils.go:380 claims [A-Z0-9]. It lies; the alphabet is lowercase.
        let id = new_id();
        assert_eq!(id, id.to_lowercase());
    }

    #[test]
    fn new_id_sets_uuid_version_and_variant_bits() {
        // Version 4 lands in the nibble encoded by characters 9-10 of the output; assert
        // via a round trip on the raw bytes instead of the string.
        for _ in 0..50 {
            let id = new_id();
            let raw = decode_for_test(&id);
            assert_eq!(raw[6] & 0xf0, 0x40, "version nibble wrong");
            assert_eq!(raw[8] & 0xc0, 0x80, "variant bits wrong");
        }
    }

    /// Test-only inverse of `zbase32_encode`.
    fn decode_for_test(s: &str) -> Vec<u8> {
        let mut buffer: u32 = 0;
        let mut bits: u32 = 0;
        let mut out = Vec::new();
        for c in s.bytes() {
            let index = ID_ALPHABET.iter().position(|&a| a == c).unwrap() as u32;
            buffer = (buffer << 5) | index;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push(((buffer >> bits) & 0xff) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        out
    }

    #[test]
    fn new_id_is_unique_across_1000_calls() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn new_username_is_prefixed_and_valid() {
        let name = new_username();
        assert_eq!(name.len(), ID_LENGTH + 1);
        assert!(name.starts_with('a'));
    }

    #[test]
    fn new_random_string_honours_length() {
        for length in [0, 1, 5, 16, 26, 64] {
            assert_eq!(new_random_string(length).chars().count(), length);
        }
    }

    // -- IsValidId ---------------------------------------------------------

    #[test]
    fn is_valid_id_accepts_generated_ids() {
        for _ in 0..100 {
            assert!(is_valid_id(&new_id()));
        }
    }

    #[test]
    fn is_valid_id_rejects_wrong_length() {
        assert!(!is_valid_id(""));
        assert!(!is_valid_id(&"a".repeat(25)));
        assert!(!is_valid_id(&"a".repeat(27)));
        assert!(is_valid_id(&"a".repeat(26)));
    }

    #[test]
    fn is_valid_id_rejects_non_alphanumeric() {
        let mut s = "a".repeat(25);
        s.push('-');
        assert!(!is_valid_id(&s));

        let mut s = "a".repeat(25);
        s.push(' ');
        assert!(!is_valid_id(&s));
    }

    #[test]
    fn is_valid_id_length_is_measured_in_bytes() {
        // 13 two-byte characters = 26 bytes but only 13 runes; Go's len() is bytes, so
        // this passes both the length check and the letter check.
        let s = "é".repeat(13);
        assert_eq!(s.len(), 26);
        assert!(is_valid_id(&s));
    }

    // -- time --------------------------------------------------------------

    #[test]
    fn millis_round_trip() {
        let millis = 1_700_000_000_123_i64;
        let time = get_time_for_millis(millis).unwrap();
        assert_eq!(get_millis_for_time(&time), millis);
    }

    #[test]
    fn get_millis_is_now_in_milliseconds() {
        let now = get_millis();
        // Sanity bound: after 2020-01-01 and before 2100-01-01.
        assert!(now > 1_577_836_800_000);
        assert!(now < 4_102_444_800_000);
    }

    #[test]
    fn pad_date_string_zeros_pads_single_digits() {
        assert_eq!(pad_date_string_zeros("2019-1-2"), "2019-01-02");
        assert_eq!(pad_date_string_zeros("2019-11-12"), "2019-11-12");
        assert_eq!(pad_date_string_zeros("2019-1-12"), "2019-01-12");
        assert_eq!(pad_date_string_zeros(""), "");
    }

    #[test]
    fn start_and_end_of_day_bracket_the_day() {
        let time = get_time_for_millis(1_700_000_000_000).unwrap();
        let start = get_start_of_day_millis(&time, 0).unwrap();
        let end = get_end_of_day_millis(&time, 0).unwrap();
        assert!(start < end);
        // 23:59:59.999 - 00:00:00.000
        assert_eq!(end - start, 86_399_999);
    }

    #[test]
    fn start_of_day_respects_the_offset() {
        let time = get_time_for_millis(1_700_000_000_000).unwrap();
        let utc = get_start_of_day_millis(&time, 0).unwrap();
        let plus_one_hour = get_start_of_day_millis(&time, 3600).unwrap();
        // Midnight one hour east of UTC is one hour earlier in absolute terms.
        assert_eq!(utc - plus_one_hour, 3_600_000);
    }

    // -- identifier validation ---------------------------------------------

    #[test]
    fn regexes_compile() {
        // The library fails closed rather than panicking on a bad pattern; this asserts
        // that path is never actually taken.
        assert!(VALID_ALPHA_NUM.is_some());
        assert!(VALID_ALPHA_NUM_HYPHEN_UNDERSCORE.is_some());
        assert!(VALID_SIMPLE_ALPHA_NUM.is_some());
        assert!(VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE.is_some());
        assert!(VALID_SIMPLE_ALPHA_NUM_HYPHEN_UNDERSCORE_PLUS.is_some());
        assert!(VALID_HASHTAG.is_some());
        assert!(HASHTAG_START.is_some());
    }

    #[test]
    fn is_valid_alpha_num_matches_go_cases() {
        assert!(is_valid_alpha_num("test"));
        assert!(is_valid_alpha_num("test-name"));
        assert!(is_valid_alpha_num("test--name"));
        assert!(is_valid_alpha_num("test__name"));
        assert!(is_valid_alpha_num("t1"));

        assert!(!is_valid_alpha_num("")); // empty
        assert!(!is_valid_alpha_num("a")); // needs >= 2 chars
        assert!(!is_valid_alpha_num("-test")); // leading hyphen
        assert!(!is_valid_alpha_num("test-")); // trailing hyphen
        assert!(!is_valid_alpha_num("test name")); // space
        assert!(!is_valid_alpha_num("Test")); // uppercase
        assert!(!is_valid_alpha_num("test_name")); // single underscore
    }

    #[test]
    fn alpha_num_hyphen_underscore_with_format_constrains_the_edges() {
        assert!(is_valid_alpha_num_hyphen_underscore("test_name", true));
        assert!(is_valid_alpha_num_hyphen_underscore("test-name", true));
        assert!(!is_valid_alpha_num_hyphen_underscore("_test", true));
        assert!(!is_valid_alpha_num_hyphen_underscore("test_", true));
        assert!(!is_valid_alpha_num_hyphen_underscore("Test", true));
    }

    #[test]
    fn alpha_num_hyphen_underscore_without_format_is_a_class_check() {
        assert!(is_valid_alpha_num_hyphen_underscore("Test", false));
        assert!(is_valid_alpha_num_hyphen_underscore("_test_", false));
        assert!(is_valid_alpha_num_hyphen_underscore("-", false));
        assert!(!is_valid_alpha_num_hyphen_underscore("", false));
        assert!(!is_valid_alpha_num_hyphen_underscore("test name", false));
        assert!(!is_valid_alpha_num_hyphen_underscore("test+name", false));
    }

    #[test]
    fn alpha_num_hyphen_underscore_plus_allows_plus() {
        assert!(is_valid_alpha_num_hyphen_underscore_plus("test+name"));
        assert!(is_valid_alpha_num_hyphen_underscore_plus("Test_1-2"));
        assert!(!is_valid_alpha_num_hyphen_underscore_plus(""));
        assert!(!is_valid_alpha_num_hyphen_underscore_plus("test name"));
    }

    #[test]
    fn simple_alpha_num_allows_a_two_char_minimum_and_trailing_class() {
        assert!(is_valid_simple_alpha_num("ab"));
        assert!(is_valid_simple_alpha_num("a-"));
        assert!(!is_valid_simple_alpha_num("-a"));
        assert!(!is_valid_simple_alpha_num("A"));
    }

    // -- string helpers ----------------------------------------------------

    #[test]
    fn clear_mention_tags_removes_both_tags() {
        assert_eq!(clear_mention_tags("<mention>hi</mention>"), "hi");
        assert_eq!(clear_mention_tags("no tags"), "no tags");
        assert_eq!(clear_mention_tags("<mention><mention>x"), "x");
    }

    #[test]
    fn remove_duplicate_strings_sorts_and_dedups() {
        let mut input: Vec<String> = ["b", "a", "b", "c", "a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        remove_duplicate_strings(&mut input);
        assert_eq!(input, vec!["a", "b", "c"]);

        let mut empty: Vec<String> = vec![];
        remove_duplicate_strings(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn remove_duplicate_strings_non_sort_preserves_order() {
        let input: Vec<String> = ["b", "a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            remove_duplicate_strings_non_sort(&input),
            vec!["b", "a", "c"]
        );
        assert!(remove_duplicate_strings_non_sort(&[]).is_empty());
    }

    #[test]
    fn get_preferred_timezone_follows_the_automatic_flag() {
        let mut tz = StringMap::new();
        tz.insert("useAutomaticTimezone".into(), "true".into());
        tz.insert("automaticTimezone".into(), "America/New_York".into());
        tz.insert("manualTimezone".into(), "Europe/Berlin".into());
        assert_eq!(get_preferred_timezone(&tz), "America/New_York");

        tz.insert("useAutomaticTimezone".into(), "false".into());
        assert_eq!(get_preferred_timezone(&tz), "Europe/Berlin");

        // Absent keys yield Go's zero value.
        assert_eq!(get_preferred_timezone(&StringMap::new()), "");
    }

    #[test]
    fn sanitize_unicode_drops_the_blocklist_and_keeps_everything_else() {
        assert_eq!(sanitize_unicode("hello"), "hello");
        assert_eq!(sanitize_unicode("a\u{FEFF}b"), "ab"); // BOM
        assert_eq!(sanitize_unicode("a\u{2028}b"), "ab"); // line separator
        assert_eq!(sanitize_unicode("a\u{202E}b"), "ab"); // BIDI override
        assert_eq!(sanitize_unicode("a\u{1D173}b"), "ab"); // musical notation
        assert_eq!(sanitize_unicode("a\u{E0001}b"), "ab"); // language tag
        assert_eq!(sanitize_unicode("héllo ☃"), "héllo ☃"); // untouched
    }

    #[test]
    fn limit_runes_counts_characters_not_bytes() {
        assert_eq!(limit_runes("hello", 10), ("hello".to_string(), false));
        assert_eq!(limit_runes("hello", 3), ("hel".to_string(), true));
        assert_eq!(limit_runes("héllo", 2), ("hé".to_string(), true));
        assert_eq!(limit_runes("", 3), (String::new(), false));
        assert_eq!(limit_runes("abc", 0), (String::new(), true));
    }

    #[test]
    fn limit_bytes_truncates_on_a_char_boundary() {
        assert_eq!(limit_bytes("hello", 10), ("hello".to_string(), false));
        assert_eq!(limit_bytes("hello", 3), ("hel".to_string(), true));
        // "é" is two bytes: Go would cut it in half here, we stop before it.
        assert_eq!(limit_bytes("aé", 2), ("a".to_string(), true));
    }

    // -- AppError ----------------------------------------------------------

    #[test]
    fn app_error_matches_go_serialization() {
        let go = include_str!("../../../fixtures/app_error.json");
        let parsed: AppError = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
    }

    #[test]
    fn app_error_omits_empty_omitempty_fields() {
        let err = AppError::new("Where.Func", "some.id", None, "", 0);
        let value = serde_json::to_value(&err).unwrap();
        let object = value.as_object().unwrap();

        // omitempty fields, both zero
        assert!(!object.contains_key("request_id"));
        assert!(!object.contains_key("status_code"));
        // json:"-" fields, never on the wire
        assert!(!object.contains_key("where_"));
        assert!(!object.contains_key("Where"));
        assert!(!object.contains_key("skip_translation"));
        // non-omitempty fields, present even when empty
        assert!(object.contains_key("detailed_error"));
        assert_eq!(object.len(), 3);
    }

    #[test]
    fn app_error_new_sets_message_to_id() {
        // Go's NewAppError calls Translate, which with no bundle registered sets Message = Id.
        let err = AppError::new("Api.Handler", "api.thing.failed", None, "detail", 400);
        assert_eq!(err.id, "api.thing.failed");
        assert_eq!(err.message, "api.thing.failed");
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "Api.Handler");
        assert_eq!(err.detailed_error, "detail");
    }

    #[test]
    fn app_error_display_includes_where_message_and_detail() {
        let err = AppError::new("Api.Handler", "an.id", None, "the detail", 400);
        assert_eq!(err.to_string(), "Api.Handler: an.id, the detail");
    }

    #[test]
    fn app_error_display_omits_empty_where_and_detail() {
        let mut err = AppError::new("", "an.id", None, "", 400);
        assert_eq!(err.to_string(), "an.id");

        err.detailed_error = "detail".into();
        assert_eq!(err.to_string(), "an.id, detail");
    }

    #[test]
    fn app_error_display_skips_the_untranslated_sentinel() {
        let mut err = AppError::new("W", "an.id", None, "detail", 400);
        err.message = NO_TRANSLATION.to_string();
        // No message, and no ", " separator before the detail.
        assert_eq!(err.to_string(), "W: detail");
    }

    #[test]
    fn app_error_display_appends_the_wrapped_error() {
        let inner = std::io::Error::other("inner boom");
        let err = AppError::new("W", "an.id", None, "detail", 400).wrap(inner);
        assert_eq!(err.to_string(), "W: an.id, detail, inner boom");
    }

    #[test]
    fn app_error_display_truncates_at_1024_bytes() {
        let mut err = AppError::new("", "an.id", None, "", 400);
        err.detailed_error = "x".repeat(2000);
        let rendered = err.to_string();
        assert!(rendered.ends_with("..."));
        assert_eq!(rendered.len(), MAX_ERROR_LENGTH + 3);
    }

    #[test]
    fn app_error_to_json_folds_the_wrapped_error_into_detail() {
        let inner = std::io::Error::other("inner boom");
        let err = AppError::new("W", "an.id", None, "detail", 400).wrap(inner);

        let json: serde_json::Value = serde_json::from_str(&err.to_json().unwrap()).unwrap();
        assert_eq!(json["detailed_error"], "detail, inner boom");
        // Folding is for the wire only; the struct is untouched, unlike Go's mutate-restore.
        assert_eq!(err.detailed_error, "detail");
    }

    #[test]
    fn app_error_to_json_uses_the_wrapped_error_alone_when_detail_is_empty() {
        let inner = std::io::Error::other("inner boom");
        let err = AppError::new("W", "an.id", None, "", 400).wrap(inner);
        let json: serde_json::Value = serde_json::from_str(&err.to_json().unwrap()).unwrap();
        assert_eq!(json["detailed_error"], "inner boom");
    }

    #[test]
    fn app_error_wipe_detailed_clears_detail_and_wrapped() {
        let inner = std::io::Error::other("inner boom");
        let mut err = AppError::new("W", "an.id", None, "detail", 400).wrap(inner);
        err.wipe_detailed();
        assert_eq!(err.detailed_error, "");
        assert_eq!(err.to_string(), "W: an.id");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn app_error_source_exposes_the_wrapped_error() {
        let inner = std::io::Error::other("inner boom");
        let err = AppError::new("W", "an.id", None, "", 400).wrap(inner);
        let source = std::error::Error::source(&err).unwrap();
        assert_eq!(source.to_string(), "inner boom");
    }
}

/// Differential tests against `fixtures/behaviour_utils.json`, which records what the **real
/// Go implementations** returned for each input (generated by `reference/dump/behaviour.go`).
///
/// The tests above encode what the Go source *says*. These encode what it *does*. They are
/// the reason `is_valid_id` uses Unicode general categories rather than
/// `char::is_alphabetic()` — the hand-reasoned version passed every test in the module above
/// and still disagreed with Go.
#[cfg(test)]
mod go_parity {
    use super::*;
    // Only the tests need a `FixedOffset` now: `day_millis_at` does the arithmetic itself,
    // because Go's offset is unbounded and chrono's type is not.
    use chrono::FixedOffset;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_utils.json")).unwrap()
    }

    /// Asserts a predicate against every recorded `input -> bool` pair.
    fn check_predicate(section: &str, predicate: impl Fn(&str) -> bool) {
        let oracle = oracle();
        let cases = oracle[section].as_object().unwrap();
        assert!(!cases.is_empty(), "{section} corpus is empty");
        for (input, want) in cases {
            let want = want.as_bool().unwrap();
            assert_eq!(
                predicate(input),
                want,
                "{section}({input:?}): Go said {want}, we said {}",
                !want
            );
        }
    }

    /// Asserts a transform against every recorded `input -> output` pair.
    fn check_transform(section: &str, transform: impl Fn(&str) -> String) {
        let oracle = oracle();
        let cases = oracle[section].as_object().unwrap();
        assert!(!cases.is_empty(), "{section} corpus is empty");
        for (input, want) in cases {
            let want = want.as_str().unwrap();
            assert_eq!(transform(input), want, "{section}({input:?})");
        }
    }

    #[test]
    fn valid_alpha_num_matches_go() {
        check_predicate("valid_alpha_num", is_valid_alpha_num);
    }

    #[test]
    fn valid_alpha_num_hyphen_underscore_matches_go() {
        check_predicate("valid_alpha_num_hyphen_underscore", |s| {
            is_valid_alpha_num_hyphen_underscore(s, true)
        });
        check_predicate("valid_simple_alpha_num_hyphen_underscore", |s| {
            is_valid_alpha_num_hyphen_underscore(s, false)
        });
    }

    #[test]
    fn valid_alpha_num_hyphen_underscore_plus_matches_go() {
        check_predicate(
            "valid_simple_alpha_num_hyphen_underscore_plus",
            is_valid_alpha_num_hyphen_underscore_plus,
        );
    }

    #[test]
    fn valid_simple_alpha_num_matches_go() {
        check_predicate("valid_simple_alpha_num", is_valid_simple_alpha_num);
    }

    #[test]
    fn is_valid_id_matches_go() {
        // Includes U+0345, which is Other_Alphabetic but general category Mn. Go rejects it;
        // char::is_alphabetic() accepts it. This case is why is_go_letter exists.
        check_predicate("is_valid_id", is_valid_id);
    }

    #[test]
    fn pad_date_string_zeros_matches_go() {
        check_transform("pad_date_string_zeros", pad_date_string_zeros);
    }

    #[test]
    fn is_valid_email_matches_go() {
        // 128 hand-picked cases: every atext special, dot placement, domain shape, IP
        // literal, display name, quoted local part, unicode boundary and case-folding edge.
        check_predicate("is_valid_email", is_valid_email);
    }

    #[test]
    fn is_valid_email_matches_go_on_generated_input() {
        // ~2.8k deterministic pseudo-random strings drawn from a hostile alphabet. The
        // hand-picked corpus above only covers cases someone thought of; this one does not
        // care what either implementer expected.
        let oracle = oracle();
        let cases = oracle["is_valid_email_fuzz"].as_object().unwrap();
        assert!(cases.len() > 2000, "fuzz corpus unexpectedly small");

        let mut accepted = 0;
        for (input, want) in cases {
            let want = want.as_bool().unwrap();
            assert_eq!(is_valid_email(input), want, "IsValidEmail({input:?})");
            accepted += usize::from(want);
        }
        // Guard against a corpus that proves nothing because Go rejected everything.
        assert!(accepted > 0, "no generated input was accepted by Go");
    }

    #[test]
    fn clear_mention_tags_matches_go() {
        check_transform("clear_mention_tags", clear_mention_tags);
    }

    #[test]
    fn sanitize_unicode_matches_go() {
        check_transform("sanitize_unicode", sanitize_unicode);
    }

    #[test]
    fn get_preferred_timezone_matches_go() {
        // The oracle is keyed by case name; the inputs are mirrored from behaviour.go.
        let inputs: [(&str, &[(&str, &str)]); 5] = [
            (
                "automatic",
                &[
                    ("useAutomaticTimezone", "true"),
                    ("automaticTimezone", "America/New_York"),
                    ("manualTimezone", "Europe/Berlin"),
                ],
            ),
            (
                "manual",
                &[
                    ("useAutomaticTimezone", "false"),
                    ("automaticTimezone", "America/New_York"),
                    ("manualTimezone", "Europe/Berlin"),
                ],
            ),
            ("empty", &[]),
            ("missing", &[("useAutomaticTimezone", "true")]),
            (
                "truthy",
                &[
                    ("useAutomaticTimezone", "TRUE"),
                    ("manualTimezone", "Europe/Berlin"),
                ],
            ),
        ];

        let oracle = oracle();
        for (name, pairs) in inputs {
            let tz: StringMap = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            let want = oracle["get_preferred_timezone"][name].as_str().unwrap();
            assert_eq!(get_preferred_timezone(&tz), want, "timezone case {name}");
        }
    }

    #[test]
    fn limits_match_go() {
        let oracle = oracle();
        let cases = oracle["limits"].as_array().unwrap();
        let mut checked = 0;

        for case in cases {
            let input = case["in"].as_str().unwrap();
            let max = case["max"].as_u64().unwrap() as usize;
            let want_out = case["out"].as_str().unwrap();
            let want_cut = case["cut"].as_bool().unwrap();

            match case["kind"].as_str().unwrap() {
                "runes" => {
                    assert_eq!(
                        limit_runes(input, max),
                        (want_out.to_string(), want_cut),
                        "LimitRunes({input:?}, {max})"
                    );
                    checked += 1;
                }
                "bytes" if want_out.contains('\u{FFFD}') => {
                    // Go split a multi-byte rune and returned invalid UTF-8; the fixture
                    // itself cannot hold it (encoding/json substituted U+FFFD). Assert the
                    // documented divergence instead: same cut flag, truncated at a boundary.
                    let (out, cut) = limit_bytes(input, max);
                    assert_eq!(cut, want_cut, "LimitBytes({input:?}, {max}) cut flag");
                    assert!(
                        out.len() < max,
                        "expected a boundary-safe short read, got {out:?}"
                    );
                    assert!(input.starts_with(&out));
                    checked += 1;
                }
                "bytes" => {
                    assert_eq!(
                        limit_bytes(input, max),
                        (want_out.to_string(), want_cut),
                        "LimitBytes({input:?}, {max})"
                    );
                    checked += 1;
                }
                other => panic!("unknown limit kind {other}"),
            }
        }
        assert_eq!(checked, cases.len());
    }

    #[test]
    fn day_bounds_match_go() {
        let oracle = oracle();
        for case in oracle["day_bounds"].as_array().unwrap() {
            let millis = case["millis"].as_i64().unwrap();
            let offset = case["offset"].as_i64().unwrap();

            // Rebuild the instant in the timezone the Go run actually used, rather than
            // this machine's. GetStartOfDayMillis reads the calendar date off the input's
            // own zone, so using Local here would make the test pass or fail by geography.
            let recorded_zone =
                FixedOffset::east_opt(case["local_offset"].as_i64().unwrap() as i32).unwrap();
            let time = recorded_zone.timestamp_millis_opt(millis).single().unwrap();

            assert_eq!(
                get_start_of_day_millis(&time, offset),
                case["start"].as_i64(),
                "GetStartOfDayMillis({millis}, {offset})"
            );
            assert_eq!(
                get_end_of_day_millis(&time, offset),
                case["end"].as_i64(),
                "GetEndOfDayMillis({millis}, {offset})"
            );
        }
    }

    #[test]
    fn app_error_rendering_matches_go() {
        let oracle = oracle();
        let cases = oracle["app_errors"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let where_ = case["where"].as_str().unwrap();
            let id = case["id"].as_str().unwrap();
            let message = case["message"].as_str().unwrap();
            let detailed = case["detailed"].as_str().unwrap();
            let wrapped = case["wrapped"].as_str().unwrap();

            let mut err = AppError::new(where_, id, None, detailed, 400);
            if !message.is_empty() {
                err.message = message.to_string();
            }
            if !wrapped.is_empty() {
                err = err.wrap(std::io::Error::other(wrapped.to_string()));
            }

            assert_eq!(
                err.to_string(),
                case["display"].as_str().unwrap(),
                "AppError::Error() for {case:?}"
            );

            // Compare JSON as value graphs, not strings — key order is not part of the contract.
            let ours: Value = serde_json::from_str(&err.to_json().unwrap()).unwrap();
            let theirs: Value = serde_json::from_str(case["to_json"].as_str().unwrap()).unwrap();
            assert_eq!(ours, theirs, "AppError::ToJSON() for {case:?}");
        }
    }
}

/// Unit tests for [`go_json_escape`], which has no Go counterpart to name — it is the delta
/// between serde_json's escaping and Go's. The parity evidence is in
/// `user::custom_status_go_parity::set_custom_status_stores_gos_bytes`, which compares a
/// marshalled struct against bytes Go produced.
#[cfg(test)]
mod go_json_escape_tests {
    use super::*;

    #[test]
    fn escapes_the_five_characters_serde_json_leaves_alone() {
        assert_eq!(go_json_escape(r#""<b>""#), r#""\u003cb\u003e""#);
        assert_eq!(go_json_escape(r#""a&b""#), r#""a\u0026b""#);
        assert_eq!(go_json_escape("\"\u{2028}\u{2029}\""), r#""\u2028\u2029""#);
    }

    #[test]
    fn leaves_structure_and_existing_escapes_alone() {
        // `<` cannot appear outside a string in valid JSON, but the walker must not treat the
        // structural characters around one as string content either.
        assert_eq!(
            go_json_escape(r#"{"k":["<",1,true,null]}"#),
            r#"{"k":["\u003c",1,true,null]}"#
        );
        // A quote inside a string ends nothing, and a backslash run is not re-interpreted.
        assert_eq!(
            go_json_escape(r#"{"a\"<":"\\","b":">"}"#),
            r#"{"a\"\u003c":"\\","b":"\u003e"}"#
        );
    }

    /// The two marshallers reach Go's bytes by different routes —
    /// [`go_json_marshal_string_map`] builds them directly, [`go_json_marshal`] re-escapes
    /// serde_json's — so their *escaping* must agree.
    ///
    /// A single entry, because their **key order** does not agree and cannot: `StringMap` is a
    /// `HashMap`, so serde_json emits it in iteration order while Go sorts. That is why
    /// `go_json_marshal` is documented as struct-only, and why `StringMap` still has its own
    /// marshaller rather than delegating to the general one.
    #[test]
    fn escapes_the_same_way_as_the_hand_written_string_map_marshaller() {
        for (key, value) in [("a<", "b&c"), ("k", "\u{2028}x"), ("q\"", "tab\there")] {
            let mut map = StringMap::new();
            map.insert(key.to_string(), value.to_string());
            assert_eq!(
                go_json_marshal(&map).unwrap(),
                go_json_marshal_string_map(Some(&map)),
                "{key:?} => {value:?}"
            );
        }
    }
}

/// Parity test for [`go_to_lower`], driven by `fixtures/behaviour_utils.json`.
#[cfg(test)]
mod go_to_lower_parity {
    use super::*;
    use serde_json::Value;

    #[test]
    fn go_to_lower_matches_go() {
        let oracle: Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_utils.json")).unwrap();
        let cases = oracle["go_to_lower"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (input, want) in cases {
            assert_eq!(
                go_to_lower(input),
                want.as_str().unwrap(),
                "input {input:?}"
            );
        }
    }

    /// The two inputs that made this function necessary. Asserted directly as well as through
    /// the corpus, so the reason the helper exists survives a corpus edit.
    #[test]
    fn str_to_lowercase_would_disagree() {
        assert_eq!(go_to_lower("İ"), "i");
        assert_ne!("İ".to_lowercase(), "i");

        assert_eq!(go_to_lower("ΟΔΟΣ"), "οδοσ");
        assert_ne!("ΟΔΟΣ".to_lowercase(), "οδοσ");
    }
}

/// Tests for [`is_valid_http_url`], asserted against `fixtures/behaviour_url.json` — produced by
/// `reference/dump/behaviour_url.go`, which runs the real `model.IsValidHTTPURL`.
///
/// This closes [D-003]. It gets its own module because it reads a different fixture from the
/// `go_parity` module above.
#[cfg(test)]
mod http_url_go_parity {
    use super::is_valid_http_url;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_url.json")).unwrap()
    }

    fn check(cases: &serde_json::Map<String, serde_json::Value>, label: &str) {
        assert!(!cases.is_empty(), "{label} corpus is empty");
        for (input, want) in cases {
            let want = want.as_bool().unwrap();
            assert_eq!(
                is_valid_http_url(input),
                want,
                "{label}({input:?}): Go said {want}, we said {}",
                !want
            );
        }
    }

    /// 136 hand-picked inputs: the prefix gate, empty authorities, userinfo splitting,
    /// bracketed IP literals, percent escapes in all three positions, control bytes and the
    /// characters that separate a host from a path.
    #[test]
    fn is_valid_http_url_matches_go() {
        let o = oracle();
        check(
            o["is_valid_http_url"].as_object().unwrap(),
            "is_valid_http_url",
        );
    }

    /// ~2.9k deterministic pseudo-random strings over a hostile alphabet, two thirds of them
    /// given a real scheme prefix so the parser behind the prefix gate is actually exercised.
    #[test]
    fn is_valid_http_url_matches_go_on_generated_input() {
        let o = oracle();
        let cases = o["is_valid_http_url_fuzz"].as_object().unwrap();
        assert!(cases.len() > 2000, "fuzz corpus unexpectedly small");
        check(cases, "is_valid_http_url_fuzz");
    }

    /// Exhaustive 0..127 sweeps at four positions. A hand-picked corpus finds the characters
    /// its author suspected; these find the rest — and they are what established that the host,
    /// the path and the query have three different rules.
    #[test]
    fn the_per_position_byte_classes_match_go() {
        let o = oracle();
        let tables = o["is_valid_http_url_bytes"].as_object().unwrap();

        for (position, build) in [
            (
                "host",
                &(|c: &str| format!("http://a{c}b.com")) as &dyn Fn(&str) -> String,
            ),
            ("path", &|c: &str| format!("http://example.com/a{c}b")),
            ("query", &|c: &str| format!("http://example.com/?q=a{c}b")),
            ("userinfo", &|c: &str| format!("http://a{c}b@example.com")),
        ] {
            let table = tables[position].as_object().unwrap();
            assert_eq!(table.len(), 128, "{position} sweep is not 0..127");
            for (hex, want) in table {
                let byte = u8::from_str_radix(hex, 16).unwrap();
                let input = build(&(byte as char).to_string());
                let want = want.as_bool().unwrap();
                assert_eq!(
                    is_valid_http_url(&input),
                    want,
                    "{position} byte 0x{hex}: Go said {want}, we said {}",
                    !want
                );
            }
        }

        for section in ["colons", "brackets"] {
            check(tables[section].as_object().unwrap(), section);
        }
    }
}

/// Port of `strconv.Quote`, which is what `fmt`'s `%q` verb produces for a string.
///
/// Rust's `{:?}` is **not** substitutable. Both quote with `"` and escape `"` and `\`, but they
/// disagree on everything else that is not plainly printable:
///
/// | input | Go `%q` | Rust `{:?}` |
/// |---|---|---|
/// | U+0007 | `\a` | `\u{7}` |
/// | U+000B | `\v` | `\u{b}` |
/// | U+001B | `\x1b` | `\u{1b}` |
/// | U+00A0 | ` ` | `\u{a0}` |
///
/// Go's rule, measured over 29 inputs: a rune is written literally when
/// [`go_is_print`] accepts it; otherwise `\a \b \f \n \r \t \v` where one applies, `\xNN` for a
/// byte below U+0020 or U+007F exactly, `\uNNNN` below U+10000, and `\UNNNNNNNN` above. Note the
/// C1 controls (U+0080..U+009F) take the **`\u` form, not `\x`** — the `\x` branch is keyed on
/// the rune value, not on its encoded width.
///
/// The one Go behaviour with no counterpart is invalid UTF-8, which Go writes as `\xNN` per
/// bad byte; a Rust `&str` cannot hold it.
pub fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');

    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ if go_is_print(c) => out.push(c),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            _ => {
                let code = c as u32;
                if code < 0x20 || code == 0x7f {
                    out.push_str(&format!("\\x{code:02x}"));
                } else if code < 0x10000 {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    out.push_str(&format!("\\U{code:08x}"));
                }
            }
        }
    }

    out.push('"');
    out
}

/// Port of `unicode.IsPrint` — general categories `L`, `M`, `N`, `P`, `S`, plus the ASCII space.
///
/// Note what this excludes: every other space (NBSP, U+3000, U+1680), the format characters
/// (U+200B, U+FEFF, U+0085) and the control characters. `char::is_control` alone would keep all
/// of those literal and diverge from Go.
fn go_is_print(c: char) -> bool {
    use unicode_general_category::GeneralCategory::*;
    if c == ' ' {
        return true;
    }
    matches!(
        unicode_general_category::get_general_category(c),
        UppercaseLetter
            | LowercaseLetter
            | TitlecaseLetter
            | ModifierLetter
            | OtherLetter
            | NonspacingMark
            | SpacingMark
            | EnclosingMark
            | DecimalNumber
            | LetterNumber
            | OtherNumber
            | ConnectorPunctuation
            | DashPunctuation
            | OpenPunctuation
            | ClosePunctuation
            | InitialPunctuation
            | FinalPunctuation
            | OtherPunctuation
            | MathSymbol
            | CurrencySymbol
            | ModifierSymbol
            | OtherSymbol
    )
}

#[cfg(test)]
mod go_quote_go_parity {
    use super::*;

    #[test]
    fn go_quote_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_dialog.json")).unwrap();

        let mut skipped = 0;
        for case in oracle.get("quote").unwrap().as_array().unwrap() {
            let input = case.get("input").unwrap().as_str().unwrap();
            let expected = case.get("quoted").unwrap().as_str().unwrap();

            // Two corpus inputs are invalid UTF-8. Go quotes the bad bytes as `\xNN`; the
            // fixture had to marshal them through JSON, which replaced each with U+FFFD, and a
            // Rust `&str` could not have held them in the first place. Unreachable rather than
            // divergent.
            if input.contains('\u{fffd}') {
                skipped += 1;
                continue;
            }

            assert_eq!(go_quote(input), expected, "input {input:?}");
        }
        assert_eq!(skipped, 2, "the invalid-UTF-8 corpus changed");
    }

    /// The four inputs where Rust's `{:?}` disagrees, asserted explicitly so the reason this
    /// shim exists cannot be forgotten.
    #[test]
    fn rusts_debug_formatting_is_not_substitutable() {
        for input in ["\u{7}", "\u{b}", "\u{1b}", "\u{a0}"] {
            assert_ne!(go_quote(input), format!("{input:?}"));
        }
        // …and agrees on ordinary text, which is why the difference is easy to miss.
        for input in ["", "abc", "a\"b", "a\\b", "é", "日本語", "😀"] {
            assert_eq!(go_quote(input), format!("{input:?}"));
        }
    }
}
