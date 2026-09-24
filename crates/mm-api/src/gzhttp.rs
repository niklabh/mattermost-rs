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

use axum::body::{Body, BodyDataStream, Bytes, HttpBody as _};
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
/// `buffered` is what the handler had written when gzhttp decided: at least `MinSize` bytes, or
/// the whole body when it was shorter. `content_type` is the header, or the sniffed type when the
/// handler set none (see [`wrap`]). Reduced from `Write` and `Close`: with a declared length the
/// decision is made on the first write, from the declared length alone; without one, gzhttp
/// buffers until it holds `MinSize` bytes or the handler returns.
fn should_compress(
    status: StatusCode,
    headers: &HeaderMap,
    content_type: &str,
    buffered: usize,
) -> bool {
    if buffered == 0 || !body_allowed_for_status(status) {
        return false;
    }
    if headers.contains_key(header::CONTENT_ENCODING) || headers.contains_key(header::CONTENT_RANGE)
    {
        return false;
    }
    let declared = content_length(headers);
    let long_enough = if declared == 0 {
        buffered >= DEFAULT_MIN_SIZE
    } else {
        declared >= DEFAULT_MIN_SIZE
    };
    long_enough && default_content_type_filter(content_type)
}

/// The wrapper's errors: reading the handler's body, or the compressor's, which on an in-memory
/// buffer can only be an allocation failure surfacing as `io::Error`.
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
/// # The body is a stream, as the handler's writes are
///
/// Go decides after the handler has written `MinSize` bytes (or on its first write, when it
/// declared a length), and compresses the rest as it arrives. So does this: at most `MinSize`
/// bytes of the body — one chunk, for a body built in memory — are read before deciding, and a
/// file download (`serve_content`, which declares its length and streams) is compressed chunk by
/// chunk rather than held whole. What reaches the client is framed by `net/http`'s rule applied
/// to what is actually *written*: at most 2048 bytes by the time the handler returns get a
/// `Content-Length`, anything longer is chunked — so up to 2049 bytes of output are held back to
/// tell the two apart, compressed or not.
///
/// # A response with no `Content-Type`
///
/// gzhttp sniffs one with `http.DetectContentType` over what it has buffered and **sets the
/// header** (its `setContentType` default) before filtering on it, so a JPEG written without a
/// type is recognised and left alone. `net/http` would sniff the same bytes had gzhttp not, so
/// the header is on the wire either way.
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

    let mut stream = body.into_data_stream();
    let mut head = Vec::new();
    let mut ended = read_until(&mut stream, &mut head, DEFAULT_MIN_SIZE).await?;

    let declared = content_length(&parts.headers);
    let content_type = match parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some(ct) if !ct.is_empty() => ct.to_owned(),
        _ if head.is_empty() => String::new(),
        _ => {
            let sniffed = mm_app::link_image::detect_content_type(&head);
            if !parts.headers.contains_key(header::CONTENT_TYPE) {
                parts
                    .headers
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(sniffed));
            }
            sniffed.to_owned()
        }
    };

    if !should_compress(parts.status, &parts.headers, &content_type, head.len()) {
        if declared > 0 {
            // The handler's own `Content-Length` frames it; nothing to hold back.
            let body = futures_util::StreamExt::chain(
                futures_util::stream::once(async move { Ok(Bytes::from(head)) }),
                stream,
            );
            return Ok(Response::from_parts(parts, Body::from_stream(body)));
        }
        if !ended {
            ended = read_until(&mut stream, &mut head, BUFFER_BEFORE_CHUNKING_SIZE + 1).await?;
        }
        let body = if ended {
            go_framed_body(Bytes::from(head))
        } else {
            Body::from_stream(futures_util::StreamExt::chain(
                futures_util::stream::once(async move { Ok(Bytes::from(head)) }),
                stream,
            ))
        };
        return Ok(Response::from_parts(parts, body));
    }

    let value = match encoding {
        Encoding::Zstd => "zstd",
        _ => "gzip",
    };
    parts
        .headers
        .insert(header::CONTENT_ENCODING, HeaderValue::from_static(value));
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ACCEPT_RANGES);
    let body = compressed_body(encoding, Bytes::from(head), stream, ended).await?;
    Ok(Response::from_parts(parts, body))
}

/// Pull chunks into `buf` until it holds at least `want` bytes; `true` when the body ended first.
async fn read_until(
    stream: &mut BodyDataStream,
    buf: &mut Vec<u8>,
    want: usize,
) -> Result<bool, axum::Error> {
    while buf.len() < want {
        match futures_util::StreamExt::next(stream).await {
            Some(chunk) => buf.extend_from_slice(&chunk?),
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// One of the two encoders, fed a chunk at a time; each call returns what it emitted.
enum Compressor {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Compressor {
    fn new(encoding: Encoding) -> std::io::Result<Self> {
        Ok(match encoding {
            // `zstd.SpeedFastest` is the library's level 1.
            Encoding::Zstd => Compressor::Zstd(zstd::stream::write::Encoder::new(Vec::new(), 1)?),
            // `gzip.DefaultCompression` — level 6 in both implementations.
            _ => Compressor::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            )),
        })
    }

    fn write(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write as _;
        let out = match self {
            Compressor::Gzip(e) => {
                e.write_all(chunk)?;
                e.get_mut()
            }
            Compressor::Zstd(e) => {
                e.write_all(chunk)?;
                e.get_mut()
            }
        };
        Ok(std::mem::take(out))
    }

    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Compressor::Gzip(e) => e.finish(),
            Compressor::Zstd(e) => e.finish(),
        }
    }
}

/// Compress one chunk off the async threads; the encoder travels in and back out.
async fn feed(
    mut compressor: Compressor,
    chunk: Bytes,
) -> Result<(Compressor, Vec<u8>), GzhttpError> {
    let (compressor, out) = tokio::task::spawn_blocking(move || {
        let out = compressor.write(&chunk);
        (compressor, out)
    })
    .await?;
    Ok((compressor, out?))
}

async fn finish(compressor: Compressor) -> Result<Vec<u8>, GzhttpError> {
    Ok(tokio::task::spawn_blocking(move || compressor.finish()).await??)
}

/// The compressed body, framed as `net/http` frames what the compressor writes: output is held
/// until it passes 2048 bytes or the body ends, which decides between a length and chunks.
async fn compressed_body(
    encoding: Encoding,
    head: Bytes,
    mut stream: BodyDataStream,
    mut ended: bool,
) -> Result<Body, GzhttpError> {
    let (mut compressor, mut out) = feed(Compressor::new(encoding)?, head).await?;
    while out.len() <= BUFFER_BEFORE_CHUNKING_SIZE {
        if ended {
            out.extend(finish(compressor).await?);
            return Ok(go_framed_body(Bytes::from(out)));
        }
        match futures_util::StreamExt::next(&mut stream).await {
            Some(chunk) => {
                let (next, emitted) = feed(compressor, chunk?).await?;
                compressor = next;
                out.extend(emitted);
            }
            None => ended = true,
        }
    }

    let rest =
        futures_util::stream::try_unfold(Some((compressor, stream, ended)), |state| async move {
            let Some((mut compressor, mut stream, ended)) = state else {
                return Ok::<_, GzhttpError>(None);
            };
            if !ended {
                while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
                    let (next, emitted) = feed(compressor, chunk?).await?;
                    compressor = next;
                    if !emitted.is_empty() {
                        return Ok(Some((
                            Bytes::from(emitted),
                            Some((compressor, stream, false)),
                        )));
                    }
                }
            }
            Ok(Some((Bytes::from(finish(compressor).await?), None)))
        });
    let first = futures_util::stream::once(async move { Ok(Bytes::from(out)) });
    Ok(Body::from_stream(futures_util::StreamExt::chain(
        first, rest,
    )))
}

/// `net/http`'s framing of an answer the wrapper left alone — no acceptable encoding, or not
/// `gzip` mode: a body over [`BUFFER_BEFORE_CHUNKING_SIZE`] bytes that declared no length is
/// chunked, where hyper would write the length of a body it holds whole.
///
/// `HEAD` is left to hyper: `net/http` writes neither a length nor chunks for a `HEAD` whose
/// handler wrote more than the buffer, and no API client reads that framing.
pub fn net_http_framing(method: &Method, response: Response) -> Response {
    let long = response
        .body()
        .size_hint()
        .exact()
        .is_some_and(|n| n > BUFFER_BEFORE_CHUNKING_SIZE as u64);
    if !long
        || method == Method::HEAD
        || !body_allowed_for_status(response.status())
        || response.headers().contains_key(header::CONTENT_LENGTH)
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::from_stream(body.into_data_stream()))
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
        let js_ct = "text/javascript; charset=utf-8";
        let ok = StatusCode::OK;
        assert!(!should_compress(ok, &headers(&js), js_ct, 1023));
        assert!(should_compress(ok, &headers(&js), js_ct, 1024));
        assert!(!should_compress(ok, &headers(&js), js_ct, 0));
        let declared_small = [("content-type", "text/css"), ("content-length", "10")];
        assert!(!should_compress(
            ok,
            &headers(&declared_small),
            "text/css",
            5000
        ));
        let declared_big = [("content-type", "text/css"), ("content-length", "5000")];
        assert!(should_compress(
            ok,
            &headers(&declared_big),
            "text/css",
            5000
        ));
        // Go decides on the first write when a length is declared, however short that write.
        assert!(should_compress(ok, &headers(&declared_big), "text/css", 1));
        // The filter reads the type it is given (the sniffed one when the header is absent).
        assert!(!should_compress(
            ok,
            &headers(&declared_big),
            "image/jpeg",
            5000
        ));
        let ranged = [
            ("content-type", "text/css"),
            ("content-range", "bytes 0-9/5000"),
        ];
        assert!(!should_compress(
            StatusCode::PARTIAL_CONTENT,
            &headers(&ranged),
            "text/css",
            5000
        ));
        assert!(!should_compress(
            StatusCode::NOT_MODIFIED,
            &headers(&js),
            js_ct,
            5000
        ));
    }
}

#[cfg(test)]
mod go_parity {
    //! Against `fixtures/behaviour_web_static.json`, which drives the real `gzhttp.GzipHandler`
    //! behind a real `net/http` server — see reference/dump/behaviour_web_static.go.
    use super::*;

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

    /// `reference/dump`'s `noise`: the top byte of a 64-bit LCG, MMIX constants.
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (x >> 56) as u8
            })
            .collect()
    }

    fn stream_payload(kind: &str, n: usize) -> Vec<u8> {
        match kind {
            "noise" => noise(n),
            "jpeg" => {
                let mut out = noise(n);
                let magic = [0xFF, 0xD8, 0xFF, 0xE0];
                let k = magic.len().min(n);
                out[..k].copy_from_slice(&magic[..k]);
                out
            }
            _ => body_of(n),
        }
    }

    /// The API handlers' shape: several writes, a streamed declared length, no content type —
    /// `gzhttp_stream` in the oracle. The handler's writes become the chunks of a stream body of
    /// unknown size, which is what `serve_content` hands the wrapper.
    #[tokio::test]
    async fn every_streamed_gzhttp_row_matches_go() {
        let oracle = oracle();
        let rows = oracle["gzhttp_stream"].as_array().unwrap();
        assert!(rows.len() >= 17, "the corpus is there");
        for row in rows {
            let case = &row["case"];
            let want = &row["response"];
            let name = case["name"].as_str().unwrap();
            let ae = case["accept_encoding"].as_str().unwrap();
            let ct = case["content_type"].as_str().unwrap();
            let declared = case["declared_length"].as_u64().unwrap();
            let n = case["body_length"].as_u64().unwrap() as usize;
            let write = case["write_size"].as_u64().unwrap() as usize;
            let input = stream_payload(case["body_kind"].as_str().unwrap(), n);

            let mut builder = Response::builder().status(StatusCode::OK);
            if !ct.is_empty() {
                builder = builder.header(header::CONTENT_TYPE, ct);
            }
            if declared > 0 {
                builder = builder.header(header::CONTENT_LENGTH, declared.to_string());
            }
            let body = if write == 0 {
                Body::from(input.clone())
            } else {
                let chunks: Vec<Result<Bytes, std::convert::Infallible>> = input
                    .chunks(write)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                Body::from_stream(futures_util::stream::iter(chunks))
            };
            let response = builder.body(body).unwrap();
            let out = wrap(&Method::GET, (!ae.is_empty()).then_some(ae), response)
                .await
                .unwrap();

            let got = |name: header::HeaderName| {
                out.headers()
                    .get(name)
                    .map(|v| v.to_str().unwrap().to_owned())
            };
            let wanted = |name: &str| want["headers"][name].as_str().map(str::to_owned);
            for h in [header::CONTENT_ENCODING, header::VARY, header::CONTENT_TYPE] {
                assert_eq!(got(h.clone()), wanted(h.as_str()), "{name}: {h}");
            }
            let compressed = wanted("content-encoding").is_some();
            let framed_length = got(header::CONTENT_LENGTH)
                .or_else(|| out.body().size_hint().exact().map(|l| l.to_string()));
            if compressed {
                assert_eq!(
                    framed_length.is_some(),
                    wanted("content-length").is_some(),
                    "{name}: a compressed length is present on both or neither"
                );
            } else {
                assert_eq!(framed_length, wanted("content-length"), "{name}: length");
            }
            assert_eq!(
                framed_length.is_none(),
                want["chunked"].as_bool().unwrap(),
                "{name}: framing"
            );

            let bytes = axum::body::to_bytes(out.into_body(), usize::MAX)
                .await
                .unwrap();
            let decoded = decode(wanted("content-encoding").as_deref(), &bytes);
            assert_eq!(
                decoded.len() as u64,
                want["decoded_length"].as_u64().unwrap(),
                "{name}: decoded length"
            );
            assert!(want["decodes_to_body"].as_bool().unwrap(), "{name}");
            assert!(decoded == input, "{name}: decodes to the input");
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
