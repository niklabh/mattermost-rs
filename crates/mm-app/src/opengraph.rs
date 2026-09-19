//! Port of `app/opengraph.go` and the `app/oembed` package — turning a fetched page, or an oEmbed
//! provider's answer, into the `*opengraph.OpenGraph` a link preview carries.
//!
//! # The image proxy is not here
//!
//! `parseOpenGraphMetadata` and `parseOpenGraphFromOEmbed` both end by rewriting every image
//! through `ImageProxyAdder` when `ImageProxySettings.Enable` is on. That branch is not ported:
//! the caller refuses the whole link-preview path while the proxy is enabled, before anything is
//! fetched, so these functions only ever run where Go's `toProxyURL` is nil.
//!
//! # When the answer is "not reproduced"
//!
//! [`OpenGraphError::Unreproducible`] means Go's answer is known to differ from what this port
//! could compute, or cannot be ruled out to: a multi-byte or UTF-16 charset with non-ASCII bytes
//! ([`mm_model::go_charset`]); a page passed through undecoded whose meta content holds invalid
//! UTF-8 (Go's `json.Marshal` writes each such byte as the escape `\ufffd`, a real U+FFFD as
//! itself, and a Rust string cannot keep the difference); a time whose zone Go cannot marshal.
//! The caller forwards rather than guess. Pinned by
//! `fixtures/behaviour_opengraph.json`.

use mm_model::go_charset::{CharsetError, force_html_encoding_to_utf8};
use mm_model::go_url::{self, GoUrl};
use mm_model::link_metadata::filter_svg_images;
use mm_model::opengraph::{self, OpenGraph};

/// `MaxOpenGraphResponseSize` (app/opengraph.go:23) — the `io.LimitReader` on every body this
/// module reads, 50 MiB.
pub const MAX_OPEN_GRAPH_RESPONSE_SIZE: usize = 1024 * 1024 * 50;

/// Why a page or an oEmbed answer produced no OpenGraph here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenGraphError {
    /// Go's answer cannot be reproduced for this input; see the module docs.
    #[error("not reproduced: {0}")]
    Unreproducible(&'static str),
    /// Go's own error — `ResponseFromJSON` refusing the body. The text is diagnostic only.
    #[error("{0}")]
    OEmbed(String),
}

impl From<CharsetError> for OpenGraphError {
    fn from(err: CharsetError) -> Self {
        match err {
            CharsetError::Unreproducible(_) => {
                OpenGraphError::Unreproducible("the page's charset decoder")
            }
        }
    }
}

fn limited(body: &[u8]) -> &[u8] {
    &body[..body.len().min(MAX_OPEN_GRAPH_RESPONSE_SIZE)]
}

/// Port of `(*App).parseOpenGraphMetadata` (app/opengraph.go:54), without the image proxy.
///
/// The steps are Go's, in Go's order: the 50 MiB limit, `forceHTMLEncodingToUTF8`,
/// `ProcessHTML`, the relative URLs made absolute against `request_url`, the title and
/// description **unescaped a second time** (`html.UnescapeString`, on top of the tokenizer's own
/// decoding of the attribute — so `&amp;lt;` in a page arrives as `<`), SVG images dropped, and
/// finally a non-empty `og:url` replaced by `request_url`: "the URL should be the link the user
/// provided in their message, not a redirected one".
pub fn parse_open_graph_metadata(
    request_url: &str,
    body: &[u8],
    content_type: &str,
) -> Result<OpenGraph, OpenGraphError> {
    let doc = force_html_encoding_to_utf8(limited(body), content_type)?;
    let mut og = OpenGraph::default();
    og.process_html(&doc.bytes);
    if og.lossy {
        // Go keeps the raw bytes, and `json.Marshal` then writes each as the six-byte escape
        // `\ufffd` — while a real U+FFFD in the page is written as itself. A Rust string cannot
        // tell the two apart afterwards, so the embed's bytes would differ.
        return Err(OpenGraphError::Unreproducible(
            "a page whose meta content is not valid UTF-8",
        ));
    }

    make_open_graph_urls_absolute(&mut og, request_url);

    open_graph_decode_html_entities(&mut og);

    let mut og = filter_svg_images_from_open_graph(og);

    if !og.url.is_empty() {
        og.url = request_url.to_owned();
    }
    if og.marshal_fails_in_go() {
        return Err(OpenGraphError::Unreproducible(
            "a time whose zone Go cannot marshal",
        ));
    }
    Ok(og)
}

/// Port of `makeOpenGraphURLsAbsolute` (app/opengraph.go:92). A request URL that does not parse
/// changes nothing; a result that does not parse, or already has a scheme, is left as it is.
fn make_open_graph_urls_absolute(og: &mut OpenGraph, request_url: &str) {
    let Ok(parsed_request_url) = go_url::go_parse(request_url) else {
        tracing::warn!(request_url, "makeOpenGraphURLsAbsolute failed to parse url");
        return;
    };
    let make = |result_url: &mut String| {
        if result_url.is_empty() {
            return;
        }
        let Ok(parsed) = go_url::go_parse(result_url) else {
            return;
        };
        if parsed.is_abs() {
            return;
        }
        *result_url = parsed_request_url.resolve_reference(&parsed).to_go_string();
    };

    make(&mut og.url);
    for image in og.images.iter_mut().flatten() {
        make(&mut image.url);
        make(&mut image.secure_url);
    }
    for audio in og.audios.iter_mut().flatten() {
        make(&mut audio.url);
        make(&mut audio.secure_url);
    }
    for video in og.videos.iter_mut().flatten() {
        make(&mut video.url);
        make(&mut video.secure_url);
    }
}

/// Port of `openGraphDecodeHTMLEntities` (app/opengraph.go:167): the standard library's
/// `html.UnescapeString`, which is not the tokenizer's `unescape` — see [`mm_model::go_html`].
fn open_graph_decode_html_entities(og: &mut OpenGraph) {
    og.title = mm_model::go_html::unescape_string(&og.title);
    og.description = mm_model::go_html::unescape_string(&og.description);
}

/// Port of `filterSVGImagesFromOpenGraph` (app/opengraph.go:157).
fn filter_svg_images_from_open_graph(mut og: OpenGraph) -> OpenGraph {
    if og.images.as_ref().is_none_or(Vec::is_empty) {
        return og;
    }
    og.images = filter_svg_images(og.images.take());
    og
}

/// Port of `(*App).parseOpenGraphFromOEmbed` (app/opengraph.go:172), without the image proxy.
///
/// The thumbnail's `int` dimensions become the image's `uint64` ones through Go's conversion,
/// so a negative width wraps to a very large one, as it does there.
pub fn parse_open_graph_from_oembed(
    request_url: &str,
    body: &[u8],
) -> Result<OpenGraph, OpenGraphError> {
    let response = oembed::response_from_json(limited(body))?;

    let mut og = OpenGraph::default();
    og.type_ = "opengraph".to_owned();
    og.title = response.title;
    og.url = request_url.to_owned();
    if !response.thumbnail_url.is_empty() {
        og.images = Some(vec![opengraph::Image {
            type_: "image".to_owned(),
            url: response.thumbnail_url,
            width: response.thumbnail_width as u64,
            height: response.thumbnail_height as u64,
            ..opengraph::Image::default()
        }]);
    }
    Ok(filter_svg_images_from_open_graph(og))
}

/// Port of `server/channels/app/oembed` (endpoint.go, oembed.go, providers_gen.go).
pub mod oembed {
    use std::sync::LazyLock;

    use super::{GoUrl, OpenGraphError, go_url};

    /// Port of `oembed.ProviderEndpoint` (endpoint.go:13).
    #[derive(Debug)]
    pub struct ProviderEndpoint {
        pub url: &'static str,
        patterns: Vec<regex::Regex>,
    }

    /// `providers` (providers_gen.go:17) — YouTube is the only one. The patterns are RE2 and
    /// every construct in them means the same in the `regex` crate.
    static PROVIDERS: LazyLock<Vec<ProviderEndpoint>> = LazyLock::new(|| {
        let patterns = [
            r"^https://[^/]*?\.youtube\.com/watch.*?$",
            r"^https://[^/]*?\.youtube\.com/v/.*?$",
            r"^https://youtu\.be/.*?$",
            r"^https://[^/]*?\.youtube\.com/playlist\?list=.*?$",
            r"^https://youtube\.com/playlist\?list=.*?$",
            r"^https://[^/]*?\.youtube\.com/shorts.*?$",
            r"^https://youtube\.com/shorts.*?$",
            r"^https://[^/]*?\.youtube\.com/embed/.*?$",
            r"^https://[^/]*?\.youtube\.com/live.*?$",
            r"^https://youtube\.com/live.*?$",
        ];
        vec![ProviderEndpoint {
            url: "https://www.youtube.com/oembed",
            // Constant patterns that compile; one that did not would drop out rather than panic.
            patterns: patterns
                .iter()
                .filter_map(|p| regex::Regex::new(p).ok())
                .collect(),
        }]
    });

    impl ProviderEndpoint {
        /// Port of `GetProviderURL` (endpoint.go:18): the endpoint with `format=json` and
        /// `url=<request>` added — `Values.Encode` sorts, so `format` comes first.
        pub fn get_provider_url(&self, request_url: &str) -> String {
            let Ok(mut url) = go_url::go_parse(self.url) else {
                return String::new();
            };
            let mut query = url.query();
            query.add(b"format", b"json");
            query.add(b"url", request_url.as_bytes());
            url.raw_query = query.encode();
            GoUrl::to_go_string(&url)
        }
    }

    /// Port of `FindEndpointForURL` (endpoint.go:32): the first provider with a pattern that
    /// matches.
    pub fn find_endpoint_for_url(request_url: &str) -> Option<&'static ProviderEndpoint> {
        PROVIDERS
            .iter()
            .find(|p| p.patterns.iter().any(|re| re.is_match(request_url)))
    }

    /// Port of `oembed.OEmbedResponse` (oembed.go:12), the fields a caller reads.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct OEmbedResponse {
        pub type_: String,
        pub version: String,
        pub title: String,
        pub thumbnail_url: String,
        pub thumbnail_width: i64,
        pub thumbnail_height: i64,
    }

    /// `OEmbedResponse`'s fields, each an `int` or a `string` in Go.
    const FIELDS: [(&str, bool); 15] = [
        ("type", false),
        ("version", false),
        ("title", false),
        ("author_name", false),
        ("author_url", false),
        ("provider_name", false),
        ("provider_url", false),
        ("cache_age", false),
        ("thumbnail_url", false),
        ("thumbnail_width", true),
        ("thumbnail_height", true),
        ("url", false),
        ("html", false),
        ("width", true),
        ("height", true),
    ];

    /// A JSON object's members in document order, which `serde_json::Map` does not keep.
    struct Ordered(Vec<(String, serde_json::Value)>);

    impl<'de> serde::Deserialize<'de> for Ordered {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = Option<Vec<(String, serde_json::Value)>>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("any JSON value")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut out = Vec::new();
                    while let Some((k, v)) = map.next_entry::<String, serde_json::Value>()? {
                        out.push((k, v));
                    }
                    Ok(Some(out))
                }
                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(Some(Vec::new()))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Self::Value, A::Error> {
                    while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                    Ok(None)
                }
                fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                    Ok(None)
                }
                fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                    Ok(None)
                }
                fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                    Ok(None)
                }
                fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                    Ok(None)
                }
                fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                    Ok(None)
                }
            }
            match d.deserialize_any(V)? {
                Some(members) => Ok(Ordered(members)),
                None => Err(serde::de::Error::custom(
                    "json: cannot unmarshal into Go value of type oembed.OEmbedResponse",
                )),
            }
        }
    }

    /// Port of `ResponseFromJSON` (oembed.go:38): `json.NewDecoder(r).Decode` — the **first**
    /// JSON value, whatever follows it — with Go's field matching (exact name, then
    /// case-insensitive, the last duplicate winning), a type error on any of the fifteen fields
    /// failing the whole decode, then the `version` and `type` smoke test.
    pub fn response_from_json(body: &[u8]) -> Result<OEmbedResponse, OpenGraphError> {
        // Go's decoder turns each invalid byte inside a string into U+FFFD; serde_json refuses
        // the document. Outside a string both refuse, so the per-byte rewrite is exact.
        let (text, _) = mm_model::opengraph::go_string(body);
        let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<Ordered>();
        let members = match stream.next() {
            Some(Ok(Ordered(members))) => members,
            Some(Err(err)) => return Err(OpenGraphError::OEmbed(err.to_string())),
            None => return Err(OpenGraphError::OEmbed("EOF".to_owned())),
        };

        let mut values: [Option<&serde_json::Value>; 15] = [None; 15];
        let mut type_error = false;
        for (key, value) in &members {
            let index = FIELDS.iter().position(|(name, _)| name == key).or_else(|| {
                let folded = mm_model::go_json::fold_name(key);
                FIELDS
                    .iter()
                    .position(|(name, _)| mm_model::go_json::fold_name(name) == folded)
            });
            let Some(index) = index else {
                continue;
            };
            let is_int = FIELDS[index].1;
            match value {
                serde_json::Value::Null => {}
                serde_json::Value::String(_) if !is_int => values[index] = Some(value),
                serde_json::Value::Number(n) if is_int && n.as_i64().is_some() => {
                    values[index] = Some(value);
                }
                _ => type_error = true,
            }
        }
        if type_error {
            return Err(OpenGraphError::OEmbed(
                "json: cannot unmarshal into a field of oembed.OEmbedResponse".to_owned(),
            ));
        }
        let text = |i: usize| {
            values[i]
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let int = |i: usize| values[i].and_then(serde_json::Value::as_i64).unwrap_or(0);
        let response = OEmbedResponse {
            type_: text(0),
            version: text(1),
            title: text(2),
            thumbnail_url: text(8),
            thumbnail_width: int(9),
            thumbnail_height: int(10),
        };
        if response.version != "1.0" {
            return Err(OpenGraphError::OEmbed(format!(
                "ResponseFromJson: Received unsupported response version {}",
                response.version
            )));
        }
        if !matches!(response.type_.as_str(), "photo" | "video" | "link" | "rich") {
            return Err(OpenGraphError::OEmbed(format!(
                "ResponseFromJson: Received unsupported response type {}",
                response.type_
            )));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn og_url_is_the_request_url_and_images_are_resolved() {
        let html = br#"<meta property="og:url" content="/elsewhere"><meta property="og:title" content="a &amp;lt; b">
            <meta property="og:image" content="../i.png"><meta property="og:image" content="x.svg">"#;
        let og = parse_open_graph_metadata("http://h/p/q", html, "text/html").unwrap();
        assert_eq!(og.url, "http://h/p/q");
        assert_eq!(og.title, "a < b");
        let images = og.images.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].url, "http://h/i.png");
    }

    #[test]
    fn oembed_needs_version_and_type() {
        assert!(oembed::response_from_json(br#"{"version":"1.0","type":"video"}"#).is_ok());
        assert!(oembed::response_from_json(br#"{"version":"2.0","type":"video"}"#).is_err());
        assert!(oembed::response_from_json(br#"{"version":"1.0","type":"x"}"#).is_err());
        assert!(
            oembed::response_from_json(br#"{"version":"1.0","type":"video","width":"1"}"#).is_err()
        );
        let r = oembed::response_from_json(
            br#"{"VERSION":"1.0","Type":"video","title":null,"TITLE":"t"} trailing"#,
        )
        .unwrap();
        assert_eq!(r.title, "t");
    }

    #[test]
    fn youtube_is_the_provider() {
        let p = oembed::find_endpoint_for_url("https://www.youtube.com/watch?v=1").unwrap();
        assert_eq!(
            p.get_provider_url("https://www.youtube.com/watch?v=1"),
            "https://www.youtube.com/oembed?format=json&url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D1"
        );
        assert!(oembed::find_endpoint_for_url("http://www.youtube.com/watch?v=1").is_none());
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine;
    use mm_model::link_metadata::truncate_open_graph;
    use mm_model::utils::go_json_marshal;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_opengraph.json"))
            .expect("the fixture is JSON")
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        match v.as_str() {
            Some(s) => base64::engine::general_purpose::STANDARD
                .decode(s)
                .expect("base64"),
            None => Vec::new(),
        }
    }

    /// `parseOpenGraphMetadata` and then `TruncateOpenGraph`, both compared as the **bytes** Go's
    /// `json.Marshal` wrote. A case this port refuses must be one Go could not marshal either.
    #[test]
    fn parse_open_graph_metadata_matches_go_byte_for_byte() {
        let o = oracle();
        let cases = o["opengraph"].as_array().expect("cases");
        assert!(cases.len() > 450);
        let mut refused = Vec::new();
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let body = bytes(&case["body"]);
            let url = case["request_url"].as_str().expect("url");
            let ct = case["content_type"].as_str().expect("ct");
            match parse_open_graph_metadata(url, &body, ct) {
                Ok(og) => {
                    assert!(!case["parsed_err"].as_bool().expect("flag"), "{name}");
                    assert_eq!(
                        go_json_marshal(&og).expect("marshals"),
                        case["parsed"].as_str().expect("parsed"),
                        "{name}: parsed"
                    );
                    assert_eq!(
                        go_json_marshal(&truncate_open_graph(Some(og))).expect("marshals"),
                        case["truncated"].as_str().expect("truncated"),
                        "{name}: truncated"
                    );
                }
                Err(OpenGraphError::Unreproducible(why)) => refused.push((name.to_owned(), why)),
                Err(err) => panic!("{name}: {err}"),
            }
        }
        // The two refusals: a page passed through undecoded whose meta content has invalid
        // bytes, and the time Go cannot marshal.
        assert_eq!(
            refused.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["invalid_utf8_raw_absolute", "doc_071"],
            "{refused:?}"
        );
    }

    /// `ResponseFromJSON` and `parseOpenGraphFromOEmbed`.
    #[test]
    fn oembed_matches_go() {
        for case in oracle()["oembed"].as_array().expect("cases") {
            let body = bytes(&case["body"]);
            let label = String::from_utf8_lossy(&body).into_owned();
            let ours = oembed::response_from_json(&body);
            assert_eq!(
                ours.is_err(),
                case["err"].as_bool().expect("err"),
                "{label}"
            );
            if let Ok(r) = ours {
                assert_eq!(r.title, case["title"].as_str().expect("t"), "{label}");
                assert_eq!(r.thumbnail_url, case["thumbnail_url"].as_str().expect("u"));
                assert_eq!(
                    r.thumbnail_width,
                    case["thumbnail_width"].as_i64().expect("w")
                );
                assert_eq!(
                    r.thumbnail_height,
                    case["thumbnail_height"].as_i64().expect("h")
                );
                let og = parse_open_graph_from_oembed("https://www.youtube.com/watch?v=x", &body)
                    .expect("parses");
                assert_eq!(
                    go_json_marshal(&og).expect("marshals"),
                    case["og"].as_str().expect("og"),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn endpoints_match_go() {
        for case in oracle()["endpoints"].as_array().expect("cases") {
            let url = case["url"].as_str().expect("url");
            let ours = oembed::find_endpoint_for_url(url)
                .map(|p| p.get_provider_url(url))
                .unwrap_or_default();
            assert_eq!(ours, case["provider"].as_str().expect("provider"), "{url}");
        }
    }
}
