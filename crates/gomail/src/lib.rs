//! Port of Mattermost's SMTP path below `platform/shared/mail` (SKELETON — API contract only).

/// Port of `mail.SMTPConfig` (platform/shared/mail/mail.go:30).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpConfig {
    pub connection_security: String,
    pub skip_server_certificate_verification: bool,
    pub hostname: String,
    pub server_name: String,
    pub server: String,
    pub port: String,
    pub server_timeout: i64,
    pub username: String,
    pub password: String,
    pub enable_smtp_auth: bool,
    pub send_email_notifications: bool,
    pub feedback_name: String,
    pub feedback_email: String,
    pub reply_to_address: String,
}

/// One file `SendMailWithEmbeddedFilesUsingConfig` embeds, in the order Go's map iteration would
/// visit it (the caller decides; Go's is random).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedFile {
    pub name: String,
    pub content: Vec<u8>,
}

/// The arguments of `SendMailWithEmbeddedFilesUsingConfig` (mail.go:229) after the config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub html_body: String,
    pub embedded_files: Vec<EmbeddedFile>,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub cc: String,
    pub category: String,
}

/// A failure whose `Display` is Go's `err.Error()` — it reaches the wire inside
/// `app.admin.test_email.failure`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MailError(pub String);

/// `SendMailWithEmbeddedFilesUsingConfig` / `SendMailUsingConfig` (mail.go:229, :250).
pub async fn send_mail_using_config(_mail: &Mail, _config: &SmtpConfig) -> Result<(), MailError> {
    Err(MailError("not implemented".into()))
}

/// `TestConnection` (mail.go:207).
pub async fn test_connection(_config: &SmtpConfig) -> Result<(), MailError> {
    Err(MailError("not implemented".into()))
}
