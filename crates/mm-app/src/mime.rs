//! Port of Go's `mime.TypeByExtension` (mime/type.go:164) with its **Unix** initialisation
//! (mime/type_unix.go) — the lookup `model.NewInfo` makes for every uploaded file's `mime_type`.
//!
//! # Why this is a port of the standard library and not a table
//!
//! `NewInfo` (file_info.go:213) calls `mime.TypeByExtension(strings.ToLower(filepath.Ext(name)))`
//! and the answer goes on the wire as `FileInfo.mime_type`. Go's table is **not** a constant: at
//! first use it loads the builtin sixty-four entries and then, in order, the host's
//! `/usr/local/share/mime/globs2`, `/usr/share/mime/globs2` — stopping at the first that opens —
//! and only if neither did, the four `mime.types` files. So `.md` is `text/markdown;
//! charset=utf-8` on a host with the freedesktop database and `""` on one without, and the two
//! servers on one stack agree only because they read the same files. [D-030] recorded that as
//! "not portable in principle"; this makes the Rust server exactly as portable as the Go one.
//!
//! # The three rules that decide an entry
//!
//! 1. **Builtin wins over `globs2`.** `loadMimeGlobsFile` skips an extension already in the
//!    exact-case map, and the builtins are stored first — so `*.png` from the database, which
//!    would be the same anyway, and `*.txt`, which would be `text/plain` *without* the builtin's
//!    `charset=utf-8`, are both ignored.
//! 2. **First `globs2` line wins.** The file is in descending weight order and the same
//!    already-seen check keeps the first. `*.C` is one line and `*.c` another, and they differ.
//! 3. **`mime.types` overrides.** The fallback loader has no already-seen check, so a later file
//!    (and a later line) replaces an earlier one — builtins included.
//!
//! And every stored `text/*` type without a `charset` parameter gains `; charset=utf-8`
//! (`setExtensionType`, mime/type.go:246), which is where `text/markdown; charset=utf-8` comes
//! from when the database says only `text/markdown`.
//!
//! # What is narrowed
//!
//! `setExtensionType` runs each file's type through `mime.ParseMediaType` and discards the entry
//! on error. The full parser — RFC 2231 continuations and all — lives in `mm_api::multipart`
//! for `Content-Disposition`; a mime-type *file* never carries a parameter, let alone a
//! continuation, so [`parse_media_type`] here accepts `type/subtype` with plain `; key=value`
//! parameters and refuses anything else, which is the same verdict Go reaches on the same input.
//! The oracle (`fixtures/behaviour_mime.json`) carries the very `globs2` lines Go read for its
//! corpus, so the test rebuilds Go's table from Go's input rather than trusting this host's.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Go's `builtinTypesLower` (mime/type.go:44) — sixty-four entries, transcribed in order.
const BUILTIN: &[(&str, &str)] = &[
    (".ai", "application/postscript"),
    (".apk", "application/vnd.android.package-archive"),
    (".apng", "image/apng"),
    (".avif", "image/avif"),
    (".bin", "application/octet-stream"),
    (".bmp", "image/bmp"),
    (".com", "application/octet-stream"),
    (".css", "text/css; charset=utf-8"),
    (".csv", "text/csv; charset=utf-8"),
    (".doc", "application/msword"),
    (
        ".docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (".ehtml", "text/html; charset=utf-8"),
    (".eml", "message/rfc822"),
    (".eps", "application/postscript"),
    (".exe", "application/octet-stream"),
    (".flac", "audio/flac"),
    (".gif", "image/gif"),
    (".gz", "application/gzip"),
    (".htm", "text/html; charset=utf-8"),
    (".html", "text/html; charset=utf-8"),
    (".ico", "image/vnd.microsoft.icon"),
    (".ics", "text/calendar; charset=utf-8"),
    (".jfif", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".js", "text/javascript; charset=utf-8"),
    (".json", "application/json"),
    (".m4a", "audio/mp4"),
    (".mjs", "text/javascript; charset=utf-8"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".oga", "audio/ogg"),
    (".ogg", "audio/ogg"),
    (".ogv", "video/ogg"),
    (".opus", "audio/ogg"),
    (".pdf", "application/pdf"),
    (".pjp", "image/jpeg"),
    (".pjpeg", "image/jpeg"),
    (".png", "image/png"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (
        ".pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    (".ps", "application/postscript"),
    (".rdf", "application/rdf+xml"),
    (".rtf", "application/rtf"),
    (".shtml", "text/html; charset=utf-8"),
    (".svg", "image/svg+xml"),
    (".text", "text/plain; charset=utf-8"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".txt", "text/plain; charset=utf-8"),
    (".vtt", "text/vtt; charset=utf-8"),
    (".wasm", "application/wasm"),
    (".wav", "audio/wav"),
    (".webm", "audio/webm"),
    (".webp", "image/webp"),
    (".xbl", "text/xml; charset=utf-8"),
    (".xbm", "image/x-xbitmap"),
    (".xht", "application/xhtml+xml"),
    (".xhtml", "application/xhtml+xml"),
    (".xls", "application/vnd.ms-excel"),
    (
        ".xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (".xml", "text/xml; charset=utf-8"),
    (".xsl", "text/xml; charset=utf-8"),
    (".zip", "application/zip"),
];

/// `mimeGlobs` (mime/type_unix.go:14) — tried in order, the first that opens ends the search.
pub const MIME_GLOBS: &[&str] = &["/usr/local/share/mime/globs2", "/usr/share/mime/globs2"];

/// `typeFiles` (mime/type_unix.go:19) — the fallback, every one that opens is loaded.
pub const TYPE_FILES: &[&str] = &[
    "/etc/mime.types",
    "/etc/apache2/mime.types",
    "/etc/apache/mime.types",
    "/etc/httpd/conf/mime.types",
];

/// The two maps `TypeByExtension` consults: `mimeTypes` (the extension as written) and
/// `mimeTypesLower`.
#[derive(Debug, Clone, Default)]
pub struct MimeTable {
    exact: HashMap<String, String>,
    lower: HashMap<String, String>,
}

impl MimeTable {
    /// `setMimeTypes(builtinTypesLower, builtinTypesLower)` — the builtins go into **both** maps
    /// under their lowercase keys.
    pub fn builtin() -> Self {
        let mut table = Self::default();
        for (ext, mime_type) in BUILTIN {
            table
                .exact
                .insert((*ext).to_owned(), (*mime_type).to_owned());
            table
                .lower
                .insert((*ext).to_owned(), (*mime_type).to_owned());
        }
        table
    }

    /// `initMimeUnix` (mime/type_unix.go:114) over the host's files.
    pub fn from_host() -> Self {
        let mut table = Self::builtin();
        for path in MIME_GLOBS {
            if let Ok(text) = std::fs::read_to_string(path) {
                table.load_globs2(&text);
                // "Stop checking more files if mimetype database is found."
                return table;
            }
        }
        for path in TYPE_FILES {
            if let Ok(text) = std::fs::read_to_string(path) {
                table.load_mime_types(&text);
            }
        }
        table
    }

    /// `loadMimeGlobsFile` (mime/type_unix.go:26), given the file's text.
    ///
    /// Each line is `weight:mimetype:glob[:more]`. Only `*.<ext>` globs count, a glob with any
    /// of `?*[` past the dot is skipped, and an extension the **exact-case** map already holds is
    /// skipped — which is what makes the builtins and the heaviest line win.
    pub fn load_globs2(&mut self, text: &str) {
        for line in text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 3 || fields[0].is_empty() || fields[2].len() < 3 {
                continue;
            }
            let glob = fields[2].as_bytes();
            if fields[0].as_bytes()[0] == b'#' || glob[0] != b'*' || glob[1] != b'.' {
                continue;
            }
            let extension = &fields[2][1..];
            if extension.contains(['?', '*', '[']) {
                continue;
            }
            if self.exact.contains_key(extension) {
                continue;
            }
            self.set_extension_type(extension, fields[1]);
        }
    }

    /// `loadMimeFile` (mime/type_unix.go:78), given the file's text: `mimetype ext ext…`, a `#`
    /// anywhere in the extension list ending the line, and **no** already-seen check.
    pub fn load_mime_types(&mut self, text: &str) {
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() <= 1 || fields[0].as_bytes()[0] == b'#' {
                continue;
            }
            let mime_type = fields[0];
            for ext in &fields[1..] {
                if ext.as_bytes()[0] == b'#' {
                    break;
                }
                self.set_extension_type(&format!(".{ext}"), mime_type);
            }
        }
    }

    /// `setExtensionType` (mime/type.go:241): parse, add `charset=utf-8` to a `text/*` type
    /// that has none, store under the extension as written and lowercased.
    fn set_extension_type(&mut self, extension: &str, mime_type: &str) {
        let Some((_, params)) = parse_media_type(mime_type) else {
            return;
        };
        let stored = if mime_type.starts_with("text/")
            && params.get("charset").is_none_or(String::is_empty)
        {
            let mut params = params;
            params.insert("charset".to_owned(), "utf-8".to_owned());
            format_media_type(mime_type, &params)
        } else {
            mime_type.to_owned()
        };
        self.exact.insert(extension.to_owned(), stored.clone());
        self.lower
            .insert(mm_model::utils::go_to_lower(extension), stored);
    }

    /// `TypeByExtension` (mime/type.go:164): the exact-case map, then the lowercased one, then
    /// `""`.
    pub fn type_by_extension(&self, ext: &str) -> String {
        if let Some(found) = self.exact.get(ext) {
            return found.clone();
        }
        // Go lowercases ASCII byte by byte and falls back to `strings.ToLower` past 0x80; the two
        // agree with `go_to_lower` on every input.
        self.lower
            .get(&mm_model::utils::go_to_lower(ext))
            .cloned()
            .unwrap_or_default()
    }
}

/// `mime.TypeByExtension` against the host's table, built once on first use like Go's
/// `once.Do(initMime)`.
pub fn type_by_extension(ext: &str) -> String {
    static HOST: OnceLock<MimeTable> = OnceLock::new();
    HOST.get_or_init(MimeTable::from_host)
        .type_by_extension(ext)
}

/// `mime.ParseMediaType`, narrowed to the grammar a mime-type file can hold: `type/subtype`
/// (or a bare token, which Go also accepts) followed by `; key=value` parameters whose values
/// are tokens or quoted strings. Keys are lowercased as Go's are; a duplicate key with a
/// different value, a continuation (`key*`), or anything else the full parser handles is
/// refused here — see the module docs for why that is the same verdict.
fn parse_media_type(raw: &str) -> Option<(String, HashMap<String, String>)> {
    let (base, mut rest) = match raw.find(';') {
        Some(at) => (&raw[..at], &raw[at..]),
        None => (raw, ""),
    };
    let media_type = mm_model::utils::go_to_lower(base.trim());
    if !is_well_formed_media_type(&media_type) {
        return None;
    }

    let mut params: HashMap<String, String> = HashMap::new();
    while !rest.is_empty() {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let after_semicolon = trimmed.strip_prefix(';')?.trim_start();
        if after_semicolon.is_empty() {
            // `consumeMediaParam` fails on the trailing `;` and Go forgives exactly that.
            break;
        }
        let (key, after_key) = consume_token(after_semicolon);
        if key.is_empty() || key.contains('*') {
            return None;
        }
        let after_equals = after_key.trim_start().strip_prefix('=')?;
        let (value, after_value) = consume_value(after_equals.trim_start())?;
        let key = mm_model::utils::go_to_lower(key);
        if params.get(&key).is_some_and(|existing| *existing != value) {
            return None;
        }
        params.insert(key, value);
        rest = after_value;
    }
    Some((media_type, params))
}

/// `checkMediaTypeDisposition` (mime/mediatype.go:100) as a predicate.
fn is_well_formed_media_type(s: &str) -> bool {
    let (typ, rest) = consume_token(s);
    if typ.is_empty() {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    let Some(after_slash) = rest.strip_prefix('/') else {
        return false;
    };
    let (subtype, rest) = consume_token(after_slash);
    !subtype.is_empty() && rest.is_empty()
}

/// `consumeToken` — the leading run of token characters and the remainder.
fn consume_token(v: &str) -> (&str, &str) {
    let end = v.bytes().position(|b| !is_token_byte(b)).unwrap_or(v.len());
    (&v[..end], &v[end..])
}

/// `consumeValue`, minus RFC 2047: a token, or a quoted string with `\` escapes.
fn consume_value(v: &str) -> Option<(String, &str)> {
    if !v.starts_with('"') {
        let (token, rest) = consume_token(v);
        if token.is_empty() {
            return None;
        }
        return Some((token.to_owned(), rest));
    }
    let bytes = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let rest = &v[i + 1..];
                return String::from_utf8(out).ok().map(|value| (value, rest));
            }
            b'\\' if i + 1 < bytes.len() && bytes[i + 1] < 0x80 => {
                out.push(bytes[i + 1]);
                i += 2;
            }
            b'\r' | b'\n' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

/// `isTokenChar` (mime/grammar.go:14): printable ASCII that is not a tspecial.
fn is_token_byte(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !is_tspecial(b)
}

/// `isTSpecial` (mime/grammar.go:9).
fn is_tspecial(b: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&b)
}

/// `mime.FormatMediaType` (mime/mediatype.go:21): the lowercased type, then every parameter in
/// key order as `; key=value`, quoted when the value is not a token and RFC 2231-encoded when it
/// is not printable ASCII. An unrepresentable type or key is the empty string, as in Go.
pub fn format_media_type(t: &str, params: &HashMap<String, String>) -> String {
    let mut out = String::new();
    match t.split_once('/') {
        None => {
            if !is_token(t) {
                return String::new();
            }
            out.push_str(&mm_model::utils::go_to_lower(t));
        }
        Some((major, sub)) => {
            if !is_token(major) || !is_token(sub) {
                return String::new();
            }
            out.push_str(&mm_model::utils::go_to_lower(major));
            out.push('/');
            out.push_str(&mm_model::utils::go_to_lower(sub));
        }
    }

    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    for attribute in keys {
        let value = &params[attribute];
        out.push_str("; ");
        if !is_token(attribute) {
            return String::new();
        }
        out.push_str(&mm_model::utils::go_to_lower(attribute));

        let needs_encoding = value
            .bytes()
            .any(|b| !(b' '..=b'~').contains(&b) && b != b'\t');
        if needs_encoding {
            out.push_str("*=utf-8''");
            for b in value.bytes() {
                if b <= b' ' || b >= 0x7f || b == b'*' || b == b'\'' || b == b'%' || is_tspecial(b)
                {
                    out.push_str(&format!("%{b:02X}"));
                } else {
                    out.push(char::from(b));
                }
            }
            continue;
        }

        out.push('=');
        if is_token(value) {
            out.push_str(value);
            continue;
        }
        out.push('"');
        for c in value.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
    out
}

/// `isToken` — non-empty and every byte a token byte.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_token_byte)
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_mime.json"))
            .expect("behaviour_mime.json is generated by reference/dump")
    }

    /// Go's table, rebuilt from the lines Go read — so the verdict does not depend on which
    /// host runs the test, only on which host generated the fixture.
    fn table_from_oracle(oracle: &serde_json::Value) -> MimeTable {
        let mut table = MimeTable::builtin();
        let lines = |key: &str| -> String {
            oracle[key]
                .as_array()
                .expect("an array of lines")
                .iter()
                .map(|line| line.as_str().expect("a line"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        if oracle["globs2_loaded"].as_bool() == Some(true) {
            table.load_globs2(&lines("globs2_lines"));
        } else {
            table.load_mime_types(&lines("mime_types_lines"));
        }
        table
    }

    #[test]
    fn type_by_extension_matches_go_over_the_corpus() {
        let oracle = oracle();
        let table = table_from_oracle(&oracle);
        let cases = oracle["type_by_extension"].as_array().expect("an array");
        assert!(cases.len() > 100, "the corpus is {} cases", cases.len());
        for case in cases {
            let ext = case["ext"].as_str().expect("ext");
            let want = case["type"].as_str().expect("type");
            assert_eq!(
                table.type_by_extension(ext),
                want,
                "TypeByExtension({ext:?})"
            );
        }
    }

    /// The corpus has to exercise every rule in the module docs, or the test above proves less
    /// than it reads.
    #[test]
    fn the_corpus_reaches_every_rule() {
        let oracle = oracle();
        let cases: HashMap<String, String> = oracle["type_by_extension"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|c| {
                (
                    c["ext"].as_str().unwrap().to_owned(),
                    c["type"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        // A builtin, unchanged by the host file.
        assert_eq!(cases[".txt"], "text/plain; charset=utf-8");
        // The lowercase fallback.
        assert_eq!(cases[".TXT"], cases[".txt"]);
        // No extension, no dot, nothing.
        assert_eq!(cases[""], "");
        assert_eq!(cases["txt"], "");
        assert_eq!(cases[".zzzzzz"], "");
        if oracle["globs2_loaded"].as_bool() == Some(true) {
            // A host-file entry, with the charset the table adds to `text/*`.
            assert_eq!(cases[".md"], "text/markdown; charset=utf-8");
            // Case decides the entry when the file has both.
            assert_ne!(
                cases[".c"], cases[".C"],
                "`*.c` and `*.C` are two globs2 lines"
            );
        }
    }

    #[test]
    fn builtin_has_go_s_sixty_four_entries() {
        assert_eq!(BUILTIN.len(), 64);
        let mut seen = std::collections::HashSet::new();
        for (ext, _) in BUILTIN {
            assert!(seen.insert(ext), "{ext} listed twice");
        }
    }

    #[test]
    fn a_later_mime_types_line_overrides_an_earlier_one_and_the_builtin() {
        let mut table = MimeTable::builtin();
        table.load_mime_types("text/x-foo foo\napplication/foo foo\ntext/plain txt # trailing\n");
        assert_eq!(table.type_by_extension(".foo"), "application/foo");
        // The builtin `.txt` is replaced — and by the type *with* the charset the loader adds.
        assert_eq!(table.type_by_extension(".txt"), "text/plain; charset=utf-8");
        // Past a `#` the rest of the line is comment.
        assert_eq!(table.type_by_extension(".trailing"), "");
    }

    #[test]
    fn globs2_keeps_the_first_line_and_skips_globs_and_builtins() {
        let mut table = MimeTable::builtin();
        table.load_globs2(
            "# comment\n50:text/x-first:*.dup\n40:text/x-second:*.dup\n50:text/plain:*.txt\n\
             50:application/x-glob:*.so.[0-9]*\n50:application/x-noglob:credits\n\
             50:image/x-upper:*.C\n50:text/x-c:*.c\nbad line\n",
        );
        assert_eq!(
            table.type_by_extension(".dup"),
            "text/x-first; charset=utf-8"
        );
        assert_eq!(table.type_by_extension(".txt"), "text/plain; charset=utf-8");
        assert_eq!(table.type_by_extension(".so.1"), "");
        assert_eq!(table.type_by_extension(".C"), "image/x-upper");
        assert_eq!(table.type_by_extension(".c"), "text/x-c; charset=utf-8");
        // `.c` lowercased is what `NewInfo` asks for — and the lower map was written by **both**
        // lines, the `*.c` one last.
        assert_eq!(table.type_by_extension(".C"), "image/x-upper");
    }

    #[test]
    fn format_media_type_quotes_and_encodes_like_go() {
        let mut params = HashMap::new();
        params.insert("charset".to_owned(), "utf-8".to_owned());
        assert_eq!(
            format_media_type("Text/Plain", &params),
            "text/plain; charset=utf-8"
        );
        params.insert("name".to_owned(), "a b\"c".to_owned());
        assert_eq!(
            format_media_type("text/plain", &params),
            "text/plain; charset=utf-8; name=\"a b\\\"c\""
        );
        params.clear();
        params.insert("name".to_owned(), "é".to_owned());
        assert_eq!(
            format_media_type("text/plain", &params),
            "text/plain; name*=utf-8''%C3%A9"
        );
        assert_eq!(format_media_type("text/pl ain", &params), "");
        assert_eq!(format_media_type("bare", &HashMap::new()), "bare");
    }

    #[test]
    fn a_type_with_a_parameter_keeps_its_charset() {
        let mut table = MimeTable::builtin();
        table.load_mime_types("text/x-latin;charset=iso-8859-1 lat\n");
        assert_eq!(
            table.type_by_extension(".lat"),
            "text/x-latin;charset=iso-8859-1"
        );
    }
}
