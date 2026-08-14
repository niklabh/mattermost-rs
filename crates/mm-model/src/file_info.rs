//! Port of `server/public/model/file_info.go`.
//!
//! Three fields make this the most wire-sensitive type ported so far:
//!
//! - **`mini_preview` is a `*[]byte`.** Go's `encoding/json` special-cases `[]byte` and emits
//!   base64; serde_json emits an array of numbers. Measured, that is `"AQID"` against
//!   `[1,2,3]`, so the field carries a custom codec ([`go_bytes`]). Go also collapses two of the
//!   three nil-ish states: a nil pointer and a pointer to a nil slice both write `null`, while a
//!   pointer to an *empty* slice writes `""`.
//! - **Four fields carry `json:"-"`** (`path`, `thumbnail_path`, `preview_path`, `content`) and
//!   never reach a client — yet `IsValid` requires `path` to be non-empty. **A `FileInfo`
//!   deserialized straight off the wire is therefore always invalid.** That looks like a port
//!   bug and is not; it is pinned by the `path_empty` oracle case.
//! - **`SanitizeFilename` NFC-normalizes before truncating**, so a decomposed name has a
//!   different length than its composed form and truncates at a different point.
//!
//! Pinned by `fixtures/file_info.json`, `fixtures/get_file_infos_options.json` and
//! `fixtures/behaviour_file_info.json`.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::utils::{AppError, AppResult, etag, get_millis, is_valid_id, new_id};

/// Port of `model.FileinfoSortByCreated` (file_info.go:17). Note the value is the **Go field
/// name** `CreateAt`, not the snake_case JSON key.
pub const FILEINFO_SORT_BY_CREATED: &str = "CreateAt";

/// Port of `model.FileinfoSortBySize` (file_info.go:18).
pub const FILEINFO_SORT_BY_SIZE: &str = "Size";

/// Port of `model.MaxFilenameLength` (file_info.go:23) — Unicode **codepoints**, matching the
/// `VARCHAR(256)` width of the `fileinfo.name` column.
pub const MAX_FILENAME_LENGTH: usize = 256;

/// The two magic `creator_id` values [`FileInfo::is_valid`] accepts besides a real id.
///
/// `"nouser"` is a literal in file_info.go:124; `BookmarkFileOwner` is borrowed from
/// `channel_bookmark.go:19` and recorded by the oracle so the borrow cannot drift (D-005).
pub const FILE_OWNER_NO_USER: &str = "nouser";
/// Port of `model.BookmarkFileOwner` (channel_bookmark.go:19).
pub const BOOKMARK_FILE_OWNER: &str = "bookmark";

/// Port of `model.FileDownloadType` (file_info.go:27).
///
/// Go declares a defined string type, so `json.Unmarshal` accepts any value into it. Kept as
/// `&str` constants for the same reason `Channel.Type` stays a `String`: a Rust enum would turn
/// a forward-compatible read into a parse failure the moment a newer Go server writes a new
/// variant.
pub const FILE_DOWNLOAD_TYPE_FILE: &str = "file";
/// Port of `model.FileDownloadTypeThumbnail` (file_info.go:33).
pub const FILE_DOWNLOAD_TYPE_THUMBNAIL: &str = "thumbnail";
/// Port of `model.FileDownloadTypePreview` (file_info.go:35).
pub const FILE_DOWNLOAD_TYPE_PREVIEW: &str = "preview";
/// Port of `model.FileDownloadTypePublic` (file_info.go:37) — unauthenticated public link.
pub const FILE_DOWNLOAD_TYPE_PUBLIC: &str = "public";

/// Go's `encoding/json` treatment of `*[]byte`: base64 (standard alphabet, padded), with `null`
/// for a nil pointer **and** for a pointer to a nil slice.
///
/// serde_json would otherwise write `[1,2,3]`. The nil/empty collapse matters too: Go cannot
/// distinguish "no pointer" from "pointer to nil slice" on the wire, so both decode back to
/// `None` and only `""` round-trips as `Some(vec![])`.
mod go_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(bytes) => s.serialize_str(&STANDARD.encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let Some(encoded) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        STANDARD
            .decode(encoded.as_bytes())
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}

/// Port of `model.GetFileInfosOptions` (file_info.go:41) — query options, not a stored type.
///
/// Every field has a plain `json:` tag with no `omitempty`, so all seven keys are always
/// present; the two slices serialise as `null` when nil.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetFileInfosOptions {
    #[serde(rename = "user_ids")]
    pub user_ids: Option<Vec<String>>,

    #[serde(rename = "channel_ids")]
    pub channel_ids: Option<Vec<String>>,

    /// Epoch milliseconds.
    #[serde(rename = "since")]
    pub since: i64,

    #[serde(rename = "include_deleted")]
    pub include_deleted: bool,

    /// One of [`FILEINFO_SORT_BY_CREATED`] / [`FILEINFO_SORT_BY_SIZE`]; empty means created.
    #[serde(rename = "sort_by")]
    pub sort_by: String,

    #[serde(rename = "sort_descending")]
    pub sort_descending: bool,

    #[serde(rename = "only_empty_content")]
    pub only_empty_content: bool,
}

/// Port of `model.FileInfo` (file_info.go:58).
///
/// Field order matches Go's declaration order, which is the order `encoding/json` emits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileInfo {
    #[serde(rename = "id")]
    pub id: String,

    /// The JSON key is `user_id` while the Go field is `CreatorId` — they do not match.
    #[serde(rename = "user_id")]
    pub creator_id: String,

    #[serde(rename = "post_id", skip_serializing_if = "String::is_empty")]
    pub post_id: String,

    /// Denormalized from the post, and *potentially distinct* from the channel the file was
    /// uploaded to — the same file can be attached to a post in another channel, or to none.
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// `json:"-"` — never sent to a client, yet required by [`Self::is_valid`].
    #[serde(skip)]
    pub path: String,

    /// `json:"-"`.
    #[serde(skip)]
    pub thumbnail_path: String,

    /// `json:"-"`.
    #[serde(skip)]
    pub preview_path: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "extension")]
    pub extension: String,

    #[serde(rename = "size")]
    pub size: i64,

    #[serde(rename = "mime_type")]
    pub mime_type: String,

    #[serde(rename = "width", skip_serializing_if = "is_zero_i64")]
    pub width: i64,

    #[serde(rename = "height", skip_serializing_if = "is_zero_i64")]
    pub height: i64,

    #[serde(rename = "has_preview_image", skip_serializing_if = "is_false")]
    pub has_preview_image: bool,

    /// A `*[]byte` in Go — base64 on the wire, `null` when absent. See [`go_bytes`].
    #[serde(rename = "mini_preview", with = "go_bytes")]
    pub mini_preview: Option<Vec<u8>>,

    /// `json:"-"` — extracted document text, kept server-side for search.
    #[serde(skip)]
    pub content: String,

    #[serde(rename = "remote_id")]
    pub remote_id: Option<String>,

    #[serde(rename = "archived")]
    pub archived: bool,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl FileInfo {
    /// Port of `(*FileInfo).PreSave` (file_info.go:101).
    ///
    /// Every step is conditional, which makes it the gentlest `PreSave` in the tree: an id is
    /// minted only when absent, `create_at` only when zero, and `update_at` is **raised to
    /// `create_at` only if it is behind** — an `update_at` already ahead is left alone. Nothing
    /// reads the clock for `update_at`, unlike `Reaction::pre_save`.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }

        if self.update_at < self.create_at {
            self.update_at = self.create_at;
        }

        if self.remote_id.is_none() {
            self.remote_id = Some(String::new());
        }
    }

    /// Port of `(*FileInfo).IsValid` (file_info.go:119).
    ///
    /// **`path` must be non-empty and `path` is `json:"-"`**, so a `FileInfo` decoded from a
    /// client request can never be valid. That is not a port bug.
    ///
    /// `creator_id` accepts a real id *or* the exact strings `nouser` and `bookmark` — the
    /// comparison is case-sensitive, so `NoUser` fails. `post_id` and `name` are optional but
    /// checked when present. `channel_id` and `delete_at` are never checked at all.
    ///
    /// Only the `id` failure omits its detail; every other one carries `id=`.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if !is_valid_id(&self.creator_id)
            && self.creator_id != FILE_OWNER_NO_USER
            && self.creator_id != BOOKMARK_FILE_OWNER
        {
            return Err(err("user_id", format!("id={}", self.id)));
        }

        if !self.post_id.is_empty() && !is_valid_id(&self.post_id) {
            return Err(err("post_id", format!("id={}", self.id)));
        }

        if self.create_at == 0 {
            return Err(err("create_at", format!("id={}", self.id)));
        }

        if self.update_at == 0 {
            return Err(err("update_at", format!("id={}", self.id)));
        }

        if self.path.is_empty() {
            return Err(err("path", format!("id={}", self.id)));
        }

        if !self.name.is_empty() && !is_valid_filename(&self.name) {
            return Err(err("name", format!("id={}", self.id)));
        }

        Ok(())
    }

    /// Port of `(*FileInfo).IsImage` (file_info.go:205).
    ///
    /// A bare `strings.HasPrefix` on `"image"`, not `"image/"` — so `"images/png"` and even
    /// `"imagex"` count as images, while `" image/png"` does not.
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image")
    }

    /// Port of `(*FileInfo).IsSvg` (file_info.go:209). Exact equality, so a mime type carrying
    /// a `; charset=` parameter is **not** an SVG.
    pub fn is_svg(&self) -> bool {
        self.mime_type == "image/svg+xml"
    }

    /// Port of `(*FileInfo).MakeContentInaccessible` (file_info.go:247).
    ///
    /// Sets `archived` and clears the six fields that could leak content. It does **not** touch
    /// `name`, `size`, `extension` or `mime_type`, so the file stays visible as metadata.
    ///
    /// Go's nil-receiver guard is unrepresentable on `&mut self` and is not ported.
    pub fn make_content_inaccessible(&mut self) {
        self.archived = true;
        self.content = String::new();
        self.has_preview_image = false;
        self.mini_preview = None;
        self.path = String::new();
        self.preview_path = String::new();
        self.thumbnail_path = String::new();
    }
}

/// Port of `model.IsValidFilename` (file_info.go:155).
///
/// Rejects `""`, `"."`, `".."`, anything over [`MAX_FILENAME_LENGTH`] **codepoints**, anything
/// containing `/` or `\`, and any ASCII control character (`< 0x20` or `0x7f`). Note `"..."` is
/// fine — only the two bare dot forms are special. The input is never mutated; see
/// [`sanitize_filename`] for the mutating form, which can return something this still rejects.
pub fn is_valid_filename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.chars().count() > MAX_FILENAME_LENGTH {
        return false;
    }
    if name.contains(['/', '\\']) {
        return false;
    }
    !name.chars().any(|c| c < '\u{20}' || c == '\u{7f}')
}

/// Port of `model.SanitizeFilename` (file_info.go:179).
///
/// In order: NFC-normalize, drop ASCII control characters, map `\` to `/`, reduce to the final
/// path element, reject the three degenerate results, then truncate to
/// [`MAX_FILENAME_LENGTH`] codepoints.
///
/// **The normalization step is load-bearing**, not cosmetic: 200 decomposed `é` are 400
/// codepoints going in and 200 coming out, so a port that skipped it would truncate a different
/// string. Measured — see the oracle's decomposed cases.
///
/// It is a sanitizer, not a validator: `""`, `"."`, `".."`, `"/"` and an all-control-character
/// input all reduce to `""`, which callers must treat as failure since
/// [`is_valid_filename`] rejects it.
///
/// `filepath.Base` is used at file_info.go:192. Go's is platform-sensitive — on Windows it also
/// splits on `\` and drive letters — but the `\`-to-`/` mapping on the line above makes the two
/// agree for this input, and the server is Unix.
pub fn sanitize_filename(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }

    let normalized: String = name.nfc().collect();
    let stripped: String = normalized
        .chars()
        .filter(|c| *c >= '\u{20}' && *c != '\u{7f}')
        .collect();
    let slashed = stripped.replace('\\', "/");

    let base = path_base(&slashed);
    if base == "." || base == ".." || base == "/" {
        return String::new();
    }

    if base.chars().count() > MAX_FILENAME_LENGTH {
        return base.chars().take(MAX_FILENAME_LENGTH).collect();
    }
    base
}

/// Go's `path/filepath.Base` on a Unix host: the last element of the path, with trailing
/// separators stripped first. `""` becomes `"."` and a path of only separators becomes `"/"`.
fn path_base(input: &str) -> String {
    if input.is_empty() {
        return ".".to_string();
    }

    let trimmed = input.trim_end_matches('/');
    if trimmed.is_empty() {
        // The whole string was separators.
        return "/".to_string();
    }

    match trimmed.rsplit_once('/') {
        Some((_, last)) => last.to_string(),
        None => trimmed.to_string(),
    }
}

/// Port of `model.NewInfo` (file_info.go:213), minus the mime lookup.
///
/// Go calls `mime.TypeByExtension`, which consults the **host's** `mime.types` files as well as
/// a small builtin table — measured, this host answered `text/plain; charset=utf-8` for `.txt`
/// and `video/mp4` for `.mp4`, neither of which is builtin. That makes the Go function
/// environment-dependent and not portable in principle, so the mime type is a parameter here
/// and the database decision belongs to the app layer. See D-030.
///
/// The rest is exact: the extension is the final `.`-suffix, **lowercased**, with the leading
/// period stripped because clients expect it that way. `".hidden"` therefore has extension
/// `"hidden"` and no stem, and `"file."` has no extension at all.
pub fn new_info(name: &str, mime_type: impl Into<String>) -> FileInfo {
    FileInfo {
        name: name.to_string(),
        extension: file_extension(name),
        mime_type: mime_type.into(),
        ..Default::default()
    }
}

/// `strings.ToLower(filepath.Ext(name))` with the leading period stripped — the portable half of
/// [`new_info`].
///
/// **`Path::extension` is not a substitute for `filepath.Ext`.** Rust treats a leading dot as
/// the start of a stem, so `".hidden"` has *no* extension; Go simply scans back to the last dot,
/// so it has extension `"hidden"`. Measured — it was the one case that failed when this was
/// first written against `Path::extension`.
pub fn file_extension(name: &str) -> String {
    let ext = crate::utils::go_to_lower(go_filepath_ext(name));
    ext.strip_prefix('.').unwrap_or(&ext).to_string()
}

/// Port of Go's `path/filepath.Ext` on a Unix host (file_info.go:218 calls it).
///
/// Scans backwards from the end for a `.`, stopping at a path separator, and returns the suffix
/// **including** the dot. `"file."` yields `"."`, which then strips to the empty string.
fn go_filepath_ext(path: &str) -> &str {
    let bytes = path.as_bytes();
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'/' {
            break;
        }
        // `.` is ASCII, so this byte index is always a character boundary.
        if bytes[i] == b'.' {
            return &path[i..];
        }
    }
    ""
}

/// Port of `model.GetEtagForFileInfos` (file_info.go:231).
///
/// An empty or absent list yields a bare [`etag`] — just the version, no components. Otherwise
/// the etag pairs **`infos[0].post_id`** with the **maximum `update_at` across the whole list**,
/// so the two halves can come from different elements. Same shape of trap as the channel-list
/// etags.
///
/// `max_update_at` starts at zero and only rises, so a list whose timestamps are all negative
/// yields `0`.
pub fn get_etag_for_file_infos(infos: &[FileInfo]) -> String {
    let Some(first) = infos.first() else {
        return etag(&[]);
    };

    let mut max_update_at: i64 = 0;
    for info in infos {
        if info.update_at > max_update_at {
            max_update_at = info.update_at;
        }
    }

    etag(&[&first.post_id, &max_update_at])
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "FileInfo.IsValid",
        format!("model.file_info.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid() -> FileInfo {
        FileInfo {
            id: "6bdz674pgq767e4jx75w4pf57a".into(),
            creator_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            post_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
            channel_id: "g1ku9ozj3bhub3hs89bqu1m3gy".into(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            path: "20231114/teams/x/file.txt".into(),
            name: "file.txt".into(),
            extension: "txt".into(),
            size: 1024,
            mime_type: "text/plain".into(),
            remote_id: Some("cluster-a".into()),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_the_generated_fixtures() {
        for raw in [
            include_str!("../../../fixtures/file_info.json"),
            include_str!("../../../fixtures/get_file_infos_options.json"),
        ] {
            let original: Value = serde_json::from_str(raw).unwrap();
            let reserialized = if original.get("mini_preview").is_some() {
                serde_json::to_value(serde_json::from_str::<FileInfo>(raw).unwrap()).unwrap()
            } else {
                serde_json::to_value(serde_json::from_str::<GetFileInfosOptions>(raw).unwrap())
                    .unwrap()
            };
            assert_eq!(reserialized, original);
        }
    }

    #[test]
    fn mini_preview_is_base64_not_an_array() {
        let mut fi = FileInfo {
            mini_preview: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(serde_json::to_value(&fi).unwrap()["mini_preview"], "AQID");

        // An empty slice is `""`, which is distinct from absent.
        fi.mini_preview = Some(Vec::new());
        assert_eq!(serde_json::to_value(&fi).unwrap()["mini_preview"], "");

        fi.mini_preview = None;
        assert_eq!(
            serde_json::to_value(&fi).unwrap()["mini_preview"],
            Value::Null
        );

        // ...and it decodes back.
        let parsed: FileInfo = serde_json::from_str(r#"{"mini_preview":"AQID"}"#).unwrap();
        assert_eq!(parsed.mini_preview, Some(vec![1, 2, 3]));
    }

    #[test]
    fn the_hidden_fields_never_reach_the_wire() {
        let fi = FileInfo {
            path: "/a/b".into(),
            thumbnail_path: "/a/t".into(),
            preview_path: "/a/p".into(),
            content: "extracted".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&fi).unwrap();
        for key in ["path", "thumbnail_path", "preview_path", "content"] {
            assert!(json.get(key).is_none(), "{key} leaked onto the wire");
        }
    }

    #[test]
    fn a_file_info_decoded_from_the_wire_is_always_invalid() {
        // path is required by IsValid and carries json:"-", so it cannot survive a round trip.
        let encoded = serde_json::to_string(&valid()).unwrap();
        let decoded: FileInfo = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.path, "");
        assert_eq!(
            decoded.is_valid().unwrap_err().id,
            "model.file_info.is_valid.path.app_error"
        );
    }

    #[test]
    fn creator_id_accepts_two_magic_strings_case_sensitively() {
        let mut fi = valid();
        for owner in [FILE_OWNER_NO_USER, BOOKMARK_FILE_OWNER] {
            fi.creator_id = owner.into();
            fi.is_valid().unwrap();
        }
        fi.creator_id = "NoUser".into();
        assert!(fi.is_valid().is_err());
    }

    #[test]
    fn the_filename_limit_is_codepoints_not_bytes() {
        assert!(is_valid_filename(&"é".repeat(MAX_FILENAME_LENGTH)));
        assert!(!is_valid_filename(&"é".repeat(MAX_FILENAME_LENGTH + 1)));
        // Only the two bare dot forms are special.
        assert!(!is_valid_filename("."));
        assert!(!is_valid_filename(".."));
        assert!(is_valid_filename("..."));
    }

    #[test]
    fn sanitize_normalizes_before_truncating() {
        // 200 decomposed "e + combining acute" is 400 codepoints in and 200 out, so without NFC
        // this would truncate mid-string and yield a different name.
        let decomposed = "e\u{301}".repeat(200);
        assert_eq!(decomposed.chars().count(), 400);
        let out = sanitize_filename(&decomposed);
        assert_eq!(out.chars().count(), 200);
        assert_eq!(out, "\u{e9}".repeat(200));
    }

    #[test]
    fn sanitize_is_not_validate() {
        // Everything that reduces to nothing usable.
        for input in ["", ".", "..", "/", "//", "\\", "\u{7f}"] {
            let out = sanitize_filename(input);
            assert_eq!(out, "", "input {input:?}");
            assert!(!is_valid_filename(&out));
        }
        // ...and the path-stripping cases, which do produce something.
        assert_eq!(sanitize_filename("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_filename(r"a\b\c.txt"), "c.txt");
        assert_eq!(sanitize_filename("dir/../file.txt"), "file.txt");
        assert_eq!(sanitize_filename("a\x01b"), "ab");
    }

    #[test]
    fn is_image_matches_a_bare_prefix() {
        let probe = |mime: &str| FileInfo {
            mime_type: mime.into(),
            ..Default::default()
        };
        // Not "image/" — the prefix is the five letters.
        assert!(probe("images/png").is_image());
        assert!(probe("imagex").is_image());
        assert!(!probe(" image/png").is_image());
        // IsSvg is exact equality, so a charset parameter defeats it.
        assert!(probe("image/svg+xml").is_svg());
        assert!(!probe("image/svg+xml; charset=utf-8").is_svg());
    }

    #[test]
    fn pre_save_only_raises_update_at_when_it_is_behind() {
        let mut fi = valid();
        fi.update_at = 1_800_000_000_000;
        fi.pre_save();
        assert_eq!(
            fi.update_at, 1_800_000_000_000,
            "an ahead update_at is left alone"
        );

        fi.update_at = 1_600_000_000_000;
        fi.pre_save();
        assert_eq!(fi.update_at, fi.create_at);
    }

    #[test]
    fn make_content_inaccessible_keeps_the_metadata() {
        let mut fi = valid();
        fi.mini_preview = Some(vec![1, 2, 3]);
        fi.has_preview_image = true;
        fi.content = "extracted".into();
        fi.make_content_inaccessible();

        assert!(fi.archived);
        assert_eq!(fi.content, "");
        assert!(!fi.has_preview_image);
        assert_eq!(fi.mini_preview, None);
        assert_eq!(fi.path, "");
        // Metadata survives.
        assert_eq!(fi.name, "file.txt");
        assert_eq!(fi.size, 1024);
    }

    #[test]
    fn the_extension_is_lowercased_and_loses_its_period() {
        assert_eq!(file_extension("file.PNG"), "png");
        assert_eq!(file_extension("a.tar.gz"), "gz");
        assert_eq!(file_extension("noextension"), "");
        assert_eq!(file_extension("file."), "");
        assert_eq!(file_extension(".hidden"), "hidden");
    }
}

/// Parity tests driven by `fixtures/behaviour_file_info.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_file_info.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        assert_eq!(
            FILEINFO_SORT_BY_CREATED,
            c["sort_by_created"].as_str().unwrap()
        );
        assert_eq!(FILEINFO_SORT_BY_SIZE, c["sort_by_size"].as_str().unwrap());
        assert_eq!(
            MAX_FILENAME_LENGTH as u64,
            c["max_filename_length"].as_u64().unwrap()
        );
        assert_eq!(
            FILE_DOWNLOAD_TYPE_FILE,
            c["download_type_file"].as_str().unwrap()
        );
        assert_eq!(
            FILE_DOWNLOAD_TYPE_THUMBNAIL,
            c["download_type_thumbnail"].as_str().unwrap()
        );
        assert_eq!(
            FILE_DOWNLOAD_TYPE_PREVIEW,
            c["download_type_preview"].as_str().unwrap()
        );
        assert_eq!(
            FILE_DOWNLOAD_TYPE_PUBLIC,
            c["download_type_public"].as_str().unwrap()
        );
        // The cross-file borrow from channel_bookmark.go.
        assert_eq!(
            BOOKMARK_FILE_OWNER,
            c["bookmark_file_owner"].as_str().unwrap()
        );
    }

    /// Byte-for-byte, because base64, the null/empty collapse, the omitempty fields and the
    /// field *order* are all being asserted at once.
    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let want = case["json"].as_str().unwrap();
            // Rebuild the value from Go's own bytes, then re-emit it.
            let parsed: FileInfo = serde_json::from_str(want).unwrap();
            assert_eq!(serde_json::to_string(&parsed).unwrap(), want, "case {name}");
        }
    }

    /// Go's nil pointer and pointer-to-nil-slice both write `null`, so only two of the three
    /// states survive a round trip. Asserted explicitly rather than left implicit.
    #[test]
    fn the_three_mini_preview_states_collapse_to_two() {
        let oracle = oracle();
        let by_name = |want: &str| -> String {
            oracle["wire"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == want)
                .unwrap()["json"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            by_name("mini_preview_nil_pointer"),
            by_name("mini_preview_pointer_to_nil_slice")
        );
        assert_ne!(
            by_name("mini_preview_nil_pointer"),
            by_name("mini_preview_pointer_to_empty_slice")
        );
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut fi: FileInfo = serde_json::from_value(case["file_info"].clone()).unwrap();
            // path carries json:"-", so the oracle records it out of band.
            fi.path = case["path"].as_str().unwrap().to_string();

            let want = case["error_id"].as_str().unwrap();
            match fi.is_valid() {
                Ok(()) => assert!(want.is_empty(), "case {name}: valid, Go returned {want}"),
                Err(e) => {
                    assert_eq!(e.id, want, "case {name}");
                    assert_eq!(
                        e.detailed_error,
                        case["detailed"].as_str().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(e.status_code, 400, "case {name}");
                }
            }
        }
    }

    #[test]
    fn is_valid_filename_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid_filename"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (name, want) in cases {
            assert_eq!(
                is_valid_filename(name),
                want.as_bool().unwrap(),
                "name {name:?}"
            );
        }
    }

    #[test]
    fn sanitize_filename_matches_go() {
        let oracle = oracle();
        let cases = oracle["sanitize_filename"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["in"].as_str().unwrap();
            let out = sanitize_filename(input);
            assert_eq!(out, case["out"].as_str().unwrap(), "input {input:?}");
            assert_eq!(
                is_valid_filename(&out),
                case["out_valid"].as_bool().unwrap(),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let in_id = case["in_id"].as_str().unwrap();
            let in_create = case["in_create_at"].as_i64().unwrap();
            let in_update = case["in_update_at"].as_i64().unwrap();

            let mut fi = FileInfo {
                id: in_id.to_string(),
                create_at: in_create,
                update_at: in_update,
                remote_id: if case["in_remote_nil"].as_bool().unwrap() {
                    None
                } else {
                    Some("cluster-a".into())
                },
                ..Default::default()
            };
            fi.pre_save();

            assert_eq!(
                !in_id.is_empty() && fi.id == in_id,
                case["id_preserved"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                in_id.is_empty() && fi.id.len() == 26,
                case["id_generated"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                in_create != 0 && fi.create_at == in_create,
                case["create_at_preserved"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                fi.update_at != in_update,
                case["update_at_raised"].as_bool().unwrap(),
                "case {name}"
            );
            // out_update_at is only meaningful when create_at was not clock-derived.
            if in_create != 0 {
                assert_eq!(
                    fi.update_at,
                    case["out_update_at"].as_i64().unwrap(),
                    "case {name}"
                );
            }
            assert_eq!(
                fi.remote_id.is_none(),
                case["out_remote_nil"].as_bool().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn is_image_and_is_svg_match_go() {
        let oracle = oracle();
        let cases = oracle["is_image"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let mime = case["mime_type"].as_str().unwrap();
            let fi = FileInfo {
                mime_type: mime.to_string(),
                ..Default::default()
            };
            assert_eq!(
                fi.is_image(),
                case["is_image"].as_bool().unwrap(),
                "mime {mime:?}"
            );
            assert_eq!(
                fi.is_svg(),
                case["is_svg"].as_bool().unwrap(),
                "mime {mime:?}"
            );
        }
    }

    /// Only the **portable** half of `NewInfo` is asserted. `mime_type` comes from the host's
    /// `mime.types` files — this host answered `text/plain; charset=utf-8` for `.txt`, which is
    /// not in Go's builtin table — so the fixture's mime column is evidence for D-030, not a
    /// target to match.
    #[test]
    fn new_info_name_and_extension_match_go() {
        let oracle = oracle();
        let cases = oracle["new_info"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["in"].as_str().unwrap();
            let info = new_info(input, "");
            assert_eq!(info.name, case["name"].as_str().unwrap(), "input {input:?}");
            assert_eq!(
                info.extension,
                case["extension"].as_str().unwrap(),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn get_etag_for_file_infos_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_etag_for_file_infos"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let infos: Vec<FileInfo> = case["infos"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| serde_json::from_value(v.clone()).unwrap())
                .collect();
            assert_eq!(
                get_etag_for_file_infos(&infos),
                case["out"].as_str().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn make_content_inaccessible_matches_go() {
        let oracle = oracle();
        let want = &oracle["make_content_inaccessible"];

        let mut fi = FileInfo {
            id: "6bdz674pgq767e4jx75w4pf57a".into(),
            creator_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            archived: false,
            content: "extracted text".into(),
            has_preview_image: true,
            mini_preview: Some(vec![1, 2, 3]),
            path: "/a/b".into(),
            preview_path: "/a/p".into(),
            thumbnail_path: "/a/t".into(),
            name: "file.txt".into(),
            size: 1024,
            ..Default::default()
        };
        fi.make_content_inaccessible();

        assert_eq!(fi.archived, want["archived"].as_bool().unwrap());
        assert_eq!(fi.content, want["content"].as_str().unwrap());
        assert_eq!(
            fi.has_preview_image,
            want["has_preview_image"].as_bool().unwrap()
        );
        assert_eq!(
            fi.mini_preview.is_none(),
            want["mini_preview_nil"].as_bool().unwrap()
        );
        assert_eq!(fi.path, want["path"].as_str().unwrap());
        assert_eq!(fi.preview_path, want["preview_path"].as_str().unwrap());
        assert_eq!(fi.thumbnail_path, want["thumbnail_path"].as_str().unwrap());
        assert_eq!(fi.name, want["name"].as_str().unwrap());
        assert_eq!(fi.size, want["size"].as_i64().unwrap());
        // And the whole thing still marshals the way Go's does.
        assert_eq!(
            serde_json::to_string(&fi).unwrap(),
            want["json"].as_str().unwrap()
        );
    }
}
