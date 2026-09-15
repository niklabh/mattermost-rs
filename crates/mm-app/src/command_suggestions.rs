//! Port of `app/command_autocomplete.go` — `GetSuggestions` and the argument parser behind it,
//! which turn a half-typed slash command into the suggestions `GET
//! /teams/{team_id}/commands/autocomplete_suggestions` answers with.
//!
//! # Where this hands over
//!
//! A **dynamic list** argument is fetched when the parser reaches it: a `builtin:` URL asks the
//! provider (`/secure-connection remove`, `/share-channel invite` — both read remote clusters and
//! permissions), any other URL is a plugin HTTP request. Neither is ported, so reaching one is
//! [`NeedsGo`] and the handler forwards the whole request. So is the one input shape where Go
//! would slice a string at a byte offset that is not a character boundary: `strings.ToLower`
//! can change a character's byte length (the Kelvin sign lowercases to ASCII `k`), and Go's
//! `namedArg[len(in):]` then indexes by the *unlowered* length — a mismatch Go survives or panics
//! on, and which this port declines rather than guesses.
//!
//! # Strings, bytes and Go's `strings` functions
//!
//! Every other offset here is at an ASCII delimiter (a space or a quote), which is always a
//! character boundary in UTF-8, so `&str` slicing at those offsets is exactly Go's byte slicing.
//! Case folding is [`mm_model::utils::go_to_lower`] throughout.

use mm_model::command::Command;
use mm_model::command_autocomplete::{
    AutocompleteArg, AutocompleteArgData, AutocompleteData, AutocompleteListItem,
    AutocompleteSuggestion, AutocompleteTextArg,
};
use mm_model::role::SYSTEM_ADMIN_ROLE_ID;
use mm_model::utils::go_to_lower;

/// The parser reached something only Go can answer — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedsGo;

/// What each parse step hands to the next: `(found, alreadyParsed, yetToBeParsed, suggestions)`.
///
/// Owned strings, because Go passes these by value and every branch builds a new one from pieces
/// of the old — there is no borrowed form that outlives the concatenation.
type Parsed = (bool, String, String, Vec<AutocompleteSuggestion>);

/// `strings.TrimPrefix(s, " ")` — at most one space.
fn trim_one_space(s: &str) -> &str {
    s.strip_prefix(' ').unwrap_or(s)
}

fn suggestion(
    complete: String,
    suggestion: &str,
    hint: &str,
    description: &str,
) -> AutocompleteSuggestion {
    AutocompleteSuggestion {
        complete,
        suggestion: suggestion.to_owned(),
        hint: hint.to_owned(),
        description: description.to_owned(),
        icon_data: String::new(),
    }
}

/// Port of `App.GetSuggestions` (command_autocomplete.go:25).
///
/// Sorts `commands` by lower-cased trigger — `sort.Slice`, which is not stable; two triggers can
/// only tie by differing in case, and then Go's order is not an order either — gives a command
/// with no autocomplete data a flat one from its hint and description, parses, and copies each
/// suggestion's icon from the first command whose trigger prefixes its completion.
pub fn get_suggestions(
    commands: &mut [Command],
    user_input: &str,
    role_id: &str,
) -> Result<Vec<AutocompleteSuggestion>, NeedsGo> {
    commands.sort_by_key(|command| go_to_lower(&command.trigger));

    let data: Vec<AutocompleteData> = commands
        .iter()
        .map(|command| {
            command.autocomplete_data.clone().unwrap_or_else(|| {
                AutocompleteData::new(
                    command.trigger.as_str(),
                    command.auto_complete_hint.as_str(),
                    command.auto_complete_desc.as_str(),
                )
            })
        })
        .collect();

    let mut suggestions = suggestions_for(&data, "", user_input, role_id)?;
    for suggestion in &mut suggestions {
        if let Some(command) = commands
            .iter()
            .find(|command| suggestion.complete.starts_with(&command.trigger))
        {
            suggestion
                .icon_data
                .clone_from(&command.autocomplete_icon_data);
        }
    }
    Ok(suggestions)
}

/// Port of `App.getSuggestions` (command_autocomplete.go:52).
fn suggestions_for(
    commands: &[AutocompleteData],
    parsed: &str,
    to_be_parsed: &str,
    role_id: &str,
) -> Result<Vec<AutocompleteSuggestion>, NeedsGo> {
    let mut suggestions = Vec::new();

    let Some(index) = to_be_parsed.find(' ') else {
        let lowered = go_to_lower(to_be_parsed);
        for command in commands {
            if command.trigger.starts_with(&lowered)
                && (command.role_id == role_id
                    || role_id == SYSTEM_ADMIN_ROLE_ID
                    || role_id.is_empty())
            {
                suggestions.push(suggestion(
                    format!("{parsed}{}", command.trigger),
                    &command.trigger,
                    &command.hint,
                    &command.help_text,
                ));
            }
        }
        return Ok(suggestions);
    };

    let word = go_to_lower(&to_be_parsed[..index]);
    for command in commands {
        if command.trigger != word {
            continue;
        }
        if !role_id.is_empty() && role_id != SYSTEM_ADMIN_ROLE_ID && role_id != command.role_id {
            continue;
        }
        let rest = &to_be_parsed[index + 1..];
        let now_parsed = format!("{parsed}{}", &to_be_parsed[..=index]);

        if command.arguments_slice().is_empty() {
            // Seek recursively in subcommands.
            suggestions.extend(suggestions_for(
                command.sub_commands_slice(),
                &now_parsed,
                rest,
                role_id,
            )?);
            continue;
        }

        let (found, _, _, found_suggestions) =
            parse_arguments(command.arguments_slice(), now_parsed, rest.to_owned())?;
        if found {
            suggestions.extend(found_suggestions);
        }
    }

    Ok(suggestions)
}

/// Port of `App.parseArguments` (command_autocomplete.go:96).
///
/// A required argument is parsed and, if nothing was suggested, the rest are parsed after it. An
/// optional one is parsed **both ways** — as present and as absent — and the two suggestion sets
/// are combined; when neither suggests anything, the branch that consumed input wins.
fn parse_arguments(
    args: &[AutocompleteArg],
    parsed: String,
    to_be_parsed: String,
) -> Result<Parsed, NeedsGo> {
    let Some((first, others)) = args.split_first() else {
        return Ok((false, parsed, to_be_parsed, Vec::new()));
    };

    if first.required {
        let (found, changed_parsed, changed_to_be_parsed, found_suggestions) =
            parse_argument(first, parsed, to_be_parsed)?;
        if found {
            return Ok((
                true,
                changed_parsed,
                changed_to_be_parsed,
                found_suggestions,
            ));
        }
        return parse_arguments(others, changed_parsed, changed_to_be_parsed);
    }

    let mut suggestions = Vec::new();

    // The optional argument as present. `parsed`/`to_be_parsed` are needed again below for the
    // absent branch, so this branch takes copies.
    let (mut found_with, mut parsed_with, mut to_be_parsed_with, with_suggestions) =
        parse_argument(first, parsed.clone(), to_be_parsed.clone())?;
    if found_with {
        suggestions.extend(with_suggestions);
    } else {
        let (found_rest, parsed_rest, to_be_parsed_rest, rest_suggestions) =
            parse_arguments(others, parsed_with, to_be_parsed_with)?;
        if found_rest {
            suggestions.extend(rest_suggestions);
        }
        found_with = found_rest;
        parsed_with = parsed_rest;
        to_be_parsed_with = to_be_parsed_rest;
    }

    // The optional argument as absent — copies again, for the comparisons after it.
    let (found_without, parsed_without, to_be_parsed_without, without_suggestions) =
        parse_arguments(others, parsed.clone(), to_be_parsed.clone())?;
    if found_without {
        suggestions.extend(without_suggestions);
    }

    if found_with || found_without {
        return Ok((
            true,
            format!("{parsed}{to_be_parsed}"),
            String::new(),
            suggestions,
        ));
    }

    if parsed_with != parsed && to_be_parsed_with != to_be_parsed {
        return Ok((false, parsed_with, to_be_parsed_with, suggestions));
    }

    Ok((
        found_without,
        parsed_without,
        to_be_parsed_without,
        suggestions,
    ))
}

/// Port of `App.parseArgument` (command_autocomplete.go:143).
fn parse_argument(
    arg: &AutocompleteArg,
    mut parsed: String,
    mut to_be_parsed: String,
) -> Result<Parsed, NeedsGo> {
    if !arg.name.is_empty() {
        let (found, changed_parsed, changed_to_be_parsed, named) =
            parse_named_argument(arg, &parsed, &to_be_parsed)?;
        if found {
            return Ok((
                true,
                changed_parsed,
                changed_to_be_parsed,
                named.into_iter().collect(),
            ));
        }
        if changed_to_be_parsed.is_empty() {
            return Ok((true, changed_parsed, changed_to_be_parsed, Vec::new()));
        }
        parsed = changed_parsed;
        to_be_parsed = if changed_to_be_parsed == " " {
            String::new()
        } else {
            changed_to_be_parsed
        };
    }

    match &arg.data {
        AutocompleteArgData::Text(data) => {
            let (found, changed_parsed, changed_to_be_parsed, text) =
                parse_input_text_argument(arg, data, &parsed, &to_be_parsed);
            if found {
                return Ok((
                    true,
                    changed_parsed,
                    changed_to_be_parsed,
                    text.into_iter().collect(),
                ));
            }
            parsed = changed_parsed;
            to_be_parsed = changed_to_be_parsed;
        }
        AutocompleteArgData::StaticList(data) => {
            let (found, changed_parsed, changed_to_be_parsed, list) = parse_list_items(
                data.possible_arguments.as_deref().unwrap_or(&[]),
                &parsed,
                &to_be_parsed,
            )?;
            if found {
                return Ok((true, changed_parsed, changed_to_be_parsed, list));
            }
            parsed = changed_parsed;
            to_be_parsed = changed_to_be_parsed;
        }
        // `getDynamicListArgument` — a provider or a plugin answers. Not ported.
        AutocompleteArgData::DynamicList(_) => return Err(NeedsGo),
        AutocompleteArgData::None => {}
    }

    Ok((false, parsed, to_be_parsed, Vec::new()))
}

/// Port of `parseNamedArgument` (command_autocomplete.go:195).
fn parse_named_argument(
    arg: &AutocompleteArg,
    parsed: &str,
    to_be_parsed: &str,
) -> Result<(bool, String, String, Option<AutocompleteSuggestion>), NeedsGo> {
    let input = trim_one_space(to_be_parsed);
    let named = format!("--{}", arg.name);

    if input.is_empty() {
        // The user has not started typing the argument.
        return Ok((
            true,
            format!("{parsed}{to_be_parsed}"),
            String::new(),
            Some(suggestion(
                format!("{parsed}{to_be_parsed}{named} "),
                &named,
                "",
                &arg.help_text,
            )),
        ));
    }

    let lowered_input = go_to_lower(input);
    let lowered_named = go_to_lower(&named);
    if lowered_named.starts_with(&lowered_input) {
        // `namedArg[len(in):]` — by the *unlowered* input's byte length.
        let rest = named.get(input.len()..).ok_or(NeedsGo)?;
        return Ok((
            true,
            format!("{parsed}{to_be_parsed}"),
            String::new(),
            Some(suggestion(
                format!("{parsed}{to_be_parsed}{rest} "),
                &named,
                "",
                &arg.help_text,
            )),
        ));
    }

    let named_and_space = format!("{lowered_named} ");
    if !lowered_input.starts_with(&named_and_space) {
        return Ok((
            false,
            format!("{parsed}{to_be_parsed}"),
            String::new(),
            None,
        ));
    }
    if lowered_input == named_and_space {
        return Ok((false, format!("{parsed}{named} "), " ".to_owned(), None));
    }
    let rest = input.get(named.len() + 1..).ok_or(NeedsGo)?;
    Ok((false, format!("{parsed}{named} "), rest.to_owned(), None))
}

/// Port of `parseInputTextArgument` (command_autocomplete.go:214).
fn parse_input_text_argument(
    arg: &AutocompleteArg,
    data: &AutocompleteTextArg,
    parsed: &str,
    to_be_parsed: &str,
) -> (bool, String, String, Option<AutocompleteSuggestion>) {
    let input = trim_one_space(to_be_parsed);
    let unfinished = || {
        (
            true,
            format!("{parsed}{to_be_parsed}"),
            String::new(),
            Some(suggestion(
                format!("{parsed}{to_be_parsed}"),
                "",
                &data.hint,
                &arg.help_text,
            )),
        )
    };

    if input.is_empty() {
        // The user has not started typing the argument.
        return unfinished();
    }

    if let Some(after_quote) = input.strip_prefix('"') {
        // Input with multiple words.
        let Some(second_quote) = after_quote.find('"') else {
            return unfinished();
        };
        // This argument is typed already.
        let mut offset = 2;
        if input.as_bytes().get(second_quote + 2) == Some(&b' ') {
            offset += 1;
        }
        return (
            false,
            format!("{parsed}{}", &input[..second_quote + offset]),
            input[second_quote + offset..].to_owned(),
            None,
        );
    }

    // Input with a single word.
    let Some(space) = input.find(' ') else {
        return unfinished();
    };
    (
        false,
        format!("{parsed}{}", &input[..=space]),
        input[space + 1..].to_owned(),
        None,
    )
}

/// Port of `parseListItems` (command_autocomplete.go:290) — reached here only through a static
/// list, which no built-in command declares.
fn parse_list_items(
    items: &[AutocompleteListItem],
    parsed: &str,
    to_be_parsed: &str,
) -> Result<Parsed, NeedsGo> {
    let input = trim_one_space(to_be_parsed);
    let lowered_input = go_to_lower(input);

    let mut max_prefix = String::new();
    for item in items {
        let item_and_space = format!("{} ", go_to_lower(&item.item));
        if lowered_input.starts_with(&item_and_space) && max_prefix.len() < item.item.len() + 1 {
            max_prefix = format!("{} ", item.item);
        }
    }
    if !max_prefix.is_empty() {
        // Typing of an argument finished. `in[:len(maxPrefix)]`, by byte length.
        let head = input.get(..max_prefix.len()).ok_or(NeedsGo)?;
        let tail = input.get(max_prefix.len()..).ok_or(NeedsGo)?;
        return Ok((
            false,
            format!("{parsed}{head}"),
            tail.to_owned(),
            Vec::new(),
        ));
    }

    // The user has not finished typing the argument.
    let suggestions = items
        .iter()
        .filter(|item| go_to_lower(&item.item).starts_with(&lowered_input))
        .map(|item| {
            suggestion(
                format!("{parsed}{}", item.item),
                &item.item,
                &item.hint,
                &item.help_text,
            )
        })
        .collect();
    Ok((
        true,
        format!("{parsed}{to_be_parsed}"),
        String::new(),
        suggestions,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::role::SYSTEM_USER_ROLE_ID;

    fn command(trigger: &str, data: Option<AutocompleteData>) -> Command {
        Command {
            trigger: trigger.to_owned(),
            auto_complete: true,
            auto_complete_hint: format!("[{trigger} hint]"),
            auto_complete_desc: format!("{trigger} description"),
            autocomplete_data: data,
            ..Command::default()
        }
    }

    fn completes(suggestions: &[AutocompleteSuggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.complete.as_str()).collect()
    }

    /// A flat command lists by prefix, sorted by lower-cased trigger, case-insensitively.
    #[test]
    fn a_prefix_lists_matching_triggers_in_trigger_order() {
        let mut commands = vec![
            command("zed", None),
            command("alpha", None),
            command("alps", None),
        ];
        let got = get_suggestions(&mut commands, "AL", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["alpha", "alps"]);
        assert_eq!(got[0].hint, "[alpha hint]");
        assert_eq!(got[0].description, "alpha description");
    }

    /// A subcommand tree is walked, and a text argument yields its hint with the argument's help.
    #[test]
    fn subcommands_and_text_arguments_are_walked() {
        let mut root = AutocompleteData::new("tool", "[action]", "the tool");
        let mut make = AutocompleteData::new("make", "", "make a thing");
        make.add_text_argument("what to make", "[thing]", "");
        root.add_command(make);
        let mut commands = vec![command("tool", Some(root))];

        let got = get_suggestions(&mut commands, "tool ", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool make"]);
        let got = get_suggestions(&mut commands, "tool make ", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool make "]);
        assert_eq!(
            (got[0].hint.as_str(), got[0].description.as_str()),
            ("[thing]", "what to make")
        );
        let got =
            get_suggestions(&mut commands, "tool make \"two words", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool make \"two words"]);
        let got = get_suggestions(&mut commands, "tool make done ", SYSTEM_USER_ROLE_ID).unwrap();
        assert!(got.is_empty());
    }

    /// A named argument is offered as `--name`, completed from a partial, and consumed when typed.
    #[test]
    fn named_arguments_are_offered_completed_and_consumed() {
        let mut root = AutocompleteData::new("tool", "", "");
        root.add_named_text_argument("name", "the name", "[name]", "", true);
        root.add_named_text_argument("colour", "the colour", "[colour]", "", false);
        let mut commands = vec![command("tool", Some(root))];

        let got = get_suggestions(&mut commands, "tool ", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool --name "]);
        let got = get_suggestions(&mut commands, "tool --NA", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool --NAme "]);
        let got = get_suggestions(&mut commands, "tool --name bob ", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool --name bob --colour "]);
    }

    /// A role other than the command's hides it from a plain user but not from an admin.
    #[test]
    fn a_role_restricted_command_is_the_admins_only() {
        let mut data = AutocompleteData::new("secret", "", "");
        data.role_id = SYSTEM_ADMIN_ROLE_ID.to_owned();
        let mut commands = vec![command("secret", Some(data))];
        assert!(
            get_suggestions(&mut commands, "sec", SYSTEM_USER_ROLE_ID)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            get_suggestions(&mut commands, "sec", SYSTEM_ADMIN_ROLE_ID)
                .unwrap()
                .len(),
            1
        );
    }

    /// Reaching a dynamic list is Go's. A length-changing case fold is reproduced byte for byte
    /// while Go's offset stays in range — the Kelvin sign is 3 bytes and lowers to `k`, so
    /// `"--kind"[len("--\u{212A}"):]` is `"d"`, and Go completes to `--\u{212A}d` just as this does
    /// — and is Go's once the offset runs past the argument name, where Go's slice would panic.
    #[test]
    fn a_dynamic_list_or_an_out_of_range_fold_needs_go() {
        let mut root = AutocompleteData::new("tool", "", "");
        root.add_named_dynamic_list_argument("id", "which", "builtin:tool", true);
        let mut commands = vec![command("tool", Some(root))];
        assert_eq!(
            get_suggestions(&mut commands, "tool --id ", SYSTEM_USER_ROLE_ID),
            Err(NeedsGo)
        );

        let mut root = AutocompleteData::new("tool", "", "");
        root.add_named_text_argument("kind", "k", "[k]", "", true);
        let mut commands = vec![command("tool", Some(root))];
        let got = get_suggestions(&mut commands, "tool --\u{212A}", SYSTEM_USER_ROLE_ID).unwrap();
        assert_eq!(completes(&got), ["tool --\u{212A}d "]);
        assert_eq!(
            get_suggestions(&mut commands, "tool --\u{212A}in", SYSTEM_USER_ROLE_ID),
            Err(NeedsGo)
        );
    }

    /// A static list: partial input suggests matching items; a finished item is consumed.
    #[test]
    fn a_static_list_suggests_and_consumes_items() {
        let items = vec![
            AutocompleteListItem {
                item: "red".into(),
                hint: "h".into(),
                help_text: "r".into(),
            },
            AutocompleteListItem {
                item: "rose".into(),
                hint: "h".into(),
                help_text: "o".into(),
            },
        ];
        let got = parse_list_items(&items, "tool ", "r").unwrap();
        assert!(got.0);
        assert_eq!(completes(&got.3), ["tool red", "tool rose"]);
        let got = parse_list_items(&items, "tool ", "red more").unwrap();
        assert_eq!(
            (got.0, got.1.as_str(), got.2.as_str()),
            (false, "tool red ", "more")
        );
    }
}
