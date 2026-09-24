//! Port of `platform/shared/mail/mail.go` — Mattermost's outbound mail: the SMTP connection and
//! client set-up (`ConnectToSMTPServer`, `NewSMTPClient`, `TestConnection`), the auth chooser and
//! `loginAuth`, and `sendMail`, the call sequence over go-mail that builds every message.
//!
//! The generic machinery (net/mail, net/smtp, mime, go-mail's writer) is `gomail`; what is here
//! is Mattermost's own code, statement for statement. Every error's `Display` is Go's
//! `err.Error()`, pkg/errors wrapping included, because `TestConnection`'s error reaches the
//! wire inside `app.admin.test_email.failure`.
//!
//! # Things a reader would get wrong
//!
//! - **No QUIT after a send.** `sendMailUsingConfigAdvanced` defers `c.Quit()` and then
//!   `c.Close()`, which run in reverse: the connection is closed first and the QUIT is written to
//!   a closed socket. `TestConnection` calls `Close` then `Quit` in that order too. The server
//!   sees the connection drop. The only QUIT Go sends is `Auth`'s own, after a failed exchange.
//! - **`ServerTimeout` bounds the greeting only**, and a zero (or negative) timeout makes every
//!   connection fail with `context deadline exceeded` — `context.WithTimeout(0)` is already done.
//!   The dial uses the same seconds, where zero means no timeout.
//! - **STARTTLS's error is ignored.** A refused STARTTLS continues in plaintext; a failed
//!   handshake leaves a broken connection whose next command reports the TLS error.
//! - **PLAIN over plaintext always fails.** Go's `PlainAuth` exempts only a server *name* of
//!   exactly `localhost`/`127.0.0.1`/`::1`, and the name here is always `ServerName:Port`.
//! - **Embedded files** arrive in Go as a map and are embedded in map order, which is random; the
//!   caller's slice order is used here.
//!
//! # Randomness and the clock
//!
//! The message id (`model.NewRandomString(16)` and the Unix time), the date, and the multipart
//! boundaries are the only non-deterministic inputs. [`build_message`] takes the text part, the
//! date and the generated id as parameters, and rendering takes a [`WriteEnv`], so the tests
//! assert exact bytes against Go's.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use gomail::msg::{AddrHeader, Msg, SystemEnv, TYPE_TEXT_HTML, TYPE_TEXT_PLAIN, WriteEnv};
use gomail::netmail::{self, Address};
use gomail::smtp::{self, AuthError, Client, Conn, PlainAuth, ServerInfo, TlsConfig};

/// `mail.TLS` (mail.go:27).
pub const TLS: &str = "TLS";
/// `mail.StartTLS` (mail.go:28).
pub const STARTTLS: &str = "STARTTLS";
/// `mail.SendGridXSMTPAPIHeader` (mail.go:301).
pub const SEND_GRID_X_SMTP_API_HEADER: &str = "X-SMTPAPI";

/// Port of `mail.SMTPConfig` (mail.go:30).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpConfig {
    pub connection_security: String,
    pub skip_server_certificate_verification: bool,
    pub hostname: String,
    pub server_name: String,
    pub server: String,
    pub port: String,
    /// Seconds.
    pub server_timeout: i64,
    pub username: String,
    pub password: String,
    pub enable_smtp_auth: bool,
    pub send_email_notifications: bool,
    pub feedback_name: String,
    pub feedback_email: String,
    pub reply_to_address: String,
}

/// A failure of the mail path, with Go's text.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MailError {
    /// `errors.Wrap(err, context)`: `"{context}: {err}"`.
    #[error("{context}: {source}")]
    Wrapped {
        context: Cow<'static, str>,
        #[source]
        source: Box<MailError>,
    },
    #[error(transparent)]
    Smtp(#[from] smtp::Error),
    #[error(transparent)]
    Dial(#[from] smtp::DialError),
    #[error(transparent)]
    DialTls(#[from] smtp::DialTlsError),
    #[error(transparent)]
    Parse(#[from] netmail::ParseError),
    #[error(transparent)]
    SetAddress(#[from] gomail::msg::AddrError),
    #[error(transparent)]
    Render(#[from] gomail::msg::WriteError),
    /// `validateSingleAddress`'s count check.
    #[error("must contain exactly one address, got {0}")]
    AddressCount(usize),
    /// The greeting did not arrive within `ServerTimeout`.
    #[error("context deadline exceeded")]
    DeadlineExceeded,
}

impl MailError {
    fn wrap(self, context: &'static str) -> Self {
        MailError::Wrapped {
            context: Cow::Borrowed(context),
            source: Box::new(self),
        }
    }
}

trait WrapExt<T> {
    fn wrap(self, context: &'static str) -> Result<T, MailError>;
}

impl<T, E: Into<MailError>> WrapExt<T> for Result<T, E> {
    fn wrap(self, context: &'static str) -> Result<T, MailError> {
        self.map_err(|e| e.into().wrap(context))
    }
}

/// Why [`LoginAuth`] refused a challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoginAuthError {
    #[error("Unknown fromServer")]
    UnknownFromServer,
}

/// Port of `loginAuth` (mail.go:82): AUTH LOGIN, which Go's `net/smtp` lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAuth {
    pub username: String,
    pub password: String,
    pub host: String,
}

impl smtp::Auth for LoginAuth {
    /// `Start` (mail.go:90): TLS required — with no localhost exemption — and the server name
    /// must equal `host`.
    fn start(&mut self, server: &ServerInfo) -> Result<(String, Vec<u8>), smtp::Error> {
        if !server.tls {
            return Err(AuthError::UnencryptedConnection.into());
        }
        if server.name != self.host {
            return Err(AuthError::WrongHostName.into());
        }
        Ok(("LOGIN".to_owned(), Vec::new()))
    }

    /// `Next` (mail.go:102).
    fn next(&mut self, from_server: &[u8], more: bool) -> Result<Option<Vec<u8>>, smtp::Error> {
        if !more {
            return Ok(None);
        }
        match from_server {
            b"Username:" => Ok(Some(self.username.as_bytes().to_vec())),
            b"Password:" => Ok(Some(self.password.as_bytes().to_vec())),
            _ => Err(AuthError::Other(Arc::new(LoginAuthError::UnknownFromServer)).into()),
        }
    }
}

enum Chosen {
    None,
    Login(LoginAuth),
    Plain(PlainAuth),
}

/// Port of `authChooser` (mail.go:70): LOGIN, unless the server advertises PLAIN.
struct AuthChooser<'a> {
    config: &'a SmtpConfig,
    chosen: Chosen,
}

impl smtp::Auth for AuthChooser<'_> {
    fn start(&mut self, server: &ServerInfo) -> Result<(String, Vec<u8>), smtp::Error> {
        let smtp_address = format!("{}:{}", self.config.server_name, self.config.port);
        if server.auth.iter().any(|m| m == "PLAIN") {
            let mut plain = PlainAuth {
                identity: String::new(),
                username: self.config.username.clone(),
                password: self.config.password.clone(),
                host: smtp_address,
            };
            let started = plain.start(server);
            self.chosen = Chosen::Plain(plain);
            started
        } else {
            let mut login = LoginAuth {
                username: self.config.username.clone(),
                password: self.config.password.clone(),
                host: smtp_address,
            };
            let started = login.start(server);
            self.chosen = Chosen::Login(login);
            started
        }
    }

    fn next(&mut self, from_server: &[u8], more: bool) -> Result<Option<Vec<u8>>, smtp::Error> {
        match &mut self.chosen {
            Chosen::Login(a) => a.next(from_server, more),
            Chosen::Plain(a) => a.next(from_server, more),
            Chosen::None => Ok(None),
        }
    }
}

/// `time.Duration(config.ServerTimeout) * time.Second` as a `net.Dialer.Timeout`: zero is none,
/// and a negative one has already expired.
fn dial_timeout(config: &SmtpConfig) -> Option<Duration> {
    match config.server_timeout {
        0 => None,
        t if t < 0 => Some(Duration::ZERO),
        t => Some(Duration::from_secs(t.unsigned_abs())),
    }
}

/// Port of `ConnectToSMTPServerAdvanced` / `ConnectToSMTPServer` (mail.go:117, :148).
#[tracing::instrument(skip_all, fields(server = %config.server, port = %config.port))]
pub async fn connect_to_smtp_server(config: &SmtpConfig) -> Result<Conn, MailError> {
    let smtp_address = format!("{}:{}", config.server, config.port);
    let timeout = dial_timeout(config);
    if config.connection_security == TLS {
        let tls = TlsConfig {
            server_name: config.server_name.clone(),
            insecure_skip_verify: config.skip_server_certificate_verification,
        };
        smtp::dial_tls(&smtp_address, timeout, &tls)
            .await
            .wrap("unable to connect to the SMTP server through TLS")
    } else {
        smtp::dial(&smtp_address, timeout)
            .await
            .wrap("unable to connect to the SMTP server")
    }
}

/// Port of `NewSMTPClientAdvanced` / `NewSMTPClient` (mail.go:152, :198): the greeting within
/// `ServerTimeout`, HELLO with `Hostname`, STARTTLS (its error ignored), then AUTH.
#[tracing::instrument(skip_all)]
pub async fn new_smtp_client(conn: Conn, config: &SmtpConfig) -> Result<Client, MailError> {
    let host = format!("{}:{}", config.server_name, config.port);
    if config.server_timeout <= 0 {
        // `context.WithTimeout(ctx, 0)` is done before the greeting can be read.
        drop(conn);
        return Err(MailError::DeadlineExceeded.wrap("unable to connect to the SMTP server"));
    }
    let greeting = Client::new(conn, &host);
    let mut c = match tokio::time::timeout(
        Duration::from_secs(config.server_timeout.unsigned_abs()),
        greeting,
    )
    .await
    {
        Ok(result) => result.wrap("unable to connect to the SMTP server")?,
        Err(_) => {
            return Err(MailError::DeadlineExceeded.wrap("unable to connect to the SMTP server"));
        }
    };

    if !config.hostname.is_empty() {
        c.hello(&config.hostname)
            .await
            .wrap("unable to send hello message")?;
    }

    if config.connection_security == STARTTLS {
        let tls = TlsConfig {
            server_name: config.server_name.clone(),
            insecure_skip_verify: config.skip_server_certificate_verification,
        };
        // Go discards StartTLS's error.
        let _ = c.start_tls(&tls).await;
    }

    if config.enable_smtp_auth {
        let mut chooser = AuthChooser {
            config,
            chosen: Chosen::None,
        };
        c.auth(&mut chooser).await.wrap("authentication failed")?;
    }
    Ok(c)
}

/// Port of `TestConnection` (mail.go:207). Unlike sending, an empty `Server` is dialled.
#[tracing::instrument(skip_all)]
pub async fn test_connection(config: &SmtpConfig) -> Result<(), MailError> {
    let conn = connect_to_smtp_server(config)
        .await
        .wrap("unable to connect")?;
    let mut c = new_smtp_client(conn, config)
        .await
        .wrap("unable to connect")?;
    // `c.Close(); c.Quit()` — the QUIT goes to a closed connection.
    c.close();
    Ok(())
}

/// Port of `mailData` (mail.go:47), without `mimeHeaders`, which no caller sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailData {
    pub mime_to: String,
    pub smtp_to: String,
    pub from: Address,
    pub cc: String,
    pub reply_to: Address,
    pub subject: String,
    pub html_body: String,
    pub embedded_files: Vec<(String, Vec<u8>)>,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub category: String,
}

impl MailData {
    /// What `SendMailWithEmbeddedFilesUsingConfig` (mail.go:229) assembles.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        to: &str,
        subject: &str,
        html_body: &str,
        embedded_files: &[(String, Vec<u8>)],
        config: &SmtpConfig,
        message_id: &str,
        in_reply_to: &str,
        references: &str,
        cc: &str,
        category: &str,
    ) -> Self {
        Self {
            mime_to: to.to_owned(),
            smtp_to: to.to_owned(),
            from: Address {
                name: config.feedback_name.clone(),
                address: config.feedback_email.clone(),
            },
            cc: cc.to_owned(),
            reply_to: Address {
                name: config.feedback_name.clone(),
                address: config.reply_to_address.clone(),
            },
            subject: subject.to_owned(),
            html_body: html_body.to_owned(),
            embedded_files: embedded_files.to_vec(),
            message_id: message_id.to_owned(),
            in_reply_to: in_reply_to.to_owned(),
            references: references.to_owned(),
            category: category.to_owned(),
        }
    }
}

/// Port of `SendMailWithEmbeddedFilesUsingConfig` (mail.go:229). Go's unused
/// `enableComplianceFeatures` parameter is dropped.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, fields(to = %to))]
pub async fn send_mail_with_embedded_files_using_config(
    to: &str,
    subject: &str,
    html_body: &str,
    embedded: &[(String, Vec<u8>)],
    config: &SmtpConfig,
    message_id: &str,
    in_reply_to: &str,
    references: &str,
    cc: &str,
    category: &str,
) -> Result<(), MailError> {
    let mail = MailData::new(
        to,
        subject,
        html_body,
        embedded,
        config,
        message_id,
        in_reply_to,
        references,
        cc,
        category,
    );
    let mut env = SystemEnv {
        type_by_extension: crate::mime::type_by_extension,
    };
    send_mail_using_config_advanced(&mail, config, &html_to_text, &mut env).await
}

/// Port of `SendMailUsingConfig` (mail.go:250).
#[allow(clippy::too_many_arguments)]
pub async fn send_mail_using_config(
    to: &str,
    subject: &str,
    html_body: &str,
    config: &SmtpConfig,
    message_id: &str,
    in_reply_to: &str,
    references: &str,
    cc: &str,
    category: &str,
) -> Result<(), MailError> {
    send_mail_with_embedded_files_using_config(
        to,
        subject,
        html_body,
        &[],
        config,
        message_id,
        in_reply_to,
        references,
        cc,
        category,
    )
    .await
}

/// `html2text.FromString` as `sendMail` uses it: a failure is logged and the text part is empty.
fn html_to_text(html: &str) -> String {
    match gohtml2text::from_string(html) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(err = %err, "Unable to convert html body to text");
            String::new()
        }
    }
}

/// Port of `sendMailUsingConfigAdvanced` (mail.go:255): nothing at all when `Server` is empty.
pub async fn send_mail_using_config_advanced(
    mail: &MailData,
    config: &SmtpConfig,
    to_text: &(dyn Fn(&str) -> String + Sync),
    env: &mut (dyn WriteEnv + Send),
) -> Result<(), MailError> {
    if config.server.is_empty() {
        return Ok(());
    }
    let conn = connect_to_smtp_server(config).await?;
    let mut c = new_smtp_client(conn, config).await?;
    let date = chrono::Local::now().fixed_offset();
    let result = send_mail(&mut c, mail, date, config, to_text, env).await;
    // `defer c.Quit(); defer c.Close()` — Close runs first, so no QUIT reaches the server.
    c.close();
    result
}

/// Port of `generateMessageID` (mail.go:303).
pub fn generate_message_id(hostname: &str) -> String {
    format!(
        "<{}-{}@{hostname}>",
        mm_model::utils::new_random_string(16),
        chrono::Utc::now().timestamp()
    )
}

/// Port of `validateSingleAddress` (mail.go:313).
pub fn validate_single_address(value: &str) -> Result<(), MailError> {
    let addresses = netmail::parse_address_list(value).wrap("failed to parse address")?;
    if addresses.len() != 1 {
        return Err(MailError::AddressCount(addresses.len()));
    }
    Ok(())
}

/// The message half of `sendMail` (mail.go:324–386): every header and part, in Go's order,
/// with the text part, the date and the fallback message id supplied. Validation errors are
/// Go's, wrapped as Go wraps them.
pub fn build_message(
    mail: &MailData,
    config: &SmtpConfig,
    text: &str,
    date: DateTime<FixedOffset>,
    generate_id: impl FnOnce(&str) -> String,
) -> Result<Msg, MailError> {
    let mut m = Msg::new();
    m.set_addr_header_from_mail_address(AddrHeader::From, std::slice::from_ref(&mail.from));
    validate_single_address(&mail.mime_to).wrap("invalid To header")?;
    m.set_addr_header(AddrHeader::To, &[&mail.mime_to])
        .wrap("failed to set To address")?;
    m.set_gen_header("Subject", &[&mail.subject]);
    m.set_gen_header("Content-Transfer-Encoding", &["8bit"]);
    m.set_gen_header("Auto-Submitted", &["auto-generated"]);
    m.set_gen_header("Precedence", &["bulk"]);

    if !mail.category.is_empty() {
        let value = format!(
            "{{\"category\": {}}}",
            mm_model::utils::go_quote(&mail.category)
        );
        m.set_gen_header(SEND_GRID_X_SMTP_API_HEADER, &[&value]);
    }

    if !mail.reply_to.address.is_empty() {
        m.set_addr_header_from_mail_address(
            AddrHeader::ReplyTo,
            std::slice::from_ref(&mail.reply_to),
        );
    }

    if !mail.cc.is_empty() {
        validate_single_address(&mail.cc).wrap("invalid Cc header")?;
        m.set_addr_header(AddrHeader::Cc, &[&mail.cc])
            .wrap("failed to set Cc address")?;
    }

    let msg_id = if mail.message_id.is_empty() {
        generate_id(&config.hostname)
    } else {
        mail.message_id.clone()
    };
    // SetGenHeader, not the preformatted form: only a generic Message-ID suppresses go-mail's own.
    m.set_gen_header("Message-ID", &[&msg_id]);

    if !mail.in_reply_to.is_empty() {
        m.set_gen_header_preformatted("In-Reply-To", &mail.in_reply_to);
    }
    if !mail.references.is_empty() {
        m.set_gen_header_preformatted("References", &mail.references);
    }

    m.set_date_with_value(date);
    m.set_body_string(TYPE_TEXT_PLAIN, text);
    m.add_alternative_string(TYPE_TEXT_HTML, &mail.html_body);
    for (name, content) in &mail.embedded_files {
        m.embed_reader(name, content.clone());
    }
    Ok(m)
}

/// Port of `sendMail` (mail.go:324).
pub async fn send_mail(
    c: &mut Client,
    mail: &MailData,
    date: DateTime<FixedOffset>,
    config: &SmtpConfig,
    to_text: &(dyn Fn(&str) -> String + Sync),
    env: &mut (dyn WriteEnv + Send),
) -> Result<(), MailError> {
    tracing::info!(to = %mail.smtp_to, "sending mail");
    let text = to_text(&mail.html_body);
    let mut m = build_message(mail, config, &text, date, generate_message_id)?;

    c.mail(&mail.from.address)
        .await
        .wrap("failed to set the from address")?;
    c.rcpt(&mail.smtp_to)
        .await
        .wrap("failed to set the to address")?;
    let mut w = c.data().await.wrap("failed to add email message data")?;
    let bytes = m.write_to(env).wrap("failed to write the email message")?;
    w.write(&bytes)
        .await
        .wrap("failed to write the email message")?;
    w.close()
        .await
        .wrap("failed to close connection to the SMTP server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gomail::testing::{SinkScript, normalise_transcript, server_tls_config, spawn_sink};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_mail.json")).unwrap()
    }

    fn s(v: &serde_json::Value, k: &str) -> String {
        v[k].as_str().unwrap_or("").to_owned()
    }

    fn embedded(v: &serde_json::Value) -> Vec<(String, Vec<u8>)> {
        v["embedded"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| {
                (
                    s(e, "name"),
                    gomail::base64::decode(&s(e, "content_base64")).unwrap(),
                )
            })
            .collect()
    }

    /// The oracle's environment: Go's boundaries in the order drawn, and `mm_app::mime` for
    /// `TypeByExtension` (the embeds are `.png`, `.jpg`, `.gif` and extensionless).
    struct FixedEnv(std::collections::VecDeque<String>);

    impl WriteEnv for FixedEnv {
        fn random_boundary(&mut self) -> String {
            self.0.pop_front().expect("Go drew as many boundaries")
        }
        fn type_by_extension(&self, ext: &str) -> String {
            crate::mime::type_by_extension(ext)
        }
        fn now(&self) -> DateTime<FixedOffset> {
            unreachable!("sendMail always sets Date")
        }
        fn random_bytes(&mut self, _: &mut [u8]) {
            unreachable!("sendMail always sets Message-ID")
        }
        fn hostname(&self) -> Option<String> {
            None
        }
    }

    /// The deterministic core against Go's bytes: every message in the oracle's corpus, built
    /// by [`build_message`] with Go's html2text output, date, message id and boundaries.
    #[test]
    fn build_message_matches_go_bytes() {
        let oracle = oracle();
        let rows = oracle["messages"].as_array().unwrap();
        assert!(rows.len() >= 15);
        for row in rows {
            let input = &row["input"];
            let name = s(input, "name");
            let config = SmtpConfig {
                feedback_name: s(input, "feedback_name"),
                feedback_email: s(input, "feedback_email"),
                reply_to_address: s(input, "reply_to_address"),
                hostname: "unused.example".into(),
                ..Default::default()
            };
            let mail = MailData::new(
                &s(input, "to"),
                &s(input, "subject"),
                &s(input, "html_body"),
                &embedded(input),
                &config,
                &s(input, "message_id"),
                &s(input, "in_reply_to"),
                &s(input, "references"),
                &s(input, "cc"),
                &s(input, "category"),
            );
            let offset = FixedOffset::east_opt(
                i32::try_from(input["date_offset_seconds"].as_i64().unwrap()).unwrap(),
            )
            .unwrap();
            let date = DateTime::from_timestamp(input["date_unix"].as_i64().unwrap(), 0)
                .unwrap()
                .with_timezone(&offset);
            let mut m = build_message(&mail, &config, &s(row, "text"), date, |_| {
                unreachable!("every corpus row has a message id")
            })
            .unwrap();
            let mut env = FixedEnv(
                row["boundaries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_str().unwrap().to_owned())
                    .collect(),
            );
            let got = String::from_utf8(m.write_to(&mut env).unwrap()).unwrap();
            assert!(env.0.is_empty(), "{name}: Go drew more boundaries");
            assert_eq!(got, s(row, "output"), "{name}");
        }
    }

    fn config_from(v: &serde_json::Value, port: &str) -> SmtpConfig {
        SmtpConfig {
            connection_security: s(v, "connection_security"),
            skip_server_certificate_verification: v["skip_server_certificate_verification"]
                .as_bool()
                .unwrap(),
            hostname: s(v, "hostname"),
            server_name: s(v, "server_name"),
            server: s(v, "server"),
            port: port.to_owned(),
            server_timeout: v["server_timeout"].as_i64().unwrap(),
            username: s(v, "username"),
            password: s(v, "password"),
            enable_smtp_auth: v["enable_smtp_auth"].as_bool().unwrap(),
            send_email_notifications: true,
            feedback_name: s(v, "feedback_name"),
            feedback_email: s(v, "feedback_email"),
            reply_to_address: s(v, "reply_to_address"),
        }
    }

    fn script(v: &serde_json::Value) -> SinkScript {
        SinkScript {
            tls: v["tls"].as_bool().unwrap_or(false),
            greeting: s(v, "greeting"),
            replies: v["replies"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            auth_replies: v["auth_replies"]
                .as_array()
                .map(|a| a.iter().map(|r| r.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default(),
            close_on: s(v, "close_on"),
        }
    }

    /// The real thing: every SMTP scenario the oracle drove through Go's
    /// `SendMailUsingConfig` / `SendMailWithEmbeddedFilesUsingConfig` / `TestConnection`,
    /// replayed through this module against a Rust sink running the same script. The error text
    /// and the whole client transcript (normalised by the documented rule) must be Go's. The
    /// text part is Go's html2text output, injected until `gohtml2text` lands.
    #[tokio::test]
    async fn smtp_scenarios_match_go() {
        let oracle = oracle();
        let tls = server_tls_config(&s(&oracle, "sink_cert_pem"), &s(&oracle, "sink_key_pem"));
        let rows = oracle["smtp"].as_array().unwrap();
        assert!(rows.len() >= 70);
        let mut checked = 0;
        for row in rows {
            let name = s(row, "name");
            if row["host_dependent"] == true {
                continue;
            }
            let (port, sink) = if row["no_sink"] == true {
                (s(&row["config"], "port"), None)
            } else {
                let (port, handle) = spawn_sink(script(&row["script"]), tls.clone()).await;
                (port.to_string(), Some(handle))
            };
            let config = config_from(&row["config"], &port);
            let args = &row["args"];
            let text = s(row, "html2text");
            let result = match s(row, "call").as_str() {
                "test" => test_connection(&config).await,
                call => {
                    let embedded = if call == "send_embedded" {
                        embedded(args)
                    } else {
                        Vec::new()
                    };
                    let mail = MailData::new(
                        &s(args, "to"),
                        &s(args, "subject"),
                        &s(args, "html_body"),
                        &embedded,
                        &config,
                        &s(args, "message_id"),
                        &s(args, "in_reply_to"),
                        &s(args, "references"),
                        &s(args, "cc"),
                        &s(args, "category"),
                    );
                    let mut env = SystemEnv {
                        type_by_extension: crate::mime::type_by_extension,
                    };
                    send_mail_using_config_advanced(&mail, &config, &|_| text.clone(), &mut env)
                        .await
                }
            };
            assert_eq!(
                result.err().map(|e| e.to_string()),
                row["error"].as_str().map(str::to_owned),
                "{name}: error"
            );
            let transcript = match sink {
                Some(handle) => tokio::time::timeout(Duration::from_secs(10), handle)
                    .await
                    .unwrap_or_else(|_| panic!("{name}: sink never finished"))
                    .unwrap(),
                None => String::new(),
            };
            assert_eq!(
                normalise_transcript(&transcript),
                s(row, "transcript"),
                "{name}: transcript"
            );
            checked += 1;
        }
        assert!(checked >= 70, "{checked}");
    }

    /// The two host-dependent rows: Go's resolver answer on the machine that generated the
    /// fixture. Checked only for shape here, since the name server and `/etc/hosts` differ.
    #[tokio::test]
    async fn host_dependent_dial_errors_have_gos_shape() {
        let config = SmtpConfig {
            server: "nonexistent.invalid".into(),
            server_name: "nonexistent.invalid".into(),
            port: "25".into(),
            server_timeout: 5,
            ..Default::default()
        };
        let err = test_connection(&config).await.unwrap_err().to_string();
        assert!(
            err.starts_with(
                "unable to connect: unable to connect to the SMTP server: dial tcp: lookup nonexistent.invalid"
            ),
            "{err}"
        );
        let config = SmtpConfig {
            server: "localhost".into(),
            port: "1".into(),
            server_timeout: 5,
            ..config
        };
        let err = test_connection(&config).await.unwrap_err().to_string();
        assert!(err.ends_with(":1: connect: connection refused"), "{err}");
    }

    #[test]
    fn login_auth_branches() {
        use smtp::Auth as _;
        let mut a = LoginAuth {
            username: "u".into(),
            password: "p".into(),
            host: "h:25".into(),
        };
        let info = |name: &str, tls| ServerInfo {
            name: name.into(),
            tls,
            auth: vec![],
        };
        assert_eq!(
            a.start(&info("h:25", false)).unwrap_err().to_string(),
            "unencrypted connection"
        );
        // No localhost exemption, unlike PlainAuth.
        let mut local = LoginAuth {
            host: "localhost".into(),
            ..a.clone()
        };
        assert_eq!(
            local
                .start(&info("localhost", false))
                .unwrap_err()
                .to_string(),
            "unencrypted connection"
        );
        assert_eq!(
            a.start(&info("x:25", true)).unwrap_err().to_string(),
            "wrong host name"
        );
        assert_eq!(
            a.start(&info("h:25", true)).unwrap(),
            ("LOGIN".to_owned(), vec![])
        );
        assert_eq!(a.next(b"Username:", true).unwrap(), Some(b"u".to_vec()));
        assert_eq!(a.next(b"Password:", true).unwrap(), Some(b"p".to_vec()));
        assert_eq!(a.next(b"Username:", false).unwrap(), None);
        assert_eq!(
            a.next(b"username:", true).unwrap_err().to_string(),
            "Unknown fromServer"
        );
    }

    #[test]
    fn auth_chooser_prefers_plain_only_when_advertised() {
        use smtp::Auth as _;
        let config = SmtpConfig {
            server_name: "mail.example".into(),
            port: "587".into(),
            username: "u".into(),
            password: "p".into(),
            ..Default::default()
        };
        let mut chooser = AuthChooser {
            config: &config,
            chosen: Chosen::None,
        };
        let info = |auth: &[&str]| ServerInfo {
            name: "mail.example:587".into(),
            tls: true,
            auth: auth.iter().map(|s| (*s).to_owned()).collect(),
        };
        assert_eq!(
            chooser.start(&info(&["LOGIN", "PLAIN"])).unwrap().0,
            "PLAIN"
        );
        assert_eq!(chooser.start(&info(&["LOGIN"])).unwrap().0, "LOGIN");
        assert_eq!(chooser.start(&info(&["plain"])).unwrap().0, "LOGIN");
        assert_eq!(chooser.start(&info(&[])).unwrap().0, "LOGIN");
    }

    #[test]
    fn generated_message_id_shape() {
        let id = generate_message_id("chat.example.com");
        let normalised = normalise_transcript(&id);
        assert_eq!(normalised, "<RANDOM-UNIX@chat.example.com>", "{id}");
    }

    #[test]
    fn validate_single_address_texts() {
        let e = |v: &str| validate_single_address(v).unwrap_err().to_string();
        assert!(validate_single_address("a@b").is_ok());
        assert!(validate_single_address("g: a@b;").is_ok());
        assert_eq!(e("a@b, c@d"), "must contain exactly one address, got 2");
        assert_eq!(e("g:;"), "must contain exactly one address, got 0");
        assert_eq!(
            e("x"),
            "failed to parse address: mail: missing '@' or angle-addr"
        );
    }

    /// With html2text still a stub on this branch, the public wrapper's text part is empty and
    /// a warning is logged — Go's own failure branch. An empty server sends nothing at all.
    #[tokio::test]
    async fn public_wrappers_short_circuit_on_empty_server() {
        let config = SmtpConfig::default();
        assert!(
            send_mail_using_config("a@b", "s", "<p>x</p>", &config, "", "", "", "", "")
                .await
                .is_ok()
        );
        assert!(
            send_mail_with_embedded_files_using_config(
                "not an address",
                "s",
                "",
                &[("a.png".into(), vec![1])],
                &config,
                "",
                "",
                "",
                "",
                ""
            )
            .await
            .is_ok()
        );
    }
}
