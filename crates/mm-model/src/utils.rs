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

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, FixedOffset, Local, TimeZone, Utc};
use rand::RngCore;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Port of `model.StringMap` — serialises as a plain JSON object.
///
/// Note: a **nil** Go map marshals to `null`, not `{}`. Struct fields that Go can leave nil
/// must therefore be `Option<StringMap>`, or the wire format drifts.
pub type StringMap = HashMap<String, String>;

/// Port of `model.StringInterface` (utils.go:48).
pub type StringInterface = HashMap<String, serde_json::Value>;

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
pub fn pad_date_string_zeros(date_string: &str) -> String {
    date_string
        .split('-')
        .map(|part| {
            if part.chars().count() == 1 {
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
pub fn get_start_of_day_millis<Tz: TimeZone>(
    this_time: &DateTime<Tz>,
    tz_offset_seconds: i32,
) -> Option<i64> {
    let zone = FixedOffset::east_opt(tz_offset_seconds)?;
    zone.with_ymd_and_hms(
        this_time.year(),
        this_time.month(),
        this_time.day(),
        0,
        0,
        0,
    )
    .single()
    .map(|t| t.timestamp_millis())
}

/// Port of `model.GetEndOfDayMillis` (utils.go:482).
///
/// Go builds 23:59:59.999999999; `UnixMilli` truncates the nanoseconds, so the result is
/// `.999`.
pub fn get_end_of_day_millis<Tz: TimeZone>(
    this_time: &DateTime<Tz>,
    tz_offset_seconds: i32,
) -> Option<i64> {
    let zone = FixedOffset::east_opt(tz_offset_seconds)?;
    zone.with_ymd_and_hms(
        this_time.year(),
        this_time.month(),
        this_time.day(),
        23,
        59,
        59,
    )
    .single()
    .map(|t| t.timestamp_millis() + 999)
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

fn matches(re: &LazyLock<Option<Regex>>, s: &str) -> bool {
    re.as_ref().is_some_and(|re| re.is_match(s))
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
// go_time — Go's `time.Time` JSON encoding
// ---------------------------------------------------------------------------

/// Go's `time.Time` as `encoding/json` writes and reads it.
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
            let offset = case["offset"].as_i64().unwrap() as i32;

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
