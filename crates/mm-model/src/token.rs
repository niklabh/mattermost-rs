//! Port of `model/token.go` — the one-shot tokens behind password recovery, email verification,
//! invitations and magic links.

use crate::utils::{AppError, AppResult, get_millis, new_random_string};

/// Port of `model.TokenSize` (token.go:8) — **characters**, not bytes of entropy.
pub const TOKEN_SIZE: usize = 64;
/// Port of `model.MaxTokenExipryTime` (token.go:9). 48 hours. Typo in the Go name preserved in
/// the doc, not in the Rust name.
pub const MAX_TOKEN_EXPIRY_TIME: i64 = 1000 * 60 * 60 * 48;
/// Port of `model.PasswordRecoverExpiryTime` (token.go:10). 24 hours.
pub const PASSWORD_RECOVER_EXPIRY_TIME: i64 = 1000 * 60 * 60 * 24;
/// Port of `model.InvitationExpiryTime` (token.go:11). 48 hours.
pub const INVITATION_EXPIRY_TIME: i64 = 1000 * 60 * 60 * 48;
/// Port of `model.MagicLinkExpiryTime` (token.go:12). 5 minutes.
pub const MAGIC_LINK_EXPIRY_TIME: i64 = 1000 * 60 * 5;

pub const TOKEN_TYPE_PASSWORD_RECOVERY: &str = "password_recovery";
pub const TOKEN_TYPE_VERIFY_EMAIL: &str = "verify_email";
pub const TOKEN_TYPE_TEAM_INVITATION: &str = "team_invitation";
pub const TOKEN_TYPE_GUEST_INVITATION: &str = "guest_invitation";
/// Note the value is `cws_access_token`, not `cws_access`.
pub const TOKEN_TYPE_CWS_ACCESS: &str = "cws_access_token";
pub const TOKEN_TYPE_GUEST_MAGIC_LINK_INVITATION: &str = "guest_magic_link_invitation";
pub const TOKEN_TYPE_GUEST_MAGIC_LINK: &str = "guest_magic_link";
pub const TOKEN_TYPE_OAUTH: &str = "oauth";
pub const TOKEN_TYPE_SAML: &str = "saml";
/// The only token type spelled with hyphens.
pub const TOKEN_TYPE_SSO_CODE_EXCHANGE: &str = "sso-code-exchange";

/// Port of `model.Token` (token.go:29). **No `json:` tags** — a token is a stored row and is
/// never marshalled as a whole; only the `Token` string itself travels, in a URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Token {
    pub token: String,
    /// Epoch milliseconds.
    pub create_at: i64,
    pub type_: String,
    /// Type-dependent payload — an email address, a team id, a serialised invite.
    pub extra: String,
}

impl Token {
    /// Port of `model.NewToken` (token.go:36).
    pub fn new(token_type: impl Into<String>, extra: impl Into<String>) -> Self {
        Self {
            token: new_random_string(TOKEN_SIZE),
            create_at: get_millis(),
            type_: token_type.into(),
            extra: extra.into(),
        }
    }

    /// Port of `(*Token).IsValid` (token.go:45).
    ///
    /// Both branches are **500**, not 400: a malformed token is the server's own doing, since
    /// only the server mints them. Note the error ids have no `.app_error` suffix, unlike almost
    /// every other validator in the package — `model.token.is_valid.size` and
    /// `model.token.is_valid.expiry`. The second id says "expiry" while the branch tests
    /// `CreateAt`.
    pub fn is_valid(&self) -> AppResult {
        if self.token.len() != TOKEN_SIZE {
            return Err(err("size"));
        }

        if self.create_at == 0 {
            return Err(err("expiry"));
        }

        Ok(())
    }

    /// Port of `(*Token).IsExpired` (token.go:59).
    ///
    /// The default lifetime is [`MAX_TOKEN_EXPIRY_TIME`], which applies to **every unlisted
    /// type** — including `oauth`, `saml` and `sso-code-exchange`, which get 48 hours by falling
    /// through rather than by decision.
    ///
    /// Go's nil receiver returns `true`; on `&self` that state is unrepresentable, so a caller
    /// holding an `Option<Token>` maps `None` to `true` itself.
    pub fn is_expired(&self) -> bool {
        let expiry_time = match self.type_.as_str() {
            TOKEN_TYPE_GUEST_MAGIC_LINK => MAGIC_LINK_EXPIRY_TIME,
            TOKEN_TYPE_GUEST_MAGIC_LINK_INVITATION => INVITATION_EXPIRY_TIME,
            TOKEN_TYPE_PASSWORD_RECOVERY => PASSWORD_RECOVER_EXPIRY_TIME,
            // Email verification borrows the password-recovery window, not its own.
            TOKEN_TYPE_VERIFY_EMAIL => PASSWORD_RECOVER_EXPIRY_TIME,
            TOKEN_TYPE_TEAM_INVITATION => INVITATION_EXPIRY_TIME,
            TOKEN_TYPE_GUEST_INVITATION => INVITATION_EXPIRY_TIME,
            _ => MAX_TOKEN_EXPIRY_TIME,
        };
        get_millis() > (self.create_at + expiry_time)
    }

    /// Port of `(*Token).IsGuestMagicLink` (token.go:78).
    pub fn is_guest_magic_link(&self) -> bool {
        self.type_ == TOKEN_TYPE_GUEST_MAGIC_LINK
            || self.type_ == TOKEN_TYPE_GUEST_MAGIC_LINK_INVITATION
    }

    /// Port of `(*Token).IsInvitationToken` (token.go:82).
    ///
    /// Three types, and `guest_magic_link` is **not** one of them even though
    /// `guest_magic_link_invitation` is.
    pub fn is_invitation_token(&self) -> bool {
        self.type_ == TOKEN_TYPE_TEAM_INVITATION
            || self.type_ == TOKEN_TYPE_GUEST_INVITATION
            || self.type_ == TOKEN_TYPE_GUEST_MAGIC_LINK_INVITATION
    }
}

fn err(suffix: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Token.IsValid",
        format!("model.token.is_valid.{suffix}"),
        None,
        "",
        500,
    ))
}
