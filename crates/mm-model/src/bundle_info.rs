//! Port of `model/bundle_info.go` — a plugin bundle on disk plus the manifest read from it.
//!
//! # Not ported
//!
//! `WrapLogger` belongs to the logging layer. `BundleInfoForPath` reads the disk, so it lives with
//! the plugin environment as `mm_plugin::environment::bundle_info_for_path`.

use crate::manifest::Manifest;

/// Port of `model.BundleInfo` (bundle_info.go:12). No `json:` tags anywhere.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BundleInfo {
    pub path: String,

    pub manifest: Option<Manifest>,
    pub manifest_path: String,
    /// Go's `error`. A bundle with an unreadable or invalid manifest still produces a
    /// `BundleInfo` — the error travels *with* it rather than replacing it, which is what lets
    /// the System Console show a broken plugin instead of dropping it.
    pub manifest_error: Option<String>,
}

impl BundleInfo {
    /// The plugin id when the manifest parsed, otherwise the bundle path — the same choice
    /// `WrapLogger` makes when deciding which field to log.
    pub fn identifier(&self) -> &str {
        match &self.manifest {
            Some(manifest) => &manifest.id,
            None => &self.path,
        }
    }
}
