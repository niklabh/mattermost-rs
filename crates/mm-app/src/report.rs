//! Port of `App.GetUsersForReporting` and `App.GetUserCountForReport` (channels/app/report.go:176
//! and :194) — the two reads behind the System Console's *User Management → Users* report.
//!
//! # Only one of the two validates its options
//!
//! `GetUsersForReporting` calls `filter.IsValid()` first, so a bad sort column or guest filter is
//! a **400** with a `model.user_report_options.is_valid.*` id. `GetUserCountForReport` does not
//! call it at all — and its handler never fills `ReportingBaseOptions` either, so the count route
//! has no sort column to reject. The asymmetry is Go's; both halves are reproduced.

use mm_model::job::Job;
use mm_model::report::{UserReport, UserReportOptions};
use mm_model::utils::{AppError, AppResult};
use mm_store::{FileInfoStore, StoreError, UserStore};

use crate::App;

impl App {
    /// Port of `App.GetUsersForReporting` (report.go:176).
    ///
    /// The rows come back as `UserReportQuery` and are converted one at a time by
    /// `ToReport()`, which **sanitises the embedded user** with `ClearNonProfileFields(true)`
    /// before it copies it. That is the only sanitisation on this path: the handler does not call
    /// `SanitizeProfile`, so what the store read is what `ClearNonProfileFields` leaves.
    ///
    /// An empty result is `make([]*model.UserReport, 0)` — a non-nil empty slice, so the route
    /// answers `[]` and not `null`.
    #[tracing::instrument(skip_all, fields(page_size = options.base.page_size, found))]
    pub async fn get_users_for_reporting(
        &self,
        options: &UserReportOptions,
    ) -> AppResult<Vec<UserReport>> {
        options.is_valid()?;

        let rows = self
            .store()
            .user()
            .get_user_report(options)
            .await
            .map_err(|err| report_error("GetUsersForReporting", "get_user_report", err))?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(|mut row| row.to_report()).collect())
    }

    /// Port of `App.GetUserCountForReport` (report.go:194).
    ///
    /// Go returns `*int64` and the handler encodes the pointer, so the body is a bare JSON
    /// number — never an object and never `null`, since the pointer is only nil on the error
    /// path the handler has already returned from.
    #[tracing::instrument(skip_all, fields(count))]
    pub async fn get_user_count_for_report(&self, options: &UserReportOptions) -> AppResult<i64> {
        let count = self
            .store()
            .user()
            .get_user_count_for_report(options)
            .await
            .map_err(|err| {
                report_error("GetUserCountForReport", "get_user_count_for_report", err)
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }
}

// ---------------------------------------------------------------------------------------------
// The batch report's four steps (app/report.go:21-174), for `export_users_to_csv`
// ---------------------------------------------------------------------------------------------

/// `makeFilePath` (report.go:163): chunk `count` of the report `prefix` (the job id).
fn report_chunk_path(prefix: &str, count: i64, extension: &str) -> String {
    format!("admin_reports/batch_report_{prefix}__{count}.{extension}")
}

/// `makeCompiledFilename` (report.go:171).
fn report_compiled_filename(prefix: &str, extension: &str) -> String {
    format!("batch_report_{prefix}.{extension}")
}

/// `makeCompiledFilePath` (report.go:167).
fn report_compiled_path(prefix: &str, extension: &str) -> String {
    format!(
        "admin_reports/{}",
        report_compiled_filename(prefix, extension)
    )
}

/// The file-backend error as the `AppError` Go's `WriteFile`/`ReadFile`/`RemoveFile` return. A
/// backend this server does not implement (S3) has no Go answer to borrow; it is a 500 naming
/// the call, since a job cannot be handed to Go.
fn file_error(where_: &'static str, err: crate::post::PrepareError) -> Box<AppError> {
    match err {
        crate::post::PrepareError::App(err) => err,
        crate::post::PrepareError::Unreproducible(why) => AppError::boxed(
            where_,
            "api.file.unsupported_driver",
            None,
            why.to_owned(),
            500,
        ),
    }
}

impl App {
    /// Port of `App.SaveReportChunk` (report.go:21): `csv` is the only format; anything else is
    /// the 400 `app.save_report_chunk.unsupported_format`.
    ///
    /// The chunk is the rows alone, no header, each written by Go's `encoding/csv` rules
    /// ([`mm_model::go_csv`]), at `admin_reports/batch_report_<job id>__<count>.csv`.
    pub async fn save_report_chunk(
        &self,
        format: &str,
        prefix: &str,
        count: i64,
        rows: &[Vec<String>],
    ) -> AppResult<()> {
        if format != "csv" {
            return Err(AppError::boxed(
                "SaveReportChunk",
                "app.save_report_chunk.unsupported_format",
                None,
                "unsupported report format".to_owned(),
                400,
            ));
        }
        let mut buf = String::new();
        for row in rows {
            mm_model::go_csv::write_record(&mut buf, row);
        }
        self.write_file(buf.as_bytes(), &report_chunk_path(prefix, count, "csv"))
            .await
            .map_err(|err| file_error("saveCSVChunk", err))?;
        Ok(())
    }

    /// Port of `App.CompileReportChunks` (report.go:48): the header row, then each chunk's bytes
    /// in order, written to `admin_reports/batch_report_<job id>.csv`. A chunk that cannot be read
    /// is `ReadFile`'s error, returned as it is.
    pub async fn compile_report_chunks(
        &self,
        format: &str,
        prefix: &str,
        number_of_chunks: i64,
        headers: &[&str],
    ) -> AppResult<()> {
        if format != "csv" {
            return Err(AppError::boxed(
                "CompileReportChunks",
                "app.compile_report_chunks.unsupported_format",
                None,
                String::new(),
                400,
            ));
        }
        let mut compiled = String::new();
        let headers: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
        mm_model::go_csv::write_record(&mut compiled, &headers);
        let mut compiled = compiled.into_bytes();
        for i in 0..number_of_chunks {
            let chunk = self
                .read_file(&report_chunk_path(prefix, i, "csv"))
                .await
                .map_err(|err| file_error("compileCSVChunks", err))?;
            compiled.extend_from_slice(&chunk);
        }
        self.write_file(&compiled, &report_compiled_path(prefix, "csv"))
            .await
            .map_err(|err| file_error("compileCSVChunks", err))?;
        Ok(())
    }

    /// Port of `App.CleanupReportChunks` (report.go:145): remove each chunk, stopping at the first
    /// failure. The compiled file stays; the post that delivers it points at it.
    pub async fn cleanup_report_chunks(
        &self,
        format: &str,
        prefix: &str,
        number_of_chunks: i64,
    ) -> AppResult<()> {
        if format != "csv" {
            // Go's copy-paste: the cleanup's refusal is `CompileReportChunks`' id and `Where`.
            return Err(AppError::boxed(
                "CompileReportChunks",
                "app.compile_report_chunks.unsupported_format",
                None,
                String::new(),
                400,
            ));
        }
        for i in 0..number_of_chunks {
            self.remove_file(&report_chunk_path(prefix, i, "csv"))
                .await
                .map_err(|err| file_error("cleanupCSVChunks", err))?;
        }
        Ok(())
    }

    /// Port of `App.SendReportToUser` (report.go:90): a `FileInfo` for the compiled report, owned
    /// by the system bot, and a post from the bot in its DM with the requester carrying it.
    ///
    /// In Go's order: `requesting_user_id` and `date_range` must be in the job's data (each its
    /// own 500), the system bot, `FileSize` of the compiled file, the `FileInfo` save
    /// (`Name` `batch_report_<id>.csv`, `Extension` the format, no `MimeType`, no post yet), the
    /// DM, the requester, and `CreatePost` — which attaches the file.
    ///
    /// The message is `app.report.send_report_to_user.export_finished` in the **requester's**
    /// locale with the range in the **server's** (`getTranslatedDateRange` uses `i18n.T`); with
    /// no bundle loaded, `en.json`'s sentences.
    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    pub async fn send_report_to_user(&self, job: &Job, format: &str) -> AppResult<()> {
        let data = job.data.clone().unwrap_or_default();
        let requesting_user_id = data.get("requesting_user_id").cloned().unwrap_or_default();
        if requesting_user_id.is_empty() {
            return Err(AppError::boxed(
                "SendReportToUser",
                "app.report.send_report_to_user.missing_user_id",
                None,
                String::new(),
                500,
            ));
        }
        let date_range = data.get("date_range").cloned().unwrap_or_default();
        if date_range.is_empty() {
            return Err(AppError::boxed(
                "SendReportToUser",
                "app.report.send_report_to_user.missing_date_range",
                None,
                String::new(),
                500,
            ));
        }

        let bot = self.get_system_bot().await?;
        let path = report_compiled_path(&job.id, format);
        let size = self.file_backend().file_size(&path).await.map_err(|err| {
            tracing::error!(error = %err, path, "the compiled report could not be sized");
            AppError::boxed(
                "FileSize",
                "api.file.file_size.app_error",
                None,
                String::new(),
                500,
            )
        })?;
        let info = self
            .store()
            .file_info()
            .save(mm_model::file_info::FileInfo {
                name: report_compiled_filename(&job.id, format),
                extension: format.to_owned(),
                size,
                path,
                creator_id: bot.user_id.clone(),
                ..Default::default()
            })
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the report's file info could not be saved");
                AppError::boxed(
                    "SendReportToUser",
                    "app.report.send_report_to_user.failed_to_save",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let hook_ctx = crate::plugin_hooks::HookContext::default();
        let channel = match self
            .get_or_create_direct_channel(&hook_ctx, None, &requesting_user_id, &bot.user_id)
            .await?
        {
            crate::channel_create::ChannelCreate::Created(channel) => channel,
            crate::channel_create::ChannelCreate::Forward(why) => {
                return Err(AppError::boxed(
                    "SendReportToUser",
                    "app.report.send_report_to_user.unreproducible",
                    None,
                    why.to_owned(),
                    500,
                ));
            }
        };
        let user = self.get_user(&requesting_user_id).await?;

        let post = mm_model::post::Post {
            channel_id: channel.id.clone(),
            message: export_finished_message(
                &user.locale,
                &date_range,
                &self.config().default_server_locale,
            )
            .await,
            user_id: bot.user_id.clone(),
            file_ids: Some(vec![info.id.clone()]),
            ..Default::default()
        };
        self.create_post(
            post,
            &channel,
            &mm_model::session::Session::default(),
            crate::post_create::CreatePostFlags {
                set_online: true,
                ..Default::default()
            },
            &hook_ctx,
        )
        .await
        .map_err(|err| file_error("SendReportToUser", err))?;
        Ok(())
    }
}

/// `getTranslatedDateRange` (report.go:304) in the server locale, inside
/// `app.report.send_report_to_user.export_finished` in the user's.
async fn export_finished_message(
    user_locale: &str,
    date_range: &str,
    server_locale: &str,
) -> String {
    let range_id = match date_range {
        mm_model::report::REPORT_DURATION_LAST_30_DAYS => "app.report.date_range.last_30_days",
        mm_model::report::REPORT_DURATION_PREVIOUS_MONTH => "app.report.date_range.previous_month",
        mm_model::report::REPORT_DURATION_LAST_6_MONTHS => "app.report.date_range.last_6_months",
        _ => "app.report.date_range.all_time",
    };
    match crate::i18n::translations().await {
        Some(bundle) => {
            let range = bundle.translate(bundle.server_locale(server_locale), range_id);
            let params = crate::i18n::Params::from([(
                "DateRange".to_owned(),
                serde_json::Value::String(range),
            )]);
            bundle.translate_with(
                bundle.user_locale(user_locale),
                "app.report.send_report_to_user.export_finished",
                Some(&params),
            )
        }
        None => {
            let range = match range_id {
                "app.report.date_range.last_30_days" => "the last 30 days",
                "app.report.date_range.previous_month" => "the previous month",
                "app.report.date_range.last_6_months" => "the last 6 months",
                _ => "all time",
            };
            format!(
                "Your export is ready. The CSV file contains user data for {range}. Click on the link below to download the report."
            )
        }
    }
}

/// Both app functions collapse **every** store failure into one 500 — there is no `ErrNotFound`
/// branch and no out-of-bounds branch, so a malformed cursor and a dead database are the same
/// response. The `where` clause is the only thing that differs between them.
fn report_error(caller: &'static str, where_: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "user report query failed");
    AppError::boxed(
        caller,
        format!("app.report.{where_}.store_error"),
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `makeFilePath`, `makeCompiledFilePath`, `makeCompiledFilename` (report.go:163-173).
    #[test]
    fn the_report_paths_are_gos() {
        assert_eq!(
            report_chunk_path("job1", 2, "csv"),
            "admin_reports/batch_report_job1__2.csv"
        );
        assert_eq!(
            report_compiled_filename("job1", "csv"),
            "batch_report_job1.csv"
        );
        assert_eq!(
            report_compiled_path("job1", "csv"),
            "admin_reports/batch_report_job1.csv"
        );
    }
}
