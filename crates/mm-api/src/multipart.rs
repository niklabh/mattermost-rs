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

/// Port of `mime.ParseMediaType` (mime/mediatype.go:141), transcribed function for function.
///
/// Go's parser is four small pieces — `checkMediaTypeDisposition`, `consumeMediaParam`,
/// `consumeValue` and the RFC 2231 stitching loop — and each of them refuses in a way a lenient
/// reader would not. They are transcribed rather than paraphrased because every refusal here
/// **drops a part** (for a `Content-Disposition`) or **rejects the body** (for a `Content-Type`),
/// and a part that Go turns into a file and this port turns into a value is a different HTTP
/// status on a route that writes.
///
/// # The refusals
///
/// * `name` with no `=`, and `name=` with nothing after it — a value must be a non-empty token or
///   a quoted string.
/// * A repeated attribute with a **different** value. An exactly repeated one is *allowed*
///   (mediatype.go:196, `exists && v != value`) — first-wins is wrong in both directions.
/// * An unterminated quoted string, and a bare CR or LF inside one.
/// * Anything between a value and the next `;` — `consumeMediaParam` fails and the outer loop
///   only forgives the failure when the whole remainder is a single `;`. So `a/b;;name=x` is an
///   **error**, not two parameters with an empty one between them.
/// * A media type that is not `token` or `token/token`.
///
/// # The backslash rule is not "escape the next character"
///
/// `consumeValue` (mediatype.go:290) honours `\` only when the character after it is one of RFC
/// 2045's `tspecials`; otherwise the backslash is kept **and** the character after it is kept.
/// The comment in Go says why: MSIE sends `"C:\dev\go\foo.txt"` unescaped, and treating `\d` as
/// `d` would eat the path separators. So `name="a\b"` is `a\b` and `name="a\\b"` is `a\b` too.
///
/// # RFC 2231, which this port now decodes (closing [D-381])
///
/// Any attribute containing `*` is diverted into a per-base-name side map and never reaches the
/// result directly; after the loop the side map is stitched:
///
/// | form | result |
/// |---|---|
/// | `filename*=utf-8''caf%C3%A9.png` | `filename` = `café.png` |
/// | `filename*0="a"; filename*1="b.png"` | `filename` = `ab.png` |
/// | `filename*0*=utf-8''caf%C3%A9; filename*1=.png` | `filename` = `café.png` |
/// | `filename*=iso-8859-1''x` | **nothing** — only `us-ascii` and `utf-8` decode, and a failed decode drops the key entirely |
/// | `filename*1="b"` with no `*0` | nothing — the continuation walk starts at 0 and stops at the first gap |
/// | `filename="a"; filename*=utf-8''b` | `filename` = `b` — the stitch runs *after* the loop and overwrites |
///
/// Note the asymmetry in the continuation walk (mediatype.go:224): segment 0 is percent-decoded
/// only through `decode2231Enc`, which needs the `charset'lang'` prefix, while segments 1..n are
/// percent-decoded **unconditionally** by `percentHexUnescape` whether or not they carry the
/// trailing `*`. A port that decoded segment 0 the same way as the rest would turn
/// `filename*0=100%` into a failure and `filename*1=100%25` into `100%%25`.
///
/// # The one thing still not byte-exact
///
/// `percentHexUnescape` yields arbitrary bytes and Go stores them in a `string`, which may not be
/// valid UTF-8. A Rust `String` cannot hold that, so `filename*=utf-8''%ff` is decoded lossily
/// here and byte-exactly there. No route this server answers reads a filename's *content* — see
/// [D-410].
fn parse_media_type(raw: &str) -> Option<(String, HashMap<String, String>)> {
    let base = raw.split(';').next().unwrap_or(raw);
    let media_type = base.trim().to_ascii_lowercase();
    if !is_well_formed_media_type(&media_type) {
        return None;
    }

    let mut params: HashMap<String, String> = HashMap::new();
    // "Map of base parameter name -> parameter name -> value, for parameters containing a `*`."
    let mut continuation: HashMap<String, HashMap<String, String>> = HashMap::new();

    let mut rest = &raw[base.len()..];
    while !rest.is_empty() {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let Some((key, value, after)) = consume_media_param(trimmed) else {
            // `consumeMediaParam` failed, so `key == ""`. Go forgives exactly one shape: the
            // whole unconsumed remainder being a single `;`.
            if trimmed.trim() == ";" {
                break;
            }
            return None;
        };

        let pmap = match key.split_once('*') {
            Some((base_name, _)) => continuation.entry(base_name.to_owned()).or_default(),
            None => &mut params,
        };
        if pmap.get(&key).is_some_and(|existing| *existing != value) {
            // "mime: duplicate parameter name". An *equal* repeat is allowed.
            return None;
        }
        pmap.insert(key, value);
        rest = after;
    }

    stitch_2231(&mut params, continuation);
    Some((media_type, params))
}

/// Port of `checkMediaTypeDisposition` (mime/mediatype.go:100), as a predicate.
///
/// Go distinguishes four errors here; every one of them is the same `None` to both callers, so
/// only the verdict is kept.
fn is_well_formed_media_type(s: &str) -> bool {
    let (typ, rest) = consume_token(s);
    if typ.is_empty() {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    if rest.contains('/') {
        return false;
    }
    let (subtype, rest) = consume_token(rest);
    !subtype.is_empty() && rest.is_empty()
}

/// Port of `consumeMediaParam` (mime/mediatype.go:316). `None` is Go's `("", "", v)`.
///
/// Whitespace is skipped in four places — before the `;`, after it, around the `=` — which is why
/// `form-data; name = "x"` parses and `form-data;;name="x"` does not.
fn consume_media_param(v: &str) -> Option<(String, String, &str)> {
    let rest = v.trim_start().strip_prefix(';')?.trim_start();
    let (param, rest) = consume_token(rest);
    if param.is_empty() {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, after) = consume_value(rest)?;
    Some((param.to_ascii_lowercase(), value, after))
}

/// Port of `consumeToken` (mime/mediatype.go:260): the leading run of token characters, and
/// whatever follows it.
fn consume_token(v: &str) -> (&str, &str) {
    let end = v.bytes().position(|b| !is_token_byte(b)).unwrap_or(v.len());
    v.split_at(end)
}

/// Port of `consumeValue` (mime/mediatype.go:277). `None` is Go's `("", v)` *when that means
/// failure* — which is every case except an unquoted empty token, and the caller refuses that
/// too (`value == "" && rest2 == rest`), so the two collapse into one `None`.
fn consume_value(v: &str) -> Option<(String, &str)> {
    if !v.starts_with('"') {
        let (token, rest) = consume_token(v);
        if token.is_empty() {
            return None;
        }
        return Some((token.to_owned(), rest));
    }

    let bytes = v.as_bytes();
    let mut value: Vec<u8> = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Byte indices only ever land on a character boundary here: every byte matched
                // above is ASCII, and a multi-byte character's continuation bytes are copied
                // through untouched.
                let consumed = String::from_utf8(value).ok()?;
                return Some((consumed, &v[i + 1..]));
            }
            b'\\' if i + 1 < bytes.len() && is_tspecial(bytes[i + 1]) => {
                value.push(bytes[i + 1]);
                i += 2;
            }
            // A bare CR or LF inside a quoted string is a failure, not a literal.
            b'\r' | b'\n' => return None,
            byte => {
                value.push(byte);
                i += 1;
            }
        }
    }
    // No closing quote.
    None
}

/// Port of the stitching loop at the end of `ParseMediaType` (mime/mediatype.go:194-232).
fn stitch_2231(
    params: &mut HashMap<String, String>,
    continuation: HashMap<String, HashMap<String, String>>,
) {
    for (key, pieces) in continuation {
        // The single-part form, `filename*=charset'lang'text`. A decode failure drops the key —
        // it does **not** fall through to the numbered walk.
        if let Some(encoded) = pieces.get(&format!("{key}*")) {
            if let Some(decoded) = decode_2231_enc(encoded) {
                params.insert(key, decoded);
            }
            continue;
        }

        let mut buf = String::new();
        let mut valid = false;
        for n in 0.. {
            let simple_part = format!("{key}*{n}");
            if let Some(piece) = pieces.get(&simple_part) {
                valid = true;
                buf.push_str(piece);
                continue;
            }
            let Some(piece) = pieces.get(&format!("{simple_part}*")) else {
                break;
            };
            valid = true;
            if n == 0 {
                // Segment zero carries the charset, so it goes through `decode2231Enc`; a
                // failure there contributes **nothing** and the walk continues to segment 1.
                if let Some(decoded) = decode_2231_enc(piece) {
                    buf.push_str(&decoded);
                }
            } else if let Some(decoded) = percent_hex_unescape(piece) {
                buf.push_str(&decoded);
            }
        }
        if valid {
            params.insert(key, buf);
        }
    }
}

/// Port of `decode2231Enc` (mime/mediatype.go:239) — `charset'language'percent-encoded`.
///
/// Both apostrophes must be present, and the charset must be `us-ascii` or `utf-8`
/// case-insensitively; every other charset, the empty charset included, fails. The language is
/// parsed and thrown away, as Go's own TODO says.
fn decode_2231_enc(v: &str) -> Option<String> {
    let (charset, rest) = v.split_once('\'')?;
    let (_language, text) = rest.split_once('\'')?;
    match charset.to_ascii_lowercase().as_str() {
        "us-ascii" | "utf-8" => percent_hex_unescape(text),
        _ => None,
    }
}

/// Port of `percentHexUnescape` (mime/mediatype.go:345).
///
/// A `%` not followed by two hex digits fails the **whole** string; a string with no `%` at all
/// is returned unchanged without allocating a second time.
///
/// Go's result is a `string` over arbitrary bytes. This one is lossy where those bytes are not
/// valid UTF-8 — see [D-410] and the note on [`parse_media_type`].
fn percent_hex_unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut percents = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        percents += 1;
        if i + 2 >= bytes.len() || !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
            return None;
        }
        i += 3;
    }
    if percents == 0 {
        return Some(s.to_owned());
    }

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() - 2 * percents);
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push((unhex(bytes[i + 1]) << 4) | unhex(bytes[i + 2]));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// `unhex` (mime/mediatype.go:390). Only ever called on a byte `is_hex` accepted.
fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// `isTSpecial` (mime/grammar.go:9) — RFC 2045's `tspecials`, the set the backslash rule in
/// [`consume_value`] keys off.
fn is_tspecial(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')'
            | b'<'
            | b'>'
            | b'@'
            | b','
            | b';'
            | b':'
            | b'\\'
            | b'"'
            | b'/'
            | b'['
            | b']'
            | b'?'
            | b'='
    )
}

/// `isTokenChar` (mime/grammar.go:15): an ASCII byte that is neither a space, a control
/// character, nor one of RFC 2045's `tspecials`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii() && !b.is_ascii_control() && b != b' ' && !is_tspecial(b)
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
