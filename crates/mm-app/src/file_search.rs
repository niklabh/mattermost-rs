//! Port of the file-search slice of `app/file.go`: `SearchFilesInTeamForUser` (file.go:1467)
//! and `FilterFilesByChannelPermissions` (:1520), plus the two cloud-limit and ABAC gates they
//! reach — `filterInaccessibleFiles` (file_helper.go:42) through `GetLastAccessibleFileTime`
//! (file.go:1752), and `buildFileDownloadSubject` / `hasFileDownloadPermission` (:1602, :1676).
//!
//! The term pipeline and the name-to-id conversions are [`crate::post_search`]'s, reused rather
//! than copied; the store query is the file store's own ([`FileInfoStore::search`]).
//!
//! # Two gates that cannot fire here, and what happens if they could
//!
//! - **`GetLastAccessibleFileTime` is `0` unless the licence `IsCloud()`.** No licence this
//!   server or the stack's oracles load is a cloud one, so `filterInaccessibleFiles` never
//!   touches the list. The `IsCloud` test is ported; past it the bounds arithmetic is not, and a
//!   cloud licence makes the search [`FileSearchError::Unreproducible`] so Go answers it.
//! - **`buildFileDownloadSubject` answers `(nil, nil)` when `AccessControl` is nil**, which it
//!   is on every build from this tree (the service lives in the enterprise repository), before
//!   `EnableAttributeBasedAccessControl` or the `PermissionPolicies` flag are read. So
//!   `hasFileDownloadPermission` is `true` for every channel the caller may read, and nothing
//!   about the config changes that.

use std::collections::{BTreeMap, HashMap};

use mm_model::channel::Channel;
use mm_model::file_info::FileInfo;
use mm_model::file_info_list::{FileInfoList, FileInfoMap};
use mm_model::search_params::parse_search_params;
use mm_model::utils::{AppError, AppResult};
use mm_store::StoreError;
use mm_store::file_info_store::FileInfoStore;
use mm_store::system_store::SystemStore;

use crate::App;
use crate::post_search::PostSearchError;

/// How a file search can fail short of a Go-shaped error.
#[derive(Debug, thiserror::Error)]
pub enum FileSearchError {
    /// A branch this server does not reproduce was reached; the request belongs to Go.
    #[error("file search is not reproducible here: {0}")]
    Unreproducible(&'static str),
    #[error(transparent)]
    App(#[from] Box<AppError>),
}

impl From<PostSearchError> for FileSearchError {
    fn from(err: PostSearchError) -> Self {
        match err {
            PostSearchError::Unreproducible(reason) => Self::Unreproducible(reason),
            PostSearchError::App(err) => Self::App(err),
        }
    }
}

impl App {
    /// Port of `app.App.SearchFilesInTeamForUser` (file.go:1467).
    ///
    /// Returns the list and `allFilesHaveMembership` — the AND over every surviving file of
    /// "the caller is a member of its channel", which Go only records in the audit log.
    ///
    /// Same skeleton as [`App::search_posts_for_user`]: parse, the `EnableFileSearch` 501, the
    /// `*` skip with an empty list when nothing survives, the store, then the two filters.
    ///
    /// # The `*` skip is unreachable from REST, and kept anyway
    ///
    /// `ParseSearchParams` trims a term's leading punctuation with `^[^\pL\d\s#"]+`, which does
    /// not keep `*`, so a word that is exactly `*` trims to nothing and never becomes an
    /// element's `Terms`; a quoted `"*"` keeps its quotes. No request can make `params.terms ==
    /// "*"` here, so removing the check is an equivalent mutation — it survived
    /// `scripts/mutations/searchmisc.plan` for that reason, not for want of a fixture. Ported
    /// because Go has it and a future caller that builds params by hand would reach it.
    /// `perPage` is parsed by the handler and never read past the store's `page > 0` test, so it
    /// is not a parameter here.
    #[tracing::instrument(
        skip(self, terms),
        fields(user_id = %user_id, team_id = %team_id, is_or_search, include_deleted_channels, page)
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn search_files_in_team_for_user(
        &self,
        terms: &str,
        user_id: &str,
        team_id: &str,
        is_or_search: bool,
        include_deleted_channels: bool,
        time_zone_offset: i64,
        page: i64,
    ) -> Result<(FileInfoList, bool), FileSearchError> {
        let params_list = parse_search_params(terms.trim(), time_zone_offset);

        if !self.config().enable_file_search {
            return Err(AppError::boxed(
                "SearchFilesInTeamForUser",
                "store.sql_file_info.search.disabled",
                None,
                format!("teamId={team_id} userId={user_id}"),
                501,
            )
            .into());
        }

        let mut final_params_list = Vec::with_capacity(params_list.len());
        for mut params in params_list {
            params.or_terms = is_or_search;
            params.include_deleted_channels = include_deleted_channels;
            // "Don't allow users to search for "*""
            if params.terms != "*" {
                params.in_channels = self
                    .convert_channel_names_to_channel_ids(
                        params.in_channels,
                        user_id,
                        team_id,
                        include_deleted_channels,
                    )
                    .await?;
                params.excluded_channels = self
                    .convert_channel_names_to_channel_ids(
                        params.excluded_channels,
                        user_id,
                        team_id,
                        include_deleted_channels,
                    )
                    .await?;
                params.from_users = self.convert_user_name_to_user_ids(params.from_users).await;
                params.excluded_users = self
                    .convert_user_name_to_user_ids(params.excluded_users)
                    .await;
                final_params_list.push(params);
            }
        }

        // "If the processed search params are empty, return empty search results."
        if final_params_list.is_empty() {
            return Ok((FileInfoList::new(), true));
        }

        let mut results = self
            .store()
            .file_info()
            .search(final_params_list, user_id, team_id, page)
            .await
            .map_err(|err| match err {
                // `errors.As(nErr, &appErr)` — the `IsSearchParamsListValid` failure, verbatim.
                StoreError::Invalid { app_error, .. } => app_error,
                other => {
                    tracing::error!(error = %other, "file search failed");
                    AppError::boxed(
                        "SearchFilesInTeamForUser",
                        "app.post.search.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        self.filter_inaccessible_files(&results).await?;

        let all_files_have_membership = self
            .filter_files_by_channel_permissions(&mut results, user_id)
            .await?;

        Ok((results, all_files_have_membership))
    }

    /// Port of `filterInaccessibleFiles` (file_helper.go:42) as far as it goes on this build:
    /// an empty list returns first, then [`App::get_last_accessible_file_time`], which is `0`
    /// without a cloud licence — "No need to filter, all files are accessible". A non-zero time
    /// would reach the bounds arithmetic, which is not ported; see the module docs.
    async fn filter_inaccessible_files(&self, list: &FileInfoList) -> Result<(), FileSearchError> {
        if list.file_infos.as_ref().is_none_or(FileInfoMap::is_empty) {
            return Ok(());
        }
        let last_accessible_file_time = self.get_last_accessible_file_time().await?;
        if last_accessible_file_time == 0 {
            return Ok(());
        }
        Err(FileSearchError::Unreproducible(
            "a cloud licence sets LastAccessibleFileTime, and filterInaccessibleFiles is not ported",
        ))
    }

    /// Port of `App.GetLastAccessibleFileTime` (file.go:1752).
    ///
    /// Zero — "all files are accessible" — unless the licence `IsCloud()`, and then for a
    /// missing `Systems.LastAccessibleFileTime` row. The row is written by a cloud limits job
    /// that is not ported; a value that does not parse is the 500 Go gives it. The read is
    /// wrapped in `app.last_accessible_file.app_error` by the caller, as in Go.
    #[tracing::instrument(skip_all, fields(last_accessible_file_time))]
    pub async fn get_last_accessible_file_time(&self) -> AppResult<i64> {
        let is_cloud = self
            .license()
            .await?
            .is_some_and(|license| license.is_cloud());
        if !is_cloud {
            return Ok(0);
        }
        let stored = self
            .store()
            .system()
            .get_by_name(mm_model::system::SYSTEM_LAST_ACCESSIBLE_FILE_TIME)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "reading LastAccessibleFileTime failed");
                AppError::boxed(
                    "filterInaccessibleFiles",
                    "app.last_accessible_file.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let Some(value) = stored else {
            return Ok(0);
        };
        let parsed = value.parse::<i64>().map_err(|_| {
            AppError::boxed(
                "filterInaccessibleFiles",
                "app.last_accessible_file.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        tracing::Span::current().record("last_accessible_file_time", parsed);
        Ok(parsed)
    }

    /// Port of `app.App.FilterFilesByChannelPermissions` (file.go:1520).
    ///
    /// Walks `Order`, keeps the files whose channel the caller may read, and answers whether the
    /// caller is a **member** of every channel it kept. One permission check per channel, cached
    /// across the list; a channel the lookup did not return — or a file with an empty
    /// `ChannelId` — is not readable. A `GetChannels` 404 is tolerated; any other failure is
    /// returned. The ABAC download check past the read check is `true` here — see the module
    /// docs — so `allowed` is the read permission alone.
    async fn filter_files_by_channel_permissions(
        &self,
        list: &mut FileInfoList,
        user_id: &str,
    ) -> AppResult<bool> {
        if list.file_infos.as_ref().is_none_or(FileInfoMap::is_empty) {
            return Ok(true);
        }

        let mut channels: BTreeMap<String, Option<Channel>> = BTreeMap::new();
        for info in list.file_infos.iter().flatten().map(|(_, info)| info) {
            if !info.channel_id.is_empty() {
                channels.insert(info.channel_id.clone(), None);
            }
        }

        if !channels.is_empty() {
            let channel_ids: Vec<String> = channels.keys().cloned().collect();
            match self.get_channels(&channel_ids).await {
                Ok(found) => {
                    for channel in found {
                        channels.insert(channel.id.clone(), Some(channel));
                    }
                }
                Err(err) if err.status_code == 404 => {}
                Err(err) => return Err(err),
            }
        }

        let mut channel_permission: HashMap<String, bool> = HashMap::new();
        let mut filtered_files: FileInfoMap = FileInfoMap::new();
        let mut filtered_order = Vec::new();
        let mut all_files_have_membership = true;

        // Moved out rather than cloned: both are replaced below, as Go replaces them.
        let order = list.order.take().unwrap_or_default();
        let mut files = list.file_infos.take().unwrap_or_default();

        for file_id in order {
            let Some(channel_id) = files.get(&file_id).map(|info| info.channel_id.as_str()) else {
                continue;
            };

            if !channel_permission.contains_key(channel_id) {
                let (allowed, is_member) = match channels.get(channel_id).and_then(Option::as_ref) {
                    Some(channel) => self.has_permission_to_read_channel(user_id, channel).await,
                    None => (false, true),
                };
                if allowed {
                    all_files_have_membership = all_files_have_membership && is_member;
                }
                channel_permission.insert(channel_id.to_owned(), allowed);
            }

            if channel_permission.get(channel_id).copied().unwrap_or(false) {
                if let Some(info) = files.remove(&file_id) {
                    let info: FileInfo = info;
                    filtered_files.insert(file_id.clone(), info);
                    filtered_order.push(file_id);
                }
            }
        }

        list.file_infos = Some(filtered_files);
        list.order = Some(filtered_order);

        Ok(all_files_have_membership)
    }
}
