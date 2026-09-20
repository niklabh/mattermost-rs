//! The plugin Marketplace: the client (platform/services/marketplace/client.go), the merged list
//! `GET /plugins/marketplace` answers with (app/plugin.go:536-810), a marketplace install
//! (app/plugin_install.go:258) and `downloadFromURL` (app/download.go).
//!
//! # What the list is
//!
//! A map by plugin id, filled in three passes: the Marketplace's own list (when
//! `EnableRemoteMarketplace` is on and the caller did not ask for local plugins only); the
//! prepackaged plugins, which replace a Marketplace entry only when strictly newer; and the
//! plugins installed here, which set `installed_version` on an entry that exists and add one —
//! with the `Local` label while the remote Marketplace is on — that does not. Then the text
//! filter, then a stable sort by lower-cased name.
//!
//! Go ranges a map before the stable sort, so two plugins whose names are equal ignoring case come
//! out in a random order on Go; here they come out by id. Nothing else about the order differs.
//!
//! An empty result is Go's nil slice, which `json.Marshal` writes as `null`, not `[]`.
//!
//! # Prepackaged plugins
//!
//! The list and the install consult the environment's prepackaged plugins, which
//! `crate::plugin_prepackaged` reads from `prepackaged_plugins/` at start-up. The transitionally
//! prepackaged ones are not among them, so neither route offers those.

use std::collections::BTreeMap;
use std::time::Duration;

use mm_model::manifest::{Manifest, StrictVersion};
use mm_model::marketplace_plugin::{
    BaseMarketplacePlugin, InstallMarketplacePluginRequest, MarketplaceLabel, MarketplacePlugin,
    MarketplacePluginFilter,
};
use mm_model::utils::AppError;

use crate::App;
use crate::config::Config;
use crate::http_guard::{CONNECT_TIMEOUT, GuardError, GuardedClient, REQUEST_TIMEOUT};

/// `HTTPRequestTimeout` (download.go:21): an hour, for large bundles on slow links.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

/// `shortBackoffTimeouts` (utils/backoff.go:10). `CustomProgressiveRetry` sleeps after **every**
/// failed attempt, the last one included.
const SHORT_BACKOFF: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(400),
];

/// Why the Marketplace did not answer with a list. Go wraps each into the `AppError` its caller
/// builds, so only the message reaches the logs.
#[derive(Debug, thiserror::Error)]
pub enum MarketplaceClientError {
    #[error("failed to parse marketplace address: {0}")]
    Address(String),
    #[error("{0}")]
    Request(String),
    #[error("failed with status code {0}")]
    Status(u16),
    #[error("{0}")]
    Decode(#[from] serde_json::Error),
    #[error("no pluginID provided")]
    NoPluginId,
    #[error("missing pluginID")]
    MissingPluginId,
    #[error("missing pluginVersion")]
    MissingVersion,
    #[error("plugin not found")]
    NotFound,
    #[error("unexpectedly more then one plugin was returned from the marketplace")]
    MoreThanOne,
}

/// Why `downloadFromURL` failed.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("invalid url {0}")]
    InvalidUrl(String),
    #[error("failed to parse url {0}")]
    Parse(String),
    #[error("insecure url not allowed {0}")]
    Insecure(String),
    #[error("download failed after multiple retries.: failed to fetch from {0}")]
    Fetch(String),
    #[error("{0}")]
    Body(String),
}

/// The two clients `httpservice.MakeClient` builds.
enum Http {
    /// `MakeClient(true)`: no address guard.
    Trusted(reqwest::Client),
    /// `MakeClient(false)`: the `AllowedUntrustedInternalConnections` guard on every dial.
    Guarded(GuardedClient),
}

/// A `MakeClient(true)` client: redirects followed (at most ten, as Go's client), the connect and
/// whole-request timeouts, and `EnableInsecureOutgoingConnections` for TLS.
fn trusted_client(config: &Config, timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .danger_accept_invalid_certs(config.enable_insecure_outgoing_connections)
        .build()
        .map_err(|e| e.to_string())
}

/// `url.URL.Hostname`: the host without its port, and without the brackets of an IPv6 literal.
fn hostname(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(h, _)| h);
    }
    host.rsplit_once(':').map_or(host, |(h, _)| h)
}

/// Port of `marketplace.Client` (platform/services/marketplace/client.go:20).
pub struct MarketplaceClient {
    address: String,
    http: Http,
}

impl MarketplaceClient {
    /// Port of `NewClient` (client.go:26): the address must parse, and a `localhost` or
    /// `127.0.0.1` Marketplace is trusted — only those two spellings.
    pub fn new(address: &str, config: &Config) -> Result<Self, MarketplaceClientError> {
        let parsed = mm_model::go_url::go_parse(address)
            .map_err(|e| MarketplaceClientError::Address(e.to_string()))?;
        let host = String::from_utf8_lossy(&parsed.host).into_owned();
        let http = match hostname(&host) {
            "localhost" | "127.0.0.1" => Http::Trusted(
                trusted_client(config, REQUEST_TIMEOUT).map_err(MarketplaceClientError::Request)?,
            ),
            _ => Http::Guarded(GuardedClient::new(
                &config.allowed_untrusted_internal_connections,
                config.enable_insecure_outgoing_connections,
            )),
        };
        Ok(Self {
            address: address.to_owned(),
            http,
        })
    }

    /// `buildURL` (client.go:122): the address without trailing slashes, a slash, the path.
    fn build_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.address.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// The URL `GetPlugins` requests: `ApplyToURL` replaces whatever query the address had.
    pub fn plugins_url(
        &self,
        filter: &MarketplacePluginFilter,
    ) -> Result<String, MarketplaceClientError> {
        let mut u = mm_model::go_url::go_parse(&self.build_url("/api/v1/plugins"))
            .map_err(|e| MarketplaceClientError::Request(e.to_string()))?;
        filter.apply_to_url(&mut u);
        Ok(u.to_go_string())
    }

    /// Port of `GetPlugins` (client.go:45): 200 is the list, anything else an error.
    #[tracing::instrument(skip_all, fields(status))]
    pub async fn get_plugins(
        &self,
        filter: &MarketplacePluginFilter,
    ) -> Result<Vec<Option<BaseMarketplacePlugin>>, MarketplaceClientError> {
        let url = self.plugins_url(filter)?;
        let response = match &self.http {
            Http::Trusted(client) => client
                .get(&url)
                .send()
                .await
                .map_err(|e| MarketplaceClientError::Request(e.to_string()))?,
            Http::Guarded(client) => client
                .get(&url, REQUEST_TIMEOUT)
                .await
                .map_err(|e: GuardError| MarketplaceClientError::Request(e.to_string()))?,
        };
        let status = response.status().as_u16();
        tracing::Span::current().record("status", status);
        if status != 200 {
            return Err(MarketplaceClientError::Status(status));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| MarketplaceClientError::Request(e.to_string()))?;
        base_plugins_from_json(&body)
    }

    /// Port of `GetPlugin` (client.go:67): every version, then the one asked for.
    pub async fn get_plugin(
        &self,
        filter: &mut MarketplacePluginFilter,
        version: &str,
    ) -> Result<Option<BaseMarketplacePlugin>, MarketplaceClientError> {
        filter.return_all_versions = true;
        if filter.plugin_id.is_empty() {
            return Err(MarketplaceClientError::MissingPluginId);
        }
        if version.is_empty() {
            return Err(MarketplaceClientError::MissingVersion);
        }
        for plugin in self.get_plugins(filter).await?.into_iter().flatten() {
            if plugin
                .manifest
                .as_ref()
                .is_some_and(|m| m.version == version)
            {
                return Ok(Some(plugin));
            }
        }
        Err(MarketplaceClientError::NotFound)
    }

    /// Port of `GetLatestPlugin` (client.go:91): exactly one entry. A `null` entry is Go's nil
    /// pointer, returned without error.
    pub async fn get_latest_plugin(
        &self,
        filter: &mut MarketplacePluginFilter,
    ) -> Result<Option<BaseMarketplacePlugin>, MarketplaceClientError> {
        filter.return_all_versions = false;
        if filter.plugin_id.is_empty() {
            return Err(MarketplaceClientError::NoPluginId);
        }
        let mut plugins = self.get_plugins(filter).await?;
        match plugins.len() {
            0 => Err(MarketplaceClientError::NotFound),
            1 => Ok(plugins.swap_remove(0)),
            _ => Err(MarketplaceClientError::MoreThanOne),
        }
    }
}

/// `BaseMarketplacePluginsFromReader` (marketplace_plugin.go:48): the **first** JSON value of the
/// body — trailing bytes are never read — where an empty body (`io.EOF`) and `null` are both the
/// empty list. Decoded through a `Value` so a repeated key keeps its last value, as Go does.
fn base_plugins_from_json(
    body: &[u8],
) -> Result<Vec<Option<BaseMarketplacePlugin>>, MarketplaceClientError> {
    let Some(first) = serde_json::Deserializer::from_slice(body)
        .into_iter::<serde_json::Value>()
        .next()
    else {
        return Ok(Vec::new());
    };
    let value = first?;
    if value.is_null() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_value(value)?)
}

pub use mm_plugin::environment::PrepackagedPlugin;

/// Port of `mergePrepackagedPlugins` (app/plugin.go:661): a prepackaged plugin is added when the
/// Marketplace lacks it and replaces the Marketplace's entry only when **strictly** newer. Either
/// version failing `StrictNewVersion` is the 400 `app.plugin.invalid_version.app_error`.
pub fn merge_prepackaged_plugins(
    remote: &mut BTreeMap<String, MarketplacePlugin>,
    prepackaged: &[PrepackagedPlugin],
) -> Result<(), Box<AppError>> {
    let invalid = |e: mm_model::manifest::VersionParseError| {
        Box::new(
            AppError::new(
                "mergePrepackagedPlugins",
                "app.plugin.invalid_version.app_error",
                None,
                "",
                400,
            )
            .wrap(e),
        )
    };
    for plugin in prepackaged {
        let Some(manifest) = &plugin.manifest else {
            continue;
        };
        let entry = MarketplacePlugin {
            base: BaseMarketplacePlugin {
                homepage_url: manifest.homepage_url.clone(),
                icon_data: plugin.icon_data.clone(),
                release_notes_url: manifest.release_notes_url.clone(),
                manifest: Some(manifest.clone()),
                ..BaseMarketplacePlugin::default()
            },
            installed_version: String::new(),
        };
        let Some(existing) = remote.get(&manifest.id) else {
            remote.insert(manifest.id.clone(), entry);
            continue;
        };
        let prepackaged_version =
            StrictVersion::parse_detailed(&manifest.version).map_err(invalid)?;
        let existing_version = existing
            .base
            .manifest
            .as_ref()
            .map_or("", |m| m.version.as_str());
        let marketplace_version =
            StrictVersion::parse_detailed(existing_version).map_err(invalid)?;
        if prepackaged_version > marketplace_version {
            remote.insert(manifest.id.clone(), entry);
        }
    }
    Ok(())
}

/// `go-is-svg`'s `Is` (github.com/h2non/go-is-svg): not binary, and — with every HTML comment
/// removed — an optional XML declaration and SVG doctype, then one `<svg …>…</svg>` with no `*`
/// between, and only whitespace around.
///
/// "Binary" is any of the first 24 bytes being at most 8, and only for input of at least 24
/// bytes. The patterns are Go's RE2 ones: `\s` is ASCII whitespace without `\v`, the negated
/// classes match any byte (Go reads invalid UTF-8 as U+FFFD, which they also match), and `(?i)`
/// folds Unicode, so `ſ` matches `s`.
pub fn is_svg(buf: &[u8]) -> bool {
    use std::sync::LazyLock;
    static COMMENT: LazyLock<Option<regex::bytes::Regex>> =
        LazyLock::new(|| regex::bytes::Regex::new(r"(?i)<!--(?s-u:.)*?-->").ok());
    static SVG: LazyLock<Option<regex::bytes::Regex>> = LazyLock::new(|| {
        regex::bytes::Regex::new(
            r"(?i)\A[\t\n\f\r ]*(?:<\?xml(?-u:[^>])*>[\t\n\f\r ]*)?(?:<!doctype svg(?-u:[^>])*>[\t\n\f\r ]*)?<svg(?-u:[^>])*>(?-u:[^*])*</svg>[\t\n\f\r ]*\z",
        )
        .ok()
    });
    let binary = buf.len() >= 24 && buf[..24].iter().any(|b| *b <= 8);
    if binary {
        return false;
    }
    let (Some(comment), Some(svg)) = (COMMENT.as_ref(), SVG.as_ref()) else {
        return false;
    };
    svg.is_match(&comment.replace_all(buf, &b""[..]))
}

/// `getIcon` (app/plugin.go:1230): an SVG file as a `data:` URI.
pub(crate) fn get_icon(path: &str) -> Result<String, String> {
    use base64::Engine;
    let icon =
        std::fs::read(path).map_err(|e| format!("failed to open icon at path {path}: {e}"))?;
    if !is_svg(&icon) {
        return Err(format!("icon is not svg {path}"));
    }
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(icon)
    ))
}

/// Port of `mergeLocalPlugins` (app/plugin.go:708) over the environment's available bundles.
pub fn merge_local_plugins(
    remote: &mut BTreeMap<String, MarketplacePlugin>,
    local: Vec<mm_model::bundle_info::BundleInfo>,
    enable_remote_marketplace: bool,
) {
    for bundle in local {
        let Some(manifest) = bundle.manifest else {
            continue;
        };
        if let Some(existing) = remote.get_mut(&manifest.id) {
            existing.installed_version = manifest.version;
            continue;
        }
        let mut icon_data = String::new();
        if !manifest.icon_path.is_empty() {
            let path = mm_model::go_path::join(&[&bundle.path, &manifest.icon_path]);
            match get_icon(&path) {
                Ok(data) => icon_data = data,
                Err(err) => tracing::warn!(
                    plugin_id = %manifest.id,
                    icon_path = %manifest.icon_path,
                    error = %err,
                    "Error loading local plugin icon"
                ),
            }
        }
        // Not localised: the Marketplace's own labels are not either.
        let labels = enable_remote_marketplace.then(|| {
            vec![MarketplaceLabel {
                name: "Local".to_owned(),
                description: "This plugin is not listed in the marketplace".to_owned(),
                ..MarketplaceLabel::default()
            }]
        });
        remote.insert(
            manifest.id.clone(),
            MarketplacePlugin {
                base: BaseMarketplacePlugin {
                    homepage_url: manifest.homepage_url.clone(),
                    icon_data,
                    release_notes_url: manifest.release_notes_url.clone(),
                    labels,
                    manifest: Some(manifest.clone()),
                    ..BaseMarketplacePlugin::default()
                },
                installed_version: manifest.version,
            },
        );
    }
}

/// `pluginMatchesFilter` (app/plugin.go:788): trimmed and lower-cased, then the exact id, or a
/// substring of the name or of the description.
pub fn plugin_matches_filter(manifest: &Manifest, filter: &str) -> bool {
    use mm_model::utils::go_to_lower;
    let filter = go_to_lower(filter);
    let filter = filter.trim();
    filter.is_empty()
        || go_to_lower(&manifest.id) == filter
        || go_to_lower(&manifest.name).contains(filter)
        || go_to_lower(&manifest.description).contains(filter)
}

/// The filter's plugins in Go's order: lower-cased name, stably.
pub fn filter_and_sort(
    plugins: BTreeMap<String, MarketplacePlugin>,
    filter: &str,
) -> Vec<MarketplacePlugin> {
    let mut result: Vec<MarketplacePlugin> = plugins
        .into_values()
        .filter(|p| {
            // A Marketplace entry without a manifest never enters the map, so every manifest
            // here is present; Go would dereference nil otherwise.
            p.base
                .manifest
                .as_ref()
                .is_some_and(|m| plugin_matches_filter(m, filter))
        })
        .collect();
    result.sort_by_cached_key(|p| {
        p.base
            .manifest
            .as_ref()
            .map(|m| mm_model::utils::go_to_lower(&m.name))
            .unwrap_or_default()
    });
    result
}

fn not_found(where_: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.plugin.marketplace_plugins.not_found.app_error",
        None,
        "",
        500,
    )
}

impl App {
    /// Port of `getBaseMarketplaceFilter` (app/plugin.go:766): this server's version, the two
    /// licence flags, whether the build is enterprise-ready, and `GOOS-GOARCH`.
    pub async fn base_marketplace_filter(&self) -> MarketplacePluginFilter {
        let license = match self.license().await {
            Ok(license) => license,
            Err(err) => {
                tracing::warn!(error = %err, "reading the licence for the marketplace filter");
                None
            }
        };
        let (os, arch) = mm_plugin::environment::go_platform();
        MarketplacePluginFilter {
            server_version: mm_model::version::CURRENT_VERSION.to_owned(),
            enterprise_plugins: license
                .as_ref()
                .is_some_and(|l| l.has_enterprise_marketplace_plugins()),
            cloud: license.as_ref().is_some_and(|l| l.is_cloud()),
            build_enterprise_ready: mm_model::version::BUILD_ENTERPRISE_READY == "true",
            platform: format!("{os}-{arch}"),
            ..MarketplacePluginFilter::default()
        }
    }

    /// Port of `getRemotePlugins` (app/plugin.go:624): every Marketplace plugin (`PerPage = -1`,
    /// so no `per_page` is sent), by id. An entry without a manifest is dropped.
    async fn get_remote_plugins(
        &self,
    ) -> Result<BTreeMap<String, MarketplacePlugin>, Box<AppError>> {
        const WHERE: &str = "getRemotePlugins";
        if self.plugins_environment().is_none() {
            return Err(AppError::boxed(
                WHERE,
                "app.plugin.config.app_error",
                None,
                "",
                500,
            ));
        }
        let config = self.config();
        let client =
            MarketplaceClient::new(&config.plugin_marketplace_url, &config).map_err(|e| {
                Box::new(
                    AppError::new(
                        WHERE,
                        "app.plugin.marketplace_client.app_error",
                        None,
                        "",
                        500,
                    )
                    .wrap(e),
                )
            })?;
        let mut filter = self.base_marketplace_filter().await;
        filter.per_page = -1;
        let plugins = client.get_plugins(&filter).await.map_err(|e| {
            Box::new(
                AppError::new(
                    WHERE,
                    "app.plugin.marketplace_client.failed_to_fetch",
                    None,
                    "",
                    500,
                )
                .wrap(e),
            )
        })?;
        let mut result = BTreeMap::new();
        for plugin in plugins.into_iter().flatten() {
            let Some(id) = plugin.manifest.as_ref().map(|m| m.id.clone()) else {
                continue;
            };
            result.insert(
                id,
                MarketplacePlugin {
                    base: plugin,
                    installed_version: String::new(),
                },
            );
        }
        Ok(result)
    }

    /// Port of `App.GetMarketplacePlugins` (app/plugin.go:538). Empty is Go's nil slice.
    #[tracing::instrument(skip_all, fields(local_only = filter.local_only, remote_only = filter.remote_only))]
    pub async fn get_marketplace_plugins(
        &self,
        filter: &MarketplacePluginFilter,
    ) -> Result<Vec<MarketplacePlugin>, Box<AppError>> {
        let mut plugins = BTreeMap::new();
        let config = self.config();
        if config.plugin_enable_remote_marketplace && !filter.local_only {
            plugins = self.get_remote_plugins().await?;
        }
        if !filter.remote_only {
            let Some(environment) = self.plugins_environment() else {
                return Err(AppError::boxed(
                    "mergePrepackagedPlugins",
                    "app.plugin.config.app_error",
                    None,
                    "",
                    500,
                ));
            };
            merge_prepackaged_plugins(&mut plugins, &environment.prepackaged_plugins())?;
            let local = environment.available().map_err(|e| {
                Box::new(
                    AppError::new(
                        "GetMarketplacePlugins",
                        "app.plugin.config.app_error",
                        None,
                        "",
                        500,
                    )
                    .wrap(e),
                )
            })?;
            merge_local_plugins(&mut plugins, local, config.plugin_enable_remote_marketplace);
        }
        Ok(filter_and_sort(plugins, &filter.filter))
    }

    /// Port of `getRemoteMarketplacePlugin` (app/plugin.go:599): the latest compatible version
    /// when none is named. Any failure of the request is `not_found`.
    async fn get_remote_marketplace_plugin(
        &self,
        plugin_id: &str,
        version: &str,
    ) -> Result<Option<BaseMarketplacePlugin>, Box<AppError>> {
        const WHERE: &str = "GetMarketplacePlugin";
        let config = self.config();
        let client =
            MarketplaceClient::new(&config.plugin_marketplace_url, &config).map_err(|e| {
                Box::new(
                    AppError::new(
                        WHERE,
                        "app.plugin.marketplace_client.app_error",
                        None,
                        "",
                        500,
                    )
                    .wrap(e),
                )
            })?;
        let mut filter = self.base_marketplace_filter().await;
        plugin_id.clone_into(&mut filter.plugin_id);
        let found = if version.is_empty() {
            client.get_latest_plugin(&mut filter).await
        } else {
            client.get_plugin(&mut filter, version).await
        };
        found.map_err(|e| {
            Box::new(
                AppError::new(
                    WHERE,
                    "app.plugin.marketplace_plugins.not_found.app_error",
                    None,
                    "",
                    500,
                )
                .wrap(e),
            )
        })
    }

    /// Port of `Channels.InstallMarketplacePlugin` (app/plugin_install.go:258).
    ///
    /// A prepackaged bundle first; then, with the remote Marketplace on,
    /// the Marketplace's entry, downloaded when newer than the prepackaged one. A failure to reach
    /// the Marketplace is only logged — the plugin may be prepackaged-only — so it surfaces as the
    /// `not_found` 500. The bundle must then carry a signature that verifies, and is installed,
    /// replacing any installed version.
    #[tracing::instrument(skip_all, fields(plugin_id = %request.id))]
    pub async fn install_marketplace_plugin(
        &self,
        request: &InstallMarketplacePluginRequest,
    ) -> Result<Option<Manifest>, Box<AppError>> {
        const WHERE: &str = "InstallMarketplacePlugin";
        tracing::info!(requested_version = %request.version, "Installing plugin from marketplace");

        // `getPrepackagedPlugin`: a nil environment is its own 500; otherwise the first bundle with
        // the id (and the version, when one is named), or none.
        let Some(environment) = self.plugins_environment() else {
            return Err(AppError::boxed(
                "getPrepackagedPlugin",
                "app.plugin.config.app_error",
                None,
                "plugin environment is nil",
                500,
            ));
        };
        let prepackaged: Option<PrepackagedPlugin> =
            environment.prepackaged_plugins().into_iter().find(|p| {
                p.manifest.as_ref().is_some_and(|m| {
                    m.id == request.id
                        && (request.version.is_empty() || m.version == request.version)
                })
            });
        let mut plugin_file: Option<Vec<u8>> = None;
        let mut signature_file: Option<Vec<u8>> = None;
        if let Some(p) = &prepackaged {
            let open_error = |detail: String, e: std::io::Error| {
                Box::new(
                    AppError::new(
                        WHERE,
                        "app.plugin.install_marketplace_plugin.app_error",
                        None,
                        &detail,
                        500,
                    )
                    .wrap(e),
                )
            };
            plugin_file = Some(std::fs::read(&p.path).map_err(|e| {
                open_error(format!("failed to open prepackaged plugin {}", p.path), e)
            })?);
            signature_file = Some(std::fs::read(&p.signature_path).map_err(|e| {
                open_error(
                    format!(
                        "failed to open prepackaged plugin signature {}",
                        p.signature_path
                    ),
                    e,
                )
            })?);
            tracing::debug!(bundle_path = %p.path, signature_path = %p.signature_path, "Found matching pre-packaged plugin");
        }

        if self.config().plugin_enable_remote_marketplace {
            let plugin = match self
                .get_remote_marketplace_plugin(&request.id, &request.version)
                .await
            {
                Ok(plugin) => plugin,
                Err(err) => {
                    if err.id != "app.plugin.marketplace_plugins.not_found.app_error" {
                        tracing::warn!(error = %err, "Failed to reach Marketplace to install plugin");
                    }
                    None
                }
            };
            if let Some(plugin) = plugin {
                let prepackaged_version = match &prepackaged {
                    None => StrictVersion::default(),
                    Some(p) => StrictVersion::parse_detailed(
                        p.manifest.as_ref().map_or("", |m| m.version.as_str()),
                    )
                    .map_err(|e| {
                        Box::new(
                            AppError::new(
                                WHERE,
                                "app.plugin.invalid_version.app_error",
                                None,
                                "",
                                400,
                            )
                            .wrap(e),
                        )
                    })?,
                };
                let marketplace_version = StrictVersion::parse_detailed(
                    plugin.manifest.as_ref().map_or("", |m| m.version.as_str()),
                )
                .map_err(|e| {
                    Box::new(
                        AppError::new(
                            WHERE,
                            "app.prepackged-plugin.invalid_version.app_error",
                            None,
                            "",
                            400,
                        )
                        .wrap(e),
                    )
                })?;
                if prepackaged_version < marketplace_version {
                    let bundle =
                        self.download_from_url(&plugin.download_url)
                            .await
                            .map_err(|e| {
                                Box::new(
                                    AppError::new(
                                        WHERE,
                                        "app.plugin.install_marketplace_plugin.app_error",
                                        None,
                                        "",
                                        500,
                                    )
                                    .wrap(e),
                                )
                            })?;
                    let signature = plugin.decode_signature().map_err(|e| {
                        Box::new(
                            AppError::new(
                                WHERE,
                                "app.plugin.signature_decode.app_error",
                                None,
                                "",
                                501,
                            )
                            .wrap(e),
                        )
                    })?;
                    plugin_file = Some(bundle);
                    signature_file = Some(signature);
                } else {
                    tracing::debug!(version = ?plugin.manifest.map(|m| m.version), "Preferring pre-packaged plugin over version in remote marketplace");
                }
            }
        }

        let Some(plugin_file) = plugin_file else {
            return Err(not_found(WHERE));
        };
        let Some(signature_file) = signature_file else {
            return Err(AppError::boxed(
                WHERE,
                "app.plugin.marketplace_plugins.signature_not_found.app_error",
                None,
                "",
                500,
            ));
        };
        self.verify_plugin(&plugin_file, &signature_file).await?;
        self.install_plugin_with(
            &plugin_file,
            Some(&signature_file),
            crate::plugin_install::InstallStrategy::Always,
        )
        .await
    }

    /// Port of `Server.downloadFromURL` (app/download.go:28): a valid `http(s)` URL; `https`
    /// only unless `AllowInsecureDownloadURL`; then up to six attempts through a trusted client
    /// with an hour's timeout, any non-2xx answer counting as a failure, and the short backoff
    /// after each failure — the last included.
    #[tracing::instrument(skip(self))]
    pub async fn download_from_url(&self, download_url: &str) -> Result<Vec<u8>, DownloadError> {
        if !mm_model::utils::is_valid_http_url(download_url) {
            return Err(DownloadError::InvalidUrl(download_url.to_owned()));
        }
        let parsed = mm_model::go_url::parse_request_uri(download_url)
            .map_err(|_| DownloadError::Parse(download_url.to_owned()))?;
        let config = self.config();
        if !config.plugin_allow_insecure_download_url && parsed.scheme != "https" {
            return Err(DownloadError::Insecure(download_url.to_owned()));
        }
        let client = trusted_client(&config, DOWNLOAD_TIMEOUT).map_err(DownloadError::Body)?;
        for backoff in SHORT_BACKOFF {
            match client.get(download_url).send().await {
                Ok(response) if response.status().is_success() => {
                    return response
                        .bytes()
                        .await
                        .map(|b| b.to_vec())
                        .map_err(|e| DownloadError::Body(e.to_string()));
                }
                Ok(response) => {
                    tracing::debug!(
                        status = response.status().as_u16(),
                        "download attempt failed"
                    );
                }
                Err(err) => tracing::debug!(error = %err, "download attempt failed"),
            }
            tokio::time::sleep(backoff).await;
        }
        Err(DownloadError::Fetch(download_url.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, name: &str, version: &str) -> Manifest {
        Manifest {
            id: id.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            ..Manifest::default()
        }
    }

    fn remote(id: &str, name: &str, version: &str) -> MarketplacePlugin {
        MarketplacePlugin {
            base: BaseMarketplacePlugin {
                download_url: format!("https://example.invalid/{id}"),
                manifest: Some(manifest(id, name, version)),
                ..BaseMarketplacePlugin::default()
            },
            installed_version: String::new(),
        }
    }

    fn prepackaged(id: &str, version: &str) -> PrepackagedPlugin {
        PrepackagedPlugin {
            manifest: Some(manifest(id, "pre", version)),
            icon_data: "data:pre".to_owned(),
            ..PrepackagedPlugin::default()
        }
    }

    #[test]
    fn go_is_svg_matches_go() {
        use base64::Engine;
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_plugin_signature.json"
        ))
        .unwrap();
        let cases = oracle["svg_is"].as_array().unwrap();
        assert!(cases.len() >= 20);
        for case in cases {
            let input = base64::engine::general_purpose::STANDARD
                .decode(case["input"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                is_svg(&input),
                case["is"].as_bool().unwrap(),
                "{:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    /// Added when absent; replaces only when strictly newer; an invalid version on either side
    /// is the 400; a bundle without a manifest is skipped.
    #[test]
    fn merge_prepackaged_plugins_follows_go() {
        let mut map = BTreeMap::from([
            ("same".to_owned(), remote("same", "Same", "1.0.0")),
            ("older".to_owned(), remote("older", "Older", "1.0.0")),
            ("newer".to_owned(), remote("newer", "Newer", "2.0.0")),
        ]);
        merge_prepackaged_plugins(
            &mut map,
            &[
                prepackaged("absent", "0.1.0"),
                prepackaged("same", "1.0.0"),
                prepackaged("older", "1.0.1"),
                prepackaged("newer", "1.9.9"),
                PrepackagedPlugin::default(),
            ],
        )
        .unwrap();
        let icon = |id: &str| map[id].base.icon_data.clone();
        assert_eq!(icon("absent"), "data:pre");
        assert_eq!(icon("same"), "", "an equal version keeps the Marketplace's");
        assert_eq!(
            icon("older"),
            "data:pre",
            "a newer prepackaged one replaces it"
        );
        assert_eq!(icon("newer"), "");
        assert_eq!(map["older"].base.download_url, "");

        let mut bad = BTreeMap::from([("x".to_owned(), remote("x", "X", "v1.0.0"))]);
        let err = merge_prepackaged_plugins(&mut bad, &[prepackaged("x", "1.0.0")]).unwrap_err();
        assert_eq!(err.id, "app.plugin.invalid_version.app_error");
        assert_eq!(err.status_code, 400);
        let mut bad = BTreeMap::from([("x".to_owned(), remote("x", "X", "1.0.0"))]);
        let err = merge_prepackaged_plugins(&mut bad, &[prepackaged("x", "1.0")]).unwrap_err();
        assert_eq!(err.id, "app.plugin.invalid_version.app_error");
    }

    fn bundle(dir: &std::path::Path, m: Manifest) -> mm_model::bundle_info::BundleInfo {
        mm_model::bundle_info::BundleInfo {
            path: dir.to_string_lossy().into_owned(),
            manifest: Some(m),
            ..mm_model::bundle_info::BundleInfo::default()
        }
    }

    /// An installed Marketplace plugin gains `installed_version`; a local-only one is added with
    /// the `Local` label only while the remote Marketplace is on, and its SVG icon inlined.
    #[test]
    fn merge_local_plugins_follows_go() {
        let dir = std::env::temp_dir().join(format!("mmrs-merge-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("icon.svg"), "<svg></svg>").unwrap();
        std::fs::write(dir.join("icon.png"), "not svg").unwrap();
        let with_icon = |id: &str, icon: &str| Manifest {
            icon_path: icon.to_owned(),
            ..manifest(id, id, "0.3.0")
        };
        for remote_on in [true, false] {
            let mut map = BTreeMap::from([("both".to_owned(), remote("both", "Both", "1.0.0"))]);
            merge_local_plugins(
                &mut map,
                vec![
                    bundle(&dir, manifest("both", "Both", "0.9.0")),
                    bundle(&dir, with_icon("svg", "icon.svg")),
                    bundle(&dir, with_icon("png", "icon.png")),
                    mm_model::bundle_info::BundleInfo::default(),
                ],
                remote_on,
            );
            assert_eq!(map.len(), 3);
            assert_eq!(map["both"].installed_version, "0.9.0");
            assert_eq!(
                map["both"].base.download_url,
                "https://example.invalid/both"
            );
            assert_eq!(map["svg"].installed_version, "0.3.0");
            assert_eq!(
                map["svg"].base.icon_data,
                "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4="
            );
            assert_eq!(map["png"].base.icon_data, "");
            assert_eq!(map["svg"].base.labels.is_some(), remote_on);
            if remote_on {
                let labels = map["svg"].base.labels.as_ref().unwrap();
                assert_eq!(labels[0].name, "Local");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_filter_is_an_exact_id_or_a_name_or_description_substring() {
        let m = Manifest {
            description: "Does Things".to_owned(),
            ..manifest("com.example.Probe", "My Probe", "1.0.0")
        };
        for (filter, matches) in [
            ("", true),
            ("   ", true),
            ("  com.example.probe ", true),
            ("com.example", false),
            ("PROBE", true),
            ("things", true),
            ("nothing", false),
        ] {
            assert_eq!(plugin_matches_filter(&m, filter), matches, "{filter:?}");
        }
    }

    #[test]
    fn the_result_is_sorted_by_lower_cased_name() {
        let map = BTreeMap::from([
            ("a".to_owned(), remote("a", "beta", "1.0.0")),
            ("b".to_owned(), remote("b", "Alpha", "1.0.0")),
            ("c".to_owned(), remote("c", "Zulu", "1.0.0")),
            ("d".to_owned(), remote("d", "ALPHA2", "1.0.0")),
        ]);
        let ids: Vec<_> = filter_and_sort(map, "")
            .into_iter()
            .map(|p| p.base.manifest.unwrap().id)
            .collect();
        assert_eq!(ids, ["b", "d", "a", "c"]);
    }

    /// Only the first JSON value is read; empty and `null` are the empty list.
    #[test]
    fn the_marketplace_body_decodes_as_go_reads_it() {
        assert!(base_plugins_from_json(b"").unwrap().is_empty());
        assert!(base_plugins_from_json(b"null").unwrap().is_empty());
        assert!(base_plugins_from_json(b"{}").is_err());
        let two = base_plugins_from_json(br#"[{"manifest":{"id":"x"}}, null] trailing"#).unwrap();
        assert_eq!(two.len(), 2);
        assert!(two[1].is_none());
    }

    #[test]
    fn hostname_drops_the_port() {
        assert_eq!(hostname("localhost:8080"), "localhost");
        assert_eq!(hostname("127.0.0.1"), "127.0.0.1");
        assert_eq!(hostname("[::1]:80"), "::1");
    }
}
