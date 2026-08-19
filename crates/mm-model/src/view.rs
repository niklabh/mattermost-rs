//! Port of `model/view.go` — the channel **views** (kanban boards) surface.
//!
//! Not to be confused with [`crate::channel_view`], which is `channel_view.go`: the
//! mark-channel-read request. Different file, different feature, similar name.
//!
//! # Five things measured rather than read
//!
//! **1. A kanban view can never validate without props.** `ViewTypeKanban` is the only accepted
//! type, and `validateViewProps` routes every kanban view into `validateKanbanProps`, which
//! rejects a nil map outright. Since `Props` carries `omitempty`, "a view with no props" is an
//! entirely ordinary wire shape that always fails.
//!
//! **2. nil props and empty props give different error ids, and the wire cannot tell them
//! apart.** `nil` gives `…props.kanban_required`; `{}` decodes to a zero `KanbanProps` and gives
//! `…props.kanban_field_id`. But `omitempty` on a Go map drops nil **and** empty, so both
//! serialise to the same document. The distinction is real inbound (a client can send
//! `"props":{}`) and unrecoverable outbound.
//!
//! **3. The title's length check reads the *untrimmed* string.** Emptiness is tested after
//! `TrimSpace` and length before it, so 256 spaces followed by `abc` is both non-empty and over
//! the 256-rune cap.
//!
//! **4. `KanbanPropsFromProps` is a JSON round trip, so a malformed props map fails with
//! `encoding/json`'s own error text** — which then lands in `DetailedError`, on the wire. That
//! text names **Go type names**; see [`KanbanPropsError`], which reproduces it.
//!
//! **5. `Clone` copies the props map but shares its values.** `maps.Copy` is shallow, so a nested
//! map is aliased between original and clone. Ours deep-clones — [D-116], same class as [D-015].
//!
//! # And one thing that is invisible from outside Go
//!
//! The three per-column branches pass `map[string]any{"Index": i}` as the `AppError`'s params.
//! `params` is **unexported** in Go, feeds `Translate` alone, and with no i18n bundle registered
//! `Translate` never reads it. The oracle recovers it by reflection so the port's index is checked
//! rather than assumed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::utils::{self, AppResult, StringInterface, get_millis, is_valid_id, new_id};

// ---------------------------------------------------------------------------
// Constants (view.go:17-41, 114-115)
// ---------------------------------------------------------------------------

/// Port of `model.ViewTypeKanban` (view.go:18) — the **only** accepted view type.
pub const VIEW_TYPE_KANBAN: &str = "kanban";

pub const VIEW_TITLE_MAX_RUNES: usize = 256;
pub const VIEW_DESCRIPTION_MAX_RUNES: usize = 1024;
pub const MAX_VIEWS_PER_CHANNEL: usize = 50;

pub const BOARDS_PROPERTY_GROUP_NAME: &str = "boards";
pub const BOARDS_PROPERTY_FIELD_NAME_BOARD: &str = "board";
pub const BOARDS_PROPERTY_FIELD_ASSIGNEE: &str = "assignee";
pub const BOARDS_PROPERTY_FIELD_STATUS: &str = "status";

pub const BOARDS_STATUS_OPTION_TODO: &str = "Todo";
pub const BOARDS_STATUS_OPTION_IN_PROGRESS: &str = "In Progress";
pub const BOARDS_STATUS_OPTION_COMPLETE: &str = "Complete";

/// Colour tokens seeded for the protected Status field, mapping to the webapp's `colorTokenMap`.
pub const BOARDS_STATUS_COLOR_TODO: &str = "default";
pub const BOARDS_STATUS_COLOR_IN_PROGRESS: &str = "blue";
pub const BOARDS_STATUS_COLOR_COMPLETE: &str = "green";

pub const MAX_KANBAN_COLUMNS: usize = 100;

pub const VIEW_QUERY_DEFAULT_PER_PAGE: i64 = 20;
pub const VIEW_QUERY_MAX_PER_PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Kanban props
// ---------------------------------------------------------------------------

/// Port of `model.KanbanColumn` (view.go:45).
///
/// # Partial documents
///
/// Go's `json.Unmarshal` leaves a missing field at its zero value, so a **partial** document
/// decodes cleanly. serde makes every field mandatory unless told otherwise, so each type in this
/// file carries container-level `#[serde(default)]` — the fix [D-043] applied to `Post` for the
/// same reason. It changes deserialization only; the emitted document is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KanbanColumn {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    /// Go: `[]string` with **no** `omitempty`, so a nil slice is `null` and an empty one is `[]`.
    /// Both are distinguishable on the wire, hence `Option<Vec<_>>` rather than `Vec<_>`.
    #[serde(rename = "option_ids")]
    pub option_ids: Option<Vec<String>>,
}

/// Port of `model.KanbanGroupBy` (view.go:52).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KanbanGroupBy {
    #[serde(rename = "field_id")]
    pub field_id: String,

    #[serde(rename = "columns")]
    pub columns: Option<Vec<KanbanColumn>>,
}

/// Port of `model.KanbanProps` (view.go:58) — the typed reading of [`View::props`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KanbanProps {
    #[serde(rename = "group_by")]
    pub group_by: KanbanGroupBy,
}

/// A failure from [`kanban_props_from_props`], carrying **Go's** message.
///
/// # Why the text matters
///
/// `validateKanbanProps` puts `err.Error()` straight into the `AppError`'s `DetailedError`, which
/// is serialised to the client. Go's message is `encoding/json`'s, and it names Go types:
///
/// ```text
/// kanban props unmarshal: json: cannot unmarshal string into Go struct field KanbanGroupBy.group_by.columns of type []model.KanbanColumn
/// ```
///
/// So reproducing it means reproducing the shape `json: cannot unmarshal <kind> into Go struct
/// field <OwnerStruct>.<json path> of type <Go type>`, where the owner is the struct **declaring**
/// the field, the path is dotted json tags from the root **without array indices**, and the type
/// is spelled as Go spells it. The corpus pins all fourteen reachable messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("kanban props unmarshal: {0}")]
pub struct KanbanPropsError(String);

impl KanbanProps {
    /// Port of `(*KanbanProps).ToProps` (view.go:63).
    ///
    /// Marshals to JSON and back into a map. Infallible in practice — the struct has no field a
    /// JSON encoder can refuse — but Go returns an error, so this does too.
    pub fn to_props(&self) -> Result<StringInterface, KanbanPropsError> {
        let raw = serde_json::to_value(self)
            .map_err(|e| KanbanPropsError(format!("kanban props marshal: {e}")))?;
        match raw {
            serde_json::Value::Object(map) => Ok(map),
            other => Err(KanbanPropsError(format!(
                "kanban props unmarshal: json: cannot unmarshal {} into Go value of type model.StringInterface",
                json_kind(&other)
            ))),
        }
    }
}

/// The word Go's `encoding/json` uses for a value's kind in an `UnmarshalTypeError`.
fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn type_error(kind: &str, owner: &str, path: &str, go_type: &str) -> KanbanPropsError {
    KanbanPropsError(format!(
        "json: cannot unmarshal {kind} into Go struct field {owner}.{path} of type {go_type}"
    ))
}

/// Port of `model.KanbanPropsFromProps` (view.go:76).
///
/// # A hand-written decoder, deliberately
///
/// serde would reject the same documents but with entirely different text, and the text is on the
/// wire (see [`KanbanPropsError`]). So this walks the map itself, reproducing four of Go's decoder
/// rules that a `#[derive(Deserialize)]` does not share:
///
/// - **Unknown keys are ignored** at every level.
/// - **`null` never fails** — it leaves the destination at its zero value, so `{"group_by":null}`
///   is a *successful* decode of an empty `KanbanProps`, and `[null]` inside `option_ids` yields
///   one empty string rather than an error.
/// - **The first error in document order wins.** Go decodes the marshalled map, and Go marshals a
///   map with its keys **sorted** — as does [`StringInterface`], which is a `BTreeMap`
///   ([D-027]) — so iterating this map in order reproduces Go's traversal exactly. Measured:
///   `{"field_id":1,"columns":"nope"}` reports `columns`, because `c` sorts before `f`.
/// - **A slice-element type error names the slice's field**, not an index: `columns` of type
///   `model.KanbanColumn`, singular, with no `[1]` anywhere in the path.
pub fn kanban_props_from_props(props: &StringInterface) -> Result<KanbanProps, KanbanPropsError> {
    let mut out = KanbanProps::default();

    for (key, value) in props {
        if key != "group_by" {
            continue; // unknown keys are ignored
        }
        match value {
            serde_json::Value::Null => {}
            serde_json::Value::Object(group) => out.group_by = decode_group_by(group)?,
            other => {
                return Err(type_error(
                    json_kind(other),
                    "KanbanProps",
                    "group_by",
                    "model.KanbanGroupBy",
                ));
            }
        }
    }

    Ok(out)
}

fn decode_group_by(
    group: &serde_json::Map<String, serde_json::Value>,
) -> Result<KanbanGroupBy, KanbanPropsError> {
    let mut out = KanbanGroupBy::default();

    for (key, value) in group {
        match key.as_str() {
            "columns" => match value {
                serde_json::Value::Null => {}
                serde_json::Value::Array(items) => {
                    let mut columns = Vec::with_capacity(items.len());
                    for item in items {
                        columns.push(decode_column(item)?);
                    }
                    out.columns = Some(columns);
                }
                other => {
                    return Err(type_error(
                        json_kind(other),
                        "KanbanGroupBy",
                        "group_by.columns",
                        "[]model.KanbanColumn",
                    ));
                }
            },
            "field_id" => match value {
                serde_json::Value::Null => {}
                serde_json::Value::String(s) => out.field_id = s.clone(),
                other => {
                    return Err(type_error(
                        json_kind(other),
                        "KanbanGroupBy",
                        "group_by.field_id",
                        "string",
                    ));
                }
            },
            _ => {}
        }
    }

    Ok(out)
}

fn decode_column(value: &serde_json::Value) -> Result<KanbanColumn, KanbanPropsError> {
    let mut out = KanbanColumn::default();

    let object = match value {
        // A null element decodes to a zero-valued column rather than being skipped.
        serde_json::Value::Null => return Ok(out),
        serde_json::Value::Object(o) => o,
        other => {
            return Err(type_error(
                json_kind(other),
                "KanbanGroupBy",
                "group_by.columns",
                "model.KanbanColumn",
            ));
        }
    };

    for (key, value) in object {
        match key.as_str() {
            "id" | "name" => match value {
                serde_json::Value::Null => {}
                serde_json::Value::String(s) => {
                    if key == "id" {
                        out.id = s.clone();
                    } else {
                        out.name = s.clone();
                    }
                }
                other => {
                    return Err(type_error(
                        json_kind(other),
                        "KanbanColumn",
                        &format!("group_by.columns.{key}"),
                        "string",
                    ));
                }
            },
            "option_ids" => match value {
                serde_json::Value::Null => {}
                serde_json::Value::Array(items) => {
                    let mut ids = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            // Null into a string is the zero string, not a skip.
                            serde_json::Value::Null => ids.push(String::new()),
                            serde_json::Value::String(s) => ids.push(s.clone()),
                            other => {
                                return Err(type_error(
                                    json_kind(other),
                                    "KanbanColumn",
                                    "group_by.columns.option_ids",
                                    "string",
                                ));
                            }
                        }
                    }
                    out.option_ids = Some(ids);
                }
                other => {
                    return Err(type_error(
                        json_kind(other),
                        "KanbanColumn",
                        "group_by.columns.option_ids",
                        "[]string",
                    ));
                }
            },
            _ => {}
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// Port of `model.View` (view.go:88).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Go's `ViewType` is a defined string type that accepts anything; `IsValid` narrows it.
    /// A `String` rather than an enum, for the reason `post_info.rs` gives for `channel_type`.
    #[serde(rename = "type")]
    pub view_type: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "title")]
    pub title: String,

    #[serde(
        rename = "description",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,

    /// Go's `int`, which is 64-bit on every platform this targets ([D-074]).
    #[serde(rename = "sort_order")]
    pub sort_order: i64,

    /// # `omitempty` on a map drops nil **and** empty
    ///
    /// So `None` and `Some({})` serialise identically, and a round trip collapses the second into
    /// the first. The distinction is nonetheless real: [`Self::is_valid`] reports
    /// `props.kanban_required` for `None` and `props.kanban_field_id` for `Some({})`. It survives
    /// inbound — a client can send `"props":{}` — and cannot survive outbound.
    #[serde(rename = "props", default, skip_serializing_if = "props_is_empty")]
    pub props: Option<StringInterface>,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,
}

/// Go's `omitempty` on a map: true for nil and for a zero-length map alike.
fn props_is_empty(props: &Option<StringInterface>) -> bool {
    props.as_ref().is_none_or(serde_json::Map::is_empty)
}

/// Port of `model.ViewPatch` (view.go:102).
///
/// Every field is a pointer **without** `omitempty`, so all four keys are always present and a
/// nil one is `null`. No skip predicates here, deliberately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewPatch {
    #[serde(rename = "title", default)]
    pub title: Option<String>,

    #[serde(rename = "description", default)]
    pub description: Option<String>,

    #[serde(rename = "sort_order", default)]
    pub sort_order: Option<i64>,

    #[serde(rename = "props", default)]
    pub props: Option<StringInterface>,
}

/// Port of `model.ViewsWithCount` (view.go:109).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewsWithCount {
    /// `[]*View` with no `omitempty`, so nil is `null` and empty is `[]` — two distinct
    /// documents, hence the `Option`.
    #[serde(rename = "views")]
    pub views: Option<Vec<Option<View>>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.ViewQueryOpts` (view.go:117).
///
/// **No `json:` tags at all**, so the wire keys are Go's field names in PascalCase — the fourth
/// instance of the `wrangler.go` shape after `wrangler.go`, `link_metadata.go` and
/// `channel_member_history.go`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewQueryOpts {
    /// 0-based page number.
    #[serde(rename = "Page")]
    pub page: i64,

    /// Zero defaults to [`VIEW_QUERY_DEFAULT_PER_PAGE`]; values above
    /// [`VIEW_QUERY_MAX_PER_PAGE`] are clamped **by the store**, not here — Go's comment
    /// documents behaviour this struct does not implement.
    #[serde(rename = "PerPage")]
    pub per_page: i64,
}

fn view_error(
    id: &str,
    detail: &str,
    params: Option<HashMap<String, serde_json::Value>>,
) -> Box<utils::AppError> {
    Box::new(utils::AppError::new(
        "View.IsValid",
        id,
        params,
        detail,
        400,
    ))
}

impl View {
    /// Port of `(*View).Auditable` (view.go:125).
    ///
    /// Seven keys — identity and timestamps only. `title`, `description`, `sort_order` and `props`
    /// are **not** projected, so the audit log records who and when rather than what the board
    /// says. Unlike the [D-028] types, this one is ported: `bot.rs` set the precedent.
    pub fn auditable(&self) -> StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("channel_id".into(), self.channel_id.clone().into());
        out.insert("type".into(), self.view_type.clone().into());
        out.insert("creator_id".into(), self.creator_id.clone().into());
        out.insert("create_at".into(), self.create_at.into());
        out.insert("update_at".into(), self.update_at.into());
        out.insert("delete_at".into(), self.delete_at.into());
        out
    }

    /// Port of `(*View).IsValid` (view.go:149).
    ///
    /// Eight field checks then the props block. Two shapes worth naming:
    ///
    /// - The **first** branch carries an empty detail; every later one carries `id=`. So a view
    ///   with a bad id produces an error that does not say which view.
    /// - The **props** branches carry an empty detail too, except `kanban_invalid`, which carries
    ///   `encoding/json`'s message.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(view_error("model.view.is_valid.id.app_error", "", None));
        }

        let detail = format!("id={}", self.id);

        if !is_valid_id(&self.channel_id) {
            return Err(view_error(
                "model.view.is_valid.channel_id.app_error",
                &detail,
                None,
            ));
        }

        if !is_valid_id(&self.creator_id) {
            return Err(view_error(
                "model.view.is_valid.creator_id.app_error",
                &detail,
                None,
            ));
        }

        if self.view_type != VIEW_TYPE_KANBAN {
            return Err(view_error(
                "model.view.is_valid.type.app_error",
                &detail,
                None,
            ));
        }

        // Emptiness after trimming, length BEFORE it.
        if self.title.trim().is_empty() || self.title.chars().count() > VIEW_TITLE_MAX_RUNES {
            return Err(view_error(
                "model.view.is_valid.title.app_error",
                &detail,
                None,
            ));
        }

        if self.description.chars().count() > VIEW_DESCRIPTION_MAX_RUNES {
            return Err(view_error(
                "model.view.is_valid.description.app_error",
                &detail,
                None,
            ));
        }

        if self.create_at == 0 {
            return Err(view_error(
                "model.view.is_valid.create_at.app_error",
                &detail,
                None,
            ));
        }

        if self.update_at == 0 {
            return Err(view_error(
                "model.view.is_valid.update_at.app_error",
                &detail,
                None,
            ));
        }

        validate_view_props(&self.view_type, self.props.as_ref())
    }

    /// Port of `(*View).PreSave` (view.go:234).
    ///
    /// `UpdateAt` is set to `CreateAt` **unconditionally**, and `DeleteAt` is cleared
    /// unconditionally — so `PreSave` un-deletes a soft-deleted view.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
        self.delete_at = 0;
    }

    /// Port of `(*View).PreUpdate` (view.go:246).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*View).Patch` (view.go:250).
    ///
    /// A nil patch is a no-op. A present-but-empty string, a zero sort order and an empty props
    /// map are all applied — the guards are on the **pointer**, not on the value.
    ///
    /// `Props` is replaced by a fresh copy rather than aliased, matching Go's
    /// `make(...)` + `maps.Copy`. Note that copies a `*StringInterface` whose target may be nil,
    /// in which case Go produces an **empty non-nil** map — so patching with a nil-target pointer
    /// changes `IsValid`'s answer from `kanban_required` to `kanban_field_id`.
    pub fn patch(&mut self, patch: Option<&ViewPatch>) {
        let Some(patch) = patch else {
            return;
        };
        if let Some(title) = &patch.title {
            self.title = title.clone();
        }
        if let Some(description) = &patch.description {
            self.description = description.clone();
        }
        if let Some(sort_order) = patch.sort_order {
            self.sort_order = sort_order;
        }
        if let Some(props) = &patch.props {
            self.props = Some(props.clone());
        }
    }
}

/// Port of `validateViewProps` (view.go:190) — unexported in Go.
///
/// A non-kanban type skips props validation entirely. Unreachable through [`View::is_valid`],
/// which rejects any other type first, but exported here because the branch exists.
pub fn validate_view_props(view_type: &str, props: Option<&StringInterface>) -> AppResult {
    if view_type == VIEW_TYPE_KANBAN {
        return validate_kanban_props(props);
    }
    Ok(())
}

/// Port of `validateKanbanProps` (view.go:197) — unexported in Go.
///
/// Six branches. The three per-column ones carry `{"Index": i}` as the error's params, which is
/// the only thing distinguishing two failures of the same shape in different columns — and which
/// **no caller outside Go's model package can read**, since `AppError.params` is unexported.
pub fn validate_kanban_props(props: Option<&StringInterface>) -> AppResult {
    let Some(props) = props else {
        return Err(view_error(
            "model.view.is_valid.props.kanban_required.app_error",
            "",
            None,
        ));
    };

    let kanban = match kanban_props_from_props(props) {
        Ok(kanban) => kanban,
        Err(err) => {
            return Err(view_error(
                "model.view.is_valid.props.kanban_invalid.app_error",
                &err.to_string(),
                None,
            ));
        }
    };

    if !is_valid_id(&kanban.group_by.field_id) {
        return Err(view_error(
            "model.view.is_valid.props.kanban_field_id.app_error",
            "",
            None,
        ));
    }

    let columns = kanban.group_by.columns.as_deref().unwrap_or_default();

    if columns.is_empty() {
        return Err(view_error(
            "model.view.is_valid.props.kanban_columns_empty.app_error",
            "",
            None,
        ));
    }

    if columns.len() > MAX_KANBAN_COLUMNS {
        return Err(view_error(
            "model.view.is_valid.props.kanban_columns_max.app_error",
            "",
            None,
        ));
    }

    for (i, col) in columns.iter().enumerate() {
        let index_param = || {
            let mut params = HashMap::new();
            params.insert("Index".to_owned(), serde_json::Value::from(i));
            Some(params)
        };

        if !is_valid_id(&col.id) {
            return Err(view_error(
                "model.view.is_valid.props.kanban_column_id.app_error",
                "",
                index_param(),
            ));
        }
        if col.name.trim().is_empty() {
            return Err(view_error(
                "model.view.is_valid.props.kanban_column_name.app_error",
                "",
                index_param(),
            ));
        }
        if col.option_ids.as_deref().unwrap_or_default().is_empty() {
            return Err(view_error(
                "model.view.is_valid.props.kanban_column_options.app_error",
                "",
                index_param(),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kanban_props() -> StringInterface {
        KanbanProps {
            group_by: KanbanGroupBy {
                field_id: "aaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                columns: Some(vec![KanbanColumn {
                    id: "bbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    name: "Todo".into(),
                    option_ids: Some(vec!["opt-1".into()]),
                }]),
            },
        }
        .to_props()
        .expect("valid props convert")
    }

    fn valid_view() -> View {
        View {
            id: "abcdefghijklmnopqrstuvwxyz".into(),
            channel_id: "zyxwvutsrqponmlkjihgfedcba".into(),
            view_type: VIEW_TYPE_KANBAN.into(),
            creator_id: "0123456789abcdefghijklmnop".into(),
            title: "Sprint board".into(),
            description: "The team's kanban".into(),
            sort_order: 3,
            props: Some(kanban_props()),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_001_000,
            delete_at: 0,
        }
    }

    #[test]
    fn a_valid_view_validates() {
        assert!(valid_view().is_valid().is_ok());
    }

    /// The distinction that changes the error id but cannot survive a round trip.
    #[test]
    fn nil_and_empty_props_differ_in_validation_and_not_on_the_wire() {
        let mut nil_props = valid_view();
        nil_props.props = None;
        assert_eq!(
            nil_props.is_valid().unwrap_err().id,
            "model.view.is_valid.props.kanban_required.app_error"
        );

        let mut empty_props = valid_view();
        empty_props.props = Some(serde_json::Map::new());
        assert_eq!(
            empty_props.is_valid().unwrap_err().id,
            "model.view.is_valid.props.kanban_field_id.app_error"
        );

        // Identical documents...
        let a = utils::go_json_marshal(&nil_props).unwrap();
        let b = utils::go_json_marshal(&empty_props).unwrap();
        assert_eq!(a, b);
        // ...so the round trip collapses one into the other.
        let decoded: View = serde_json::from_str(&b).unwrap();
        assert_eq!(decoded.props, None);

        // But a client can still send the distinction inbound — and a partial document decodes,
        // because Go zero-fills a missing field and this type carries `#[serde(default)]`.
        let inbound: View = serde_json::from_str(r#"{"props":{}}"#).unwrap();
        assert_eq!(inbound.props, Some(serde_json::Map::new()));
        assert_eq!(
            inbound.id, "",
            "every other field is left at Go's zero value"
        );
    }

    /// Length before trimming, emptiness after.
    #[test]
    fn the_title_length_counts_the_untrimmed_string() {
        let mut v = valid_view();

        v.title = format!("{}abc", " ".repeat(VIEW_TITLE_MAX_RUNES));
        assert!(!v.title.trim().is_empty(), "not empty after trimming");
        assert_eq!(
            v.is_valid().unwrap_err().id,
            "model.view.is_valid.title.app_error",
            "...and still too long, because the count includes the padding"
        );

        v.title = "é".repeat(VIEW_TITLE_MAX_RUNES);
        assert_eq!(v.title.len(), VIEW_TITLE_MAX_RUNES * 2, "bytes");
        assert!(v.is_valid().is_ok(), "the cap counts runes, not bytes");
    }

    #[test]
    fn pre_save_undeletes_and_realigns_the_timestamps() {
        let mut v = View {
            id: "abcdefghijklmnopqrstuvwxyz".into(),
            create_at: 1,
            update_at: 99,
            delete_at: 1_700_000_000_000,
            ..Default::default()
        };
        v.pre_save();
        assert_eq!(v.create_at, 1);
        assert_eq!(
            v.update_at, 1,
            "UpdateAt is set to CreateAt unconditionally"
        );
        assert_eq!(v.delete_at, 0, "PreSave un-deletes");

        let mut fresh = View::default();
        fresh.pre_save();
        assert!(utils::is_valid_id(&fresh.id));
        assert!(fresh.create_at > 0);
        assert_eq!(fresh.update_at, fresh.create_at);
    }

    #[test]
    fn patch_guards_the_pointer_not_the_value() {
        let mut v = valid_view();
        v.patch(None);
        assert_eq!(v.title, "Sprint board", "a nil patch is a no-op");

        v.patch(Some(&ViewPatch::default()));
        assert_eq!(v.title, "Sprint board", "an all-nil patch is a no-op");

        v.patch(Some(&ViewPatch {
            title: Some(String::new()),
            sort_order: Some(0),
            ..Default::default()
        }));
        assert_eq!(v.title, "", "an empty string IS applied");
        assert_eq!(v.sort_order, 0);
    }

    /// Go's decoder rules that a derived `Deserialize` does not share.
    #[test]
    fn the_decoder_ignores_unknown_keys_and_tolerates_null() {
        let parse = |raw: &str| {
            let props: StringInterface = serde_json::from_str(raw).unwrap();
            kanban_props_from_props(&props)
        };

        assert_eq!(parse(r#"{"unknown":1}"#).unwrap(), KanbanProps::default());
        assert_eq!(
            parse(r#"{"group_by":null}"#).unwrap(),
            KanbanProps::default()
        );
        assert_eq!(
            parse(r#"{"group_by":{"columns":null}}"#)
                .unwrap()
                .group_by
                .columns,
            None
        );
        // A null element in a []string is the empty string, not a skip.
        assert_eq!(
            parse(r#"{"group_by":{"columns":[{"option_ids":[null]}]}}"#)
                .unwrap()
                .group_by
                .columns
                .unwrap()[0]
                .option_ids,
            Some(vec![String::new()])
        );
        // A null element in a []struct is a zero-valued struct.
        assert_eq!(
            parse(r#"{"group_by":{"columns":[null]}}"#)
                .unwrap()
                .group_by
                .columns
                .unwrap()
                .len(),
            1
        );
    }

    /// The first error in **sorted** key order wins, because that is the order Go marshals a map.
    #[test]
    fn the_first_error_in_sorted_order_wins() {
        let props: StringInterface =
            serde_json::from_str(r#"{"group_by":{"field_id":1,"columns":"nope"}}"#).unwrap();
        let err = kanban_props_from_props(&props).unwrap_err();
        assert!(
            err.to_string().contains("columns"),
            "'columns' sorts before 'field_id', so it is seen first: {err}"
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_view.json");
            serde_json::from_str(raw).expect("behaviour_view.json parses")
        })
    }

    const ID: &str = "abcdefghijklmnopqrstuvwxyz";
    const CHANNEL_ID: &str = "zyxwvutsrqponmlkjihgfedcba";
    const CREATOR_ID: &str = "0123456789abcdefghijklmnop";
    const FIELD_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COLUMN_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn props_from_json(raw: &str) -> StringInterface {
        serde_json::from_str(raw).expect("corpus literal parses")
    }

    fn kanban_props() -> StringInterface {
        KanbanProps {
            group_by: KanbanGroupBy {
                field_id: FIELD_ID.into(),
                columns: Some(vec![KanbanColumn {
                    id: COLUMN_ID.into(),
                    name: "Todo".into(),
                    option_ids: Some(vec!["opt-1".into()]),
                }]),
            },
        }
        .to_props()
        .unwrap()
    }

    fn many_columns(n: usize) -> StringInterface {
        KanbanProps {
            group_by: KanbanGroupBy {
                field_id: FIELD_ID.into(),
                columns: Some(
                    (0..n)
                        .map(|i| KanbanColumn {
                            id: COLUMN_ID.into(),
                            name: format!("Col {i}"),
                            option_ids: Some(vec!["opt".into()]),
                        })
                        .collect(),
                ),
            },
        }
        .to_props()
        .unwrap()
    }

    fn base_view() -> View {
        View {
            id: ID.into(),
            channel_id: CHANNEL_ID.into(),
            view_type: VIEW_TYPE_KANBAN.into(),
            creator_id: CREATOR_ID.into(),
            title: "Sprint board".into(),
            description: "The team's kanban".into(),
            sort_order: 3,
            props: Some(kanban_props()),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_001_000,
            delete_at: 0,
        }
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["ViewTypeKanban"], VIEW_TYPE_KANBAN);
        assert_eq!(c["ViewTitleMaxRunes"], VIEW_TITLE_MAX_RUNES);
        assert_eq!(c["ViewDescriptionMaxRunes"], VIEW_DESCRIPTION_MAX_RUNES);
        assert_eq!(c["MaxViewsPerChannel"], MAX_VIEWS_PER_CHANNEL);
        assert_eq!(c["BoardsPropertyGroupName"], BOARDS_PROPERTY_GROUP_NAME);
        assert_eq!(
            c["BoardsPropertyFieldNameBoard"],
            BOARDS_PROPERTY_FIELD_NAME_BOARD
        );
        assert_eq!(
            c["BoardsPropertyFieldAssignee"],
            BOARDS_PROPERTY_FIELD_ASSIGNEE
        );
        assert_eq!(c["BoardsPropertyFieldStatus"], BOARDS_PROPERTY_FIELD_STATUS);
        assert_eq!(c["BoardsStatusOptionTodo"], BOARDS_STATUS_OPTION_TODO);
        assert_eq!(
            c["BoardsStatusOptionInProgress"],
            BOARDS_STATUS_OPTION_IN_PROGRESS
        );
        assert_eq!(
            c["BoardsStatusOptionComplete"],
            BOARDS_STATUS_OPTION_COMPLETE
        );
        assert_eq!(c["BoardsStatusColorTodo"], BOARDS_STATUS_COLOR_TODO);
        assert_eq!(
            c["BoardsStatusColorInProgress"],
            BOARDS_STATUS_COLOR_IN_PROGRESS
        );
        assert_eq!(c["BoardsStatusColorComplete"], BOARDS_STATUS_COLOR_COMPLETE);
        assert_eq!(c["MaxKanbanColumns"], MAX_KANBAN_COLUMNS);
        assert_eq!(c["ViewQueryDefaultPerPage"], VIEW_QUERY_DEFAULT_PER_PAGE);
        assert_eq!(c["ViewQueryMaxPerPage"], VIEW_QUERY_MAX_PER_PAGE);
    }

    #[test]
    fn is_valid_matches_go() {
        for case in oracle()["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut v = base_view();

            match name {
                "valid" => {}
                "bad_id" => v.id = "nope".into(),
                "empty_id" => v.id = String::new(),
                "bad_channel_id" => v.channel_id = "nope".into(),
                "bad_creator_id" => v.creator_id = "nope".into(),
                "empty_type" => v.view_type = String::new(),
                "unknown_type" => v.view_type = "list".into(),
                "uppercase_type" => v.view_type = "Kanban".into(),
                "empty_title" => v.title = String::new(),
                "whitespace_title" => v.title = "   \t\n  ".into(),
                "nbsp_only_title" => v.title = "\u{a0}\u{a0}".into(),
                "zwsp_only_title" => v.title = "\u{200b}".into(),
                "title_at_cap" => v.title = "t".repeat(VIEW_TITLE_MAX_RUNES),
                "title_over_cap" => v.title = "t".repeat(VIEW_TITLE_MAX_RUNES + 1),
                "title_padded_over_cap" => {
                    v.title = format!("{}abc", " ".repeat(VIEW_TITLE_MAX_RUNES));
                }
                "title_multibyte_at_cap" => v.title = "é".repeat(VIEW_TITLE_MAX_RUNES),
                "title_multibyte_over_cap" => v.title = "é".repeat(VIEW_TITLE_MAX_RUNES + 1),
                "description_at_cap" => v.description = "d".repeat(VIEW_DESCRIPTION_MAX_RUNES),
                "description_over_cap" => {
                    v.description = "d".repeat(VIEW_DESCRIPTION_MAX_RUNES + 1)
                }
                "empty_description_is_fine" => v.description = String::new(),
                "zero_create_at" => v.create_at = 0,
                "zero_update_at" => v.update_at = 0,
                "negative_create_at" => v.create_at = -1,
                "negative_sort_order" => v.sort_order = -5,
                "nil_props" => v.props = None,
                "empty_props" => v.props = Some(serde_json::Map::new()),
                "props_columns_at_max" => v.props = Some(many_columns(MAX_KANBAN_COLUMNS)),
                "props_columns_over_max" => v.props = Some(many_columns(MAX_KANBAN_COLUMNS + 1)),
                "bad_id_and_bad_props" => {
                    v.id = "nope".into();
                    v.props = None;
                }
                "bad_type_and_bad_title" => {
                    v.view_type = "list".into();
                    v.title = String::new();
                }
                "zero_update_at_and_nil_props" => {
                    v.update_at = 0;
                    v.props = None;
                }
                // Everything else sets props from a literal.
                "props_missing_group_by" => {
                    v.props = Some(props_from_json(r#"{"something":"else"}"#))
                }
                "props_group_by_is_a_string" => {
                    v.props = Some(props_from_json(r#"{"group_by":"nope"}"#));
                }
                "props_group_by_is_a_number" => {
                    v.props = Some(props_from_json(r#"{"group_by":42}"#));
                }
                "props_columns_is_a_string" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":"nope"}}}}"#
                    )));
                }
                "props_field_id_is_a_number" => {
                    v.props = Some(props_from_json(
                        r#"{"group_by":{"field_id":42,"columns":[]}}"#,
                    ));
                }
                "props_bad_field_id" => {
                    v.props = Some(props_from_json(
                        r#"{"group_by":{"field_id":"nope","columns":[]}}"#,
                    ));
                }
                "props_empty_columns" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[]}}}}"#
                    )));
                }
                "props_null_columns" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":null}}}}"#
                    )));
                }
                "props_bad_column_id" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"nope","name":"Todo","option_ids":["o"]}}]}}}}"#
                    )));
                }
                "props_empty_column_name" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"{COLUMN_ID}","name":"  ","option_ids":["o"]}}]}}}}"#
                    )));
                }
                "props_empty_column_options" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"{COLUMN_ID}","name":"Todo","option_ids":[]}}]}}}}"#
                    )));
                }
                "props_second_column_bad" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"{COLUMN_ID}","name":"Todo","option_ids":["o"]}},{{"id":"nope","name":"Doing","option_ids":["o"]}}]}}}}"#
                    )));
                }
                "props_third_column_no_options" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"{COLUMN_ID}","name":"A","option_ids":["o"]}},{{"id":"{COLUMN_ID}","name":"B","option_ids":["o"]}},{{"id":"{COLUMN_ID}","name":"C","option_ids":[]}}]}}}}"#
                    )));
                }
                "props_extra_keys_ignored" => {
                    v.props = Some(props_from_json(&format!(
                        r#"{{"group_by":{{"field_id":"{FIELD_ID}","columns":[{{"id":"{COLUMN_ID}","name":"Todo","option_ids":["o"],"extra":1}}],"unknown":true}},"top_level_extra":"x"}}"#
                    )));
                }
                other => panic!("unmapped corpus case: {other}"),
            }

            match v.is_valid() {
                Ok(()) => assert!(case["ok"].as_bool().unwrap(), "{name}: Go rejected this"),
                Err(err) => {
                    assert!(!case["ok"].as_bool().unwrap(), "{name}: Go accepted this");
                    assert_eq!(err.id, case["id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                    assert_eq!(
                        err.status_code,
                        case["status"].as_i64().unwrap() as i32,
                        "{name}: status"
                    );
                    assert_eq!(
                        err.detailed_error,
                        case["detailed_error"].as_str().unwrap(),
                        "{name}: detailed_error"
                    );

                    // The unexported params, recovered by reflection in the oracle.
                    match case["params"].as_object() {
                        Some(want) => {
                            let got = err.params.as_ref().unwrap_or_else(|| {
                                panic!("{name}: Go carried params and we did not")
                            });
                            assert_eq!(got.len(), want.len(), "{name}: param count");
                            for (k, v) in want {
                                assert_eq!(got.get(k), Some(v), "{name}: param {k}");
                            }
                        }
                        None => assert!(
                            err.params.is_none(),
                            "{name}: Go carried no params and we did"
                        ),
                    }
                }
            }
        }
    }

    /// `encoding/json`'s error text, reproduced verbatim.
    #[test]
    fn kanban_props_from_props_matches_go() {
        let mut errors = 0;

        for case in oracle()["kanban_props"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let props = props_from_json(case["input"].as_str().unwrap());
            let got = kanban_props_from_props(&props);

            assert_eq!(got.is_ok(), case["ok"].as_bool().unwrap(), "{name}: ok");

            match got {
                Ok(kp) => {
                    assert_eq!(
                        kp.group_by.field_id,
                        case["field_id"].as_str().unwrap(),
                        "{name}: field_id"
                    );
                    assert_eq!(
                        kp.group_by.columns.as_deref().unwrap_or_default().len() as u64,
                        case["column_count"].as_u64().unwrap(),
                        "{name}: column count"
                    );
                    assert_eq!(
                        kp.group_by.columns.is_none(),
                        case["columns_is_nil"].as_bool().unwrap(),
                        "{name}: nil vs empty columns"
                    );
                    // The decoded columns, marshalled, against Go's.
                    let ours = utils::go_json_marshal(&kp.group_by.columns).unwrap();
                    assert_eq!(
                        ours,
                        case["columns_json"].as_str().unwrap(),
                        "{name}: columns"
                    );
                }
                Err(err) => {
                    errors += 1;
                    assert_eq!(
                        err.to_string(),
                        case["error"].as_str().unwrap(),
                        "{name}: Go's encoding/json message, verbatim"
                    );
                }
            }
        }

        assert!(errors >= 12, "the corpus must exercise every message shape");
    }

    #[test]
    fn to_props_matches_go() {
        for case in oracle()["to_props"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let kp = match name {
                "zero" => KanbanProps::default(),
                "field_only" => KanbanProps {
                    group_by: KanbanGroupBy {
                        field_id: FIELD_ID.into(),
                        columns: None,
                    },
                },
                "one_column" => KanbanProps {
                    group_by: KanbanGroupBy {
                        field_id: FIELD_ID.into(),
                        columns: Some(vec![KanbanColumn {
                            id: COLUMN_ID.into(),
                            name: "Todo".into(),
                            option_ids: Some(vec!["a".into(), "b".into()]),
                        }]),
                    },
                },
                "empty_columns_slice" => KanbanProps {
                    group_by: KanbanGroupBy {
                        field_id: FIELD_ID.into(),
                        columns: Some(vec![]),
                    },
                },
                "column_with_nil_options" => KanbanProps {
                    group_by: KanbanGroupBy {
                        field_id: FIELD_ID.into(),
                        columns: Some(vec![KanbanColumn {
                            id: COLUMN_ID.into(),
                            name: "Todo".into(),
                            option_ids: None,
                        }]),
                    },
                },
                other => panic!("unmapped corpus case: {other}"),
            };

            let props = kp.to_props().expect("ToProps does not fail");
            assert_eq!(
                utils::go_json_marshal(&props).unwrap(),
                case["props_json"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn pre_save_matches_go() {
        for case in oracle()["pre_save"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let id_empty = case["in_id_empty"].as_bool().unwrap();
            let create_at = case["in_create_at"].as_i64().unwrap();

            let mut v = View {
                id: if id_empty { String::new() } else { ID.into() },
                create_at,
                update_at: 5,
                delete_at: if name == "deleted" {
                    1_700_000_000_000
                } else {
                    0
                },
                ..Default::default()
            };
            v.pre_save();

            assert_eq!(
                v.delete_at,
                case["out_delete_at"].as_i64().unwrap(),
                "{name}"
            );
            assert_eq!(
                v.update_at == v.create_at,
                case["update_at_equals_create_at"].as_bool().unwrap(),
                "{name}"
            );
            if case["id_is_generated"].as_bool().unwrap() {
                assert!(utils::is_valid_id(&v.id), "{name}: generated id");
            } else {
                assert_eq!(v.id, case["out_id"].as_str().unwrap(), "{name}: id kept");
            }
            if case["create_at_uses_now"].as_bool().unwrap() {
                assert!(v.create_at > 0, "{name}: create_at from the clock");
            } else {
                assert_eq!(
                    v.create_at,
                    case["out_create_at"].as_i64().unwrap(),
                    "{name}: create_at kept"
                );
            }
        }
    }

    #[test]
    fn patch_matches_go() {
        for case in oracle()["patch"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let patch = match name {
                "nil_patch" => None,
                "empty_patch" => Some(ViewPatch::default()),
                "title" => Some(ViewPatch {
                    title: Some("New title".into()),
                    ..Default::default()
                }),
                "empty_title" => Some(ViewPatch {
                    title: Some(String::new()),
                    ..Default::default()
                }),
                "description" => Some(ViewPatch {
                    description: Some("New description".into()),
                    ..Default::default()
                }),
                "empty_description" => Some(ViewPatch {
                    description: Some(String::new()),
                    ..Default::default()
                }),
                "sort_order" => Some(ViewPatch {
                    sort_order: Some(42),
                    ..Default::default()
                }),
                "zero_sort_order" => Some(ViewPatch {
                    sort_order: Some(0),
                    ..Default::default()
                }),
                "props" => Some(ViewPatch {
                    props: Some(props_from_json(r#"{"a":1}"#)),
                    ..Default::default()
                }),
                "empty_props" | "nil_props_pointer_target" => Some(ViewPatch {
                    props: Some(serde_json::Map::new()),
                    ..Default::default()
                }),
                "everything" => Some(ViewPatch {
                    title: Some("T".into()),
                    description: Some("D".into()),
                    sort_order: Some(7),
                    props: Some(props_from_json(r#"{"z":true}"#)),
                }),
                other => panic!("unmapped corpus case: {other}"),
            };

            let mut v = base_view();
            v.patch(patch.as_ref());

            assert_eq!(
                utils::go_json_marshal(&v).unwrap(),
                case["out_json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                v.props.as_ref().map_or(0, serde_json::Map::len) as u64,
                case["props_len"].as_u64().unwrap(),
                "{name}: props len"
            );
        }
    }

    /// [D-116]: Go's `Clone` shares nested maps; ours does not.
    #[test]
    fn clone_shares_nested_maps_in_go_and_not_here() {
        let c = &oracle()["clone"];
        assert_eq!(c["title_is_independent"], true);
        assert_eq!(c["top_level_key_is_independent"], true);
        assert_eq!(c["nil_receiver_returns_nil"], true);
        assert_eq!(
            c["nested_map_is_shared"], true,
            "Go's maps.Copy is shallow — this is the divergence D-116 records"
        );

        // Ours deep-clones, so the same probe gives the opposite answer.
        let original = View {
            props: Some(props_from_json(r#"{"nested":{"k":"v"},"flat":"x"}"#)),
            ..base_view()
        };
        let mut clone = original.clone();
        if let Some(props) = clone.props.as_mut() {
            props.insert("flat".into(), "y".into());
        }
        assert_eq!(
            original.props.as_ref().unwrap()["flat"],
            "x",
            "the top-level key is independent in both languages"
        );

        let mut mutated = original;
        if let Some(nested) = mutated
            .props
            .as_mut()
            .and_then(|p| p.get_mut("nested"))
            .and_then(|n| n.as_object_mut())
        {
            nested.insert("k".into(), "mutated".into());
        }
        assert_eq!(
            clone.props.as_ref().unwrap()["nested"]["k"],
            "v",
            "ours does NOT share the nested map — the divergence, asserted"
        );
    }

    #[test]
    fn auditable_matches_go() {
        let a = &oracle()["auditable"];
        let ours = base_view().auditable();

        assert_eq!(ours.len() as u64, a["key_count"].as_u64().unwrap());
        assert_eq!(
            utils::go_json_marshal(&ours).unwrap(),
            a["json"].as_str().unwrap()
        );
        for omitted in ["title", "description", "sort_order", "props"] {
            assert!(
                !ours.contains_key(omitted),
                "{omitted} must not be projected"
            );
        }
        assert_eq!(a["type_value"], VIEW_TYPE_KANBAN);
    }

    /// `strings.TrimSpace` against `str::trim` — the same Unicode property, measured.
    #[test]
    fn the_whitespace_definitions_agree() {
        let probes = oracle()["trim_space_charset"].as_array().unwrap();
        let mut whitespace = 0;

        for probe in probes {
            let cp = u32::try_from(probe["codepoint"].as_i64().unwrap()).unwrap();
            let ch = char::from_u32(cp).unwrap();
            let title = ch.to_string();

            assert_eq!(
                title.trim().is_empty(),
                probe["trims_to_empty"].as_bool().unwrap(),
                "U+{cp:04X}: str::trim vs strings.TrimSpace"
            );

            let v = View {
                title,
                ..base_view()
            };
            let rejected = v
                .is_valid()
                .is_err_and(|e| e.id == "model.view.is_valid.title.app_error");
            assert_eq!(
                rejected,
                probe["title_rejected"].as_bool().unwrap(),
                "U+{cp:04X}: title rejected"
            );

            if probe["trims_to_empty"].as_bool().unwrap() {
                whitespace += 1;
            }
        }

        assert!(
            whitespace > 5 && whitespace < probes.len(),
            "the sweep must separate whitespace from the rest, got {whitespace}"
        );
    }

    #[test]
    fn the_wire_format_matches_go() {
        for probe in oracle()["wire"].as_array().unwrap() {
            let name = probe["name"].as_str().unwrap();
            let want = probe["json"].as_str().unwrap();

            let got = match name {
                "zero" => utils::go_json_marshal(&View::default()).unwrap(),
                "full" => utils::go_json_marshal(&base_view()).unwrap(),
                "no_description" => utils::go_json_marshal(&View {
                    description: String::new(),
                    ..base_view()
                })
                .unwrap(),
                "nil_props" => utils::go_json_marshal(&View {
                    props: None,
                    ..base_view()
                })
                .unwrap(),
                "empty_props" => utils::go_json_marshal(&View {
                    props: Some(serde_json::Map::new()),
                    ..base_view()
                })
                .unwrap(),
                "negative_sort_order" => utils::go_json_marshal(&View {
                    sort_order: -1,
                    ..base_view()
                })
                .unwrap(),
                "views_with_count_nil" => utils::go_json_marshal(&ViewsWithCount {
                    views: None,
                    total_count: 7,
                })
                .unwrap(),
                "views_with_count_empty" => utils::go_json_marshal(&ViewsWithCount {
                    views: Some(vec![]),
                    total_count: 0,
                })
                .unwrap(),
                "views_with_count_one" => utils::go_json_marshal(&ViewsWithCount {
                    views: Some(vec![Some(base_view())]),
                    total_count: 1,
                })
                .unwrap(),
                "views_with_count_null_element" => utils::go_json_marshal(&ViewsWithCount {
                    views: Some(vec![None]),
                    total_count: 1,
                })
                .unwrap(),
                "view_patch_zero" => utils::go_json_marshal(&ViewPatch::default()).unwrap(),
                "view_patch_title_only" => utils::go_json_marshal(&ViewPatch {
                    title: Some("T".into()),
                    ..Default::default()
                })
                .unwrap(),
                "view_query_opts" => utils::go_json_marshal(&ViewQueryOpts {
                    page: 2,
                    per_page: 50,
                })
                .unwrap(),
                other => panic!("unmapped wire probe: {other}"),
            };

            assert_eq!(got, want, "{name}");
        }
    }
}
