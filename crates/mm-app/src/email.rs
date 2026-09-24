//! Port of `channels/app/email` — the `EmailService` behind every e-mail the server sends — and of
//! the `App` methods that mint a one-shot token and mail it (`SendPasswordReset`,
//! `SendEmailVerification`, `TestEmail`).
//!
//! # Where the pieces come from
//!
//! An e-mail is three ports deep. The body is one of `templates/*.html` executed by Go's
//! `html/template` ([`gotemplate`]) against a `templates.Data` whose strings are translated by
//! go-i18n ([`crate::i18n`]); the envelope and MIME structure are `platform/shared/mail`
//! ([`crate::mail`]) over go-mail and `net/smtp` ([`gomail`]), with the text/plain alternative
//! produced by html2text ([`gohtml2text`]). This module is only the Mattermost call sequence on top:
//! which template, which translation ids with which params, which category, which recipient.
//!
//! # The configuration is read per send, whole
//!
//! Go's `es.config()` is the live `*model.Config`, and a send reads a dozen settings across four
//! sections (`EmailSettings`, `SupportSettings`, `TeamSettings`, `ServiceSettings`). They are read
//! here from [`crate::config::load_model_config`] — the persisted document with this process's
//! environment overlaid — rather than from the projection in [`crate::Config`], which models a
//! setting only when a gate reads it. The SMTP settings in particular are overridden per stack by
//! environment (`scripts/stack-env.sh`), exactly as on the Go server beside it.
//!
//! # What is not here
//!
//! Email batching, the notification e-mail (`notification_email.go`), invitations, the cloud and
//! licence e-mails, and the rate limiters (which only the invitation path consults). Each is owed
//! with the route that sends it.

use std::collections::BTreeMap;

use gotemplate::{HtmlTemplates, Value};
use mm_model::token::{TOKEN_TYPE_PASSWORD_RECOVERY, TOKEN_TYPE_VERIFY_EMAIL, Token};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, TokenStore};

use crate::App;
use crate::i18n::{Params, Translations};

/// The errors a send can fail with. Go returns a bare `error` from every `EmailService` method
/// and its callers wrap it into an `AppError` (or only log it); the variants keep the causes
/// typed until that wrap.
#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// No `templates` directory was found — a server Go refuses to start
    /// (`email.NewService` requires a `TemplatesContainer`).
    #[error("unable to find the templates directory")]
    NoTemplates,
    #[error(transparent)]
    Template(#[from] gotemplate::Error),
    /// The large-stack thread the recursive ports run on could not be started
    /// ([`crate::deep_stack`]).
    #[error("could not start a rendering thread: {0}")]
    Thread(std::io::Error),
    #[error(transparent)]
    Mail(#[from] crate::mail::MailError),
    /// The translations failed to load — also a server Go refuses to start.
    #[error("the translations are not loaded")]
    NoTranslations,
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An app-layer failure inside a send — the verify-token mint in `SendWelcomeEmail`.
    #[error("{0}")]
    App(Box<AppError>),
    /// Go's `SendWelcomeEmail` refusal, with its own text.
    #[error(
        "send email notifications and require email verification is disabled in the system console"
    )]
    WelcomeDisabled,
}

/// `email.TokenTypePasswordRecovery` and friends live in `mm_model::token`; this is the one
/// shape both invalidations parse out of `Token.Extra`.
///
/// Go declares the struct inline, twice, with no tags — so the keys are the field names and
/// `json.Marshal` writes them in declaration order.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TokenExtra {
    #[serde(rename = "UserId", default)]
    user_id: String,
    #[serde(rename = "Email", default)]
    email: String,
}

/// Port of `condenseSiteURL` (email/service.go:41): the host alone for a site at the root, host
/// and path joined otherwise.
///
/// Go ignores `url.Parse`'s error and dereferences the result, so an unparseable `SiteURL` panics
/// the send's goroutine. Here it condenses to the empty string, the zero value Go would have
/// printed had it not crashed.
pub fn condense_site_url(site_url: &str) -> String {
    let Ok(parsed) = mm_model::go_url::go_parse(site_url) else {
        return String::new();
    };
    let host = String::from_utf8_lossy(&parsed.host).into_owned();
    let path = String::from_utf8_lossy(&parsed.path).into_owned();
    if path.is_empty() || path == "/" {
        return host;
    }
    go_path_join(&host, &path)
}

/// `path.Join(a, b)` for the two-element case `condenseSiteURL` needs: join with `/`, then
/// `path.Clean` — which collapses repeated slashes, drops `.` and resolves `..`.
fn go_path_join(a: &str, b: &str) -> String {
    let joined = match (a.is_empty(), b.is_empty()) {
        (true, true) => return String::new(),
        (true, false) => b.to_owned(),
        (false, true) => a.to_owned(),
        (false, false) => format!("{a}/{b}"),
    };
    go_path_clean(&joined)
}

/// Port of `path.Clean` (path/path.go:74).
fn go_path_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let body = out.join("/");
    match (rooted, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".to_owned(),
        (false, false) => body,
    }
}

/// `utils.GetHostnameFromSiteURL` (channels/utils/utils.go:116): the parsed URL's hostname, or
/// `""` when it does not parse.
fn hostname_from_site_url(site_url: &str) -> String {
    mm_model::go_url::go_parse(site_url)
        .map(|u| String::from_utf8_lossy(&u.hostname()).into_owned())
        .unwrap_or_default()
}

/// A string a `templates.Data` prop or a translation param holds.
fn s(value: impl Into<String>) -> Value {
    Value::String(value.into())
}

/// The process's `templates.Container`: every `templates/*.html`, parsed once.
///
/// Go builds it at startup from `templates.GetTemplateDirectory()` — `fileutils.FindDir`, which
/// resolves against the working directory first, the same search [`crate::logs::find_dir`]
/// ports. `scripts/mm-api-env.sh` launches this process from the Go server's run directory, so
/// both read the same files.
static TEMPLATES: tokio::sync::OnceCell<Option<HtmlTemplates>> = tokio::sync::OnceCell::const_new();

/// Port of `templates.New(dir)` (platform/shared/templates/templates.go:41): `ParseGlob` over
/// `*.html`, whose `filepath.Glob` hands `ParseFiles` the names **sorted**.
pub fn load_templates(dir: &std::path::Path) -> Result<HtmlTemplates, EmailError> {
    Ok(HtmlTemplates::parse_glob_html(dir)?)
}

/// Execute one of `templates/*.html` — `TemplatesContainer().RenderToString(name, data)`.
pub(crate) async fn render_template(name: &str, data: TemplateData) -> Result<String, EmailError> {
    let templates = templates().await?;
    let value = data.into_value();
    Ok(
        crate::deep_stack::run(|| templates.execute(name, &value))
            .map_err(EmailError::Thread)??,
    )
}

async fn templates() -> Result<&'static HtmlTemplates, EmailError> {
    TEMPLATES
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| {
                let (dir, found) = crate::logs::find_dir("templates");
                if !found {
                    tracing::error!("no templates directory; e-mail cannot be rendered");
                    return None;
                }
                load_templates(&dir)
                    .map_err(|err| tracing::error!(error = %err, "could not parse the templates"))
                    .ok()
            })
            .await
            .ok()
            .flatten()
        })
        .await
        .as_ref()
        .ok_or(EmailError::NoTemplates)
}

/// A translate function bound to one locale: `i18n.GetUserTranslations(locale)` or `i18n.T`.
pub struct Tr {
    bundle: &'static Translations,
    locale: String,
}

impl Tr {
    /// `T(id)`.
    pub fn t(&self, id: &str) -> String {
        self.bundle.translate(&self.locale, id)
    }

    /// `T(id, map[string]any{...})`.
    pub fn tp(&self, id: &str, params: &[(&str, serde_json::Value)]) -> String {
        let params: Params = params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        self.bundle.translate_with(&self.locale, id, Some(&params))
    }
}

/// Port of `i18n.GetUserTranslations(locale)` (shared/i18n/i18n.go:251): the locale itself when
/// the server loaded it — an **exact**, case-sensitive match, unlike the fallback chain a request
/// locale goes through — else `en`.
pub async fn user_translations(locale: &str) -> Result<Tr, EmailError> {
    let bundle = crate::i18n::translations()
        .await
        .ok_or(EmailError::NoTranslations)?;
    Ok(Tr {
        bundle,
        locale: bundle.user_locale(locale).to_owned(),
    })
}

/// `i18n.T`: the translations of `LocalizationSettings.DefaultServerLocale`.
async fn server_translations(config: &mm_model::config::Config) -> Result<Tr, EmailError> {
    let bundle = crate::i18n::translations()
        .await
        .ok_or(EmailError::NoTranslations)?;
    let default = config
        .localization_settings
        .default_server_locale
        .as_deref()
        .unwrap_or("en");
    Ok(Tr {
        bundle,
        locale: bundle.server_locale(default).to_owned(),
    })
}

/// `*p` for a `*string` setting Go dereferences unconditionally. `SetDefaults` has filled every
/// one of them in a persisted document, so `None` only arises from a document nothing but a test
/// wrote; it reads as Go's zero value rather than a panic.
fn setting(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or_default()
}

/// The props every body starts from, plus what one send adds, as `templates.Data`.
pub struct TemplateData {
    pub props: BTreeMap<String, Value>,
    /// `Data.HTML` — `template.HTML` values. No send ported here puts anything in it.
    pub html: BTreeMap<String, Value>,
}

impl TemplateData {
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.props.insert(key.to_owned(), s(value));
    }

    fn into_value(self) -> Value {
        Value::Struct(
            "templates.Data".to_owned(),
            vec![
                ("Props".to_owned(), Value::Map(self.props)),
                ("HTML".to_owned(), Value::Map(self.html)),
            ],
        )
    }
}

/// `time.Now().Year()` in the server's local zone, which the footer's `CurrentYear` prints.
fn current_year() -> i64 {
    i64::from(chrono::Datelike::year(&chrono::Local::now()))
}

/// The config values the senders need, read once per send.
struct SendContext {
    config: mm_model::config::Config,
    license: Option<std::sync::Arc<mm_model::license::License>>,
}

impl SendContext {
    fn site_name(&self) -> &str {
        setting(&self.config.team_settings.site_name)
    }

    fn site_name_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.site_name().to_owned())
    }
}

impl App {
    async fn email_context(&self) -> Result<SendContext, EmailError> {
        let config = crate::config::load_model_config(self.store().config()).await?;
        // A licence the store cannot read is Go's nil licence here: `es.license()` never errors.
        let license = self.license().await.ok().flatten();
        Ok(SendContext { config, license })
    }

    /// Port of `App.GetSiteURL` (app/config.go:200) against the live configuration.
    pub async fn live_site_url(&self) -> Result<String, EmailError> {
        let config = crate::config::load_model_config(self.store().config()).await?;
        Ok(setting(&config.service_settings.site_url).to_owned())
    }

    /// Port of `Service.NewEmailTemplateData` (email/email.go:861).
    ///
    /// **An empty locale is the server's `T`, not English.** Every sender ported here passes the
    /// recipient's locale, which the user model never leaves empty, so the branch is reached only
    /// by a caller that means it.
    async fn new_email_template_data(
        &self,
        ctx: &SendContext,
        locale: &str,
    ) -> Result<TemplateData, EmailError> {
        let local_t = if locale.is_empty() {
            server_translations(&ctx.config).await?
        } else {
            user_translations(locale).await?
        };
        let email = &ctx.config.email_settings;
        let feedback_organization = setting(&email.feedback_organization);
        let organization = if feedback_organization.is_empty() {
            String::new()
        } else {
            local_t.t("api.templates.email_organization") + feedback_organization
        };

        let mut props = BTreeMap::new();
        props.insert(
            "EmailInfo1".to_owned(),
            s(local_t.t("api.templates.email_info1")),
        );
        props.insert(
            "EmailInfo2".to_owned(),
            s(local_t.t("api.templates.email_info2")),
        );
        props.insert(
            "EmailInfo3".to_owned(),
            s(local_t.tp(
                "api.templates.email_info3",
                &[("SiteName", ctx.site_name_value())],
            )),
        );
        props.insert(
            "SupportEmail".to_owned(),
            s(setting(&ctx.config.support_settings.support_email)),
        );
        props.insert(
            "Footer".to_owned(),
            s(local_t.t("api.templates.email_footer")),
        );
        props.insert(
            "FooterV2".to_owned(),
            s(local_t.tp(
                "api.templates.email_footer_v2",
                &[("CurrentYear", serde_json::Value::from(current_year()))],
            )),
        );
        props.insert("Organization".to_owned(), s(organization));
        Ok(TemplateData {
            props,
            html: BTreeMap::new(),
        })
    }

    /// [`App::new_email_template_data`] with the live configuration read here, for callers
    /// outside this module.
    pub(crate) async fn new_email_template_data_for(
        &self,
        locale: &str,
    ) -> Result<TemplateData, EmailError> {
        let ctx = self.email_context().await?;
        self.new_email_template_data(&ctx, locale).await
    }

    /// Port of `Service.SendMailWithEmbeddedFiles` (email/email.go:955): the post notification's
    /// send, with its own message id and thread headers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_mail_with_embedded_files(
        &self,
        to: &str,
        subject: &str,
        html_body: &str,
        embedded: &[(String, Vec<u8>)],
        message_id: &str,
        in_reply_to: &str,
        references: &str,
        category: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        let config = Self::mail_service_config(&ctx, "");
        let is_cloud = ctx.license.as_ref().is_some_and(|l| l.is_cloud());
        let category = if is_cloud { category } else { "" };
        crate::mail::send_mail_with_embedded_files_using_config(
            to,
            subject,
            html_body,
            embedded,
            &config,
            message_id,
            in_reply_to,
            references,
            "",
            category,
        )
        .await?;
        Ok(())
    }

    /// Port of `Service.mailServiceConfig` (email/utils.go:11).
    fn mail_service_config(ctx: &SendContext, reply_to_address: &str) -> crate::mail::SmtpConfig {
        let email = &ctx.config.email_settings;
        let reply_to = if reply_to_address.is_empty() {
            setting(&email.reply_to_address)
        } else {
            reply_to_address
        };
        let server = setting(&email.smtp_server).to_owned();
        crate::mail::SmtpConfig {
            hostname: hostname_from_site_url(setting(&ctx.config.service_settings.site_url)),
            connection_security: setting(&email.connection_security).to_owned(),
            skip_server_certificate_verification: email
                .skip_server_certificate_verification
                .unwrap_or(false),
            server_name: server.clone(), // Go copies `*SMTPServer` into both fields.
            server,
            port: setting(&email.smtp_port).to_owned(),
            server_timeout: email.smtp_server_timeout.unwrap_or(0),
            username: setting(&email.smtp_username).to_owned(),
            password: setting(&email.smtp_password).to_owned(),
            enable_smtp_auth: email.enable_smtp_auth.unwrap_or(false),
            send_email_notifications: email.send_email_notifications.unwrap_or(false),
            feedback_name: setting(&email.feedback_name).to_owned(),
            feedback_email: setting(&email.feedback_email).to_owned(),
            reply_to_address: reply_to.to_owned(),
        }
    }

    /// Port of `Service.sendMailWithCC` (email/email.go:937) — and, with `cc` empty, of
    /// `sendMail`. The SendGrid category header is written only under a **cloud** licence
    /// (`getSendGridCategory`), which a self-hosted server never holds.
    async fn send_mail_with_cc(
        ctx: &SendContext,
        to: &str,
        subject: &str,
        html_body: &str,
        cc: &str,
        category: &str,
    ) -> Result<(), EmailError> {
        let config = Self::mail_service_config(ctx, "");
        let is_cloud = ctx.license.as_ref().is_some_and(|l| l.is_cloud());
        let category = if is_cloud { category } else { "" };
        crate::mail::send_mail_using_config(
            to, subject, html_body, &config, "", "", "", cc, category,
        )
        .await?;
        Ok(())
    }

    async fn render(name: &str, data: TemplateData) -> Result<String, EmailError> {
        render_template(name, data).await
    }

    /// Port of `Service.SendPasswordResetEmail` (email/email.go:364). `Ok(true)` once sent.
    #[tracing::instrument(skip_all)]
    pub async fn send_password_reset_email(
        &self,
        email: &str,
        token: &Token,
        locale: &str,
        site_url: &str,
    ) -> Result<bool, EmailError> {
        let ctx = self.email_context().await?;
        let t = user_translations(locale).await?;
        let link = format!(
            "{site_url}/reset_password_complete?token={}",
            mm_model::go_url::query_escape(&token.token)
        );
        let subject = t.tp(
            "api.templates.reset_subject",
            &[("SiteName", ctx.site_name_value())],
        );
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set("Title", t.t("api.templates.reset_body.title"));
        data.set("SubTitle", t.t("api.templates.reset_body.subTitle"));
        data.set("Info", t.t("api.templates.reset_body.info"));
        data.set("ButtonURL", link);
        data.set("Button", t.t("api.templates.reset_body.button"));
        data.set("QuestionTitle", t.t("api.templates.questions_footer.title"));
        data.set("QuestionInfo", t.t("api.templates.questions_footer.info"));
        let body = Self::render("reset_body", data).await?;
        Self::send_mail_with_cc(&ctx, email, &subject, &body, "", "PasswordResetEmail").await?;
        Ok(true)
    }

    /// Port of `Service.SendVerifyEmail` (email/email.go:123).
    ///
    /// The redirect is appended **unescaped** — `fmt.Sprintf("&redirect_to=%s", redirect)` — while
    /// the address is `url.QueryEscape`d. A redirect carrying `&` therefore adds parameters to the
    /// link. That is Go's, and the link is what the client follows.
    #[tracing::instrument(skip_all)]
    pub async fn send_verify_email(
        &self,
        user_email: &str,
        locale: &str,
        site_url: &str,
        token: &str,
        redirect: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        let t = user_translations(locale).await?;
        let mut link = format!(
            "{site_url}/do_verify_email?token={token}&email={}",
            mm_model::go_url::query_escape(user_email)
        );
        if !redirect.is_empty() {
            link.push_str(&format!("&redirect_to={redirect}"));
        }
        let server_url = condense_site_url(site_url);
        let subject = t.tp(
            "api.templates.verify_subject",
            &[("SiteName", ctx.site_name_value())],
        );
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set("Title", t.t("api.templates.verify_body.title"));
        data.set("SubTitle1", t.t("api.templates.verify_body.subTitle1"));
        data.set(
            "ServerURL",
            t.tp(
                "api.templates.verify_body.serverURL",
                &[("ServerURL", serde_json::Value::String(server_url))],
            ),
        );
        data.set("SubTitle2", t.t("api.templates.verify_body.subTitle2"));
        data.set("ButtonURL", link);
        data.set("Button", t.t("api.templates.verify_body.button"));
        data.set("Info", t.t("api.templates.verify_body.info"));
        data.set("Info1", t.t("api.templates.verify_body.info1"));
        data.set("QuestionTitle", t.t("api.templates.questions_footer.title"));
        data.set("QuestionInfo", t.t("api.templates.questions_footer.info"));
        let body = Self::render("verify_body", data).await?;
        Self::send_mail_with_cc(&ctx, user_email, &subject, &body, "", "VerifyEmail").await
    }

    /// Port of `Service.SendEmailChangeVerifyEmail` (email/email.go:64).
    ///
    /// Two props are **not** the template-data defaults: `EmailInfo1` is overwritten with
    /// `api.templates.email_us_anytime_at`, and `SupportEmail` with the literal
    /// `feedback@mattermost.com` — whatever `SupportSettings.SupportEmail` says.
    #[tracing::instrument(skip_all)]
    pub async fn send_email_change_verify_email(
        &self,
        new_user_email: &str,
        locale: &str,
        site_url: &str,
        token: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        let t = user_translations(locale).await?;
        let link = format!(
            "{site_url}/do_verify_email?token={token}&email={}",
            mm_model::go_url::query_escape(new_user_email)
        );
        let subject = t.tp(
            "api.templates.email_change_verify_subject",
            &[
                ("SiteName", ctx.site_name_value()),
                ("TeamDisplayName", ctx.site_name_value()),
            ],
        );
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set("Title", t.t("api.templates.email_change_verify_body.title"));
        data.set(
            "Info",
            t.tp(
                "api.templates.email_change_verify_body.info",
                &[("TeamDisplayName", ctx.site_name_value())],
            ),
        );
        data.set("VerifyUrl", link);
        data.set(
            "VerifyButton",
            t.t("api.templates.email_change_verify_body.button"),
        );
        data.set("QuestionTitle", t.t("api.templates.questions_footer.title"));
        data.set("EmailInfo1", t.t("api.templates.email_us_anytime_at"));
        data.set("SupportEmail", "feedback@mattermost.com");
        data.set(
            "FooterV2",
            t.tp(
                "api.templates.email_footer_v2",
                &[("CurrentYear", serde_json::Value::from(current_year()))],
            ),
        );
        let body = Self::render("email_change_verify_body", data).await?;
        Self::send_mail_with_cc(
            &ctx,
            new_user_email,
            &subject,
            &body,
            "",
            "EmailChangeVerifyEmail",
        )
        .await
    }

    /// The four notices that share one shape — a title, an info line with params and
    /// `api.templates.email_warning` — rendered with `template`: `SendEmailChangeEmail`,
    /// `SendPasswordChangeEmail`, `SendSignInChangeEmail` and `SendMfaChangeEmail`
    /// (email/email.go:97, :288, :161, :394).
    #[allow(clippy::too_many_arguments)]
    async fn send_notice(
        &self,
        to: &str,
        locale: &str,
        site_url: &str,
        subject: (&str, &[(&str, serde_json::Value)]),
        title_id: &str,
        info: (&str, &[(&str, serde_json::Value)]),
        template: &str,
        category: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        let t = user_translations(locale).await?;
        let subject = t.tp(subject.0, &with_site_name(subject.1, &ctx));
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set("Title", t.t(title_id));
        data.set("Info", t.tp(info.0, &with_site_name(info.1, &ctx)));
        data.set("Warning", t.t("api.templates.email_warning"));
        let body = Self::render(template, data).await?;
        Self::send_mail_with_cc(&ctx, to, &subject, &body, "", category).await
    }

    /// Port of `Service.SendEmailChangeEmail` (email/email.go:97) — sent to the **old** address.
    #[tracing::instrument(skip_all)]
    pub async fn send_email_change_email(
        &self,
        old_email: &str,
        new_email: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        self.send_notice(
            old_email,
            locale,
            site_url,
            (
                "api.templates.email_change_subject",
                &[("SiteName", SITE_NAME), ("TeamDisplayName", SITE_NAME)],
            ),
            "api.templates.email_change_body.title",
            (
                "api.templates.email_change_body.info",
                &[
                    ("TeamDisplayName", SITE_NAME),
                    ("NewEmail", serde_json::Value::String(new_email.to_owned())),
                ],
            ),
            "email_change_body",
            "EmailChangeEmail",
        )
        .await
    }

    /// Port of `Service.SendPasswordChangeEmail` (email/email.go:288). `method` is already
    /// translated by the caller (`api.user.update_password.menu` and friends).
    #[tracing::instrument(skip_all)]
    pub async fn send_password_change_email(
        &self,
        email: &str,
        method: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        self.send_notice(
            email,
            locale,
            site_url,
            (
                "api.templates.password_change_subject",
                &[("SiteName", SITE_NAME), ("TeamDisplayName", SITE_NAME)],
            ),
            "api.templates.password_change_body.title",
            (
                "api.templates.password_change_body.info",
                &[
                    ("TeamDisplayName", SITE_NAME),
                    ("TeamURL", serde_json::Value::String(site_url.to_owned())),
                    ("Method", serde_json::Value::String(method.to_owned())),
                ],
            ),
            "password_change_body",
            "PasswordChangeEmail",
        )
        .await
    }

    /// Port of `Service.SendMfaChangeEmail` (email/email.go:394). Note the info line comes
    /// **before** the title in Go, which changes nothing: both are map writes.
    #[tracing::instrument(skip_all)]
    pub async fn send_mfa_change_email(
        &self,
        email: &str,
        activated: bool,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        let (info, title) = if activated {
            (
                "api.templates.mfa_activated_body.info",
                "api.templates.mfa_activated_body.title",
            )
        } else {
            (
                "api.templates.mfa_deactivated_body.info",
                "api.templates.mfa_deactivated_body.title",
            )
        };
        self.send_notice(
            email,
            locale,
            site_url,
            (
                "api.templates.mfa_change_subject",
                &[("SiteName", SITE_NAME)],
            ),
            title,
            (
                info,
                &[("SiteURL", serde_json::Value::String(site_url.to_owned()))],
            ),
            "mfa_change_body",
            "MfaChangeEmail",
        )
        .await
    }

    /// Port of `Service.SendUserAccessTokenAddedEmail` (email/email.go:314) — rendered with
    /// `password_change_body`, which is the only template of that shape.
    #[tracing::instrument(skip_all)]
    pub async fn send_user_access_token_added_email(
        &self,
        email: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        self.send_notice(
            email,
            locale,
            site_url,
            (
                "api.templates.user_access_token_subject",
                &[("SiteName", SITE_NAME)],
            ),
            "api.templates.user_access_token_body.title",
            (
                "api.templates.user_access_token_body.info",
                &[
                    ("SiteName", SITE_NAME),
                    ("SiteURL", serde_json::Value::String(site_url.to_owned())),
                ],
            ),
            "password_change_body",
            "UserAccessTokenAddedEmail",
        )
        .await
    }

    /// Port of `Service.SendUserAccessTokenRotatedEmail` (email/email.go:339).
    #[tracing::instrument(skip_all)]
    pub async fn send_user_access_token_rotated_email(
        &self,
        email: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        self.send_notice(
            email,
            locale,
            site_url,
            (
                "api.templates.user_access_token_rotated_subject",
                &[("SiteName", SITE_NAME)],
            ),
            "api.templates.user_access_token_rotated_body.title",
            (
                "api.templates.user_access_token_rotated_body.info",
                &[
                    ("SiteName", SITE_NAME),
                    ("SiteURL", serde_json::Value::String(site_url.to_owned())),
                ],
            ),
            "password_change_body",
            "UserAccessTokenRotatedEmail",
        )
        .await
    }

    /// Port of `Service.SendChangeUsernameEmail` (email/email.go:38) — rendered with
    /// `email_change_body`, and with `TeamDisplayName` set to the **site** name in both the
    /// subject and the body.
    #[tracing::instrument(skip_all)]
    pub async fn send_change_username_email(
        &self,
        new_username: &str,
        email: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        self.send_notice(
            email,
            locale,
            site_url,
            (
                "api.templates.username_change_subject",
                &[("SiteName", SITE_NAME), ("TeamDisplayName", SITE_NAME)],
            ),
            "api.templates.username_change_body.title",
            (
                "api.templates.username_change_body.info",
                &[
                    ("TeamDisplayName", SITE_NAME),
                    (
                        "NewUsername",
                        serde_json::Value::String(new_username.to_owned()),
                    ),
                ],
            ),
            "email_change_body",
            "ChangeUsernameEmail",
        )
        .await
    }

    /// Port of `Service.SendNotificationMail` (email/email.go:917): nothing, successfully, while
    /// `SendEmailNotifications` is off — read from the **live** configuration at send time.
    #[tracing::instrument(skip_all)]
    pub async fn send_notification_mail(
        &self,
        to: &str,
        subject: &str,
        html_body: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        if !ctx
            .config
            .email_settings
            .send_email_notifications
            .unwrap_or(false)
        {
            return Ok(());
        }
        Self::send_mail_with_cc(&ctx, to, subject, html_body, "", "NotificationEmail").await
    }

    /// Port of `Service.SendDeactivateAccountEmail` (email/email.go:889).
    ///
    /// Its title takes a param and its warning is its own id, so it does not fit
    /// [`App::send_notice`].
    #[tracing::instrument(skip_all)]
    pub async fn send_deactivate_account_email(
        &self,
        email: &str,
        locale: &str,
        site_url: &str,
    ) -> Result<(), EmailError> {
        let ctx = self.email_context().await?;
        let t = user_translations(locale).await?;
        let server_url = serde_json::Value::String(condense_site_url(site_url));
        let subject = t.tp(
            "api.templates.deactivate_subject",
            &[
                ("SiteName", ctx.site_name_value()),
                ("ServerURL", server_url.clone()),
            ],
        );
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set(
            "Title",
            t.tp(
                "api.templates.deactivate_body.title",
                &[("ServerURL", server_url)],
            ),
        );
        data.set(
            "Info",
            t.tp(
                "api.templates.deactivate_body.info",
                &[("SiteURL", serde_json::Value::String(site_url.to_owned()))],
            ),
        );
        data.set("Warning", t.t("api.templates.deactivate_body.warning"));
        let body = Self::render("deactivate_body", data).await?;
        Self::send_mail_with_cc(&ctx, email, &subject, &body, "", "DeactivateAccountEmail").await
    }

    /// Port of `Service.SendWelcomeEmail` (email/email.go:186).
    ///
    /// # Two early exits, in this order
    ///
    /// `disableWelcomeEmail` returns **nil** — nothing is sent and nothing is wrong. Otherwise,
    /// with both `SendEmailNotifications` and `RequireEmailVerification` off it is an **error**,
    /// which every caller only logs. So on a stock server with notifications off an account is
    /// created silently either way; the difference is a warning in the log.
    ///
    /// # The verify link is minted only when it will be needed
    ///
    /// An unverified user on a server that requires verification gets a `verify_email` token and
    /// a `ButtonURL`; everyone else gets the same body with no button URL — the template's
    /// `{{if .Props.ButtonURL}}` drops the button. The token is minted **before** the body is
    /// rendered, so a render or send failure leaves it behind, as in Go.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn send_welcome_email(
        &self,
        user_id: &str,
        email: &str,
        verified: bool,
        disable_welcome_email: bool,
        locale: &str,
        site_url: &str,
        redirect: &str,
    ) -> Result<(), EmailError> {
        if disable_welcome_email {
            return Ok(());
        }
        let ctx = self.email_context().await?;
        let settings = &ctx.config.email_settings;
        let require_verification = settings.require_email_verification.unwrap_or(false);
        if !settings.send_email_notifications.unwrap_or(false) && !require_verification {
            return Err(EmailError::WelcomeDisabled);
        }

        let t = user_translations(locale).await?;
        let server_url = serde_json::Value::String(condense_site_url(site_url));
        let subject = t.tp(
            "api.templates.welcome_subject",
            &[
                ("SiteName", ctx.site_name_value()),
                ("ServerURL", server_url.clone()),
            ],
        );
        let mut data = self.new_email_template_data(&ctx, locale).await?;
        data.set("SiteURL", site_url);
        data.set("Title", t.t("api.templates.welcome_body.title"));
        data.set("SubTitle1", t.t("api.templates.welcome_body.subTitle1"));
        data.set(
            "ServerURL",
            t.tp(
                "api.templates.welcome_body.serverURL",
                &[("ServerURL", server_url)],
            ),
        );
        data.set("SubTitle2", t.t("api.templates.welcome_body.subTitle2"));
        data.set("Button", t.t("api.templates.welcome_body.button"));
        data.set("Info", t.t("api.templates.welcome_body.info"));
        data.set("Info1", t.t("api.templates.welcome_body.info1"));

        let app_download_link = setting(&ctx.config.native_app_settings.app_download_link);
        if !app_download_link.is_empty() {
            data.set(
                "AppDownloadTitle",
                t.t("api.templates.welcome_body.app_download_title"),
            );
            data.set(
                "AppDownloadInfo",
                t.t("api.templates.welcome_body.app_download_info"),
            );
            data.set(
                "AppDownloadButton",
                t.t("api.templates.welcome_body.app_download_button"),
            );
            data.set("AppDownloadLink", app_download_link);
        }

        if !verified && require_verification {
            let token = self
                .create_verify_email_token(user_id, email)
                .await
                .map_err(EmailError::App)?;
            let mut link = format!(
                "{site_url}/do_verify_email?token={}&email={}",
                token.token,
                mm_model::go_url::query_escape(email)
            );
            if !redirect.is_empty() {
                link.push_str(&format!("&redirect_to={redirect}"));
            }
            data.set("ButtonURL", link);
        }

        let body = Self::render("welcome_body", data).await?;
        Self::send_mail_with_cc(&ctx, email, &subject, &body, "", "WelcomeEmail").await
    }

    // -----------------------------------------------------------------------------------------
    // Tokens
    // -----------------------------------------------------------------------------------------

    /// The loop `InvalidatePasswordRecoveryTokensForUser` (app/user.go:1976) and
    /// `Service.InvalidateVerifyEmailTokensForUser` (email/email.go:964) share: every token of the
    /// type, parsed, and the user's deleted.
    ///
    /// **A failure does not stop the loop.** A token whose `Extra` will not parse, or whose delete
    /// fails, records its error and the loop moves on; the *last* recorded error is returned.
    async fn invalidate_tokens_for_user(
        &self,
        token_type: &str,
        user_id: &str,
        where_: &str,
        ids: [&str; 3],
    ) -> AppResult {
        let [list_id, parse_id, delete_id] = ids;
        let tokens = self
            .store()
            .token()
            .get_all_tokens_by_type(token_type)
            .await
            .map_err(|err| AppError::boxed(where_, list_id, None, "", 500).wrap_boxed(err))?;

        let mut last: AppResult = Ok(());
        for token in tokens {
            let extra: TokenExtra = match serde_json::from_str(&token.extra) {
                Ok(extra) => extra,
                Err(err) => {
                    last = Err(AppError::boxed(where_, parse_id, None, "", 500).wrap_boxed(err));
                    continue;
                }
            };
            if extra.user_id != user_id {
                continue;
            }
            if let Err(err) = self.store().token().delete(&token.token).await {
                last = Err(AppError::boxed(where_, delete_id, None, "", 500).wrap_boxed(err));
            }
        }
        last
    }

    /// Port of `App.InvalidatePasswordRecoveryTokensForUser` (app/user.go:1976).
    pub async fn invalidate_password_recovery_tokens_for_user(&self, user_id: &str) -> AppResult {
        self.invalidate_tokens_for_user(
            TOKEN_TYPE_PASSWORD_RECOVERY,
            user_id,
            "InvalidatePasswordRecoveryTokensForUser",
            [
                "api.user.invalidate_password_recovery_tokens.error",
                "api.user.invalidate_password_recovery_tokens_parse.error",
                "api.user.invalidate_password_recovery_tokens_delete.error",
            ],
        )
        .await
    }

    /// Port of `Service.InvalidateVerifyEmailTokensForUser` (email/email.go:964).
    pub async fn invalidate_verify_email_tokens_for_user(&self, user_id: &str) -> AppResult {
        self.invalidate_tokens_for_user(
            TOKEN_TYPE_VERIFY_EMAIL,
            user_id,
            "InvalidateVerifyEmailTokensForUser",
            [
                "api.user.invalidate_verify_email_tokens.error",
                "api.user.invalidate_verify_email_tokens_parse.error",
                "api.user.invalidate_verify_email_tokens_delete.error",
            ],
        )
        .await
    }

    /// `json.Marshal` of the inline `{UserId, Email}` struct both token creators build.
    fn token_extra(user_id: &str, email: &str) -> Result<String, serde_json::Error> {
        mm_model::utils::go_json_marshal(&TokenExtra {
            user_id: user_id.to_owned(),
            email: email.to_owned(),
        })
    }

    /// Port of `App.CreatePasswordRecoveryToken` (app/user.go:1943).
    ///
    /// **The invalidation's failure is only logged**, so a user whose old tokens could not be
    /// removed still gets a new one. The save's failure is passed through as itself when the
    /// store returned an `AppError` (`token.IsValid()`), and wrapped as
    /// `app.recover.save.app_error` otherwise.
    pub async fn create_password_recovery_token(
        &self,
        user_id: &str,
        email: &str,
    ) -> AppResult<Token> {
        let extra = Self::token_extra(user_id, email).map_err(|err| {
            AppError::boxed(
                "CreatePasswordRecoveryToken",
                "api.user.create_password_token.error",
                None,
                "",
                500,
            )
            .wrap_boxed(err)
        })?;
        if let Err(err) = self
            .invalidate_password_recovery_tokens_for_user(user_id)
            .await
        {
            tracing::warn!(error = %err.id, "Error while deleting additional user tokens.");
        }
        let token = Token::new(TOKEN_TYPE_PASSWORD_RECOVERY, extra);
        self.store()
            .token()
            .save(&token)
            .await
            .map_err(|err| save_error("CreatePasswordRecoveryToken", err))?;
        Ok(token)
    }

    /// Port of `Service.CreateVerifyEmailToken` (email/email.go:993).
    ///
    /// Unlike the password-recovery creator, **the invalidation's failure aborts**: Go returns it
    /// (as the `*AppError` it is) before saving.
    pub async fn create_verify_email_token(
        &self,
        user_id: &str,
        new_email: &str,
    ) -> AppResult<Token> {
        let extra = Self::token_extra(user_id, new_email).map_err(|err| {
            AppError::boxed(
                "CreateVerifyEmailToken",
                "api.user.create_email_token.error",
                None,
                "",
                500,
            )
            .wrap_boxed(err)
        })?;
        let token = Token::new(TOKEN_TYPE_VERIFY_EMAIL, extra);
        self.invalidate_verify_email_tokens_for_user(user_id)
            .await?;
        self.store()
            .token()
            .save(&token)
            .await
            .map_err(|err| save_error("CreateVerifyEmailToken", err))?;
        Ok(token)
    }

    // -----------------------------------------------------------------------------------------
    // The App methods the routes call
    // -----------------------------------------------------------------------------------------

    /// The tail of `App.SendPasswordReset` (app/user.go:1911) past its three refusals, which the
    /// handler has already applied: mint the token and mail it.
    ///
    /// A send failure is `api.user.send_password_reset.send.app_error` at **500** — the same id
    /// as the remote-user refusal's 400, told apart only by status.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn send_password_reset_to(&self, user: &User, site_url: &str) -> AppResult<bool> {
        let token = self
            .create_password_recovery_token(&user.id, &user.email)
            .await?;
        self.send_password_reset_email(&user.email, &token, &user.locale, site_url)
            .await
            .map_err(|err| {
                AppError::boxed(
                    "SendPasswordReset",
                    "api.user.send_password_reset.send.app_error",
                    None,
                    "",
                    500,
                )
                .wrap_boxed(err)
            })
    }

    /// Port of `App.SendEmailVerification` (app/user.go:2283).
    ///
    /// # Which e-mail is decided by the `Status` row
    ///
    /// A user with **no** status row — one who has never connected — gets the verify-your-address
    /// e-mail; one who has a row gets the email-*change* verification. `GetStatus` erroring with
    /// anything but a 404 aborts with that error, after the token was already written.
    #[tracing::instrument(skip_all, fields(user_id = %user.id))]
    pub async fn send_email_verification(
        &self,
        user: &User,
        new_email: &str,
        redirect: &str,
    ) -> AppResult {
        let token = self
            .create_verify_email_token(&user.id, new_email)
            .await
            .map_err(|err| {
                // `errors.Is(err, email.CreateEmailTokenError)` is the marshal failure; anything
                // else — the invalidation's AppError or the save — becomes
                // `app.recover.save.app_error`, the AppError itself wrapped.
                if err.id == "api.user.create_email_token.error" {
                    AppError::boxed(
                        "CreateVerifyEmailToken",
                        "api.user.create_email_token.error",
                        None,
                        "",
                        500,
                    )
                } else {
                    AppError::boxed(
                        "CreateVerifyEmailToken",
                        "app.recover.save.app_error",
                        None,
                        "",
                        500,
                    )
                    .wrap_boxed(*err)
                }
            })?;

        let site_url = self.live_site_url().await.unwrap_or_default();
        match self.get_status(&user.id).await {
            Err(err) if err.status_code != 404 => Err(err),
            Err(_) => self
                .send_verify_email(new_email, &user.locale, &site_url, &token.token, redirect)
                .await
                .map_err(|err| {
                    AppError::boxed(
                        "SendVerifyEmail",
                        "api.user.send_verify_email_and_forget.failed.error",
                        None,
                        "",
                        500,
                    )
                    .wrap_boxed(err)
                }),
            Ok(_) => self
                .send_email_change_verify_email(new_email, &user.locale, &site_url, &token.token)
                .await
                .map_err(|err| {
                    AppError::boxed(
                        "sendEmailChangeVerifyEmail",
                        "api.user.send_email_change_verify_email_and_forget.error",
                        None,
                        "",
                        500,
                    )
                    .wrap_boxed(err)
                }),
        }
    }

    /// Port of `App.TestEmail` (app/admin.go:173) past `checkHasNilFields` and the permission,
    /// which the handler applies.
    ///
    /// # The body's settings are checked, and then the **live** ones are used
    ///
    /// `cfg` is the configuration the administrator is editing. Go refuses it for an empty
    /// `SMTPServer`, swaps a `FakeSetting` password for the stored one when the server, port and
    /// user match the live configuration — and then sends with `a.Srv().MailServiceConfig()`,
    /// which reads the **live** configuration and never looks at `cfg` again. So the test sends
    /// through the settings already saved, not the ones on screen. That is Go's behaviour and is
    /// ported as such.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn test_email(
        &self,
        user_id: &str,
        cfg: &mm_model::config::EmailSettings,
    ) -> AppResult {
        if setting(&cfg.smtp_server).is_empty() {
            let detail = match crate::i18n::translations().await {
                Some(bundle) => {
                    let params: Params = [(
                        "Name".to_owned(),
                        serde_json::Value::String("SMTPServer".to_owned()),
                    )]
                    .into_iter()
                    .collect();
                    let config = crate::config::load_model_config(self.store().config())
                        .await
                        .ok();
                    let default = config
                        .as_ref()
                        .and_then(|c| c.localization_settings.default_server_locale.as_deref())
                        .unwrap_or("en");
                    bundle.translate_with(
                        bundle.server_locale(default),
                        "api.context.invalid_param.app_error",
                        Some(&params),
                    )
                }
                None => "api.context.invalid_param.app_error".to_owned(),
            };
            return Err(AppError::boxed(
                "testEmail",
                "api.admin.test_email.missing_server",
                None,
                detail,
                400,
            ));
        }

        let ctx = self.email_context().await.map_err(internal_error)?;
        let live = &ctx.config.email_settings;
        if setting(&cfg.smtp_password) == mm_model::utils::FAKE_SETTING
            && !(cfg.smtp_server == live.smtp_server
                && cfg.smtp_port == live.smtp_port
                && cfg.smtp_username == live.smtp_username)
        {
            return Err(AppError::boxed(
                "testEmail",
                "api.admin.test_email.reenter_password",
                None,
                "",
                400,
            ));
        }
        // The swapped-in password is written into `cfg`, which nothing reads afterwards.

        let user = self.get_user(user_id).await?;
        let t = user_translations(&user.locale)
            .await
            .map_err(internal_error)?;
        let mail_config = Self::mail_service_config(&ctx, "");
        crate::mail::send_mail_using_config(
            &user.email,
            &t.t("api.admin.test_email.subject"),
            &t.t("api.admin.test_email.body"),
            &mail_config,
            "",
            "",
            "",
            "",
            "",
        )
        .await
        .map_err(|err| {
            let params = [(
                "Error".to_owned(),
                serde_json::Value::String(err.to_string()),
            )]
            .into_iter()
            .collect();
            AppError::boxed(
                "testEmail",
                "app.admin.test_email.failure",
                Some(params),
                "",
                500,
            )
        })
    }
}

/// The `SiteName` placeholder in a [`App::send_notice`] param list, replaced by the live value.
const SITE_NAME: serde_json::Value = serde_json::Value::Null;

/// Replace every [`SITE_NAME`] placeholder with `TeamSettings.SiteName`.
fn with_site_name<'a>(
    params: &[(&'a str, serde_json::Value)],
    ctx: &SendContext,
) -> Vec<(&'a str, serde_json::Value)> {
    params
        .iter()
        .map(|(key, value)| {
            let value = if value.is_null() {
                ctx.site_name_value()
            } else {
                value.clone()
            };
            (*key, value)
        })
        .collect()
}

/// The `Token().Save` error mapping both creators share: the store's `AppError` as itself,
/// anything else as `app.recover.save.app_error`.
fn save_error(where_: &str, err: StoreError) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            AppError::boxed(where_, "app.recover.save.app_error", None, "", 500).wrap_boxed(other)
        }
    }
}

/// A failure to read the configuration or the translations in the middle of an app method.
fn internal_error(err: EmailError) -> Box<AppError> {
    AppError::boxed("testEmail", "app.admin.test_email.failure", None, "", 500).wrap_boxed(err)
}

/// `Box<AppError>` has no `wrap` of its own.
trait WrapBoxed {
    fn wrap_boxed(self, err: impl std::error::Error + Send + Sync + 'static) -> Self;
}

impl WrapBoxed for Box<AppError> {
    fn wrap_boxed(self, err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Box::new((*self).wrap(err))
    }
}
