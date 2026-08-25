//! Port of `model/manifest.go` — the plugin manifest and its settings schema.
//!
//! # Two things a reader would get wrong
//!
//! - **`PluginSetting.Hosting` and `PluginSetting.Secret` have no `yaml:` tag.** Every other
//!   field in the file carries a `yaml:` tag mirroring its `json:` tag; these two do not, so a
//!   `plugin.yaml` reaches them only through goccy's default (lowercased field name), not through
//!   `hosting`/`secret` as the JSON side would suggest. Nothing here parses YAML, but the JSON
//!   tags are what this port pins.
//! - **`isValid` is a *lenient* validator with one strict half.** `Version` and
//!   `MinServerVersion` are parsed with `semver.StrictNewVersion` when non-empty — which rejects
//!   `1.2`, `v1.2.3` and `01.2.3` — while `MeetMinServerVersion` parses the *server's* version
//!   with `semver.MustParse`, which accepts all three. The asymmetry is deliberate in Go and is
//!   reproduced here as [`StrictVersion::parse`] versus [`StrictVersion::parse_lenient`].
//!
//! # Not ported
//!
//! `FindManifest` reads `plugin.yml`/`plugin.yaml`/`plugin.json` off disk and needs a YAML
//! parser; it is filesystem plumbing rather than a wire format, and `mm-model` has neither a YAML
//! dependency nor any other filesystem access. `BundleInfoForPath` in `bundle_info.rs` is
//! deferred with it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::serde_helpers::{is_empty_str, is_none, is_none_or_empty_vec};
use crate::utils::{StringInterface, StringMap, is_valid_http_url};

/// Port of `model.MinIdLength` (plugin_valid.go:12).
pub const MIN_ID_LENGTH: usize = 3;
/// Port of `model.MaxIdLength` (plugin_valid.go:13).
pub const MAX_ID_LENGTH: usize = 190;
/// Port of `model.ValidIdRegex` (plugin_valid.go:14).
pub const VALID_ID_REGEX: &str = r"^[a-zA-Z0-9-_\.]+$";

/// Port of `model.IsValidPluginId` (plugin_valid.go:29).
///
/// The bounds are counted in **runes**, not bytes — `utf8.RuneCountInString` — so a 190-character
/// id of two-byte characters is valid while a 191-character ASCII id is not. The regex is ASCII
/// only, so any multi-byte id fails the pattern anyway; the rune count still has to match Go's
/// because the length check runs first and a byte count would reject a shorter string.
///
/// These constraints exist because the id becomes part of a filesystem path.
pub fn is_valid_plugin_id(id: &str) -> bool {
    let runes = id.chars().count();
    if !(MIN_ID_LENGTH..=MAX_ID_LENGTH).contains(&runes) {
        return false;
    }
    // `^[a-zA-Z0-9-_\.]+$` — inlined rather than compiled, since the class is a plain ASCII set
    // and `+` with anchors on both ends is exactly "non-empty and every char is in the set".
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Port of `model.PluginOption` (manifest.go:19).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginOption {
    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "value")]
    pub value: String,
}

/// Port of `model.PluginSettingType` (manifest.go:27) — a Go `int` with `iota`, never tagged, so
/// it is a real enum here. The wire form of a setting's type is the **string** in
/// `PluginSetting.Type`; this enum is only what [`convert_type_to_plugin_setting_type`] produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PluginSettingType {
    /// `iota`, so the zero value.
    #[default]
    Bool,
    Dropdown,
    Generated,
    Radio,
    Text,
    LongText,
    Number,
    Username,
    Custom,
}

/// Port of `model.convertTypeToPluginSettingType` (manifest.go:398).
///
/// Note the case list is **not** in declaration order — Go tests `number` before `longtext` —
/// which changes nothing, but is worth not "fixing" into a difference.
pub fn convert_type_to_plugin_setting_type(t: &str) -> Result<PluginSettingType, ManifestError> {
    match t {
        "bool" => Ok(PluginSettingType::Bool),
        "dropdown" => Ok(PluginSettingType::Dropdown),
        "generated" => Ok(PluginSettingType::Generated),
        "radio" => Ok(PluginSettingType::Radio),
        "text" => Ok(PluginSettingType::Text),
        "number" => Ok(PluginSettingType::Number),
        "longtext" => Ok(PluginSettingType::LongText),
        "username" => Ok(PluginSettingType::Username),
        "custom" => Ok(PluginSettingType::Custom),
        other => Err(ManifestError::InvalidSettingType(other.to_string())),
    }
}

/// Port of `model.PluginSetting` (manifest.go:39).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSetting {
    /// The key this setting is stored under in the configuration file.
    #[serde(rename = "key")]
    pub key: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    /// One of `bool`, `dropdown`, `generated`, `radio`, `text`, `longtext`, `number`,
    /// `username`, `custom`. A **string** on the wire — see [`PluginSettingType`].
    #[serde(rename = "type")]
    pub type_: String,

    /// Markdown.
    #[serde(rename = "help_text")]
    pub help_text: String,

    /// Only meaningful for `generated`; `isValid` rejects it on any other type.
    #[serde(rename = "regenerate_help_text", skip_serializing_if = "is_empty_str")]
    pub regenerate_help_text: String,

    #[serde(rename = "placeholder")]
    pub placeholder: String,

    /// Go's bare `any` — a bool, a string or a number depending on `type_`.
    #[serde(rename = "default")]
    pub default: serde_json::Value,

    /// For `radio` and `dropdown` only.
    #[serde(rename = "options", skip_serializing_if = "is_none_or_empty_vec")]
    pub options: Option<Vec<PluginOption>>,

    /// `cloud` or `on-prem`. Enforced client-side only — the plugin must still validate.
    /// No `yaml:` tag in Go, unlike its neighbours.
    #[serde(rename = "hosting")]
    pub hosting: String,

    /// Sanitised out of System Console and API responses when true.
    #[serde(rename = "secret")]
    pub secret: bool,
}

impl PluginSetting {
    /// Port of `(*PluginSetting).isValid` (manifest.go:355). Unexported in Go; the two callers
    /// are both in this file, so it is `pub(crate)` here rather than `pub`.
    pub(crate) fn is_valid(&self) -> Result<(), ManifestError> {
        let setting_type = convert_type_to_plugin_setting_type(&self.type_)?;

        if !self.regenerate_help_text.is_empty() && setting_type != PluginSettingType::Generated {
            return Err(ManifestError::RegenerateHelpTextOnNonGenerated);
        }

        if !self.placeholder.is_empty()
            && !matches!(
                setting_type,
                PluginSettingType::Generated
                    | PluginSettingType::Text
                    | PluginSettingType::LongText
                    | PluginSettingType::Number
                    | PluginSettingType::Username
                    | PluginSettingType::Custom
            )
        {
            return Err(ManifestError::PlaceholderOnWrongType);
        }

        // Go checks `s.Options != nil`, which a *present but empty* list satisfies — so
        // `"options": []` on a `bool` setting is rejected. `Option<Vec<_>>` preserves that:
        // `Some(vec![])` is not `None`.
        if let Some(options) = &self.options {
            if setting_type != PluginSettingType::Radio
                && setting_type != PluginSettingType::Dropdown
            {
                return Err(ManifestError::OptionsOnWrongType);
            }

            for option in options {
                if option.display_name.is_empty() || option.value.is_empty() {
                    return Err(ManifestError::EmptyOption);
                }
            }
        }

        Ok(())
    }
}

/// Port of `model.PluginSettingsSection` (manifest.go:94).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettingsSection {
    #[serde(rename = "key")]
    pub key: String,

    #[serde(rename = "title")]
    pub title: String,

    #[serde(rename = "subtitle")]
    pub subtitle: String,

    #[serde(rename = "settings")]
    pub settings: Option<Vec<PluginSetting>>,

    /// Markdown, rendered above the settings.
    #[serde(rename = "header")]
    pub header: String,

    /// Markdown, rendered below the settings.
    #[serde(rename = "footer")]
    pub footer: String,

    /// Loads the component registered with `registry.registerAdminConsoleCustomSection`.
    #[serde(rename = "custom")]
    pub custom: bool,

    /// With `custom`, still renders the declared settings as a fallback while the plugin is
    /// disabled — unless the individual setting is itself `custom`.
    #[serde(rename = "fallback")]
    pub fallback: bool,
}

impl PluginSettingsSection {
    /// Port of `(*PluginSettingsSection).IsValid` (manifest.go:340).
    pub fn is_valid(&self) -> Result<(), ManifestError> {
        if self.key.is_empty() {
            return Err(ManifestError::EmptySectionKey);
        }

        for setting in self.settings.iter().flatten() {
            setting.is_valid()?;
        }

        Ok(())
    }
}

/// Port of `model.PluginSettingsSchema` (manifest.go:120).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettingsSchema {
    #[serde(rename = "header")]
    pub header: String,

    #[serde(rename = "footer")]
    pub footer: String,

    #[serde(rename = "settings")]
    pub settings: Option<Vec<PluginSetting>>,

    #[serde(rename = "sections")]
    pub sections: Option<Vec<PluginSettingsSection>>,
}

impl PluginSettingsSchema {
    /// Port of `(*PluginSettingsSchema).isValid` (manifest.go:324).
    pub(crate) fn is_valid(&self) -> Result<(), ManifestError> {
        for setting in self.settings.iter().flatten() {
            setting.is_valid()?;
        }

        for section in self.sections.iter().flatten() {
            section.is_valid()?;
        }

        Ok(())
    }
}

/// Port of `model.ManifestServer` (manifest.go:216).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestServer {
    /// Keyed `"<goos>-<goarch>"`, e.g. `linux-amd64`.
    #[serde(rename = "executables", skip_serializing_if = "Option::is_none")]
    pub executables: Option<StringMap>,

    /// The single-platform fallback. On Windows it must end in `.exe`.
    #[serde(rename = "executable")]
    pub executable: String,
}

/// Port of `model.ManifestWebapp` (manifest.go:229).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestWebapp {
    #[serde(rename = "bundle_path")]
    pub bundle_path: String,

    /// `json:"-"` — the 64-bit FNV-1a hash of the bundle, computed at load time and never sent.
    /// It is still *read* on the wire path: [`Manifest::client_manifest`] hex-formats it into
    /// `bundle_path`.
    #[serde(skip)]
    pub bundle_hash: Vec<u8>,
}

/// Port of `model.Manifest` (manifest.go:174) — the metadata required to load and present a
/// plugin, read from `plugin.json` or `plugin.yaml` at the top of the bundle.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// Globally unique; 3–190 characters matching `^[a-zA-Z0-9-_\.]+$`. Reverse-DNS is
    /// conventional. Becomes part of a filesystem path — see [`is_valid_plugin_id`].
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "description", skip_serializing_if = "is_empty_str")]
    pub description: String,

    #[serde(rename = "homepage_url", skip_serializing_if = "is_empty_str")]
    pub homepage_url: String,

    #[serde(rename = "support_url", skip_serializing_if = "is_empty_str")]
    pub support_url: String,

    #[serde(rename = "release_notes_url", skip_serializing_if = "is_empty_str")]
    pub release_notes_url: String,

    /// Relative path to the plugin's SVG icon for the Marketplace. Bitmaps are not supported.
    #[serde(rename = "icon_path", skip_serializing_if = "is_empty_str")]
    pub icon_path: String,

    #[serde(rename = "version")]
    pub version: String,

    /// Minimum server version, e.g. `5.6.0`. Validated **strictly**.
    #[serde(rename = "min_server_version", skip_serializing_if = "is_empty_str")]
    pub min_server_version: String,

    #[serde(rename = "server", skip_serializing_if = "is_none")]
    pub server: Option<ManifestServer>,

    #[serde(rename = "webapp", skip_serializing_if = "is_none")]
    pub webapp: Option<ManifestWebapp>,

    #[serde(rename = "settings_schema", skip_serializing_if = "is_none")]
    pub settings_schema: Option<PluginSettingsSchema>,

    /// Arbitrary data other plugins can read.
    #[serde(rename = "props", skip_serializing_if = "is_none")]
    pub props: Option<StringInterface>,
}

impl Manifest {
    /// Port of `(*Manifest).HasClient` (manifest.go:239).
    pub fn has_client(&self) -> bool {
        self.webapp.is_some()
    }

    /// Port of `(*Manifest).HasServer` (manifest.go:281).
    pub fn has_server(&self) -> bool {
        self.server.is_some()
    }

    /// Port of `(*Manifest).HasWebapp` (manifest.go:285).
    ///
    /// Identical to [`has_client`](Self::has_client) in Go too — both are `m.Webapp != nil`.
    pub fn has_webapp(&self) -> bool {
        self.webapp.is_some()
    }

    /// Port of `(*Manifest).ClientManifest` (manifest.go:243) — the manifest as sent to the web
    /// app: no description, no server section, and a `bundle_path` rewritten to the static route.
    ///
    /// Go's `cm.Name = m.Name` line is a no-op after the struct copy and is not reproduced.
    /// `%x` over the hash is lowercase hex with no separator, and an **empty** hash formats as
    /// the empty string — so a manifest whose bundle has not been hashed yet yields
    /// `/static/<id>/<id>__bundle.js`, with two underscores. That is Go's output, not a bug here.
    pub fn client_manifest(&self) -> Manifest {
        let mut cm = self.clone();
        cm.description = String::new();
        cm.server = None;
        if let Some(webapp) = &mut cm.webapp {
            let hex: String = webapp
                .bundle_hash
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            webapp.bundle_path = format!("/static/{id}/{id}_{hex}_bundle.js", id = self.id);
        }
        cm
    }

    /// Port of `(*Manifest).GetExecutableForRuntime` (manifest.go:263).
    ///
    /// Falls back to `Executable` whenever the platform map is absent, empty, or has no entry for
    /// `<go_os>-<go_arch>`. Go does not check that the result can actually run here, and neither
    /// does this.
    pub fn get_executable_for_runtime(&self, go_os: &str, go_arch: &str) -> &str {
        let Some(server) = &self.server else {
            return "";
        };

        let mut executable = "";
        if let Some(executables) = &server.executables {
            if !executables.is_empty() {
                let os_arch = format!("{go_os}-{go_arch}");
                executable = executables.get(&os_arch).map_or("", String::as_str);
            }
        }

        if executable.is_empty() {
            executable = &server.executable;
        }

        executable
    }

    /// Port of `(*Manifest).MeetMinServerVersion` (manifest.go:289).
    ///
    /// Asymmetric on purpose, matching Go: the manifest's own `MinServerVersion` is parsed
    /// **strictly**, while `server_version` goes through `semver.MustParse`, which is lenient.
    ///
    /// **Divergence:** Go's `MustParse` *panics* on an unparseable server version. A panic in
    /// library code is forbidden here, so an unparseable `server_version` is an `Err` instead —
    /// every input Go survives answers identically.
    pub fn meet_min_server_version(&self, server_version: &str) -> Result<bool, ManifestError> {
        let min_server_version = StrictVersion::parse_detailed(&self.min_server_version)
            .map_err(ManifestError::UnparseableMinServerVersion)?;
        let sv = StrictVersion::parse_lenient(server_version)
            .ok_or_else(|| ManifestError::UnparseableServerVersion(server_version.to_string()))?;
        Ok(sv >= min_server_version)
    }

    /// Port of `(*Manifest).IsValid` (manifest.go:301).
    ///
    /// Order matters and is Go's: id, name, the three URLs, `Version`, `MinServerVersion`, then
    /// the settings schema. `Version` and `MinServerVersion` are checked only when non-empty, so
    /// a manifest with **no** version at all is valid.
    pub fn is_valid(&self) -> Result<(), ManifestError> {
        if !is_valid_plugin_id(&self.id) {
            return Err(ManifestError::InvalidPluginId);
        }

        if self.name.trim().is_empty() {
            return Err(ManifestError::MissingName);
        }

        if !self.homepage_url.is_empty() && !is_valid_http_url(&self.homepage_url) {
            return Err(ManifestError::InvalidHomepageUrl);
        }

        if !self.support_url.is_empty() && !is_valid_http_url(&self.support_url) {
            return Err(ManifestError::InvalidSupportUrl);
        }

        if !self.release_notes_url.is_empty() && !is_valid_http_url(&self.release_notes_url) {
            return Err(ManifestError::InvalidReleaseNotesUrl);
        }

        if !self.version.is_empty() {
            StrictVersion::parse_detailed(&self.version)
                .map_err(ManifestError::UnparseableVersion)?;
        }

        if !self.min_server_version.is_empty() {
            StrictVersion::parse_detailed(&self.min_server_version)
                .map_err(ManifestError::UnparseableMinServerVersion)?;
        }

        if let Some(schema) = &self.settings_schema {
            schema
                .is_valid()
                .map_err(|e| ManifestError::InvalidSettingsSchema(Box::new(e)))?;
        }

        Ok(())
    }
}

/// The error set of `manifest.go`. Go returns bare `errors.New`/`errors.Wrap` values; the text of
/// each variant is the Go string, so a caller that logs it sees what the Go server logged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("invalid plugin ID")]
    InvalidPluginId,
    #[error("a plugin name is needed")]
    MissingName,
    #[error("invalid HomepageURL")]
    InvalidHomepageUrl,
    #[error("invalid SupportURL")]
    InvalidSupportUrl,
    #[error("invalid ReleaseNotesURL")]
    InvalidReleaseNotesUrl,
    /// Go's `errors.Wrap(err, "invalid settings schema")` around every failure the schema's own
    /// validators report — the prefix is part of the message a plugin developer sees.
    #[error("invalid settings schema: {0}")]
    InvalidSettingsSchema(Box<ManifestError>),
    #[error("failed to parse Version: {0}")]
    UnparseableVersion(VersionParseError),
    #[error("failed to parse MinServerVersion: {0}")]
    UnparseableMinServerVersion(VersionParseError),
    /// Not reachable in Go: `semver.MustParse` panics instead. See
    /// [`Manifest::meet_min_server_version`].
    #[error("failed to parse server version: {0}")]
    UnparseableServerVersion(String),
    #[error("invalid setting type: {0}")]
    InvalidSettingType(String),
    #[error("should not set RegenerateHelpText for setting type that is not generated")]
    RegenerateHelpTextOnNonGenerated,
    #[error(
        "should not set Placeholder for setting type not in text, generated, number, username, or custom"
    )]
    PlaceholderOnWrongType,
    #[error("should not set Options for setting type not in radio or dropdown")]
    OptionsOnWrongType,
    #[error("should not have empty Displayname or Value for any option")]
    EmptyOption,
    #[error("invalid empty Key")]
    EmptySectionKey,
}

/// The slice of `Masterminds/semver/v3` this file needs, rather than a new dependency.
///
/// Only two entry points are used in the Go tree here — `StrictNewVersion` and `MustParse` — and
/// only ordering is asked of the result. Precedence follows SemVer 2.0.0 §11: build metadata is
/// ignored, a version with a prerelease sorts **below** the same version without one, and
/// prerelease identifiers compare numerically when both are numeric and ASCII-lexically otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrictVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated identifiers, without the leading `-`. Empty when absent.
    pub prerelease: Vec<String>,
    /// Dot-separated identifiers, without the leading `+`. Ignored by comparison.
    pub build: Vec<String>,
}

/// Why a version string was rejected.
///
/// Masterminds distinguishes seven reasons and `Manifest.IsValid` folds the text straight into
/// the message a plugin developer sees, so the reason is part of the API surface, not a detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VersionParseError {
    #[error("version string empty")]
    EmptyString,
    #[error("version string is too long (max 256 bytes)")]
    TooLong,
    #[error("invalid semantic version")]
    InvalidSemVer,
    #[error("invalid characters in version")]
    InvalidCharacters,
    #[error("version segment starts with 0")]
    SegmentStartsZero,
    #[error("invalid metadata string")]
    InvalidMetadata,
    #[error("invalid prerelease string")]
    InvalidPrerelease,
}

/// `semver.MaxVersionLen`.
const MAX_VERSION_LEN: usize = 256;

impl StrictVersion {
    /// `semver.StrictNewVersion`: all three components required, no `v` prefix, no leading zeros.
    ///
    /// The step order is Masterminds' own and is observable: metadata and prerelease are
    /// validated **before** the numeric segments, so `v1.2.3-01` is rejected for its prerelease
    /// rather than for its `v`.
    pub fn parse_detailed(input: &str) -> Result<Self, VersionParseError> {
        if input.is_empty() {
            return Err(VersionParseError::EmptyString);
        }
        if input.len() > MAX_VERSION_LEN {
            return Err(VersionParseError::TooLong);
        }

        // `SplitN(v, ".", 3)`: the third part keeps any further dots, which is why `1.2.3.4`
        // fails on *characters* rather than on component count.
        let mut split = input.splitn(3, '.');
        let (Some(major_s), Some(minor_s), Some(rest)) = (split.next(), split.next(), split.next())
        else {
            return Err(VersionParseError::InvalidSemVer);
        };

        let (rest, build) = match rest.split_once('+') {
            Some((head, meta)) => (head, Self::validate_metadata(meta)?),
            None => (rest, Vec::new()),
        };
        let (patch_s, prerelease) = match rest.split_once('-') {
            Some((head, pre)) => (head, Self::validate_prerelease(pre)?),
            None => (rest, Vec::new()),
        };

        let mut nums = [0u64; 3];
        for (slot, part) in nums.iter_mut().zip([major_s, minor_s, patch_s]) {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(VersionParseError::InvalidCharacters);
            }
            if part.len() > 1 && part.starts_with('0') {
                return Err(VersionParseError::SegmentStartsZero);
            }
            *slot = part
                .parse()
                .map_err(|_| VersionParseError::InvalidCharacters)?;
        }

        Ok(Self {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
            prerelease,
            build,
        })
    }

    /// `semver.NewVersion`. Since Masterminds v3.5.0 that runs `coerceNewVersion` by default
    /// (`CoerceNewVersion = true`), which is the *loose* regex: a leading `v`, an omitted minor
    /// or patch, and **leading zeros** are all accepted and normalised away. Only the prerelease
    /// and metadata keep their strict rules.
    ///
    /// The leading-zero difference between the two parsers is what makes
    /// [`Manifest::meet_min_server_version`]'s asymmetry observable, and the oracle caught the
    /// port applying the strict rule to both.
    pub fn parse_lenient_detailed(input: &str) -> Result<Self, VersionParseError> {
        if input.len() > MAX_VERSION_LEN {
            return Err(VersionParseError::TooLong);
        }

        // `^v?([0-9]+)(\.[0-9]+)?(\.[0-9]+)?(-…)?(\+…)?$`, hand-rolled: the crate has no regex
        // dependency and the shape is small enough to read.
        let body = input.strip_prefix('v').unwrap_or(input);

        // Neither charset contains `+`, so the first `+` unambiguously starts the metadata.
        let (body, build) = match body.split_once('+') {
            Some((head, meta)) => (head, Some(meta)),
            None => (body, None),
        };
        // The prerelease charset *does* contain `-`, so only the first one is the separator.
        let (core, pre) = match body.split_once('-') {
            Some((head, pre)) => (head, Some(pre)),
            None => (body, None),
        };

        let mut parts = core.split('.');
        let mut nums = [0u64; 3];
        let Some(major) = parts.next() else {
            return Err(VersionParseError::InvalidSemVer);
        };
        nums[0] = Self::loose_numeric(major)?;
        for slot in nums.iter_mut().skip(1) {
            match parts.next() {
                Some(part) => *slot = Self::loose_numeric(part)?,
                None => break,
            }
        }
        if parts.next().is_some() {
            // A fourth component: the loose regex does not match, so it is not a version at all.
            return Err(VersionParseError::InvalidSemVer);
        }

        // Two-stage, because Go's two stages report different errors: a malformed prerelease or
        // metadata fails the *regex*, so it is `invalid semantic version`, and only the
        // leading-zero rule — checked afterwards by `validatePrerelease` — reports itself.
        let prerelease = match pre {
            Some(pre) => {
                Self::loose_identifiers(pre)?;
                Self::validate_prerelease(pre)?
            }
            None => Vec::new(),
        };
        let build = match build {
            Some(meta) => {
                Self::loose_identifiers(meta)?;
                Self::validate_metadata(meta)?
            }
            None => Vec::new(),
        };

        Ok(Self {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
            prerelease,
            build,
        })
    }

    /// `semver.StrictNewVersion`, as an [`Option`] for the callers that only branch on success.
    pub fn parse(input: &str) -> Option<Self> {
        Self::parse_detailed(input).ok()
    }

    /// `semver.NewVersion`, as an [`Option`].
    pub fn parse_lenient(input: &str) -> Option<Self> {
        Self::parse_lenient_detailed(input).ok()
    }

    /// A loose numeric segment: digits only, leading zeros allowed and normalised away. An empty
    /// or non-numeric segment means the loose regex did not match at all.
    fn loose_numeric(part: &str) -> Result<u64, VersionParseError> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(VersionParseError::InvalidSemVer);
        }
        part.parse().map_err(|_| VersionParseError::InvalidSemVer)
    }

    /// The `([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)` shape both the prerelease and the metadata group
    /// have in the loose regex. Failing it means the regex did not match the version at all.
    fn loose_identifiers(s: &str) -> Result<(), VersionParseError> {
        for id in s.split('.') {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(VersionParseError::InvalidSemVer);
            }
        }
        Ok(())
    }

    /// `semver.validatePrerelease`: dot-separated identifiers of `[0-9A-Za-z-]`, where a purely
    /// numeric identifier may not have a leading zero — and *that* failure reports
    /// [`VersionParseError::SegmentStartsZero`], not an invalid prerelease.
    fn validate_prerelease(pre: &str) -> Result<Vec<String>, VersionParseError> {
        let mut out = Vec::new();
        for id in pre.split('.') {
            if id.is_empty() {
                return Err(VersionParseError::InvalidPrerelease);
            }
            if id.bytes().all(|b| b.is_ascii_digit()) {
                if id.len() > 1 && id.starts_with('0') {
                    return Err(VersionParseError::SegmentStartsZero);
                }
            } else if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(VersionParseError::InvalidPrerelease);
            }
            out.push(id.to_string());
        }
        Ok(out)
    }

    /// `semver.validateMetadata`: the same identifiers, with no leading-zero rule.
    fn validate_metadata(meta: &str) -> Result<Vec<String>, VersionParseError> {
        let mut out = Vec::new();
        for id in meta.split('.') {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(VersionParseError::InvalidMetadata);
            }
            out.push(id.to_string());
        }
        Ok(out)
    }

    /// SemVer §11 identifier comparison.
    fn compare_prerelease(a: &[String], b: &[String]) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (a.is_empty(), b.is_empty()) {
            (true, true) => return Ordering::Equal,
            // A version *with* a prerelease has lower precedence than one without.
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }

        for (x, y) in a.iter().zip(b.iter()) {
            let x_num = x.bytes().all(|c| c.is_ascii_digit());
            let y_num = y.bytes().all(|c| c.is_ascii_digit());
            let ord = match (x_num, y_num) {
                (true, true) => x
                    .parse::<u64>()
                    .unwrap_or(0)
                    .cmp(&y.parse::<u64>().unwrap_or(0)),
                // Numeric identifiers always have lower precedence than alphanumeric ones.
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => x.cmp(y),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }

        a.len().cmp(&b.len())
    }
}

impl PartialOrd for StrictVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StrictVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| Self::compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

impl std::fmt::Display for StrictVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            write!(f, "-{}", self.prerelease.join("."))?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}

/// Port of `model.PluginInfo` (plugins_response.go:6) — a `Manifest` embedded anonymously, which
/// `encoding/json` **inlines**. `#[serde(flatten)]` is the equivalent; a nested `manifest` key
/// would be a wire break.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    #[serde(flatten)]
    pub manifest: Manifest,
}

/// Port of `model.PluginsResponse` (plugins_response.go:10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsResponse {
    #[serde(rename = "active")]
    pub active: Option<Vec<PluginInfo>>,

    #[serde(rename = "inactive")]
    pub inactive: Option<Vec<PluginInfo>>,
}

/// Not a Go type. `BTreeMap` is re-exported here only so callers constructing a
/// [`ManifestServer`] need not reach into `std` for the executables map.
pub type ExecutableMap = BTreeMap<String, String>;

#[cfg(test)]
mod go_parity {
    use super::StrictVersion;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// `Masterminds/semver`'s two constructors, which `Manifest` uses asymmetrically: the
    /// manifest's own versions go through `StrictNewVersion`, the *server's* through the lenient
    /// `MustParse`. The corpus is the same one `access_policy.rs` asserts `x/mod/semver` against,
    /// so the rows where the two libraries disagree — every bare `v` prefix — are visible here.
    #[test]
    fn strict_and_lenient_parsing_match_go() {
        let oracle = oracle();
        let cases = oracle["strict_semver"].as_array().unwrap();
        assert!(cases.len() >= 30);
        for case in cases {
            let input = case["in"].as_str().unwrap();

            let strict = StrictVersion::parse(input);
            assert_eq!(
                strict.is_some(),
                case["strict_ok"].as_bool().unwrap(),
                "StrictNewVersion({input:?})"
            );
            // The rejection *reason*: `Manifest.IsValid` folds it into the message, and the six
            // Masterminds reasons are only distinguishable if the step order matches — metadata
            // and prerelease are validated before the numeric segments.
            if let Some(expected) = case.get("strict_err").and_then(|v| v.as_str()) {
                assert_eq!(
                    StrictVersion::parse_detailed(input)
                        .unwrap_err()
                        .to_string(),
                    expected,
                    "StrictNewVersion({input:?}) reason"
                );
            }
            if let Some(parsed) = &strict {
                assert_eq!(parsed.major, case["strict_major"].as_u64().unwrap());
                assert_eq!(parsed.minor, case["strict_minor"].as_u64().unwrap());
                assert_eq!(parsed.patch, case["strict_patch"].as_u64().unwrap());
                assert_eq!(
                    parsed.prerelease.join("."),
                    case["strict_prerelease"].as_str().unwrap(),
                    "prerelease of {input:?}"
                );
                assert_eq!(
                    parsed.build.join("."),
                    case["strict_metadata"].as_str().unwrap(),
                    "metadata of {input:?}"
                );
                assert_eq!(
                    parsed.to_string(),
                    case["strict_string"].as_str().unwrap(),
                    "String() of {input:?}"
                );
            }

            let lenient = StrictVersion::parse_lenient(input);
            assert_eq!(
                lenient.is_some(),
                case["lenient_ok"].as_bool().unwrap(),
                "NewVersion({input:?})"
            );
            if let Some(expected) = case.get("lenient_err").and_then(|v| v.as_str()) {
                assert_eq!(
                    StrictVersion::parse_lenient_detailed(input)
                        .unwrap_err()
                        .to_string(),
                    expected,
                    "NewVersion({input:?}) reason"
                );
            }
            if let Some(parsed) = &lenient {
                assert_eq!(parsed.major, case["lenient_major"].as_u64().unwrap());
                assert_eq!(parsed.minor, case["lenient_minor"].as_u64().unwrap());
                assert_eq!(parsed.patch, case["lenient_patch"].as_u64().unwrap());
            }
        }
    }

    /// SemVer §11 ordering, which `MeetMinServerVersion` depends on: a prerelease sorts **below**
    /// its release, numeric prerelease identifiers below alphanumeric ones, and build metadata is
    /// ignored entirely.
    #[test]
    fn version_ordering_matches_go() {
        let oracle = oracle();
        let cases = oracle["strict_semver_cmp"].as_array().unwrap();
        assert!(cases.len() >= 15);
        for case in cases {
            if case.get("error").is_some() {
                continue;
            }
            let a_text = case["a"].as_str().unwrap();
            let b_text = case["b"].as_str().unwrap();
            // The corpus writes them with the `v` the *other* parser needs; Masterminds does not.
            let a = StrictVersion::parse(a_text.trim_start_matches('v')).unwrap();
            let b = StrictVersion::parse(b_text.trim_start_matches('v')).unwrap();

            let want = case["compare"].as_i64().unwrap();
            let got = match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            assert_eq!(got, want, "{a_text}.Compare({b_text})");
            assert_eq!(
                a < b,
                case["a_less_than_b"].as_bool().unwrap(),
                "{a_text}.LessThan({b_text})"
            );
        }
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn plugin_option_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginOption, "plugin_option");
    }
    #[test]
    fn plugin_setting_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginSetting, "plugin_setting");
    }
    #[test]
    fn plugin_settings_section_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginSettingsSection, "plugin_settings_section");
    }
    #[test]
    fn plugin_settings_schema_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginSettingsSchema, "plugin_settings_schema");
    }
    #[test]
    fn manifest_round_trips_the_fixture() {
        assert_fixture_round_trips!(Manifest, "manifest");
    }
    #[test]
    fn manifest_server_round_trips_the_fixture() {
        assert_fixture_round_trips!(ManifestServer, "manifest_server");
    }
    #[test]
    fn manifest_webapp_round_trips_the_fixture() {
        assert_fixture_round_trips!(ManifestWebapp, "manifest_webapp");
    }
    #[test]
    fn plugins_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(PluginsResponse, "plugins_response");
    }
}

#[cfg(test)]
mod sweep_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    fn manifest(mut mutate: impl FnMut(&mut Manifest)) -> Manifest {
        let mut m = Manifest {
            id: "com.example.plugin".to_string(),
            name: "Example".to_string(),
            version: "1.2.3".to_string(),
            ..Default::default()
        };
        mutate(&mut m);
        m
    }

    fn setting(key: &str, type_: &str) -> PluginSetting {
        PluginSetting {
            key: key.to_string(),
            type_: type_.to_string(),
            ..Default::default()
        }
    }

    /// The error **strings** are asserted, not just the fact of failure: `manifest.go` returns
    /// plain `errors.New`/`errors.Wrap` values with no ids, so the message is the only thing a
    /// caller — or an operator reading a log — can match on.
    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["manifest_is_valid"].as_array().unwrap();

        let inputs: Vec<(&str, Manifest)> = vec![
            ("ok", manifest(|_| {})),
            ("no_version", manifest(|m| m.version = String::new())),
            ("loose_version", manifest(|m| m.version = "1.2".to_string())),
            (
                "v_prefixed_version",
                manifest(|m| m.version = "v1.2.3".to_string()),
            ),
            ("short_id", manifest(|m| m.id = "ab".to_string())),
            (
                "bad_id_chars",
                manifest(|m| m.id = "com example".to_string()),
            ),
            ("blank_name", manifest(|m| m.name = "   ".to_string())),
            (
                "bad_homepage",
                manifest(|m| m.homepage_url = "not a url".to_string()),
            ),
            (
                "good_homepage",
                manifest(|m| m.homepage_url = "https://example.com".to_string()),
            ),
            (
                "bad_min_server_version",
                manifest(|m| m.min_server_version = "5.6".to_string()),
            ),
            (
                "good_min_server_version",
                manifest(|m| m.min_server_version = "5.6.0".to_string()),
            ),
            (
                "regenerate_on_text",
                manifest(|m| {
                    let mut s = setting("k", "text");
                    s.regenerate_help_text = "no".to_string();
                    m.settings_schema = Some(PluginSettingsSchema {
                        settings: Some(vec![s]),
                        ..Default::default()
                    });
                }),
            ),
            (
                "placeholder_on_bool",
                manifest(|m| {
                    let mut s = setting("k", "bool");
                    s.placeholder = "no".to_string();
                    m.settings_schema = Some(PluginSettingsSchema {
                        settings: Some(vec![s]),
                        ..Default::default()
                    });
                }),
            ),
            (
                "options_on_bool",
                manifest(|m| {
                    let mut s = setting("k", "bool");
                    s.options = Some(vec![]);
                    m.settings_schema = Some(PluginSettingsSchema {
                        settings: Some(vec![s]),
                        ..Default::default()
                    });
                }),
            ),
            (
                "empty_option",
                manifest(|m| {
                    let mut s = setting("k", "dropdown");
                    s.options = Some(vec![PluginOption {
                        display_name: String::new(),
                        value: "v".to_string(),
                    }]);
                    m.settings_schema = Some(PluginSettingsSchema {
                        settings: Some(vec![s]),
                        ..Default::default()
                    });
                }),
            ),
            (
                "unknown_setting_type",
                manifest(|m| {
                    m.settings_schema = Some(PluginSettingsSchema {
                        settings: Some(vec![setting("k", "colour")]),
                        ..Default::default()
                    });
                }),
            ),
            (
                "section_without_key",
                manifest(|m| {
                    m.settings_schema = Some(PluginSettingsSchema {
                        sections: Some(vec![PluginSettingsSection::default()]),
                        ..Default::default()
                    });
                }),
            ),
        ];

        for (case, (name, m)) in cases.iter().zip(inputs.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            match (m.is_valid(), case.get("error").and_then(|v| v.as_str())) {
                (Ok(()), None) => {}
                (Ok(()), Some(e)) => panic!("{name}: Go rejected with {e:?}, the port accepted"),
                (Err(e), None) => panic!("{name}: Go accepted, the port rejected with {e}"),
                (Err(e), Some(expected)) => assert_eq!(e.to_string(), expected, "{name}"),
            }
        }

        // MeetMinServerVersion parses the manifest's version **strictly** and the server's
        // leniently — the `5.6.0|5.6` and `5.6|5.7.0` rows are the two halves of that.
        for case in &cases[inputs.len()..] {
            let name = case["name"].as_str().unwrap();
            let spec = name.strip_prefix("meet:").expect("meet row");
            let (min, server) = spec.split_once('|').expect("meet row");
            let m = manifest(|m| m.min_server_version = min.to_string());
            match (m.meet_min_server_version(server), case.get("error")) {
                (Ok(actual), None) => {
                    assert_eq!(actual, case["meets"].as_bool().unwrap(), "{name}")
                }
                (Ok(_), Some(e)) => panic!("{name}: Go failed with {e}, the port succeeded"),
                (Err(e), None) => panic!("{name}: Go succeeded, the port failed with {e}"),
                (Err(e), Some(expected)) => {
                    // Go wraps with `errors.Wrap`, so its message has the cause appended; the
                    // port's message is the prefix Go's starts with.
                    let expected = expected.as_str().unwrap();
                    assert!(
                        e.to_string().starts_with(expected),
                        "{name}: {e:?} should start with {expected:?}"
                    );
                }
            }
        }
    }

    /// The `Executables` map wins only on an **exact** `os-arch` hit with a non-empty value;
    /// everything else falls back to the single `Executable`.
    #[test]
    fn get_executable_for_runtime_matches_go() {
        let oracle = oracle();
        let cases = oracle["manifest_executable"].as_array().unwrap();

        let mut hit = crate::utils::StringMap::new();
        hit.insert("linux-amd64".to_string(), "dist/linux".to_string());
        hit.insert("darwin-arm64".to_string(), "dist/darwin".to_string());
        let mut only_linux = crate::utils::StringMap::new();
        only_linux.insert("linux-amd64".to_string(), "dist/linux".to_string());
        let mut empty_value = crate::utils::StringMap::new();
        empty_value.insert("linux-amd64".to_string(), String::new());

        let inputs: Vec<(&str, Option<ManifestServer>, &str, &str)> = vec![
            ("no_server", None, "linux", "amd64"),
            (
                "single",
                Some(ManifestServer {
                    executable: "plugin".to_string(),
                    ..Default::default()
                }),
                "linux",
                "amd64",
            ),
            (
                "map_hit",
                Some(ManifestServer {
                    executable: "fallback".to_string(),
                    executables: Some(hit),
                }),
                "linux",
                "amd64",
            ),
            (
                "map_miss",
                Some(ManifestServer {
                    executable: "fallback".to_string(),
                    executables: Some(only_linux),
                }),
                "windows",
                "amd64",
            ),
            (
                "map_empty",
                Some(ManifestServer {
                    executable: "fallback".to_string(),
                    executables: Some(crate::utils::StringMap::new()),
                }),
                "linux",
                "amd64",
            ),
            (
                "map_hit_empty_value",
                Some(ManifestServer {
                    executable: "fallback".to_string(),
                    executables: Some(empty_value),
                }),
                "linux",
                "amd64",
            ),
        ];
        assert_eq!(cases.len(), inputs.len());

        for (case, (name, server, go_os, go_arch)) in cases.iter().zip(inputs) {
            assert_eq!(case["name"].as_str().unwrap(), name, "corpus order drifted");
            let m = manifest(|m| m.server = server.clone());
            assert_eq!(
                m.get_executable_for_runtime(go_os, go_arch),
                case["out"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    /// `ClientManifest` is a **filter**, not a copy: it drops the server section and rewrites the
    /// bundle path to the static route, cache-busted by the bundle hash when there is one.
    #[test]
    fn client_manifest_matches_go() {
        let oracle = oracle();
        let cases = oracle["manifest_client"].as_array().unwrap();

        let inputs: Vec<(&str, Manifest)> = vec![
            (
                "no_webapp",
                manifest(|m| m.description = "desc".to_string()),
            ),
            (
                "webapp_no_hash",
                manifest(|m| {
                    m.description = "desc".to_string();
                    m.server = Some(ManifestServer {
                        executable: "plugin".to_string(),
                        ..Default::default()
                    });
                    m.webapp = Some(ManifestWebapp {
                        bundle_path: "webapp/dist/main.js".to_string(),
                        ..Default::default()
                    });
                }),
            ),
            (
                "webapp_with_hash",
                manifest(|m| {
                    m.description = "desc".to_string();
                    m.webapp = Some(ManifestWebapp {
                        bundle_path: "webapp/dist/main.js".to_string(),
                        bundle_hash: vec![0x0a, 0xff, 0x01],
                    });
                }),
            ),
        ];
        assert_eq!(cases.len(), inputs.len());

        for (case, (name, m)) in cases.iter().zip(inputs.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            let client = m.client_manifest();
            assert_eq!(
                client.description,
                case["description"].as_str().unwrap(),
                "{name} description"
            );
            assert_eq!(
                client.server.is_some(),
                case["has_server"].as_bool().unwrap(),
                "{name} server"
            );
            assert_eq!(
                m.has_client(),
                case["has_client"].as_bool().unwrap(),
                "{name}"
            );
            if let Some(expected) = case.get("bundle_path").and_then(|v| v.as_str()) {
                assert_eq!(
                    client.webapp.as_ref().map(|w| w.bundle_path.as_str()),
                    Some(expected),
                    "{name} bundle path"
                );
            }
        }
    }
}
