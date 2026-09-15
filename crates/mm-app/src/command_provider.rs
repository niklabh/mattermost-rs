//! The built-in slash-command registry of `app/slashcommands/` — as far as `GetCommand` goes —
//! and the two app functions that read it without running a provider: `ListAutocompleteCommands`
//! (app/command.go:100) and the dispatch half of `ExecuteCommand` (app/command.go:219).
//!
//! # What is here and what is not
//!
//! Go registers 35 `CommandProvider`s in `init()` functions. Each has a `GetCommand` — the
//! definition clients autocomplete from — and a `DoCommand` that does the work. **Every
//! `GetCommand` is ported** ([`provider_command`]); **no `DoCommand` is**: a request that would
//! run a provider is the handler's to forward, and so is one that matches a custom (webhook)
//! command, whose `DoCommandRequest` calls out over HTTP and posts the answer. Forwarded by
//! trigger, with the reason, in `crates/mm-api/src/commands.rs`.
//!
//! # The strings are English, and only English is served
//!
//! `GetCommand` translates through `c.AppContext.T`, which web/handlers.go:191 builds from the
//! request's **`Accept-Language`** alone — not the user's locale. This server has no i18n bundle
//! ([D-092]), so [`EN`] carries exactly the strings the 35 `GetCommand`s read, transcribed by a
//! script from `i18n/en.json` and checked against it by a unit test, and the handlers serve only
//! a request that [`request_translation_locale`] resolves to `en`. Every other locale forwards.
//!
//! # Go's list order is not an order
//!
//! `ListAutocompleteCommands` ranges over `commandProviders`, a Go **map**, so the built-ins come
//! back in a different order on every call of the same Go process (measured: three consecutive
//! reads, three orders). [`BUILTIN_TRIGGERS`] fixes one; any fixed order is an order Go can emit.

use mm_model::command::Command;
use mm_model::command_autocomplete::AutocompleteData;
use mm_model::utils::{AppError, AppResult, go_to_lower};
use mm_store::command_store::CommandStore;
use mm_store::user_store::UserStore;

use crate::App;
use crate::config::Config;

pub(crate) const EN: &[(&str, &str)] = &[
    ("api.command_away.desc", "Set your status away"),
    ("api.command_away.name", "away"),
    ("api.command_channel_header.desc", "Edit the channel header"),
    ("api.command_channel_header.hint", "[text]"),
    ("api.command_channel_header.name", "header"),
    (
        "api.command_channel_purpose.desc",
        "Edit the channel purpose",
    ),
    ("api.command_channel_purpose.hint", "[text]"),
    ("api.command_channel_purpose.name", "purpose"),
    ("api.command_channel_rename.desc", "Rename the channel"),
    ("api.command_channel_rename.hint", "[text]"),
    ("api.command_channel_rename.name", "rename"),
    ("api.command_code.desc", "Display text as a code block"),
    ("api.command_code.hint", "[text]"),
    ("api.command_code.name", "code"),
    ("api.command_custom_status.desc", "Set or clear your status"),
    (
        "api.command_custom_status.hint",
        "[:emoji_name:] [status_message] or clear",
    ),
    ("api.command_custom_status.name", "status"),
    (
        "api.command_dnd.desc",
        "Do not disturb disables desktop and mobile push notifications.",
    ),
    ("api.command_dnd.name", "dnd"),
    ("api.command_echo.desc", "Echo back text from your account"),
    ("api.command_echo.hint", "'message' [delay in seconds]"),
    ("api.command_echo.name", "echo"),
    (
        "api.command_expand.desc",
        "Turn off auto-collapsing of image previews",
    ),
    ("api.command_expand.name", "expand"),
    (
        "api.command_collapse.desc",
        "Turn on auto-collapsing of image previews",
    ),
    ("api.command_collapse.name", "collapse"),
    (
        "api.command_groupmsg.desc",
        "Sends a Group Message to the specified users",
    ),
    (
        "api.command_groupmsg.hint",
        "@[username1],@[username2] 'message'",
    ),
    ("api.command_groupmsg.name", "message"),
    ("api.command_help.desc", "Show Mattermost help message"),
    ("api.command_help.name", "help"),
    ("api.command_invite.desc", "Invite a user to a channel"),
    ("api.command_invite.hint", "@[username]... ~[channel]..."),
    ("api.command_invite.name", "invite"),
    ("api.command_join.desc", "Join the open channel"),
    ("api.command_join.hint", "~[channel]"),
    ("api.command_join.name", "join"),
    ("api.command_leave.desc", "Leave the current channel"),
    ("api.command_leave.name", "leave"),
    ("api.command_logout.desc", "Logout of Mattermost"),
    ("api.command_logout.name", "logout"),
    ("api.command_marketplace.desc", "Open the Marketplace"),
    ("api.command_marketplace.name", "marketplace"),
    ("api.command_me.desc", "Do an action"),
    ("api.command_me.hint", "[message]"),
    ("api.command_me.name", "me"),
    (
        "api.command_mobile_logs.desc",
        "Manage mobile app log attachment for yourself or another user.",
    ),
    (
        "api.command_mobile_logs.hint",
        "[on|off|status] [@username]",
    ),
    ("api.command_mobile_logs.name", "mobile-logs"),
    ("api.command_msg.desc", "Send Direct Message to a user"),
    ("api.command_msg.hint", "@[username] 'message'"),
    ("api.command_msg.name", "message"),
    (
        "api.command_mute.desc",
        "Turns off desktop, email and push notifications for the current channel or the [channel] specified.",
    ),
    ("api.command_mute.hint", "~[channel]"),
    ("api.command_mute.name", "mute"),
    ("api.command_offline.desc", "Set your status offline"),
    ("api.command_offline.name", "offline"),
    ("api.command_online.desc", "Set your status online"),
    ("api.command_online.name", "online"),
    (
        "api.command_remove.desc",
        "Remove a member from the channel",
    ),
    ("api.command_remove.hint", "@[username]"),
    ("api.command_remove.name", "remove"),
    ("api.command_search.desc", "Search text in messages"),
    ("api.command_search.hint", "[text]"),
    ("api.command_search.name", "search"),
    ("api.command_settings.desc", "Open the Settings dialog"),
    ("api.command_settings.name", "settings"),
    (
        "api.command_shortcuts.desc",
        "Displays a list of keyboard shortcuts",
    ),
    ("api.command_shortcuts.name", "shortcuts"),
    ("api.command_shrug.desc", "Adds ¯\\_(ツ)_/¯ to your message"),
    ("api.command_shrug.hint", "[message]"),
    ("api.command_shrug.name", "shrug"),
    (
        "api.command_remote.desc",
        "Invite secure connections for communication across Mattermost instances.",
    ),
    ("api.command_remote.hint", "[action]"),
    ("api.command_remote.name", "secure-connection"),
    (
        "api.command_share.desc",
        "Shares the current channel with an external Mattermost instance.",
    ),
    ("api.command_share.hint", "[action]"),
    ("api.command_share.name", "share-channel"),
    ("api.command_kick.name", "kick"),
    ("api.command_open.name", "open"),
    (
        "api.command.invite_people.desc",
        "Send an email invite to your Mattermost team",
    ),
    ("api.command.invite_people.hint", "[name@domain.com ...]"),
    ("api.command.invite_people.name", "invite_people"),
    (
        "api.command_remote.remote_add_remove.help",
        "Add/remove secure connections. Available actions: {{.Actions}}",
    ),
    (
        "api.command_remote.invite.help",
        "Invite a secure connection",
    ),
    ("api.command_remote.name.help", "Secure connection name"),
    (
        "api.command_remote.name.hint",
        "A unique name for the secure connection",
    ),
    (
        "api.command_remote.displayname.help",
        "Secure connection display name",
    ),
    (
        "api.command_remote.displayname.hint",
        "A display name for the secure connection",
    ),
    (
        "api.command_remote.invite_password.help",
        "Invitation password",
    ),
    (
        "api.command_remote.invite_password.hint",
        "Password to be used to encrypt the invitation",
    ),
    (
        "api.command_remote.accept.help",
        "Accept an invitation from an external Mattermost instance",
    ),
    (
        "api.command_remote.invitation.help",
        "Invitation from secure connection",
    ),
    (
        "api.command_remote.invitation.hint",
        "The encrypted invitation from a secure connection",
    ),
    (
        "api.command_remote.remove.help",
        "Removes a secure connection",
    ),
    (
        "api.command_remote.remove_remote_id.help",
        "ID of secure connection to remove.",
    ),
    (
        "api.command_remote.status.help",
        "Displays status for all secure connections",
    ),
    (
        "api.command_share.available_actions",
        "Available actions: {{.Actions}}",
    ),
    (
        "api.command_share.invite_remote.help",
        "Invites an external Mattermost instance to the current shared channel",
    ),
    (
        "api.command_share.remote_id.help",
        "ID of an existing secure connection. See `secure-connection` command to add a secure connection.",
    ),
    (
        "api.command_share.share_read_only.help",
        "Channel will be shared in read-only mode",
    ),
    (
        "api.command_share.share_read_only.hint",
        "[readonly] - 'Y' or 'N'.  Defaults to 'N'",
    ),
    (
        "api.command_share.uninvite_remote.help",
        "Uninvites a secure connection from this shared channel",
    ),
    (
        "api.command_share.uninvite_remote_id.help",
        "ID of secure connection to uninvite.",
    ),
    (
        "api.command_share.unshare_channel.help",
        "Unshares the current channel",
    ),
    (
        "api.command_share.channel_status.help",
        "Displays status for this shared channel",
    ),
];

/// `T(id)` on an English request: the translation, or the id itself when en.json has none —
/// which is what go-i18n returns for a missing id.
fn t(id: &'static str) -> &'static str {
    EN.iter()
        .find(|(key, _)| *key == id)
        .map_or(id, |(_, value)| value)
}

/// `T(id, map[string]any{"Actions": actions})` — the one template variable any `GetCommand` uses.
fn t_actions(id: &'static str, actions: &str) -> String {
    t(id).replace("{{.Actions}}", actions)
}

/// `slashcommands.AvailableRemoteActions` (command_remote.go:19).
const AVAILABLE_REMOTE_ACTIONS: &str = "create, accept, remove, status";
/// `slashcommands.AvailableShareActions` (command_share.go:25).
const AVAILABLE_SHARE_ACTIONS: &str = "invite, uninvite, unshare, status";

/// Every registered provider's trigger — the keys of Go's `commandProviders` map, in the fixed
/// order this server lists them. `exportlink` and `test` are registered but their `GetCommand`
/// is nil on a stock server, so they never reach a list.
pub const BUILTIN_TRIGGERS: &[&str] = &[
    "away",
    "header",
    "purpose",
    "rename",
    "code",
    "status",
    "dnd",
    "echo",
    "expand",
    "collapse",
    "exportlink",
    "groupmsg",
    "help",
    "invite",
    "invite_people",
    "join",
    "leave",
    "test",
    "logout",
    "marketplace",
    "me",
    "mobile-logs",
    "msg",
    "mute",
    "offline",
    "online",
    "open",
    "secure-connection",
    "remove",
    "kick",
    "search",
    "settings",
    "shortcuts",
    "share-channel",
    "shrug",
];

/// `app.CmdCustomStatusTrigger` (app/command.go:28).
pub const CMD_CUSTOM_STATUS_TRIGGER: &str = "status";

/// What `GetCommandProvider(trigger).GetCommand(a, T)` gives.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderCommand {
    /// No provider registered under this trigger.
    Unregistered,
    /// Registered, and `GetCommand` returned nil — `tryExecuteBuiltInCommand` treats that as no
    /// command, and the list skips it.
    Nil,
    /// `GetCommand` depends on something this server cannot evaluate. Only `/exportlink` with
    /// `DedicatedExportStore` on, where it asks the export backend whether it can generate links.
    Undecidable,
    Command(Box<Command>),
}

/// A built-in command definition: `Command{Trigger, AutoComplete, AutoCompleteDesc,
/// AutoCompleteHint, DisplayName}` with every other field zero.
fn builtin(
    trigger: &str,
    auto_complete: bool,
    desc: &str,
    hint: &str,
    display_name: &str,
) -> Command {
    Command {
        trigger: trigger.to_owned(),
        auto_complete,
        auto_complete_desc: desc.to_owned(),
        auto_complete_hint: hint.to_owned(),
        display_name: display_name.to_owned(),
        ..Command::default()
    }
}

/// Port of `(*RemoteProvider).GetCommand` (slashcommands/command_remote.go:40).
fn remote_command() -> Command {
    let mut remote = AutocompleteData::new(
        "secure-connection",
        "[action]",
        t_actions(
            "api.command_remote.remote_add_remove.help",
            AVAILABLE_REMOTE_ACTIONS,
        ),
    );
    let name_help = t("api.command_remote.name.help");
    let name_hint = t("api.command_remote.name.hint");
    let display_help = t("api.command_remote.displayname.help");
    let display_hint = t("api.command_remote.displayname.hint");
    let password_help = t("api.command_remote.invite_password.help");
    let password_hint = t("api.command_remote.invite_password.hint");

    let mut create = AutocompleteData::new("create", "", t("api.command_remote.invite.help"));
    create.add_named_text_argument("name", name_help, name_hint, "", true);
    create.add_named_text_argument("displayname", display_help, display_hint, "", false);
    create.add_named_text_argument("password", password_help, password_hint, "", true);

    let mut accept = AutocompleteData::new("accept", "", t("api.command_remote.accept.help"));
    accept.add_named_text_argument("name", name_help, name_hint, "", true);
    accept.add_named_text_argument("displayname", display_help, display_hint, "", false);
    accept.add_named_text_argument("password", password_help, password_hint, "", true);
    accept.add_named_text_argument(
        "invite",
        t("api.command_remote.invitation.help"),
        t("api.command_remote.invitation.hint"),
        "",
        true,
    );

    let mut remove = AutocompleteData::new("remove", "", t("api.command_remote.remove.help"));
    remove.add_named_dynamic_list_argument(
        "connectionID",
        t("api.command_remote.remove_remote_id.help"),
        "builtin:secure-connection",
        true,
    );

    let status = AutocompleteData::new("status", "", t("api.command_remote.status.help"));

    remote.add_command(create);
    remote.add_command(accept);
    remote.add_command(remove);
    remote.add_command(status);

    Command {
        autocomplete_data: Some(remote),
        ..builtin(
            "secure-connection",
            true,
            t("api.command_remote.desc"),
            t("api.command_remote.hint"),
            t("api.command_remote.name"),
        )
    }
}

/// Port of `(*ShareProvider).GetCommand` (slashcommands/command_share.go:36).
fn share_command() -> Command {
    let mut share = AutocompleteData::new(
        "share-channel",
        "[action]",
        t_actions(
            "api.command_share.available_actions",
            AVAILABLE_SHARE_ACTIONS,
        ),
    );

    let mut invite = AutocompleteData::new("invite", "", t("api.command_share.invite_remote.help"));
    invite.add_named_dynamic_list_argument(
        "connectionID",
        t("api.command_share.remote_id.help"),
        "builtin:share-channel",
        true,
    );
    invite.add_named_text_argument(
        "readonly",
        t("api.command_share.share_read_only.help"),
        t("api.command_share.share_read_only.hint"),
        "Y|N|y|n",
        false,
    );

    let mut uninvite =
        AutocompleteData::new("uninvite", "", t("api.command_share.uninvite_remote.help"));
    uninvite.add_named_dynamic_list_argument(
        "connectionID",
        t("api.command_share.uninvite_remote_id.help"),
        "builtin:share-channel",
        true,
    );

    let unshare = AutocompleteData::new("unshare", "", t("api.command_share.unshare_channel.help"));
    let status = AutocompleteData::new("status", "", t("api.command_share.channel_status.help"));

    share.add_command(invite);
    share.add_command(uninvite);
    share.add_command(unshare);
    share.add_command(status);

    Command {
        autocomplete_data: Some(share),
        ..builtin(
            "share-channel",
            true,
            t("api.command_share.desc"),
            t("api.command_share.hint"),
            t("api.command_share.name"),
        )
    }
}

/// Port of `GetCommandProvider(trigger)` followed by its `GetCommand(a, T)`, for an English
/// request — the registry of `app/slashcommands/`, one arm per provider file.
pub fn provider_command(config: &Config, trigger: &str) -> ProviderCommand {
    let join = || {
        builtin(
            "join",
            true,
            t("api.command_join.desc"),
            t("api.command_join.hint"),
            t("api.command_join.name"),
        )
    };
    let command = match trigger {
        "away" => builtin(
            "away",
            true,
            t("api.command_away.desc"),
            "",
            t("api.command_away.name"),
        ),
        "header" => builtin(
            "header",
            true,
            t("api.command_channel_header.desc"),
            t("api.command_channel_header.hint"),
            t("api.command_channel_header.name"),
        ),
        "purpose" => builtin(
            "purpose",
            true,
            t("api.command_channel_purpose.desc"),
            t("api.command_channel_purpose.hint"),
            t("api.command_channel_purpose.name"),
        ),
        // command_channel_rename.go:28 — the only one of the three channel-property commands
        // with autocomplete data, and its argument's *help text* is the hint string.
        "rename" => {
            let hint = t("api.command_channel_rename.hint");
            let desc = t("api.command_channel_rename.desc");
            let mut data = AutocompleteData::new("rename", hint, desc);
            data.add_text_argument(hint, "[text]", "");
            Command {
                autocomplete_data: Some(data),
                ..builtin(
                    "rename",
                    true,
                    desc,
                    hint,
                    t("api.command_channel_rename.name"),
                )
            }
        }
        "code" => builtin(
            "code",
            true,
            t("api.command_code.desc"),
            t("api.command_code.hint"),
            t("api.command_code.name"),
        ),
        "status" => builtin(
            "status",
            true,
            t("api.command_custom_status.desc"),
            t("api.command_custom_status.hint"),
            t("api.command_custom_status.name"),
        ),
        "dnd" => builtin(
            "dnd",
            true,
            t("api.command_dnd.desc"),
            "",
            t("api.command_dnd.name"),
        ),
        "echo" => builtin(
            "echo",
            true,
            t("api.command_echo.desc"),
            t("api.command_echo.hint"),
            t("api.command_echo.name"),
        ),
        "expand" => builtin(
            "expand",
            true,
            t("api.command_expand.desc"),
            "",
            t("api.command_expand.name"),
        ),
        "collapse" => builtin(
            "collapse",
            true,
            t("api.command_collapse.desc"),
            "",
            t("api.command_collapse.name"),
        ),
        // command_exportlink.go:35 — nil unless the direct-download flag, a dedicated export
        // store and a link-generating backend all hold. The first is the only one readable here
        // on a stock server, and it is enough to say nil.
        "exportlink" => {
            if !config.dedicated_export_store {
                return ProviderCommand::Nil;
            }
            return ProviderCommand::Undecidable;
        }
        "groupmsg" => builtin(
            "groupmsg",
            true,
            t("api.command_groupmsg.desc"),
            t("api.command_groupmsg.hint"),
            t("api.command_groupmsg.name"),
        ),
        "help" => builtin(
            "help",
            true,
            t("api.command_help.desc"),
            "",
            t("api.command_help.name"),
        ),
        "invite" => builtin(
            "invite",
            true,
            t("api.command_invite.desc"),
            t("api.command_invite.hint"),
            t("api.command_invite.name"),
        ),
        // command_invite_people.go:31 — listed only when all three settings allow invites.
        "invite_people" => builtin(
            "invite_people",
            config.send_email_notifications
                && config.enable_user_creation
                && config.enable_email_invitations,
            t("api.command.invite_people.desc"),
            t("api.command.invite_people.hint"),
            t("api.command.invite_people.name"),
        ),
        "join" => join(),
        "leave" => builtin(
            "leave",
            true,
            t("api.command_leave.desc"),
            "",
            t("api.command_leave.name"),
        ),
        // command_loadtest.go:130 — nil unless `EnableTesting`, and never autocompleted.
        "test" => {
            if !config.enable_testing {
                return ProviderCommand::Nil;
            }
            builtin("test", false, "Debug Load Testing", "help", "test")
        }
        "logout" => builtin(
            "logout",
            true,
            t("api.command_logout.desc"),
            "",
            t("api.command_logout.name"),
        ),
        // command_marketplace.go:28 — autocompleted only when plugins and the marketplace are on.
        "marketplace" => builtin(
            "marketplace",
            config.plugin_enable && config.plugin_enable_marketplace,
            t("api.command_marketplace.desc"),
            "",
            t("api.command_marketplace.name"),
        ),
        "me" => builtin(
            "me",
            true,
            t("api.command_me.desc"),
            t("api.command_me.hint"),
            t("api.command_me.name"),
        ),
        "mobile-logs" => builtin(
            "mobile-logs",
            true,
            t("api.command_mobile_logs.desc"),
            t("api.command_mobile_logs.hint"),
            t("api.command_mobile_logs.name"),
        ),
        "msg" => builtin(
            "msg",
            true,
            t("api.command_msg.desc"),
            t("api.command_msg.hint"),
            t("api.command_msg.name"),
        ),
        "mute" => builtin(
            "mute",
            true,
            t("api.command_mute.desc"),
            t("api.command_mute.hint"),
            t("api.command_mute.name"),
        ),
        "offline" => builtin(
            "offline",
            true,
            t("api.command_offline.desc"),
            "",
            t("api.command_offline.name"),
        ),
        "online" => builtin(
            "online",
            true,
            t("api.command_online.desc"),
            "",
            t("api.command_online.name"),
        ),
        // command_open.go:28 — `/join`'s definition under another trigger and display name.
        "open" => Command {
            trigger: "open".to_owned(),
            display_name: t("api.command_open.name").to_owned(),
            ..join()
        },
        "secure-connection" => remote_command(),
        "remove" => builtin(
            "remove",
            true,
            t("api.command_remove.desc"),
            t("api.command_remove.hint"),
            t("api.command_remove.name"),
        ),
        // command_remove.go:49 — `/remove`'s description and hint under its own display name.
        "kick" => builtin(
            "kick",
            true,
            t("api.command_remove.desc"),
            t("api.command_remove.hint"),
            t("api.command_kick.name"),
        ),
        "search" => builtin(
            "search",
            true,
            t("api.command_search.desc"),
            t("api.command_search.hint"),
            t("api.command_search.name"),
        ),
        "settings" => builtin(
            "settings",
            true,
            t("api.command_settings.desc"),
            "",
            t("api.command_settings.name"),
        ),
        "shortcuts" => builtin(
            "shortcuts",
            true,
            t("api.command_shortcuts.desc"),
            "",
            t("api.command_shortcuts.name"),
        ),
        "share-channel" => share_command(),
        "shrug" => builtin(
            "shrug",
            true,
            t("api.command_shrug.desc"),
            t("api.command_shrug.hint"),
            t("api.command_shrug.name"),
        ),
        _ => return ProviderCommand::Unregistered,
    };
    ProviderCommand::Command(Box::new(command))
}

/// The locale `i18n.GetTranslationsAndLocaleFromRequest` (shared/i18n/i18n.go:264) builds the
/// request's translate function for — the first `Accept-Language` entry whole, then its part
/// before `-`, then the default client locale, then `en`.
///
/// Only the translate function's locale is ported, because only it decides the strings; Go's
/// second return value differs from it in one branch and nothing here reads that value.
pub fn request_translation_locale<'a>(
    accept_language: Option<&'a str>,
    default_client_locale: &'a str,
) -> &'a str {
    let full = accept_language
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default();
    let short = full.split('-').next().unwrap_or_default();
    if crate::i18n::is_supported_locale(full) {
        full
    } else if crate::i18n::is_supported_locale(short) {
        short
    } else if crate::i18n::is_supported_locale(default_client_locale) {
        default_client_locale
    } else {
        "en"
    }
}

/// Where `ExecuteCommand` would send a command, decided before anything runs.
#[derive(Debug)]
pub enum CommandDispatch {
    /// A custom (webhook) command of the team matches the trigger.
    Custom,
    /// A built-in provider with a non-nil `GetCommand` matches.
    BuiltIn,
    /// [`ProviderCommand::Undecidable`].
    Undecidable,
    /// Nothing matches: the 404 `api.command.execute_command.not_found.app_error`.
    NotFound(Box<AppError>),
}

/// `maxTriggerLen` (app/command.go:30).
const MAX_TRIGGER_LEN: usize = 512;

/// The trigger `ExecuteCommand` matches against (app/command.go:220-234): everything before the
/// first `unicode.IsSpace` rune, lower-cased by Go's rules, without its leading `/`. `None` for a
/// command that does not start with `/` — the 400 `format.app_error`, which `executeCommand`'s own
/// prefix check makes unreachable through the route.
pub fn command_trigger(command: &str) -> Option<String> {
    // Go's `unicode.IsSpace` is the Unicode `White_Space` property, which is what
    // `char::is_whitespace` tests.
    let head = command
        .find(char::is_whitespace)
        .map_or(command, |index| &command[..index]);
    go_to_lower(head).strip_prefix('/').map(str::to_owned)
}

impl App {
    /// Port of `App.ListAutocompleteCommands` (app/command.go:100) without its plugin half,
    /// which the handler has already established is empty.
    ///
    /// Precedence is by trigger, first come: `EnableCustomUserStatuses` off reserves `status`
    /// before anything is listed, then the team's custom commands — sanitised — then the
    /// built-ins. So a custom `/shrug` hides the built-in one. `Ok(None)` is
    /// [`ProviderCommand::Undecidable`], for the handler to forward.
    #[tracing::instrument(skip(self), fields(count))]
    pub async fn list_autocomplete_commands(
        &self,
        team_id: &str,
    ) -> AppResult<Option<Vec<Command>>> {
        let config = self.config();
        let mut commands: Vec<Command> = Vec::with_capacity(32);
        let mut seen = std::collections::HashSet::new();

        if !config.enable_custom_user_statuses {
            seen.insert(CMD_CUSTOM_STATUS_TRIGGER.to_owned());
        }

        if config.enable_commands {
            let team_commands =
                self.store()
                    .command()
                    .get_by_team(team_id)
                    .await
                    .map_err(|err| {
                        tracing::error!(error = %err, "team command listing failed");
                        AppError::boxed(
                            "ListAutocompleteCommands",
                            "app.command.listautocompletecommands.internal_error",
                            None,
                            String::new(),
                            500,
                        )
                    })?;
            for mut command in team_commands {
                if command.auto_complete && seen.insert(command.trigger.clone()) {
                    command.sanitize();
                    commands.push(command);
                }
            }
        }

        for trigger in BUILTIN_TRIGGERS {
            match provider_command(config, trigger) {
                ProviderCommand::Command(command) => {
                    if command.auto_complete && seen.insert(command.trigger.clone()) {
                        commands.push(*command);
                    }
                }
                ProviderCommand::Undecidable => return Ok(None),
                ProviderCommand::Nil | ProviderCommand::Unregistered => {}
            }
        }

        tracing::Span::current().record("count", commands.len());
        Ok(Some(commands))
    }

    /// The part of `ExecuteCommand` (app/command.go:219) that decides *where* a command goes,
    /// without running it: `tryExecuteCustomCommand`'s gate and lookups up to its trigger match
    /// (:403-485), then `tryExecuteBuiltInCommand`'s provider lookup (:387), then the 404.
    ///
    /// The lookups happen **before** the match and whatever the trigger — so a direct message
    /// naming a team that does not exist is the team 404 even for `/shrug` (measured). The
    /// channel lookup Go makes beside them is skipped: the handler read the same row a moment
    /// earlier, and `Get(id, true)` differs from its read only in the cache.
    #[tracing::instrument(skip(self), fields(dispatch))]
    pub async fn command_dispatch(
        &self,
        team_id: &str,
        user_id: &str,
        trigger: &str,
    ) -> AppResult<CommandDispatch> {
        const WHERE: &str = "tryExecuteCustomCommand";
        let config = self.config();
        if !config.enable_commands {
            return Err(AppError::boxed(
                "ExecuteCommand",
                "api.command.disabled.app_error",
                None,
                String::new(),
                501,
            ));
        }

        let team_commands = self
            .store()
            .command()
            .get_by_team(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "team command listing failed");
                AppError::boxed(
                    WHERE,
                    "app.command.tryexecutecustomcommand.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // `tr := <-teamChan` is read before `ur := <-userChan`, so a missing team wins.
        self.get_team(team_id).await?;
        self.store().user().get(user_id).await.map_err(|err| {
            if err.is_not_found() {
                AppError::boxed(
                    WHERE,
                    "app.user.missing_account.const",
                    None,
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "command user lookup failed");
                AppError::boxed(WHERE, "app.user.get.app_error", None, String::new(), 500)
            }
        })?;

        if team_commands
            .iter()
            .any(|command| command.trigger == trigger)
        {
            tracing::Span::current().record("dispatch", "custom");
            return Ok(CommandDispatch::Custom);
        }

        match provider_command(config, trigger) {
            ProviderCommand::Command(_) => {
                tracing::Span::current().record("dispatch", "built_in");
                Ok(CommandDispatch::BuiltIn)
            }
            ProviderCommand::Undecidable => Ok(CommandDispatch::Undecidable),
            ProviderCommand::Nil | ProviderCommand::Unregistered => {
                tracing::Span::current().record("dispatch", "not_found");
                Ok(CommandDispatch::NotFound(not_found(trigger)))
            }
        }
    }
}

/// The 404 of `ExecuteCommand` (app/command.go:266-270): the trigger, cut to 512 **bytes** with
/// `...` appended, goes into the message's `Trigger` parameter — which only the translated
/// message shows.
fn not_found(trigger: &str) -> Box<AppError> {
    let shown = if trigger.len() > MAX_TRIGGER_LEN {
        let mut cut = MAX_TRIGGER_LEN;
        while !trigger.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &trigger[..cut])
    } else {
        trigger.to_owned()
    };
    let params =
        std::collections::HashMap::from([("Trigger".to_owned(), serde_json::Value::String(shown))]);
    AppError::boxed(
        "command",
        "api.command.execute_command.not_found.app_error",
        Some(params),
        String::new(),
        404,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string in [`EN`] is en.json's, byte for byte — the table was generated from the file
    /// and this is what keeps it from drifting.
    #[test]
    fn the_english_table_is_en_json() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n/en.json");
        let raw = std::fs::read_to_string(path).unwrap();
        let entries: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        for (key, value) in EN {
            let found = entries
                .iter()
                .find(|entry| entry["id"] == *key)
                .unwrap_or_else(|| panic!("{key} is not in en.json"));
            assert_eq!(found["translation"], *value, "{key}");
        }
    }

    /// Every id a `GetCommand` reads is in the table — a missing one would silently be the id.
    #[test]
    fn every_registered_command_translates() {
        let config = Config::default();
        for trigger in BUILTIN_TRIGGERS {
            if let ProviderCommand::Command(command) = provider_command(&config, trigger) {
                for text in [
                    &command.auto_complete_desc,
                    &command.auto_complete_hint,
                    &command.display_name,
                ] {
                    assert!(!text.starts_with("api."), "{trigger}: {text}");
                }
            }
        }
        assert_eq!(
            provider_command(&config, "nope"),
            ProviderCommand::Unregistered
        );
    }

    /// The config-dependent providers: `exportlink` and `test` are nil on a stock server,
    /// `invite_people` and `marketplace` lose `AutoComplete` when any setting they read is off.
    #[test]
    fn the_conditional_providers_follow_their_settings() {
        let mut config = Config::default();
        assert_eq!(
            provider_command(&config, "exportlink"),
            ProviderCommand::Nil
        );
        assert_eq!(provider_command(&config, "test"), ProviderCommand::Nil);
        config.dedicated_export_store = true;
        assert_eq!(
            provider_command(&config, "exportlink"),
            ProviderCommand::Undecidable
        );
        config.enable_testing = true;
        let ProviderCommand::Command(test) = provider_command(&config, "test") else {
            panic!("test is registered")
        };
        assert!(!test.auto_complete);

        let auto = |config: &Config, trigger| match provider_command(config, trigger) {
            ProviderCommand::Command(command) => command.auto_complete,
            other => panic!("{trigger}: {other:?}"),
        };
        let config = Config {
            enable_email_invitations: true,
            ..Config::default()
        };
        assert!(auto(&config, "invite_people"));
        for off in 0..3 {
            let mut c = config.clone();
            match off {
                0 => c.send_email_notifications = false,
                1 => c.enable_user_creation = false,
                _ => c.enable_email_invitations = false,
            }
            assert!(!auto(&c, "invite_people"), "setting {off}");
        }
        assert!(auto(&config, "marketplace"));
        let mut c = config.clone();
        c.plugin_enable = false;
        assert!(!auto(&c, "marketplace"));
        let mut c = config.clone();
        c.plugin_enable_marketplace = false;
        assert!(!auto(&c, "marketplace"));
    }

    /// `/open` is `/join` renamed; `/kick` is `/remove` renamed.
    #[test]
    fn the_aliases_share_their_originals_text() {
        let config = Config::default();
        let get = |trigger| match provider_command(&config, trigger) {
            ProviderCommand::Command(command) => *command,
            other => panic!("{other:?}"),
        };
        let (open, join) = (get("open"), get("join"));
        assert_eq!(open.auto_complete_hint, join.auto_complete_hint);
        assert_eq!(open.display_name, "open");
        let (kick, remove) = (get("kick"), get("remove"));
        assert_eq!(kick.auto_complete_desc, remove.auto_complete_desc);
        assert_eq!(kick.display_name, "kick");
    }

    /// The locale ladder, one row per branch.
    #[test]
    fn the_request_locale_follows_gos_ladder() {
        assert_eq!(request_translation_locale(None, "en"), "en");
        assert_eq!(request_translation_locale(Some("de"), "en"), "de");
        assert_eq!(request_translation_locale(Some("en-US,de"), "en"), "en");
        assert_eq!(request_translation_locale(Some("en-AU"), "en"), "en-AU");
        assert_eq!(request_translation_locale(Some("xx-YY"), "fr"), "fr");
        assert_eq!(request_translation_locale(Some("xx"), "zz"), "en");
    }

    /// The trigger: cut at the first Unicode space, Go-lowercased, slash removed.
    #[test]
    fn the_trigger_is_the_lowered_first_word() {
        assert_eq!(command_trigger("/Shrug hello").as_deref(), Some("shrug"));
        assert_eq!(command_trigger("/echo\u{a0}x").as_deref(), Some("echo"));
        assert_eq!(command_trigger("/away").as_deref(), Some("away"));
        assert_eq!(command_trigger("away"), None);
    }

    /// Past 512 bytes the trigger is cut and `...` appended, on a character boundary.
    #[test]
    fn a_long_trigger_is_truncated_in_the_not_found_error() {
        let err = not_found(&"a".repeat(600));
        let shown = err.params.as_ref().unwrap()["Trigger"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(shown.len(), 515);
        assert!(shown.ends_with("..."));
        assert_eq!(err.status_code, 404);
        let short = not_found("abc");
        assert_eq!(short.params.as_ref().unwrap()["Trigger"], "abc");
    }
}
