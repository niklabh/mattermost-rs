//! Port of app/plugin_commands.go — the slash commands plugins register, which live in the host's
//! memory and in no table — and of the part of app/command.go a plugin command's execution
//! reaches: `ExecuteCommand`'s trigger id (:219), `MentionsToTeamMembers` (:275),
//! `MentionsToPublicChannels` (:347), `HandleCommandResponse` (:619), `HandleCommandResponsePost`
//! (:653) and `CreateCommandPost` (:55), with app/slack.go's `ProcessSlackText` and
//! `ProcessMessageAttachments`.
//!
//! # The registry is Go's, quirks included
//!
//! [`PluginCommandRegistry`] is `Channels.pluginCommands`. Three rules a reader would guess
//! differently:
//!
//! - **Two plugins may register the same trigger.** There is no "already registered by another
//!   plugin" refusal: a second plugin's registration is *appended*, and execution takes the first
//!   match, so the earlier plugin keeps the trigger until it goes. Only the **same** plugin
//!   re-registering a trigger on the same team replaces its entry in place.
//! - **`UnregisterCommand` removes every plugin's entry** for that team and trigger, not just the
//!   caller's — `UnregisterPluginCommand` takes the plugin id and never reads it.
//! - **Only `DisablePlugin` and `removePluginLocally` drop a plugin's commands**
//!   (`unregisterPluginCommands`). A deactivation that comes from a `PluginStates` change, a crash
//!   or a shutdown leaves them registered; executing one then finds no running plugin and answers
//!   500 `model.plugin_command.error.app_error`.
//!
//! The registered command is a **copy** of eight fields: no `PluginId`, no `Username`, no
//! `IconURL`, and the trigger lower-cased — while the default autocomplete data keeps the trigger
//! as the plugin spelled it.
//!
//! # Execution runs the plugin first
//!
//! `ExecuteCommand` tries plugins before custom and built-in commands ("Plugins can override
//! built in and custom commands"), so a plugin that registers `shrug` answers `/shrug`. A plugin
//! that answers neither a response nor an error hands the command on to custom and built-in
//! commands, which [`ExecuteOutcome::NotPlugin`] leaves to the caller.
//!
//! # A response post this server cannot write
//!
//! The plugin answers before the post is made, so a response whose post `CreatePost` or
//! `SendEphemeralPost` refuses as unreproducible (an attachment, a non-default post type, a
//! username override, a channel with an outgoing webhook) cannot be forwarded: the plugin has
//! already run. It is answered as Go answers a post that failed —
//! `api.command.execute_command.create_post_failed.app_error` — and logged at error level; see
//! [D-1020].

use std::collections::HashMap;
use std::sync::RwLock;

use mm_model::channel::CHANNEL_TYPE_OPEN;
use mm_model::command::Command;
use mm_model::command_args::CommandArgs;
use mm_model::command_autocomplete::{
    AutocompleteArg, AutocompleteArgData, AutocompleteArgType, AutocompleteData,
    AutocompleteDynamicListArg, AutocompleteError, AutocompleteListItem, AutocompleteStaticListArg,
    AutocompleteTextArg,
};
use mm_model::command_response::{
    COMMAND_RESPONSE_TYPE_EPHEMERAL, COMMAND_RESPONSE_TYPE_IN_CHANNEL, CommandResponse,
};
use mm_model::message_attachment::{
    MessageAttachment, parse_message_attachment, parse_slack_links_to_markdown,
    stringify_message_attachment_field_value,
};
use mm_model::post::{
    POST_PROPS_FROM_WEBHOOK, POST_PROPS_OVERRIDE_ICON_URL, POST_PROPS_OVERRIDE_USERNAME,
    POST_SYSTEM_MESSAGE_PREFIX, Post,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, get_millis, go_to_lower, new_id};
use mm_plugin::wire::model as wire_model;
use mm_plugin::wire::plugin as wire_plugin;
use mm_plugin::wire::registered;
use mm_store::user_store::UserStore;

use crate::App;
use crate::plugin_hooks::{HookContext, props_from_wire, props_to_wire};
use crate::post::PrepareError;
use crate::post_create::CreatePostFlags;

/// Port of `PluginCommand` (app/plugin_commands.go:19).
#[derive(Debug, Clone, PartialEq)]
pub struct PluginCommand {
    pub command: Command,
    pub plugin_id: String,
}

/// Port of `Channels.pluginCommands` with its lock (app/channels.go:48).
#[derive(Debug, Default)]
pub struct PluginCommandRegistry {
    commands: RwLock<Vec<PluginCommand>>,
}

/// Why `RegisterPluginCommand` refused, as the plugin reads it: each crosses as an
/// `ErrorString` holding this text (`encodableError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterCommandError {
    /// `errors.New("invalid command")`: an empty trigger.
    #[error("invalid command")]
    InvalidCommand,
    /// `errors.Wrap(err, "invalid autocomplete data in command")`.
    #[error("invalid autocomplete data in command: {0}")]
    InvalidAutocompleteData(AutocompleteError),
    /// `errors.Wrapf(err, "Can't parse url %s", "/plugins/"+pluginID)`.
    #[error("Can't parse url /plugins/{0}: invalid URL")]
    BaseUrl(String),
    /// `errors.Wrap(err, "Can't update relative urls for plugin commands")`.
    #[error("Can't update relative urls for plugin commands: {0}")]
    RelativeUrls(AutocompleteError),
}

impl PluginCommandRegistry {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<PluginCommand>> {
        self.commands
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<PluginCommand>> {
        self.commands
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Port of `App.RegisterPluginCommand` (app/plugin_commands.go:24). See the module docs for
    /// what is copied and the collision rule.
    pub fn register(
        &self,
        plugin_id: &str,
        mut command: Command,
    ) -> Result<(), RegisterCommandError> {
        if command.trigger.is_empty() {
            return Err(RegisterCommandError::InvalidCommand);
        }
        if let Some(data) = command.autocomplete_data.as_ref() {
            data.is_valid()
                .map_err(RegisterCommandError::InvalidAutocompleteData)?;
        }
        match command.autocomplete_data.as_mut() {
            None => {
                // The trigger as the plugin spelled it: only the command's is lower-cased below.
                command.autocomplete_data = Some(AutocompleteData::new(
                    &command.trigger,
                    &command.auto_complete_hint,
                    &command.auto_complete_desc,
                ));
            }
            Some(data) => {
                let base = format!("/plugins/{plugin_id}");
                let base_url = mm_model::go_url::go_parse(&base)
                    .map_err(|_| RegisterCommandError::BaseUrl(plugin_id.to_owned()))?;
                data.update_relative_urls_for_plugin_commands(&base_url)
                    .map_err(RegisterCommandError::RelativeUrls)?;
            }
        }

        let command = Command {
            trigger: go_to_lower(&command.trigger),
            team_id: command.team_id,
            auto_complete: command.auto_complete,
            auto_complete_desc: command.auto_complete_desc,
            auto_complete_hint: command.auto_complete_hint,
            display_name: command.display_name,
            autocomplete_data: command.autocomplete_data,
            autocomplete_icon_data: command.autocomplete_icon_data,
            ..Command::default()
        };

        let mut commands = self.write();
        for registered in commands.iter_mut() {
            if registered.command.trigger == command.trigger
                && registered.command.team_id == command.team_id
                && registered.plugin_id == plugin_id
            {
                registered.command = command;
                return Ok(());
            }
        }
        commands.push(PluginCommand {
            command,
            plugin_id: plugin_id.to_owned(),
        });
        Ok(())
    }

    /// Port of `App.UnregisterPluginCommand` (app/plugin_commands.go:79): **every** plugin's entry
    /// for the team and the lower-cased trigger. Go takes the plugin id and does not read it.
    pub fn unregister(&self, team_id: &str, trigger: &str) {
        let trigger = go_to_lower(trigger);
        self.write()
            .retain(|pc| pc.command.team_id != team_id || pc.command.trigger != trigger);
    }

    /// Port of `Channels.unregisterPluginCommands` (app/plugin_commands.go:92): every command
    /// `plugin_id` registered.
    pub fn unregister_plugin(&self, plugin_id: &str) {
        self.write().retain(|pc| pc.plugin_id != plugin_id);
    }

    /// Port of `App.CommandsForTeam` (app/plugin_commands.go:106): the commands registered for
    /// every team and those registered for `team_id`, in registration order.
    pub fn for_team(&self, team_id: &str) -> Vec<Command> {
        self.read()
            .iter()
            .filter(|pc| pc.command.team_id.is_empty() || pc.command.team_id == team_id)
            .map(|pc| pc.command.clone())
            .collect()
    }

    /// The match `tryExecutePluginCommand` takes (app/plugin_commands.go:125): the **first**
    /// registration, in order, for the trigger on this team or every team.
    pub fn matching(&self, team_id: &str, trigger: &str) -> Option<PluginCommand> {
        self.read()
            .iter()
            .find(|pc| {
                (pc.command.team_id.is_empty() || pc.command.team_id == team_id)
                    && pc.command.trigger == trigger
            })
            .cloned()
    }
}

/// The trigger `tryExecutePluginCommand` matches (app/plugin_commands.go:122): the command split
/// on **spaces** only — not `ExecuteCommand`'s `unicode.IsSpace` — with its first byte dropped
/// and lower-cased. So `/cmd\targ` looks for a plugin trigger `cmd\targ`.
pub fn plugin_trigger(command: &str) -> String {
    let head = command.split(' ').next().unwrap_or_default();
    go_to_lower(head.get(1..).unwrap_or_default())
}

// -------------------------------------------------------------------------------------------
// Execution
// -------------------------------------------------------------------------------------------

/// What executing a command through the plugin half of `ExecuteCommand` decided.
#[derive(Debug)]
pub enum ExecuteOutcome {
    /// Go's answer: the response, or the error — with the response beside it when
    /// `HandleCommandResponse` failed, since Go returns both.
    Answered(Result<Box<CommandResponse>, ExecuteError>),
    /// No plugin took the command — none registered it, plugins are off, or the plugin answered
    /// neither a response nor an error: custom and built-in commands come next.
    NotPlugin,
}

/// An `ExecuteCommand` error and the response Go returns beside it.
#[derive(Debug)]
pub struct ExecuteError {
    pub response: Option<Box<CommandResponse>>,
    pub error: Box<AppError>,
    /// The plugin's own `*AppError`, which Go hands on as it arrived; every other error here
    /// was made by `NewAppError`, which translates at construction.
    pub from_plugin: bool,
}

impl App {
    /// The plugin commands this process hosts.
    pub fn plugin_commands(&self) -> &PluginCommandRegistry {
        self.plugin_host().commands()
    }

    /// Port of `model.GenerateTriggerId` with `App.AsymmetricSigningKey` (app/command.go:236):
    /// `(client trigger id, trigger id)`. The trigger id is base64 of
    /// `<client id>:<user id>:<millis>:<base64 of the DER ECDSA signature>`. Without a key Go's
    /// signer fails, which it only logs, and both are empty.
    pub async fn generate_trigger_id(&self, user_id: &str) -> (String, String) {
        use base64::Engine as _;
        use p256::ecdsa::signature::Signer as _;

        let Some(key) = self.asymmetric_signing_key().await else {
            tracing::warn!("error occurred in generating trigger Id for a user: no signing key");
            return (String::new(), String::new());
        };
        let client_trigger_id = new_id();
        let trigger_data = format!("{client_trigger_id}:{user_id}:{}:", get_millis());
        let signature: p256::ecdsa::Signature = key.sign(trigger_data.as_bytes());
        let b64 = base64::engine::general_purpose::STANDARD;
        let signature = b64.encode(signature.to_der().as_bytes());
        let trigger_id = b64.encode(format!("{trigger_data}{signature}").as_bytes());
        (client_trigger_id, trigger_id)
    }

    /// `ExecuteCommand` (app/command.go:219) from the trigger id to the plugin's answer handled:
    /// `GenerateTriggerId`, then `tryExecutePluginCommand`, then `HandleCommandResponse` with
    /// `builtIn` **true** — a plugin's response is not a bot post unless it overrides the
    /// username or icon. `args.user_id`, `site_url` and `connection_id` are the caller's to set.
    #[tracing::instrument(skip_all, fields(user_id = %args.user_id))]
    pub async fn execute_plugin_command(
        &self,
        ctx: &HookContext,
        session: &Session,
        args: &mut CommandArgs,
    ) -> ExecuteOutcome {
        let (client_trigger_id, trigger_id) = self.generate_trigger_id(&args.user_id).await;
        args.trigger_id = trigger_id;

        let (command, mut response) = match self.try_execute_plugin_command(ctx, args).await {
            Err((error, from_plugin)) => {
                return ExecuteOutcome::Answered(Err(ExecuteError {
                    response: None,
                    error,
                    from_plugin,
                }));
            }
            Ok(None) => return ExecuteOutcome::NotPlugin,
            Ok(Some(answered)) => answered,
        };
        response.trigger_id = client_trigger_id;
        match self
            .handle_command_response(ctx, session, &command, args, &mut response, true)
            .await
        {
            Ok(()) => ExecuteOutcome::Answered(Ok(Box::new(response))),
            Err(error) => ExecuteOutcome::Answered(Err(ExecuteError {
                response: Some(Box::new(response)),
                error,
                from_plugin: false,
            })),
        }
    }

    /// Port of `App.tryExecutePluginCommand` (app/plugin_commands.go:121). `Ok(None)` is Go's
    /// fall-through: no match, no plugins environment, or a plugin that answered nothing. The
    /// error's flag says it is the plugin's own.
    async fn try_execute_plugin_command(
        &self,
        ctx: &HookContext,
        args: &mut CommandArgs,
    ) -> Result<Option<(Command, CommandResponse)>, (Box<AppError>, bool)> {
        let trigger = plugin_trigger(&args.command);
        let Some(matched) = self.plugin_commands().matching(&args.team_id, &trigger) else {
            return Ok(None);
        };
        let Some(environment) = self.plugins_environment() else {
            return Ok(None);
        };
        let command_param = || {
            HashMap::from([(
                "Command".to_owned(),
                serde_json::Value::String(trigger.clone()),
            )])
        };

        if environment
            .perform_health_check(&matched.plugin_id)
            .await
            .is_err()
        {
            return Err((
                AppError::boxed(
                    "ExecutePluginCommand",
                    "model.plugin_command_error.error.app_error",
                    Some(command_param()),
                    format!("err= Plugin has recently crashed: {}", matched.plugin_id),
                    500,
                ),
                false,
            ));
        }
        let hooks = environment
            .hooks_for_plugin(&matched.plugin_id)
            .map_err(|err| {
                (
                    Box::new(
                        AppError::new(
                            "ExecutePluginCommand",
                            "model.plugin_command.error.app_error",
                            None,
                            String::new(),
                            500,
                        )
                        .wrap(err),
                    ),
                    false,
                )
            })?;

        for (username, user_id) in self
            .mentions_to_team_members(&args.command, &args.team_id)
            .await
        {
            args.add_user_mention(username, user_id);
        }
        for (name, channel_id) in self
            .mentions_to_public_channels(&args.command, &args.team_id)
            .await
        {
            args.add_channel_mention(name, channel_id);
        }

        let returns = hooks
            .execute_command(wire_plugin::Z_ExecuteCommandArgs {
                a: Some(Box::new(ctx.to_wire())),
                b: Some(Box::new(command_args_to_wire(args))),
            })
            .await;

        if environment
            .perform_health_check(&matched.plugin_id)
            .await
            .is_err()
        {
            let mut params = command_param();
            params.insert(
                "PluginId".to_owned(),
                serde_json::Value::String(matched.plugin_id.clone()),
            );
            return Err((
                AppError::boxed(
                    "ExecutePluginCommand",
                    "model.plugin_command_crash.error.app_error",
                    Some(params),
                    format!(
                        "err= Plugin {} crashed due to /{trigger} command",
                        matched.plugin_id
                    ),
                    500,
                ),
                false,
            ));
        }

        if let Some(err) = returns.b {
            return Err((app_error_from_wire(&err, &matched.plugin_id), true));
        }
        Ok(returns
            .a
            .map(|response| (matched.command, command_response_from_wire(&response))))
    }

    /// Port of `App.HandleCommandResponse` (app/command.go:619): the response's post, then each
    /// extra response's, and one error for all of them — the **last** failure is logged and
    /// replaced by `create_post_failed`, naming the trigger.
    pub async fn handle_command_response(
        &self,
        ctx: &HookContext,
        session: &Session,
        command: &Command,
        args: &CommandArgs,
        response: &mut CommandResponse,
        built_in: bool,
    ) -> Result<(), Box<AppError>> {
        let trigger = if args.command.is_empty() {
            String::new()
        } else {
            plugin_trigger(&args.command)
        };

        let mut failed = false;
        if let Err(err) = self
            .handle_command_response_post(ctx, session, command, args, response, built_in)
            .await
        {
            tracing::debug!(error = %err, "Error occurred in handling command response post");
            failed = true;
        }
        if let Some(extra) = response.extra_responses.as_mut() {
            for resp in extra.iter_mut() {
                if let Err(err) = self
                    .handle_command_response_post(ctx, session, command, args, resp, built_in)
                    .await
                {
                    tracing::debug!(error = %err, "Error occurred in handling command response post");
                    failed = true;
                }
            }
        }

        if failed {
            return Err(AppError::boxed(
                "command",
                "api.command.execute_command.create_post_failed.app_error",
                Some(HashMap::from([(
                    "Trigger".to_owned(),
                    serde_json::Value::String(trigger),
                )])),
                String::new(),
                500,
            ));
        }
        Ok(())
    }

    /// Port of `App.HandleCommandResponsePost` (app/command.go:653). `response` is rewritten in
    /// place — its text and attachments Slack-processed — because Go's is a pointer and the
    /// client's answer shows the result.
    async fn handle_command_response_post(
        &self,
        ctx: &HookContext,
        session: &Session,
        command: &Command,
        args: &CommandArgs,
        response: &mut CommandResponse,
        built_in: bool,
    ) -> Result<Post, CommandPostError> {
        let mut post = Post {
            channel_id: args.channel_id.clone(),
            root_id: args.root_id.clone(),
            user_id: args.user_id.clone(),
            post_type: response.type_.clone(),
            ..Post::default()
        };
        post.set_props(response.props.clone());

        if !response.channel_id.is_empty() {
            if let Err(err) = self
                .get_channel_member(&response.channel_id, &args.user_id)
                .await
            {
                return Err(CommandPostError::App(Box::new(
                    AppError::new(
                        "HandleCommandResponsePost",
                        "api.command.command_post.forbidden.app_error",
                        None,
                        String::new(),
                        403,
                    )
                    .wrap(*err),
                )));
            }
            post.channel_id.clone_from(&response.channel_id);
        }

        let mut is_bot_post = !built_in;
        let config = self.config();
        if config.enable_post_username_override {
            if !command.username.is_empty() {
                post.add_prop(
                    POST_PROPS_OVERRIDE_USERNAME,
                    serde_json::Value::String(command.username.clone()),
                );
                is_bot_post = true;
            } else if !response.username.is_empty() {
                post.add_prop(
                    POST_PROPS_OVERRIDE_USERNAME,
                    serde_json::Value::String(response.username.clone()),
                );
                is_bot_post = true;
            }
        }
        if config.enable_post_icon_override {
            if !command.icon_url.is_empty() {
                post.add_prop(
                    POST_PROPS_OVERRIDE_ICON_URL,
                    serde_json::Value::String(command.icon_url.clone()),
                );
                is_bot_post = true;
            } else if !response.icon_url.is_empty() {
                post.add_prop(
                    POST_PROPS_OVERRIDE_ICON_URL,
                    serde_json::Value::String(response.icon_url.clone()),
                );
                is_bot_post = true;
            }
        }
        if is_bot_post {
            post.add_prop(
                POST_PROPS_FROM_WEBHOOK,
                serde_json::Value::String("true".to_owned()),
            );
        }

        if !response.skip_slack_parsing {
            response.text = self.process_slack_text(&response.text).await;
            if let Some(attachments) = response.attachments.take() {
                response.attachments = Some(self.process_message_attachments(attachments).await);
            }
        }

        let skip = response.skip_slack_parsing;
        self.create_command_post(ctx, session, &mut post, response, skip)
            .await?;
        Ok(post)
    }

    /// Port of `App.CreateCommandPost` (app/command.go:55): an in-channel response is a real post,
    /// made with `SetOnline` through `CreatePost`, **not** `CreatePostAsUser` (so nothing is
    /// marked viewed); an ephemeral one, or one with no type, is sent only when it has text or
    /// attachments; any other response type posts nothing and succeeds.
    async fn create_command_post(
        &self,
        ctx: &HookContext,
        session: &Session,
        post: &mut Post,
        response: &CommandResponse,
        skip_slack_parsing: bool,
    ) -> Result<(), CommandPostError> {
        post.message = if skip_slack_parsing {
            response.text.clone()
        } else {
            parse_slack_links_to_markdown(&response.text)
        };
        post.create_at = get_millis();

        if post.post_type.starts_with(POST_SYSTEM_MESSAGE_PREFIX) {
            return Err(CommandPostError::App(Box::new(AppError::new(
                "CreateCommandPost",
                "api.context.invalid_param.app_error",
                Some(HashMap::from([(
                    "Name".to_owned(),
                    serde_json::Value::String("post.type".to_owned()),
                )])),
                String::new(),
                400,
            ))));
        }

        if let Some(attachments) = response.attachments.as_ref() {
            parse_message_attachment(post, attachments.clone());
        }

        if response.response_type == COMMAND_RESPONSE_TYPE_IN_CHANNEL {
            // A bot-style response's `from_webhook` becomes the `FromIncomingWebhook` flag, which
            // this port's `CreatePost` does not model: only an override reaches it, and the
            // override props are refused there anyway.
            if post
                .get_prop(POST_PROPS_FROM_WEBHOOK)
                .is_some_and(|v| v.as_str() == Some("true"))
            {
                return Err(CommandPostError::Unreproducible(
                    "a bot-style command response posts FromIncomingWebhook",
                ));
            }
            let channel = self
                .get_channel(&post.channel_id)
                .await
                .map_err(|mut err| {
                    err.where_ = "CreatePostMissingChannel".to_owned();
                    CommandPostError::App(err)
                })?;
            let flags = CreatePostFlags {
                set_online: true,
                ..CreatePostFlags::default()
            };
            self.create_post(std::mem::take(post), &channel, session, flags, ctx)
                .await
                .map_err(CommandPostError::from)?;
            return Ok(());
        }

        if (response.response_type.is_empty()
            || response.response_type == COMMAND_RESPONSE_TYPE_EPHEMERAL)
            && (!response.text.is_empty() || response.attachments.is_some())
        {
            let user_id = post.user_id.clone();
            match self.send_ephemeral_post(ctx, &user_id, post.clone()).await {
                Ok(_) => {}
                // Go's `SendEphemeralPost` cannot fail; a store error in the prepare is logged.
                Err(PrepareError::App(err)) => {
                    tracing::error!(error = %err, "the ephemeral command response could not be prepared");
                }
                Err(PrepareError::Unreproducible(why)) => {
                    return Err(CommandPostError::Unreproducible(why));
                }
            }
        }
        Ok(())
    }

    /// Port of `PluginAPI.ListBuiltInCommands`' loop (app/plugin_api.go:1443), which
    /// `ListAllCommandsByUser` and `ListAutocompleteCommands` share: each built-in provider's
    /// command that autocompletes, sanitised, first trigger wins. Go's order is its map's, which
    /// is none; this is [`crate::command_provider::BUILTIN_TRIGGERS`]'.
    ///
    /// The strings are `GetCommand(a, i18n.T)`'s — the **server** locale — and this server holds
    /// them in English only, so `None` for any other server locale.
    pub fn list_built_in_commands(&self) -> Option<Vec<Command>> {
        use crate::command_provider::{BUILTIN_TRIGGERS, ProviderCommand, provider_command};
        let config = self.config();
        let locale = crate::i18n::loaded().map_or("en", |bundle| {
            bundle.server_locale(&config.default_server_locale)
        });
        if locale != "en" {
            return None;
        }
        let export_links = self.export_file_backend().generates_links();
        let mut seen = std::collections::HashSet::new();
        let mut commands = Vec::new();
        for trigger in BUILTIN_TRIGGERS {
            match provider_command(&config, export_links, trigger) {
                ProviderCommand::Command(command) => {
                    let mut command = *command;
                    if command.auto_complete && seen.insert(command.trigger.clone()) {
                        command.sanitize();
                        commands.push(command);
                    }
                }
                ProviderCommand::Nil | ProviderCommand::Unregistered => {}
            }
        }
        Some(commands)
    }

    /// Port of `App.ProcessSlackText` (app/slack.go:80): `<!channel>`, `<!here>` and `<!all>`
    /// expanded, then every `<@id>` naming a user replaced by `@username`.
    pub async fn process_slack_text(&self, text: &str) -> String {
        let text = expand_announcement(text);
        self.replace_user_ids(&text).await
    }

    /// `replaceUserIds` (app/slack.go:125). A lookup failure leaves the text as it was.
    async fn replace_user_ids(&self, text: &str) -> String {
        let ids: Vec<String> = slack_user_ids(text);
        if ids.is_empty() {
            return text.to_owned();
        }
        let users = match self.store().user().get_profile_by_ids(&ids, 0).await {
            Ok(users) => users,
            Err(err) => {
                tracing::debug!(error = %err, "replaceUserIds: the profiles could not be read");
                return text.to_owned();
            }
        };
        let mut text = text.to_owned();
        for user in users {
            text = text.replace(&format!("<@{}>", user.id), &format!("@{}", user.username));
        }
        text
    }

    /// Port of `App.ProcessMessageAttachments` (app/slack.go:92): the field values stringified,
    /// then `pretext`, `text`, `title` and every field value put through
    /// [`App::process_slack_text`].
    pub async fn process_message_attachments(
        &self,
        attachments: Vec<MessageAttachment>,
    ) -> Vec<MessageAttachment> {
        let mut attachments = stringify_message_attachment_field_value(attachments);
        for attachment in &mut attachments {
            attachment.pretext = self.process_slack_text(&attachment.pretext).await;
            attachment.text = self.process_slack_text(&attachment.text).await;
            attachment.title = self.process_slack_text(&attachment.title).await;
            for field in attachment.fields.iter_mut().flatten() {
                if !field.value.is_null() {
                    let value = mm_model::utils::go_format_v(&field.value);
                    field.value = serde_json::Value::String(self.process_slack_text(&value).await);
                }
            }
        }
        attachments
    }

    /// Port of `App.MentionsToTeamMembers` (app/command.go:275): each possible `@mention` that
    /// names a member of the team — the name whole, or with trailing `.`, `-` or `_` trimmed
    /// one at a time until a user is found. A store failure other than not-found drops that
    /// mention silently, as Go's goroutine returns.
    pub async fn mentions_to_team_members(
        &self,
        message: &str,
        team_id: &str,
    ) -> HashMap<String, String> {
        let mut found = HashMap::new();
        for mention in possible_at_mentions(message) {
            let user = match self.store().user().get_by_username(&mention).await {
                Ok(user) => Some((mention.clone(), user)),
                Err(err) if !err.is_not_found() => {
                    tracing::warn!(error = %err, "Failed to retrieve user @{mention}");
                    continue;
                }
                Err(_) => {
                    let mut hit = None;
                    let mut word = mention.clone();
                    while let Some(trimmed) = trim_username_special_char(&word) {
                        match self.store().user().get_by_username(&trimmed).await {
                            Ok(user) => {
                                hit = Some((trimmed, user));
                                break;
                            }
                            Err(err) if !err.is_not_found() => break,
                            Err(_) => word = trimmed,
                        }
                    }
                    hit
                }
            };
            let Some((name, user)) = user else {
                continue;
            };
            if self.get_team_member(team_id, &user.id).await.is_ok() {
                found.insert(name, user.id);
            }
        }
        found
    }

    /// Port of `App.MentionsToPublicChannels` (app/command.go:347): each `~name` that names an
    /// open channel of the team.
    pub async fn mentions_to_public_channels(
        &self,
        message: &str,
        team_id: &str,
    ) -> HashMap<String, String> {
        let mut found = HashMap::new();
        for name in mm_model::channel_mentions::channel_mentions(message) {
            let Ok(channel) = self.get_channel_by_name(&name, team_id, false).await else {
                continue;
            };
            if channel.channel_type == CHANNEL_TYPE_OPEN {
                found.insert(name, channel.id);
            }
        }
        found
    }
}

/// Why a command response's post was not made.
#[derive(Debug, thiserror::Error)]
enum CommandPostError {
    /// Go's own failure.
    #[error("{0}")]
    App(Box<AppError>),
    /// A post Go would have made and this server cannot; see the module docs.
    #[error("unreproducible: {0}")]
    Unreproducible(&'static str),
}

impl From<PrepareError> for CommandPostError {
    fn from(err: PrepareError) -> Self {
        match err {
            PrepareError::App(err) => Self::App(err),
            PrepareError::Unreproducible(why) => {
                tracing::error!(
                    why,
                    "a command response post this server cannot write; answered as a failed post (D-1020)"
                );
                Self::Unreproducible(why)
            }
        }
    }
}

/// `expandAnnouncement` (app/slack.go:113).
fn expand_announcement(text: &str) -> String {
    text.replace("<!channel>", "@channel")
        .replace("<!here>", "@here")
        .replace("<!all>", "@all")
}

/// The ids `<@([a-zA-Z0-9]+)>` captures, in order.
fn slack_user_ids(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("<@([a-zA-Z0-9]+)>").ok())
        .as_ref()
        .map(|re| {
            re.captures_iter(text)
                .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Port of `possibleAtMentions` (app/command.go:879): every match of
/// `\B@[[:alnum:]][[:alnum:]\.\-_:]*`, lower-cased and kept when it is a valid username
/// (remote allowed), each once, in order.
///
/// RE2's `\B` and `[[:alnum:]]` are ASCII, so the `@` must be at the start or after a byte that
/// is not an ASCII word character — a letter with an accent before it counts as a boundary.
pub fn possible_at_mentions(message: &str) -> Vec<String> {
    let mut names = Vec::new();
    if !message.contains('@') {
        return names;
    }
    let bytes = message.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@'
            || (i > 0 && is_word(bytes[i - 1]))
            || !bytes.get(i + 1).is_some_and(u8::is_ascii_alphanumeric)
        {
            i += 1;
            continue;
        }
        let mut end = i + 2;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric()
                || matches!(bytes[end], b'.' | b'-' | b'_' | b':'))
        {
            end += 1;
        }
        let name = mm_model::user::normalize_username(&message[i + 1..end]);
        if !names.contains(&name) && mm_model::user::is_valid_username_allow_remote(&name) {
            names.push(name);
        }
        i = end;
    }
    names
}

/// Port of `trimUsernameSpecialChar` (app/command.go:901): the word without its last byte when
/// that byte is `.`, `-` or `_`.
fn trim_username_special_char(word: &str) -> Option<String> {
    word.strip_suffix(['.', '-', '_']).map(str::to_owned)
}

// -------------------------------------------------------------------------------------------
// model <-> wire
// -------------------------------------------------------------------------------------------

/// A `model.CommandArgs` as gob sends it: `T` is a function and does not cross.
pub fn command_args_to_wire(args: &CommandArgs) -> wire_model::CommandArgs {
    wire_model::CommandArgs {
        user_id: args.user_id.clone(),
        channel_id: args.channel_id.clone(),
        team_id: args.team_id.clone(),
        root_id: args.root_id.clone(),
        parent_id: args.parent_id.clone(),
        trigger_id: args.trigger_id.clone(),
        connection_id: args.connection_id.clone(),
        command: args.command.clone(),
        site_url: args.site_url.clone(),
        user_mentions: args
            .user_mentions
            .0
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        channel_mentions: args
            .channel_mentions
            .0
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// The reverse, for `ExecuteSlashCommand`.
pub fn command_args_from_wire(wire: &wire_model::CommandArgs) -> CommandArgs {
    let mut args = CommandArgs {
        user_id: wire.user_id.clone(),
        channel_id: wire.channel_id.clone(),
        team_id: wire.team_id.clone(),
        root_id: wire.root_id.clone(),
        parent_id: wire.parent_id.clone(),
        trigger_id: wire.trigger_id.clone(),
        connection_id: wire.connection_id.clone(),
        command: wire.command.clone(),
        site_url: wire.site_url.clone(),
        ..CommandArgs::default()
    };
    for (k, v) in &wire.user_mentions {
        args.add_user_mention(k.clone(), v.clone());
    }
    for (k, v) in &wire.channel_mentions {
        args.add_channel_mention(k.clone(), v.clone());
    }
    args
}

/// A `*model.CommandResponse` a plugin answered. Gob sends an empty slice or map as nothing, so
/// each arrives as Go's nil — `null` when the response is written as JSON.
pub fn command_response_from_wire(wire: &wire_model::CommandResponse) -> CommandResponse {
    CommandResponse {
        response_type: wire.response_type.clone(),
        text: wire.text.clone(),
        username: wire.username.clone(),
        channel_id: wire.channel_id.clone(),
        icon_url: wire.icon_url.clone(),
        type_: wire.r#type.clone(),
        props: (!wire.props.is_empty()).then(|| props_from_wire(&wire.props)),
        goto_location: wire.goto_location.clone(),
        trigger_id: wire.trigger_id.clone(),
        skip_slack_parsing: wire.skip_slack_parsing,
        attachments: attachments_from_wire(&wire.attachments),
        extra_responses: (!wire.extra_responses.is_empty()).then(|| {
            wire.extra_responses
                .iter()
                .map(command_response_from_wire)
                .collect()
        }),
    }
}

/// The attachments through their JSON: the wire type has Go's field names and the model has the
/// `json:` tags, and `interface_to_json` is the generated bridge between them. `None` for Go's
/// nil, and for a value the bridge cannot render (logged).
fn attachments_from_wire(wire: &[wire_model::MessageAttachment]) -> Option<Vec<MessageAttachment>> {
    if wire.is_empty() {
        return None;
    }
    let json = gobwire::Interface::new(registered::MESSAGE_ATTACHMENT_PTR_SLICE, &wire.to_vec())
        .ok()
        .and_then(|i| mm_plugin::wire::interface_to_json(&i));
    match json.map(serde_json::from_value::<Vec<MessageAttachment>>) {
        Some(Ok(attachments)) => Some(attachments),
        other => {
            tracing::error!(
                ?other,
                "a plugin's command response attachments did not convert"
            );
            None
        }
    }
}

/// A `*model.CommandResponse` as gob sends it back to a plugin (`ExecuteSlashCommand`).
///
/// **Without its attachments**: the model's cannot go back into the wire type. Only a response
/// whose post was refused as unreproducible, or one of an unknown response type, can carry one
/// here ([D-1020]).
pub fn command_response_to_wire(response: &CommandResponse) -> wire_model::CommandResponse {
    wire_model::CommandResponse {
        response_type: response.response_type.clone(),
        text: response.text.clone(),
        username: response.username.clone(),
        channel_id: response.channel_id.clone(),
        icon_url: response.icon_url.clone(),
        r#type: response.type_.clone(),
        props: props_to_wire(response.props.as_ref()),
        goto_location: response.goto_location.clone(),
        trigger_id: response.trigger_id.clone(),
        skip_slack_parsing: response.skip_slack_parsing,
        attachments: Vec::new(),
        extra_responses: response
            .extra_responses
            .iter()
            .flatten()
            .map(command_response_to_wire)
            .collect(),
    }
}

/// A plugin's `*model.AppError`, as the host holds it: gob carried every exported field and not
/// the parameters, so a `Translate` afterwards finds none. A status outside 100-999 is bucketed
/// to 500 first, as `tryExecutePluginCommand` does ("setting a status code of 0 will crash the
/// server").
pub fn app_error_from_wire(wire: &wire_model::AppError, plugin_id: &str) -> Box<AppError> {
    let status = if (100..=999).contains(&wire.status_code) {
        wire.status_code
    } else {
        tracing::warn!(
            plugin_id,
            status_code = wire.status_code,
            "Invalid status code returned from plugin. Converting to internal server error."
        );
        500
    };
    let mut err = AppError::new(
        wire.r#where.clone(),
        wire.id.clone(),
        None,
        wire.detailed_error.clone(),
        i32::try_from(status).unwrap_or(500),
    );
    err.message.clone_from(&wire.message);
    err.request_id.clone_from(&wire.request_id);
    err.skip_translation = wire.skip_translation;
    Box::new(err)
}

/// A `*model.Command` as gob sends it.
pub fn command_to_wire(command: &Command) -> wire_model::Command {
    wire_model::Command {
        id: command.id.clone(),
        token: command.token.clone(),
        create_at: command.create_at,
        update_at: command.update_at,
        delete_at: command.delete_at,
        creator_id: command.creator_id.clone(),
        team_id: command.team_id.clone(),
        trigger: command.trigger.clone(),
        method: command.method.clone(),
        username: command.username.clone(),
        icon_url: command.icon_url.clone(),
        auto_complete: command.auto_complete,
        auto_complete_desc: command.auto_complete_desc.clone(),
        auto_complete_hint: command.auto_complete_hint.clone(),
        display_name: command.display_name.clone(),
        description: command.description.clone(),
        url: command.url.clone(),
        plugin_id: command.plugin_id.clone(),
        autocomplete_data: command
            .autocomplete_data
            .as_ref()
            .map(|d| Box::new(autocomplete_data_to_wire(d))),
        autocomplete_icon_data: command.autocomplete_icon_data.clone(),
    }
}

/// The reverse, for `RegisterCommand`.
pub fn command_from_wire(wire: &wire_model::Command) -> Command {
    Command {
        id: wire.id.clone(),
        token: wire.token.clone(),
        create_at: wire.create_at,
        update_at: wire.update_at,
        delete_at: wire.delete_at,
        creator_id: wire.creator_id.clone(),
        team_id: wire.team_id.clone(),
        trigger: wire.trigger.clone(),
        method: wire.method.clone(),
        username: wire.username.clone(),
        icon_url: wire.icon_url.clone(),
        auto_complete: wire.auto_complete,
        auto_complete_desc: wire.auto_complete_desc.clone(),
        auto_complete_hint: wire.auto_complete_hint.clone(),
        display_name: wire.display_name.clone(),
        description: wire.description.clone(),
        url: wire.url.clone(),
        plugin_id: wire.plugin_id.clone(),
        autocomplete_data: wire
            .autocomplete_data
            .as_deref()
            .map(autocomplete_data_from_wire),
        autocomplete_icon_data: wire.autocomplete_icon_data.clone(),
    }
}

fn autocomplete_data_to_wire(data: &AutocompleteData) -> wire_model::AutocompleteData {
    wire_model::AutocompleteData {
        trigger: data.trigger.clone(),
        hint: data.hint.clone(),
        help_text: data.help_text.clone(),
        role_id: data.role_id.clone(),
        arguments: data
            .arguments_slice()
            .iter()
            .map(autocomplete_arg_to_wire)
            .collect(),
        sub_commands: data
            .sub_commands_slice()
            .iter()
            .map(autocomplete_data_to_wire)
            .collect(),
    }
}

/// Gob sends an empty slice as nothing, so both arrive as Go's nil.
fn autocomplete_data_from_wire(wire: &wire_model::AutocompleteData) -> AutocompleteData {
    AutocompleteData {
        trigger: wire.trigger.clone(),
        hint: wire.hint.clone(),
        help_text: wire.help_text.clone(),
        role_id: wire.role_id.clone(),
        arguments: (!wire.arguments.is_empty()).then(|| {
            wire.arguments
                .iter()
                .map(autocomplete_arg_from_wire)
                .collect()
        }),
        sub_commands: (!wire.sub_commands.is_empty()).then(|| {
            wire.sub_commands
                .iter()
                .map(autocomplete_data_from_wire)
                .collect()
        }),
    }
}

fn autocomplete_arg_to_wire(arg: &AutocompleteArg) -> wire_model::AutocompleteArg {
    let data = match &arg.data {
        AutocompleteArgData::Text(t) => gobwire::Interface::new(
            registered::AUTOCOMPLETE_TEXT_ARG,
            &wire_model::AutocompleteTextArg {
                hint: t.hint.clone(),
                pattern: t.pattern.clone(),
            },
        )
        .ok(),
        AutocompleteArgData::StaticList(s) => gobwire::Interface::new(
            registered::AUTOCOMPLETE_STATIC_LIST_ARG,
            &wire_model::AutocompleteStaticListArg {
                possible_arguments: s
                    .possible_arguments
                    .iter()
                    .flatten()
                    .map(|item| wire_model::AutocompleteListItem {
                        item: item.item.clone(),
                        hint: item.hint.clone(),
                        help_text: item.help_text.clone(),
                    })
                    .collect(),
            },
        )
        .ok(),
        AutocompleteArgData::DynamicList(d) => gobwire::Interface::new(
            registered::AUTOCOMPLETE_DYNAMIC_LIST_ARG,
            &wire_model::AutocompleteDynamicListArg {
                fetch_url: d.fetch_url.clone(),
            },
        )
        .ok(),
        AutocompleteArgData::None => None,
    };
    wire_model::AutocompleteArg {
        name: arg.name.clone(),
        help_text: arg.help_text.clone(),
        r#type: arg.type_.0.clone(),
        required: arg.required,
        data,
    }
}

/// `Data` by the type it was registered under — not by `Type`, which is what lets
/// `AutocompleteData.IsValid` catch a mismatch as Go does. An unknown or nil value is Go's nil.
fn autocomplete_arg_from_wire(wire: &wire_model::AutocompleteArg) -> AutocompleteArg {
    let data = match wire.data.as_ref() {
        Some(i) if i.name == registered::AUTOCOMPLETE_TEXT_ARG => i
            .downcast::<wire_model::AutocompleteTextArg>()
            .map(|t| {
                AutocompleteArgData::Text(AutocompleteTextArg {
                    hint: t.hint,
                    pattern: t.pattern,
                })
            })
            .unwrap_or_default(),
        Some(i) if i.name == registered::AUTOCOMPLETE_STATIC_LIST_ARG => i
            .downcast::<wire_model::AutocompleteStaticListArg>()
            .map(|s| {
                AutocompleteArgData::StaticList(AutocompleteStaticListArg {
                    possible_arguments: (!s.possible_arguments.is_empty()).then(|| {
                        s.possible_arguments
                            .into_iter()
                            .map(|item| AutocompleteListItem {
                                item: item.item,
                                hint: item.hint,
                                help_text: item.help_text,
                            })
                            .collect()
                    }),
                })
            })
            .unwrap_or_default(),
        Some(i) if i.name == registered::AUTOCOMPLETE_DYNAMIC_LIST_ARG => i
            .downcast::<wire_model::AutocompleteDynamicListArg>()
            .map(|d| {
                AutocompleteArgData::DynamicList(AutocompleteDynamicListArg {
                    fetch_url: d.fetch_url,
                })
            })
            .unwrap_or_default(),
        _ => AutocompleteArgData::None,
    };
    AutocompleteArg {
        name: wire.name.clone(),
        help_text: wire.help_text.clone(),
        type_: AutocompleteArgType(wire.r#type.clone()),
        required: wire.required,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(trigger: &str, team: &str) -> Command {
        Command {
            trigger: trigger.to_owned(),
            team_id: team.to_owned(),
            auto_complete: true,
            auto_complete_desc: "desc".into(),
            auto_complete_hint: "hint".into(),
            display_name: "Display".into(),
            username: "dropped".into(),
            icon_url: "dropped".into(),
            plugin_id: "dropped".into(),
            method: "P".into(),
            ..Command::default()
        }
    }

    /// The copy keeps eight fields, lower-cases the trigger, and builds the default autocomplete
    /// data from the trigger **as spelled**.
    #[test]
    fn a_registration_is_a_lower_cased_copy_of_eight_fields() {
        let registry = PluginCommandRegistry::default();
        registry
            .register("p", command("HookRec", ""))
            .expect("registered");
        let listed = registry.for_team("t");
        assert_eq!(listed.len(), 1);
        let c = &listed[0];
        assert_eq!(c.trigger, "hookrec");
        assert_eq!(
            (
                c.username.as_str(),
                c.icon_url.as_str(),
                c.plugin_id.as_str()
            ),
            ("", "", "")
        );
        assert_eq!(c.method, "");
        assert_eq!(c.display_name, "Display");
        let data = c.autocomplete_data.as_ref().expect("default data");
        assert_eq!(data.trigger, "HookRec");
        assert_eq!(
            (data.hint.as_str(), data.help_text.as_str()),
            ("hint", "desc")
        );
        assert_eq!(data.arguments, Some(vec![]));
    }

    #[test]
    fn an_empty_trigger_is_an_invalid_command() {
        let registry = PluginCommandRegistry::default();
        assert_eq!(
            registry.register("p", command("", "")),
            Err(RegisterCommandError::InvalidCommand)
        );
        assert_eq!(
            RegisterCommandError::InvalidCommand.to_string(),
            "invalid command"
        );
        assert!(registry.for_team("").is_empty());
    }

    /// Invalid autocomplete data is refused with pkg/errors' `Wrap` text.
    #[test]
    fn invalid_autocomplete_data_is_refused_with_the_wrapped_text() {
        let registry = PluginCommandRegistry::default();
        let mut c = command("x", "");
        c.autocomplete_data = Some(AutocompleteData::new("X", "", ""));
        let err = registry.register("p", c).expect_err("refused");
        assert_eq!(
            err.to_string(),
            "invalid autocomplete data in command: Command should be lowercase"
        );
        assert!(registry.for_team("").is_empty());
    }

    /// A relative `FetchURL` is made absolute under `/plugins/<id>`; an absolute one is kept.
    #[test]
    fn a_relative_fetch_url_is_rooted_at_the_plugin() {
        let registry = PluginCommandRegistry::default();
        let mut data = AutocompleteData::new("x", "", "");
        data.add_dynamic_list_argument("", "dynamic/list", true);
        data.add_dynamic_list_argument("", "https://example.com/abs", true);
        let mut c = command("x", "");
        c.autocomplete_data = Some(data);
        registry.register("plug.in", c).expect("registered");
        let listed = registry.for_team("");
        let urls: Vec<String> = listed[0]
            .autocomplete_data
            .as_ref()
            .unwrap()
            .arguments_slice()
            .iter()
            .map(|a| match &a.data {
                AutocompleteArgData::DynamicList(d) => d.fetch_url.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            urls,
            vec!["/plugins/plug.in/dynamic/list", "https://example.com/abs"]
        );
    }

    /// The same plugin re-registering replaces its entry in place; another plugin's is appended,
    /// and the first registration keeps the trigger.
    #[test]
    fn a_second_plugin_does_not_take_a_trigger() {
        let registry = PluginCommandRegistry::default();
        registry.register("a", command("go", "")).unwrap();
        registry.register("b", command("go", "")).unwrap();
        let mut again = command("go", "");
        again.display_name = "again".into();
        registry.register("a", again).unwrap();
        let listed = registry.for_team("");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].display_name, "again");
        assert_eq!(registry.matching("", "go").unwrap().plugin_id, "a");
        // Another team's registration of the same trigger is a separate entry.
        registry.register("a", command("go", "t1")).unwrap();
        assert_eq!(registry.for_team("t1").len(), 3);
        assert_eq!(registry.for_team("t2").len(), 2);
    }

    /// A team's command matches that team only; a global one matches every team.
    #[test]
    fn a_team_command_matches_its_team_only() {
        let registry = PluginCommandRegistry::default();
        registry.register("a", command("t", "team1")).unwrap();
        assert!(registry.matching("team1", "t").is_some());
        assert!(registry.matching("team2", "t").is_none());
        assert!(registry.matching("team1", "u").is_none());
        assert!(registry.for_team("team2").is_empty());
    }

    /// `UnregisterCommand` removes every plugin's entry for the team and trigger — and only that
    /// team's; `unregisterPluginCommands` removes one plugin's commands everywhere.
    #[test]
    fn unregistering_follows_go() {
        let registry = PluginCommandRegistry::default();
        registry.register("a", command("go", "")).unwrap();
        registry.register("b", command("go", "")).unwrap();
        registry.register("b", command("go", "t1")).unwrap();
        registry.register("b", command("stay", "")).unwrap();
        registry.unregister("", "GO");
        let triggers: Vec<(String, String)> = registry
            .for_team("t1")
            .into_iter()
            .map(|c| (c.trigger, c.team_id))
            .collect();
        assert_eq!(
            triggers,
            vec![("go".into(), "t1".into()), ("stay".into(), String::new())]
        );
        registry.unregister_plugin("b");
        assert!(registry.for_team("t1").is_empty());
        registry.register("a", command("x", "")).unwrap();
        registry.register("c", command("y", "")).unwrap();
        registry.unregister_plugin("a");
        assert_eq!(registry.for_team("")[0].trigger, "y");
    }

    /// The plugin match splits on spaces only and drops the first byte.
    #[test]
    fn the_plugin_trigger_splits_on_spaces_only() {
        assert_eq!(plugin_trigger("/HookRec ephemeral x"), "hookrec");
        assert_eq!(plugin_trigger("/hookrec\tx"), "hookrec\tx");
        assert_eq!(plugin_trigger("/"), "");
        assert_eq!(plugin_trigger(""), "");
    }

    #[test]
    fn possible_at_mentions_follow_the_ascii_boundary() {
        assert_eq!(
            possible_at_mentions("hi @Alice and @bob. and a@c and @alice"),
            vec!["alice", "bob."]
        );
        assert_eq!(possible_at_mentions("é@carol"), vec!["carol"]);
        assert_eq!(possible_at_mentions("@-x @_y"), Vec::<String>::new());
        assert_eq!(possible_at_mentions("no mentions"), Vec::<String>::new());
        assert_eq!(possible_at_mentions("(@dave:remote)"), vec!["dave:remote"]);
    }

    #[test]
    fn trimming_takes_one_special_character_at_a_time() {
        assert_eq!(trim_username_special_char("bob._"), Some("bob.".into()));
        assert_eq!(trim_username_special_char("bob"), None);
        assert_eq!(trim_username_special_char(""), None);
    }

    #[test]
    fn announcements_expand_and_user_ids_are_found() {
        assert_eq!(
            expand_announcement("<!channel> <!here> <!all> <!other>"),
            "@channel @here @all <!other>"
        );
        assert_eq!(
            slack_user_ids("<@abc123> <@x-y> <@Z9>"),
            vec!["abc123", "Z9"]
        );
    }

    /// Go buckets a status outside 100-999 to 500 and keeps everything else gob carried.
    #[test]
    fn a_plugin_error_keeps_its_fields_with_a_bucketed_status() {
        let wire = wire_model::AppError {
            id: "p.id".into(),
            message: "msg".into(),
            detailed_error: "d".into(),
            request_id: "r".into(),
            status_code: 0,
            r#where: "W".into(),
            skip_translation: true,
        };
        let err = app_error_from_wire(&wire, "p");
        assert_eq!(err.status_code, 500);
        assert_eq!(
            (err.id.as_str(), err.message.as_str(), err.where_.as_str()),
            ("p.id", "msg", "W")
        );
        assert!(err.skip_translation);
        for (sent, kept) in [(99, 500), (100, 100), (418, 418), (999, 999), (1000, 500)] {
            let wire = wire_model::AppError {
                status_code: sent,
                ..wire.clone()
            };
            assert_eq!(app_error_from_wire(&wire, "p").status_code, kept, "{sent}");
        }
    }

    /// A command and its autocomplete tree survive the wire both ways, and gob's empty slices
    /// come back as Go's nil.
    #[test]
    fn a_command_round_trips_through_the_wire() {
        let mut data = AutocompleteData::new("x", "h", "t");
        data.add_text_argument("text", "hint", "pat");
        data.add_static_list_argument(
            "list",
            true,
            vec![AutocompleteListItem {
                item: "a".into(),
                hint: "ha".into(),
                help_text: "ta".into(),
            }],
        );
        data.add_dynamic_list_argument("dyn", "u", false);
        let mut c = command("x", "t");
        c.autocomplete_data = Some(data);
        let back = command_from_wire(&command_to_wire(&c));
        // `NewAutocompleteData`'s empty `SubCommands` is gob's nothing, so Go's nil on arrival.
        if let Some(d) = c.autocomplete_data.as_mut() {
            d.sub_commands = None;
        }
        assert_eq!(back, c);

        let empty = Command {
            autocomplete_data: Some(AutocompleteData::new("x", "", "")),
            ..Command::default()
        };
        let back = command_from_wire(&command_to_wire(&empty));
        let data = back.autocomplete_data.unwrap();
        assert_eq!((data.arguments, data.sub_commands), (None, None));
    }

    /// An empty props map, attachment list or extra-response list is Go's nil after gob.
    #[test]
    fn a_response_from_the_wire_keeps_nil_apart() {
        let wire = wire_model::CommandResponse {
            response_type: "ephemeral".into(),
            text: "t".into(),
            goto_location: "g".into(),
            skip_slack_parsing: true,
            extra_responses: vec![wire_model::CommandResponse {
                text: "e".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let response = command_response_from_wire(&wire);
        assert_eq!(response.props, None);
        assert_eq!(response.attachments, None);
        let extra = response.extra_responses.as_ref().expect("one extra");
        assert_eq!(extra[0].text, "e");
        assert_eq!(extra[0].extra_responses, None);
        assert!(response.skip_slack_parsing);
        let back = command_response_to_wire(&response);
        assert_eq!(back, wire);
    }
}
