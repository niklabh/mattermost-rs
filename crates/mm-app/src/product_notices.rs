//! Port of `app/product_notices.go` — the notice cache, its refresh, and `GetProductNotices`,
//! the app function behind `GET /api/v4/system/notices/{team_id}`.
//!
//! # The cache is this process's, refreshed the way Go refreshes its own
//!
//! Go fills `Channels.cachedNotices` (with the post count, the user count and the database
//! version the conditions read) in a goroutine at start-up and then from the `product_notices`
//! job every `AnnouncementSettings.NoticesFetchFrequency` seconds, from
//! `AnnouncementSettings.NoticesURL` with `ETag`/`Date` revalidation. This server does the same
//! from `main.rs`: one fetch at start-up, then a task on the same period. The two caches are
//! filled from the same feed at different instants, which is the only way they can differ.
//!
//! # `UpdateViewedProductNoticesForNewUser` is not this module's
//!
//! Go marks every cached notice viewed for a user the moment `CreateUser` writes the row, so a
//! user created through Go sees nothing until the feed changes. `mm_app::user_create` does not
//! do that yet ([D-683]); a user created here therefore sees the notices a Go-created one has
//! already dismissed. Recorded, not hidden: it is the user family's row to write.

use std::sync::{Arc, RwLock};

use chrono::{Datelike, TimeZone, Utc};
use mm_model::notice_conditions::{
    config_entry_matches, new_constraint, new_date_constraint, new_version,
};
use mm_model::permission::{PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM};
use mm_model::product_notices::{
    NOTICE_CLIENT_TYPE_MOBILE_ANDROID, NOTICE_CLIENT_TYPE_MOBILE_IOS, NoticeClientType,
    NoticeMessage, NoticeMessages, ProductNotice, ProductNotices,
};
use mm_model::session::Session;
use mm_model::user_count::UserCountOptions;
use mm_model::utils::{AppError, AppResult, CURRENT_VERSION};
use mm_store::{PostStore, PreferenceStore, ProductNoticesStore, UserStore};

use crate::App;

/// `MaxRepeatViewings` and `MinSecondsBetweenRepeatViewings` (product_notices.go:24-25).
pub const MAX_REPEAT_VIEWINGS: i32 = 3;
pub const MIN_SECONDS_BETWEEN_REPEAT_VIEWINGS: i64 = 60 * 60;

/// `model.DatabaseDriverPostgres` and `model.SearchengineElasticsearch`, the two names
/// `DeprecatingDependency` switches on.
const DATABASE_DRIVER_POSTGRES: &str = "postgres";
const SEARCHENGINE_ELASTICSEARCH: &str = "elasticsearch";

/// The feed as this process last fetched it, plus `utils.RequestCache`'s revalidation pair and
/// the three counts `UpdateProductNotices` refreshes beside it.
#[derive(Debug, Default)]
pub struct NoticesCache {
    pub notices: ProductNotices,
    pub post_count: i64,
    pub user_count: i64,
    pub dbms_version: String,
    data: Option<Vec<u8>>,
    etag: String,
    date: String,
}

/// A shared handle, one per `App` and every clone of it.
pub type SharedNoticesCache = Arc<RwLock<NoticesCache>>;

impl App {
    /// Port of `App.UpdateProductNotices` (product_notices.go:322): the three counts (each
    /// failure logged and the old value kept), the fetch, the parse, and `ClearOldNotices`.
    ///
    /// The fetch is `utils.GetURLWithCache`: `If-None-Match`/`If-Modified-Since` from the last
    /// answer unless `NoticesSkipCache`, a 304 keeps the cached bytes, anything but a 200 drops
    /// them and is the `fetch_failed` error. Go's client has no timeout; this one waits thirty
    /// seconds, since a hung feed must not hold the refresh task forever.
    #[tracing::instrument(skip(self), fields(notices))]
    pub async fn update_product_notices(&self) -> AppResult {
        let url = self.config().notices_url.clone();
        let skip = self.config().notices_skip_cache;
        tracing::debug!(url, skip_cache = skip, "Will fetch notices from");

        match self.store().post().analytics_post_count_total().await {
            Ok(count) => {
                self.notices_cache()
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .post_count = count
            }
            Err(err) => tracing::warn!(error = %err, "Failed to fetch post count"),
        }
        match self
            .store()
            .user()
            .count(&UserCountOptions {
                include_deleted: true,
                ..UserCountOptions::default()
            })
            .await
        {
            Ok(count) => {
                self.notices_cache()
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .user_count = count
            }
            Err(err) => tracing::warn!(error = %err, "Failed to fetch user count"),
        }
        match self.store().get_db_version(false).await {
            Ok(version) => {
                // `strings.Split(version, " ")[0]`: `SHOW server_version` says `16.4 (Debian …)`.
                let head = version.split(' ').next().unwrap_or_default().to_owned();
                self.notices_cache()
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .dbms_version = head;
            }
            Err(err) => tracing::warn!(error = %err, "Failed to get DBMS version"),
        }

        let data = self.fetch_notices(&url, skip).await.map_err(|err| {
            tracing::warn!(error = %err, url, "notices fetch failed");
            AppError::boxed(
                "UpdateProductNotices",
                "api.system.update_notices.fetch_failed",
                None,
                String::new(),
                400,
            )
        })?;
        let notices: ProductNotices = serde_json::from_slice(&data).map_err(|err| {
            tracing::warn!(error = %err, "notices parse failed");
            AppError::boxed(
                "UpdateProductNotices",
                "api.system.update_notices.parse_failed",
                None,
                String::new(),
                400,
            )
        })?;
        tracing::Span::current().record("notices", notices.0.len());

        if let Err(err) = self
            .store()
            .product_notices()
            .clear_old_notices(&notices)
            .await
        {
            tracing::warn!(error = %err, "clearing old notice views failed");
            return Err(AppError::boxed(
                "UpdateProductNotices",
                "api.system.update_notices.clear_failed",
                None,
                String::new(),
                400,
            ));
        }
        self.notices_cache()
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .notices = notices;
        Ok(())
    }

    /// `utils.GetURLWithCache` (channels/utils/utils.go:133).
    async fn fetch_notices(&self, url: &str, skip: bool) -> Result<Vec<u8>, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|err| err.to_string())?;
        let (cached, etag, date) = {
            let cache = self
                .notices_cache()
                .read()
                .unwrap_or_else(|p| p.into_inner());
            (cache.data.clone(), cache.etag.clone(), cache.date.clone())
        };
        let mut request = client.get(url);
        if !skip && cached.is_some() {
            request = request
                .header("If-None-Match", etag)
                .header("If-Modified-Since", date);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                self.notices_cache()
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .data = None;
                return Err(err.to_string());
            }
        };
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return cached.ok_or_else(|| "not modified with no cached body".to_owned());
        }
        if response.status() != reqwest::StatusCode::OK {
            self.notices_cache()
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .data = None;
            return Err(format!(
                "Fetching notices failed with status code {}",
                response.status().as_u16()
            ));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let date = response
            .headers()
            .get("date")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = match response.bytes().await {
            Ok(body) => body.to_vec(),
            Err(err) => {
                self.notices_cache()
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .data = None;
                return Err(err.to_string());
            }
        };
        let mut cache = self
            .notices_cache()
            .write()
            .unwrap_or_else(|p| p.into_inner());
        cache.data = Some(body.clone());
        cache.etag = etag;
        cache.date = date;
        Ok(body)
    }

    /// Port of `App.GetProductNotices` (product_notices.go:213).
    ///
    /// The two `AnnouncementSettings` gates come first and answer `[]` — the user gate for a
    /// non-admin, the admin gate for an admin of either kind. Then the user's view rows (a store
    /// failure is the 400 `update_viewed_notices.failed`), and each cached notice in feed order:
    /// skipped when viewed (a repeatable one after `MaxRepeatViewings` or within the hour), else
    /// judged by [`notice_matches_conditions`], whose error is the 400 `validating_failed` and
    /// whose match yields the **English** message whatever `locale` said — `selectedLocale :=
    /// "en"` is hard-coded, and a notice without an `en` message contributes a zero message.
    #[tracing::instrument(skip(self, session), fields(user_id = %user_id, team_id = %team_id, matched))]
    pub async fn get_product_notices(
        &self,
        session: &Session,
        user_id: &str,
        team_id: &str,
        client: &NoticeClientType,
        client_version: &str,
        _locale: &str,
    ) -> AppResult<NoticeMessages> {
        let is_system_admin = self
            .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
            .await;
        let is_team_admin = self
            .session_has_permission_to_team(session, team_id, &PERMISSION_MANAGE_TEAM)
            .await;

        if !self.config().user_notices_enabled && !is_system_admin {
            return Ok(NoticeMessages(Vec::new()));
        }
        if !self.config().admin_notices_enabled && (is_team_admin || is_system_admin) {
            return Ok(NoticeMessages(Vec::new()));
        }

        let views = self
            .store()
            .product_notices()
            .get_views(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reading the notice views failed");
                AppError::boxed(
                    "GetProductNotices",
                    "api.system.update_viewed_notices.failed",
                    None,
                    String::new(),
                    400,
                )
            })?;

        let sku = self
            .client_license()
            .await?
            .get("SkuShortName")
            .cloned()
            .unwrap_or_default();
        let license = self.license().await?;
        let is_cloud = license
            .as_deref()
            .is_some_and(|l| l.features.as_ref().and_then(|f| f.cloud) == Some(true));
        let db_name = DATABASE_DRIVER_POSTGRES;
        // No search engine here, as on the Team Edition binary: both names are empty.
        let (search_engine_name, search_engine_version) = ("", "");

        let (notices, post_count, user_count, dbms_version) = {
            let cache = self
                .notices_cache()
                .read()
                .unwrap_or_else(|p| p.into_inner());
            (
                cache.notices.0.clone(),
                cache.post_count,
                cache.user_count,
                cache.dbms_version.clone(),
            )
        };
        // The running configuration is read once, lazily, for the first notice that asks.
        let mut running_config: Option<serde_json::Value> = None;
        let now = Utc::now();

        let mut filtered = Vec::new();
        for notice in &notices {
            if let Some(view) = views.iter().find(|v| v.notice_id == notice.id) {
                if notice.repeatable == Some(true) {
                    if view.viewed > MAX_REPEAT_VIEWINGS {
                        continue;
                    }
                    if now.timestamp() - view.timestamp < MIN_SECONDS_BETWEEN_REPEAT_VIEWINGS {
                        continue;
                    }
                } else if view.viewed > 0 {
                    continue;
                }
            }

            let needs_config = notice
                .conditions
                .server_config
                .as_ref()
                .is_some_and(|m| !m.is_empty());
            if needs_config && running_config.is_none() {
                let config = crate::config::load_model_config(self.store().config())
                    .await
                    .map_err(|err| {
                        tracing::error!(error = %err, "could not load the running configuration");
                        AppError::boxed(
                            "GetProductNotices",
                            "api.system.update_notices.validating_failed",
                            None,
                            String::new(),
                            400,
                        )
                    })?;
                running_config = Some(serde_json::to_value(&config).unwrap_or_default());
            }

            let inputs = ConditionInputs {
                config: running_config.as_ref(),
                user_id,
                client,
                server_version: CURRENT_VERSION,
                client_version,
                post_count,
                user_count,
                is_system_admin,
                is_team_admin,
                is_cloud,
                sku: &sku,
                db_name,
                db_version: &dbms_version,
                search_engine_name,
                search_engine_version,
                now,
            };
            let matched = self
                .notice_matches_conditions(&inputs, notice)
                .await
                .map_err(|reason| {
                    tracing::debug!(notice = %notice.id, reason, "notice condition error");
                    AppError::boxed(
                        "GetProductNotices",
                        "api.system.update_notices.validating_failed",
                        None,
                        String::new(),
                        400,
                    )
                })?;
            if matched {
                let internal = notice
                    .localized_messages
                    .as_ref()
                    .and_then(|m| m.get("en"))
                    .cloned()
                    .unwrap_or_default();
                filtered.push(NoticeMessage {
                    internal,
                    id: notice.id.clone(),
                    team_admin_only: notice.team_admin_only(),
                    sys_admin_only: notice.sys_admin_only(),
                });
            }
        }
        tracing::Span::current().record("matched", filtered.len());
        Ok(NoticeMessages(filtered))
    }

    /// Port of `noticeMatchesConditions` (product_notices.go:30), in its order. `Err` is any
    /// of the function's `error` returns — an unparsable client version, constraint, date range
    /// or dependency version, or a malformed `userConfig` entry — and `Ok(false)` any of its
    /// silent refusals.
    async fn notice_matches_conditions(
        &self,
        inputs: &ConditionInputs<'_>,
        notice: &ProductNotice,
    ) -> Result<bool, String> {
        let cnd = &notice.conditions;

        if let Some(client_type) = &cnd.client_type {
            if !client_type.matches(inputs.client) {
                return Ok(false);
            }
        }

        let client_versions = if inputs.client.as_str() == NOTICE_CLIENT_TYPE_MOBILE_ANDROID
            || inputs.client.as_str() == NOTICE_CLIENT_TYPE_MOBILE_IOS
        {
            cnd.mobile_version.as_ref()
        } else {
            cnd.desktop_version.as_ref()
        };
        // Parsed whether or not there is a range to check it against.
        let client_version = new_version(inputs.client_version)
            .ok_or_else(|| format!("Cannot parse version range {}", inputs.client_version))?;
        for v in client_versions.map(Vec::as_slice).unwrap_or(&[]) {
            let c = new_constraint(v).ok_or_else(|| format!("Cannot parse version range {v}"))?;
            if !c.check(&client_version) {
                return Ok(false);
            }
        }

        if let Some(display_date) = &cnd.display_date {
            let (y, m, d) = (inputs.now.year(), inputs.now.month(), inputs.now.day());
            let trunc = Utc
                .with_ymd_and_hms(y, m, d, 0, 0, 0)
                .single()
                .unwrap_or(inputs.now);
            let c = new_date_constraint(display_date)
                .ok_or_else(|| format!("Cannot parse date range {display_date}"))?;
            if !c.check(trunc) {
                return Ok(false);
            }
        }

        if !inputs.is_cloud {
            if let Some(server_versions) = &cnd.server_version {
                let Some(server_version) = new_version(inputs.server_version) else {
                    tracing::warn!(
                        version_number = inputs.server_version,
                        "Version number is not in semver format"
                    );
                    return Ok(false);
                };
                for v in server_versions {
                    let c = new_constraint(v)
                        .ok_or_else(|| format!("Cannot parse version range {v}"))?;
                    if !c.check(&server_version) {
                        return Ok(false);
                    }
                }
            }
        }

        if let Some(sku) = &cnd.sku {
            if !sku.matches(inputs.sku) {
                return Ok(false);
            }
        }

        if let Some(audience) = &cnd.audience {
            if !audience.matches(inputs.is_system_admin, inputs.is_team_admin) {
                return Ok(false);
            }
        }

        if let Some(number_of_users) = cnd.number_of_users {
            if inputs.user_count > 0 && inputs.user_count < number_of_users {
                return Ok(false);
            }
        }
        if let Some(number_of_posts) = cnd.number_of_posts {
            if inputs.post_count > 0 && inputs.post_count < number_of_posts {
                return Ok(false);
            }
        }

        if let Some(dependency) = &cnd.deprecating_dependency {
            let ext = new_version(&dependency.minimum_version).ok_or_else(|| {
                format!(
                    "Cannot parse external dependency version {}",
                    dependency.minimum_version
                )
            })?;
            return match dependency.name.as_str() {
                DATABASE_DRIVER_POSTGRES => {
                    if inputs.db_name != dependency.name {
                        return Ok(false);
                    }
                    let db = new_version(inputs.db_version).ok_or_else(|| {
                        format!("Cannot parse DBMS version {}", inputs.db_version)
                    })?;
                    Ok(ext.compare(&db) == std::cmp::Ordering::Greater)
                }
                SEARCHENGINE_ELASTICSEARCH => {
                    if inputs.search_engine_name != SEARCHENGINE_ELASTICSEARCH {
                        return Ok(false);
                    }
                    let es = new_version(inputs.search_engine_version).ok_or_else(|| {
                        format!(
                            "Cannot parse search engine version {}",
                            inputs.search_engine_version
                        )
                    })?;
                    Ok(ext.compare(&es) == std::cmp::Ordering::Greater)
                }
                _ => Ok(false),
            };
        }

        if let Some(server_config) = &cnd.server_config {
            let config = inputs.config.ok_or_else(|| "no configuration".to_owned())?;
            for (path, expected) in server_config {
                if !config_entry_matches(config, path, expected) {
                    return Ok(false);
                }
            }
        }

        if let Some(user_config) = &cnd.user_config {
            for (key, expected) in user_config {
                let parts: Vec<&str> = key.split('.').collect();
                if parts.len() != 2 {
                    return Err(
                        "Invalid format of user config. Must be in form of Category.SettingName"
                            .to_owned(),
                    );
                }
                let Some(expected) = expected.as_str() else {
                    return Err("Invalid format of user config. Value should be string".to_owned());
                };
                match self
                    .store()
                    .preference()
                    .get(inputs.user_id, parts[0], parts[1])
                    .await
                {
                    Ok(preference) => {
                        if preference.value != expected {
                            return Ok(false);
                        }
                    }
                    Err(_) => return Ok(false),
                }
            }
        }

        if let Some(instance_type) = &cnd.instance_type {
            if !instance_type.matches(inputs.is_cloud) {
                return Ok(false);
            }
        }

        Ok(true)
    }
}

/// Everything `noticeMatchesConditions` takes besides the notice.
struct ConditionInputs<'a> {
    config: Option<&'a serde_json::Value>,
    user_id: &'a str,
    client: &'a NoticeClientType,
    server_version: &'a str,
    client_version: &'a str,
    post_count: i64,
    user_count: i64,
    is_system_admin: bool,
    is_team_admin: bool,
    is_cloud: bool,
    sku: &'a str,
    db_name: &'a str,
    db_version: &'a str,
    search_engine_name: &'a str,
    search_engine_version: &'a str,
    now: chrono::DateTime<Utc>,
}
