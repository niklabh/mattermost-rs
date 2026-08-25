//! Port of `model/plugin_on_install_event.go` — handed to a plugin when it is installed.

/// Port of `model.OnInstallEvent` (plugin_on_install_event.go:6). No `json:` tag; the field is
/// the id of the user who performed the install.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnInstallEvent {
    pub user_id: String,
}
