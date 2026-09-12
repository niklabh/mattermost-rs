//! Port of the app layer behind the four **read** routes of `api4/properties.go` — the generic
//! PSAv2 property API: `App.GetPropertyGroup` (app/property_group.go:25),
//! `App.SearchPropertyFields` (app/property_field.go:192) and `App.SearchPropertyValues`
//! (app/property_value.go:101), as those routes reach them.
//!
//! [`crate::custom_profile_attributes`] is the same three calls pinned to one group and one
//! object type; this module is them with the group as an argument. The reason it is a separate
//! module rather than a generalisation of that one is the subject below: **the hooks are scoped
//! to a group, so the answer depends on which group was named in the URL.**
//!
//! # Which groups carry a hook, and which are simply readable
//!
//! `RegisterBuiltinGroups` (app/server.go:279) writes five rows unconditionally, on any edition.
//! The hooks that wrap them are registered afterwards, each against a **specific group id**:
//!
//! | group | version | hooks on the read path | what this server does |
//! |---|---|---|---|
//! | `access_control` | 2 | licence, access-control, attribute-validation | serve unlicensed, forward licensed |
//! | `boards` | 2 | **none** | serve, on any edition |
//! | `post_attributes` | 2 | **none** | serve, on any edition |
//! | `session_attributes` | 2 | none — but the handler's own gate refuses first | 501 while the flag is off |
//! | `content_flagging` | 1 | n/a — `getV2Group` refuses a v1 group | 404 |
//!
//! A sixth row, `managed_channel_categories`, is **version 3** and is registered somewhere other
//! than that list; `IsPSAv2()` is `Version == 2` exactly, so it is a 404 here like the v1 group.
//! Measured on the stack's database, not read off `server.go`.
//!
//! That table is why these routes are worth porting rather than forwarding wholesale. The CPA
//! family had to stop at the licence because every one of its calls was hooked. Two of the groups
//! here are not hooked at all, so their reads are ordinary reads and the whole predicate set —
//! cursors, delta mode, the channel/team hierarchy — is exercisable against Go on this stack.
//!
//! # The licence hook's shape, reproduced for the one group that has it
//!
//! `LicenseCheckHook.PostGetPropertyFields` and `PostGetPropertyValues` (app/properties/
//! license_check.go:161, :265) both **return `nil` for an empty slice** and read
//! `slice[0].GroupID` otherwise. So on an unlicensed server a search of `access_control` that
//! matches nothing is a real `200 []`, and the same search a row later is a `403`. Reproducing
//! only the 403 would break the empty case; reproducing only the 200 would leak rows.
//!
//! The caller must have established that no *other* hook can fire — in practice, that the
//! installation is unlicensed whenever the group is `access_control` — because the access-control
//! and attribute-validation hooks that run after the licence one exist only in Go. See
//! [`crate::App::search_property_fields`].

use mm_model::permission::{
    PERMISSION_MANAGE_CHANNEL_ROLES, PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM,
    PERMISSION_READ_CHANNEL, PERMISSION_VIEW_TEAM,
};
use mm_model::property_field::{
    PROPERTY_FIELD_TARGET_LEVEL_CHANNEL, PROPERTY_FIELD_TARGET_LEVEL_SYSTEM,
    PROPERTY_FIELD_TARGET_LEVEL_TEAM, PermissionLevel, PropertyField, PropertyFieldSearchOpts,
};
use mm_model::property_group::{ACCESS_CONTROL_PROPERTY_GROUP_NAME, PropertyGroup};
use mm_model::property_value::{PropertyValue, PropertyValueSearchOpts};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult};
use mm_store::PropertyStore;

use crate::App;
use crate::custom_profile_attributes::property_licence_refusal;

impl App {
    /// Port of `App.GetPropertyGroup` (app/property_group.go:25) for an arbitrary group name.
    ///
    /// One error id across both arms — `app.property_group.get.app_error` — and only the status
    /// differs: **404** for a missing group, 500 for anything else. The store collapses a driver
    /// failure into not-found (see [`mm_store::SqlPropertyStore::get_group`]), so the 500 arm is
    /// currently unreachable; it is written out because the id is shared and a reader would
    /// otherwise assume the 404 is the only answer.
    ///
    /// Go reads this through `PropertyService.GetPropertyGroup`, which goes **straight to the
    /// store** — it is `Group()`, used at startup, that consults the in-memory cache. So this is
    /// a query on every request on both sides, not a cache read, and a group inserted while both
    /// servers are up is visible to both immediately.
    #[tracing::instrument(skip_all, fields(name = %name, found))]
    pub async fn property_group(&self, name: &str) -> AppResult<PropertyGroup> {
        let group = self
            .store()
            .property()
            .get_group(name)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                if !not_found {
                    tracing::error!(error = ?err, "the property group lookup failed");
                }
                AppError::boxed(
                    "GetPropertyGroup",
                    "app.property_group.get.app_error",
                    None,
                    String::new(),
                    if not_found { 404 } else { 500 },
                )
            })?;

        tracing::Span::current().record("found", true);
        Ok(group)
    }

    /// Port of `App.SearchPropertyFields` (app/property_field.go:192) plus the one hook arm that
    /// can fire on this deployment, `LicenseCheckHook.PostGetPropertyFields`.
    ///
    /// # Precondition the type cannot hold
    ///
    /// The caller must already have decided that no unported hook applies: either `group` is not
    /// `access_control`, or the installation is unlicensed. `api4/properties.go`'s handlers make
    /// that decision before they get here, because the alternative — a licensed `access_control`
    /// read — runs the access-control hook's per-caller option filtering and the
    /// attribute-validation hook, neither of which exists on this side.
    ///
    /// # The empty short-circuit is the whole behaviour, not an optimisation
    ///
    /// `if len(fields) == 0 { return fields, nil }` runs **before** the licence is consulted, so
    /// an unlicensed `access_control` search answers `200 []` for a group with no matching row and
    /// `403` for a group with one. Two different statuses from the same request, decided by the
    /// database.
    #[tracing::instrument(skip_all, fields(group = %group.name, found))]
    pub async fn search_property_fields(
        &self,
        group: &PropertyGroup,
        opts: &PropertyFieldSearchOpts,
    ) -> AppResult<Vec<PropertyField>> {
        let fields = self
            .store()
            .property()
            .search_fields(opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the property field search failed");
                AppError::boxed(
                    "SearchPropertyFields",
                    "app.property_field.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", fields.len());
        if licence_hook_refuses(group, fields.is_empty()) {
            return Err(property_licence_refusal("SearchPropertyFields"));
        }
        Ok(fields)
    }

    /// Port of `App.SearchPropertyValues` (app/property_value.go:101) and
    /// `LicenseCheckHook.PostGetPropertyValues`, which has the same empty short-circuit as its
    /// field twin. Same precondition as [`App::search_property_fields`].
    #[tracing::instrument(skip_all, fields(group = %group.name, found))]
    pub async fn search_property_values(
        &self,
        group: &PropertyGroup,
        opts: &PropertyValueSearchOpts,
    ) -> AppResult<Vec<PropertyValue>> {
        let values = self
            .store()
            .property()
            .search_values(opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the property value search failed");
                AppError::boxed(
                    "SearchPropertyValues",
                    "app.property_value.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", values.len());
        if licence_hook_refuses(group, values.is_empty()) {
            return Err(property_licence_refusal("SearchPropertyValues"));
        }
        Ok(values)
    }

    /// Port of `App.GetPropertyField` (app/property_field.go:137) for a group that carries no
    /// post-get hook.
    ///
    /// The store collapses every failure into not-found, so this is 404
    /// `app.property.not_found.app_error` or a row — `mapPropertyServiceError`'s `*ErrNotFound`
    /// arm, not `GetPropertyField`'s own 500 fallback, which is unreachable from here.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, field_id = %field_id, found))]
    pub async fn get_property_field(
        &self,
        group_id: &str,
        field_id: &str,
    ) -> AppResult<PropertyField> {
        let field = self
            .store()
            .property()
            .get_field(group_id, field_id)
            .await
            .map_err(|err| {
                if !err.is_not_found() {
                    tracing::error!(error = ?err, "the property field read failed");
                }
                AppError::boxed(
                    "GetPropertyField",
                    "app.property.not_found.app_error",
                    None,
                    String::new(),
                    404,
                )
            })?;

        tracing::Span::current().record("found", true);
        Ok(field)
    }

    /// Port of `App.DeletePropertyField` (app/property_field.go:425) composed with the service's
    /// own `deletePropertyField` (app/properties/property_field.go:435), which is where two of
    /// the three steps live.
    ///
    /// In Go's order, and the order is the contract:
    ///
    /// 1. **Read the field.** The caller has already done this to run its permission check, so it
    ///    is passed in rather than re-read — Go reads it twice (once in the app layer, once
    ///    inside the service's group check) and the second read cannot fail if the first did not.
    /// 2. **Protected fields refuse with 403**, before anything is written.
    /// 3. **Linked dependents refuse with 409** `…delete.has_linked_dependents.app_error`. This
    ///    is the one refusal a caller can act on: unlink or delete the dependents, then retry.
    /// 4. **Cascade the values, then delete the field** — in that order, and the cascade ignores
    ///    how many rows it touched while the field delete treats zero as a 404.
    ///
    /// # The websocket event is addressed by the field's *target*, not its object type
    ///
    /// `propertyFieldBroadcastParams` (app/property_field.go:29) sends a `team` field to that
    /// team, a `channel` field to that channel, and a `system` field to **everyone** — a
    /// `WebSocketEvent` with neither a team nor a channel is broadcast unscoped. An unrecognised
    /// target type skips the broadcast entirely rather than falling back to unscoped, which is
    /// the safe direction and worth keeping: the fallback would leak a field definition.
    ///
    /// The payload is `field_id` and `object_type` — **not** the field itself, unlike the create
    /// and update events, which carry the whole encoded field.
    #[tracing::instrument(skip_all, fields(group = %group.name, field_id = %field.id, protected = field.protected))]
    pub async fn delete_property_field(
        &self,
        group: &PropertyGroup,
        field: &PropertyField,
        connection_id: &str,
    ) -> AppResult<()> {
        if field.protected {
            return Err(AppError::boxed(
                "DeletePropertyField",
                "app.property_field.delete.protected.app_error",
                None,
                String::new(),
                403,
            ));
        }

        let store = self.store();
        let linked = store
            .property()
            .count_linked_fields(&field.id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the linked-field count failed");
                AppError::boxed(
                    "DeletePropertyField",
                    "app.property_field.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        if linked > 0 {
            // **409**, and it is the only conflict status this family mints. The service returns a
            // bare `*model.AppError` here, which `mapPropertyServiceError` passes through
            // untouched via its `errors.As` arm rather than rewriting to a 500.
            return Err(AppError::boxed(
                "DeletePropertyField",
                "app.property_field.delete.has_linked_dependents.app_error",
                None,
                String::new(),
                409,
            ));
        }

        store
            .property()
            .delete_values_for_field(&group.id, &field.id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the property value cascade failed");
                AppError::boxed(
                    "DeletePropertyField",
                    "app.property_field.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        store
            .property()
            .delete_field(&group.id, &field.id)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                if !not_found {
                    tracing::error!(error = ?err, "the property field delete failed");
                }
                AppError::boxed(
                    "DeletePropertyField",
                    if not_found {
                        "app.property.not_found.app_error"
                    } else {
                        "app.property_field.delete.app_error"
                    },
                    None,
                    String::new(),
                    if not_found { 404 } else { 500 },
                )
            })?;

        self.publish_property_field_deleted(field, connection_id)
            .await;
        Ok(())
    }

    /// The `property_field_deleted` half of `publishPropertyFieldEvent`, which for a delete is
    /// written out inline in Go (app/property_field.go:463) rather than sharing the helper.
    ///
    /// Guarded on `IsPSAv2()` exactly as Go is: a v1 field is deleted silently.
    async fn publish_property_field_deleted(&self, field: &PropertyField, connection_id: &str) {
        if !field.is_psav2() {
            return;
        }
        let Some((team_id, channel_id)) = property_field_broadcast_params(field) else {
            tracing::warn!(
                target_type = %field.target_type,
                field_id = %field.id,
                "Unrecognized property field TargetType, skipping broadcast"
            );
            return;
        };
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_PROPERTY_FIELD_DELETED,
            team_id,
            channel_id,
            "",
            None,
            connection_id,
        );
        message.add("field_id", serde_json::Value::String(field.id.clone()));
        message.add(
            "object_type",
            serde_json::Value::String(field.object_type.clone()),
        );
        self.publish(message).await;
    }

    /// Port of `App.SessionHasPermissionToEditPropertyField` (app/authorization.go:514).
    ///
    /// Three refusals before any lookup, and each is a different reason to say no: a **protected**
    /// field is never editable by anyone, a field whose `PermissionField` is `nil` is a legacy
    /// (PSAv1) row with no permission model at all, and only then does an unrestricted session
    /// pass. Reordering the unrestricted check above the protected one would let local mode delete
    /// a protected field, which no session may do.
    pub async fn session_has_permission_to_edit_property_field(
        &self,
        session: &Session,
        field: &PropertyField,
    ) -> bool {
        if field.protected {
            return false;
        }
        let Some(level) = field.permission_field.as_ref() else {
            return false;
        };
        if session.is_unrestricted() {
            return true;
        }
        self.has_property_field_permission_level(&session.user_id, field, level)
            .await
    }

    /// Port of `App.hasPropertyFieldPermissionLevel` (app/authorization.go:607).
    ///
    /// `admin` resolves against the field's **target type**, not its object type: `manage_system`
    /// on a system target, `manage_team` on a team target, `manage_channel_roles` on a channel
    /// target. Go's own comment flags that this is stricter than `hasTargetAccess`, which uses
    /// `manage_*_channel_properties` — the outer gate says "you may write here at all" and this
    /// inner one says "you are an admin of this thing".
    ///
    /// An unknown level or an unknown target type is `false`, never a fallthrough to `true`.
    async fn has_property_field_permission_level(
        &self,
        user_id: &str,
        field: &PropertyField,
        level: &PermissionLevel,
    ) -> bool {
        match level.as_str() {
            PermissionLevel::NONE => false,
            PermissionLevel::SYSADMIN => {
                self.has_permission_to(user_id, &PERMISSION_MANAGE_SYSTEM)
                    .await
            }
            PermissionLevel::MEMBER => self.has_property_field_scope_access(user_id, field).await,
            PermissionLevel::ADMIN => match field.target_type.as_str() {
                PROPERTY_FIELD_TARGET_LEVEL_SYSTEM => {
                    self.has_permission_to(user_id, &PERMISSION_MANAGE_SYSTEM)
                        .await
                }
                PROPERTY_FIELD_TARGET_LEVEL_TEAM => {
                    self.has_permission_to_team(user_id, &field.target_id, &PERMISSION_MANAGE_TEAM)
                        .await
                }
                PROPERTY_FIELD_TARGET_LEVEL_CHANNEL => {
                    self.has_permission_to_channel(
                        user_id,
                        &field.target_id,
                        &PERMISSION_MANAGE_CHANNEL_ROLES,
                    )
                    .await
                    .0
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Port of `App.hasPropertyFieldScopeAccess` (app/authorization.go:723) — the `member` level.
    ///
    /// **A system-target field is readable-and-writable at member level by any authenticated
    /// user**, with no check at all. That is why `CanonicalizeSystemObjectField` pins all three
    /// permission levels of a *system-object* field to `sysadmin`: without it, a member-level
    /// system field would be world-writable.
    async fn has_property_field_scope_access(&self, user_id: &str, field: &PropertyField) -> bool {
        match field.target_type.as_str() {
            PROPERTY_FIELD_TARGET_LEVEL_SYSTEM => true,
            PROPERTY_FIELD_TARGET_LEVEL_TEAM => {
                self.has_permission_to_team(user_id, &field.target_id, &PERMISSION_VIEW_TEAM)
                    .await
            }
            PROPERTY_FIELD_TARGET_LEVEL_CHANNEL => {
                self.has_permission_to_channel(user_id, &field.target_id, &PERMISSION_READ_CHANNEL)
                    .await
                    .0
            }
            _ => false,
        }
    }
}

/// Port of `propertyFieldBroadcastParams` (app/property_field.go:29): `(team_id, channel_id)`, or
/// `None` for a target type the broadcast does not understand.
fn property_field_broadcast_params(field: &PropertyField) -> Option<(&str, &str)> {
    match field.target_type.as_str() {
        PROPERTY_FIELD_TARGET_LEVEL_TEAM => Some((field.target_id.as_str(), "")),
        PROPERTY_FIELD_TARGET_LEVEL_CHANNEL => Some(("", field.target_id.as_str())),
        PROPERTY_FIELD_TARGET_LEVEL_SYSTEM => Some(("", "")),
        _ => None,
    }
}

/// `LicenseCheckHook.requireLicense` (app/properties/license_check.go:45) folded together with the
/// empty short-circuit both post-get arms open with.
///
/// The group test is by **name**, where Go's is by id: the hook is constructed with
/// `cpaGroup.ID` (app/server.go:325), and that id is whatever row `access_control` occupies. Same
/// predicate, one lookup fewer, and it cannot go stale if the row is ever recreated.
///
/// The licence half is the caller's — see [`App::search_property_fields`] — so reaching this with
/// a licensed server and the managed group would be a bug in the handler, not here.
fn licence_hook_refuses(group: &PropertyGroup, result_is_empty: bool) -> bool {
    !result_is_empty && group.name == ACCESS_CONTROL_PROPERTY_GROUP_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, version: i64) -> PropertyGroup {
        PropertyGroup {
            id: "rrf4bnnxkbry9jdfp9kwau4xch".to_owned(),
            name: name.to_owned(),
            version,
            schema_version: 1,
        }
    }

    /// The empty short-circuit runs **before** the group is even looked at, so an unlicensed
    /// search of the managed group that matches nothing is a 200 and not a 403. Inverting this is
    /// the single most plausible mistake in the module and it turns five reachable 200s into
    /// refusals.
    #[test]
    fn an_empty_result_is_never_refused_even_on_the_managed_group() {
        assert!(!licence_hook_refuses(&group("access_control", 2), true));
        assert!(!licence_hook_refuses(&group("boards", 2), true));
    }

    /// A row in the managed group is a 403; the same row in any other group is not. This is the
    /// whole reason the two families are separate modules.
    #[test]
    fn only_the_access_control_group_is_licence_managed() {
        assert!(licence_hook_refuses(&group("access_control", 2), false));
        assert!(!licence_hook_refuses(&group("boards", 2), false));
        assert!(!licence_hook_refuses(&group("post_attributes", 2), false));
        assert!(!licence_hook_refuses(
            &group("session_attributes", 2),
            false
        ));
    }

    /// The deprecated CPA name is **not** the managed group. `custom_profile_attributes` is
    /// accepted by the plugin API for backward compatibility (property_group.go:17) and is not a
    /// row in this table, so a group by that name would carry no hook — matching Go, whose hook
    /// holds one id.
    #[test]
    fn the_deprecated_cpa_name_is_not_the_managed_group() {
        assert!(!licence_hook_refuses(
            &group("custom_profile_attributes", 2),
            false
        ));
    }
}
