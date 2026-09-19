//! Port of `golang.org/x/net@v0.56.0/html/charset` (charset.go) — how a fetched page is turned
//! into the UTF-8 the tokenizer ([`crate::go_html`]) reads. Not Mattermost source; it is what
//! `forceHTMLEncodingToUTF8` (app/opengraph.go:83) calls.
//!
//! # Which encoding, in Go's order
//!
//! [`determine_encoding`] looks at the first 1024 bytes only: a byte-order mark, then the
//! `charset` parameter of the response's `Content-Type` (through `mime.ParseMediaType`, so a
//! malformed header contributes nothing), then a `<meta>` prescan of the preview with the same
//! tokenizer, then "is the preview valid UTF-8 with a high bit set", and finally **windows-1252**.
//!
//! # Two ways to be UTF-8, and only one of them decodes
//!
//! `Lookup` wraps every encoding it returns, so it is never `encoding.Nop` — even for `utf-8`.
//! The document is therefore **decoded** (ill-formed bytes become U+FFFD, W3C maximal subpart,
//! the same rule as [`String::from_utf8_lossy`]) whenever the charset came from a BOM, the header
//! or a `<meta charset>`. Only the two paths that answer `encoding.Nop` — the preview heuristic,
//! and a prescan that named `utf-16` — hand the bytes through **untouched**, invalid bytes after
//! the first kilobyte included. [`ForcedUtf8::raw_invalid`] reports that case, because a Rust
//! string cannot carry what the Go string does from there on.
//!
//! # Which decoders are reproduced
//!
//! x/text and encoding_rs implement the same WHATWG tables, and for the single-byte encodings,
//! `x-user-defined` and UTF-8 they agree byte for byte **except** on the bytes a legacy table
//! leaves undefined: WHATWG decodes those to the C1 control of the same value, x/text to U+FFFD
//! ([`decode`] rewrites them). `fixtures/behaviour_opengraph.json` sweeps all 256 byte values
//! through every single-byte table on both sides. The multi-byte
//! decoders do **not** agree on ill-formed input (x/text's GBK, for one, rejects the four-byte
//! GB18030 sequences WHATWG accepts under that label), so for those the answer is
//! [`CharsetError::Unreproducible`] unless the document is plain ASCII that every decoder passes
//! through unchanged. UTF-16 is decoded when it is well-formed and refused otherwise, and
//! `replacement` is always refused. The caller forwards rather than guess.

use crate::go_html::{TokenType, Tokenizer};
use crate::go_html_tables::ENCODING_LABELS;

/// Why a document's decoding is not reproduced here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CharsetError {
    #[error("the {0} decoder is not reproduced for this input")]
    Unreproducible(&'static str),
}

/// An encoding as `charset.Lookup` returns it: the canonical name `htmlindex.Name` gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoEncoding {
    /// `encoding.Nop` — the bytes are passed through as they are.
    Nop,
    /// A `Lookup` result, by canonical name.
    Named(&'static str),
}

/// Port of `charset.Lookup` (charset.go:30): `htmlindex.Get`, which lower-cases and trims the
/// label with Go's Unicode rules before the table lookup.
pub fn lookup(label: &str) -> Option<&'static str> {
    let key = crate::utils::go_to_lower(label.trim());
    ENCODING_LABELS
        .binary_search_by(|(l, _)| l.as_bytes().cmp(key.as_bytes()))
        .ok()
        .map(|i| ENCODING_LABELS[i].1)
}

/// `boms` (charset.go:252).
const BOMS: [(&[u8], &str); 3] = [
    (&[0xfe, 0xff], "utf-16be"),
    (&[0xff, 0xfe], "utf-16le"),
    (&[0xef, 0xbb, 0xbf], "utf-8"),
];

/// Port of `DetermineEncoding` (charset.go:52): the encoding, its name, and whether it is
/// certain.
pub fn determine_encoding(content: &[u8], content_type: &str) -> (GoEncoding, &'static str, bool) {
    let mut content = &content[..content.len().min(1024)];

    for (bom, enc) in BOMS {
        if content.starts_with(bom) {
            // Every BOM label is in the table.
            if let Some(name) = lookup(enc) {
                return (GoEncoding::Named(name), name, true);
            }
        }
    }

    if let Some(params) = parse_media_type_params(content_type) {
        if let Some(cs) = params.iter().find(|(k, _)| k == "charset").map(|(_, v)| v) {
            if let Some(name) = lookup(cs) {
                return (GoEncoding::Named(name), name, true);
            }
        }
    }

    if !content.is_empty() {
        if let Some((e, name)) = prescan(content) {
            return (e, name, false);
        }
    }

    // "Try to detect UTF-8. First eliminate any partial rune at the end."
    let len = content.len();
    let mut i = len;
    while i > 0 && i + 3 > len {
        i -= 1;
        let b = content[i];
        if b < 0x80 {
            break;
        }
        // `utf8.RuneStart`
        if b & 0xC0 != 0x80 {
            content = &content[..i];
            break;
        }
    }
    let has_high_bit = content.iter().any(|&c| c >= 0x80);
    if has_high_bit && std::str::from_utf8(content).is_ok() {
        return (GoEncoding::Nop, "utf-8", false);
    }
    (GoEncoding::Named("windows-1252"), "windows-1252", false)
}

/// Port of `prescan` (charset.go:157): the first `<meta>` in the preview that names an encoding
/// in a way the algorithm accepts.
fn prescan(content: &[u8]) -> Option<(GoEncoding, &'static str)> {
    let mut z = Tokenizer::new(content);
    loop {
        match z.next_token() {
            TokenType::Error => return None,
            TokenType::StartTag | TokenType::SelfClosingTag => {
                let (tag_name, mut has_attr) = z.tag_name();
                if tag_name.as_deref() != Some(b"meta".as_slice()) {
                    continue;
                }
                let mut seen: Vec<Vec<u8>> = Vec::new();
                let mut got_pragma = false;
                #[derive(PartialEq)]
                enum Need {
                    DontKnow,
                    DoNeedPragma,
                    DoNotNeedPragma,
                }
                let mut need_pragma = Need::DontKnow;
                let mut name: Option<&'static str> = None;
                // `e` in Go; the encoding is determined by the name here.
                let mut found: Option<GoEncoding> = None;
                while has_attr {
                    let (key, val, more) = z.tag_attr();
                    has_attr = more;
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.push(key.clone());
                    let val = val.to_ascii_lowercase();
                    match key.as_slice() {
                        b"http-equiv" => {
                            if val == b"content-type" {
                                got_pragma = true;
                            }
                        }
                        b"content" => {
                            if found.is_none() {
                                let meta = from_meta_element(&String::from_utf8_lossy(&val));
                                if !meta.is_empty() {
                                    name = lookup(&meta);
                                    found = name.map(GoEncoding::Named);
                                    if found.is_some() {
                                        need_pragma = Need::DoNeedPragma;
                                    }
                                }
                            }
                        }
                        b"charset" => {
                            name = lookup(&String::from_utf8_lossy(&val));
                            found = name.map(GoEncoding::Named);
                            need_pragma = Need::DoNotNeedPragma;
                        }
                        _ => {}
                    }
                }
                if need_pragma == Need::DontKnow
                    || (need_pragma == Need::DoNeedPragma && !got_pragma)
                {
                    continue;
                }
                if name.is_some_and(|n| n.starts_with("utf-16")) {
                    return Some((GoEncoding::Nop, "utf-8"));
                }
                if let (Some(e), Some(n)) = (found, name) {
                    return Some((e, n));
                }
            }
            _ => {}
        }
    }
}

/// Port of `fromMetaElement` (charset.go:220).
fn from_meta_element(s: &str) -> String {
    let ws: &[char] = &[' ', '\t', '\n', '\x0c', '\r'];
    let mut s = s;
    while !s.is_empty() {
        let Some(cs_loc) = s.find("charset") else {
            return String::new();
        };
        s = s[cs_loc + "charset".len()..].trim_start_matches(ws);
        let Some(rest) = s.strip_prefix('=') else {
            continue;
        };
        s = rest.trim_start_matches(ws);
        if s.is_empty() {
            return String::new();
        }
        let q = s.as_bytes()[0];
        if q == b'"' || q == b'\'' {
            let rest = &s[1..];
            return match rest.find(char::from(q)) {
                Some(close) => rest[..close].to_owned(),
                None => String::new(),
            };
        }
        let end = s
            .find([';', ' ', '\t', '\n', '\x0c', '\r'])
            .unwrap_or(s.len());
        return s[..end].to_owned();
    }
    String::new()
}

/// What `forceHTMLEncodingToUTF8` hands the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedUtf8 {
    /// The document the tokenizer reads.
    pub bytes: Vec<u8>,
    /// The bytes were passed through undecoded (`encoding.Nop`) and are not valid UTF-8.
    pub raw_invalid: bool,
}

/// Port of `forceHTMLEncodingToUTF8` (app/opengraph.go:83) over `charset.NewReader`
/// (charset.go:112): the preview is the first 1024 bytes; an **empty** body makes `NewReader`
/// fail with `io.EOF`, and Go then reads the original — empty — body.
pub fn force_html_encoding_to_utf8(
    body: &[u8],
    content_type: &str,
) -> Result<ForcedUtf8, CharsetError> {
    if body.is_empty() {
        return Ok(ForcedUtf8 {
            bytes: Vec::new(),
            raw_invalid: false,
        });
    }
    let preview = &body[..body.len().min(1024)];
    let (e, _, _) = determine_encoding(preview, content_type);
    let name = match e {
        GoEncoding::Nop => {
            return Ok(ForcedUtf8 {
                bytes: body.to_vec(),
                raw_invalid: std::str::from_utf8(body).is_err(),
            });
        }
        GoEncoding::Named(name) => name,
    };
    let bytes = decode(name, body)?;
    Ok(ForcedUtf8 {
        bytes,
        raw_invalid: false,
    })
}

/// The decoders named by canonical name, as `e.NewDecoder()` would run them over the whole body.
pub fn decode(name: &'static str, body: &[u8]) -> Result<Vec<u8>, CharsetError> {
    match name {
        "utf-8" => Ok(String::from_utf8_lossy(body).into_owned().into_bytes()),
        "gbk" | "gb18030" | "big5" | "euc-jp" | "shift_jis" | "euc-kr" => {
            if body.is_ascii() {
                Ok(body.to_vec())
            } else {
                Err(CharsetError::Unreproducible(name))
            }
        }
        "iso-2022-jp" => {
            // ESC, SO and SI are what move it out of its ASCII state.
            if body.is_ascii() && !body.iter().any(|&b| matches!(b, 0x1b | 0x0e | 0x0f)) {
                Ok(body.to_vec())
            } else {
                Err(CharsetError::Unreproducible(name))
            }
        }
        "utf-16be" | "utf-16le" => {
            // Well-formed UTF-16 has one decoding; the two libraries' error paths are not
            // compared, so ill-formed input (an odd byte, a lone surrogate) is refused.
            let encoding = if name == "utf-16be" {
                encoding_rs::UTF_16BE
            } else {
                encoding_rs::UTF_16LE
            };
            let (text, had_errors) = encoding.decode_without_bom_handling(body);
            if had_errors {
                return Err(CharsetError::Unreproducible(name));
            }
            Ok(text.into_owned().into_bytes())
        }
        "replacement" => Err(CharsetError::Unreproducible(name)),
        _ => {
            let encoding = encoding_rs::Encoding::for_label(name.as_bytes())
                .ok_or(CharsetError::Unreproducible(name))?;
            let (text, _) = encoding.decode_without_bom_handling(body);
            // WHATWG maps a byte its legacy table leaves undefined to the C1 control of the same
            // value (windows-1252's 0x81 is U+0081); x/text's charmaps answer U+FFFD for it
            // instead. No single-byte table maps a defined byte into U+0080..=U+009F, so the
            // rewrite is exact — the 256-byte sweep in the fixture checks every table.
            Ok(text
                .chars()
                .map(|c| {
                    if ('\u{80}'..='\u{9f}').contains(&c) {
                        '\u{FFFD}'
                    } else {
                        c
                    }
                })
                .collect::<String>()
                .into_bytes())
        }
    }
}

// --- mime.ParseMediaType, as far as `DetermineEncoding` asks ---------------------------------
//
// `crates/mm-api/src/multipart.rs` carries the full transcription for multipart bodies; this is
// the same function for the one question asked here (was there an error, and what is `charset`),
// in this crate because mm-model sits below mm-api.

/// Port of `isTSpecial` (mime/grammar.go:9).
fn is_tspecial(c: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&c)
}

/// Port of `isTokenChar` (mime/grammar.go:40).
fn is_token_char(c: u8) -> bool {
    c > b' ' && c < 0x7f && !is_tspecial(c)
}

fn consume_token(v: &str) -> (&str, &str) {
    let end = v.bytes().position(|b| !is_token_char(b)).unwrap_or(v.len());
    v.split_at(end)
}

fn consume_value(v: &str) -> Option<(Vec<u8>, &str)> {
    if !v.starts_with('"') {
        let (t, rest) = consume_token(v);
        return if t.is_empty() {
            None
        } else {
            Some((t.as_bytes().to_vec(), rest))
        };
    }
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some((out, &v[i + 1..])),
            b'\\' if i + 1 < b.len() && is_tspecial(b[i + 1]) => {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            b'\r' | b'\n' => return None,
            c => out.push(c),
        }
        i += 1;
    }
    None
}

fn consume_media_param(v: &str) -> Option<(String, Vec<u8>, &str)> {
    let rest = v.trim_start().strip_prefix(';')?.trim_start();
    let (param, rest) = consume_token(rest);
    if param.is_empty() {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, rest) = consume_value(rest)?;
    Some((param.to_ascii_lowercase(), value, rest))
}

fn percent_hex_unescape(s: &[u8]) -> Option<Vec<u8>> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'%' {
            out.push(s[i]);
            i += 1;
            continue;
        }
        let hi = s.get(i + 1).copied().and_then(hex)?;
        let lo = s.get(i + 2).copied().and_then(hex)?;
        out.push(hi << 4 | lo);
        i += 3;
    }
    Some(out)
}

fn decode_2231_enc(v: &[u8]) -> Option<Vec<u8>> {
    let first = v.iter().position(|&c| c == b'\'')?;
    let rest = &v[first + 1..];
    let second = rest.iter().position(|&c| c == b'\'')?;
    let charset = v[..first].to_ascii_lowercase();
    if charset != b"us-ascii" && charset != b"utf-8" {
        return None;
    }
    percent_hex_unescape(&rest[second + 1..])
}

/// Parameters in insertion order, as `(name, value bytes)`.
type Params = Vec<(String, Vec<u8>)>;

/// Port of `mime.ParseMediaType` (mime/mediatype.go:141), answering only the parameters, and
/// `None` wherever Go answers an error. Values are Go strings (bytes), rendered lossily — the
/// only consumer is a label lookup, which no invalid byte can satisfy.
fn parse_media_type_params(v: &str) -> Option<Vec<(String, String)>> {
    let base = v.split(';').next().unwrap_or(v);
    let media_type = crate::utils::go_to_lower(base).trim().to_owned();
    // checkMediaTypeDisposition
    let (typ, rest) = consume_token(&media_type);
    if typ.is_empty() {
        return None;
    }
    if !rest.is_empty() {
        let rest = rest.strip_prefix('/')?;
        let (sub, rest) = consume_token(rest);
        if sub.is_empty() || !rest.is_empty() {
            return None;
        }
    }

    let mut params: Params = Vec::new();
    let mut continuation: Vec<(String, Params)> = Vec::new();
    let mut rest = &v[base.len()..];
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some((key, value, after)) = consume_media_param(rest) else {
            if rest.trim() == ";" {
                break;
            }
            return None;
        };
        let pmap = match key.split_once('*') {
            Some((base_name, _)) => {
                let idx = match continuation.iter().position(|(k, _)| k == base_name) {
                    Some(i) => i,
                    None => {
                        continuation.push((base_name.to_owned(), Vec::new()));
                        continuation.len() - 1
                    }
                };
                &mut continuation[idx].1
            }
            None => &mut params,
        };
        match pmap.iter_mut().find(|(k, _)| *k == key) {
            Some((_, existing)) if *existing != value => return None,
            Some(_) => {}
            None => pmap.push((key, value)),
        }
        rest = after;
    }

    for (key, pieces) in continuation {
        let get = |k: &str| pieces.iter().find(|(p, _)| p == k).map(|(_, v)| v);
        let set = |params: &mut Params, value: Vec<u8>| match params
            .iter_mut()
            .find(|(k, _)| *k == key)
        {
            Some((_, v)) => *v = value,
            None => params.push((key.clone(), value)),
        };
        if let Some(v) = get(&format!("{key}*")) {
            if let Some(dec) = decode_2231_enc(v) {
                set(&mut params, dec);
            }
            continue;
        }
        let mut buf = Vec::new();
        let mut valid = false;
        for n in 0.. {
            let simple = format!("{key}*{n}");
            if let Some(v) = get(&simple) {
                valid = true;
                buf.extend_from_slice(v);
                continue;
            }
            let Some(v) = get(&format!("{simple}*")) else {
                break;
            };
            valid = true;
            if n == 0 {
                if let Some(dec) = decode_2231_enc(v) {
                    buf.extend_from_slice(&dec);
                }
            } else if let Some(dec) = percent_hex_unescape(v) {
                buf.extend_from_slice(&dec);
            }
        }
        if valid {
            set(&mut params, buf);
        }
    }
    Some(
        params
            .into_iter()
            .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_wins_over_a_meta_but_not_over_a_bom() {
        let doc = b"<meta charset=iso-8859-2>\xa1";
        assert_eq!(
            determine_encoding(doc, "text/html; charset=koi8-r").1,
            "koi8-r"
        );
        assert_eq!(determine_encoding(doc, "text/html").1, "iso-8859-2");
        assert_eq!(
            determine_encoding(b"\xef\xbb\xbfx", "text/html; charset=koi8-r").1,
            "utf-8"
        );
    }

    #[test]
    fn a_duplicate_charset_is_a_parse_error_and_is_ignored() {
        assert_eq!(
            determine_encoding(b"abc", "text/html; charset=koi8-r; charset=utf-8").1,
            "windows-1252"
        );
    }

    #[test]
    fn valid_utf8_in_the_preview_is_passed_through() {
        assert_eq!(determine_encoding("é!".as_bytes(), "").0, GoEncoding::Nop);
        // Go drops the last rune before its validity test **even when it is complete**, so a
        // preview that is one multi-byte character and nothing else has no high bit left.
        assert_eq!(determine_encoding("é".as_bytes(), "").1, "windows-1252");
        let forced = force_html_encoding_to_utf8(b"\xc3\xa9! \xff", "").unwrap();
        assert!(forced.raw_invalid);
        assert_eq!(forced.bytes, b"\xc3\xa9! \xff");
    }

    #[test]
    fn a_meta_content_needs_the_pragma() {
        let doc = b"<meta content='text/html; charset=koi8-r'>";
        assert_eq!(determine_encoding(doc, "").1, "windows-1252");
        let doc = b"<meta http-equiv=Content-Type content='text/html; charset=koi8-r'>";
        assert_eq!(determine_encoding(doc, "").1, "koi8-r");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_opengraph.json"))
            .expect("the fixture is JSON")
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        use base64::Engine;
        match v.as_str() {
            Some(s) => base64::engine::general_purpose::STANDARD
                .decode(s)
                .expect("base64"),
            None => Vec::new(),
        }
    }

    /// `charset.Lookup` over every htmlindex label plus a few that must fail or fold.
    #[test]
    fn lookup_matches_go_for_every_label() {
        let o = oracle();
        let cases = o["labels"].as_array().expect("labels");
        assert!(cases.len() > 228);
        for case in cases {
            let label = case["label"].as_str().expect("a label");
            let theirs = case["name"].as_str().expect("a name");
            assert_eq!(lookup(label).unwrap_or(""), theirs, "{label:?}");
        }
    }

    /// `DetermineEncoding` and `NewReader`. Where this port refuses a decoder, the name must
    /// still agree and the refusal must be one of the documented ones.
    #[test]
    fn determine_encoding_and_decoding_match_go() {
        let mut refused = 0;
        for case in oracle()["charset"].as_array().expect("cases") {
            let body = bytes(&case["body"]);
            let ct = case["content_type"].as_str().expect("ct");
            let (_, name, certain) = determine_encoding(&body[..body.len().min(1024)], ct);
            assert_eq!(
                name,
                case["name"].as_str().expect("name"),
                "{ct:?} {body:?}"
            );
            assert_eq!(
                certain,
                case["certain"].as_bool().expect("certain"),
                "{ct:?} {body:?}"
            );
            let theirs = if case["err"].as_bool() == Some(true) {
                body.clone()
            } else {
                bytes(&case["out"])
            };
            match force_html_encoding_to_utf8(&body, ct) {
                Ok(ours) => assert_eq!(ours.bytes, theirs, "{ct:?} {body:?}"),
                Err(CharsetError::Unreproducible(n)) => {
                    assert!(
                        matches!(n, "shift_jis" | "iso-2022-jp" | "replacement"),
                        "an unexpected refusal for {n}"
                    );
                    refused += 1;
                }
            }
        }
        assert_eq!(
            refused, 3,
            "one non-ASCII Shift_JIS, one ISO-2022-JP, one replacement"
        );
    }

    /// Every single-byte table, all 256 byte values: x/text and encoding_rs agree.
    #[test]
    fn every_single_byte_table_matches_go() {
        let all: Vec<u8> = (0..=255).collect();
        let cases = oracle()["sweep"].as_array().expect("sweep").clone();
        assert_eq!(cases.len(), 29);
        for case in cases {
            let name = case["name"].as_str().expect("a name");
            let canonical = lookup(name).expect("a label Go knows");
            assert_eq!(
                decode(canonical, &all).expect("decodes"),
                bytes(&case["out"]),
                "{name}"
            );
        }
    }
}
