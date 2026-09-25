//! The plugin API's property methods (app/plugin_api.go:1713-1985): groups, fields and values,
//! and the five `*WithOptions` variants — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! Every one returns a Go `error`, so what crosses is `encodableError`'s: the app layer's
//! `*AppError` whole (with `Where`, which gob carries), or an `ErrorString` for the two
//! `fmt.Errorf` refusals of the deprecated v1 CPA group name.
//!
//! # The hooked groups are not implemented
//!
//! Go's property service runs a hook chain on two groups: `access_control` (licence, access
//! control, attribute validation, value audit, field limit) and `session_attributes` (the schema
//! guard). Their plugin-caller arms — `isCallerPlugin`, owner identity, `ActingAsScope` — are
//! not ported ([D-1331]), so a call whose hooks would fire answers not implemented. Which id a
//! hook gates on is Go's, per method: the `groupID` argument for writes, counts and deletes; the
//! field's or value's own `GroupID` for creates and upserts; **the returned rows'** `GroupID` for
//! reads, which is why a read with an empty group id is checked after it runs. Every other group
//! — the ones plugins register (PSAv1), `boards`, `post_attributes` — carries no hook but the
//! type-change cleanup, and is answered in full.
//!
//! # `Value` is bytes, and which bytes is Go's
//!
//! A `json.RawMessage` crosses gob verbatim. After a read it is Postgres's rendering of the
//! `jsonb` column (`value::text`); `CreatePropertyValue` and `UpdatePropertyValue(s)` hand back
//! the value they were given after `SanitizePropertyValue`, which re-marshals only a string or a
//! string array it changed ([`sanitize_raw`]); `UpsertPropertyValue(s)` hand back the `RETURNING`
//! row. A value that is not JSON (Go would fail the insert with a 500) is not implemented.
//!
//! # What else is not implemented, per call
//!
//! `UpdatePropertyFields` with more than one field (the app function updates one); a nil field
//! to an update (Go dereferences it).

use std::collections::{HashMap, HashSet};

use gobwire::Interface;
use mm_model::property_field::{
    PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE, PermissionLevel, PropertyField, PropertyFieldSearchCursor,
    PropertyFieldSearchOpts, PropertyFieldType,
};
use mm_model::property_group::{
    ACCESS_CONTROL_PROPERTY_GROUP_NAME, DEPRECATED_CPA_PROPERTY_GROUP_NAME,
    PROPERTY_GROUP_VERSION_V1, PropertyGroup,
};
use mm_model::property_value::{
    PropertyValue, PropertyValueSearchCursor, PropertyValueSearchOpts, sanitize_property_value,
};
use mm_model::session_attributes::SESSION_ATTRIBUTES_PROPERTY_GROUP_NAME;
use mm_model::utils::{AppError, go_json_marshal, go_quote, is_valid_id};
use mm_model::websocket_message::{WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED, WebSocketEvent};
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model as wire;
use mm_plugin::wire::plugin as api;
use mm_store::{PropertyStore, RawPropertyValue, StoreError};

use super::AppPluginApi;
use crate::plugin_hooks::{HookContext, props_from_wire, props_to_wire};
use crate::property_hooks::{PropertyCaller, PropertyServiceError};

/// `fmt.Errorf` of `RegisterPropertyGroup` and `GetPropertyGroup` (plugin_api.go:1843) for the
/// deprecated v1 CPA group name.
pub fn deprecated_group_text() -> String {
    format!(
        "{} is a version 1 PSA group that has been deprecated; use the version 2 PSA group {} instead",
        go_quote(DEPRECATED_CPA_PROPERTY_GROUP_NAME),
        go_quote(ACCESS_CONTROL_PROPERTY_GROUP_NAME)
    )
}

/// A `model.PropertyField` as gob sends it: `Attrs` as the `map[string]any` Go's JSON decode of
/// the column left, every number a `float64`.
pub fn property_field_to_wire(field: &PropertyField) -> wire::PropertyField {
    wire::PropertyField {
        id: field.id.clone(),
        group_id: field.group_id.clone(),
        name: field.name.clone(),
        r#type: field.type_.0.clone(),
        attrs: props_to_wire(field.attrs.as_ref()),
        target_id: field.target_id.clone(),
        target_type: field.target_type.clone(),
        object_type: field.object_type.clone(),
        protected: field.protected,
        permission_field: field.permission_field.as_ref().map(|p| p.0.clone()),
        permission_values: field.permission_values.as_ref().map(|p| p.0.clone()),
        permission_options: field.permission_options.as_ref().map(|p| p.0.clone()),
        linked_field_id: field.linked_field_id.clone(),
        create_at: field.create_at,
        update_at: field.update_at,
        delete_at: field.delete_at,
        created_by: field.created_by.clone(),
        updated_by: field.updated_by.clone(),
    }
}

/// The reverse. gob omits an empty map, so an empty `Attrs` is Go's nil.
pub fn property_field_from_wire(wire: &wire::PropertyField) -> PropertyField {
    PropertyField {
        id: wire.id.clone(),
        group_id: wire.group_id.clone(),
        name: wire.name.clone(),
        type_: PropertyFieldType(wire.r#type.clone()),
        attrs: (!wire.attrs.is_empty()).then(|| props_from_wire(&wire.attrs)),
        target_id: wire.target_id.clone(),
        target_type: wire.target_type.clone(),
        object_type: wire.object_type.clone(),
        protected: wire.protected,
        permission_field: wire.permission_field.clone().map(PermissionLevel),
        permission_values: wire.permission_values.clone().map(PermissionLevel),
        permission_options: wire.permission_options.clone().map(PermissionLevel),
        linked_field_id: wire.linked_field_id.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        created_by: wire.created_by.clone(),
        updated_by: wire.updated_by.clone(),
    }
}

fn property_group_to_wire(group: &PropertyGroup) -> wire::PropertyGroup {
    wire::PropertyGroup {
        id: group.id.clone(),
        name: group.name.clone(),
        version: group.version,
        schema_version: group.schema_version,
    }
}

/// A value as gob sends it, with `raw` as its `Value` bytes.
fn property_value_to_wire(value: &PropertyValue, raw: Vec<u8>) -> wire::PropertyValue {
    wire::PropertyValue {
        id: value.id.clone(),
        target_id: value.target_id.clone(),
        target_type: value.target_type.clone(),
        group_id: value.group_id.clone(),
        field_id: value.field_id.clone(),
        value: raw,
        create_at: value.create_at,
        update_at: value.update_at,
        delete_at: value.delete_at,
        created_by: value.created_by.clone(),
        updated_by: value.updated_by.clone(),
    }
}

fn raw_to_wire(raw: RawPropertyValue) -> wire::PropertyValue {
    property_value_to_wire(&raw.value, raw.raw.into_bytes())
}

/// Port of `model.SanitizePropertyValue` (property_value.go:212) on the bytes: the input is kept
/// verbatim unless it is a string or a string array the sanitiser changed, which is re-marshalled
/// as `json.Marshal` writes it. `Err` for empty bytes or bytes that are not JSON.
pub fn sanitize_raw(raw: &[u8]) -> Result<(serde_json::Value, Vec<u8>), &'static str> {
    if raw.is_empty() {
        return Err("a nil value");
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| "a value that is not JSON")?;
    let sanitized = sanitize_property_value(&parsed);
    if sanitized == parsed {
        return Ok((parsed, raw.to_vec()));
    }
    let text = go_json_marshal(&sanitized).map_err(|_| "a value that does not marshal")?;
    Ok((sanitized, text.into_bytes()))
}

/// A plugin's value, its bytes sanitised; `Err` as [`sanitize_raw`].
fn property_value_from_wire(
    wire: &wire::PropertyValue,
) -> Result<(PropertyValue, Vec<u8>), &'static str> {
    let (value, raw) = sanitize_raw(&wire.value)?;
    Ok((
        PropertyValue {
            id: wire.id.clone(),
            target_id: wire.target_id.clone(),
            target_type: wire.target_type.clone(),
            group_id: wire.group_id.clone(),
            field_id: wire.field_id.clone(),
            value,
            create_at: wire.create_at,
            update_at: wire.update_at,
            delete_at: wire.delete_at,
            created_by: wire.created_by.clone(),
            updated_by: wire.updated_by.clone(),
        },
        raw,
    ))
}

fn field_search_opts_from_wire(
    group_id: &str,
    wire: &wire::PropertyFieldSearchOpts,
) -> PropertyFieldSearchOpts {
    PropertyFieldSearchOpts {
        // `searchPropertyFields` overwrites the caller's (property_field.go:230).
        group_id: group_id.to_owned(),
        object_type: wire.object_type.clone(),
        object_types: wire.object_types.clone(),
        target_type: wire.target_type.clone(),
        target_ids: wire.target_i_ds.clone(),
        channel_id: wire.channel_id.clone(),
        team_id: wire.team_id.clone(),
        linked_field_id: wire.linked_field_id.clone(),
        since_update_at: wire.since_update_at,
        include_deleted: wire.include_deleted,
        cursor: PropertyFieldSearchCursor {
            property_field_id: wire.cursor.property_field_id.clone(),
            create_at: wire.cursor.create_at,
            update_at: wire.cursor.update_at,
        },
        per_page: wire.per_page,
    }
}

fn value_search_opts_from_wire(
    group_id: &str,
    wire: &wire::PropertyValueSearchOpts,
) -> Result<PropertyValueSearchOpts, &'static str> {
    let value = if wire.value.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&wire.value).map_err(|_| "a value filter that is not JSON")?)
    };
    Ok(PropertyValueSearchOpts {
        group_id: group_id.to_owned(),
        target_type: wire.target_type.clone(),
        target_ids: wire.target_i_ds.clone(),
        field_id: wire.field_id.clone(),
        since_update_at: wire.since_update_at,
        include_deleted: wire.include_deleted,
        cursor: PropertyValueSearchCursor {
            property_value_id: wire.cursor.property_value_id.clone(),
            create_at: wire.cursor.create_at,
            update_at: wire.cursor.update_at,
        },
        per_page: wire.per_page,
        value,
    })
}

/// `app.property_<kind>.invalid_input.app_error` (property_field.go:103, property_value.go:43).
fn invalid_input(where_: &'static str, kind: &str, detail: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        format!("app.property_{kind}.invalid_input.app_error"),
        None,
        detail,
        400,
    )
}

impl AppPluginApi {
    /// The caller `psaPluginContext` (plugin_api.go:1702) names: the plugin, with the
    /// `*WithOptions` scope when there is one.
    fn property_caller(&self, options: Option<&wire::PropertyRequestOptions>) -> PropertyCaller {
        PropertyCaller {
            id: self.id.clone(),
            acting_as_scope: options
                .map(|o| o.acting_as_scope.clone())
                .unwrap_or_default(),
        }
    }

    /// The ids of the two groups Go's hooks are constructed with (app/server.go:316-367).
    async fn hooked_group_ids(&self) -> Vec<String> {
        let mut ids = Vec::with_capacity(2);
        for name in [
            ACCESS_CONTROL_PROPERTY_GROUP_NAME,
            SESSION_ATTRIBUTES_PROPERTY_GROUP_NAME,
        ] {
            if let Ok(group) = self.app.store().property().get_group(name).await {
                ids.push(group.id);
            }
        }
        ids
    }

    /// `Err(NotImplemented)` when any of `group_ids` is a hooked group.
    async fn refuse_hooked<'a>(
        &self,
        method: &'static str,
        group_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), NotImplemented> {
        let hooked = self.hooked_group_ids().await;
        if group_ids
            .into_iter()
            .any(|id| hooked.iter().any(|h| h == id))
        {
            return Err(self.not_implemented(method, "a group the property hooks manage (D-1331)"));
        }
        Ok(())
    }

    /// A field write's hook check: the `groupID` argument, and — when it is empty, so the reads
    /// run unscoped and their post-get hooks gate on the row — the field's own group.
    async fn refuse_hooked_field(
        &self,
        method: &'static str,
        group_id: &str,
        field_id: &str,
    ) -> Result<(), NotImplemented> {
        let mut ids = vec![group_id.to_owned()];
        if group_id.is_empty()
            && let Ok(field) = self
                .app
                .store()
                .property()
                .get_field_in_any_group(field_id)
                .await
        {
            ids.push(field.group_id);
        }
        self.refuse_hooked(method, ids.iter().map(String::as_str))
            .await
    }

    /// The group an app function is handed: the row, or — for an id with no row, which Go's
    /// hooks simply do not match — a bare group carrying the id.
    async fn group_or_bare(&self, id: &str) -> PropertyGroup {
        self.app
            .store()
            .property()
            .get_group_by_id(id)
            .await
            .unwrap_or_else(|_| PropertyGroup {
                id: id.to_owned(),
                ..PropertyGroup::default()
            })
    }

    fn property_error(
        &self,
        err: PropertyServiceError,
        where_: &'static str,
        fallback_id: &'static str,
    ) -> Option<Interface> {
        self.app_error_as_error(err.into_app_error(where_, fallback_id))
    }

    fn store_error(
        &self,
        err: StoreError,
        where_: &'static str,
        fallback_id: &'static str,
    ) -> Option<Interface> {
        self.property_error(PropertyServiceError::Store(err), where_, fallback_id)
    }

    /// Port of `rejectTemplateValues` (app/properties/property_value.go:21): the values' fields
    /// read by id across every group — a missing one is `ErrResultsMismatch`, the 404 — and any
    /// `template` field among them refused, in the order the rows came back.
    async fn reject_template_values(
        &self,
        values: &[PropertyValue],
    ) -> Result<(), PropertyServiceError> {
        let mut seen = HashSet::new();
        let ids: Vec<String> = values
            .iter()
            .filter(|v| seen.insert(v.field_id.as_str()))
            .map(|v| v.field_id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let fields = self
            .app
            .store()
            .property()
            .get_many_fields("", &ids)
            .await?;
        if fields.len() < ids.len() {
            return Err(PropertyServiceError::Store(StoreError::NotFound {
                entity: "PropertyField",
                criteria: String::new(),
            }));
        }
        if let Some(field) = fields
            .iter()
            .find(|f| f.object_type == PROPERTY_FIELD_OBJECT_TYPE_TEMPLATE)
        {
            return Err(PropertyServiceError::App(AppError::boxed(
                "PropertyService",
                "app.property_value.template_no_values.app_error",
                None,
                format!("template field {} cannot have values", go_quote(&field.id)),
                400,
            )));
        }
        Ok(())
    }

    /// Publishes `property_values_updated` scoped by `resolveValueBroadcastParams`; a failure to
    /// resolve is logged and nothing is sent, as Go does.
    async fn publish_values_updated(&self, object_type: &str, target_id: &str, values: String) {
        let (team_id, channel_id) = match self
            .app
            .resolve_value_broadcast_params(&HookContext::default(), object_type, target_id)
            .await
        {
            Ok(scope) => scope,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to resolve broadcast params for property value deletion");
                return;
            }
        };
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED,
            &team_id,
            &channel_id,
            "",
            None,
            "",
        );
        message.add(
            "object_type",
            serde_json::Value::String(object_type.to_owned()),
        );
        message.add("target_id", serde_json::Value::String(target_id.to_owned()));
        message.add("values", serde_json::Value::String(values));
        self.app.publish(message).await;
    }

    // -- groups -------------------------------------------------------------------------------

    /// Port of `PluginAPI.RegisterPropertyGroup` (plugin_api.go:1843) and `App
    /// .RegisterPropertyGroup`: a v1 group, the existing row on a name conflict, and **every**
    /// failure — an invalid name included — the 500, since the app function does not look inside.
    pub(super) async fn properties_register_group(
        &self,
        args: api::Z_RegisterPropertyGroupArgs,
    ) -> Result<api::Z_RegisterPropertyGroupReturns, NotImplemented> {
        if args.a == DEPRECATED_CPA_PROPERTY_GROUP_NAME {
            return Ok(api::Z_RegisterPropertyGroupReturns {
                a: None,
                b: Self::message_as_error(deprecated_group_text()),
            });
        }
        let group = PropertyGroup {
            name: args.a,
            version: PROPERTY_GROUP_VERSION_V1,
            ..PropertyGroup::default()
        };
        Ok(
            match self.app.store().property().register_group(group).await {
                Ok(group) => api::Z_RegisterPropertyGroupReturns {
                    a: Some(Box::new(property_group_to_wire(&group))),
                    b: None,
                },
                Err(err) => {
                    tracing::error!(error = ?err, "a plugin's property group registration failed");
                    api::Z_RegisterPropertyGroupReturns {
                        a: None,
                        b: self.app_error_as_error(AppError::boxed(
                            "RegisterPropertyGroup",
                            "app.property_group.register.app_error",
                            None,
                            String::new(),
                            500,
                        )),
                    }
                }
            },
        )
    }

    /// Port of `PluginAPI.GetPropertyGroup` (plugin_api.go:1861): [`crate::App::property_group`].
    pub(super) async fn properties_get_group(
        &self,
        args: api::Z_GetPropertyGroupArgs,
    ) -> Result<api::Z_GetPropertyGroupReturns, NotImplemented> {
        if args.a == DEPRECATED_CPA_PROPERTY_GROUP_NAME {
            return Ok(api::Z_GetPropertyGroupReturns {
                a: None,
                b: Self::message_as_error(deprecated_group_text()),
            });
        }
        Ok(match self.app.property_group(&args.a).await {
            Ok(group) => api::Z_GetPropertyGroupReturns {
                a: Some(Box::new(property_group_to_wire(&group))),
                b: None,
            },
            Err(err) => api::Z_GetPropertyGroupReturns {
                a: None,
                b: self.app_error_as_error(err),
            },
        })
    }

    // -- fields -------------------------------------------------------------------------------

    /// Port of `PluginAPI.CreatePropertyField` (plugin_api.go:1713): `App.CreatePropertyField`
    /// without the protected bypass ([`crate::App::cpa_create_field`]).
    pub(super) async fn properties_create_field(
        &self,
        args: api::Z_CreatePropertyFieldArgs,
    ) -> Result<api::Z_CreatePropertyFieldReturns, NotImplemented> {
        const WHERE: &str = "CreatePropertyField";
        let Some(wire_field) = args.a.as_deref() else {
            return Ok(api::Z_CreatePropertyFieldReturns {
                a: None,
                b: self.app_error_as_error(invalid_input(
                    WHERE,
                    "field",
                    "property field is required",
                )),
            });
        };
        let field = property_field_from_wire(wire_field);
        self.refuse_hooked(WHERE, [field.group_id.as_str()]).await?;
        let group = self.group_or_bare(&field.group_id).await;
        let caller = self.property_caller(None);
        Ok(
            match self.app.cpa_create_field(&group, &caller, field, "").await {
                Ok(created) => api::Z_CreatePropertyFieldReturns {
                    a: Some(Box::new(property_field_to_wire(&created))),
                    b: None,
                },
                Err(err) => api::Z_CreatePropertyFieldReturns {
                    a: None,
                    b: self.app_error_as_error(err),
                },
            },
        )
    }

    /// Port of `PluginAPI.GetPropertyField` (plugin_api.go:1721): the store's `Get` — any group
    /// when the id is empty, deleted rows included — then the row's hook check.
    pub(super) async fn properties_get_field(
        &self,
        args: api::Z_GetPropertyFieldArgs,
    ) -> Result<api::Z_GetPropertyFieldReturns, NotImplemented> {
        const WHERE: &str = "GetPropertyField";
        Ok(
            match self
                .app
                .store()
                .property()
                .get_field(&args.a, &args.b)
                .await
            {
                Ok(field) => {
                    self.refuse_hooked(WHERE, [field.group_id.as_str()]).await?;
                    api::Z_GetPropertyFieldReturns {
                        a: Some(Box::new(property_field_to_wire(&field))),
                        b: None,
                    }
                }
                Err(err) => api::Z_GetPropertyFieldReturns {
                    a: None,
                    b: self.store_error(err, WHERE, "app.property_field.get.app_error"),
                },
            },
        )
    }

    /// Port of `PluginAPI.GetPropertyFields` (plugin_api.go:1729): fewer rows than ids is the
    /// 404 `app.property_field.not_found.app_error` (`ErrFieldNotFound`).
    pub(super) async fn properties_get_fields(
        &self,
        args: api::Z_GetPropertyFieldsArgs,
    ) -> Result<api::Z_GetPropertyFieldsReturns, NotImplemented> {
        const WHERE: &str = "GetPropertyFields";
        Ok(
            match self.app.get_property_fields_raw(&args.a, &args.b).await {
                Ok(fields) => {
                    self.refuse_hooked(WHERE, fields.iter().map(|f| f.group_id.as_str()))
                        .await?;
                    api::Z_GetPropertyFieldsReturns {
                        a: fields.iter().map(property_field_to_wire).collect(),
                        b: None,
                    }
                }
                Err(err) => api::Z_GetPropertyFieldsReturns {
                    a: Vec::new(),
                    b: self.property_error(err, WHERE, "app.property_field.get_many.app_error"),
                },
            },
        )
    }

    /// Port of `PluginAPI.GetPropertyFieldByName` (plugin_api.go:1877): group, target and name,
    /// live rows, any object type.
    pub(super) async fn properties_get_field_by_name(
        &self,
        args: api::Z_GetPropertyFieldByNameArgs,
    ) -> Result<api::Z_GetPropertyFieldByNameReturns, NotImplemented> {
        const WHERE: &str = "GetPropertyFieldByName";
        Ok(
            match self
                .app
                .store()
                .property()
                .get_field_by_name(&args.a, &args.b, &args.c)
                .await
            {
                Ok(field) => {
                    self.refuse_hooked(WHERE, [field.group_id.as_str()]).await?;
                    api::Z_GetPropertyFieldByNameReturns {
                        a: Some(Box::new(property_field_to_wire(&field))),
                        b: None,
                    }
                }
                Err(err) => api::Z_GetPropertyFieldByNameReturns {
                    a: None,
                    b: self.store_error(err, WHERE, "app.property_field.get_by_name.app_error"),
                },
            },
        )
    }

    /// Port of `PluginAPI.SearchPropertyFields` (plugin_api.go:1753): the group argument replaces
    /// the options' own; invalid options and a per-page below one are the 500.
    pub(super) async fn properties_search_fields(
        &self,
        args: api::Z_SearchPropertyFieldsArgs,
    ) -> Result<api::Z_SearchPropertyFieldsReturns, NotImplemented> {
        const WHERE: &str = "SearchPropertyFields";
        let opts = field_search_opts_from_wire(&args.a, &args.b);
        Ok(
            match self.app.store().property().search_fields(&opts).await {
                Ok(fields) => {
                    self.refuse_hooked(WHERE, fields.iter().map(|f| f.group_id.as_str()))
                        .await?;
                    api::Z_SearchPropertyFieldsReturns {
                        a: fields.iter().map(property_field_to_wire).collect(),
                        b: None,
                    }
                }
                Err(err) => {
                    tracing::error!(error = ?err, "a plugin's property field search failed");
                    api::Z_SearchPropertyFieldsReturns {
                        a: Vec::new(),
                        b: self.app_error_as_error(AppError::boxed(
                            WHERE,
                            "app.property_field.search.app_error",
                            None,
                            String::new(),
                            500,
                        )),
                    }
                }
            },
        )
    }

    /// Port of `PluginAPI.CountPropertyFields` (plugin_api.go:1761) → `App
    /// .CountPropertyFieldsForGroup`.
    pub(super) async fn properties_count_fields(
        &self,
        args: api::Z_CountPropertyFieldsArgs,
    ) -> Result<api::Z_CountPropertyFieldsReturns, NotImplemented> {
        const WHERE: &str = "CountPropertyFieldsForGroup";
        self.refuse_hooked(WHERE, [args.a.as_str()]).await?;
        Ok(
            match self
                .app
                .store()
                .property()
                .count_fields_for_group(&args.a, args.b)
                .await
            {
                Ok(count) => api::Z_CountPropertyFieldsReturns { a: count, b: None },
                Err(err) => api::Z_CountPropertyFieldsReturns {
                    a: 0,
                    b: self.store_error(err, WHERE, "app.property_field.count_for_group.app_error"),
                },
            },
        )
    }

    /// Port of `PluginAPI.CountPropertyFieldsForTarget` (plugin_api.go:1769).
    pub(super) async fn properties_count_fields_for_target(
        &self,
        args: api::Z_CountPropertyFieldsForTargetArgs,
    ) -> Result<api::Z_CountPropertyFieldsForTargetReturns, NotImplemented> {
        const WHERE: &str = "CountPropertyFieldsForTarget";
        self.refuse_hooked(WHERE, [args.a.as_str()]).await?;
        Ok(
            match self
                .app
                .store()
                .property()
                .count_fields_for_target(&args.a, &args.b, &args.c, args.d)
                .await
            {
                Ok(count) => api::Z_CountPropertyFieldsForTargetReturns { a: count, b: None },
                Err(err) => api::Z_CountPropertyFieldsForTargetReturns {
                    a: 0,
                    b: self.store_error(
                        err,
                        WHERE,
                        "app.property_field.count_for_target.app_error",
                    ),
                },
            },
        )
    }

    /// The one-field update behind `UpdatePropertyField` and `UpdatePropertyFields`
    /// ([`crate::App::cpa_update_field`]).
    async fn update_one_field(
        &self,
        method: &'static str,
        group_id: &str,
        field: PropertyField,
    ) -> Result<Result<PropertyField, Box<AppError>>, NotImplemented> {
        self.refuse_hooked_field(method, group_id, &field.id)
            .await?;
        let group = self.group_or_bare(group_id).await;
        let caller = self.property_caller(None);
        Ok(self
            .app
            .cpa_update_field(&group, &caller, field, "")
            .await
            .map(|update| update.field))
    }

    /// Port of `PluginAPI.UpdatePropertyField` (plugin_api.go:1737).
    pub(super) async fn properties_update_field(
        &self,
        args: api::Z_UpdatePropertyFieldArgs,
    ) -> Result<api::Z_UpdatePropertyFieldReturns, NotImplemented> {
        let Some(wire_field) = args.b.as_deref() else {
            return Err(self.not_implemented("UpdatePropertyField", "a nil field"));
        };
        let field = property_field_from_wire(wire_field);
        Ok(
            match self
                .update_one_field("UpdatePropertyField", &args.a, field)
                .await?
            {
                Ok(updated) => api::Z_UpdatePropertyFieldReturns {
                    a: Some(Box::new(property_field_to_wire(&updated))),
                    b: None,
                },
                Err(err) => api::Z_UpdatePropertyFieldReturns {
                    a: None,
                    b: self.app_error_as_error(err),
                },
            },
        )
    }

    /// Port of `PluginAPI.UpdatePropertyFields` (plugin_api.go:1885): none is the 400; one is
    /// `UpdatePropertyField`'s path; more is not implemented.
    pub(super) async fn properties_update_fields(
        &self,
        args: api::Z_UpdatePropertyFieldsArgs,
    ) -> Result<api::Z_UpdatePropertyFieldsReturns, NotImplemented> {
        const WHERE: &str = "UpdatePropertyFields";
        let field = match args.b.as_slice() {
            [] => {
                return Ok(api::Z_UpdatePropertyFieldsReturns {
                    a: Vec::new(),
                    b: self.app_error_as_error(invalid_input(
                        WHERE,
                        "field",
                        "property fields are required",
                    )),
                });
            }
            [field] => property_field_from_wire(field),
            _ => return Err(self.not_implemented(WHERE, "more than one field")),
        };
        Ok(match self.update_one_field(WHERE, &args.a, field).await? {
            Ok(updated) => api::Z_UpdatePropertyFieldsReturns {
                a: vec![property_field_to_wire(&updated)],
                b: None,
            },
            Err(err) => api::Z_UpdatePropertyFieldsReturns {
                a: Vec::new(),
                b: self.app_error_as_error(err),
            },
        })
    }

    /// Port of `PluginAPI.DeletePropertyField` (plugin_api.go:1745)
    /// ([`crate::App::delete_property_field_with_hooks`]).
    pub(super) async fn properties_delete_field(
        &self,
        args: api::Z_DeletePropertyFieldArgs,
    ) -> Result<api::Z_DeletePropertyFieldReturns, NotImplemented> {
        const WHERE: &str = "DeletePropertyField";
        self.refuse_hooked_field(WHERE, &args.a, &args.b).await?;
        let group = self.group_or_bare(&args.a).await;
        let caller = self.property_caller(None);
        Ok(api::Z_DeletePropertyFieldReturns {
            a: self
                .app
                .delete_property_field_with_hooks(&group, &caller, &args.b, "")
                .await
                .err()
                .and_then(|err| self.app_error_as_error(err)),
        })
    }

    // -- values -------------------------------------------------------------------------------

    /// Port of `PluginAPI.CreatePropertyValue` (plugin_api.go:1777) and `App
    /// .CreatePropertyValue`: sanitise, the template refusal, the store's `Create` — an id already
    /// set is its `ErrInvalidInput`, the 500 — and the value as given back.
    pub(super) async fn properties_create_value(
        &self,
        args: api::Z_CreatePropertyValueArgs,
    ) -> Result<api::Z_CreatePropertyValueReturns, NotImplemented> {
        const WHERE: &str = "CreatePropertyValue";
        let Some(wire_value) = args.a.as_deref() else {
            return Ok(api::Z_CreatePropertyValueReturns {
                a: None,
                b: self.app_error_as_error(invalid_input(
                    WHERE,
                    "value",
                    "property value is required",
                )),
            });
        };
        let (value, raw) =
            property_value_from_wire(wire_value).map_err(|why| self.not_implemented(WHERE, why))?;
        self.refuse_hooked(WHERE, [value.group_id.as_str()]).await?;
        let created = async {
            self.reject_template_values(std::slice::from_ref(&value))
                .await?;
            Ok::<_, PropertyServiceError>(self.app.store().property().create_value(value).await?)
        }
        .await;
        Ok(match created {
            Ok(created) => api::Z_CreatePropertyValueReturns {
                a: Some(Box::new(property_value_to_wire(&created, raw))),
                b: None,
            },
            Err(err) => api::Z_CreatePropertyValueReturns {
                a: None,
                b: self.property_error(err, WHERE, "app.property_value.create.app_error"),
            },
        })
    }

    /// Port of `PluginAPI.GetPropertyValue` (plugin_api.go:1785): the store's `Get` — no
    /// `DeleteAt` filter, any group when the id is empty.
    pub(super) async fn properties_get_value(
        &self,
        args: api::Z_GetPropertyValueArgs,
    ) -> Result<api::Z_GetPropertyValueReturns, NotImplemented> {
        const WHERE: &str = "GetPropertyValue";
        Ok(
            match self
                .app
                .store()
                .property()
                .get_value_raw(&args.a, &args.b)
                .await
            {
                Ok(raw) => {
                    self.refuse_hooked(WHERE, [raw.value.group_id.as_str()])
                        .await?;
                    api::Z_GetPropertyValueReturns {
                        a: Some(Box::new(raw_to_wire(raw))),
                        b: None,
                    }
                }
                Err(err) => api::Z_GetPropertyValueReturns {
                    a: None,
                    b: self.store_error(err, WHERE, "app.property_value.get.app_error"),
                },
            },
        )
    }

    /// Port of `PluginAPI.GetPropertyValues` (plugin_api.go:1793): fewer rows than ids is
    /// `ErrResultsMismatch`, the 404 `app.property.not_found.app_error`.
    pub(super) async fn properties_get_values(
        &self,
        args: api::Z_GetPropertyValuesArgs,
    ) -> Result<api::Z_GetPropertyValuesReturns, NotImplemented> {
        const WHERE: &str = "GetPropertyValues";
        let read = match self
            .app
            .store()
            .property()
            .get_many_values_raw(&args.a, &args.b)
            .await
        {
            Ok(rows) if rows.len() < args.b.len() => Err(StoreError::NotFound {
                entity: "PropertyValue",
                criteria: String::new(),
            }),
            other => other,
        };
        Ok(match read {
            Ok(rows) => {
                self.refuse_hooked(WHERE, rows.iter().map(|r| r.value.group_id.as_str()))
                    .await?;
                api::Z_GetPropertyValuesReturns {
                    a: rows.into_iter().map(raw_to_wire).collect(),
                    b: None,
                }
            }
            Err(err) => api::Z_GetPropertyValuesReturns {
                a: Vec::new(),
                b: self.store_error(err, WHERE, "app.property_value.get_many.app_error"),
            },
        })
    }

    /// Port of `PluginAPI.SearchPropertyValues` (plugin_api.go:1835).
    pub(super) async fn properties_search_values(
        &self,
        args: api::Z_SearchPropertyValuesArgs,
    ) -> Result<api::Z_SearchPropertyValuesReturns, NotImplemented> {
        const WHERE: &str = "SearchPropertyValues";
        let opts = value_search_opts_from_wire(&args.a, &args.b)
            .map_err(|why| self.not_implemented(WHERE, why))?;
        Ok(
            match self.app.store().property().search_values_raw(&opts).await {
                Ok(rows) => {
                    self.refuse_hooked(WHERE, rows.iter().map(|r| r.value.group_id.as_str()))
                        .await?;
                    api::Z_SearchPropertyValuesReturns {
                        a: rows.into_iter().map(raw_to_wire).collect(),
                        b: None,
                    }
                }
                Err(err) => {
                    tracing::error!(error = ?err, "a plugin's property value search failed");
                    api::Z_SearchPropertyValuesReturns {
                        a: Vec::new(),
                        b: self.app_error_as_error(AppError::boxed(
                            WHERE,
                            "app.property_value.search.app_error",
                            None,
                            String::new(),
                            500,
                        )),
                    }
                }
            },
        )
    }

    /// The update behind `UpdatePropertyValue` and `UpdatePropertyValues`: `App
    /// .UpdatePropertyValues`' sanitising, the service's one-group rule (a plain error, the 500),
    /// the template refusal and the store's `Update`, which hands back the values given.
    async fn update_values(
        &self,
        method: &'static str,
        fallback_id: &'static str,
        group_id: &str,
        wire_values: &[wire::PropertyValue],
    ) -> Result<Result<Vec<wire::PropertyValue>, Option<Interface>>, NotImplemented> {
        let mut values = Vec::with_capacity(wire_values.len());
        let mut raws = Vec::with_capacity(wire_values.len());
        for wire_value in wire_values {
            let (value, raw) = property_value_from_wire(wire_value)
                .map_err(|why| self.not_implemented(method, why))?;
            values.push(value);
            raws.push(raw);
        }
        if values.iter().any(|v| v.group_id != values[0].group_id) {
            tracing::error!(method, "mixed group IDs in batch");
            return Ok(Err(self.app_error_as_error(AppError::boxed(
                method,
                fallback_id,
                None,
                String::new(),
                500,
            ))));
        }
        self.refuse_hooked(method, [group_id]).await?;
        let updated = async {
            self.reject_template_values(&values).await?;
            Ok::<_, PropertyServiceError>(
                self.app
                    .store()
                    .property()
                    .update_values(group_id, values)
                    .await?,
            )
        }
        .await;
        Ok(match updated {
            Ok(updated) => Ok(updated
                .iter()
                .zip(raws)
                .map(|(value, raw)| property_value_to_wire(value, raw))
                .collect()),
            Err(err) => Err(self.property_error(err, method, fallback_id)),
        })
    }

    /// Port of `PluginAPI.UpdatePropertyValue` (plugin_api.go:1801).
    pub(super) async fn properties_update_value(
        &self,
        args: api::Z_UpdatePropertyValueArgs,
    ) -> Result<api::Z_UpdatePropertyValueReturns, NotImplemented> {
        const WHERE: &str = "UpdatePropertyValue";
        let Some(wire_value) = args.b.as_deref() else {
            return Ok(api::Z_UpdatePropertyValueReturns {
                a: None,
                b: self.app_error_as_error(invalid_input(
                    WHERE,
                    "value",
                    "property value is required",
                )),
            });
        };
        Ok(
            match self
                .update_values(
                    WHERE,
                    "app.property_value.update.app_error",
                    &args.a,
                    std::slice::from_ref(wire_value),
                )
                .await?
            {
                Ok(mut values) => api::Z_UpdatePropertyValueReturns {
                    a: values.pop().map(Box::new),
                    b: None,
                },
                Err(b) => api::Z_UpdatePropertyValueReturns { a: None, b },
            },
        )
    }

    /// Port of `PluginAPI.UpdatePropertyValues` (plugin_api.go:1893).
    pub(super) async fn properties_update_values(
        &self,
        args: api::Z_UpdatePropertyValuesArgs,
    ) -> Result<api::Z_UpdatePropertyValuesReturns, NotImplemented> {
        const WHERE: &str = "UpdatePropertyValues";
        if args.b.is_empty() {
            return Ok(api::Z_UpdatePropertyValuesReturns {
                a: Vec::new(),
                b: self.app_error_as_error(invalid_input(
                    WHERE,
                    "value",
                    "property values are required",
                )),
            });
        }
        Ok(
            match self
                .update_values(
                    WHERE,
                    "app.property_value.update_many.app_error",
                    &args.a,
                    &args.b,
                )
                .await?
            {
                Ok(a) => api::Z_UpdatePropertyValuesReturns { a, b: None },
                Err(b) => api::Z_UpdatePropertyValuesReturns { a: Vec::new(), b },
            },
        )
    }

    /// The upsert behind the four upsert methods. `batch` is `App.UpsertPropertyValues`' own
    /// checks with an empty object type — one group, a valid and distinct field id each; the
    /// single upsert has none. Then the template refusal and the store's `Upsert`, whose
    /// `RETURNING` rows are what comes back. No event: the object type is empty.
    async fn upsert_values(
        &self,
        method: &'static str,
        fallback_id: &'static str,
        wire_values: &[wire::PropertyValue],
        batch: bool,
    ) -> Result<Result<Vec<wire::PropertyValue>, Option<Interface>>, NotImplemented> {
        let mut values = Vec::with_capacity(wire_values.len());
        let mut seen = HashSet::new();
        for wire_value in wire_values {
            let refuse = |id: &str, detail: &str, field: bool| {
                let params = field.then(|| {
                    HashMap::from([(
                        "FieldID".to_owned(),
                        serde_json::Value::String(wire_value.field_id.clone()),
                    )])
                });
                Ok(Err(self.app_error_as_error(AppError::boxed(
                    method, id, params, detail, 400,
                ))))
            };
            if batch {
                if wire_value.group_id != wire_values[0].group_id {
                    return refuse(
                        "app.property_value.upsert.mixed_groups.app_error",
                        "all values in a batch must belong to the same group",
                        false,
                    );
                }
                if !is_valid_id(&wire_value.field_id) {
                    return refuse(
                        "app.property_value.upsert.invalid_field_id.app_error",
                        "invalid field ID",
                        true,
                    );
                }
                if !seen.insert(wire_value.field_id.as_str()) {
                    return refuse(
                        "app.property_value.upsert.duplicate_field_id.app_error",
                        "duplicate field ID in batch",
                        true,
                    );
                }
            }
            let (value, _) = property_value_from_wire(wire_value)
                .map_err(|why| self.not_implemented(method, why))?;
            values.push(value);
        }
        self.refuse_hooked(method, values.first().map(|v| v.group_id.as_str()))
            .await?;
        let upserted = async {
            self.reject_template_values(&values).await?;
            Ok::<_, PropertyServiceError>(
                self.app
                    .store()
                    .property()
                    .upsert_values_raw(values)
                    .await?,
            )
        }
        .await;
        Ok(match upserted {
            Ok(rows) => Ok(rows.into_iter().map(raw_to_wire).collect()),
            Err(err) => Err(self.property_error(err, method, fallback_id)),
        })
    }

    /// Port of `PluginAPI.UpsertPropertyValue` (plugin_api.go:1809) and its `WithOptions` twin.
    pub(super) async fn properties_upsert_value(
        &self,
        value: Option<&wire::PropertyValue>,
    ) -> Result<(Option<Box<wire::PropertyValue>>, Option<Interface>), NotImplemented> {
        const WHERE: &str = "UpsertPropertyValue";
        let Some(wire_value) = value else {
            return Ok((
                None,
                self.app_error_as_error(invalid_input(
                    WHERE,
                    "value",
                    "property value is required",
                )),
            ));
        };
        Ok(
            match self
                .upsert_values(
                    WHERE,
                    "app.property_value.upsert.app_error",
                    std::slice::from_ref(wire_value),
                    false,
                )
                .await?
            {
                Ok(mut rows) => (rows.pop().map(Box::new), None),
                Err(err) => (None, err),
            },
        )
    }

    /// Port of `PluginAPI.UpsertPropertyValues` (plugin_api.go:1901) and its `WithOptions` twin.
    pub(super) async fn properties_upsert_values(
        &self,
        values: &[wire::PropertyValue],
    ) -> Result<(Vec<wire::PropertyValue>, Option<Interface>), NotImplemented> {
        const WHERE: &str = "UpsertPropertyValues";
        if values.is_empty() {
            return Ok((
                Vec::new(),
                self.app_error_as_error(invalid_input(
                    WHERE,
                    "value",
                    "property values are required",
                )),
            ));
        }
        Ok(
            match self
                .upsert_values(
                    WHERE,
                    "app.property_value.upsert_many.app_error",
                    values,
                    true,
                )
                .await?
            {
                Ok(rows) => (rows, None),
                Err(err) => (Vec::new(), err),
            },
        )
    }

    /// Port of `PluginAPI.DeletePropertyValue` (plugin_api.go:1817) and `App.DeletePropertyValue`:
    /// the value read first (a miss is `GetPropertyValue`'s 404), the soft delete, then
    /// `property_values_updated` with the deleted value's four keys and nothing else.
    pub(super) async fn properties_delete_value(
        &self,
        group_id: &str,
        value_id: &str,
    ) -> Result<Option<Interface>, NotImplemented> {
        const WHERE: &str = "DeletePropertyValue";
        let value = match self
            .app
            .store()
            .property()
            .get_value_raw(group_id, value_id)
            .await
        {
            Ok(raw) => raw.value,
            Err(err) => {
                return Ok(self.store_error(
                    err,
                    "GetPropertyValue",
                    "app.property_value.get.app_error",
                ));
            }
        };
        self.refuse_hooked(WHERE, [group_id, value.group_id.as_str()])
            .await?;
        if let Err(err) = self
            .app
            .store()
            .property()
            .delete_value(group_id, value_id)
            .await
        {
            return Ok(self.store_error(err, WHERE, "app.property_value.delete.app_error"));
        }
        let deleted = PropertyValue {
            target_id: value.target_id.clone(),
            target_type: value.target_type.clone(),
            group_id: value.group_id.clone(),
            field_id: value.field_id.clone(),
            value: serde_json::Value::Null,
            ..PropertyValue::default()
        };
        match go_json_marshal(&[deleted]) {
            Ok(json) => {
                self.publish_values_updated(&value.target_type, &value.target_id, json)
                    .await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "Failed to encode deleted property value to JSON")
            }
        }
        Ok(None)
    }

    /// Port of `PluginAPI.DeletePropertyValuesForTarget` (plugin_api.go:1909): the hard delete
    /// (an empty type or id is the store's `ErrInvalidInput`, the 500), then the event with
    /// `values` `[]`.
    pub(super) async fn properties_delete_values_for_target(
        &self,
        group_id: &str,
        target_type: &str,
        target_id: &str,
    ) -> Result<Option<Interface>, NotImplemented> {
        const WHERE: &str = "DeletePropertyValuesForTarget";
        self.refuse_hooked(WHERE, [group_id]).await?;
        if let Err(err) = self
            .app
            .store()
            .property()
            .delete_values_for_target(group_id, target_type, target_id)
            .await
        {
            return Ok(self.store_error(
                err,
                WHERE,
                "app.property_value.delete_for_target.app_error",
            ));
        }
        self.publish_values_updated(target_type, target_id, "[]".to_owned())
            .await;
        Ok(None)
    }

    /// Port of `PluginAPI.DeletePropertyValuesForField` (plugin_api.go:1916): the soft delete,
    /// then an unscoped event with `field_id` and `values` `[]`.
    pub(super) async fn properties_delete_values_for_field(
        &self,
        group_id: &str,
        field_id: &str,
    ) -> Result<Option<Interface>, NotImplemented> {
        const WHERE: &str = "DeletePropertyValuesForField";
        self.refuse_hooked(WHERE, [group_id]).await?;
        if let Err(err) = self
            .app
            .store()
            .property()
            .delete_values_for_field(group_id, field_id)
            .await
        {
            return Ok(self.store_error(
                err,
                WHERE,
                "app.property_value.delete_for_field.app_error",
            ));
        }
        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED,
            "",
            "",
            "",
            None,
            "",
        );
        message.add("field_id", serde_json::Value::String(field_id.to_owned()));
        message.add("values", serde_json::Value::String("[]".to_owned()));
        self.app.publish(message).await;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_raw_keeps_the_bytes_it_does_not_change() {
        // Go's spacing survives: nothing re-marshals an untouched value.
        let (value, raw) = sanitize_raw(br#"{"a":  1}"#).unwrap();
        assert_eq!(raw, br#"{"a":  1}"#);
        assert_eq!(value, serde_json::json!({"a": 1}));
        let (_, raw) = sanitize_raw(br#"[ "x", "y" ]"#).unwrap();
        assert_eq!(raw, br#"[ "x", "y" ]"#);
        let (_, raw) = sanitize_raw(b"null").unwrap();
        assert_eq!(raw, b"null");
    }

    #[test]
    fn sanitize_raw_remarshals_what_it_trims() {
        let (_, raw) = sanitize_raw(br#""  <a>  ""#).unwrap();
        assert_eq!(raw, br#""\u003ca\u003e""#);
        let (_, raw) = sanitize_raw(br#"[" x", "", "y"]"#).unwrap();
        assert_eq!(raw, br#"["x","y"]"#);
    }

    #[test]
    fn sanitize_raw_refuses_what_go_would_fail_to_store() {
        assert!(sanitize_raw(b"").is_err());
        assert!(sanitize_raw(b"{nope").is_err());
    }

    #[test]
    fn the_deprecated_group_text_is_gos() {
        assert_eq!(
            deprecated_group_text(),
            "\"custom_profile_attributes\" is a version 1 PSA group that has been deprecated; use the version 2 PSA group \"access_control\" instead"
        );
    }

    #[test]
    fn an_empty_attrs_map_is_gos_nil() {
        let field = property_field_from_wire(&wire::PropertyField::default());
        assert_eq!(field.attrs, None);
        let wired = property_field_to_wire(&PropertyField {
            permission_field: Some(PermissionLevel("sysadmin".to_owned())),
            ..PropertyField::default()
        });
        assert_eq!(wired.permission_field.as_deref(), Some("sysadmin"));
    }
}
