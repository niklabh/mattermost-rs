//! Port of the **read** half of `SqlPropertyGroupStore`, `SqlPropertyFieldStore` and
//! `SqlPropertyValueStore` (channels/store/sqlstore/property_{group,field,value}_store.go).
//!
//! Ported for the seven custom-profile-attribute routes in
//! `api4/custom_profile_attributes.go`, then extended to the whole predicate set for the four
//! read routes of `api4/properties.go`; see [`mm_app::App::search_property_fields`] for what each
//! read is reached by. Go hangs these off three separate store interfaces, which this crate keeps
//! in one module because they are one table family, read through one group.
//!
//! # Nothing here writes
//!
//! Every write these tables take — create, patch, delete, upsert — runs the property service's
//! hook chain first, and none of those hooks exist on this side. For the `access_control` group
//! the first of them is a `LicenseCheckHook` that refuses outright without an Enterprise licence
//! (app/properties/license_check.go:45), so that group's write path is unreachable here at all.
//! The reads are ported because they are *not* unreachable: the licence hook's post-get arms
//! short-circuit on an empty result set, so an unlicensed server answers a real 200 whenever the
//! group holds no matching row and a 403 the moment it holds one. Telling those two apart is a
//! database read, and that is what this module is for.
//!
//! **The licence hook is scoped to one group, not to the tables.** It is constructed with
//! `cpaGroup.ID` (app/server.go:325), so the `boards` and `post_attributes` groups — both
//! registered unconditionally at startup and both PSAv2 — carry no hook at all and read straight
//! through on any edition. That is what makes the generic `api4/properties.go` reads worth
//! porting rather than forwarding: on those two groups there is nothing to refuse.
//!
//! # The searches used to implement a subset; they no longer do
//!
//! `SearchPropertyFields`/`SearchPropertyValues` build a dozen optional predicates with squirrel,
//! and compile-time-checked SQL cannot be assembled that way. The first port implemented only the
//! predicates its CPA call sites set and refused the rest with [`StoreError::Argument`], so that
//! a later route would get a loud failure to extend rather than a quietly truncated page. The
//! properties routes were that route: both searches now carry the **whole** predicate set —
//! cursors, delta mode, the four scopes, `IncludeDeleted` and the value filter — as one statement
//! whose branches are chosen by bound parameters. Nothing is refused any more; each method's own
//! docs say which Go branch each `CASE` arm is.

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

    /// # Every predicate `SearchPropertyFields` builds, in one statement
    ///
    /// Go assembles a dozen optional `WHERE` clauses with squirrel and picks the ordering, the
    /// tombstone rule and the cursor key from one derived flag, `deltaMode := SinceUpdateAt > 0`
    /// (property_field_store.go:229). Compile-time-checked SQL cannot be assembled clause by
    /// clause, so the whole decision tree is a single statement whose branches are taken by bound
    /// parameters. Read it against the Go comment block above `SearchPropertyFields`; the two
    /// modes and the four scopes are in the same order here.
    ///
    /// Three of those branches are the ones a reader gets wrong:
    ///
    /// * **Delta mode auto-includes tombstones.** `DeleteAt = 0` is applied only when
    ///   `!deltaMode && !IncludeDeleted`, so a `since` query returns soft-deleted rows without
    ///   asking — that is how a client learns a field was deleted.
    /// * **The cursor key follows the mode, not the caller.** Delta mode compares `UpdateAt`,
    ///   directory mode `CreateAt`, and [`PropertyFieldSearchOpts::is_valid`] rejects the
    ///   mismatch rather than letting it compare against a column of zeroes and skip every row.
    /// * **An empty cursor is no clause at all**, which is not the same as a clause that matches
    ///   everything: `id > ''` would still drop nothing, but the paired `UpdateAt > 0` would drop
    ///   every row whose timestamp Go never sets.
    ///
    /// The `since` boundary is `>=`, not `>`: a row updated at exactly `since` is on the first
    /// page, and the cursor is what disambiguates the same millisecond across pages.
    #[tracing::instrument(skip_all, fields(group_id = %opts.group_id, delta, found))]
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

        let delta = opts.since_update_at > 0;
        let has_cursor = !opts.cursor.is_empty();
        tracing::Span::current().record("delta", delta);

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
             WHERE ($1::bool OR $2::bool OR deleteat = 0)
               AND ($3::text = '' OR groupid = $3)
               AND (CASE
                      WHEN cardinality($4::text[]) > 0 THEN objecttype = ANY($4)
                      WHEN $5::text <> ''              THEN objecttype = $5
                      ELSE TRUE
                    END)
               AND (CASE
                      WHEN $6::text <> '' AND $7::text <> ''
                        THEN targettype = 'system'
                          OR (targettype = 'team'    AND targetid = $7)
                          OR (targettype = 'channel' AND targetid = $6)
                      WHEN $6 <> ''
                        THEN targettype = 'system'
                          OR (targettype = 'channel' AND targetid = $6)
                      WHEN $7 <> ''
                        THEN targettype = 'system'
                          OR (targettype = 'team'    AND targetid = $7)
                      ELSE ($8::text = '' OR targettype = $8)
                       AND (cardinality($9::text[]) = 0 OR targetid = ANY($9))
                    END)
               AND ($10::text = '' OR linkedfieldid = $10)
               AND (NOT $11::bool
                    OR CASE WHEN $1 THEN updateat > $12::bigint
                                      OR (updateat = $12 AND id > $13::text)
                            ELSE      createat > $14::bigint
                                      OR (createat = $14 AND id > $13)
                       END)
               AND (NOT $1 OR updateat >= $15::bigint)
             ORDER BY (CASE WHEN $1 THEN updateat ELSE createat END) ASC, id ASC
             LIMIT $16
            "#,
            delta,
            opts.include_deleted,
            opts.group_id,
            &opts.object_types,
            opts.object_type,
            opts.channel_id,
            opts.team_id,
            opts.target_type,
            &opts.target_ids,
            opts.linked_field_id,
            has_cursor,
            opts.cursor.update_at,
            opts.cursor.property_field_id,
            opts.cursor.create_at,
            opts.since_update_at,
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

    /// # Every predicate `SearchPropertyValues` builds, in one statement
    ///
    /// The same two modes and the same cursor rule as [`SqlPropertyStore::search_fields`], minus
    /// the channel/team hierarchy — values have no scope switch, only `TargetType`, `TargetIDs`
    /// and `FieldID`, each applied independently (property_value_store.go:137).
    ///
    /// `Value` is compared as **jsonb**, which is how Go's `sq.Eq{"Value": string(opts.Value)}`
    /// lands too: the column is `jsonb`, so the text on the wire is cast before the comparison
    /// and the match is semantic — key order and insignificant whitespace do not matter.
    #[tracing::instrument(skip_all, fields(group_id = %opts.group_id, delta, found))]
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

        let delta = opts.since_update_at > 0;
        let has_cursor = !opts.cursor.is_empty();
        tracing::Span::current().record("delta", delta);

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
             WHERE ($1::bool OR $2::bool OR deleteat = 0)
               AND ($3::text = '' OR groupid = $3)
               AND ($4::text = '' OR targettype = $4)
               AND (cardinality($5::text[]) = 0 OR targetid = ANY($5))
               AND ($6::text = '' OR fieldid = $6)
               AND (NOT $7::bool
                    OR CASE WHEN $1 THEN updateat > $8::bigint
                                      OR (updateat = $8 AND id > $9::text)
                            ELSE      createat > $10::bigint
                                      OR (createat = $10 AND id > $9)
                       END)
               AND (NOT $1 OR updateat >= $11::bigint)
               AND ($12::jsonb IS NULL OR value = $12)
             ORDER BY (CASE WHEN $1 THEN updateat ELSE createat END) ASC, id ASC
             LIMIT $13
            "#,
            delta,
            opts.include_deleted,
            opts.group_id,
            opts.target_type,
            target_ids,
            opts.field_id,
            has_cursor,
            opts.cursor.update_at,
            opts.cursor.property_value_id,
            opts.cursor.create_at,
            opts.since_update_at,
            opts.value.as_ref(),
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
        // # A SQL `NULL` is `{}` and a jsonb `null` is `null`, and it is that way round
        //
        // `Attrs` is `StringInterface`, a Go **map**, and sqlx's `reflectx.FieldByIndexes`
        // allocates a nil map before it scans into it. So a SQL `NULL`, whose
        // `StringInterface.Scan` returns early without touching the destination (utils.go:186),
        // leaves that freshly allocated *empty* map behind and marshals as `{}`. A jsonb `null`
        // does reach `json.Unmarshal`, which sets a map destination to the zero value — nil — and
        // marshals as `null`.
        //
        // Both measured against the Go server, and the port had them the other way round until
        // the `boards` group gave the CPA reads a row to return: an empty result set cannot tell
        // the two apart, so a whole family shipped on the inverted rule. Anything else in a
        // `jsonb` column is a scan error in Go too.
        let attrs = match self.attrs {
            None => Some(serde_json::Map::new()),
            Some(serde_json::Value::Null) => None,
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
