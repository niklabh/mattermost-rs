//! Port of the app layer behind `api4/custom_profile_attributes.go` — `App.GetPropertyGroup`
//! (app/property_group.go:25), `App.SearchPropertyFields` (app/property_field.go:192),
//! `App.GetPropertyField` (:137), `App.GetPropertyFields` (:149) and
//! `App.SearchPropertyValues` (app/property_value.go:101), as the seven CPA routes reach them.
//!
//! # Every function here is the **unlicensed** path, and that is the whole design
//!
//! Go registers a `LicenseCheckHook` over the `access_control` property group at startup
//! (app/server.go:322) and it is the *first* hook, so it runs before access control and attribute
//! validation on every field and value operation in that group. Without an Enterprise licence it
//! returns `ErrLicenseRequired`, which `mapPropertyServiceError` turns into **403
//! `app.property.license_error`** (app/property_errors.go:44).
//!
//! What makes the routes worth porting rather than forwarding is that the hook does **not** fire
//! uniformly:
//!
//! | hook arm | when it refuses |
//! |---|---|
//! | `PreCreatePropertyField` | always — a create is a 403 before it touches the table |
//! | `PostGetPropertyField` | only once the row has been **found**; a miss is a 404 first |
//! | `PostGetPropertyFields` | `len(fields) == 0` returns **nil**, so an empty page is a 200 |
//! | `PostGetPropertyValues` | same empty short-circuit |
//!
//! So an unlicensed server answers `[]` to `listCPAFields` on a group with no user fields, `{}`
//! to `listCPAValues` for a user with no values, `404` to a patch of a field that does not exist,
//! and `403` the moment any of those reads finds a row. Telling those apart needs the database,
//! which is why these are real reads and not a constant refusal. All four were verified against
//! the Go server on this stack, with and without a seeded `access_control` user field.
//!
//! The licensed half is **not** ported: it reaches the write hooks, the access-control hook and
//! the attribute-validation hook, none of which exist on this side. The API layer forwards to Go
//! whenever anything says this installation is licensed, which is why each function below is
//! named `…_unlicensed` — the precondition is in the name because nothing in the type system can
//! hold it.
//!
//! # Two handlers order the group read and the target check differently
//!
//! `listCPAValues` runs `hasTargetAccess` **before** `GetPropertyGroup`
//! (custom_profile_attributes.go:378); `cpaPatchValues`, which both PATCH routes share, runs the
//! group read **first** (:311). Nothing here can enforce that — the target check is
//! session-bound and lives in the API layer — but the asymmetry is real and observable when the
//! group is missing, so it is written down where a reader porting the next property route will
//! see it.

use std::collections::BTreeMap;

use mm_model::custom_profile_attributes::{CPAField, cpa_fields_from_property_fields};
use mm_model::property_field::{
    PROPERTY_FIELD_OBJECT_TYPE_USER, PropertyField, PropertyFieldSearchOpts,
};
use mm_model::property_group::{
    ACCESS_CONTROL_GROUP_FIELD_LIMIT, ACCESS_CONTROL_PROPERTY_GROUP_NAME, PropertyGroup,
};
use mm_model::property_value::{PROPERTY_VALUE_TARGET_TYPE_USER, PropertyValueSearchOpts};
use mm_model::utils::{AppError, AppResult};
use mm_store::{PropertyStore, StoreError};

use crate::App;

/// Port of the page size every CPA read asks for (`AccessControlGroupFieldLimit + 5`).
///
/// The `+ 5` is Go's, and the comment on [`ACCESS_CONTROL_GROUP_FIELD_LIMIT`] says why the whole
/// result set is read in one page rather than paginated: the limit is assumed to bound it.
const CPA_READ_PER_PAGE: i64 = ACCESS_CONTROL_GROUP_FIELD_LIMIT + 5;

impl App {
    /// Port of `App.GetPropertyGroup` (app/property_group.go:25) for the CPA group.
    ///
    /// One error id across both arms — `app.property_group.get.app_error` — and only the status
    /// differs: **404** for a missing group, 500 for anything else. The store collapses a driver
    /// failure into not-found (see [`mm_store::SqlPropertyStore::get_group`]), so the 500 arm is
    /// currently unreachable; it is written out because the id is shared and a reader would
    /// otherwise assume the 404 is the only answer.
    ///
    /// Go reads this through `PropertyService.GetPropertyGroup`, which goes **straight to the
    /// store** — it is `Group()`, used at startup, that consults the in-memory cache. So this is
    /// a query on every request on both sides, not a cache read.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn cpa_property_group(&self) -> AppResult<PropertyGroup> {
        let group = self
            .store()
            .property()
            .get_group(ACCESS_CONTROL_PROPERTY_GROUP_NAME)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                if !not_found {
                    tracing::error!(error = ?err, "the CPA property group lookup failed");
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

    /// The unlicensed half of `listCPAFields` (custom_profile_attributes.go:34).
    ///
    /// `SearchPropertyFields` with `ObjectType: user` and no other filter, then
    /// `PostGetPropertyFields` — which returns `nil` for an empty slice and `ErrLicenseRequired`
    /// otherwise. So this is `[]` or a 403, and never a field.
    ///
    /// The conversion through [`cpa_fields_from_property_fields`] is kept even though it can only
    /// ever be handed an empty slice here: it is what fixes the response *type*, and dropping it
    /// would leave the 200 arm writing a `Vec<PropertyField>`, whose `attrs` is the untyped blob
    /// rather than `CPAAttrs`. Nothing observable today, wrong the moment a licence appears.
    #[tracing::instrument(skip_all, fields(fields))]
    pub async fn cpa_list_fields_unlicensed(&self, group_id: &str) -> AppResult<Vec<CPAField>> {
        let fields = self.cpa_search_user_fields(group_id).await?;

        tracing::Span::current().record("fields", fields.len());
        if !fields.is_empty() {
            return Err(property_licence_refusal("SearchPropertyFields"));
        }

        cpa_fields_from_property_fields(&fields).map_err(|err| {
            tracing::error!(error = %err, "a CPA field would not convert");
            AppError::boxed(
                "listCPAFields",
                "app.custom_profile_attributes.property_field_conversion.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// The unlicensed half of `patchCPAField` (:130) and `deleteCPAField` (:236), which share
    /// every step from the group read to the licence refusal.
    ///
    /// Returns an error unconditionally, and **which** error is the point: `GetPropertyField`
    /// reads the row before the post-get hook runs, so a field that is not in this group is a
    /// **404 `app.property.not_found.app_error`** and a field that is is a **403**. A port that
    /// refused everything with the licence error would answer 403 to a patch of a field id that
    /// does not exist, which is both wrong and a disclosure.
    ///
    /// # The `ObjectType != user` check is deliberately absent
    ///
    /// Both handlers compare `existingField.ObjectType` against `user` and answer 404
    /// `api.property_field.object_type_mismatch.app_error` on a mismatch — but that comparison is
    /// *after* `GetPropertyField`, whose hook has already refused with 403 on this deployment. It
    /// is unreachable while unlicensed, so it is not written; see the module docs on why this
    /// file stops where the licence does.
    #[tracing::instrument(skip_all, fields(field_id = %field_id))]
    pub async fn cpa_field_write_unlicensed(
        &self,
        group_id: &str,
        field_id: &str,
    ) -> Box<AppError> {
        match self.store().property().get_field(group_id, field_id).await {
            Ok(_) => property_licence_refusal("GetPropertyField"),
            Err(err) => property_read_error("GetPropertyField", err),
        }
    }

    /// The unlicensed half of `cpaPatchValues` (:302) from the group read onwards, shared by
    /// `patchCPAValues` and `patchCPAValuesForUser`.
    ///
    /// Returns an error unconditionally, and again which one is the point. `GetPropertyFields`
    /// loads every id in the batch: if the store returns **fewer rows than ids** the service
    /// rewrites the mismatch to `ErrFieldNotFound` and the app layer to **404
    /// `app.property_field.not_found.app_error`** — note the id differs from the single-field
    /// read's `app.property.not_found.app_error` by one word. Only once every id resolves does
    /// `PostGetPropertyFields` see a non-empty slice and refuse with 403.
    ///
    /// The empty-batch, batch-cap and id-validity checks are the handler's and run ahead of this,
    /// so `field_ids` here is non-empty and every entry is a valid id.
    #[tracing::instrument(skip_all, fields(wanted = field_ids.len(), found))]
    pub async fn cpa_patch_values_unlicensed(
        &self,
        group_id: &str,
        field_ids: &[String],
    ) -> Box<AppError> {
        let fields = match self
            .store()
            .property()
            .get_many_fields(group_id, field_ids)
            .await
        {
            Ok(fields) => fields,
            Err(err) => return property_read_error("GetPropertyFields", err),
        };

        tracing::Span::current().record("found", fields.len());
        // Go's cardinality check, moved up one layer from the store — see
        // [`mm_store::SqlPropertyStore::get_many_fields`]. `<`, not `!=`: a duplicate id in the
        // request would make the row count *lower* than the id count, never higher.
        if fields.len() < field_ids.len() {
            return AppError::boxed(
                "GetPropertyFields",
                "app.property_field.not_found.app_error",
                None,
                String::new(),
                404,
            );
        }

        property_licence_refusal("GetPropertyFields")
    }

    /// The unlicensed half of `listCPAValues` (:371), from the group read onwards.
    ///
    /// `SearchPropertyValues` for one target, then `PostGetPropertyValues` — the same empty
    /// short-circuit as the field list. So this is `{}` or a 403.
    ///
    /// The response is keyed by `field_id`, and a `BTreeMap` is right rather than merely
    /// convenient: Go builds a `map[string]json.RawMessage` and `encoding/json` **sorts map keys**
    /// when it marshals one, so the wire order is the sorted order on both sides.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, values))]
    pub async fn cpa_list_values_unlicensed(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> AppResult<BTreeMap<String, serde_json::Value>> {
        let opts = PropertyValueSearchOpts {
            group_id: group_id.to_owned(),
            target_type: PROPERTY_VALUE_TARGET_TYPE_USER.to_owned(),
            target_ids: vec![user_id.to_owned()],
            per_page: CPA_READ_PER_PAGE,
            ..PropertyValueSearchOpts::default()
        };
        let values = self
            .store()
            .property()
            .search_values(&opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the CPA value search failed");
                AppError::boxed(
                    "SearchPropertyValues",
                    "app.property_value.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("values", values.len());
        if !values.is_empty() {
            return Err(property_licence_refusal("SearchPropertyValues"));
        }

        Ok(values
            .into_iter()
            .map(|value| (value.field_id, value.value))
            .collect())
    }

    /// `SearchPropertyFields(group, {ObjectType: user, PerPage: limit+5})`, shared by the field
    /// list and nothing else yet.
    async fn cpa_search_user_fields(&self, group_id: &str) -> AppResult<Vec<PropertyField>> {
        let opts = PropertyFieldSearchOpts {
            group_id: group_id.to_owned(),
            object_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned(),
            per_page: CPA_READ_PER_PAGE,
            ..PropertyFieldSearchOpts::default()
        };

        self.store()
            .property()
            .search_fields(&opts)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the CPA field search failed");
                AppError::boxed(
                    "SearchPropertyFields",
                    "app.property_field.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

/// Port of `properties.ErrLicenseRequired` as `mapPropertyServiceError` renders it
/// (app/property_errors.go:44).
///
/// **403, not 501**, and the `detailed_error` is empty: Go's `NewAppError(..., nil, "", 403)`
/// carries the sentinel only through `Wrap`, which never reaches the wire. The same id and status
/// as `getCPAGroup`'s inline check in [`crate::App`]'s API layer, which is the point — Go's own
/// comment says that route exists to reproduce this hook's contract by hand.
pub fn property_licence_refusal(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.property.license_error",
        None,
        String::new(),
        403,
    )
}

/// `mapPropertyServiceError`'s `*store.ErrNotFound` arm (app/property_errors.go:62) and the 500
/// fallback the callers wrap around it.
///
/// The id here is `app.property.not_found.app_error` — the **generic** one, because the sentinel
/// `ErrFieldNotFound` is raised only by the multi-id read. A single-field miss goes through the
/// store's plain not-found and lands on this id.
fn property_read_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    let not_found = err.is_not_found();
    if !not_found {
        tracing::error!(error = ?err, "a CPA property read failed");
    }
    AppError::boxed(
        where_,
        if not_found {
            "app.property.not_found.app_error"
        } else {
            "app.property_field.get.app_error"
        },
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two not-found ids are **one word apart and mean different things**: a single-field read
    /// that misses is `app.property.not_found.app_error`, a batch read that is short a row is
    /// `app.property_field.not_found.app_error`. Both are 404, so a port that reused one would
    /// pass every status assertion and answer the wrong id to a client that branches on it.
    #[test]
    fn the_single_and_batch_misses_carry_different_ids() {
        let single = property_read_error(
            "GetPropertyField",
            StoreError::NotFound {
                entity: "PropertyField",
                criteria: "Id=x".to_owned(),
            },
        );
        assert_eq!(single.id, "app.property.not_found.app_error");
        assert_eq!(single.status_code, 404);
        assert_ne!(single.id, "app.property_field.not_found.app_error");
    }

    /// A driver failure on a single-field read is a 500 with its own id, not the 404.
    #[test]
    fn a_broken_field_read_is_a_five_hundred() {
        let broken = property_read_error(
            "GetPropertyField",
            StoreError::Db {
                context: "boom".to_owned(),
                source: sqlx::Error::RowNotFound,
            },
        );
        assert_eq!(broken.id, "app.property_field.get.app_error");
        assert_eq!(broken.status_code, 500);
    }

    /// **403, and an empty `detailed_error`.** The status is the one thing a licence refusal in
    /// this tree does not agree on — the neighbouring gated reads answer 501 — and Go's own
    /// `getCPAGroup` comment exists because this contract is easy to get wrong.
    #[test]
    fn the_licence_refusal_is_a_forbidden() {
        let refusal = property_licence_refusal("SearchPropertyFields");
        assert_eq!(refusal.id, "app.property.license_error");
        assert_eq!(refusal.status_code, 403);
        assert_eq!(refusal.detailed_error, "");
    }

    /// The page size is `AccessControlGroupFieldLimit + 5`, and the `+ 5` is load-bearing: it is
    /// how Go tells "a full page" from "the group is at its cap", so a port that asked for
    /// exactly the limit would silently drop the field that proves the cap was hit.
    #[test]
    fn the_read_page_is_the_group_limit_plus_five() {
        assert_eq!(CPA_READ_PER_PAGE, 205);
        assert_eq!(CPA_READ_PER_PAGE, ACCESS_CONTROL_GROUP_FIELD_LIMIT + 5);
    }
}
