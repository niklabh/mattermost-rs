//! The hook recorder's slash commands, for `parity::plugin_hooks`' command tranche
//! (docs/PLUGIN_PLAN.md, Phase 6): what it registers on activation, how `ExecuteCommand` answers,
//! and the command script — the command half of the plugin API, run from inside
//! `ExecuteCommand`.
//!
//! All of it is switched on by `HOOK_RECORDER_COMMANDS`, so the other tranches' transcripts carry
//! neither the `OnActivate` entry nor `ExecuteCommand` in `Implemented`.
//!
//! # `ExecuteCommand` answers by the command's first word
//!
//! | word | answer |
//! |---|---|
//! | `ephemeral <text>` | an ephemeral response |
//! | `in_channel <text>` | an in-channel response |
//! | `plain <text>` | a response with no type, which Go treats as ephemeral |
//! | `skip <text>` | ephemeral, `skip_slack_parsing` set |
//! | `props` | ephemeral, with props |
//! | `goto` | only a `goto_location`: no text, so no post |
//! | `extra` | ephemeral, with an in-channel and an ephemeral extra response |
//! | `forbidden <channel>` | in-channel into a channel the user is not in |
//! | `system` | in-channel with a `system_` post type |
//! | `error`, `error-skip`, `error-status` | an `*AppError`: plain, `SkipTranslation`, status 0 |
//! | `nothing` | neither a response nor an error |
//! | `script` | runs the command script, then an ephemeral `script done` |
//! | anything else | ephemeral, echoing the command |

use go_netrpc::Client;
use gobwire::{Decode, Encode};
use mm_plugin::wire::model::{
    AppError, AutocompleteArg, AutocompleteData, AutocompleteDynamicListArg, AutocompleteListItem,
    AutocompleteStaticListArg, AutocompleteTextArg, Command, CommandArgs, CommandResponse,
};
use mm_plugin::wire::plugin::*;
use mm_plugin::wire::registered;
use serde_json::{Value as Json, json};

use crate::render::render_typed;

/// The variable that switches the commands on.
pub const SWITCH: &str = "HOOK_RECORDER_COMMANDS";

/// A team the suite names, for the one team-scoped registration.
const TEAM_VAR: &str = "HOOK_RECORDER_COMMAND_TEAM";

/// A well-formed id that names nothing.
const MISSING: &str = "cmdmissingcmdmissingcmdmis";

pub fn enabled() -> bool {
    std::env::var_os(SWITCH).is_some()
}

async fn call<A, R>(api: &Client, out: &mut Vec<Json>, name: &str, args: A) -> Option<R>
where
    A: Encode,
    R: Decode + Default + Encode + Send + 'static,
{
    call_then(api, out, name, args, |_| {}).await
}

/// [`call`] with the answer adjusted before it is written down — sorted, for the lists whose
/// order is Go's map order.
async fn call_then<A, R>(
    api: &Client,
    out: &mut Vec<Json>,
    name: &str,
    args: A,
    adjust: impl FnOnce(&mut R),
) -> Option<R>
where
    A: Encode,
    R: Decode + Default + Encode + Send + 'static,
{
    match api.call::<A, R>(&format!("Plugin.{name}"), &args).await {
        Ok(mut returns) => {
            adjust(&mut returns);
            out.push(json!({
                "call": name,
                "args": render_typed(&args),
                "returns": render_typed(&returns),
            }));
            Some(returns)
        }
        Err(e) => {
            out.push(json!({ "call": name, "args": render_typed(&args), "error": e.to_string() }));
            None
        }
    }
}

fn interface<T: Encode>(name: &str, value: &T) -> Option<gobwire::Interface> {
    gobwire::Interface::new(name, value).ok()
}

/// `/hookrec`'s autocomplete tree: three subcommands, one of each argument kind, the dynamic
/// one's URL relative so the host roots it at `/plugins/<id>`.
fn hookrec_autocomplete() -> AutocompleteData {
    let sub = |trigger: &str, help: &str, arg: AutocompleteArg| AutocompleteData {
        trigger: trigger.to_owned(),
        hint: format!("[{trigger}]"),
        help_text: help.to_owned(),
        role_id: "system_user".to_owned(),
        arguments: vec![arg],
        sub_commands: Vec::new(),
    };
    AutocompleteData {
        trigger: "hookrec".to_owned(),
        hint: "[verb]".to_owned(),
        help_text: "The hook recorder's command".to_owned(),
        role_id: "system_user".to_owned(),
        arguments: Vec::new(),
        sub_commands: vec![
            sub(
                "ephemeral",
                "Answer only to you",
                AutocompleteArg {
                    help_text: "what to say".to_owned(),
                    r#type: "TextInput".to_owned(),
                    required: true,
                    data: interface(
                        registered::AUTOCOMPLETE_TEXT_ARG,
                        &AutocompleteTextArg {
                            hint: "text".to_owned(),
                            pattern: ".*".to_owned(),
                        },
                    ),
                    ..AutocompleteArg::default()
                },
            ),
            sub(
                "pick",
                "Pick one",
                AutocompleteArg {
                    help_text: "a choice".to_owned(),
                    r#type: "StaticList".to_owned(),
                    required: true,
                    data: interface(
                        registered::AUTOCOMPLETE_STATIC_LIST_ARG,
                        &AutocompleteStaticListArg {
                            possible_arguments: vec![
                                AutocompleteListItem {
                                    item: "alpha".to_owned(),
                                    hint: "the first".to_owned(),
                                    help_text: "Alpha".to_owned(),
                                },
                                AutocompleteListItem {
                                    item: "beta".to_owned(),
                                    hint: "the second".to_owned(),
                                    help_text: "Beta".to_owned(),
                                },
                            ],
                        },
                    ),
                    ..AutocompleteArg::default()
                },
            ),
            sub(
                "fetch",
                "Ask the plugin",
                AutocompleteArg {
                    help_text: "from the plugin".to_owned(),
                    r#type: "DynamicList".to_owned(),
                    required: true,
                    data: interface(
                        registered::AUTOCOMPLETE_DYNAMIC_LIST_ARG,
                        &AutocompleteDynamicListArg {
                            fetch_url: "suggest/fetch".to_owned(),
                        },
                    ),
                    ..AutocompleteArg::default()
                },
            ),
        ],
    }
}

/// What the recorder registers on activation, in order: `/hookrec` with its tree, `/shrug` —
/// the built-in's trigger, which a plugin overrides — a mixed-case trigger with no tree, one
/// scoped to the suite's team, and two the host must refuse.
fn registrations() -> Vec<Command> {
    let team = std::env::var(TEAM_VAR).unwrap_or_default();
    vec![
        Command {
            trigger: "hookrec".to_owned(),
            auto_complete: true,
            auto_complete_desc: "Drive the hook recorder".to_owned(),
            auto_complete_hint: "[verb] [text]".to_owned(),
            display_name: "Hook Recorder".to_owned(),
            // Dropped by the host's copy.
            username: "not-kept".to_owned(),
            icon_url: "https://example.com/not-kept.png".to_owned(),
            autocomplete_data: Some(Box::new(hookrec_autocomplete())),
            autocomplete_icon_data: "PHN2Zy8+".to_owned(),
            ..Command::default()
        },
        Command {
            trigger: "shrug".to_owned(),
            display_name: "The recorder's shrug".to_owned(),
            ..Command::default()
        },
        Command {
            trigger: "HookRecCase".to_owned(),
            auto_complete: true,
            auto_complete_desc: "Mixed case".to_owned(),
            auto_complete_hint: "[nothing]".to_owned(),
            ..Command::default()
        },
        Command {
            trigger: "hookrecteam".to_owned(),
            team_id: team,
            auto_complete: true,
            auto_complete_desc: "One team's".to_owned(),
            ..Command::default()
        },
        Command::default(),
        Command {
            trigger: "hookrecbad".to_owned(),
            autocomplete_data: Some(Box::new(AutocompleteData {
                trigger: "HookRecBad".to_owned(),
                ..AutocompleteData::default()
            })),
            ..Command::default()
        },
    ]
}

/// `OnActivate`: every registration, written down with what the host answered.
pub async fn on_activate(api: &Client) -> Vec<Json> {
    let mut out = Vec::new();
    for command in registrations() {
        let _: Option<Z_RegisterCommandReturns> = call(
            api,
            &mut out,
            "RegisterCommand",
            Z_RegisterCommandArgs {
                a: Some(Box::new(command)),
            },
        )
        .await;
    }
    out
}

fn respond(response_type: &str, text: &str) -> CommandResponse {
    CommandResponse {
        response_type: response_type.to_owned(),
        text: text.to_owned(),
        ..CommandResponse::default()
    }
}

fn refuse(id: &str, skip_translation: bool, status_code: i64) -> Z_ExecuteCommandReturns {
    Z_ExecuteCommandReturns {
        a: None,
        b: Some(Box::new(AppError {
            id: id.to_owned(),
            message: "the hook recorder refuses this command".to_owned(),
            detailed_error: "refused by the hook recorder".to_owned(),
            status_code,
            r#where: "HookRecorder.ExecuteCommand".to_owned(),
            skip_translation,
            ..AppError::default()
        })),
    }
}

/// `ExecuteCommand`'s answer to `args` — the table in the module docs. `script` is the caller's.
pub fn answer(args: &CommandArgs) -> Z_ExecuteCommandReturns {
    let rest = args.command.split_once(' ').map_or("", |(_, rest)| rest);
    let (verb, text) = rest.split_once(' ').unwrap_or((rest, ""));
    let ok = |response: CommandResponse| Z_ExecuteCommandReturns {
        a: Some(Box::new(response)),
        b: None,
    };
    match verb {
        "ephemeral" => ok(respond("ephemeral", text)),
        "in_channel" => ok(respond("in_channel", text)),
        "plain" => ok(respond("", text)),
        "skip" => ok(CommandResponse {
            skip_slack_parsing: true,
            ..respond("ephemeral", text)
        }),
        "props" => ok(CommandResponse {
            props: std::collections::HashMap::from([
                (
                    "from_recorder".to_owned(),
                    Some(gobwire::Interface::string("yes")),
                ),
                ("count".to_owned(), Some(gobwire::Interface::float64(3.0))),
            ]),
            ..respond("ephemeral", "with props")
        }),
        "goto" => ok(CommandResponse {
            goto_location: "https://example.com/hookrec".to_owned(),
            ..CommandResponse::default()
        }),
        "extra" => ok(CommandResponse {
            extra_responses: vec![
                respond("in_channel", "the second"),
                respond("ephemeral", "the third, <!here>"),
            ],
            ..respond("ephemeral", "the first")
        }),
        "forbidden" => ok(CommandResponse {
            channel_id: text.to_owned(),
            ..respond("in_channel", "somewhere else")
        }),
        "system" => ok(CommandResponse {
            r#type: "system_join_leave".to_owned(),
            ..respond("in_channel", "a system post")
        }),
        "error" => refuse("mmrs.hookrecorder.command_error", false, 418),
        "error-skip" => refuse("mmrs.hookrecorder.command_error_skip", true, 409),
        "error-status" => refuse("mmrs.hookrecorder.command_error_status", false, 0),
        "nothing" => Z_ExecuteCommandReturns::default(),
        "script" => ok(respond("ephemeral", "script done")),
        _ => ok(respond("ephemeral", &format!("hookrec: {}", args.command))),
    }
}

/// Every element's rendering, so a list in Go's map order sorts the same on both hosts.
fn sort_commands(commands: &mut [Command]) {
    commands.sort_by_key(|c| render_typed(c).to_string());
}

fn slash(args: &CommandArgs, command: &str) -> Option<Box<CommandArgs>> {
    Some(Box::new(CommandArgs {
        user_id: args.user_id.clone(),
        channel_id: args.channel_id.clone(),
        team_id: args.team_id.clone(),
        command: command.to_owned(),
        ..CommandArgs::default()
    }))
}

/// The command script, run from inside `ExecuteCommand` for `/hookrec script`: the lists, a
/// registration and its removal, and `ExecuteSlashCommand` of this plugin's own command (which
/// re-enters `ExecuteCommand`), of nothing, for a missing user, without a slash, and of the
/// plugin's refusal.
pub async fn run(api: &Client, args: &CommandArgs) -> Vec<Json> {
    let mut out = Vec::new();
    let team = args.team_id.clone();
    let _: Option<Z_ListPluginCommandsReturns> = call(
        api,
        &mut out,
        "ListPluginCommands",
        Z_ListPluginCommandsArgs { a: team.clone() },
    )
    .await;
    let _: Option<Z_ListPluginCommandsReturns> = call(
        api,
        &mut out,
        "ListPluginCommands",
        Z_ListPluginCommandsArgs { a: MISSING.into() },
    )
    .await;
    let _: Option<Z_ListBuiltInCommandsReturns> = call_then(
        api,
        &mut out,
        "ListBuiltInCommands",
        Z_ListBuiltInCommandsArgs {},
        |r: &mut Z_ListBuiltInCommandsReturns| sort_commands(&mut r.a),
    )
    .await;
    let _: Option<Z_ListCustomCommandsReturns> = call(
        api,
        &mut out,
        "ListCustomCommands",
        Z_ListCustomCommandsArgs { a: team.clone() },
    )
    .await;
    let _: Option<Z_ListCommandsReturns> = call_then(
        api,
        &mut out,
        "ListCommands",
        Z_ListCommandsArgs { a: team.clone() },
        |r: &mut Z_ListCommandsReturns| sort_commands(&mut r.a),
    )
    .await;

    let _: Option<Z_RegisterCommandReturns> = call(
        api,
        &mut out,
        "RegisterCommand",
        Z_RegisterCommandArgs {
            a: Some(Box::new(Command {
                trigger: "HookRecTemp".to_owned(),
                auto_complete: true,
                ..Command::default()
            })),
        },
    )
    .await;
    let _: Option<Z_ListPluginCommandsReturns> = call(
        api,
        &mut out,
        "ListPluginCommands",
        Z_ListPluginCommandsArgs { a: team.clone() },
    )
    .await;
    let _: Option<Z_UnregisterCommandReturns> = call(
        api,
        &mut out,
        "UnregisterCommand",
        Z_UnregisterCommandArgs {
            a: String::new(),
            b: "HOOKRECTEMP".to_owned(),
        },
    )
    .await;
    let _: Option<Z_ListPluginCommandsReturns> = call(
        api,
        &mut out,
        "ListPluginCommands",
        Z_ListPluginCommandsArgs { a: team.clone() },
    )
    .await;

    for command in [
        "/hookrec ephemeral from the script, <!all>",
        "/hookrec in_channel posted by the script",
        "/hookrec error",
        "/nosuchhookrec at all",
        "hookrec without a slash",
    ] {
        let _: Option<Z_ExecuteSlashCommandReturns> = call(
            api,
            &mut out,
            "ExecuteSlashCommand",
            Z_ExecuteSlashCommandArgs {
                a: slash(args, command),
            },
        )
        .await;
    }
    let mut missing = slash(args, "/hookrec ephemeral nobody");
    if let Some(a) = missing.as_mut() {
        a.user_id = MISSING.to_owned();
    }
    let _: Option<Z_ExecuteSlashCommandReturns> = call(
        api,
        &mut out,
        "ExecuteSlashCommand",
        Z_ExecuteSlashCommandArgs { a: missing },
    )
    .await;
    out
}
