//! Port of the **read** half of `SqlPropertyGroupStore`, `SqlPropertyFieldStore` and
//! `SqlPropertyValueStore` (channels/store/sqlstore/property_{group,field,value}_store.go).
//!
//! Ported for the seven custom-profile-attribute routes in
//! `api4/custom_profile_attributes.go`; see [`mm_app::App::cpa_list_fields`] for what each read
//! is reached by. Go hangs these off three separate store interfaces, which this crate keeps in
//! one module because they are one table family, read through one group.
//!
//! # Nothing here writes
//!
//! Every CPA *write* — create, patch, delete, upsert — reaches the property service's
//! `LicenseCheckHook` before it reaches the store, and that hook refuses without an Enterprise
//! licence (app/properties/license_check.go:45). On an unlicensed deployment the write path is
//! therefore unreachable, so porting it would produce code no test on this stack can exercise.
//! The reads are ported because they are *not* unreachable: the licence hook's post-get arms
//! short-circuit on an empty result set, so an unlicensed server answers a real 200 whenever the
//! group holds no matching row and a 403 the moment it holds one. Telling those two apart is a
//! database read, and that is what this module is for.
//!
//! # The searches implement a subset, and say so
//!
//! `SearchPropertyFields`/`SearchPropertyValues` build a dozen optional predicates with squirrel.
//! Compile-time-checked SQL cannot be assembled that way, and guessing at the rows a partially
//! implemented predicate would return is exactly the failure this project exists to prevent — so
//! [`SqlPropertyStore::search_fields`] and [`SqlPropertyStore::search_values`] implement the
//! predicates their ported call sites set and **refuse** ([`StoreError::Argument`]) any option
//! that is set but unimplemented. A later route that needs cursors, delta mode or the
//! team/channel hierarchy gets a loud failure to extend, never a quietly truncated page.

use mm_model::property_field::{PermissionLevel, PropertyField, PropertyFieldSearchOpts};
use mm_model::property_group::PropertyGroup;
use mm_model::property_value::{PropertyValue, PropertyValueSearchOpts};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `PropertyGroupStore`, `PropertyFieldStore` and `PropertyValueStore`
/// (store/store.go) that the CPA routes reach.
pub trait PropertyStore {
    /// Port of `SqlPropertyGroupStore.Get` (property_group_store.go:73).
    fn get_group(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<PropertyGroup, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.Get` (property_field_store.go:61).
    fn get_field(
        &self,
        group_id: &str,
        id: &str,
    ) -> impl std::future::Future<Output = Result<PropertyField, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.GetMany` (property_field_store.go:120), **without** its
    /// cardinality check — see the method's own docs.
    fn get_many_fields(
        &self,
        group_id: &str,
        ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<PropertyField>, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.SearchPropertyFields` (property_field_store.go:220).
    fn search_fields(
        &self,
        opts: &PropertyFieldSearchOpts,
    ) -> impl std::future::Future<Output = Result<Vec<PropertyField>, StoreError>> + Send;

    /// Port of `SqlPropertyValueStore.SearchPropertyValues` (property_value_store.go:137).
    fn search_values(
        &self,
        opts: &PropertyValueSearchOpts,
    ) -> impl std::future::Future<Output = Result<Vec<PropertyValue>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlPropertyStore {
    pool: PgPool,
}

impl SqlPropertyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl PropertyStore for SqlPropertyStore {
    /// # A driver failure answers not-found, because Go's does
    ///
    /// `Get` discards the scan error and returns `store.NewErrNotFound("PropertyGroup", name)`
    /// for **every** failure, no-rows and broken-connection alike (property_group_store.go:83).
    /// The app layer then answers 404 rather than 500. Reproduced, because the status is on the
    /// wire; the driver error is logged at `error` rather than dropped, so an outage is still
    /// visible to an operator on this side.
    #[tracing::instrument(skip_all, fields(name = %name, found))]
    async fn get_group(&self, name: &str) -> Result<PropertyGroup, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT id                       AS "id!",
                   name                     AS "name!",
                   version::bigint          AS "version!",
                   schemaversion::bigint    AS "schemaversion!"
              FROM propertygroups
             WHERE name = $1
            "#,
            name
        )
        .fetch_optional(&self.pool)
        .await;

        let row = match row {
            Ok(row) => row,
            Err(source) => {
                tracing::error!(error = %source, "the PropertyGroups lookup failed");
                None
            }
        };

        let row = row.ok_or_else(|| StoreError::NotFound {
            entity: "PropertyGroup",
            criteria: format!("Name={name}"),
        })?;

        tracing::Span::current().record("found", true);
        Ok(PropertyGroup {
            id: row.id,
            name: row.name,
            version: row.version,
            schema_version: row.schemaversion,
        })
    }

    /// # No `DeleteAt` filter
    ///
    /// `Get` matches on id and group and nothing else, so a **soft-deleted field is returned**.
    /// `patchCPAField` and `deleteCPAField` both read through here, which is why a second delete
    /// of the same field does not answer 404.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, field_id = %id, found))]
    async fn get_field(&self, group_id: &str, id: &str) -> Result<PropertyField, StoreError> {
        let row = sqlx::query_as!(
            PropertyFieldRow,
            r#"
            SELECT id                                   AS "id!",
                   groupid                              AS "groupid!",
                   name                                 AS "name!",
                   COALESCE(type::text, '')             AS "type_text!",
                   attrs                                AS "attrs?",
                   COALESCE(targetid, '')               AS "targetid!",
                   COALESCE(targettype, '')             AS "targettype!",
                   objecttype                           AS "objecttype!",
                   protected                            AS "protected!",
                   permissionfield::text                AS "permissionfield?",
                   permissionvalues::text               AS "permissionvalues?",
                   permissionoptions::text              AS "permissionoptions?",
                   linkedfieldid                        AS "linkedfieldid?",
                   createat                             AS "createat!",
                   updateat                             AS "updateat!",
                   deleteat                             AS "deleteat!",
                   COALESCE(createdby, '')              AS "createdby!",
                   COALESCE(updatedby, '')              AS "updatedby!"
              FROM propertyfields
             WHERE id = $1
               AND groupid = $2
            "#,
            id,
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_get_select".to_owned(),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "PropertyField",
            criteria: format!("Id={id}"),
        })?;

        tracing::Span::current().record("found", true);
        row.into_field()
    }

    /// # The cardinality check is *not* here
    ///
    /// Go's `GetMany` compares `len(fields) < len(ids)` and raises `store.ErrResultsMismatch`,
    /// which the property service rewrites to `ErrFieldNotFound` and the app layer to a 404
    /// (`app.property_field.not_found.app_error`). That comparison lives in
    /// [`mm_app::App::cpa_get_fields`] on this side — one layer up, same wire — so the mismatch
    /// does not need a new `StoreError` variant that only one caller could ever read.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, wanted = ids.len(), found))]
    async fn get_many_fields(
        &self,
        group_id: &str,
        ids: &[String],
    ) -> Result<Vec<PropertyField>, StoreError> {
        let rows = sqlx::query_as!(
            PropertyFieldRow,
            r#"
            SELECT id                                   AS "id!",
                   groupid                              AS "groupid!",
                   name                                 AS "name!",
                   COALESCE(type::text, '')             AS "type_text!",
                   attrs                                AS "attrs?",
                   COALESCE(targetid, '')               AS "targetid!",
                   COALESCE(targettype, '')             AS "targettype!",
                   objecttype                           AS "objecttype!",
                   protected                            AS "protected!",
                   permissionfield::text                AS "permissionfield?",
                   permissionvalues::text               AS "permissionvalues?",
                   permissionoptions::text              AS "permissionoptions?",
                   linkedfieldid                        AS "linkedfieldid?",
                   createat                             AS "createat!",
                   updateat                             AS "updateat!",
                   deleteat                             AS "deleteat!",
                   COALESCE(createdby, '')              AS "createdby!",
                   COALESCE(updatedby, '')              AS "updatedby!"
              FROM propertyfields
             WHERE id = ANY($1)
               AND groupid = $2
            "#,
            ids,
            group_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_get_many_query".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(PropertyFieldRow::into_field).collect()
    }

    /// # The predicates this implements
    ///
    /// `GroupID`, `ObjectType`, `PerPage` and the implicit `DeleteAt = 0`, ordered
    /// `CreateAt ASC, Id ASC` — which is the whole of what `listCPAFields`
    /// (custom_profile_attributes.go:43) sets. Every other option is refused rather than ignored;
    /// see the module docs.
    #[tracing::instrument(skip_all, fields(group_id = %opts.group_id, found))]
    async fn search_fields(
        &self,
        opts: &PropertyFieldSearchOpts,
    ) -> Result<Vec<PropertyField>, StoreError> {
        // `SearchPropertyFields` runs `opts.IsValid()` and the per-page floor before it builds a
        // query (property_field_store.go:221-227); both are 500s at the app layer.
        opts.is_valid().map_err(|_| StoreError::Argument {
            entity: "PropertyField",
            detail: "opts is invalid",
        })?;
        if opts.per_page < 1 {
            return Err(StoreError::Argument {
                entity: "PropertyField",
                detail: "per page must be positive integer greater than zero",
            });
        }
        if !opts.object_types.is_empty()
            || !opts.target_type.is_empty()
            || !opts.target_ids.is_empty()
            || !opts.channel_id.is_empty()
            || !opts.team_id.is_empty()
            || !opts.linked_field_id.is_empty()
            || opts.since_update_at > 0
            || opts.include_deleted
            || !opts.cursor.is_empty()
        {
            return Err(StoreError::Argument {
                entity: "PropertyField",
                detail: "this search option is not ported; see property_store.rs",
            });
        }

        let rows = sqlx::query_as!(
            PropertyFieldRow,
            r#"
            SELECT id                                   AS "id!",
                   groupid                              AS "groupid!",
                   name                                 AS "name!",
                   COALESCE(type::text, '')             AS "type_text!",
                   attrs                                AS "attrs?",
                   COALESCE(targetid, '')               AS "targetid!",
                   COALESCE(targettype, '')             AS "targettype!",
                   objecttype                           AS "objecttype!",
                   protected                            AS "protected!",
                   permissionfield::text                AS "permissionfield?",
                   permissionvalues::text               AS "permissionvalues?",
                   permissionoptions::text              AS "permissionoptions?",
                   linkedfieldid                        AS "linkedfieldid?",
                   createat                             AS "createat!",
                   updateat                             AS "updateat!",
                   deleteat                             AS "deleteat!",
                   COALESCE(createdby, '')              AS "createdby!",
                   COALESCE(updatedby, '')              AS "updatedby!"
              FROM propertyfields
             WHERE deleteat = 0
               AND ($1 = '' OR groupid = $1)
               AND ($2 = '' OR objecttype = $2)
             ORDER BY createat ASC, id ASC
             LIMIT $3
            "#,
            opts.group_id,
            opts.object_type,
            opts.per_page,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_search_query".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(PropertyFieldRow::into_field).collect()
    }

    /// # The predicates this implements
    ///
    /// `GroupID`, `TargetType`, `TargetIDs`, `PerPage` and the implicit `DeleteAt = 0`, ordered
    /// `CreateAt ASC, Id ASC` — the whole of what `listCPAValues`
    /// (custom_profile_attributes.go:390) sets. Every other option is refused; see the module
    /// docs.
    #[tracing::instrument(skip_all, fields(group_id = %opts.group_id, found))]
    async fn search_values(
        &self,
        opts: &PropertyValueSearchOpts,
    ) -> Result<Vec<PropertyValue>, StoreError> {
        opts.is_valid().map_err(|_| StoreError::Argument {
            entity: "PropertyValue",
            detail: "opts is invalid",
        })?;
        if opts.per_page < 1 {
            return Err(StoreError::Argument {
                entity: "PropertyValue",
                detail: "per page must be positive integer greater than zero",
            });
        }
        if !opts.field_id.is_empty()
            || opts.since_update_at > 0
            || opts.include_deleted
            || !opts.cursor.is_empty()
            || opts.value.is_some()
        {
            return Err(StoreError::Argument {
                entity: "PropertyValue",
                detail: "this search option is not ported; see property_store.rs",
            });
        }

        // Go applies the `TargetID IN (…)` predicate only when the list is non-empty; an empty
        // list is "no filter", not "match nothing".
        let target_ids = &opts.target_ids;
        let rows = sqlx::query!(
            r#"
            SELECT id                                   AS "id!",
                   targetid                             AS "targetid!",
                   targettype                           AS "targettype!",
                   groupid                              AS "groupid!",
                   fieldid                              AS "fieldid!",
                   value                                AS "value!",
                   createat                             AS "createat!",
                   updateat                             AS "updateat!",
                   deleteat                             AS "deleteat!",
                   COALESCE(createdby, '')              AS "createdby!",
                   COALESCE(updatedby, '')              AS "updatedby!"
              FROM propertyvalues
             WHERE deleteat = 0
               AND ($1 = '' OR groupid = $1)
               AND ($2 = '' OR targettype = $2)
               AND (cardinality($3::text[]) = 0 OR targetid = ANY($3))
             ORDER BY createat ASC, id ASC
             LIMIT $4
            "#,
            opts.group_id,
            opts.target_type,
            target_ids,
            opts.per_page,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_value_search_query".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| PropertyValue {
                id: row.id,
                target_id: row.targetid,
                target_type: row.targettype,
                group_id: row.groupid,
                field_id: row.fieldid,
                value: row.value,
                create_at: row.createat,
                update_at: row.updateat,
                delete_at: row.deleteat,
                created_by: row.createdby,
                updated_by: row.updatedby,
            })
            .collect())
    }
}

/// One row of `SqlPropertyFieldStore.tableSelectQuery` (property_field_store.go:28).
///
/// `Type`, `PermissionField`, `PermissionValues` and `PermissionOptions` are Postgres **enum**
/// columns (`property_field_type`, `permission_level`), which is why each is read through
/// `::text`: sqlx has no mapping for an enum it was not told about, and the model side is a
/// newtype over `String` on both.
struct PropertyFieldRow {
    id: String,
    groupid: String,
    name: String,
    type_text: String,
    attrs: Option<serde_json::Value>,
    targetid: String,
    targettype: String,
    objecttype: String,
    protected: bool,
    permissionfield: Option<String>,
    permissionvalues: Option<String>,
    permissionoptions: Option<String>,
    linkedfieldid: Option<String>,
    createat: i64,
    updateat: i64,
    deleteat: i64,
    createdby: String,
    updatedby: String,
}

impl PropertyFieldRow {
    fn into_field(self) -> Result<PropertyField, StoreError> {
        // `Attrs` is `StringInterface`, so a `jsonb` holding anything but an object is a scan
        // error in Go too. A SQL `NULL` is the nil map, which is `"attrs": null` on the wire and
        // not `{}` — see `mm_model::property_field::PropertyField::attrs`.
        let attrs = match self.attrs {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Object(map)) => Some(map),
            Some(_) => {
                return Err(StoreError::Decode {
                    entity: "PropertyField",
                    column: "Attrs",
                    source: serde::de::Error::custom("Attrs is not a JSON object"),
                });
            }
        };

        Ok(PropertyField {
            id: self.id,
            group_id: self.groupid,
            name: self.name,
            type_: self.type_text.as_str().into(),
            attrs,
            target_id: self.targetid,
            target_type: self.targettype,
            object_type: self.objecttype,
            protected: self.protected,
            permission_field: self.permissionfield.map(PermissionLevel),
            permission_values: self.permissionvalues.map(PermissionLevel),
            permission_options: self.permissionoptions.map(PermissionLevel),
            linked_field_id: self.linkedfieldid,
            create_at: self.createat,
            update_at: self.updateat,
            delete_at: self.deleteat,
            created_by: self.createdby,
            updated_by: self.updatedby,
        })
    }
}
