//! TEMPORARY STUB — replaced by the real port of platform/shared/mail on merge. Not committed.
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
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MailError(pub String);
#[allow(clippy::too_many_arguments)]
pub async fn send_mail_using_config(
    _to: &str,
    _subject: &str,
    _html_body: &str,
    _config: &SmtpConfig,
    _message_id: &str,
    _in_reply_to: &str,
    _references: &str,
    _cc: &str,
    _category: &str,
) -> Result<(), MailError> {
    Err(MailError("stub".into()))
}
