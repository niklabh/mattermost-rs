//! Port of `github.com/klauspost/compress/gzhttp` v1.18.6 (gzhttp/compress.go) as Mattermost
//! uses it: `gzhttp.GzipHandler(h)`, the **default** wrapper, around the two static file servers
//! when `ServiceSettings.WebserverMode` is `gzip` (web/static.go:43).
//!
//! Not a Mattermost file — a third-party library whose wire format is the wire format of every
//! `/static/` response a browser receives. `fixtures/behaviour_web_static.json` (`gzhttp`) drives
//! the real `GzipHandler` inside a real `net/http` server and records what a client gets; the
//! `go_parity` module asserts this file against it row by row.
//!
//! # The defaults, which are the whole configuration
//!
//! `NewWrapper()` with no options: gzip at `DefaultCompression`, **zstd enabled and preferred**
//! (`SpeedFastest`), `MinSize` 1024, `DefaultContentTypeFilter`, `setContentType: true`, no
//! jitter, no ETag rewriting, `Accept-Ranges` dropped on compression. So a browser that sends
//! `Accept-Encoding: gzip, deflate, br, zstd` — every current Firefox and Chrome — gets **zstd**,
//! not gzip. The compressed *bytes* cannot match Go's (a different compressor); everything a
//! client can act on — the choice of encoding, the headers, the decoded body — does.
//!
//! # What decides compression
//!
//! Reduced from `GzipResponseWriter.Write` / `Close` for a handler that writes its body in one
//! go after setting its headers, which is what `http.FileServer` does:
//!
//! 1. `Vary: Accept-Encoding` is **added** to every response, compressed or not, HEAD included —
//!    it is the first thing the wrapper does, before it looks at the request.
//! 2. `HEAD`, or no acceptable encoding (`q=0` counts as refused): the handler runs unwrapped.
//! 3. Nothing is compressed that has a `Content-Encoding` or `Content-Range` (so a `206` never
//!    is), that carries no body (`304`, a redirect, a `412`), or whose body is shorter than
//!    `MinSize` — judged by `Content-Length` when the handler set one, by the bytes otherwise.
//! 4. The content type must pass `DefaultContentTypeFilter`: **`image/png` is compressed**; only
//!    `video/*`, `audio/*`, `image/jp*` and types naming a compressed format are not.
//! 5. Compressing deletes `Content-Length` and `Accept-Ranges` and sets `Content-Encoding`.
//!
//! # Framing
//!
//! After compression the length is unknown to the handler, so `net/http` decides: a body of at
//! most 2048 bytes written before the handler returns gets a `Content-Length`, a longer one is
//! chunked. [`go_framed_body`] reproduces that rule; the corpus pins it.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

/// `DefaultMinSize` (compress.go:44) — "the default minimum size until we enable compression.
/// 1500 is the MTU size for the internet since that is the largest size allowed at the network
/// level", and then 1024 regardless.
const DEFAULT_MIN_SIZE: usize = 1024;

/// `bufferBeforeChunkingSize` (net/http/server.go) — the most a handler can write and still get
/// a `Content-Length` computed for it.
const BUFFER_BEFORE_CHUNKING_SIZE: usize = 2048;

/// The encoding `selectEncoding` picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    None,
    Gzip,
    Zstd,
}

/// Port of `selectEncoding` (compress.go) with the default wrapper's `gzipEnabled`,
/// `zstdEnabled` and `preferZstd`, all `true`.
///
/// Per-coding q-values, not a sorted preference list: the **first** occurrence of each coding
/// name decides its q, a strictly higher q wins, and a tie goes to zstd.
pub fn select_encoding(method: &Method, accept_encoding: Option<&str>) -> Encoding {
    if method == Method::HEAD {
        return Encoding::None;
    }
    let Some(ae) = accept_encoding.filter(|ae| !ae.is_empty()) else {
        return Encoding::None;
    };
    let gzip_q = parse_encoding_q_value(ae, "gzip");
    let zstd_q = parse_encoding_q_value(ae, "zstd");

    if gzip_q <= 0.0 && zstd_q <= 0.0 {
        return Encoding::None;
    }
    if zstd_q <= 0.0 {
        return Encoding::Gzip;
    }
    if gzip_q <= 0.0 {
        return Encoding::Zstd;
    }
    if zstd_q > gzip_q {
        return Encoding::Zstd;
    }
    if gzip_q > zstd_q {
        return Encoding::Gzip;
    }
    Encoding::Zstd
}

/// Port of `parseEncodingQValue` (compress.go): walk the comma-separated codings and return the
/// q of the first whose name is `enc`, or 0.
fn parse_encoding_q_value(header: &str, enc: &str) -> f64 {
    let mut header = header.trim();
    while !header.is_empty() {
        let stop = header.find(',').unwrap_or(header.len());
        let (coding, qvalue) = parse_coding(&header[..stop]);
        if coding == enc {
            return qvalue;
        }
        if stop == header.len() {
            break;
        }
        header = &header[stop + 1..];
    }
    0.0
}

/// Port of `parseCoding` (compress.go) — the name lowercased and the q-value clamped to
/// `[0, 1]`. The error half is not returned: `parseEncodingQValue` discards it, and an
/// unparseable `q=` leaves the value `strconv.ParseFloat` returned, which is `0`.
fn parse_coding(s: &str) -> (String, f64) {
    if s.is_empty() {
        return (String::new(), 0.0);
    }
    if !s.contains(';') {
        return (s.trim().to_lowercase(), 1.0);
    }
    let mut coding = String::new();
    let mut qvalue = 1.0;
    for (n, part) in s.split(';').enumerate() {
        let part = part.trim();
        if n == 0 {
            coding = part.to_lowercase();
        } else if let Some(after) = part.strip_prefix("q=") {
            // `strconv.ParseFloat` answers 0 on a syntax error, and Go keeps that 0.
            qvalue = go_parse_float(after).clamp(0.0, 1.0);
        }
    }
    (coding, qvalue)
}

/// `strconv.ParseFloat(s, 64)`'s value, with the error folded to `0` the way `parseCoding` uses
/// it. Rust's parser accepts `inf`, `nan` and `infinity` as Go's does; both reject a leading `+`
/// on nothing, a trailing garbage byte and the empty string. The one grammar Go has and Rust does
/// not is hex floats and `_` separators, and a q-value is neither.
fn go_parse_float(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0)
}

/// Port of `DefaultContentTypeFilter` (compress.go): lowercase and trim, then refuse anything
/// naming a compressed format anywhere, or starting with `video/`, `audio/` or `image/jp`.
/// An empty type passes.
pub fn default_content_type_filter(ct: &str) -> bool {
    const EXCLUDE_CONTAINS: [&str; 8] = [
        "compress", "zip", "snappy", "lzma", "xz", "zstd", "brotli", "stuffit",
    ];
    const EXCLUDE_PREFIX: [&str; 3] = ["video/", "audio/", "image/jp"];
    let ct = ct.trim().to_lowercase();
    if ct.is_empty() {
        return true;
    }
    if EXCLUDE_CONTAINS.iter().any(|s| ct.contains(s)) {
        return false;
    }
    !EXCLUDE_PREFIX.iter().any(|p| ct.starts_with(p))
}

/// Port of gzhttp's `atoi`: the header's integer, or 0 when it is absent or not a number.
fn content_length(headers: &HeaderMap) -> usize {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0)
}

/// `bodyAllowedForStatus` (compress.go).
fn body_allowed_for_status(status: StatusCode) -> bool {
    let code = status.as_u16();
    !((100..=199).contains(&code) || code == 204 || code == 304)
}

/// Whether the wrapper compresses a response of this shape — steps 3 and 4 of the module docs.
///
/// `body_len` is what the handler wrote. `content_type` is the header, which the static handler
/// always sets before writing, so gzhttp's own `DetectContentType` fallback is not reached.
fn should_compress(status: StatusCode, headers: &HeaderMap, body_len: usize) -> bool {
    if body_len == 0 || !body_allowed_for_status(status) {
        return false;
    }
    if headers.contains_key(header::CONTENT_ENCODING) || headers.contains_key(header::CONTENT_RANGE)
    {
        return false;
    }
    let declared = content_length(headers);
    let long_enough = if declared == 0 {
        body_len >= DEFAULT_MIN_SIZE
    } else {
        declared >= DEFAULT_MIN_SIZE
    };
    if !long_enough {
        return false;
    }
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    default_content_type_filter(ct)
}

/// The wrapper's errors — only the compressor's, which on an in-memory buffer can only be an
/// allocation failure surfacing as `io::Error`.
#[derive(Debug, thiserror::Error)]
pub enum GzhttpError {
    #[error("reading the response body to compress it: {0}")]
    Body(#[from] axum::Error),
    #[error("compressing the response body: {0}")]
    Compress(#[from] std::io::Error),
    #[error("the compression task did not finish: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Port of `GzipHandler(h)` applied to a finished response: add `Vary`, then compress when the
/// request accepts an encoding and the response qualifies.
///
/// The body is read whole to compress it. A static asset is at most a few tens of megabytes (the
/// largest are source maps), and `http.FileServer` behind gzhttp buffers nothing, so this is the
/// one place the port holds more in memory than Go does.
pub async fn wrap(
    method: &Method,
    accept_encoding: Option<&str>,
    response: Response,
) -> Result<Response, GzhttpError> {
    let (mut parts, body) = response.into_parts();
    parts
        .headers
        .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));

    let encoding = select_encoding(method, accept_encoding);
    if encoding == Encoding::None || !body_allowed_for_status(parts.status) {
        return Ok(Response::from_parts(parts, body));
    }
    if parts.headers.contains_key(header::CONTENT_ENCODING)
        || parts.headers.contains_key(header::CONTENT_RANGE)
    {
        return Ok(Response::from_parts(parts, body));
    }

    let bytes = axum::body::to_bytes(body, usize::MAX).await?;
    if !should_compress(parts.status, &parts.headers, bytes.len()) {
        return Ok(Response::from_parts(parts, go_framed_body(bytes)));
    }

    let compressed = tokio::task::spawn_blocking(move || compress(encoding, &bytes)).await??;
    let value = match encoding {
        Encoding::Zstd => "zstd",
        _ => "gzip",
    };
    parts
        .headers
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static(value));
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ACCEPT_RANGES);
    Ok(Response::from_parts(
        parts,
        go_framed_body(Bytes::from(compressed)),
    ))
}

fn compress(encoding: Encoding, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    match encoding {
        // `zstd.SpeedFastest` is the library's level 1.
        Encoding::Zstd => zstd::stream::encode_all(bytes, 1),
        _ => {
            // `gzip.DefaultCompression` — level 6 in both implementations.
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes)?;
            encoder.finish()
        }
    }
}

/// A body framed the way `net/http` frames one a handler wrote without setting
/// `Content-Length`: at most [`BUFFER_BEFORE_CHUNKING_SIZE`] bytes get a length, anything longer
/// is chunked.
///
/// hyper writes a `Content-Length` for a body whose size it knows exactly and chunks a stream, so
/// the long case is handed over as a one-item stream to hide its size.
pub fn go_framed_body(bytes: Bytes) -> Body {
    if bytes.len() <= BUFFER_BEFORE_CHUNKING_SIZE {
        Body::from(bytes)
    } else {
        Body::from_stream(futures_util::stream::once(async move {
            Ok::<_, std::convert::Infallible>(bytes)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tie_goes_to_zstd_and_a_higher_q_wins() {
        let get = Method::GET;
        assert_eq!(
            select_encoding(&get, Some("gzip, deflate, br, zstd")),
            Encoding::Zstd
        );
        assert_eq!(select_encoding(&get, Some("gzip")), Encoding::Gzip);
        assert_eq!(
            select_encoding(&get, Some("zstd;q=0.5, gzip")),
            Encoding::Gzip
        );
        assert_eq!(
            select_encoding(&get, Some("gzip;q=0.5, zstd;q=0.9")),
            Encoding::Zstd
        );
        assert_eq!(select_encoding(&get, Some("gzip;q=0")), Encoding::None);
        assert_eq!(select_encoding(&get, Some("br")), Encoding::None);
        assert_eq!(select_encoding(&get, Some("")), Encoding::None);
        assert_eq!(select_encoding(&get, None), Encoding::None);
        assert_eq!(select_encoding(&Method::HEAD, Some("gzip")), Encoding::None);
    }

    #[test]
    fn the_first_occurrence_of_a_coding_decides_its_q() {
        assert_eq!(
            select_encoding(&Method::GET, Some("gzip;q=0, gzip")),
            Encoding::None
        );
        assert_eq!(parse_encoding_q_value("GZIP;Q=0.4", "gzip"), 1.0);
        assert_eq!(parse_encoding_q_value("gzip;q=0.4", "gzip"), 0.4);
        assert_eq!(parse_encoding_q_value("gzip;q=7", "gzip"), 1.0);
        assert_eq!(parse_encoding_q_value("gzip;q=x", "gzip"), 0.0);
    }

    #[test]
    fn png_is_compressed_and_jpeg_audio_video_and_archives_are_not() {
        assert!(default_content_type_filter("image/png"));
        assert!(default_content_type_filter(
            "text/javascript; charset=utf-8"
        ));
        assert!(default_content_type_filter(""));
        assert!(!default_content_type_filter("image/jpeg"));
        assert!(!default_content_type_filter("audio/mpeg"));
        assert!(!default_content_type_filter("video/mp4"));
        assert!(!default_content_type_filter("application/zip"));
        assert!(!default_content_type_filter("application/x-Compress"));
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn the_minimum_size_is_judged_by_the_declared_length_when_there_is_one() {
        let js = [("content-type", "text/javascript; charset=utf-8")];
        let ok = StatusCode::OK;
        assert!(!should_compress(ok, &headers(&js), 1023));
        assert!(should_compress(ok, &headers(&js), 1024));
        let declared_small = [("content-type", "text/css"), ("content-length", "10")];
        assert!(!should_compress(ok, &headers(&declared_small), 5000));
        let declared_big = [("content-type", "text/css"), ("content-length", "5000")];
        assert!(should_compress(ok, &headers(&declared_big), 5000));
        let ranged = [
            ("content-type", "text/css"),
            ("content-range", "bytes 0-9/5000"),
        ];
        assert!(!should_compress(
            StatusCode::PARTIAL_CONTENT,
            &headers(&ranged),
            5000
        ));
        assert!(!should_compress(
            StatusCode::NOT_MODIFIED,
            &headers(&js),
            5000
        ));
    }
}

#[cfg(test)]
mod go_parity {
    //! Against `fixtures/behaviour_web_static.json`, which drives the real `gzhttp.GzipHandler`
    //! behind a real `net/http` server — see reference/dump/behaviour_web_static.go.
    use super::*;
    use axum::body::HttpBody as _;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_web_static.json"))
            .expect("behaviour_web_static.json is generated by reference/dump")
    }

    fn body_of(n: usize) -> Vec<u8> {
        let pattern = b"function mattermost(){return 'static asset';}\n";
        pattern.iter().copied().cycle().take(n).collect()
    }

    fn decode(encoding: Option<&str>, bytes: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        match encoding {
            Some("gzip") => {
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(bytes)
                    .read_to_end(&mut out)
                    .unwrap();
                out
            }
            Some("zstd") => zstd::stream::decode_all(bytes).unwrap(),
            _ => bytes.to_vec(),
        }
    }

    #[tokio::test]
    async fn every_gzhttp_row_matches_go() {
        let oracle = oracle();
        let rows = oracle["gzhttp"].as_array().unwrap();
        assert!(rows.len() > 40, "the corpus is there");
        for row in rows {
            let case = &row["case"];
            let want = &row["response"];
            let method = Method::from_bytes(case["method"].as_str().unwrap().as_bytes()).unwrap();
            let ae = case["accept_encoding"].as_str().unwrap();
            let status = StatusCode::from_u16(case["status"].as_u64().unwrap() as u16).unwrap();
            let ct = case["content_type"].as_str().unwrap();
            let declared = case["declared_length"].as_u64().unwrap();
            let n = case["body_length"].as_u64().unwrap() as usize;
            let range = case["content_range"].as_str().unwrap();

            let mut builder = Response::builder().status(status);
            if !ct.is_empty() {
                builder = builder.header(header::CONTENT_TYPE, ct);
            }
            if declared > 0 {
                builder = builder.header(header::CONTENT_LENGTH, declared.to_string());
            }
            if !range.is_empty() {
                builder = builder.header(header::CONTENT_RANGE, range);
            }
            let written =
                status != StatusCode::NOT_MODIFIED && status != StatusCode::MOVED_PERMANENTLY;
            let input = if written { body_of(n) } else { Vec::new() };
            // What reaches the wire for a HEAD is decided by hyper, which drops the body.
            let response = builder.body(Body::from(input.clone())).unwrap();
            let accept = (!ae.is_empty()).then_some(ae);
            let out = wrap(&method, accept, response).await.unwrap();

            let context = format!("{case}");
            assert_eq!(
                out.status().as_u16() as u64,
                want["status"].as_u64().unwrap(),
                "{context}"
            );
            let got = |name: header::HeaderName| {
                out.headers()
                    .get(name)
                    .map(|v| v.to_str().unwrap().to_owned())
            };
            let wanted = |name: &str| want["headers"][name].as_str().map(str::to_owned);
            assert_eq!(
                got(header::CONTENT_ENCODING),
                wanted("content-encoding"),
                "{context}"
            );
            assert_eq!(got(header::VARY), wanted("vary"), "{context}");
            assert_eq!(
                got(header::CONTENT_TYPE),
                wanted("content-type"),
                "{context}"
            );
            assert_eq!(
                got(header::CONTENT_RANGE),
                wanted("content-range"),
                "{context}"
            );
            assert_eq!(
                got(header::ACCEPT_RANGES),
                wanted("accept-ranges"),
                "{context}"
            );

            let compressed = wanted("content-encoding").is_some();
            let head = method == Method::HEAD;
            let exact = out.body().size_hint().exact();
            let explicit = got(header::CONTENT_LENGTH);
            // hyper writes the explicit header, else a known body size (never for a HEAD body it
            // is about to drop, which here is always explicitly sized when Go sized it).
            // And never for a status that carries no body (a 304).
            let framed_length = explicit.clone().or_else(|| {
                (!head && body_allowed_for_status(out.status()))
                    .then_some(exact)
                    .flatten()
                    .map(|l| l.to_string())
            });
            if compressed {
                assert_eq!(
                    framed_length.is_some(),
                    wanted("content-length").is_some(),
                    "{context}: a compressed length is present on both or neither"
                );
            } else {
                assert_eq!(framed_length, wanted("content-length"), "{context}");
            }
            assert_eq!(
                !head && body_allowed_for_status(out.status()) && framed_length.is_none(),
                want["chunked"].as_bool().unwrap(),
                "{context}: framing"
            );

            let bytes = axum::body::to_bytes(out.into_body(), usize::MAX)
                .await
                .unwrap();
            let decoded = if head {
                Vec::new()
            } else {
                decode(wanted("content-encoding").as_deref(), &bytes)
            };
            let expected_len = want["decoded_length"].as_u64().unwrap() as usize;
            assert_eq!(decoded.len(), expected_len, "{context}: decoded length");
            if !head && written {
                assert_eq!(decoded, input, "{context}: decodes to the input");
            }
        }
    }

    #[test]
    fn net_http_framing_is_a_length_up_to_2048_bytes_and_chunked_above() {
        let oracle = oracle();
        for row in oracle["framing"].as_array().unwrap() {
            let n = row["body_length"].as_u64().unwrap() as usize;
            let head = row["method"] == "HEAD";
            let want_length = row["content_length"].as_str().unwrap();
            if head && n == 0 {
                // `web_static::head_framing`'s case, not this function's.
                continue;
            }
            let exact = go_framed_body(Bytes::from(vec![b'x'; n]))
                .size_hint()
                .exact();
            let got = exact.map(|l| l.to_string()).unwrap_or_default();
            assert_eq!(got, want_length, "{row}");
            assert_eq!(
                !head && exact.is_none(),
                row["chunked"].as_bool().unwrap(),
                "{row}"
            );
        }
    }
}
