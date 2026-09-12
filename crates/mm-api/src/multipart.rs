//! `multipart/form-data`, as much of it as `http.Request.ParseMultipartForm` needs.
//!
//! # Why this is written out rather than pulled in
//!
//! `createEmoji` is the only migrated route that takes a multipart body, and the answers it gives
//! are entirely decided by Go's parser: which parts become `Form.Value`, which become `Form.File`,
//! and which are silently **dropped**. A general-purpose crate would agree on the common shape and
//! diverge on precisely the edges that produce a different HTTP status — a part with no `name`, a
//! `filename=""`, a `Content-Disposition` that does not parse — and the whole point of the port is
//! that those edges match. Every rule below cites the Go line it comes from and is pinned by
//! `fixtures/behaviour_emoji_upload.json`, whose corpus is run through `ParseMultipartForm` itself.
//!
//! # The four rules that decide a part's fate
//!
//! From `Reader.readForm` (mime/multipart/formdata.go:60) and `Part.FormName` /
//! `Part.FileName` (mime/multipart/multipart.go:91):
//!
//! 1. The part's `Content-Disposition` must parse **and** its disposition must be `form-data`.
//!    Anything else yields an empty name.
//! 2. An empty `name` parameter drops the part — `if name == "" { continue }`. Not an error.
//! 3. `filename` decides value-versus-file, and **an empty `filename` is no filename at all**, so
//!    `filename=""` makes a *value* part.
//! 4. A filename that survives is passed through `filepath.Base`, so `../../etc/passwd` arrives as
//!    `passwd`. `createEmoji` only reads the extension off it, but the stripping is Go's.
//!
//! # Errors are one answer
//!
//! `createEmoji` turns every failure of `ParseMultipartForm` into the same 400
//! (`api.emoji.create.parse.app_error`), so this module distinguishes causes for the log and not
//! for the wire.

use std::collections::HashMap;

use thiserror::Error;

/// Why a body could not be read as `multipart/form-data`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MultipartError {
    /// `http.Request.multipartReader` (net/http/request.go:474): no `Content-Type`, or one whose
    /// media type is not `multipart/form-data`. Go's `ErrNotMultipart`.
    #[error("the request is not multipart/form-data")]
    NotMultipart,
    /// The media type was right but carried no `boundary` parameter. Go's `ErrMissingBoundary`.
    #[error("the multipart Content-Type carries no boundary")]
    MissingBoundary,
    /// `Reader.nextPart` ran off the end of the body without meeting the closing delimiter, or met
    /// a line where a delimiter had to be.
    #[error("the multipart body is malformed: {0}")]
    Malformed(&'static str),
    /// `readForm`'s `maxParts` (mime/multipart/formdata.go:83) — 1000 parts, then
    /// `ErrMessageTooLarge`.
    #[error("the multipart body has more than 1000 parts")]
    TooManyParts,
}

/// Go's `maxParts` default (mime/multipart/formdata.go:83). Settable there through a GODEBUG; the
/// server does not set it.
const MAX_PARTS: usize = 1000;

/// One file part, as `multipart.FileHeader` carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePart {
    /// `FileHeader.Filename` — already through `filepath.Base`.
    pub filename: String,
    /// The part's bytes.
    pub data: Vec<u8>,
}

impl FilePart {
    /// `FileHeader.Size`, which `createEmoji` compares against `MaxEmojiFileSize`.
    pub fn size(&self) -> i64 {
        i64::try_from(self.data.len()).unwrap_or(i64::MAX)
    }
}

/// Port of `multipart.Form` (mime/multipart/formdata.go:20).
///
/// Both maps hold **lists**, because a form may repeat a name and Go appends; `createEmoji` reads
/// `props["emoji"][0]` and `File["image"][0]`, so the order within a name is load-bearing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Form {
    pub value: HashMap<String, Vec<String>>,
    pub file: HashMap<String, Vec<FilePart>>,
}

impl Form {
    /// `m.Value[name]` — the first value under `name`, or `None` when there is none.
    pub fn first_value(&self, name: &str) -> Option<&str> {
        self.value.get(name)?.first().map(String::as_str)
    }

    /// `m.File[name]` — the first file part under `name`.
    pub fn first_file(&self, name: &str) -> Option<&FilePart> {
        self.file.get(name)?.first()
    }
}

/// Port of `http.Request.multipartReader` (net/http/request.go:474) followed by
/// `Reader.ReadForm`.
///
/// `content_type` is the raw header value; `body` is the whole body, already buffered. Go streams
/// and spills to a temporary file past `maxMemory`; the body here is capped at 512 KiB by the
/// caller before this is reached, so there is nothing to spill.
pub fn parse_form(content_type: Option<&str>, body: &[u8]) -> Result<Form, MultipartError> {
    let boundary = multipart_boundary(content_type)?;
    read_form(body, &boundary)
}

/// `mime.ParseMediaType` applied to `Content-Type`, narrowed to the two things
/// `multipartReader` asks of it.
///
/// Go additionally accepts `multipart/mixed`, but only when its caller passes `allowMixed` —
/// `ParseMultipartForm` does not, so `multipart/form-data` is the only media type here.
fn multipart_boundary(content_type: Option<&str>) -> Result<String, MultipartError> {
    let raw = content_type.unwrap_or_default();
    if raw.is_empty() {
        return Err(MultipartError::NotMultipart);
    }
    let (media_type, params) = parse_media_type(raw).ok_or(MultipartError::NotMultipart)?;
    if media_type != "multipart/form-data" {
        return Err(MultipartError::NotMultipart);
    }
    params
        .get("boundary")
        .cloned()
        .ok_or(MultipartError::MissingBoundary)
}

/// Port of `mime.ParseMediaType` (mime/mediatype.go:112), for the two headers this module reads.
///
/// # The three refusals that are easy to miss
///
/// Go returns an error — and therefore, for a part header, **drops the part** — on each of these,
/// where a lenient parser would carry on:
///
/// * `name` with no `=` at all, and `name=` with nothing after it. A parameter must have a
///   non-empty token or a quoted string as its value.
/// * A **repeated** attribute. `name="a"; name="b"` is not "first wins", it is a parse error.
/// * An unterminated quoted string.
///
/// What is otherwise reproduced: the media type is lower-cased and trimmed, attribute names are
/// lower-cased (values are not), a trailing `;` is ignored, and a quoted value honours backslash
/// escapes.
///
/// # What is not reproduced
///
/// RFC 2231's `filename*=utf-8''x` continuations and charset-tagged values. Go decodes those into
/// the un-starred attribute; here `filename*` stays a separate attribute and `filename` is absent,
/// so such a part would be read as a **value** where Go reads it as a file. No browser sends that
/// form in a `multipart/form-data` body and `createEmoji` is the only route affected; recorded as
/// [D-381] rather than left implicit.
fn parse_media_type(raw: &str) -> Option<(String, HashMap<String, String>)> {
    let mut parts = raw.splitn(2, ';');
    let media_type = parts.next()?.trim().to_ascii_lowercase();
    if media_type.is_empty() {
        return None;
    }
    let mut params: HashMap<String, String> = HashMap::new();
    let Some(rest) = parts.next() else {
        return Some((media_type, params));
    };

    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    loop {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ';') {
            i += 1;
        }
        if i >= chars.len() {
            // Go's "ignore trailing semicolons" break.
            return Some((media_type, params));
        }

        let key_start = i;
        while i < chars.len() && is_token_char(chars[i]) {
            i += 1;
        }
        let key: String = chars[key_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        if key.is_empty() || i >= chars.len() || chars[i] != '=' {
            return None;
        }
        i += 1; // past '='

        let value = if chars.get(i) == Some(&'"') {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < chars.len() {
                match chars[i] {
                    '\\' if i + 1 < chars.len() => {
                        value.push(chars[i + 1]);
                        i += 2;
                    }
                    '"' => {
                        i += 1;
                        closed = true;
                        break;
                    }
                    c => {
                        value.push(c);
                        i += 1;
                    }
                }
            }
            if !closed {
                return None;
            }
            value
        } else {
            // `consumeToken`: an unquoted value is a token, and an empty one is an error.
            let start = i;
            while i < chars.len() && is_token_char(chars[i]) {
                i += 1;
            }
            if i == start {
                return None;
            }
            chars[start..i].iter().collect::<String>()
        };

        // Whatever follows a value must be whitespace and then a `;` or the end.
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] != ';' {
            return None;
        }

        if params.insert(key, value).is_some() {
            // "mime: duplicate parameter name" — an error, not a last-wins overwrite.
            return None;
        }
    }
}

/// `isTokenChar` (mime/grammar.go:15): an ASCII character that is neither a space, a control
/// character, nor one of RFC 2045's `tspecials`.
fn is_token_char(c: char) -> bool {
    c.is_ascii()
        && !c.is_ascii_control()
        && c != ' '
        && !matches!(
            c,
            '(' | ')'
                | '<'
                | '>'
                | '@'
                | ','
                | ';'
                | ':'
                | '\\'
                | '"'
                | '/'
                | '['
                | ']'
                | '?'
                | '='
        )
}

/// Port of `Reader.readForm` (mime/multipart/formdata.go:60) over a buffered body.
fn read_form(body: &[u8], boundary: &str) -> Result<Form, MultipartError> {
    let mut form = Form::default();
    let mut parts_read = 0usize;

    let dash_boundary = format!("--{boundary}").into_bytes();
    let mut cursor = 0usize;
    // `Reader.nl` (mime/multipart/multipart.go:320): CRLF, unless the **first** delimiter line
    // ends in a bare LF, in which case the whole body is read that way. Decided once.
    let mut newline: Option<&'static [u8]> = None;

    loop {
        let Some(line_end) = line_end(body, cursor) else {
            return Err(MultipartError::Malformed(
                "the body ended before the closing delimiter",
            ));
        };
        let line = &body[cursor..line_end.0];
        let terminator = &body[line_end.0..line_end.1];
        cursor = line_end.1;

        if !line.starts_with(&dash_boundary) {
            if parts_read == 0 {
                // The preamble: everything before the first delimiter is discarded.
                continue;
            }
            return Err(MultipartError::Malformed("expected a boundary delimiter"));
        }

        // `skipLWSPChar` — spaces and tabs between the boundary and the line ending.
        let rest = line[dash_boundary.len()..]
            .iter()
            .copied()
            .skip_while(|b| *b == b' ' || *b == b'\t')
            .collect::<Vec<u8>>();

        if rest.starts_with(b"--") {
            // The closing delimiter. Go discards the epilogue that follows it.
            return Ok(form);
        }
        if !rest.is_empty() {
            if parts_read == 0 {
                continue; // still the preamble: a line that merely starts with the boundary
            }
            return Err(MultipartError::Malformed(
                "trailing bytes on a boundary delimiter line",
            ));
        }

        let nl = *newline.get_or_insert(if terminator == b"\r\n" {
            b"\r\n"
        } else {
            b"\n"
        });
        if terminator != nl {
            return Err(MultipartError::Malformed(
                "the body mixes CRLF and LF delimiters",
            ));
        }

        parts_read += 1;
        if parts_read > MAX_PARTS {
            return Err(MultipartError::TooManyParts);
        }

        let headers = read_part_headers(body, &mut cursor, nl)?;
        let data = read_part_body(body, &mut cursor, nl, &dash_boundary)?;

        // Rules 1-4 from the module docs.
        let disposition = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-disposition"))
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        let Some((disposition_type, params)) = parse_media_type(disposition) else {
            continue;
        };
        if disposition_type != "form-data" {
            continue;
        }
        let Some(name) = params.get("name").filter(|name| !name.is_empty()) else {
            continue;
        };

        match params.get("filename").filter(|f| !f.is_empty()) {
            Some(filename) => form.file.entry(name.clone()).or_default().push(FilePart {
                filename: go_filepath_base(filename),
                data,
            }),
            None => form
                .value
                .entry(name.clone())
                .or_default()
                // Go stores the raw bytes as a Go string, which is not required to be UTF-8.
                // `String::from_utf8_lossy` is the closest a Rust `String` gets; the only field
                // `createEmoji` reads from here is then fed to a JSON decoder, which would refuse
                // the invalid bytes anyway.
                .push(String::from_utf8_lossy(&data).into_owned()),
        }
    }
}

/// The end of the line starting at `from`, as `(content_end, line_end)`.
///
/// `None` when there is no line terminator left, which is what makes a body with no closing
/// delimiter an error rather than a silent success.
fn line_end(body: &[u8], from: usize) -> Option<(usize, usize)> {
    let newline = body[from..].iter().position(|b| *b == b'\n')? + from;
    if newline > from && body[newline - 1] == b'\r' {
        Some((newline - 1, newline + 1))
    } else {
        Some((newline, newline + 1))
    }
}

/// `textproto.Reader.ReadMIMEHeader`, narrowed: `Name: value` lines until a blank one.
///
/// Continuation lines (a header value folded onto a following line that starts with whitespace)
/// are joined, because Go's reader joins them and a `Content-Disposition` long enough to fold is
/// exactly where a filename would hide.
fn read_part_headers(
    body: &[u8],
    cursor: &mut usize,
    nl: &[u8],
) -> Result<Vec<(String, String)>, MultipartError> {
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let Some((content_end, next)) = line_end(body, *cursor) else {
            return Err(MultipartError::Malformed(
                "the body ended inside a part's headers",
            ));
        };
        let line = &body[*cursor..content_end];
        let terminator = &body[content_end..next];
        if terminator != nl {
            return Err(MultipartError::Malformed(
                "a header line does not end the way the body's delimiters do",
            ));
        }
        *cursor = next;

        if line.is_empty() {
            return Ok(headers);
        }

        let text = String::from_utf8_lossy(line).into_owned();
        if text.starts_with(' ') || text.starts_with('\t') {
            match headers.last_mut() {
                Some((_, value)) => {
                    value.push(' ');
                    value.push_str(text.trim());
                    continue;
                }
                // A continuation with nothing to continue. `ReadMIMEHeader` errors on it.
                None => {
                    return Err(MultipartError::Malformed(
                        "a header line begins with folding",
                    ));
                }
            }
        }

        let Some((name, value)) = text.split_once(':') else {
            return Err(MultipartError::Malformed("a header line has no colon"));
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
}

/// The part's bytes: everything up to the next `<nl>--<boundary>` at a line start.
///
/// The newline **before** the delimiter belongs to the delimiter, not to the part — which is why a
/// part's content is not terminated by its own trailing CRLF.
fn read_part_body(
    body: &[u8],
    cursor: &mut usize,
    nl: &[u8],
    dash_boundary: &[u8],
) -> Result<Vec<u8>, MultipartError> {
    let mut needle = nl.to_vec();
    needle.extend_from_slice(dash_boundary);

    let start = *cursor;
    let Some(offset) = find(&body[start..], &needle) else {
        return Err(MultipartError::Malformed(
            "a part is not terminated by a boundary",
        ));
    };
    let end = start + offset;
    *cursor = end + nl.len();
    Ok(body[start..end].to_vec())
}

/// The first index at which `needle` occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Port of `filepath.Base` (path/filepath/path.go:190), which `Part.FileName` applies.
///
/// The corners are Go's and they are not what a `rsplit('/')` gives: an empty path is `"."`, a
/// path of only separators is `"/"`, and trailing separators are stripped before the last element
/// is taken — so `a/b/` is `b`.
fn go_filepath_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    match trimmed.rfind('/') {
        Some(index) => trimmed[index + 1..].to_owned(),
        None => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_emoji_upload.json"
        ))
        .expect("behaviour_emoji_upload.json is generated by reference/dump")
    }

    fn decode(value: &serde_json::Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(value.as_str().expect("a base64 string"))
            .expect("valid base64")
    }

    /// `mime.ParseMediaType` itself, over both headers this module parses.
    ///
    /// The rows that matter are the three Go **refuses**: `name` with no value, an unterminated
    /// quote, and a repeated attribute. Each of them drops a part rather than refusing the body,
    /// so a lenient parser here turns a dropped part into a present one — which for `image` is the
    /// difference between `createEmoji`'s `invalid_body_param` and a stored file.
    #[test]
    fn media_type_parsing_matches_go() {
        let oracle = oracle();
        let cases = oracle["parse_media_type"].as_array().expect("an array");
        assert!(cases.len() >= 20, "the corpus should cover both headers");

        for case in cases {
            let raw = case["in"].as_str().expect("a string");
            let got = parse_media_type(raw);

            if !case["ok"].as_bool().expect("ok") {
                assert_eq!(got, None, "Go refuses {raw:?}");
                continue;
            }

            let (media_type, params) = got.unwrap_or_else(|| panic!("Go accepts {raw:?}"));
            assert_eq!(
                media_type,
                case["media_type"].as_str().expect("a media type"),
                "{raw:?}"
            );

            let expected: std::collections::HashMap<String, String> =
                serde_json::from_value(case["params"].clone()).expect("a string map");
            assert_eq!(params, expected, "{raw:?}");
        }
    }

    /// `multipart.Reader.ReadForm` over whole bodies.
    ///
    /// Both halves are asserted: which bodies Go *refuses*, and — for the ones it accepts — the
    /// exact `Form.Value` and `Form.File` maps, filenames and sizes included. The drop rules are
    /// the point: `no_name_is_dropped`, `not_form_data_is_dropped` and
    /// `unparseable_disposition_is_dropped` are all **200**s with an empty form on Go, and a port
    /// that refused them would answer 400 where Go answers 400 with a different id.
    #[test]
    fn form_reading_matches_go() {
        let oracle = oracle();
        let cases = oracle["read_form"].as_array().expect("an array");
        assert!(cases.len() >= 20, "the corpus should cover the drop rules");

        for case in cases {
            let name = case["name"].as_str().expect("a name");
            let boundary = case["boundary"].as_str().expect("a boundary");
            let body = decode(&case["body_base64"]);
            let got = read_form(&body, boundary);

            if !case["ok"].as_bool().expect("ok") {
                assert!(got.is_err(), "{name}: Go refuses this body");
                continue;
            }
            let form = got.unwrap_or_else(|err| panic!("{name}: Go accepts this body ({err})"));

            let expected_values: std::collections::HashMap<String, Vec<String>> =
                serde_json::from_value(case["values"].clone()).expect("a map of lists");
            assert_eq!(form.value, expected_values, "{name}: Form.Value");

            let expected_files = case["files"].as_array().expect("an array");
            let mut flat: Vec<(String, usize, String, i64)> = Vec::new();
            let mut names: Vec<&String> = form.file.keys().collect();
            names.sort();
            for key in names {
                for (index, part) in form.file[key].iter().enumerate() {
                    flat.push((key.clone(), index, part.filename.clone(), part.size()));
                }
            }
            assert_eq!(flat.len(), expected_files.len(), "{name}: Form.File length");
            for (got, want) in flat.iter().zip(expected_files) {
                assert_eq!(got.0, want["name"].as_str().expect("a name"), "{name}");
                assert_eq!(
                    got.1 as i64,
                    want["index"].as_i64().expect("an index"),
                    "{name}"
                );
                assert_eq!(
                    got.2,
                    want["filename"].as_str().expect("a filename"),
                    "{name}"
                );
                assert_eq!(got.3, want["size"].as_i64().expect("a size"), "{name}");
            }
        }
    }

    /// `filepath.Base`, which `Part.FileName` applies to every filename that survives.
    ///
    /// The corners are not what a `rsplit('/')` gives: `""` is `"."`, `"///"` is `"/"`, and
    /// `"a/b/"` is `"b"`.
    #[test]
    fn filepath_base_matches_go() {
        let oracle = oracle();
        for case in oracle["filepath_base"].as_array().expect("an array") {
            let input = case["in"].as_str().expect("a string");
            assert_eq!(
                go_filepath_base(input),
                case["out"].as_str().expect("a string"),
                "{input:?}"
            );
        }
    }

    /// The boundary extraction `http.Request.multipartReader` performs, and its two refusals.
    #[test]
    fn the_boundary_comes_off_the_content_type() {
        assert_eq!(
            multipart_boundary(Some("multipart/form-data; boundary=abc")),
            Ok("abc".to_owned())
        );
        assert_eq!(
            multipart_boundary(Some("MULTIPART/FORM-DATA; BOUNDARY=AbC")),
            Ok("AbC".to_owned()),
            "the attribute name folds, the value does not"
        );
        assert_eq!(
            multipart_boundary(Some("multipart/form-data")),
            Err(MultipartError::MissingBoundary)
        );
        // `allowMixed` is false for `ParseMultipartForm`, so `multipart/mixed` is not multipart
        // as far as this route is concerned.
        assert_eq!(
            multipart_boundary(Some("multipart/mixed; boundary=abc")),
            Err(MultipartError::NotMultipart)
        );
        assert_eq!(
            multipart_boundary(Some("application/json")),
            Err(MultipartError::NotMultipart)
        );
        assert_eq!(
            multipart_boundary(Some("")),
            Err(MultipartError::NotMultipart)
        );
        assert_eq!(multipart_boundary(None), Err(MultipartError::NotMultipart));
    }

    /// `maxParts` is 1000 and the 1001st is `ErrMessageTooLarge`, not a truncated form.
    #[test]
    fn a_thousand_parts_is_the_cap() {
        let one = "--B\r\nContent-Disposition: form-data; name=\"n\"\r\n\r\nv\r\n";
        let body = one.repeat(1000) + "--B--\r\n";
        assert_eq!(
            read_form(body.as_bytes(), "B")
                .expect("1000 parts is allowed")
                .value["n"]
                .len(),
            1000
        );

        let body = one.repeat(1001) + "--B--\r\n";
        assert_eq!(
            read_form(body.as_bytes(), "B"),
            Err(MultipartError::TooManyParts)
        );
    }
}
