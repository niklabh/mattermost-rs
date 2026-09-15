//! Port of `app/board.go` — `CreateBoardChannel` and `buildBoardKanbanView`, the app half of
//! `POST /api/v4/boards`.
//!
//! A board is a channel of type `BO` or `BP` whose `Props` link it to the two boards property
//! fields (`status`, `assignee`) and which is born with one kanban view: a column per option of
//! the `status` field, in the option order the field's `attrs` hold. The channel row and the
//! view are one transaction ([`mm_store::channel_store::ChannelStore::save_board_channel`]);
//! the creator's membership, the join-history row and the two broadcasts follow it, each with
//! its own error id, and none of them is undone if a later one fails — the same shape as
//! `CreateChannel`.
//!
//! # Two things a reader would get wrong from the channel-create port
//!
//! - **Every store failure is a 400 or a 500 with a board-specific id**, not the channel ones:
//!   `ErrInvalidInput` → `app.channel.create_board_channel.invalid.app_error`, the name conflict
//!   → `store.sql_channel.save_channel.exists.app_error` (that one is shared), the team's
//!   channel limit → `app.channel.create_board_channel.limit.app_error`, a model `AppError`
//!   passes through, and anything else is `app.channel.create_board_channel.internal_error`.
//! - **The board is not put in a sidebar category, nor in `PublicChannels`**, and the event is
//!   `board_created`, not `channel_created`.

use mm_model::channel::{CHANNEL_PROPS_BOARD_LINKED_PROPERTIES, Channel};
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_model::property_field::{PROPERTY_FIELD_OBJECT_TYPE_POST, PropertyField};
use mm_model::utils::{AppError, AppResult, get_millis, new_id};
use mm_model::view::{
    BOARDS_PROPERTY_FIELD_ASSIGNEE, BOARDS_PROPERTY_FIELD_STATUS, BOARDS_PROPERTY_GROUP_NAME,
    KanbanColumn, KanbanGroupBy, KanbanProps, VIEW_TYPE_KANBAN, View,
};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_BOARD_CREATED, WEBSOCKET_EVENT_VIEW_CREATED, WebSocketEvent,
};
use mm_store::StoreError;
use mm_store::channel_member_history_store::ChannelMemberHistoryStore;
use mm_store::channel_store::{ChannelSave, ChannelStore};
use mm_store::property_store::PropertyStore;
use mm_store::user_store::UserStore;

use crate::App;

const WHERE: &str = "CreateBoardChannel";
const INTERNAL_ERROR: &str = "app.channel.create_board_channel.internal_error";

/// The 500 every "should not happen" branch of `CreateBoardChannel` answers, with Go's detail.
fn internal_error(detail: &str) -> Box<AppError> {
    AppError::boxed(WHERE, INTERNAL_ERROR, None, detail.to_owned(), 500)
}

/// Port of `buildBoardKanbanView` (board.go:117): one column per `{id, name}` option of the
/// status field, in the field's own order, each column minted a fresh id. An option missing
/// either string is skipped; a field with no `options` at all is the 500.
pub(crate) fn build_board_kanban_view(
    creator_id: &str,
    status_field: &PropertyField,
) -> AppResult<View> {
    let options = status_field
        .attrs
        .as_ref()
        .and_then(|attrs| attrs.get("options"))
        .and_then(serde_json::Value::as_array)
        .filter(|options| !options.is_empty())
        .ok_or_else(|| internal_error("status field has no options"))?;

    // Go's `var columns []model.KanbanColumn` is nil until the first append, and a nil slice
    // marshals as `null` — so no valid option means `None`, not `Some(vec![])`.
    let mut columns: Option<Vec<KanbanColumn>> = None;
    for option in options {
        let Some(option) = option.as_object() else {
            continue;
        };
        let id = option.get("id").and_then(serde_json::Value::as_str);
        let name = option.get("name").and_then(serde_json::Value::as_str);
        if let (Some(id), Some(name)) = (id, name)
            && !id.is_empty()
            && !name.is_empty()
        {
            columns.get_or_insert_with(Vec::new).push(KanbanColumn {
                id: new_id(),
                name: name.to_owned(),
                option_ids: Some(vec![id.to_owned()]),
            });
        }
    }

    let kanban = KanbanProps {
        group_by: KanbanGroupBy {
            field_id: status_field.id.clone(),
            columns,
        },
    };
    let props = kanban
        .to_props()
        .map_err(|_| internal_error("failed to serialize kanban props"))?;

    Ok(View {
        creator_id: creator_id.to_owned(),
        view_type: VIEW_TYPE_KANBAN.to_owned(),
        title: "Board".to_owned(),
        props: Some(props),
        ..View::default()
    })
}

impl App {
    /// Port of `App.CreateBoardChannel` (board.go:16).
    ///
    /// Mutates `channel` into the saved row — id, timestamps, the sanitised names and the
    /// `board:linked_properties` prop — which is what the handler marshals at 201.
    #[tracing::instrument(skip_all, fields(team_id = %channel.team_id, channel_type = %channel.channel_type, channel_id))]
    pub async fn create_board_channel(&self, channel: &mut Channel) -> AppResult<()> {
        if !self.config().feature_flag_integrated_boards {
            return Err(AppError::boxed(
                WHERE,
                "app.channel.create_board_channel.boards_not_enabled.app_error",
                None,
                "The Integrated Boards feature is not enabled.".to_owned(),
                403,
            ));
        }

        channel.display_name = channel.display_name.trim().to_owned();
        channel.is_valid_board()?;

        // Look up boards property fields by name.
        let boards_group = self
            .get_property_group(BOARDS_PROPERTY_GROUP_NAME)
            .await
            .map_err(|_| internal_error("boards property group not found"))?;

        let field = async |name: &str, missing: &str| {
            self.store()
                .property()
                .get_field_by_name_for_object_type(
                    &boards_group.id,
                    "",
                    PROPERTY_FIELD_OBJECT_TYPE_POST,
                    name,
                )
                .await
                .map_err(|err| {
                    tracing::debug!(error = %err, name, "boards property field lookup failed");
                    internal_error(missing)
                })
        };
        let assignee_field = field(
            BOARDS_PROPERTY_FIELD_ASSIGNEE,
            "assignee property field not found",
        )
        .await?;
        let status_field = field(
            BOARDS_PROPERTY_FIELD_STATUS,
            "status property field not found",
        )
        .await?;

        // Set linked properties on channel — status first, then assignee.
        channel
            .props
            .get_or_insert_with(serde_json::Map::new)
            .insert(
                CHANNEL_PROPS_BOARD_LINKED_PROPERTIES.to_owned(),
                serde_json::Value::Array(vec![
                    serde_json::Value::String(status_field.id.clone()),
                    serde_json::Value::String(assignee_field.id.clone()),
                ]),
            );

        let mut view = build_board_kanban_view(&channel.creator_id, &status_field)?;

        // Atomically save channel + view.
        let max = self.config().max_channels_per_team;
        match self
            .store()
            .channel()
            .save_board_channel(channel, max, &mut view)
            .await
        {
            Ok(ChannelSave::Saved) => {}
            Ok(ChannelSave::Existing(_)) => {
                return Err(AppError::boxed(
                    WHERE,
                    "store.sql_channel.save_channel.exists.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }
            Err(err) => return Err(save_board_channel_error(err)),
        }
        tracing::Span::current().record("channel_id", &channel.id);

        // Add creator as admin member.
        let user = self
            .store()
            .user()
            .get(&channel.creator_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "creator lookup failed");
                AppError::boxed(WHERE, "app.user.get.app_error", None, String::new(), 500)
            })?;

        let member = ChannelMember {
            channel_id: channel.id.clone(),
            user_id: user.id.clone(),
            scheme_guest: user.is_guest(),
            scheme_user: !user.is_guest(),
            scheme_admin: true,
            notify_props: Some(get_default_channel_notify_props()),
            ..ChannelMember::default()
        };
        self.store()
            .channel()
            .save_member(member)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "board member save failed");
                AppError::boxed(
                    WHERE,
                    "app.channel.save_member.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.store()
            .channel_member_history()
            .log_join_event(&channel.creator_id, &channel.id, get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "join history write failed");
                AppError::boxed(
                    WHERE,
                    "app.channel_member_history.log_join_event.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.hub()
            .invalidate_channel_members_for_user(&channel.creator_id);

        // Publish board_created event (NOT channel_created), to the creator.
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_BOARD_CREATED,
            "",
            "",
            &channel.creator_id,
            None,
            "",
        );
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));
        message.add(
            "team_id",
            serde_json::Value::String(channel.team_id.clone()),
        );
        self.publish(message).await;

        self.publish_view_event(WEBSOCKET_EVENT_VIEW_CREATED, &view, "")
            .await;

        // Do NOT add to sidebar categories — boards don't appear in sidebar.
        Ok(())
    }
}

/// The `errors.As` ladder of `CreateBoardChannel` (board.go:56-72), in Go's order.
fn save_board_channel_error(err: StoreError) -> Box<AppError> {
    match err {
        StoreError::InvalidInput { .. } => AppError::boxed(
            WHERE,
            "app.channel.create_board_channel.invalid.app_error",
            None,
            String::new(),
            400,
        ),
        StoreError::Conflict { .. } => AppError::boxed(
            WHERE,
            "store.sql_channel.save_channel.exists.app_error",
            None,
            String::new(),
            400,
        ),
        StoreError::LimitExceeded { .. } => AppError::boxed(
            WHERE,
            "app.channel.create_board_channel.limit.app_error",
            None,
            String::new(),
            400,
        ),
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = %other, "board channel save failed");
            internal_error("")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_field(options: serde_json::Value) -> PropertyField {
        PropertyField {
            id: "statusfieldid00000000000000".to_owned(),
            attrs: Some(
                serde_json::json!({"options": options})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
            ..PropertyField::default()
        }
    }

    /// One column per option, in the field's order, each carrying exactly its option id.
    #[test]
    fn the_kanban_view_has_a_column_per_status_option() {
        let field = status_field(serde_json::json!([
            {"id": "opt1", "name": "Todo"},
            {"id": "opt2", "name": "In Progress"},
            {"id": "opt3", "name": "Complete"},
        ]));
        let view = build_board_kanban_view("creator", &field).unwrap();
        assert_eq!(view.view_type, "kanban");
        assert_eq!(view.title, "Board");
        assert_eq!(view.creator_id, "creator");
        let props = view.props.unwrap();
        assert_eq!(props["group_by"]["field_id"], field.id);
        let columns = props["group_by"]["columns"].as_array().unwrap();
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[1]["name"], "In Progress");
        assert_eq!(columns[1]["option_ids"], serde_json::json!(["opt2"]));
        assert_eq!(columns[0]["id"].as_str().unwrap().len(), 26);
    }

    /// An option missing its id or name is skipped, not an error; none valid is `null` columns.
    #[test]
    fn a_malformed_option_is_skipped() {
        let field = status_field(serde_json::json!([
            {"id": "", "name": "Nameless id"},
            {"name": "No id"},
            "not an object",
            {"id": "opt9", "name": "Kept"},
        ]));
        let view = build_board_kanban_view("creator", &field).unwrap();
        let columns = view.props.unwrap()["group_by"]["columns"].clone();
        assert_eq!(columns.as_array().unwrap().len(), 1);

        let field = status_field(serde_json::json!([{"name": "No id"}]));
        let view = build_board_kanban_view("creator", &field).unwrap();
        assert_eq!(
            view.props.unwrap()["group_by"]["columns"],
            serde_json::Value::Null
        );
    }

    /// No `options` at all — absent, or an empty list — is the 500 with Go's detail.
    #[test]
    fn a_status_field_without_options_is_the_internal_error() {
        for attrs in [
            serde_json::json!({}),
            serde_json::json!({"options": []}),
            serde_json::json!({"options": "todo"}),
        ] {
            let field = PropertyField {
                attrs: Some(attrs.as_object().cloned().unwrap()),
                ..PropertyField::default()
            };
            let err = build_board_kanban_view("creator", &field).unwrap_err();
            assert_eq!(err.id, INTERNAL_ERROR);
            assert_eq!(err.status_code, 500);
            assert_eq!(err.detailed_error, "status field has no options");
        }
    }

    /// The store ladder: invalid input and the limit are board-specific 400s, the conflict is the
    /// shared channel id, a model error passes through, and a driver error is the 500.
    #[test]
    fn the_store_errors_map_in_gos_order() {
        let invalid = save_board_channel_error(StoreError::InvalidInput {
            entity: "Channel",
            field: "Id",
            value: "x".to_owned(),
        });
        assert_eq!(
            (invalid.id.as_str(), invalid.status_code),
            ("app.channel.create_board_channel.invalid.app_error", 400)
        );
        let limit = save_board_channel_error(StoreError::LimitExceeded {
            what: "channels",
            count: 3,
            details: String::new(),
        });
        assert_eq!(
            (limit.id.as_str(), limit.status_code),
            ("app.channel.create_board_channel.limit.app_error", 400)
        );
        let model = save_board_channel_error(StoreError::Invalid {
            entity: "Channel",
            app_error: AppError::boxed(
                "Channel.IsValid",
                "model.channel.is_valid.name.app_error",
                None,
                String::new(),
                400,
            ),
        });
        assert_eq!(model.id, "model.channel.is_valid.name.app_error");
        let other = save_board_channel_error(StoreError::NotFound {
            entity: "Channel",
            criteria: String::new(),
        });
        assert_eq!(
            (other.id.as_str(), other.status_code),
            (INTERNAL_ERROR, 500)
        );
    }
}
