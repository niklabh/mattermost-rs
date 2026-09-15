//! Port of the **read** half of `SqlPropertyGroupStore`, `SqlPropertyFieldStore` and
//! `SqlPropertyValueStore` (channels/store/sqlstore/property_{group,field,value}_store.go).
//!
//! Ported for the seven custom-profile-attribute routes in
//! `api4/custom_profile_attributes.go`, then extended to the whole predicate set for the four
//! read routes of `api4/properties.go`; see [`mm_app::App::search_property_fields`] for what each
//! read is reached by. Go hangs these off three separate store interfaces, which this crate keeps
//! in one module because they are one table family, read through one group.
//!
//! # The writes, and the hook chain that fronts them
//!
//! Every write these tables take — create, patch, delete, upsert — runs the property service's
//! hook chain first, and for the `access_control` group that chain begins with a
//! `LicenseCheckHook` that refuses outright without an Enterprise licence
//! (app/properties/license_check.go:45). The chain is [`mm_app::property_hooks`]; the writes are
//! here: [`SqlPropertyStore::create_field`], [`SqlPropertyStore::update_field`] (with the
//! optimistic-concurrency check and the linked-dependent propagation), the two counts the field
//! limit reads, the system-level [`SqlPropertyStore::check_property_name_conflict`],
//! [`SqlPropertyStore::upsert_values`], and the three delete statements —
//! [`SqlPropertyStore::count_linked_fields`], [`SqlPropertyStore::delete_values_for_field`] and
//! [`SqlPropertyStore::delete_field`], whose two deletes disagree about whether touching zero
//! rows is an error. The reads are the licence hook's post-get arms: they short-circuit on an
//! empty result set, so an unlicensed server answers a real 200 whenever the group holds no
//! matching row and a 403 the moment it holds one.
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
    /// Port of `SqlPropertyFieldStore.GetFieldByNameForObjectType` (property_field_store.go:95).
    ///
    /// `objectType` is matched **exactly** — the empty string is itself a valid object type, not
    /// a wildcard — and `TargetID` likewise, so the boards group's post-level fields (`TargetID`
    /// empty) are found with an empty target. `DeleteAt = 0` is part of the predicate.
    fn get_field_by_name_for_object_type(
        &self,
        group_id: &str,
        target_id: &str,
        object_type: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<PropertyField, StoreError>> + Send;

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

    /// Port of `SqlPropertyFieldStore.CountLinkedFields` (property_field_store.go:734) — how many
    /// **live** fields name this one as their `LinkedFieldID`.
    fn count_linked_fields(
        &self,
        field_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlPropertyValueStore.DeleteForField` (property_value_store.go:369) — the cascade
    /// a field delete runs before it soft-deletes the field itself.
    fn delete_values_for_field(
        &self,
        group_id: &str,
        field_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.Delete` (property_field_store.go:504).
    fn delete_field(
        &self,
        group_id: &str,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.Get` with an **empty** group id (property_field_store.go:61):
    /// `if groupID != ""` is the only place the group predicate is added, so this is the by-id
    /// read across every group. Reached by the linked-source lookups in `createPropertyField`
    /// (app/properties/property_field.go:142) and `rejectTemplateValues`, which pass `""`.
    fn get_field_in_any_group(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<PropertyField, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.Create` (property_field_store.go:35): `PreSave`,
    /// `EnsureOptionIDs`, `IsValid`, one `INSERT`. Takes the field by value and hands it back
    /// **as written**, because that is what Go returns — the struct it was given, not a re-read.
    fn create_field(
        &self,
        field: PropertyField,
    ) -> impl std::future::Future<Output = Result<PropertyField, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.Update` (property_field_store.go:342) for one field, with
    /// the optimistic-concurrency check `UpdateAt = expected` and the linked-dependent
    /// propagation that follows it in the same transaction.
    ///
    /// Returns the field as written and the dependents the propagation touched.
    fn update_field(
        &self,
        group_id: &str,
        field: PropertyField,
        expected_update_at: i64,
    ) -> impl std::future::Future<Output = Result<(PropertyField, Vec<PropertyField>), StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.CountForGroup` (property_field_store.go:139).
    fn count_fields_for_group(
        &self,
        group_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.CountForGroupObjectType` (property_field_store.go:156).
    fn count_fields_for_group_object_type(
        &self,
        group_id: &str,
        object_type: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlPropertyFieldStore.CheckPropertyNameConflict` (property_field_store.go:579)
    /// for a **system-target** field — the only target the CPA routes produce. Returns the level
    /// of the conflicting field (`system`, `team` or `channel`) or `""`.
    ///
    /// The team- and channel-target arms are not ported: nothing served reaches them, and each
    /// is its own three-way `COALESCE` with a `Channels` join. A field with either target is
    /// refused with [`StoreError::Argument`] rather than answered wrongly.
    fn check_property_name_conflict(
        &self,
        field: &PropertyField,
        exclude_id: &str,
    ) -> impl std::future::Future<Output = Result<String, StoreError>> + Send;

    /// Port of `SqlPropertyValueStore.Upsert` (property_value_store.go:280): one transaction, one
    /// `INSERT … ON CONFLICT (GroupID, TargetID, FieldID) WHERE DeleteAt = 0 DO UPDATE` per value,
    /// each `RETURNING` the row as stored.
    fn upsert_values(
        &self,
        values: Vec<PropertyValue>,
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

    #[tracing::instrument(skip_all, fields(group_id = %group_id, object_type = %object_type, name = %name, found = false))]
    async fn get_field_by_name_for_object_type(
        &self,
        group_id: &str,
        target_id: &str,
        object_type: &str,
        name: &str,
    ) -> Result<PropertyField, StoreError> {
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
             WHERE groupid = $1
               AND targetid = $2
               AND name = $3
               AND deleteat = 0
               AND objecttype = $4
            "#,
            group_id,
            target_id,
            name,
            object_type
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_get_by_name_select".to_owned(),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "PropertyField",
            criteria: name.to_owned(),
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

    /// # `DeleteAt = 0`, not "exists"
    ///
    /// A field whose dependent has itself been soft-deleted no longer blocks the delete, which is
    /// what makes the refusal recoverable: delete the dependent, then the source. The `GetMaster`
    /// in Go is a read-your-writes concern on a replica set this deployment does not have.
    #[tracing::instrument(skip_all, fields(field_id = %field_id, linked))]
    async fn count_linked_fields(&self, field_id: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(id) AS "count!"
              FROM propertyfields
             WHERE linkedfieldid = $1
               AND deleteat = 0
            "#,
            field_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_count_linked_fields".to_owned(),
            source,
        })?;

        tracing::Span::current().record("linked", count);
        Ok(count)
    }

    /// # It is a soft delete, and it does not care how many rows it touched
    ///
    /// Go's `DeleteForField` ignores `RowsAffected` entirely (property_value_store.go:379), so a
    /// field with no values is not an error — unlike the field delete below, whose zero-row case
    /// *is* a not-found. Two statements one line apart in the same service function, with
    /// opposite conventions.
    ///
    /// The `DeleteAt` it stamps is **not** filtered on: a value already soft-deleted is stamped
    /// again with the new timestamp. Reproduced rather than improved, because the column is on
    /// the wire for any delta read that follows.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, field_id = %field_id, cleared))]
    async fn delete_values_for_field(
        &self,
        group_id: &str,
        field_id: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            UPDATE propertyvalues
               SET deleteat = $1
             WHERE fieldid = $2
               AND ($3 = '' OR groupid = $3)
            "#,
            mm_model::utils::get_millis(),
            field_id,
            group_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_value_delete_for_field_exec".to_owned(),
            source,
        })?;

        tracing::Span::current().record("cleared", result.rows_affected());
        Ok(())
    }

    /// # Zero rows affected is a **not-found**, and the group is part of the key
    ///
    /// `RowsAffected() == 0` raises `store.NewErrNotFound("PropertyField", id)`, which the app
    /// layer turns into 404 `app.property.not_found.app_error`. So deleting an id that is real but
    /// lives in another group is a 404 and not a silent success — the `GroupID` predicate is what
    /// makes a group boundary a boundary, and dropping it would let any group delete any field.
    ///
    /// Already-deleted rows still match (there is no `DeleteAt = 0` predicate), so a second
    /// delete of the same field succeeds and re-stamps the timestamp.
    ///
    /// # Both of those guards are unreachable from `api4/properties.go`, and that is fine
    ///
    /// The handler reads the field **with the group** before it deletes
    /// ([`mm_app::App::get_property_field`], whose query is `id = $1 AND groupid = $2`), so by the
    /// time this runs the row is known to exist in the group: neither the `GroupID` predicate nor
    /// the zero-row branch can fire. Go has the identical shape — the service's
    /// `deletePropertyField` opens with `getPropertyField(groupID, id)`.
    ///
    /// Established by mutation, not by reading: both survive the whole parity suite, and the one
    /// that appeared to be caught was a **false catch** from a fixture that wiped itself. They
    /// stay because they are the contract for a caller that skips the read — Go's plugin API is
    /// one — and because the cost of a redundant predicate is nothing.
    ///
    /// The same is true of the `GroupID` predicate on
    /// [`SqlPropertyStore::delete_values_for_field`], for a different reason: `PropertyFields.id`
    /// is the primary key, so a field id belongs to exactly one group and the group adds nothing
    /// once `FieldID` is applied.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, id = %id))]
    async fn delete_field(&self, group_id: &str, id: &str) -> Result<(), StoreError> {
        let result = sqlx::query!(
            r#"
            UPDATE propertyfields
               SET deleteat = $1
             WHERE id = $2
               AND ($3 = '' OR groupid = $3)
            "#,
            mm_model::utils::get_millis(),
            id,
            group_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete property field with id: {id}"),
            source,
        })?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                entity: "PropertyField",
                criteria: format!("id={id}"),
            });
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(field_id = %id, found))]
    async fn get_field_in_any_group(&self, id: &str) -> Result<PropertyField, StoreError> {
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
            "#,
            id
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

    /// # The three model steps run here, in Go's order, and the first refuses a caller-set id
    ///
    /// `field.ID != ""` is `ErrInvalidInput` before `PreSave` — a caller cannot choose an id.
    /// Then `PreSave` mints one and stamps the two timestamps, `EnsureOptionIDs` fills option ids,
    /// and `IsValid` runs on the result. A validation failure is Go's `*AppError` wrapped, which
    /// `mapPropertyServiceError` unwraps and answers as the model's own 400.
    #[tracing::instrument(skip_all, fields(group_id = %field.group_id, field_id))]
    async fn create_field(&self, mut field: PropertyField) -> Result<PropertyField, StoreError> {
        if !field.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "PropertyField",
                field: "id",
                value: field.id,
            });
        }

        field.pre_save();
        field
            .ensure_option_ids()
            .map_err(|_| StoreError::Argument {
                entity: "PropertyField",
                detail: "property_field_create_ensure_option_ids",
            })?;
        field.is_valid().map_err(|app_error| StoreError::Invalid {
            entity: "PropertyField",
            app_error,
        })?;
        tracing::Span::current().record("field_id", &field.id);
        normalize_attrs(&mut field);

        let attrs = field
            .attrs
            .as_ref()
            .map(|a| serde_json::Value::Object(a.clone()))
            .unwrap_or(serde_json::Value::Null);
        sqlx::query!(
            r#"
            INSERT INTO propertyfields
                (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
                 permissionfield, permissionvalues, permissionoptions, linkedfieldid,
                 createat, updateat, deleteat, createdby, updatedby)
            VALUES ($1, $2, $3, $4::text::property_field_type, $5::jsonb, $6, $7, $8, $9,
                    $10::text::permission_level, $11::text::permission_level,
                    $12::text::permission_level, $13, $14, $15, $16, $17, $18)
            "#,
            field.id,
            field.group_id,
            field.name,
            field.type_.as_str(),
            attrs,
            field.target_id,
            field.target_type,
            field.object_type,
            field.protected,
            field.permission_field.as_ref().map(|p| p.as_str()),
            field.permission_values.as_ref().map(|p| p.as_str()),
            field.permission_options.as_ref().map(|p| p.as_str()),
            field.linked_field_id.as_deref(),
            field.create_at,
            field.update_at,
            field.delete_at,
            field.created_by,
            field.updated_by,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_create_insert".to_owned(),
            source,
        })?;

        Ok(field)
    }

    /// # `UpdateAt = expected` is the whole concurrency model
    ///
    /// The service read the row moments ago and hands its `UpdateAt` here; if the row has moved
    /// since, the `UPDATE` matches nothing and the answer is [`StoreError::Stale`] — Go's
    /// `ErrConflict`, a **409** at the app layer. Without the expectation Go reports a bare
    /// "not found" error instead; every caller in this tree passes one, so that arm is not
    /// reproduced.
    ///
    /// # The propagation is a second statement in the same transaction
    ///
    /// A source field's type and options are copied onto every live dependent whose
    /// `LinkedFieldID` names it, but only where they differ, and the dependents touched are read
    /// back by their fresh `UpdateAt` so the caller can broadcast them. A CPA field is never a
    /// link source — `createCPAField` cannot make a template — so on this server's routes the
    /// statement matches nothing; it is here because Go runs it and the table is shared.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, field_id = %field.id, propagated))]
    async fn update_field(
        &self,
        group_id: &str,
        mut field: PropertyField,
        expected_update_at: i64,
    ) -> Result<(PropertyField, Vec<PropertyField>), StoreError> {
        let update_time = mm_model::utils::get_millis();
        field.update_at = update_time;
        field
            .ensure_option_ids()
            .map_err(|_| StoreError::Argument {
                entity: "PropertyField",
                detail: "property_field_update_ensure_option_ids",
            })?;
        field.is_valid().map_err(|app_error| StoreError::Invalid {
            entity: "PropertyField",
            app_error,
        })?;
        normalize_attrs(&mut field);

        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "property_field_update_begin_transaction".to_owned(),
            source,
        })?;

        let attrs = field
            .attrs
            .as_ref()
            .map(|a| serde_json::Value::Object(a.clone()))
            .unwrap_or(serde_json::Value::Null);
        let result = sqlx::query!(
            r#"
            UPDATE propertyfields
               SET name = $1,
                   type = $2::text::property_field_type,
                   attrs = $3::jsonb,
                   targetid = $4,
                   targettype = $5,
                   protected = $6,
                   permissionfield = $7::text::permission_level,
                   permissionvalues = $8::text::permission_level,
                   permissionoptions = $9::text::permission_level,
                   linkedfieldid = $10,
                   updateat = $11,
                   deleteat = $12,
                   updatedby = $13
             WHERE id = $14
               AND ($15::text = '' OR groupid = $15)
               AND updateat = $16
            "#,
            field.name,
            field.type_.as_str(),
            attrs,
            field.target_id,
            field.target_type,
            field.protected,
            field.permission_field.as_ref().map(|p| p.as_str()),
            field.permission_values.as_ref().map(|p| p.as_str()),
            field.permission_options.as_ref().map(|p| p.as_str()),
            field.linked_field_id.as_deref(),
            update_time,
            field.delete_at,
            field.updated_by,
            field.id,
            group_id,
            expected_update_at,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_update_exec".to_owned(),
            source,
        })?;

        if result.rows_affected() != 1 {
            return Err(StoreError::Stale {
                entity: "PropertyField",
                detail: "concurrent modification detected; retry the update",
            });
        }

        sqlx::query!(
            r#"
            UPDATE propertyfields AS linked
               SET type = source.type,
                   attrs = jsonb_set(COALESCE(linked.attrs, '{}'::jsonb), '{options}',
                                     COALESCE(source.attrs->'options', '[]'::jsonb)),
                   updateat = $1
              FROM propertyfields AS source
             WHERE source.id = $2
               AND linked.linkedfieldid = source.id
               AND linked.deleteat = 0
               AND (linked.type != source.type
                    OR linked.attrs->'options' IS DISTINCT FROM source.attrs->'options')
            "#,
            update_time,
            field.id,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_update_propagate".to_owned(),
            source,
        })?;

        let propagated = sqlx::query_as!(
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
             WHERE linkedfieldid = $1
               AND deleteat = 0
               AND updateat = $2
            "#,
            field.id,
            update_time,
        )
        .fetch_all(&mut *transaction)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_update_select_propagated".to_owned(),
            source,
        })?;

        transaction
            .commit()
            .await
            .map_err(|source| StoreError::Db {
                context: "property_field_update_commit_transaction".to_owned(),
                source,
            })?;

        tracing::Span::current().record("propagated", propagated.len());
        let propagated = propagated
            .into_iter()
            .map(PropertyFieldRow::into_field)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((field, propagated))
    }

    #[tracing::instrument(skip_all, fields(group_id = %group_id, count))]
    async fn count_fields_for_group(
        &self,
        group_id: &str,
        include_deleted: bool,
    ) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(id) AS "count!"
              FROM propertyfields
             WHERE groupid = $1
               AND ($2::bool OR deleteat = 0)
            "#,
            group_id,
            include_deleted,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            // Go's own message — it was copied from the sessions store and never changed.
            context: "failed to count Sessions".to_owned(),
            source,
        })?;
        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip_all, fields(group_id = %group_id, object_type = %object_type, count))]
    async fn count_fields_for_group_object_type(
        &self,
        group_id: &str,
        object_type: &str,
        include_deleted: bool,
    ) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(id) AS "count!"
              FROM propertyfields
             WHERE groupid = $1
               AND objecttype = $2
               AND ($3::bool OR deleteat = 0)
            "#,
            group_id,
            object_type,
            include_deleted,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count property fields for group and object type".to_owned(),
            source,
        })?;
        tracing::Span::current().record("count", count);
        Ok(count)
    }

    /// # A system-level name conflicts at every level, and the same level is checked first
    ///
    /// `checkSystemLevelConflict` is `COALESCE` over three `LIMIT 1` subqueries — system, then
    /// team, then channel — each on the same object type, group, name and `DeleteAt = 0`, each
    /// excluding `exclude_id` when one is given. The first non-null wins, so a name held at both
    /// team and channel level reports `team`. Legacy (PSAv1) fields skip the check entirely.
    #[tracing::instrument(skip_all, fields(name = %field.name, target_type = %field.target_type, level))]
    async fn check_property_name_conflict(
        &self,
        field: &PropertyField,
        exclude_id: &str,
    ) -> Result<String, StoreError> {
        if field.is_psav1() {
            return Ok(String::new());
        }
        match field.target_type.as_str() {
            mm_model::property_field::PROPERTY_FIELD_TARGET_LEVEL_SYSTEM => {}
            mm_model::property_field::PROPERTY_FIELD_TARGET_LEVEL_TEAM
            | mm_model::property_field::PROPERTY_FIELD_TARGET_LEVEL_CHANNEL => {
                return Err(StoreError::Argument {
                    entity: "PropertyField",
                    detail: "team- and channel-level name conflict checks are not ported",
                });
            }
            // "Unknown target type - let DB constraint handle"
            _ => return Ok(String::new()),
        }

        let level = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(
                     (SELECT 'system' FROM propertyfields
                       WHERE objecttype = $1 AND groupid = $2 AND targettype = 'system'
                         AND name = $3 AND deleteat = 0 AND ($4::text = '' OR id <> $4)
                       LIMIT 1),
                     (SELECT 'team' FROM propertyfields
                       WHERE objecttype = $1 AND groupid = $2 AND targettype = 'team'
                         AND name = $3 AND deleteat = 0 AND ($4::text = '' OR id <> $4)
                       LIMIT 1),
                     (SELECT 'channel' FROM propertyfields
                       WHERE objecttype = $1 AND groupid = $2 AND targettype = 'channel'
                         AND name = $3 AND deleteat = 0 AND ($4::text = '' OR id <> $4)
                       LIMIT 1),
                     '') AS "level!"
            "#,
            field.object_type,
            field.group_id,
            field.name,
            exclude_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "property_field_check_conflict_system".to_owned(),
            source,
        })?;
        tracing::Span::current().record("level", &level);
        Ok(level)
    }

    /// # `CreateAt` is pinned to the batch's one timestamp before `PreSave` runs
    ///
    /// Go sets `CreateAt = updateTime` when it is zero *and then* calls `PreSave`, so a fresh
    /// value has `CreateAt == UpdateAt` and every value in the batch shares one clock reading.
    /// `PreSave` still mints the id. On conflict the existing row keeps its id and `CreateAt`;
    /// `Value`, `UpdateAt`, `UpdatedBy` are replaced and `DeleteAt` reset to 0 — a soft-deleted
    /// row cannot conflict (the index is partial on `DeleteAt = 0`), so that reset is for a row
    /// that was live already and is a no-op.
    #[tracing::instrument(skip_all, fields(values = values.len()))]
    async fn upsert_values(
        &self,
        values: Vec<PropertyValue>,
    ) -> Result<Vec<PropertyValue>, StoreError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let mut transaction = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "property_value_upsert_begin_transaction".to_owned(),
            source,
        })?;

        let update_time = mm_model::utils::get_millis();
        let mut upserted = Vec::with_capacity(values.len());
        for mut value in values {
            if value.create_at == 0 {
                value.create_at = update_time;
            }
            value.pre_save();
            value.update_at = update_time;
            value.is_valid().map_err(|app_error| StoreError::Invalid {
                entity: "PropertyValue",
                app_error,
            })?;

            let row = sqlx::query!(
                r#"
                INSERT INTO propertyvalues
                    (id, targetid, targettype, groupid, fieldid, value,
                     createat, updateat, deleteat, createdby, updatedby)
                VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8, $9, $10, $11)
                ON CONFLICT (groupid, targetid, fieldid) WHERE deleteat = 0
                DO UPDATE SET value = $6::jsonb, updateat = $8, deleteat = 0, updatedby = $11
                RETURNING id                      AS "id!",
                          targetid                AS "targetid!",
                          targettype              AS "targettype!",
                          groupid                 AS "groupid!",
                          fieldid                 AS "fieldid!",
                          value                   AS "value!",
                          createat                AS "createat!",
                          updateat                AS "updateat!",
                          deleteat                AS "deleteat!",
                          COALESCE(createdby, '') AS "createdby!",
                          COALESCE(updatedby, '') AS "updatedby!"
                "#,
                value.id,
                value.target_id,
                value.target_type,
                value.group_id,
                value.field_id,
                value.value,
                value.create_at,
                value.update_at,
                value.delete_at,
                value.created_by,
                value.updated_by,
            )
            .fetch_one(&mut *transaction)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to upsert property value with id: {}", value.id),
                source,
            })?;

            upserted.push(PropertyValue {
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
            });
        }

        transaction
            .commit()
            .await
            .map_err(|source| StoreError::Db {
                context: "property_value_upsert_commit".to_owned(),
                source,
            })?;
        Ok(upserted)
    }
}

/// What Go's `StringInterface.Value()` writes: the attrs map marshalled by `encoding/json`, in
/// which an integral `float64` is `3`, not `3.0`. Applied before every write so the stored
/// document is the one Go would have stored, and the field handed back — the in-memory struct,
/// as Go hands it back — prints the same way.
fn normalize_attrs(field: &mut PropertyField) {
    if let Some(attrs) = field.attrs.as_mut() {
        attrs
            .values_mut()
            .for_each(mm_model::utils::go_normalize_json_numbers);
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

        // Go decodes `Attrs` into `map[string]any`, so every number is a `float64` and prints
        // without a fraction when integral. See `go_normalize_json_numbers`.
        let attrs = attrs.map(|mut map| {
            map.values_mut()
                .for_each(mm_model::utils::go_normalize_json_numbers);
            map
        });

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
