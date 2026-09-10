//! Port of `SqlJobStore` (channels/store/sqlstore/job_store.go) — the read half.
//!
//! Ported for the three job reads: `getJobs` (`GET /api/v4/jobs`), `getJob`
//! (`/api/v4/jobs/{job_id}`) and `getJobsByType` (`/api/v4/jobs/type/{job_type}`). The write half
//! (`Save`, `UpdateStatus`, `Cleanup`, …) belongs to the job *runner*, which this server does not
//! host — the Go server beside it still schedules and executes every job.
//!
//! # The empty result is `null` from one query and `[]` from another
//!
//! This is the single most important thing in the file and it is invisible in the SQL. Go's four
//! list methods differ only in how they initialise the destination slice:
//!
//! | Go method | initialiser | empty page marshals as |
//! |---|---|---|
//! | `GetAllByTypesPage` | `var jobs []*model.Job` | `null` |
//! | `GetAllByTypePage` | `statuses := []*model.Job{}` | `[]` |
//! | `GetAllByTypesAndStatusesPage` | `jobs := []*model.Job{}` | `[]` |
//! | `GetByTypeAndData` | `var jobs []*model.Job` | `null` |
//!
//! Measured against the running server, not inferred: `GET /api/v4/jobs?job_type=data_retention`
//! answers the four bytes `null` while `GET /api/v4/jobs/type/data_retention` answers `[]`, on the
//! same database with the same zero rows. A `Vec` cannot carry that distinction, so it is carried
//! at the API edge instead — see `mm_api::jobs::encode_jobs` — and each method here documents
//! which side of the table it is on.

use mm_model::job::Job;
use mm_model::utils::StringMap;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.JobStore` (store/store.go) the three read routes need.
pub trait JobStore {
    /// Port of `SqlJobStore.Get` (job_store.go:301). `ErrNotFound` on a miss.
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<Job, StoreError>> + Send;

    /// Port of `SqlJobStore.GetAllByTypesPage` (job_store.go:319).
    ///
    /// **Go's nil-slice method** — an empty page is `null` on the wire. See the module note.
    fn get_all_by_types_page(
        &self,
        job_types: &[String],
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Job>, StoreError>> + Send;

    /// Port of `SqlJobStore.GetAllByTypePage` (job_store.go:370).
    ///
    /// **Go's empty-slice method** — an empty page is `[]`. See the module note.
    fn get_all_by_type_page(
        &self,
        job_type: &str,
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Job>, StoreError>> + Send;

    /// Port of `SqlJobStore.GetAllByTypesAndStatusesPage` (job_store.go:405).
    ///
    /// Note the signature: Go takes an **offset**, not a page, and `App.GetJobsByTypesAndStatuses`
    /// does the `page * perPage` multiplication itself. Reproduced rather than normalised, so the
    /// two call paths keep multiplying in the same place Go does.
    fn get_all_by_types_and_statuses_page(
        &self,
        job_types: &[String],
        statuses: &[String],
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Job>, StoreError>> + Send;

    /// Port of `SqlJobStore.GetByTypeAndData` (job_store.go:478), narrowed to **one** data pair.
    ///
    /// Go loops over a `map[string]string` and appends a `Data->? = ?` predicate per key, so the
    /// statement's shape depends on the map's size. Both api4 callers pass exactly one pair —
    /// `{"team_id": …}` and `{"policy_id": …}` (api4/job.go:385, 338) — and a compile-time-checked
    /// query cannot have a variable number of predicates, so the single pair is the signature. A
    /// caller needing two would be adding a route, and would add the method it needs then.
    ///
    /// **`Data->$2 = $3` compares JSONB to JSONB**, which is why Go wraps the value in quotes
    /// (`fmt.Sprintf("\"%s\"", value)`): the right-hand side is a JSON *string literal*, not the
    /// bare text. Comparing against the unquoted value matches nothing, silently.
    ///
    /// **Go's nil-slice method** — but every caller re-slices the result into a fresh `[]`, so
    /// this one never reaches the wire as `null`. See the module note.
    fn get_by_type_and_data(
        &self,
        job_type: &str,
        data_key: &str,
        data_value: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Job>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlJobStore {
    pool: PgPool,
}

impl SqlJobStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `jobQuery` (job_store.go:47-51) — Go's nine columns, in Go's order.
///
/// Only `Id` is `NOT NULL` in the schema while Go scans `Type`, `Status` into plain `string` and
/// the four timestamps into `int64`, so a NULL is a scan error on Go's side. `COALESCE` gives the
/// zero value Go's struct would have held, which is agreement on the answer rather than on the
/// failure.
struct JobRow {
    id: String,
    job_type: String,
    priority: i64,
    createat: i64,
    startat: i64,
    lastactivityat: i64,
    status: String,
    progress: i64,
    data: Option<serde_json::Value>,
}

impl JobRow {
    /// `Data` is `jsonb` and nullable, and **a JSON `null` in the column is not the same document
    /// as a SQL `NULL`** — Go renders the first as `null` and the second as `{}`, on both the list
    /// and the single-job route.
    ///
    /// Measured 2026-09-10 against the pinned server, after an earlier version of this comment
    /// claimed both became Go's nil map. They do not: `StringMap.Scan` returns early on a nil
    /// value and leaves the map as the destination struct had it, which marshals as `{}`, while
    /// `json.Unmarshal("null", &m)` sets it nil, which marshals as `null`.
    ///
    /// Which shape a row has depends on who wrote it, and **no Mattermost worker writes a SQL
    /// NULL** — every null-ish `Jobs` row on a months-old database holds a literal JSON `null`
    /// from the product-notices worker. That is why this divergence survived until a *freshly
    /// created* stack was seeded with the other shape by hand. It is reproduced rather than left,
    /// because reproducing it costs one line and the next reader has no way to know it was
    /// unreachable.
    fn into_job(self) -> Result<Job, StoreError> {
        let data =
            match self.data {
                None => Some(StringMap::new()),
                Some(serde_json::Value::Null) => None,
                Some(value) => Some(serde_json::from_value::<StringMap>(value).map_err(
                    |source| StoreError::Decode {
                        entity: "Job",
                        column: "data",
                        source,
                    },
                )?),
            };

        Ok(Job {
            id: self.id,
            job_type: self.job_type,
            priority: self.priority,
            create_at: self.createat,
            start_at: self.startat,
            last_activity_at: self.lastactivityat,
            status: self.status,
            progress: self.progress,
            data,
        })
    }
}

// The nine-column projection of `jobQuery` (job_store.go:47-51) is repeated in full in every
// query below rather than shared through a constant. `sqlx::query_as!` checks a *string literal*
// and cannot see through a macro that builds one, so the repetition is what buys compile-time
// verification of every column against the live schema.

/// Map a page of rows, failing the whole page if any one `data` column will not decode.
fn rows_into_jobs(rows: Vec<JobRow>) -> Result<Vec<Job>, StoreError> {
    rows.into_iter().map(JobRow::into_job).collect()
}

impl JobStore for SqlJobStore {
    #[tracing::instrument(skip_all, fields(job_id = %id))]
    async fn get(&self, id: &str) -> Result<Job, StoreError> {
        let row = sqlx::query_as!(
            JobRow,
            r#"
            SELECT                    id                          AS "id!",
                   COALESCE(type, '')          AS "job_type!",
                   COALESCE(priority, 0)       AS "priority!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(startat, 0)        AS "startat!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(progress, 0)       AS "progress!",
                   data                        AS "data?"
              FROM jobs
             WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Job with id={id}"),
            source,
        })?;

        row.ok_or_else(|| StoreError::NotFound {
            entity: "Job",
            criteria: format!("id={id}"),
        })?
        .into_job()
    }

    /// `ORDER BY CreateAt DESC` with **no tiebreak**, exactly as Go writes it. Two jobs created in
    /// the same millisecond have no defined order on either server; a stabilising `, id` here
    /// would make our order more defined than Go's, which is a divergence dressed as a fix.
    #[tracing::instrument(skip_all, fields(types = job_types.len(), page, per_page, found))]
    async fn get_all_by_types_page(
        &self,
        job_types: &[String],
        page: i64,
        per_page: i64,
    ) -> Result<Vec<Job>, StoreError> {
        let offset = page * per_page;
        let rows = sqlx::query_as!(
            JobRow,
            r#"
            SELECT                    id                          AS "id!",
                   COALESCE(type, '')          AS "job_type!",
                   COALESCE(priority, 0)       AS "priority!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(startat, 0)        AS "startat!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(progress, 0)       AS "progress!",
                   data                        AS "data?"
              FROM jobs
             WHERE type = ANY($1)
             ORDER BY createat DESC
             LIMIT $2 OFFSET $3
            "#,
            job_types,
            per_page,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Jobs with types".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows_into_jobs(rows)
    }

    #[tracing::instrument(skip_all, fields(job_type = %job_type, page, per_page, found))]
    async fn get_all_by_type_page(
        &self,
        job_type: &str,
        page: i64,
        per_page: i64,
    ) -> Result<Vec<Job>, StoreError> {
        let offset = page * per_page;
        let rows = sqlx::query_as!(
            JobRow,
            r#"
            SELECT                    id                          AS "id!",
                   COALESCE(type, '')          AS "job_type!",
                   COALESCE(priority, 0)       AS "priority!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(startat, 0)        AS "startat!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(progress, 0)       AS "progress!",
                   data                        AS "data?"
              FROM jobs
             WHERE type = $1
             ORDER BY createat DESC
             LIMIT $2 OFFSET $3
            "#,
            job_type,
            per_page,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Jobs with type={job_type}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows_into_jobs(rows)
    }

    #[tracing::instrument(skip_all, fields(types = job_types.len(), offset, limit, found))]
    async fn get_all_by_types_and_statuses_page(
        &self,
        job_types: &[String],
        statuses: &[String],
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Job>, StoreError> {
        let rows = sqlx::query_as!(
            JobRow,
            r#"
            SELECT                    id                          AS "id!",
                   COALESCE(type, '')          AS "job_type!",
                   COALESCE(priority, 0)       AS "priority!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(startat, 0)        AS "startat!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(progress, 0)       AS "progress!",
                   data                        AS "data?"
              FROM jobs
             WHERE type = ANY($1) AND status = ANY($2)
             ORDER BY createat DESC
             LIMIT $3 OFFSET $4
            "#,
            job_types,
            statuses,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Jobs with types and statuses".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows_into_jobs(rows)
    }

    /// **No `ORDER BY`** — Go has none either, and its two callers sort the result in Go before
    /// paginating it. Adding one here would hide that the sort is the handler's job.
    #[tracing::instrument(skip_all, fields(job_type = %job_type, data_key = %data_key, found))]
    async fn get_by_type_and_data(
        &self,
        job_type: &str,
        data_key: &str,
        data_value: &str,
    ) -> Result<Vec<Job>, StoreError> {
        // The JSON string literal Go builds with `fmt.Sprintf("\"%s\"", value)`. Built with
        // `serde_json` rather than by concatenating quotes, so a value containing a quote or a
        // backslash escapes the way Postgres will parse it.
        let json_value = serde_json::Value::String(data_value.to_owned());

        let rows = sqlx::query_as!(
            JobRow,
            r#"
            SELECT                    id                          AS "id!",
                   COALESCE(type, '')          AS "job_type!",
                   COALESCE(priority, 0)       AS "priority!",
                   COALESCE(createat, 0)       AS "createat!",
                   COALESCE(startat, 0)        AS "startat!",
                   COALESCE(lastactivityat, 0) AS "lastactivityat!",
                   COALESCE(status, '')        AS "status!",
                   COALESCE(progress, 0)       AS "progress!",
                   data                        AS "data?"
              FROM jobs
             WHERE type = $1 AND data->$2 = $3
            "#,
            job_type,
            data_key,
            json_value
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get Jobs by type and data".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows_into_jobs(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NULL column and a JSON `null` are the same answer, and it is `None` — which serialises
    /// back as `"data":null`, the value the running server returns for every product-notices job.
    #[test]
    fn null_data_in_either_form_is_none() {
        let row = |data| JobRow {
            id: "fmuj6jho4jd65j54dpg83m7eyh".to_owned(),
            job_type: "product_notices".to_owned(),
            priority: 0,
            createat: 1788869528340,
            startat: 1788869537753,
            lastactivityat: 1788869537934,
            status: "success".to_owned(),
            progress: 100,
            data,
        };

        assert_eq!(row(None).into_job().expect("maps").data, None);
        assert_eq!(
            row(Some(serde_json::Value::Null))
                .into_job()
                .expect("maps")
                .data,
            None
        );
    }

    /// An object maps to the `StringMap`, and an empty object stays `Some({})` — distinct from
    /// `None` on the wire (`{}` versus `null`).
    #[test]
    fn an_object_maps_to_a_string_map_and_empty_is_not_null() {
        let row = |data| JobRow {
            id: "fmuj6jho4jd65j54dpg83m7eyh".to_owned(),
            job_type: "extract_content".to_owned(),
            priority: 0,
            createat: 1,
            startat: 2,
            lastactivityat: 3,
            status: "success".to_owned(),
            progress: 100,
            data: Some(data),
        };

        let populated = row(serde_json::json!({"catchup": "true", "errors": "0"}))
            .into_job()
            .expect("maps");
        let data = populated.data.expect("present");
        assert_eq!(data.get("catchup").map(String::as_str), Some("true"));
        assert_eq!(data.get("errors").map(String::as_str), Some("0"));

        let empty = row(serde_json::json!({})).into_job().expect("maps");
        assert_eq!(empty.data, Some(StringMap::new()));
    }

    /// `Data` is `map[string]string` in Go, so a non-string value is a decode failure rather than
    /// a silently stringified one. Nothing the Go server writes produces this; the test pins that
    /// we fail rather than invent a value if something else does.
    #[test]
    fn a_non_string_data_value_is_a_decode_error() {
        let row = JobRow {
            id: "fmuj6jho4jd65j54dpg83m7eyh".to_owned(),
            job_type: "migrations".to_owned(),
            priority: 0,
            createat: 1,
            startat: 2,
            lastactivityat: 3,
            status: "success".to_owned(),
            progress: 0,
            data: Some(serde_json::json!({"processed": 12})),
        };

        let err = row.into_job().expect_err("a number is not a string");
        assert!(matches!(
            err,
            StoreError::Decode {
                entity: "Job",
                column: "data",
                ..
            }
        ));
    }
}
