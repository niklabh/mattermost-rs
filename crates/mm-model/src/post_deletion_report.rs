//! Port of `model/post_deletion_report.go` — the Markdown report posted after a data-spillage
//! deletion.
//!
//! # Everything here is user-visible Markdown
//!
//! The output is posted into a channel, so **the exact spacing, the `&nbsp;|&nbsp;` separators,
//! the heading levels and the emoji are the wire format**. A "tidier" table or a different
//! separator changes what reviewers see. Every literal is reproduced byte for byte.
//!
//! # i18n is a function parameter, not a global
//!
//! Go threads an `i18n.TranslateFunc` through every method. That type belongs to the server, not
//! to this crate, so it is represented here as [`TranslateFn`] — a closure taking the message id
//! and optional interpolation params. `translate_detail` reproduces Go's three-way call:
//! no params, params, or **params containing `Count`**, which Go passes positionally first
//! because `go-i18n` selects a plural form from it.

use crate::utils::StringInterface;

/// The translate function Go passes as `T`. The `params` argument carries the interpolation map;
/// a `Count` key inside it selects a plural form.
pub type TranslateFn<'a> = &'a dyn Fn(&str, Option<&StringInterface>) -> String;

/// Port of `model.DeletionStepStatus` (post_deletion_report.go:12) — a Go `int` with `iota`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DeletionStepStatus {
    /// `iota`, so the zero value — an uninitialised step reads as **success**.
    #[default]
    Success,
    Failed,
    Partial,
    NotApplicable,
}

impl DeletionStepStatus {
    /// Port of `(DeletionStepStatus).Icon` (post_deletion_report.go:21).
    ///
    /// Go has a `default: "❓"` arm for an out-of-range value; that state is unrepresentable in a
    /// Rust enum, so the arm has no counterpart.
    ///
    /// **`⚠️` is two code points** (U+26A0 U+FE0F) — the variation selector is part of the
    /// literal and must survive.
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Success => "✅",
            Self::Failed => "❌",
            Self::Partial => "⚠️",
            Self::NotApplicable => "➖",
        }
    }

    /// Port of `(DeletionStepStatus).Label` (post_deletion_report.go:35).
    pub fn label(&self, t: TranslateFn<'_>) -> String {
        let id = match self {
            Self::Success => "app.data_spillage.report.status.removed",
            Self::Failed => "app.data_spillage.report.status.failed",
            Self::Partial => "app.data_spillage.report.status.partial",
            Self::NotApplicable => "app.data_spillage.report.status.not_applicable",
        };
        t(id, None)
    }
}

/// Port of `model.DeletionSubStep` (post_deletion_report.go:50) — one post revision.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeletionSubStep {
    /// A message **id**, translated at render time — not display text.
    pub name: String,
    pub status: DeletionStepStatus,
    /// Also a message id, or empty.
    pub detail: String,
    pub detail_params: Option<StringInterface>,
    pub errors: Vec<String>,
}

/// Port of `model.DeletionStepResult` (post_deletion_report.go:58).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeletionStepResult {
    pub name: String,
    pub status: DeletionStepStatus,
    pub detail: String,
    pub detail_params: Option<StringInterface>,
    pub errors: Vec<String>,
    /// When non-empty, the step renders as a revisions block rather than a single line.
    pub sub_steps: Vec<DeletionSubStep>,
}

/// Port of `model.PostDeletionReport` (post_deletion_report.go:67). No `json:` tags — it is
/// rendered, never marshalled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PostDeletionReport {
    pub post_id: String,
    /// Rendered as `2006-01-02 at 15:04:05 UTC` — **the literal `UTC` is part of the layout**, so
    /// a non-UTC timestamp is still labelled UTC.
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub steps: Vec<DeletionStepResult>,
}

impl PostDeletionReport {
    /// Port of `(*PostDeletionReport).AddStep` (post_deletion_report.go:73).
    pub fn add_step(
        &mut self,
        name: impl Into<String>,
        status: DeletionStepStatus,
        detail: impl Into<String>,
        errs: Vec<String>,
    ) {
        self.steps.push(DeletionStepResult {
            name: name.into(),
            status,
            detail: detail.into(),
            errors: errs,
            ..Default::default()
        });
    }

    /// Port of `(*PostDeletionReport).AddStepWithParams` (post_deletion_report.go:82).
    pub fn add_step_with_params(
        &mut self,
        name: impl Into<String>,
        status: DeletionStepStatus,
        detail: impl Into<String>,
        detail_params: Option<StringInterface>,
        errs: Vec<String>,
    ) {
        self.steps.push(DeletionStepResult {
            name: name.into(),
            status,
            detail: detail.into(),
            detail_params,
            errors: errs,
            ..Default::default()
        });
    }

    /// Port of `(*PostDeletionReport).translateDetail` (post_deletion_report.go:92).
    fn translate_detail(
        t: TranslateFn<'_>,
        detail: &str,
        detail_params: Option<&StringInterface>,
    ) -> String {
        if detail.is_empty() {
            return String::new();
        }
        match detail_params {
            Some(params) if !params.is_empty() => t(detail, Some(params)),
            _ => t(detail, None),
        }
    }

    /// Port of `(*PostDeletionReport).CountStatuses` (post_deletion_report.go:209) — returns
    /// `(success, failed, partial, not_applicable)`.
    pub fn count_statuses(&self) -> (i64, i64, i64, i64) {
        let mut success = 0;
        let mut failed = 0;
        let mut partial = 0;
        let mut not_applicable = 0;
        for step in &self.steps {
            match step.status {
                DeletionStepStatus::Success => success += 1,
                DeletionStepStatus::Failed => failed += 1,
                DeletionStepStatus::Partial => partial += 1,
                DeletionStepStatus::NotApplicable => not_applicable += 1,
            }
        }
        (success, failed, partial, not_applicable)
    }

    fn format_timestamp(&self) -> String {
        match self.timestamp {
            Some(t) => t.format("%Y-%m-%d at %H:%M:%S UTC").to_string(),
            None => String::new(),
        }
    }

    /// Port of `(*PostDeletionReport).Render` (post_deletion_report.go:104) — the full report.
    ///
    /// The header line orders the counts **removed, not applicable, partial, failed** — not the
    /// declaration order of the enum, and not worst-first.
    pub fn render(&self, t: TranslateFn<'_>) -> String {
        let mut b = String::new();

        let (success_count, failed_count, partial_count, not_applicable_count) =
            self.count_statuses();
        let total_steps = self.steps.len();

        b.push_str(&format!(
            "### {}\n\n",
            t("app.data_spillage.report.title", None)
        ));
        b.push_str(&format!(
            "**{}** {}\n",
            t("app.data_spillage.report.generated", None),
            self.format_timestamp()
        ));
        b.push_str(&format!(
            "**{}** `{}`\n",
            t("app.data_spillage.report.post_id", None),
            self.post_id
        ));
        b.push_str(&format!(
            "**{}** {} &nbsp;|&nbsp; ✅ {}: {} &nbsp;|&nbsp; ➖ {}: {} &nbsp;|&nbsp; ⚠️ {}: {} &nbsp;|&nbsp; ❌ {}: {}\n",
            t("app.data_spillage.report.total_steps", None),
            total_steps,
            t("app.data_spillage.report.status.removed", None),
            success_count,
            t("app.data_spillage.report.status.not_applicable", None),
            not_applicable_count,
            t("app.data_spillage.report.status.partial", None),
            partial_count,
            t("app.data_spillage.report.status.failed", None),
            failed_count
        ));
        b.push_str("\n---\n\n");

        for (i, step) in self.steps.iter().enumerate() {
            self.render_step(t, &mut b, i + 1, step);
        }

        b.push_str("---\n\n");
        self.render_summary_table(t, &mut b);

        if failed_count > 0 || partial_count > 0 {
            b.push_str(&format!(
                "\n> ⚠️ **{}**\n",
                t("app.data_spillage.report.incomplete_warning", None)
            ));
        }

        b
    }

    /// Port of `(*PostDeletionReport).RenderSummary` (post_deletion_report.go:135) — the table
    /// and the warning only, with **no header and no per-step sections**.
    pub fn render_summary(&self, t: TranslateFn<'_>) -> String {
        let mut b = String::new();

        let (_, failed_count, partial_count, _) = self.count_statuses();

        self.render_summary_table(t, &mut b);

        if failed_count > 0 || partial_count > 0 {
            b.push_str(&format!(
                "\n> ⚠️ **{}**\n",
                t("app.data_spillage.report.incomplete_warning", None)
            ));
        }

        b
    }

    /// Port of `(*PostDeletionReport).renderStep` (post_deletion_report.go:148).
    ///
    /// A step with sub-steps counts **anything that is not `Success` as failed** — `Partial` and
    /// `NotApplicable` included — because the sub-step tally is a two-way split.
    fn render_step(
        &self,
        t: TranslateFn<'_>,
        b: &mut String,
        num: usize,
        step: &DeletionStepResult,
    ) {
        let translated_name = t(&step.name, None);
        let translated_detail =
            Self::translate_detail(t, &step.detail, step.detail_params.as_ref());

        if !step.sub_steps.is_empty() {
            b.push_str(&format!("##### {num}. {translated_name}\n\n"));
            let mut success_count = 0;
            let mut failed_count = 0;
            for sub in &step.sub_steps {
                if sub.status == DeletionStepStatus::Success {
                    success_count += 1;
                } else {
                    failed_count += 1;
                }
            }
            b.push_str(&format!(
                "**{}** {} &nbsp;|&nbsp; ✅ {}: {} &nbsp;|&nbsp; ❌ {}: {}\n\n",
                t("app.data_spillage.report.revisions_found", None),
                step.sub_steps.len(),
                t("app.data_spillage.report.cleared", None),
                success_count,
                t("app.data_spillage.report.status.failed", None),
                failed_count
            ));

            for (j, sub) in step.sub_steps.iter().enumerate() {
                let sub_detail = Self::translate_detail(t, &sub.detail, sub.detail_params.as_ref());
                b.push_str(&format!(
                    "###### {} {} {} — `{}`\n",
                    sub.status.icon(),
                    t("app.data_spillage.report.revision", None),
                    j + 1,
                    sub.name
                ));
                if !sub_detail.is_empty() {
                    b.push_str(&format!("- {sub_detail}\n"));
                }
                Self::render_errors(t, b, &sub.errors);
                b.push('\n');
            }
        } else {
            b.push_str(&format!(
                "##### {num}. {} {translated_name}\n",
                step.status.icon()
            ));
            if !translated_detail.is_empty() {
                b.push_str(&format!("{translated_detail}\n"));
            }
            Self::render_errors(t, b, &step.errors);
        }
        b.push('\n');
    }

    /// Port of `(*PostDeletionReport).renderErrors` (post_deletion_report.go:186).
    ///
    /// Each error is split on `\n` and every line is prefixed with `> ` so a multi-line error
    /// stays inside the block quote. Note the fence is opened on the same line as the label.
    fn render_errors(t: TranslateFn<'_>, b: &mut String, errs: &[String]) {
        if errs.is_empty() {
            return;
        }
        b.push_str(&format!(
            "\n> **{}**\n> ```\n",
            t("app.data_spillage.report.error_log", None)
        ));
        for e in errs {
            for line in e.split('\n') {
                b.push_str(&format!("> {line}\n"));
            }
        }
        b.push_str("> ```\n");
    }

    /// Port of `(*PostDeletionReport).renderSummaryTable` (post_deletion_report.go:198).
    ///
    /// **The header row has three columns and the separator row has four** (`|---|---|---|---|`),
    /// because the body rows carry a leading number column the header does not name. Reproduced.
    fn render_summary_table(&self, t: TranslateFn<'_>, b: &mut String) {
        b.push_str(&format!(
            "##### 📊 {}\n\n",
            t("app.data_spillage.report.summary", None)
        ));
        b.push_str(&format!(
            "| # | {} | {} | {} |\n",
            t("app.data_spillage.report.column.step", None),
            t("app.data_spillage.report.column.status", None),
            t("app.data_spillage.report.column.detail", None)
        ));
        b.push_str("|---|---|---|---|\n");
        for (i, step) in self.steps.iter().enumerate() {
            let translated_name = t(&step.name, None);
            let translated_detail =
                Self::translate_detail(t, &step.detail, step.detail_params.as_ref());
            b.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                i + 1,
                translated_name,
                step.status.icon(),
                translated_detail
            ));
        }
    }
}

/// Port of `model.CountSubStepSuccesses` (post_deletion_report.go:225).
pub fn count_sub_step_successes(sub_steps: &[DeletionSubStep]) -> i64 {
    sub_steps
        .iter()
        .filter(|s| s.status == DeletionStepStatus::Success)
        .count() as i64
}
