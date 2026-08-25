//! Port of `model/command_webhook.go` — the short-lived callback a slash command can post back to.

use crate::utils::{AppError, AppResult, get_millis, is_valid_id, new_id};

/// Port of `model.CommandWebhookLifetime` (command_webhook.go:18) — 30 minutes in milliseconds.
pub const COMMAND_WEBHOOK_LIFETIME: i64 = 1000 * 60 * 30;

/// Port of `model.CommandWebhook` (command_webhook.go:7).
///
/// **No `json:` tags** — the hook is addressed by its id in a URL and never marshalled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandWebhook {
    pub id: String,
    /// Epoch milliseconds.
    pub create_at: i64,
    pub command_id: String,
    pub user_id: String,
    pub channel_id: String,
    pub root_id: String,
    /// Incremented on each use; the server caps it, not this type.
    pub use_count: i64,
}

impl CommandWebhook {
    /// Port of `(*CommandWebhook).PreSave` (command_webhook.go:22).
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }
    }

    /// Port of `(*CommandWebhook).IsValid` (command_webhook.go:32).
    ///
    /// The error ids are `model.command_hook.<field>.app_error` — **`command_hook`**, not
    /// `command_webhook`, and there is no `is_valid` segment, unlike nearly every other validator
    /// in the package. Only the `create_at` branch carries details.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", format!("id={}", self.id)));
        }

        if !is_valid_id(&self.command_id) {
            return Err(err("command_id", String::new()));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", String::new()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", String::new()));
        }

        // Empty is allowed: a webhook that posts to the channel rather than into a thread.
        if !self.root_id.is_empty() && !is_valid_id(&self.root_id) {
            return Err(err("root_id", String::new()));
        }

        Ok(())
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "CommandWebhook.IsValid",
        format!("model.command_hook.{field}.app_error"),
        None,
        details,
        400,
    ))
}
