//! Port of the link-preview half of `channels/app/post_metadata.go`: which link of a message is
//! previewed, which images are measured, and what fetching them finds.
//!
//! This module is the pure half — link selection — and [`crate::post`] drives the pipeline
//! (`getEmbedsAndImages` → `getEmbedForPost` / `getImagesForPost` → `getLinkMetadata`) on top of
//! it. Every function here is pinned against Go's own by `fixtures/behaviour_link_preview.json`
//! (`reference/dump/behaviour_link_preview.go`), which calls Go's `markdown.Inspect`,
//! `idna.Lookup.ToASCII` and `url.Parse` rather than a transcription of them.
//!
//! # `RestrictLinkPreviews` is a substring test on an IDNA-mapped host
//!
//! `isLinkAllowedForPreview` lower-cases the setting, treats `@`, `,` and whitespace as
//! separators, and refuses a link whose host — after `idna.Lookup.ToASCII` — **contains** any
//! entry: `example.com` also refuses `notexample.com`. The parse and the mapping happen *inside*
//! the loop over the entries, so with the setting empty nothing is parsed at all and every link is
//! allowed, while with it set a link whose host the mapping rejects (an underscore, an IPv6
//! literal — `:` is disallowed under the STD3 rules `Lookup` applies) is refused outright.
//!
//! The mapping is reproduced for ASCII hosts, where UTS #46 with STD3 rules reduces to "lower-case
//! the letters; letters, digits, `-` and `.` are the only valid bytes; no label starts or ends with
//! `-`; no `--` at positions 3-4". A host with a non-ASCII byte or an `xn--` label needs the
//! Unicode mapping tables and Punycode, which are not ported: [`idna_lookup_to_ascii`] answers
//! [`Unreproducible`] and the caller forwards. Only reachable with the setting non-empty.

use mm_markdown::{Inline, Node};
use mm_model::channel::{CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP};
use mm_model::link_metadata::truncate_open_graph;
use mm_model::opengraph::OpenGraph;
use mm_model::permalink::PreviewPost;
use mm_model::post::{POST_TYPE_BURN_ON_READ, Post};
use mm_model::post_embed::{PostEmbed, PostEmbedData};
use mm_model::post_metadata::PostImage;
use mm_model::team::Team;
use mm_model::utils::{AppError, go_to_lower};

use crate::App;
use crate::http_guard::GuardedClient;
use crate::link_image::ImageProbe;
use crate::plugin_hooks::HookContext;
use crate::post::{PrepareError, PreparePostForClientOpts};

/// A decision this port cannot make the way Go makes it; the request is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreproducible(pub &'static str);

/// Port of `normalizeDomains` (post_metadata.go:760): "commas and @ signs are optional".
///
/// `strings.Fields` splits on Unicode white space, which is `char::is_whitespace`'s set; the
/// lower-casing is Go's `strings.ToLower` ([`go_to_lower`]).
pub fn normalize_domains(domains: &str) -> Vec<String> {
    let replaced = domains.replace(['@', ','], " ");
    go_to_lower(&replaced)
        .split(char::is_whitespace)
        .filter(|field| !field.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Why `idna.Lookup.ToASCII` refused a host. The text is diagnostic only: Go's caller logs it
/// and refuses the preview, whatever it says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("idna: invalid label {0:?}")]
pub struct IdnaError(pub String);

/// `idna.Lookup.ToASCII` (x/net/idna) for an ASCII host; [`Unreproducible`] for any other.
///
/// `Lookup` is UTS #46 non-transitional with `useSTD3Rules` and `checkHyphens`. On ASCII input
/// the mapping lower-cases `A-Z` and leaves `a-z`, `0-9`, `-` and `.` valid; every other ASCII
/// byte is `disallowed_STD3_valid`, an error. `checkHyphens` refuses a label with `--` at
/// positions 3-4 or a `-` at either end. `verifyDNSLength` is off, so empty labels (`a..b`,
/// `.a`, `a.`) and the empty host pass unchanged. An `xn--` label is Punycode-decoded and
/// re-validated in Go, which needs the Unicode tables: unreproducible, as is any non-ASCII byte.
pub fn idna_lookup_to_ascii(host: &str) -> Result<Result<String, IdnaError>, Unreproducible> {
    if !host.is_ascii() {
        return Err(Unreproducible(
            "a non-ASCII host under RestrictLinkPreviews needs the UTS #46 tables",
        ));
    }
    let mapped = host.to_ascii_lowercase();
    let mut error = None;
    for label in mapped.split('.') {
        if label.starts_with("xn--") {
            return Err(Unreproducible(
                "a Punycode label under RestrictLinkPreviews needs the UTS #46 tables",
            ));
        }
        if error.is_some() || label.is_empty() {
            continue;
        }
        let bytes = label.as_bytes();
        let bad_byte = bytes
            .iter()
            .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-'));
        let bad_hyphens = (bytes.len() >= 4 && bytes[2] == b'-' && bytes[3] == b'-')
            || bytes.first() == Some(&b'-')
            || bytes.last() == Some(&b'-');
        if bad_byte || bad_hyphens {
            error = Some(IdnaError(label.to_owned()));
        }
    }
    Ok(match error {
        Some(error) => Err(error),
        None => Ok(mapped),
    })
}

/// Port of `isLinkAllowedForPreview` (post_metadata.go:735). See the module docs.
pub fn is_link_allowed_for_preview(
    restrict_link_previews: &str,
    link: &str,
) -> Result<bool, Unreproducible> {
    for domain in normalize_domains(restrict_link_previews) {
        let Ok(parsed) = mm_model::go_url::go_parse(link) else {
            // "We disable link preview if link is badly formed to remain on the safe side."
            return Ok(false);
        };
        let Ok(cleaned) = idna_lookup_to_ascii(&String::from_utf8_lossy(&parsed.hostname()))?
        else {
            // "Same applies if compatibility processing fails."
            return Ok(false);
        };
        if cleaned.contains(domain.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Port of `getFirstLink` (post_metadata.go:785): the first **autolink** of `message` that
/// [`is_link_allowed_for_preview`] admits — not a `[text](url)` link and not an image — or `""`.
///
/// Go's `firstLink == "" && isLinkAllowedForPreview(…)` evaluates the setting only until a link
/// is found, so a later link is never parsed; the short circuit is kept so an unreproducible
/// later link does not forward a message whose preview is already decided.
pub fn get_first_link(
    restrict_link_previews: &str,
    message: &str,
) -> Result<String, Unreproducible> {
    let mut first_link = String::new();
    let mut failure = None;
    mm_markdown::inspect(message, |node| {
        if let Some(Node::Inline(Inline::Autolink(autolink))) = node {
            if first_link.is_empty() && failure.is_none() {
                let link = autolink.destination();
                match is_link_allowed_for_preview(restrict_link_previews, &link) {
                    Ok(true) => first_link = link,
                    Ok(false) => {}
                    Err(why) => failure = Some(why),
                }
            }
        }
        true
    });
    match failure {
        Some(why) => Err(why),
        None => Ok(first_link),
    }
}

/// Port of `getImages` (post_metadata.go:799): every inline and reference **image** destination
/// of `message` that [`is_link_allowed_for_preview`] admits, in document order, duplicates kept.
pub fn get_images(
    restrict_link_previews: &str,
    message: &str,
) -> Result<Vec<String>, Unreproducible> {
    let mut images = Vec::new();
    let mut failure = None;
    mm_markdown::inspect(message, |node| {
        let link = match node {
            Some(Node::Inline(Inline::InlineImage(image))) => image.destination(),
            Some(Node::Inline(Inline::ReferenceImage(image))) => image.destination(),
            _ => return true,
        };
        if failure.is_none() {
            match is_link_allowed_for_preview(restrict_link_previews, &link) {
                Ok(true) => images.push(link),
                Ok(false) => {}
                Err(why) => failure = Some(why),
            }
        }
        true
    });
    match failure {
        Some(why) => Err(why),
        None => Ok(images),
    }
}

/// Port of `looksLikeAPermalink` (post_metadata.go:817): after trimming white space and cutting
/// the site URL off the front — **any** URL "has" the empty site URL as a prefix — and one
/// optional `/`, the rest is `team/pl/postid` with the team `[0-9a-z_-]{1,64}` and the id
/// `[a-z0-9]{26}`. Nothing may follow the id, not even a query.
pub fn looks_like_a_permalink(url: &str, site_url: &str) -> bool {
    let Some(path) = url.trim().strip_prefix(site_url) else {
        return false;
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let Some((team, id)) = path.split_once("/pl/") else {
        return false;
    };
    (1..=64).contains(&team.len())
        && team
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Port of `resolveMetadataURL` (post_metadata.go:1063): `requestURL` resolved against the site
/// URL with Go's `url.Parse` and `ResolveReference`, re-serialised by `URL.String()` — so it is
/// also a normalisation (`HTTP://Example.COM/a/../b` comes back as `http://Example.COM/b`).
/// Either parse failing is `""`, which the caller then fetches as it would any other string.
pub fn resolve_metadata_url(request_url: &str, site_url: &str) -> String {
    let Ok(base) = mm_model::go_url::go_parse(site_url) else {
        return String::new();
    };
    match base.parse_with_base(request_url) {
        Ok(resolved) => resolved.to_go_string(),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// The fetch half: `getLinkMetadata` and what it calls
// ---------------------------------------------------------------------------------------------

/// `LinkCacheSize` (platform/link_cache.go:12).
const LINK_CACHE_SIZE: usize = 10_000;
/// `LinkCacheDuration` (platform/link_cache.go:13).
const LINK_CACHE_DURATION: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Port of `linkMetadataCache` (post_metadata.go:33), one entry of the link cache.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CachedLink {
    pub open_graph: Option<OpenGraph>,
    pub post_image: Option<PostImage>,
    pub permalink: Option<PreviewPost>,
    pub url: String,
}

/// Port of `platform.linkCache` — an LRU of [`LINK_CACHE_SIZE`] entries, each expiring
/// [`LINK_CACHE_DURATION`] after it was written, keyed by the hex of
/// `GenerateLinkMetadataHash(url, hour)`.
///
/// **Per process, like Go's — decided, not approximated.** `platform.linkCache` is a package
/// global LRU: never shared between cluster nodes, never invalidated by an edit or a new row, and
/// purged only when the image-proxy settings change. So in Go itself the answer to a read
/// depends on which node served it, and this server holding its own cache is exactly one more
/// node. Sharing Go's would need a channel into the Go process that no Go node has either.
///
/// What the two processes share is the `LinkMetadata` row, which a read (`isNewPost` false)
/// consults after its own cache and before fetching. The cache and the row can disagree in
/// exactly the ways they disagree between two Go nodes, all within the hour an entry lives:
///
/// - **a URL longer than `LinkMetadataMaxURLLength`** (2,048 bytes): the save fails, so only the
///   process that fetched it has the answer, and any other process — this one reading a post Go
///   created, or a second Go node — fetches again. The preview agrees unless the site changed.
/// - **a permalink preview without a `previewed_post` prop** (a post written while
///   `EnablePermalinkPreviews` was off, or an ephemeral confirmation): the cached preview is the
///   referenced post as it was, so an edit of that post within the hour shows here and not on the
///   node that cached it. Every permalink `CreatePost` previews carries the prop, which bypasses
///   both the cache and the row, so a post read back is always previewed fresh on both.
/// - **a failed fetch**: `getEmbedForPost` drops the embed on the error, but the entry cached
///   beside it holds no error, so the fetching process answers `link` on its next read — as does
///   every other process, from the `none` row. Only the create response itself differs.
///
/// Go's LRU stores entries **msgpack-encoded** and decodes them on the way out; this one stores
/// the values. The two agree on every value a fetch produces except where msgpack is lossy,
/// which none of these fields are on the shapes served, and `parity::post_link_reads` compares a
/// Go read from its cache with a read here from the row, and the reverse.
#[derive(Debug, Default)]
pub struct LinkCache {
    entries: std::sync::Mutex<LinkCacheEntries>,
}

#[derive(Debug, Default)]
struct LinkCacheEntries {
    map: std::collections::HashMap<String, (std::time::Instant, u64, CachedLink)>,
    clock: u64,
}

impl LinkCache {
    /// `LinkCache().Get` — a hit refreshes the entry's recency; an expired entry is a miss and is
    /// dropped.
    fn get(&self, key: &str) -> Option<CachedLink> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.clock += 1;
        let clock = entries.clock;
        let expired = match entries.map.get_mut(key) {
            None => return None,
            Some((expires_at, used, value)) => {
                if *expires_at > std::time::Instant::now() {
                    *used = clock;
                    return Some(value.clone());
                }
                true
            }
        };
        if expired {
            entries.map.remove(key);
        }
        None
    }

    /// `LinkCache().SetWithExpiry` — evicting the least recently used entry when full.
    fn set(&self, key: String, value: CachedLink) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.clock += 1;
        let clock = entries.clock;
        if !entries.map.contains_key(&key) && entries.map.len() >= LINK_CACHE_SIZE {
            if let Some(oldest) = entries
                .map
                .iter()
                .min_by_key(|(_, (_, used, _))| *used)
                .map(|(key, _)| key.clone())
            {
                entries.map.remove(&oldest);
            }
        }
        entries.map.insert(
            key,
            (
                std::time::Instant::now() + LINK_CACHE_DURATION,
                clock,
                value,
            ),
        );
    }
}

/// `strconv.FormatInt(model.GenerateLinkMetadataHash(requestURL, timestamp), 16)`.
fn link_cache_key(request_url: &str, timestamp: i64) -> String {
    format!(
        "{:x}",
        mm_model::link_metadata::generate_link_metadata_hash(request_url, timestamp)
    )
}

/// What `getLinkMetadataForURL` and `parseLinkMetadata` answer: `(og, image, err)`.
type FetchedLinkMetadata = (
    Option<OpenGraph>,
    Option<PostImage>,
    Option<LinkMetadataError>,
);

/// What `getLinkMetadata` found: `(og, image, permalink)`, all of them possibly nil.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkMetadataFound {
    pub open_graph: Option<OpenGraph>,
    pub post_image: Option<PostImage>,
    pub permalink: Option<PreviewPost>,
}

/// The `error` `getLinkMetadata` returned. Every caller only logs it — at debug, and not at all
/// for a 404 `AppError` — and drops the embed or the image, so the text is diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMetadataError {
    pub not_found: bool,
    pub message: String,
}

impl LinkMetadataError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            not_found: false,
            message: message.into(),
        }
    }
}

/// `getLinkMetadata`'s two outcomes Go can produce, or [`PrepareError::Unreproducible`] around
/// them for a shape this port forwards.
pub type LinkMetadataOutcome = Result<LinkMetadataFound, LinkMetadataError>;

/// Port of `filterSVGImage` (post_metadata.go:832) — applied on the two **cached** paths of
/// `getLinkMetadata` and not on the fresh one, an asymmetry reproduced as it is.
fn filter_svg_image(image: Option<PostImage>, image_url: &str) -> Option<PostImage> {
    let image = image?;
    if image.format == "svg" || mm_model::link_metadata::is_svg_image_url(image_url) {
        return None;
    }
    Some(image)
}

impl App {
    /// The process's link cache; see [`LinkCache`].
    fn link_cache(&self) -> &LinkCache {
        &self.link_cache
    }

    /// Port of `getLinkMetadata` (post_metadata.go:848).
    ///
    /// The order is Go's and every step is observable:
    ///
    /// 1. the URL is resolved against the site URL — a normalisation, since `URL.String()`
    ///    re-serialises it — and a `data:image/` URL answers nothing at all;
    /// 2. the timestamp is floored to the hour, which with the URL keys both caches;
    /// 3. the per-process cache, bypassed when the post carries a `previewed_post` prop;
    /// 4. for an existing post only, the `LinkMetadata` row;
    /// 5. a permalink is previewed from the database, a YouTube URL goes to oEmbed, and anything
    ///    else is fetched — and only that last kind writes a `LinkMetadata` row, **even when the
    ///    fetch failed** ("we want to save that there is no metadata for this link");
    /// 6. the cache is written whatever happened, except after a failed permalink lookup, which
    ///    returns first.
    #[tracing::instrument(skip(self, ctx), fields(timestamp))]
    pub(crate) async fn get_link_metadata(
        &self,
        ctx: &HookContext,
        request_url: &str,
        timestamp: i64,
        is_new_post: bool,
        previewed_post_prop_val: &str,
    ) -> Result<LinkMetadataOutcome, PrepareError> {
        let config = self.config();
        let site_url = config.site_url.as_deref().unwrap_or("");
        let request_url = resolve_metadata_url(request_url, site_url);

        // "If it's an embedded image, nothing to do."
        if go_to_lower(&request_url).starts_with("data:image/") {
            return Ok(Ok(LinkMetadataFound::default()));
        }

        let timestamp = mm_model::link_metadata::floor_to_nearest_hour(timestamp);
        tracing::Span::current().record("timestamp", timestamp);

        // Check cache
        let key = link_cache_key(&request_url, timestamp);
        let cached = self
            .link_cache()
            .get(&key)
            .filter(|cached| cached.url == request_url);
        if let Some(cached) = cached {
            if previewed_post_prop_val.is_empty() {
                return Ok(Ok(LinkMetadataFound {
                    open_graph: truncate_open_graph(cached.open_graph),
                    post_image: filter_svg_image(cached.post_image, &request_url),
                    permalink: cached
                        .permalink
                        .filter(|_| config.enable_permalink_previews),
                }));
            }
        }

        // "Check the database if this isn't a new post. If it is a new post and the data is
        // cached, it should be in memory."
        if !is_new_post {
            if let Some((open_graph, post_image)) = self
                .get_link_metadata_from_database(&request_url, timestamp)
                .await
            {
                if previewed_post_prop_val.is_empty() {
                    let open_graph = truncate_open_graph(open_graph);
                    let post_image = filter_svg_image(post_image, &request_url);
                    self.cache_link_metadata(
                        &request_url,
                        timestamp,
                        open_graph.clone(),
                        post_image.clone(),
                        None,
                    );
                    return Ok(Ok(LinkMetadataFound {
                        open_graph,
                        post_image,
                        permalink: None,
                    }));
                }
            }
        }

        let (found, error) = if looks_like_a_permalink(&request_url, site_url)
            && config.enable_permalink_previews
        {
            match self
                .get_link_metadata_for_permalink(ctx, &request_url)
                .await?
            {
                Ok(preview) => (
                    LinkMetadataFound {
                        permalink: Some(preview),
                        ..LinkMetadataFound::default()
                    },
                    None,
                ),
                // A failed lookup returns before the cache is written.
                Err(error) => return Ok(Err(error)),
            }
        } else if let Some(provider) = crate::opengraph::oembed::find_endpoint_for_url(&request_url)
        {
            let (open_graph, error) = self
                .get_link_metadata_from_oembed(&request_url, provider)
                .await?;
            (
                LinkMetadataFound {
                    open_graph,
                    ..LinkMetadataFound::default()
                },
                error,
            )
        } else {
            let (open_graph, post_image, error) =
                self.get_link_metadata_for_url(&request_url).await?;
            // "We intentionally don't return early on an error because we want to save that
            // there is no metadata for this link."
            self.save_link_metadata_to_database(
                &request_url,
                timestamp,
                open_graph.as_ref(),
                post_image.as_ref(),
            )
            .await;
            (
                LinkMetadataFound {
                    open_graph,
                    post_image,
                    permalink: None,
                },
                error,
            )
        };

        // "Write back to cache and database, even if there was an error and the results are nil"
        self.cache_link_metadata(
            &request_url,
            timestamp,
            found.open_graph.clone(),
            found.post_image.clone(),
            found.permalink.clone(),
        );

        Ok(match error {
            Some(error) => Err(error),
            None => Ok(found),
        })
    }

    /// Port of `cacheLinkMetadata` (post_metadata.go:1124).
    fn cache_link_metadata(
        &self,
        request_url: &str,
        timestamp: i64,
        open_graph: Option<OpenGraph>,
        post_image: Option<PostImage>,
        permalink: Option<PreviewPost>,
    ) {
        self.link_cache().set(
            link_cache_key(request_url, timestamp),
            CachedLink {
                open_graph,
                post_image,
                permalink,
                url: request_url.to_owned(),
            },
        );
    }

    /// Port of `getLinkMetadataFromDatabase` (post_metadata.go:1086) and the store's
    /// `DeserializeDataToConcreteType`: `None` for a miss or a read error; for a hit, the
    /// OpenGraph document or the image the row's `Type` says it holds, and neither for `none` or
    /// an unknown type. A `Data` that does not decode as its type is nil in Go (an OpenGraph
    /// `json.Unmarshal` error is **ignored**, so it keeps whatever it decoded before failing);
    /// here an OpenGraph that fails to decode is treated as nil, which is the same for every row
    /// either server writes.
    async fn get_link_metadata_from_database(
        &self,
        request_url: &str,
        timestamp: i64,
    ) -> Option<(Option<OpenGraph>, Option<PostImage>)> {
        use mm_store::LinkMetadataStore as _;
        let row = self
            .store()
            .link_metadata()
            .get(request_url, timestamp)
            .await
            .ok()?;
        let data = row.data.unwrap_or(serde_json::Value::Null);
        Some(match row.link_type.as_str() {
            mm_model::link_metadata::LINK_METADATA_TYPE_OPENGRAPH => {
                (serde_json::from_value(data).ok(), None)
            }
            mm_model::link_metadata::LINK_METADATA_TYPE_IMAGE => {
                // `json.Unmarshal` of an image that does not decode is an error the store
                // returns, which the app reads as a miss.
                match serde_json::from_value::<Option<PostImage>>(data) {
                    Ok(image) => (None, image),
                    Err(_) => return None,
                }
            }
            _ => (None, None),
        })
    }

    /// Port of `saveLinkMetadataToDatabase` (post_metadata.go:1104). The type follows the value
    /// — OpenGraph first — and a failed save (the URL over 2,048 bytes, an hour of zero) is
    /// logged and nothing more.
    async fn save_link_metadata_to_database(
        &self,
        request_url: &str,
        timestamp: i64,
        open_graph: Option<&OpenGraph>,
        post_image: Option<&PostImage>,
    ) {
        use mm_store::LinkMetadataStore as _;
        let (link_type, data) = if let Some(open_graph) = open_graph {
            (
                mm_model::link_metadata::LINK_METADATA_TYPE_OPENGRAPH,
                serde_json::to_value(open_graph).ok(),
            )
        } else if let Some(post_image) = post_image {
            (
                mm_model::link_metadata::LINK_METADATA_TYPE_IMAGE,
                serde_json::to_value(post_image).ok(),
            )
        } else {
            (mm_model::link_metadata::LINK_METADATA_TYPE_NONE, None)
        };
        let mut metadata = mm_model::link_metadata::LinkMetadata {
            url: request_url.to_owned(),
            timestamp,
            link_type: link_type.to_owned(),
            data,
            ..Default::default()
        };
        if let Err(err) = self.store().link_metadata().save(&mut metadata).await {
            tracing::warn!(request_url, error = %err, "Failed to write link metadata");
        }
    }

    /// Port of `getLinkMetadataForPermalink` (post_metadata.go:902).
    ///
    /// Each lookup's `AppError` is the error `getLinkMetadata` returns — a 404 is not even
    /// logged by the caller — and a burn-on-read post is the 403
    /// `api.post.get_link_metadata_for_permalink.burn_on_read.app_error`. The referenced post is
    /// prepared with `IncludePriority` unless its own first link is a permalink
    /// (`containsPermalink`), in which case the preview carries the bare row; preparing it goes
    /// through the **read** path, so the referenced post's own link is answered from its
    /// `LinkMetadata` row, or fetched when it has none. `populatePostListTranslations` needs the
    /// autotranslation service and is inert without it.
    async fn get_link_metadata_for_permalink(
        &self,
        ctx: &HookContext,
        request_url: &str,
    ) -> Result<Result<PreviewPost, LinkMetadataError>, PrepareError> {
        let from_app_error = |err: Box<AppError>| LinkMetadataError {
            not_found: err.status_code == 404,
            message: err.to_string(),
        };
        let referenced_post_id = request_url
            .get(request_url.len().saturating_sub(26)..)
            .unwrap_or(request_url);
        // `a.GetSinglePost(rctx, ...)`: the previewed post is told to the plugins too, which is
        // why the context reaches this far down the metadata pipeline.
        let referenced = match self.get_single_post(ctx, referenced_post_id, false).await {
            Ok(post) => post,
            Err(err) => return Ok(Err(from_app_error(err))),
        };
        if referenced.post_type == POST_TYPE_BURN_ON_READ {
            return Ok(Err(from_app_error(AppError::boxed(
                "getLinkMetadataForPermalink",
                "api.post.get_link_metadata_for_permalink.burn_on_read.app_error",
                None,
                String::new(),
                403,
            ))));
        }
        let channel = match self.get_channel(&referenced.channel_id).await {
            Ok(channel) => channel,
            Err(err) => return Ok(Err(from_app_error(err))),
        };
        let team = if channel.channel_type == CHANNEL_TYPE_DIRECT
            || channel.channel_type == CHANNEL_TYPE_GROUP
        {
            Team::default()
        } else {
            match self.get_team(&channel.team_id).await {
                Ok(team) => team,
                Err(err) => return Ok(Err(from_app_error(err))),
            }
        };

        let previewed = if self.contains_permalink(&referenced)? {
            referenced
        } else {
            Box::pin(self.prepare_post_for_client_with_embeds_and_images(
                ctx,
                &referenced,
                PreparePostForClientOpts {
                    include_priority: true,
                    ..PreparePostForClientOpts::default()
                },
            ))
            .await?
        };
        // `NewPreviewPost` returns nil only for a nil post.
        mm_model::permalink::new_preview_post(Some(&previewed), &team, &channel)
            .map(Ok)
            .ok_or(PrepareError::Unreproducible("a nil preview post"))
    }

    /// Port of `containsPermalink` (post_metadata.go:824): the post's **first** link is a
    /// permalink of this site.
    pub(crate) fn contains_permalink(&self, post: &Post) -> Result<bool, PrepareError> {
        let config = self.config();
        let link = get_first_link(&config.restrict_link_previews, &post.message)
            .map_err(|Unreproducible(why)| PrepareError::Unreproducible(why))?;
        if link.is_empty() {
            return Ok(false);
        }
        Ok(looks_like_a_permalink(
            &link,
            config.site_url.as_deref().unwrap_or(""),
        ))
    }

    /// Port of `getLinkMetadataFromOEmbed` (post_metadata.go:957): a `GET` of the provider's
    /// endpoint through the outbound guard, `Accept: application/json`, the server locale, the
    /// link-metadata timeout. A transport failure is Go's error; the answer is parsed whatever
    /// its status. Nothing is saved to `LinkMetadata` on this path.
    async fn get_link_metadata_from_oembed(
        &self,
        request_url: &str,
        provider: &crate::opengraph::oembed::ProviderEndpoint,
    ) -> Result<(Option<OpenGraph>, Option<LinkMetadataError>), PrepareError> {
        let config = self.config();
        let client = GuardedClient::new(
            &config.allowed_untrusted_internal_connections,
            config.enable_insecure_outgoing_connections,
        );
        let response = client
            .get_with_headers(
                &provider.get_provider_url(request_url),
                link_metadata_timeout(&config),
                &[
                    ("Accept", "application/json"),
                    ("Accept-Language", &config.default_server_locale),
                ],
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(error = %err, "error fetching oEmbed data");
                return Ok((
                    None,
                    Some(LinkMetadataError::other(format!(
                        "getLinkMetadataFromOEmbed: Unable to get oEmbed data: {err}"
                    ))),
                ));
            }
        };
        let body = read_body(response, crate::opengraph::MAX_OPEN_GRAPH_RESPONSE_SIZE).await?;
        match crate::opengraph::parse_open_graph_from_oembed(request_url, &body) {
            Ok(open_graph) => Ok((Some(open_graph), None)),
            Err(crate::opengraph::OpenGraphError::Unreproducible(why)) => {
                Err(PrepareError::Unreproducible(why))
            }
            Err(crate::opengraph::OpenGraphError::OEmbed(err)) => Ok((
                None,
                Some(LinkMetadataError::other(format!(
                    "parseOpenGraphFromOEmbed: Unable to parse oEmbed response: {err}"
                ))),
            )),
        }
    }

    /// Port of `getLinkMetadataForURL` (post_metadata.go:986): one `GET` of the link through the
    /// outbound guard (`MakeClient(false)`), asking for `image/*` and then `text/html;q=0.8` as
    /// two `Accept` lines, in the server's locale, within `LinkMetadataTimeoutMilliseconds`.
    ///
    /// The status is never looked at: a 404 page with OpenGraph tags previews like any other.
    /// A transport failure is the error, and nothing is parsed. The OpenGraph is truncated
    /// (`TruncateOpenGraph`) on the way out.
    ///
    /// `{SiteURL}/api/v4/image` bypasses the request and reads through the image proxy, which is
    /// not ported: forwarded.
    async fn get_link_metadata_for_url(
        &self,
        request_url: &str,
    ) -> Result<FetchedLinkMetadata, PrepareError> {
        let config = self.config();
        // `http.NewRequest` fails only on a URL `url.Parse` refuses.
        let parsed = match mm_model::go_url::go_parse(request_url) {
            Ok(parsed) => parsed,
            Err(err) => return Ok((None, None, Some(LinkMetadataError::other(err.to_string())))),
        };
        let site_url = config.site_url.as_deref().unwrap_or("");
        if format!(
            "{}://{}",
            parsed.scheme,
            String::from_utf8_lossy(&parsed.host)
        ) == site_url
            && parsed.path == b"/api/v4/image"
        {
            return Err(PrepareError::Unreproducible(
                "/api/v4/image is read through the image proxy",
            ));
        }
        let client = GuardedClient::new(
            &config.allowed_untrusted_internal_connections,
            config.enable_insecure_outgoing_connections,
        );
        let response = client
            .get_with_headers(
                request_url,
                link_metadata_timeout(&config),
                &[
                    ("Accept", "image/*"),
                    ("Accept", "text/html;q=0.8"),
                    ("Accept-Language", &config.default_server_locale),
                ],
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(error = %err, "error fetching OG image data");
                return Ok((None, None, Some(LinkMetadataError::other(err.to_string()))));
            }
        };
        // `res.Header.Get("Content-Type")` — the first value, as sent.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .unwrap_or_default();
        let body = read_body(response, crate::opengraph::MAX_OPEN_GRAPH_RESPONSE_SIZE).await?;
        let (open_graph, post_image, error) =
            parse_link_metadata(request_url, &body, &content_type)?;
        // "remove unwanted length of texts"
        Ok((truncate_open_graph(open_graph), post_image, error))
    }
}

impl App {
    /// Port of `getEmbedsAndImages` (post_metadata.go:277), for a post being created
    /// (`is_new_post`, run by `CreatePost` on the pre-save post and by `SendEphemeralPost`) and
    /// for every read, edit and preview of an existing post (`PreparePostForClientWithEmbedsAndImages`
    /// with `IsNewPost` false).
    ///
    /// Embeds come from the message's first autolink only — "not from attachments or blocks" —
    /// and a failed lookup drops the embed without failing anything. `Images` is always
    /// assigned, and an empty map is dropped by `omitempty`. The image proxy rewrites both
    /// (`ImageProxyAdder`); `PreparePostForClient` refuses it before this runs, and
    /// [`App::create_post`](crate::App) before the write.
    ///
    /// `is_new_post` decides one thing: whether [`App::get_link_metadata`] consults the
    /// `LinkMetadata` row before fetching. On a read it does, so a post whose preview was fetched
    /// when it was created — by either server — is answered from the row and not fetched again.
    pub(crate) async fn get_embeds_and_images(
        &self,
        ctx: &HookContext,
        post: &mut Post,
        is_new_post: bool,
    ) -> Result<(), PrepareError> {
        let config = self.config();
        let first_link = get_first_link(&config.restrict_link_previews, &post.message)
            .map_err(|Unreproducible(why)| PrepareError::Unreproducible(why))?;
        // `if post.Metadata == nil { post.Metadata = &model.PostMetadata{} }`.
        post.metadata.get_or_insert_with(Default::default);

        if post.has_unsafe_links()
            && !looks_like_a_permalink(&first_link, config.site_url.as_deref().unwrap_or(""))
        {
            return Ok(());
        }

        match self
            .get_embed_for_post(ctx, post, &first_link, is_new_post)
            .await?
        {
            Ok(Some(embed)) => {
                if let Some(metadata) = post.metadata.as_mut() {
                    metadata.embeds.push(embed);
                }
            }
            Ok(None) => {}
            Err(error) => {
                // "Ignore NotFound errors."
                if !error.not_found {
                    tracing::debug!(post_id = %post.id, error = %error.message, "Failed to get embedded content for a post");
                }
            }
        }
        let images = self.get_images_for_post(ctx, post, is_new_post).await?;
        if let Some(metadata) = post.metadata.as_mut() {
            metadata.images = images;
        }
        Ok(())
    }

    /// Port of `getEmbedForPost` (post_metadata.go:545).
    ///
    /// An `attachments` prop is a bare `message_attachment` embed and a `boards` prop a `boards`
    /// embed carrying the prop, both before the link is looked at. Link previews off
    /// (`EnableLinkPreviews`) still preview a permalink. Of what `getLinkMetadata` found,
    /// OpenGraph wins over an image, which wins over a permalink; an image that is an SVG by
    /// format **or by URL** is demoted to a plain `link` embed (MM-67372), and a link with no
    /// metadata at all is a `link` embed too.
    async fn get_embed_for_post(
        &self,
        ctx: &HookContext,
        post: &Post,
        first_link: &str,
        is_new_post: bool,
    ) -> Result<Result<Option<PostEmbed>, LinkMetadataError>, PrepareError> {
        if post
            .get_prop(mm_model::post::POST_PROPS_ATTACHMENTS)
            .is_some()
        {
            return Ok(Ok(Some(PostEmbed {
                type_: mm_model::post_embed::POST_EMBED_MESSAGE_ATTACHMENT.to_owned(),
                ..PostEmbed::default()
            })));
        }
        if let Some(boards) = post.get_prop("boards") {
            return Ok(Ok(Some(PostEmbed {
                type_: mm_model::post_embed::POST_EMBED_BOARDS.to_owned(),
                data: Some(PostEmbedData::Json(boards.clone())),
                ..PostEmbed::default()
            })));
        }
        if first_link.is_empty() {
            return Ok(Ok(None));
        }

        let config = self.config();
        // "Permalink previews are not toggled via the ServiceSettings.EnableLinkPreviews config
        // setting."
        if !config.enable_link_previews
            && !looks_like_a_permalink(first_link, config.site_url.as_deref().unwrap_or(""))
        {
            return Ok(Ok(None));
        }

        let found = match self
            .get_link_metadata(
                ctx,
                first_link,
                post.create_at,
                is_new_post,
                post.get_previewed_post_prop(),
            )
            .await?
        {
            Ok(found) => found,
            Err(error) => return Ok(Err(error)),
        };
        let permalink = found.permalink.filter(|_| config.enable_permalink_previews);

        let encode = |value: Result<PostEmbedData, serde_json::Error>| {
            value.map_err(|err| {
                PrepareError::App(AppError::boxed(
                    "getEmbedForPost",
                    "api.marshal_error",
                    None,
                    err.to_string(),
                    500,
                ))
            })
        };

        if let Some(open_graph) = found.open_graph {
            return Ok(Ok(Some(PostEmbed {
                type_: mm_model::post_embed::POST_EMBED_OPENGRAPH.to_owned(),
                url: first_link.to_owned(),
                data: Some(encode(PostEmbedData::encode(&open_graph))?),
            })));
        }
        if let Some(image) = found.post_image {
            // See MM-67372
            let type_ =
                if image.format == "svg" || mm_model::link_metadata::is_svg_image_url(first_link) {
                    mm_model::post_embed::POST_EMBED_LINK
                } else {
                    // "Note that we're not passing the image info here since it'll be part of the
                    // PostMetadata.Images field"
                    mm_model::post_embed::POST_EMBED_IMAGE
                };
            return Ok(Ok(Some(PostEmbed {
                type_: type_.to_owned(),
                url: first_link.to_owned(),
                data: None,
            })));
        }
        if let Some(preview) = permalink {
            return Ok(Ok(Some(PostEmbed {
                type_: mm_model::post_embed::POST_EMBED_PERMALINK.to_owned(),
                url: String::new(),
                data: Some(encode(PostEmbedData::encode(&preview))?),
            })));
        }
        Ok(Ok(Some(PostEmbed {
            type_: mm_model::post_embed::POST_EMBED_LINK.to_owned(),
            url: first_link.to_owned(),
            data: None,
        })))
    }

    /// Port of `getImagesForPost` (post_metadata.go:604).
    ///
    /// The candidates are the markdown images of every string `AllStrings` yields, the images
    /// of interactive blocks, the URL of an `image` embed, and each OpenGraph image (its secure
    /// URL when it has one). More than one is **sorted** and deduplicated
    /// (`RemoveDuplicateStrings`), which is also the order they are fetched in. A candidate
    /// that resolves to a permalink is skipped — "prevent infinite loop if a OG image URL is the
    /// same post's permalink" — and one whose lookup fails or finds no image is left out.
    async fn get_images_for_post(
        &self,
        ctx: &HookContext,
        post: &Post,
        is_new_post: bool,
    ) -> Result<std::collections::BTreeMap<String, PostImage>, PrepareError> {
        let mut post_images = std::collections::BTreeMap::new();
        if post.has_unsafe_links() {
            return Ok(post_images);
        }
        let config = self.config();
        let site_url = config.site_url.as_deref().unwrap_or("");

        let mut image_urls = Vec::new();
        for string in post.all_strings(mm_model::post::AllStringsOptions {
            omit_interactive_blocks: !config.feature_flags.mm_blocks_enabled,
        }) {
            image_urls.extend(
                get_images(&config.restrict_link_previews, &string)
                    .map_err(|Unreproducible(why)| PrepareError::Unreproducible(why))?,
            );
        }
        image_urls
            .extend(post.interactive_blocks_image_urls(config.feature_flags.mm_blocks_enabled));

        for embed in post.metadata.iter().flat_map(|metadata| &metadata.embeds) {
            match embed.type_.as_str() {
                mm_model::post_embed::POST_EMBED_IMAGE => image_urls.push(embed.url.clone()),
                mm_model::post_embed::POST_EMBED_OPENGRAPH => {
                    let Some(open_graph) = embed
                        .data
                        .as_ref()
                        .and_then(PostEmbedData::decode_typed::<OpenGraph>)
                    else {
                        tracing::warn!(post_id = %post.id, "Could not read the image data: the data could not be casted to OpenGraph");
                        continue;
                    };
                    for image in open_graph.images.into_iter().flatten() {
                        let image_url = if !image.secure_url.is_empty() {
                            image.secure_url
                        } else {
                            image.url
                        };
                        if !image_url.is_empty() {
                            image_urls.push(image_url);
                        }
                    }
                }
                _ => {}
            }
        }

        // "Removing duplicates isn't strictly required since postImages is a map, but it feels
        // safer to do it beforehand"
        if image_urls.len() > 1 {
            mm_model::utils::remove_duplicate_strings(&mut image_urls);
        }

        for image_url in image_urls {
            // "prevent infinite loop if a OG image URL is the same post's permalink"
            let resolved = resolve_metadata_url(&image_url, site_url);
            if looks_like_a_permalink(&resolved, site_url) {
                continue;
            }
            match self
                .get_link_metadata(
                    ctx,
                    &image_url,
                    post.create_at,
                    is_new_post,
                    post.get_previewed_post_prop(),
                )
                .await?
            {
                Ok(found) => {
                    if let Some(image) = found.post_image {
                        post_images.insert(image_url, image);
                    }
                }
                Err(error) => {
                    if !error.not_found {
                        tracing::debug!(post_id = %post.id, image_url, error = %error.message, "Failed to get dimensions of an image in a post");
                    }
                }
            }
        }
        Ok(post_images)
    }
}

/// `time.Duration(*LinkMetadataTimeoutMilliseconds) * time.Millisecond`. `Config.IsValid`
/// refuses a value `<= 0`, so the clamp is never what decides.
fn link_metadata_timeout(config: &crate::config::Config) -> std::time::Duration {
    std::time::Duration::from_millis(
        u64::try_from(config.link_metadata_timeout_milliseconds).unwrap_or(1),
    )
}

/// The response body, at most `limit` bytes of it — Go wraps every reader it parses in an
/// `io.LimitReader` of the same size. A body that fails part-way is forwarded: Go's parsers see
/// the bytes that did arrive and a read error, and which of them decides is parser-specific.
async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, PrepareError> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = limit.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if body.len() >= limit {
                    return Ok(body);
                }
            }
            Ok(None) => return Ok(body),
            Err(err) => {
                tracing::debug!(error = %err, "a link-preview body failed part-way");
                return Err(PrepareError::Unreproducible(
                    "a link-preview response failed part-way through its body",
                ));
            }
        }
    }
}

/// Port of `parseLinkMetadata` (post_metadata.go:1143) over a body already read.
///
/// A missing `Content-Type` is sniffed from the first 512 bytes (`peekContentType`). Then, by
/// **case-sensitive** prefix: `image/svg+xml` is dropped (MM-67372), any other `image` is
/// measured, `text/html` is parsed for OpenGraph and kept only when it has a title, a type or a
/// URL ("the OpenGraph library and Go HTML library don't error for malformed input"), and
/// anything else has no metadata.
fn parse_link_metadata(
    request_url: &str,
    body: &[u8],
    content_type: &str,
) -> Result<FetchedLinkMetadata, PrepareError> {
    let content_type = if content_type.is_empty() {
        crate::link_image::peek_content_type(body)
    } else {
        content_type
    };
    if content_type.starts_with("image/svg+xml") {
        return Ok((None, None, None));
    }
    if content_type.starts_with("image") {
        let limited = &body[..body.len().min(crate::link_image::MAX_METADATA_IMAGE_SIZE)];
        return match crate::link_image::parse_images(limited) {
            ImageProbe::Image(image) => Ok((None, Some(image), None)),
            ImageProbe::Nil => Ok((None, None, None)),
            ImageProbe::Error(err) => Ok((None, None, Some(LinkMetadataError::other(err)))),
            ImageProbe::Unreproducible(why) => Err(PrepareError::Unreproducible(why)),
        };
    }
    if content_type.starts_with("text/html") {
        let open_graph =
            crate::opengraph::parse_open_graph_metadata(request_url, body, content_type).map_err(
                |err| match err {
                    crate::opengraph::OpenGraphError::Unreproducible(why) => {
                        PrepareError::Unreproducible(why)
                    }
                    crate::opengraph::OpenGraphError::OEmbed(_) => {
                        PrepareError::Unreproducible("an oEmbed error from an HTML parse")
                    }
                },
            )?;
        if !open_graph.title.is_empty()
            || !open_graph.type_.is_empty()
            || !open_graph.url.is_empty()
        {
            return Ok((Some(open_graph), None, None));
        }
        return Ok((None, None, None));
    }
    // "Not an image or web page with OpenGraph information"
    Ok((None, None, None))
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_link_preview.json"
        ))
        .expect("generated by reference/dump")
    }

    /// A JSON array of strings; Go's nil slice (`null`) is the empty list.
    fn strings(value: &serde_json::Value) -> Vec<String> {
        if value.is_null() {
            return Vec::new();
        }
        value
            .as_array()
            .expect("an array")
            .iter()
            .map(|s| s.as_str().expect("a string").to_owned())
            .collect()
    }

    #[test]
    fn normalize_domains_matches_go() {
        let oracle = oracle();
        for case in oracle["normalize_domains"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let want = strings(&case["domains"]);
            assert_eq!(normalize_domains(input), want, "{input:?}");
        }
    }

    /// Every ASCII host without an `xn--` label is answered exactly; the rest are refused, and
    /// the test counts them so a corpus change cannot quietly turn every case into a refusal.
    #[test]
    fn idna_lookup_to_ascii_matches_go_on_ascii_hosts() {
        let oracle = oracle();
        let mut unreproducible = 0;
        for case in oracle["idna_lookup_to_ascii"].as_array().unwrap() {
            let host = case["host"].as_str().unwrap();
            match idna_lookup_to_ascii(host) {
                Err(_) => unreproducible += 1,
                Ok(Ok(ascii)) => {
                    assert_eq!(case["error"], false, "{host:?}");
                    assert_eq!(case["ascii"].as_str().unwrap(), ascii, "{host:?}");
                }
                Ok(Err(_)) => assert_eq!(case["error"], true, "{host:?}"),
            }
        }
        // münchen.de, ÉCOLE.fr, ß.de, xn--nxasmq6b.com, xn--zz.com, xn--.
        assert_eq!(unreproducible, 6);
    }

    #[test]
    fn is_link_allowed_for_preview_matches_go() {
        let oracle = oracle();
        let mut answered = 0;
        for case in oracle["is_link_allowed_for_preview"].as_array().unwrap() {
            let restrict = case["restrict"].as_str().unwrap();
            let link = case["link"].as_str().unwrap();
            if let Ok(allowed) = is_link_allowed_for_preview(restrict, link) {
                answered += 1;
                assert_eq!(
                    case["allowed"].as_bool().unwrap(),
                    allowed,
                    "{restrict:?} {link:?}"
                );
            } else {
                assert!(!restrict.is_empty(), "an empty setting parses nothing");
            }
        }
        assert!(answered >= 110, "only {answered} cases answered");
    }

    #[test]
    fn get_first_link_matches_go() {
        let oracle = oracle();
        let mut answered = 0;
        for case in oracle["first_link"].as_array().unwrap() {
            let restrict = case["restrict"].as_str().unwrap();
            let message = case["message"].as_str().unwrap();
            if let Ok(link) = get_first_link(restrict, message) {
                answered += 1;
                assert_eq!(
                    case["first_link"].as_str().unwrap(),
                    link,
                    "{restrict:?} {message:?}"
                );
            }
        }
        assert!(answered >= 160, "only {answered} cases answered");
    }

    #[test]
    fn get_images_matches_go() {
        let oracle = oracle();
        for case in oracle["images"].as_array().unwrap() {
            let restrict = case["restrict"].as_str().unwrap();
            let message = case["message"].as_str().unwrap();
            let Ok(mut images) = get_images(restrict, message) else {
                continue;
            };
            assert_eq!(strings(&case["images"]), images, "{restrict:?} {message:?}");
            // `model.RemoveDuplicateStrings` — sorted, then deduplicated.
            mm_model::utils::remove_duplicate_strings(&mut images);
            assert_eq!(strings(&case["deduplicated"]), images, "{message:?}");
        }
    }

    #[test]
    fn looks_like_a_permalink_matches_go() {
        let oracle = oracle();
        for case in oracle["looks_like_a_permalink"].as_array().unwrap() {
            let site_url = case["site_url"].as_str().unwrap();
            let url = case["url"].as_str().unwrap();
            assert_eq!(
                case["matches"].as_bool().unwrap(),
                looks_like_a_permalink(url, site_url),
                "{site_url:?} {url:?}"
            );
        }
    }

    #[test]
    fn resolve_metadata_url_matches_go() {
        let oracle = oracle();
        for case in oracle["resolve_metadata_url"].as_array().unwrap() {
            let site_url = case["site_url"].as_str().unwrap();
            let url = case["url"].as_str().unwrap();
            assert_eq!(
                case["resolved"].as_str().unwrap(),
                resolve_metadata_url(url, site_url),
                "{site_url:?} {url:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(format: &str) -> PostImage {
        PostImage {
            width: 3,
            height: 2,
            format: format.to_owned(),
            frame_count: 0,
        }
    }

    #[test]
    fn the_cache_misses_on_a_different_key_and_answers_the_same_one() {
        let cache = LinkCache::default();
        cache.set(
            "k".to_owned(),
            CachedLink {
                post_image: Some(image("png")),
                url: "u".to_owned(),
                ..CachedLink::default()
            },
        );
        assert!(cache.get("other").is_none());
        assert_eq!(
            cache.get("k").and_then(|c| c.post_image),
            Some(image("png"))
        );
    }

    #[test]
    fn a_full_cache_evicts_the_least_recently_used_entry() {
        let cache = LinkCache::default();
        for i in 0..LINK_CACHE_SIZE {
            cache.set(format!("k{i}"), CachedLink::default());
        }
        // Touch the oldest, so the second-oldest is the least recently used.
        assert!(cache.get("k0").is_some());
        cache.set("new".to_owned(), CachedLink::default());
        assert!(cache.get("k0").is_some(), "a read refreshes recency");
        assert!(cache.get("k1").is_none(), "the least recently used goes");
        assert!(cache.get("new").is_some());
        // Rewriting an existing key evicts nothing.
        cache.set("k2".to_owned(), CachedLink::default());
        assert!(cache.get("k3").is_some());
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        let cache = LinkCache::default();
        cache.set("k".to_owned(), CachedLink::default());
        if let Some(entry) = cache.entries.lock().unwrap().map.get_mut("k") {
            entry.0 = std::time::Instant::now() - std::time::Duration::from_secs(1);
        }
        assert!(cache.get("k").is_none());
        assert!(
            cache.entries.lock().unwrap().map.is_empty(),
            "and it is dropped"
        );
    }

    /// `strconv.FormatInt(hash, 16)` — lower-case hex of the non-negative hash.
    #[test]
    fn the_cache_key_is_the_hash_in_hex() {
        let hash = mm_model::link_metadata::generate_link_metadata_hash("http://x/", 3_600_000);
        assert_eq!(link_cache_key("http://x/", 3_600_000), format!("{hash:x}"));
    }

    #[test]
    fn filter_svg_image_drops_by_format_and_by_url() {
        assert_eq!(filter_svg_image(None, "http://x/a.png"), None);
        assert_eq!(filter_svg_image(Some(image("svg")), "http://x/a.png"), None);
        assert_eq!(filter_svg_image(Some(image("png")), "http://x/a.SVG"), None);
        assert_eq!(
            filter_svg_image(Some(image("png")), "http://x/a.png?x=.svg"),
            Some(image("png"))
        );
    }

    fn png_3x2() -> Vec<u8> {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_link_image.json"))
                .unwrap();
        let case = fixture["images"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["kind"] == "image" && c["format"] == "png")
            .unwrap();
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            case["input"].as_str().unwrap(),
        )
        .unwrap()
    }

    /// `parseLinkMetadata`'s content-type switch, case-sensitive prefixes and all.
    #[test]
    fn parse_link_metadata_switches_on_the_content_type() {
        let url = "http://x/page";
        let png = png_3x2();
        let html = br#"<meta property="og:title" content="T">"#;
        let untitled = br#"<meta property="og:description" content="D">"#;

        let (og, image, error) =
            parse_link_metadata(url, html, "text/html; charset=utf-8").unwrap();
        assert_eq!(og.map(|og| og.title), Some("T".to_owned()));
        assert!(image.is_none() && error.is_none());

        // No title, type or URL: the malformed-input guard.
        let (og, _, _) = parse_link_metadata(url, untitled, "text/html").unwrap();
        assert!(og.is_none());

        let (og, image, _) = parse_link_metadata(url, &png, "image/png").unwrap();
        assert!(og.is_none());
        assert_eq!(image.map(|i| i.format), Some("png".to_owned()));

        // Sniffed when the header is missing.
        let (_, image, _) = parse_link_metadata(url, &png, "").unwrap();
        assert!(image.is_some());

        // `image/svg+xml` is dropped before the generic `image` arm; the prefix test is
        // case-sensitive, so `Image/png` is neither an image nor a page.
        assert_eq!(
            parse_link_metadata(url, &png, "image/svg+xml").unwrap(),
            (None, None, None)
        );
        assert_eq!(
            parse_link_metadata(url, &png, "Image/png").unwrap(),
            (None, None, None)
        );
        assert_eq!(
            parse_link_metadata(url, html, "text/plain").unwrap(),
            (None, None, None)
        );

        // An image that does not decode is Go's error, not a refusal.
        let (_, image, error) = parse_link_metadata(url, b"not an image", "image/png").unwrap();
        assert!(image.is_none());
        assert!(error.is_some());
    }

    #[test]
    fn the_link_metadata_timeout_is_milliseconds() {
        let config = crate::config::Config {
            link_metadata_timeout_milliseconds: 1500,
            ..crate::config::Config::default()
        };
        assert_eq!(
            link_metadata_timeout(&config),
            std::time::Duration::from_millis(1500)
        );
    }
}
