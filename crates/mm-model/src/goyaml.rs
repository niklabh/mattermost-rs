//! A block-style YAML emitter that writes what `github.com/goccy/go-yaml` v1.19.2 writes
//! (`yaml.Marshal`, `yaml.MarshalWithOptions(…, yaml.WithComment(…))`) for the value shapes the
//! Support Packet marshals: mappings in field order, sorted string maps, sequences, strings,
//! integers, floats, booleans, times and `null`.
//!
//! # Why not `serde_yaml`
//!
//! The packet's YAML files are read by people and by Mattermost's support tooling, and the
//! difference between two emitters is exactly where a reader is misled: goccy quotes a string
//! `time.Parse` accepts under any of five layouts (so every `FormatMillis` timestamp is quoted),
//! every YAML 1.1 boolean (`y`, `on`, `No`), anything with `: ` or ` #` inside it, and anything
//! that parses as a number *or overflows trying*; it writes a float with no point as `1.0`, a nil
//! slice as `[]` and a nil map as `{}`, and a multi-line string as a `|-`, `|` or `|+` block
//! indented from its owner's column. Each of those is pinned against goccy's own output by
//! `go_parity` below (`fixtures/behaviour_goyaml.json`), not reasoned about.
//!
//! # What is not modelled
//!
//! Flow style, anchors and aliases, `MarshalYAML` documents, tags and the `json` style — none of
//! them is reachable from a packet file. A nil pointer is [`Node::Null`]; the builder decides
//! where Go's `omitempty` drops a field, since that is a property of the struct, not the value.

use std::fmt::Write as _;

use crate::utils::go_quote;

/// One value, as the encoder's AST holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A nil pointer or interface: `null`.
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    /// `strconv.FormatFloat(v, 'g', -1, 64)`, with `.0` appended when that has no point and no
    /// exponent (encode.go:541).
    Float(f64),
    /// A string, quoted when [`is_need_quoted`] says so and otherwise written plain or as a
    /// literal block.
    Str(String),
    /// Text written verbatim — `encodeTime`'s `RFC3339Nano` string, which goccy never quotes.
    Verbatim(String),
    /// A mapping in insertion order. Empty is `{}`.
    Map(Vec<Entry>),
    /// A sequence. Empty is `[]`; so is a nil slice (goccy does not treat one as invalid).
    Seq(Vec<Node>),
}

/// One key of a mapping, with the comments `yaml.WithComment` attaches to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub value: Node,
    /// `yaml.HeadComment` texts, each written on its own line above the key as `#<text>`.
    pub head: Vec<String>,
    /// `yaml.LineComment` text, written after a scalar value as ` #<text>`.
    pub line: Option<String>,
}

impl Entry {
    pub fn new(key: &str, value: Node) -> Self {
        Entry {
            key: key.to_owned(),
            value,
            head: Vec::new(),
            line: None,
        }
    }
}

/// A mapping builder, so a struct's fields read in order at the call site.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapBuilder(Vec<Entry>);

impl MapBuilder {
    pub fn new() -> Self {
        MapBuilder(Vec::new())
    }

    /// A field with no `omitempty`.
    pub fn field(mut self, key: &str, value: Node) -> Self {
        self.0.push(Entry::new(key, value));
        self
    }

    /// A field with `omitempty`: dropped when `omit`.
    pub fn field_unless(self, omit: bool, key: &str, value: Node) -> Self {
        if omit { self } else { self.field(key, value) }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn build(self) -> Node {
        Node::Map(self.0)
    }

    /// The entries, for attaching comments before building.
    pub fn entries_mut(&mut self) -> &mut Vec<Entry> {
        &mut self.0
    }
}

impl Node {
    pub fn str(s: &str) -> Node {
        Node::Str(s.to_owned())
    }

    /// A `[]string`, nil or not — both are `[]` when empty.
    pub fn strings<S: AsRef<str>>(items: &[S]) -> Node {
        Node::Seq(items.iter().map(|s| Node::str(s.as_ref())).collect())
    }

    /// A `map[string]string`, which goccy sorts by `fmt.Sprint(key)` — byte order.
    pub fn string_map<'a>(map: impl IntoIterator<Item = (&'a String, &'a String)>) -> Node {
        let mut entries: Vec<(&String, &String)> = map.into_iter().collect();
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        Node::Map(
            entries
                .into_iter()
                .map(|(k, v)| Entry::new(k, Node::str(v)))
                .collect(),
        )
    }
}

/// `yaml.Marshal` of `node` as a whole document, with goccy's trailing newline.
pub fn marshal(node: &Node) -> String {
    let mut out = String::new();
    match node {
        Node::Map(entries) if !entries.is_empty() => write_map(&mut out, entries, 0),
        Node::Seq(items) if !items.is_empty() => write_seq(&mut out, items, 0),
        other => {
            out.push_str(&scalar_text(other, 2));
            out.push('\n');
        }
    }
    out
}

fn spaces(n: usize) -> String {
    " ".repeat(n)
}

/// A mapping whose keys sit at `indent`.
fn write_map(out: &mut String, entries: &[Entry], indent: usize) {
    for entry in entries {
        for head in &entry.head {
            let _ = writeln!(out, "{}#{head}", spaces(indent));
        }
        out.push_str(&spaces(indent));
        out.push_str(&string_text(&entry.key, indent + 2));
        out.push(':');
        write_value(out, &entry.value, indent, entry.line.as_deref());
    }
}

/// What follows `key:` (or `-`) for a value whose owner sits at `indent`.
fn write_value(out: &mut String, value: &Node, indent: usize, line: Option<&str>) {
    match value {
        Node::Map(entries) if !entries.is_empty() => {
            out.push('\n');
            write_map(out, entries, indent + 2);
        }
        Node::Seq(items) if !items.is_empty() => {
            out.push('\n');
            write_seq(out, items, indent);
        }
        scalar => {
            out.push(' ');
            out.push_str(&scalar_text(scalar, indent + 2));
            if let Some(comment) = line {
                out.push_str(" #");
                out.push_str(comment);
            }
            out.push('\n');
        }
    }
}

/// A sequence whose dashes sit at `indent` — goccy's default `IndentSequence(false)` puts a
/// mapping's sequence at the mapping's own column.
///
/// Port of `SequenceNode.blockStyleString` (ast/ast.go:1633), which is not "indent the item by
/// two": each item is rendered **at the sequence's own column**, then split on `\n` only, its
/// first line stripped of leading spaces, and every later line re-indented by two — or emptied
/// when it held nothing past the first line's indentation. Two consequences are visible and
/// pinned by the corpus: a whitespace-only line of a literal block inside a sequence comes out
/// empty, and a block broken by a bare `\r` inside a sequence item's mapping is *not*
/// re-indented, because `\r` is not where the printer splits.
fn write_seq(out: &mut String, items: &[Node], indent: usize) {
    let prefix = spaces(indent + 2);
    for item in items {
        let (rendered, is_string) = match item {
            Node::Map(entries) if !entries.is_empty() => {
                let mut nested = String::new();
                write_map(&mut nested, entries, indent);
                (nested, false)
            }
            Node::Seq(inner) if !inner.is_empty() => {
                let mut nested = String::new();
                write_seq(&mut nested, inner, indent);
                (nested, false)
            }
            scalar => (
                scalar_text(scalar, indent + 2),
                matches!(scalar, Node::Str(_)),
            ),
        };
        // A nested container's rendering ends in its own last line's newline; a scalar's text
        // carries none of its own (a `|+` block's trailing newlines are content).
        let body = if matches!(item, Node::Map(e) if !e.is_empty())
            || matches!(item, Node::Seq(i) if !i.is_empty())
        {
            rendered.strip_suffix('\n').unwrap_or(&rendered)
        } else {
            &rendered
        };
        let lines: Vec<&str> = body.split('\n').collect();
        let first = lines[0].trim_start_matches(' ');
        let diff = lines[0].len() - first.len();
        out.push_str(&spaces(indent));
        out.push_str("- ");
        out.push_str(first);
        for line in &lines[1..] {
            // "If multi-line string, the space characters for indent have already been added,
            // so delete them" — and put them back below.
            let line = if is_string {
                line.strip_prefix(prefix.as_str()).unwrap_or(line)
            } else {
                line
            };
            out.push('\n');
            if line.len() > diff {
                out.push_str(&prefix);
                out.push_str(&line[diff..]);
            }
        }
        out.push('\n');
    }
}

/// A scalar's text; `content_indent` is where a literal block's lines start.
fn scalar_text(node: &Node, content_indent: usize) -> String {
    match node {
        Node::Null => "null".to_owned(),
        Node::Bool(b) => b.to_string(),
        Node::Int(i) => i.to_string(),
        Node::Uint(u) => u.to_string(),
        Node::Float(f) => format_float(*f),
        Node::Str(s) => string_text(s, content_indent),
        Node::Verbatim(s) => s.clone(),
        Node::Map(_) => "{}".to_owned(),
        Node::Seq(_) => "[]".to_owned(),
    }
}

/// `encodeString` then `StringNode.String` (encode.go:590, ast/ast.go:811): quoted with
/// `strconv.Quote` when needed; otherwise a string holding its line break becomes a literal
/// block, and anything else is written as it is — control characters included.
fn string_text(s: &str, content_indent: usize) -> String {
    if is_need_quoted(s) {
        return go_quote(s);
    }
    let lbc = detect_line_break_character(s);
    if s.contains(lbc) {
        let pad = spaces(content_indent);
        let header = literal_block_header(s, lbc);
        let lines: Vec<String> = s.split(lbc).map(|l| format!("{pad}{l}")).collect();
        let joined = lines.join(lbc);
        let once = joined
            .strip_suffix(&format!("{lbc}{pad}"))
            .unwrap_or(&joined);
        let block = once.strip_suffix(&pad).unwrap_or(once);
        return format!("{header}{lbc}{block}");
    }
    s.to_owned()
}

/// Port of `token.DetectLineBreakCharacter` (token/token.go:1165). A string with no line break
/// at all answers `"\r\n"`, because both counts are zero.
fn detect_line_break_character(s: &str) -> &'static str {
    let nc = s.matches('\n').count();
    let rc = s.matches('\r').count();
    let rnc = s.matches("\r\n").count();
    if nc == rnc && rc == rnc {
        "\r\n"
    } else if rc > nc {
        "\r"
    } else {
        "\n"
    }
}

/// Port of `token.LiteralBlockHeader` (token/token.go:718).
fn literal_block_header(s: &str, lbc: &str) -> &'static str {
    if !s.contains(lbc) {
        ""
    } else if s.ends_with(&format!("{lbc}{lbc}")) {
        "|+"
    } else if s.ends_with(lbc) {
        "|"
    } else {
        "|-"
    }
}

/// `reservedEncKeywordMap` (token/token.go:341): the null and boolean keywords, and the YAML 1.1
/// booleans kept only so the encoder quotes them. **Not** `.inf` or `.nan`, which are decode-only.
const RESERVED_ENC_KEYWORDS: [&str; 26] = [
    "null", "Null", "NULL", "~", "true", "True", "TRUE", "false", "False", "FALSE", "y", "Y",
    "yes", "Yes", "YES", "n", "N", "no", "No", "NO", "on", "On", "ON", "off", "Off", "OFF",
];

/// Port of `token.IsNeedQuoted` (token/token.go:678).
pub fn is_need_quoted(value: &str) -> bool {
    if value.is_empty() || RESERVED_ENC_KEYWORDS.contains(&value) || is_number(value) {
        return true;
    }
    if value == "-" {
        return true;
    }
    let bytes = value.as_bytes();
    if matches!(
        bytes[0],
        b'*' | b'&'
            | b'['
            | b'{'
            | b'}'
            | b']'
            | b','
            | b'!'
            | b'|'
            | b'>'
            | b'%'
            | b'\''
            | b'"'
            | b'@'
            | b' '
            | b'`'
    ) {
        return true;
    }
    if matches!(bytes[bytes.len() - 1], b':' | b' ') {
        return true;
    }
    if is_timestamp(value) {
        return true;
    }
    for (i, c) in value.char_indices() {
        match c {
            '#' | '\\' => return true,
            ':' | '-' if bytes.get(i + 1) == Some(&b' ') => return true,
            _ => {}
        }
    }
    false
}

/// Port of `isNumber` (token/token.go:568): `toNumber` succeeded, **or failed only because the
/// value is out of range** — an overflowing integer is still quoted.
fn is_number(value: &str) -> bool {
    if value.is_empty() || value.starts_with('_') {
        return false;
    }
    let dot_count = value.matches('.').count();
    if dot_count > 1 {
        return false;
    }
    let is_negative = value.starts_with('-');
    let trimmed = value.strip_prefix('+').unwrap_or(value);
    let trimmed = trimmed.strip_prefix('-').unwrap_or(trimmed);
    let normalized = trimmed.replace('_', "");

    enum Kind {
        Int(u32),
        Float,
    }
    let (digits, kind) = if let Some(rest) = normalized.strip_prefix("0x") {
        (rest.to_owned(), Kind::Int(16))
    } else if let Some(rest) = normalized.strip_prefix("0o") {
        (rest.to_owned(), Kind::Int(8))
    } else if let Some(rest) = normalized.strip_prefix("0b") {
        (rest.to_owned(), Kind::Int(2))
    } else if normalized.starts_with('0') && normalized.len() > 1 && dot_count == 0 {
        (normalized.clone(), Kind::Int(8))
    } else if dot_count == 1 {
        (normalized.clone(), Kind::Float)
    } else {
        (normalized.clone(), Kind::Int(10))
    };

    match kind {
        // `strconv.ParseInt`/`ParseUint`: a syntax error is "not a number", a range error is.
        // Both answer the same thing for any string of valid digits, so validity is the test.
        // The negative branch prefixes `-` to digits that may themselves start with a sign
        // (`--5`), which `ParseInt` rejects as syntax.
        Kind::Int(base) => {
            !digits.is_empty()
                && !(is_negative && digits.starts_with(['-', '+']))
                && digits.chars().all(|c| c.is_digit(base))
        }
        Kind::Float => {
            let text = if is_negative {
                format!("-{digits}")
            } else {
                digits
            };
            go_parse_float_ok_or_range(&text)
        }
    }
}

/// Whether `strconv.ParseFloat(text, 64)` answers a value or `ErrRange` — i.e. whether the text
/// is syntactically a Go float. The texts reaching this have exactly one `.` and no `_`, so Go's
/// hex and underscore forms cannot occur; `inf`/`nan` spellings cannot carry the dot.
fn go_parse_float_ok_or_range(text: &str) -> bool {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    // Rust also accepts `inf`, `infinity` and `nan`, none of which can hold a single `.`; and a
    // sign after the sign, which Go rejects.
    if unsigned.is_empty() || unsigned.starts_with(['-', '+']) {
        return false;
    }
    let valid = unsigned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    valid && text.parse::<f64>().is_ok()
}

/// Port of `isTimestamp` (token/token.go:668): `time.Parse` under any of `RFC3339Nano`, its
/// lower-case-`t` twin, `DateTime`, `DateOnly` and `"15:4"`.
fn is_timestamp(value: &str) -> bool {
    parse_rfc3339ish(value, b'T')
        || parse_rfc3339ish(value, b't')
        || parse_date_time(value)
        || parse_date_only(value).is_some_and(|rest| rest.is_empty())
        || parse_hour_minute(value)
}

/// `2006-01-02`, checked as `time.Parse` checks it: four-digit year, two-digit month in 1..=12,
/// two-digit day valid for that month (leap years included). Answers the unparsed remainder.
fn parse_date_only(value: &str) -> Option<&str> {
    let b = value.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits_n(&b[0..4])?;
    let month = digits_n(&b[5..7])?;
    let day = digits_n(&b[8..10])?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > days {
        return None;
    }
    Some(&value[10..])
}

fn digits_n(b: &[u8]) -> Option<u32> {
    if b.iter().all(u8::is_ascii_digit) {
        std::str::from_utf8(b).ok()?.parse().ok()
    } else {
        None
    }
}

/// `15:04:05` with fixed-width fields — `stdHour` takes one or two digits, the others two.
/// Answers the remainder.
fn parse_clock(value: &str) -> Option<&str> {
    let b = value.as_bytes();
    let hour_len = if b.len() > 1 && b[1].is_ascii_digit() {
        2
    } else {
        1
    };
    let hour = digits_n(b.get(..hour_len)?)?;
    if hour >= 24 {
        return None;
    }
    let rest = &b[hour_len..];
    if rest.len() < 6 || rest[0] != b':' || rest[3] != b':' {
        return None;
    }
    let minute = digits_n(&rest[1..3])?;
    let second = digits_n(&rest[4..6])?;
    if minute >= 60 || second >= 60 {
        return None;
    }
    Some(&value[hour_len + 6..])
}

/// `.999999999`: optional; when present, a dot and at least one digit.
fn parse_fraction(value: &str) -> Option<&str> {
    let Some(rest) = value.strip_prefix('.') else {
        return Some(value);
    };
    let n = rest.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 {
        return None;
    }
    Some(&rest[n..])
}

/// `Z07:00`: `Z`, or a sign, two-digit hours, a colon and two-digit minutes.
fn parse_zone(value: &str) -> Option<&str> {
    if let Some(rest) = value.strip_prefix('Z') {
        return Some(rest);
    }
    let b = value.as_bytes();
    if b.len() < 6 || !matches!(b[0], b'+' | b'-') || b[3] != b':' {
        return None;
    }
    let hours = digits_n(&b[1..3])?;
    let minutes = digits_n(&b[4..6])?;
    if hours > 24 || minutes > 60 {
        return None;
    }
    Some(&value[6..])
}

fn parse_rfc3339ish(value: &str, separator: u8) -> bool {
    let Some(rest) = parse_date_only(value) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(separator as char) else {
        return false;
    };
    parse_clock(rest)
        .and_then(parse_fraction)
        .and_then(parse_zone)
        .is_some_and(str::is_empty)
}

fn parse_date_time(value: &str) -> bool {
    parse_date_only(value)
        .and_then(|rest| rest.strip_prefix(' '))
        .and_then(parse_clock)
        .and_then(parse_fraction)
        .is_some_and(str::is_empty)
}

/// `"15:4"`: an hour of one or two digits in 0..24, a colon, a minute of one or two digits in
/// 0..60, and nothing else.
fn parse_hour_minute(value: &str) -> bool {
    let Some((hour, minute)) = value.split_once(':') else {
        return false;
    };
    let small = |s: &str, limit: u32| {
        (1..=2).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<u32>().is_ok_and(|n| n < limit)
    };
    small(hour, 24) && small(minute, 60)
}

/// Port of `encodeFloat` (encode.go:541): `strconv.FormatFloat(v, 'g', -1, 64)`, then `.0` when
/// the text has neither a point nor an exponent.
pub fn format_float(v: f64) -> String {
    if v.is_nan() {
        return ".nan".to_owned();
    }
    if v == f64::INFINITY {
        return ".inf".to_owned();
    }
    if v == f64::NEG_INFINITY {
        return "-.inf".to_owned();
    }
    let text = go_format_float_g(v);
    if !text.contains('.') && !text.contains('e') {
        format!("{text}.0")
    } else {
        text
    }
}

/// `strconv.FormatFloat(v, 'g', -1, 64)` for a finite `v`: the shortest round-tripping digits,
/// in `%e` form when the decimal exponent is below -4 or at least 6 (`eprec = 6` for the
/// shortest precision, ftoa.go), with a sign and at least two exponent digits.
fn go_format_float_g(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    // Rust's `{:e}` is the shortest round-trip representation: `d.ddde±x` without padding.
    let sci = format!("{:e}", v.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if v < 0.0 { "-" } else { "" };
    if !(-4..6).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{frac}e{esign}{:02}", exp.abs());
    }
    // Fixed notation with exactly the significant digits.
    let dp = exp + 1; // digits before the point
    let nd = digits.len() as i32;
    let body = if dp <= 0 {
        format!("0.{}{digits}", "0".repeat((-dp) as usize))
    } else if dp >= nd {
        format!("{digits}{}", "0".repeat((dp - nd) as usize))
    } else {
        let (int, frac) = digits.split_at(dp as usize);
        format!("{int}.{frac}")
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_containers_are_flow_markers() {
        assert_eq!(marshal(&Node::Map(vec![])), "{}\n");
        let doc = MapBuilder::new()
            .field("m", Node::Map(vec![]))
            .field("l", Node::Seq(vec![]))
            .field("p", Node::Null)
            .build();
        assert_eq!(marshal(&doc), "m: {}\nl: []\np: null\n");
    }

    #[test]
    fn floats_follow_format_float_g() {
        for (v, want) in [
            (0.0, "0.0"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (0.5, "0.5"),
            (1e20, "1e+20"),
            (123456789.0, "1.23456789e+08"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (999999.0, "999999.0"),
            (1e6, "1e+06"),
            (-2.5, "-2.5"),
        ] {
            assert_eq!(format_float(v), want, "{v}");
        }
    }

    #[test]
    fn quoting_branches() {
        for (s, quoted) in [
            ("", true),
            ("plain", false),
            ("y", true),
            ("123", true),
            ("0x1F", true),
            ("089", false),
            ("1.5", true),
            ("1.2.3", false),
            ("99999999999999999999999", true),
            ("-", true),
            ("a: b", true),
            ("a:b", false),
            ("a #b", true),
            ("a - b", true),
            ("a -b", false),
            ("x:", true),
            ("2006-01-02", true),
            ("2006-02-30", false),
            ("12:30", true),
            ("25:30", false),
            (".inf", false),
            ("*x", true),
        ] {
            assert_eq!(is_need_quoted(s), quoted, "{s:?}");
        }
    }

    #[test]
    fn literal_blocks_indent_from_the_owner() {
        let doc = MapBuilder::new()
            .field("k", Node::str("a\nb"))
            .field(
                "o",
                MapBuilder::new().field("i", Node::str("c\nd\n")).build(),
            )
            .build();
        assert_eq!(marshal(&doc), "k: |-\n  a\n  b\no:\n  i: |\n    c\n    d\n");
    }

    /// Every case of the goccy corpus, byte for byte.
    mod go_parity {
        use super::super::*;
        use serde_json::Value;

        fn corpus() -> Value {
            serde_json::from_str(include_str!("../../../fixtures/behaviour_goyaml.json"))
                .expect("behaviour_goyaml.json is generated by reference/dump")
        }

        #[test]
        fn strings_match_goccy() {
            let corpus = corpus();
            let cases = corpus["strings"].as_array().expect("strings");
            assert!(cases.len() > 400, "the corpus walks every ASCII byte");
            let mut failures = Vec::new();
            for case in cases {
                let s = case["input"].as_str().expect("input");
                let top = marshal(&MapBuilder::new().field("k", Node::str(s)).build());
                let nested = marshal(
                    &MapBuilder::new()
                        .field(
                            "outer",
                            MapBuilder::new()
                                .field("inner", Node::str(s))
                                .field("list", Node::strings(&[s, "x"]))
                                .build(),
                        )
                        .field(
                            "seq",
                            Node::Seq(vec![
                                MapBuilder::new()
                                    .field("name", Node::str("n"))
                                    .field("value", Node::str(s))
                                    .build(),
                            ]),
                        )
                        .build(),
                );
                let key = marshal(&MapBuilder::new().field(s, Node::Int(1)).build());
                for (what, got, want) in [
                    ("top", top, &case["top"]),
                    ("nested", nested, &case["nested"]),
                    ("key", key, &case["key"]),
                ] {
                    if Some(got.as_str()) != want.as_str() {
                        failures.push(format!("{what} {s:?}: got {got:?}, Go {want}"));
                    }
                }
            }
            assert!(
                failures.is_empty(),
                "{} mismatches:\n{}",
                failures.len(),
                failures.join("\n")
            );
        }

        #[test]
        fn floats_match_goccy() {
            for case in corpus()["floats"].as_array().expect("floats") {
                let v = case["input"].as_f64().expect("input");
                let got = marshal(&MapBuilder::new().field("k", Node::Float(v)).build());
                assert_eq!(Some(got.as_str()), case["yaml"].as_str(), "{v}");
            }
        }

        #[test]
        fn shapes_match_goccy() {
            let corpus = corpus();
            let shapes = &corpus["shapes"];
            let item = |a: &str, b: &[&str]| {
                MapBuilder::new()
                    .field("a", Node::str(a))
                    .field("b", Node::strings(b))
                    .build()
            };
            let one = |k: &str, v: Node| marshal(&MapBuilder::new().field(k, v).build());
            let cases: Vec<(&str, String)> = vec![
                ("nil_slice", one("l", Node::Seq(vec![]))),
                ("empty_slice", one("l", Node::Seq(vec![]))),
                ("nil_map", one("m", Node::Map(vec![]))),
                ("empty_map", one("m", Node::Map(vec![]))),
                (
                    "sorted_map",
                    one(
                        "m",
                        Node::string_map(
                            &[
                                ("b", "2"),
                                ("a", "1"),
                                ("B", "3"),
                                ("_", "4"),
                                ("10", "x"),
                                ("9", "y"),
                            ]
                            .iter()
                            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                            .collect::<std::collections::BTreeMap<_, _>>(),
                        ),
                    ),
                ),
                ("nil_ptr", one("p", Node::Null)),
                ("ptr", one("p", Node::str("s"))),
                ("zero_ptr", one("p", Node::Int(0))),
                (
                    "bools",
                    marshal(
                        &MapBuilder::new()
                            .field("t", Node::Bool(true))
                            .field("f", Node::Bool(false))
                            .build(),
                    ),
                ),
                (
                    "ints",
                    marshal(
                        &MapBuilder::new()
                            .field("a", Node::Int(-1))
                            .field("b", Node::Int(0))
                            .field("c", Node::Int(i64::MAX))
                            .build(),
                    ),
                ),
                ("uints", one("u", Node::Uint(u64::MAX))),
                (
                    "seq_of_maps",
                    one(
                        "s",
                        Node::Seq(vec![item("1", &["x", "y"]), item("2", &[]), item("3", &[])]),
                    ),
                ),
                (
                    "seq_of_seqs",
                    one(
                        "s",
                        Node::Seq(vec![
                            Node::strings(&["a", "b"]),
                            Node::Seq(vec![]),
                            Node::Seq(vec![]),
                            Node::strings(&["c"]),
                        ]),
                    ),
                ),
                (
                    "nested_empty",
                    one("o", MapBuilder::new().field("i", Node::Map(vec![])).build()),
                ),
                (
                    "multiline_in_seq",
                    one("s", Node::strings(&["a\nb", "c\nd\n"])),
                ),
                (
                    "multiline_deep",
                    one(
                        "a",
                        MapBuilder::new()
                            .field(
                                "b",
                                MapBuilder::new()
                                    .field("c", Node::Seq(vec![item("x\ny", &["p\nq"])]))
                                    .build(),
                            )
                            .build(),
                    ),
                ),
                (
                    "time_utc",
                    one("t", Node::Verbatim("2024-01-02T03:04:05Z".into())),
                ),
                (
                    "time_zero",
                    one("t", Node::Verbatim("0001-01-01T00:00:00Z".into())),
                ),
                ("time_ptr_nil", one("t", Node::Null)),
                ("string_map_any", marshal(&Node::Map(vec![]))),
                ("top_empty_struct", marshal(&Node::Map(vec![]))),
            ];
            for (name, got) in cases {
                assert_eq!(Some(got.as_str()), shapes[name].as_str(), "{name}");
            }
        }
    }
}
