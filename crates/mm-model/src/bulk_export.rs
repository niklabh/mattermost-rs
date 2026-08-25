//! Port of `model/bulk_export.go` — the bulk-export option set.
//!
//! `BulkExportOpts` has **no `json:` tags at all**; it is a server-side option bag, never a wire
//! type. It is ported for the export job's sake, not for a route.

/// Port of `model.ExportDataDir` (bulk_export.go:8) — the subdirectory attachments go into.
pub const EXPORT_DATA_DIR: &str = "data";

/// Port of `model.BulkExportOpts` (bulk_export.go:10).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkExportOpts {
    pub include_attachments: bool,
    pub include_profile_pictures: bool,
    pub include_archived_channels: bool,
    pub include_roles_and_schemes: bool,
    pub create_archive: bool,
}
