//! Port of the read side of `server/channels/app/file.go` — `GetFileInfo` and the mini-preview
//! guard every file read shares.
//!
//! # Nothing here touches a file
//!
//! The routes served from this module return the `FileInfo` **row**, never bytes.
//! `GET /files/{file_id}`, `/thumbnail`, `/preview` and `/link` all read the file backend and
//! are unregistered, so they stay Go's.

use mm_model::file_info::FileInfo;
use mm_model::utils::AppError;
use mm_store::file_info_store::FileInfoStore;

use crate::App;
use crate::post::PrepareError;

impl App {
    /// Port of `app.App.GetFileInfo` (app/file.go:1310) and the `Server.getFileInfo`
    /// (app/file.go:1295) it delegates to.
    ///
    /// # Three stages, and only the first one survives the port
    ///
    /// 1. **The store read.** Both its branches carry the *same* error id,
    ///    `app.file_info.get.app_error`, and differ only in status: 404 for a miss, 500 for a
    ///    query failure. A client that branches on the id cannot tell them apart, which is why
    ///    the status is asserted rather than the id alone.
    /// 2. **`isInaccessibleFile`** returns `app.file.cloud.get.app_error` (403) for a file past
    ///    a cloud plan's file limit. `GetLastAccessibleFileTime` reads a system value that only
    ///    a licence carrying a `Files` limit ever writes, so on this deployment it is `0` and
    ///    the branch cannot fire. Not reproduced — the same treatment, and for the same reason,
    ///    as `First-Inaccessible-Post-Time` in `mm_api::posts`.
    /// 3. **`generateMiniPreview`** is a *write*: for an image with no stored preview it reads
    ///    the original out of the file backend, encodes a thumbnail, returns it **and upserts
    ///    it into the row**. This port has no file backend, so the request is refused with
    ///    [`PrepareError::Unreproducible`] and the handler forwards it. See
    ///    [`App::mini_preview_would_be_generated`] for how narrow that is.
    #[tracing::instrument(skip(self), fields(file_id = %file_id))]
    pub async fn get_file_info(&self, file_id: &str) -> Result<FileInfo, PrepareError> {
        let info = self
            .store()
            .file_info()
            .get(file_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetFileInfo",
                        "app.file_info.get.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "file info lookup failed");
                    AppError::boxed(
                        "GetFileInfo",
                        "app.file_info.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
            .map_err(PrepareError::App)?;

        if Self::mini_preview_would_be_generated(&info) {
            return Err(PrepareError::Unreproducible(
                "generateMiniPreview reads the file backend and writes the row back",
            ));
        }

        Ok(info)
    }

    /// The guard on `generateMiniPreview` (app/file.go:1253), lifted out so both file reads use
    /// one copy of it.
    ///
    /// **All three conditions have to hold**, and the third is what makes the branch rare: the
    /// upload path already generates a preview for every image it accepts (app/file.go:1016), so
    /// a NULL `MiniPreview` on an image means a row written before that code, by a plugin, or by
    /// a direct `INSERT`. An SVG is excluded because it has no raster to sample.
    ///
    /// When the branch *does* fire, Go's own answer depends on whether the file is still in the
    /// backend: present, it returns a freshly encoded preview and persists it; missing, it logs
    /// at debug and returns `mini_preview: null` — which is what we would have returned anyway.
    /// We cannot tell those apart without the backend, so both are forwarded.
    pub(crate) fn mini_preview_would_be_generated(info: &FileInfo) -> bool {
        info.is_image() && !info.is_svg() && info.mini_preview.is_none()
    }
}

/// Port of `app.App.HasPermissionToFileAction` (app/authorization.go:741).
///
/// **Always `true` on this deployment, and the first line of Go's own function is why**:
/// `a.Srv().Channels().AccessControl` is nil unless the attribute-based access control service
/// is registered, and that registration lives in the out-of-scope `enterprise/` tree. The two
/// config gates below it — `AccessControlSettings.EnableAttributeBasedAccessControl` (default
/// `false`, config.go:4090) and `FeatureFlags.PermissionPolicies` — are each independently
/// sufficient to return `true` as well, and only the *third* of the three has a default of
/// `true` (feature_flags.go:172).
///
/// Written as a free function returning a constant rather than as a config read, because there
/// is no configuration reachable from Team Edition that makes it return anything else:
/// modelling `EnableAttributeBasedAccessControl` here would suggest that setting it changes our
/// answer, and it does not — it changes Go's only in the presence of a service we cannot run.
///
/// Called at both of Go's call sites so that the day an ABAC evaluator exists, the two gates are
/// already in the right place.
pub fn has_permission_to_file_action() -> bool {
    true
}

/// The 403 both file routes raise when [`has_permission_to_file_action`] denies.
///
/// Unreachable today, for the reason that function gives. Kept because the two call sites spell
/// the `where` differently — `getFileInfo` against `getFileInfosForPost` — and that is the only
/// thing distinguishing two otherwise identical errors in a log.
pub fn abac_denied(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.file.get_file.abac_denied.app_error",
        None,
        String::new(),
        403,
    )
}
