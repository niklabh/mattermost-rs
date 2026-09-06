//! Port of `model/command_autocomplete.go` — the tree a slash command publishes so the client can
//! autocomplete it.
//!
//! # Not one `json:` tag in the file
//!
//! Every wire key here is a Go **field name**: `Trigger`, `HelpText`, `RoleID`, `SubCommands`,
//! `PossibleArguments`, `FetchURL`, `IconData`. Renaming any of them to snake_case — which every
//! other model in this crate uses — is a wire break, and it is the single most likely mistake in
//! this file. `RoleID` in particular is neither `role_id` nor `RoleId`.
//!
//! # `Data` is a discriminated union keyed by `Type`
//!
//! `AutocompleteArg.Data` is a bare `any` holding one of three pointers, and Go hand-writes
//! `UnmarshalJSON` to pick the right one from the sibling `Type` field. That is a tagged union
//! with an external tag, so it is an enum here ([`AutocompleteArgData`]) with hand-written serde
//! on the containing struct — `#[serde(tag = ...)]` cannot express it, because the tag lives
//! outside the payload and the payload is not nested under a key of its own.
//!
//! Go leaves `Data` **nil** when `Type` is none of the three known values, and does not error —
//! so an unknown argument type decodes successfully with no data. [`AutocompleteArgData::None`]
//! is that state.

use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::go_url::{GoUrl, go_parse};
use crate::role::{SYSTEM_ADMIN_ROLE_ID, SYSTEM_USER_ROLE_ID};
use crate::utils::go_to_lower;

/// Port of `model.AutocompleteArgType` (command_autocomplete.go:18) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AutocompleteArgType(pub String);

impl AutocompleteArgType {
    /// `AutocompleteArgTypeText` (command_autocomplete.go:22). Note the **value** is `TextInput`,
    /// not `Text`.
    pub const TEXT: &'static str = "TextInput";
    /// `AutocompleteArgTypeStaticList` (command_autocomplete.go:23).
    pub const STATIC_LIST: &'static str = "StaticList";
    /// `AutocompleteArgTypeDynamicList` (command_autocomplete.go:24).
    pub const DYNAMIC_LIST: &'static str = "DynamicList";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AutocompleteArgType {
    fn from(s: &str) -> Self {
        AutocompleteArgType(s.to_string())
    }
}

/// Port of `model.AutocompleteTextArg` (command_autocomplete.go:60).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompleteTextArg {
    #[serde(rename = "Hint")]
    pub hint: String,

    /// A regex the input must match. **Nothing here compiles or validates it** — an invalid
    /// pattern is the client's problem.
    #[serde(rename = "Pattern")]
    pub pattern: String,
}

/// Port of `model.AutocompleteListItem` (command_autocomplete.go:68).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompleteListItem {
    #[serde(rename = "Item")]
    pub item: String,

    #[serde(rename = "Hint")]
    pub hint: String,

    #[serde(rename = "HelpText")]
    pub help_text: String,
}

/// Port of `model.AutocompleteStaticListArg` (command_autocomplete.go:76).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompleteStaticListArg {
    /// `[]AutocompleteListItem` in Go, so a nil list is `null` on the wire and an empty one is
    /// `[]`. Modelled `Option` for that reason — a plain `Vec` cannot decode the `null` a real
    /// command carries.
    #[serde(rename = "PossibleArguments")]
    pub possible_arguments: Option<Vec<AutocompleteListItem>>,
}

/// Port of `model.AutocompleteDynamicListArg` (command_autocomplete.go:81).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutocompleteDynamicListArg {
    #[serde(rename = "FetchURL")]
    pub fetch_url: String,
}

/// The three shapes `AutocompleteArg.Data` can hold, plus Go's nil.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AutocompleteArgData {
    Text(AutocompleteTextArg),
    StaticList(AutocompleteStaticListArg),
    DynamicList(AutocompleteDynamicListArg),
    /// `Type` was none of the three known values, so Go's `UnmarshalJSON` left `Data` nil.
    #[default]
    None,
}

/// Port of `model.AutocompleteSuggestion` (command_autocomplete.go:90) — one row in the client's
/// suggestion box.
///
/// For input `/jira cre`: `complete` is `/jira create`, `suggestion` is `create`, `hint` is
/// `[issue text]`, `description` is `Create a new Issue`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutocompleteSuggestion {
    #[serde(rename = "Complete")]
    pub complete: String,

    #[serde(rename = "Suggestion")]
    pub suggestion: String,

    #[serde(rename = "Hint")]
    pub hint: String,

    #[serde(rename = "Description")]
    pub description: String,

    /// A base64-encoded SVG **as a string**, so no `[]byte` special case applies.
    #[serde(rename = "IconData")]
    pub icon_data: String,
}

/// Port of `model.AutocompleteData` (command_autocomplete.go:28).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutocompleteData {
    #[serde(rename = "Trigger")]
    pub trigger: String,

    #[serde(rename = "Hint")]
    pub hint: String,

    #[serde(rename = "HelpText")]
    pub help_text: String,

    /// The role a user must hold to see this command. `RoleID` on the wire.
    #[serde(rename = "RoleID")]
    pub role_id: String,

    /// Named or positional, never mixed — see [`AutocompleteData::is_valid`].
    ///
    /// `Option` because Go's `[]*AutocompleteArg` is nil on any command that declares none, and
    /// `null` is what reaches the wire — `NewAutocompleteData` allocates an empty slice, but a
    /// command built any other way does not.
    #[serde(rename = "Arguments")]
    pub arguments: Option<Vec<AutocompleteArg>>,

    #[serde(rename = "SubCommands")]
    pub sub_commands: Option<Vec<AutocompleteData>>,
}

/// Port of `model.AutocompleteArg` (command_autocomplete.go:47).
///
/// An empty `name` makes the argument **positional**; a non-empty one makes it named, passed as
/// `--name value`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutocompleteArg {
    pub name: String,
    pub help_text: String,
    pub type_: AutocompleteArgType,
    pub required: bool,
    pub data: AutocompleteArgData,
}

impl Serialize for AutocompleteArg {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("AutocompleteArg", 5)?;
        st.serialize_field("Name", &self.name)?;
        st.serialize_field("HelpText", &self.help_text)?;
        st.serialize_field("Type", &self.type_)?;
        st.serialize_field("Required", &self.required)?;
        match &self.data {
            AutocompleteArgData::Text(d) => st.serialize_field("Data", d)?,
            AutocompleteArgData::StaticList(d) => st.serialize_field("Data", d)?,
            AutocompleteArgData::DynamicList(d) => st.serialize_field("Data", d)?,
            // A nil `any` marshals as `null`, and the key is still written.
            AutocompleteArgData::None => st.serialize_field("Data", &())?,
        }
        st.end()
    }
}

/// The shape Go's `UnmarshalJSON` reads before dispatching on `Type`.
#[derive(Deserialize)]
struct AutocompleteArgWire {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "HelpText")]
    help_text: String,
    #[serde(rename = "Type")]
    type_: String,
    #[serde(rename = "Required")]
    required: bool,
    #[serde(rename = "Data")]
    data: serde_json::Value,
}

impl<'de> Deserialize<'de> for AutocompleteArg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = AutocompleteArgWire::deserialize(d)?;

        // Go dispatches on `Type` and errors when the payload does not fit; an unrecognised
        // `Type` leaves `Data` nil without an error.
        let data = match wire.type_.as_str() {
            AutocompleteArgType::TEXT => AutocompleteArgData::Text(
                serde_json::from_value(wire.data).map_err(DeError::custom)?,
            ),
            AutocompleteArgType::STATIC_LIST => AutocompleteArgData::StaticList(
                serde_json::from_value(wire.data).map_err(DeError::custom)?,
            ),
            AutocompleteArgType::DYNAMIC_LIST => AutocompleteArgData::DynamicList(
                serde_json::from_value(wire.data).map_err(DeError::custom)?,
            ),
            _ => AutocompleteArgData::None,
        };

        Ok(AutocompleteArg {
            name: wire.name,
            help_text: wire.help_text,
            type_: AutocompleteArgType(wire.type_),
            required: wire.required,
            data,
        })
    }
}

impl AutocompleteArg {
    /// Port of `(*AutocompleteArg).Equals` (command_autocomplete.go:290).
    ///
    /// Go compares `Data` with `reflect.DeepEqual`; the derived `PartialEq` on
    /// [`AutocompleteArgData`] is the same comparison.
    pub fn equals(&self, other: &AutocompleteArg) -> bool {
        self == other
    }
}

impl AutocompleteData {
    /// Port of `model.NewAutocompleteData` (command_autocomplete.go:104).
    ///
    /// Note the default role is `system_user`, **not** empty — so a command built any other way
    /// is visible to everyone while one built here is not.
    pub fn new(
        trigger: impl Into<String>,
        hint: impl Into<String>,
        help_text: impl Into<String>,
    ) -> Self {
        Self {
            trigger: trigger.into(),
            hint: hint.into(),
            help_text: help_text.into(),
            role_id: SYSTEM_USER_ROLE_ID.to_string(),
            // Go allocates both, so a command built here serialises `[]`, not `null`.
            arguments: Some(Vec::new()),
            sub_commands: Some(Vec::new()),
        }
    }

    /// Port of `(*AutocompleteData).AddCommand` (command_autocomplete.go:116).
    pub fn add_command(&mut self, command: AutocompleteData) {
        self.sub_commands.get_or_insert_with(Vec::new).push(command);
    }

    /// The arguments as a slice, with nil and empty collapsed — every reader wants that.
    pub fn arguments_slice(&self) -> &[AutocompleteArg] {
        self.arguments.as_deref().unwrap_or(&[])
    }

    /// The subcommands as a slice.
    pub fn sub_commands_slice(&self) -> &[AutocompleteData] {
        self.sub_commands.as_deref().unwrap_or(&[])
    }

    /// Port of `(*AutocompleteData).AddTextArgument` (command_autocomplete.go:121) — positional,
    /// and always `required: true` because a positional optional argument is rejected by
    /// [`Self::is_valid`].
    pub fn add_text_argument(&mut self, help_text: &str, hint: &str, pattern: &str) {
        self.add_named_text_argument("", help_text, hint, pattern, true);
    }

    /// Port of `(*AutocompleteData).AddNamedTextArgument` (command_autocomplete.go:126).
    pub fn add_named_text_argument(
        &mut self,
        name: &str,
        help_text: &str,
        hint: &str,
        pattern: &str,
        required: bool,
    ) {
        self.arguments
            .get_or_insert_with(Vec::new)
            .push(AutocompleteArg {
                name: name.to_string(),
                help_text: help_text.to_string(),
                type_: AutocompleteArgType::TEXT.into(),
                required,
                data: AutocompleteArgData::Text(AutocompleteTextArg {
                    hint: hint.to_string(),
                    pattern: pattern.to_string(),
                }),
            });
    }

    /// Port of `(*AutocompleteData).AddStaticListArgument` (command_autocomplete.go:138).
    pub fn add_static_list_argument(
        &mut self,
        help_text: &str,
        required: bool,
        items: Vec<AutocompleteListItem>,
    ) {
        self.add_named_static_list_argument("", help_text, required, items);
    }

    /// Port of `(*AutocompleteData).AddNamedStaticListArgument` (command_autocomplete.go:143).
    pub fn add_named_static_list_argument(
        &mut self,
        name: &str,
        help_text: &str,
        required: bool,
        items: Vec<AutocompleteListItem>,
    ) {
        self.arguments
            .get_or_insert_with(Vec::new)
            .push(AutocompleteArg {
                name: name.to_string(),
                help_text: help_text.to_string(),
                type_: AutocompleteArgType::STATIC_LIST.into(),
                required,
                data: AutocompleteArgData::StaticList(AutocompleteStaticListArg {
                    possible_arguments: Some(items),
                }),
            });
    }

    /// Port of `(*AutocompleteData).AddDynamicListArgument` (command_autocomplete.go:155).
    pub fn add_dynamic_list_argument(&mut self, help_text: &str, url: &str, required: bool) {
        self.add_named_dynamic_list_argument("", help_text, url, required);
    }

    /// Port of `(*AutocompleteData).AddNamedDynamicListArgument` (command_autocomplete.go:160).
    pub fn add_named_dynamic_list_argument(
        &mut self,
        name: &str,
        help_text: &str,
        url: &str,
        required: bool,
    ) {
        self.arguments
            .get_or_insert_with(Vec::new)
            .push(AutocompleteArg {
                name: name.to_string(),
                help_text: help_text.to_string(),
                type_: AutocompleteArgType::DYNAMIC_LIST.into(),
                required,
                data: AutocompleteArgData::DynamicList(AutocompleteDynamicListArg {
                    fetch_url: url.to_string(),
                }),
            });
    }

    /// Port of `(*AutocompleteData).Equals` (command_autocomplete.go:172).
    ///
    /// **`Trigger`, `HelpText`, `RoleID` and `Hint` only** — plus the two child lists, compared
    /// pairwise. Two commands that differ in nothing else are equal.
    pub fn equals(&self, other: &AutocompleteData) -> bool {
        if !(self.trigger == other.trigger
            && self.help_text == other.help_text
            && self.role_id == other.role_id
            && self.hint == other.hint)
        {
            return false;
        }
        if self.arguments_slice().len() != other.arguments_slice().len()
            || self.sub_commands_slice().len() != other.sub_commands_slice().len()
        {
            return false;
        }
        for (a, b) in self
            .arguments_slice()
            .iter()
            .zip(other.arguments_slice().iter())
        {
            if !a.equals(b) {
                return false;
            }
        }
        for (a, b) in self
            .sub_commands_slice()
            .iter()
            .zip(other.sub_commands_slice().iter())
        {
            if !a.equals(b) {
                return false;
            }
        }
        true
    }

    /// Port of `(*AutocompleteData).UpdateRelativeURLsForPluginCommands`
    /// (command_autocomplete.go:192).
    ///
    /// Rewrites every relative `FetchURL` to be absolute against `base_url`, recursively. Two
    /// details are load-bearing:
    ///
    /// - Go assigns `absURL.Path` **directly** rather than through `setPath`, so `RawPath` is
    ///   whatever it was copied from `baseURL`. `String()` then re-escapes only if that `RawPath`
    ///   is not a valid encoding of the new `Path`. Reproduced by cloning and assigning the field.
    /// - "Relative" is `!IsAbs()`, i.e. **no scheme** — so `//host/path` counts as relative and
    ///   gets joined onto the base path.
    pub fn update_relative_urls_for_plugin_commands(
        &mut self,
        base_url: &GoUrl,
    ) -> Result<(), AutocompleteError> {
        for arg in self.arguments.iter_mut().flatten() {
            if arg.type_.as_str() != AutocompleteArgType::DYNAMIC_LIST {
                continue;
            }
            let AutocompleteArgData::DynamicList(dynamic_list) = &mut arg.data else {
                return Err(AutocompleteError::NotADynamicList);
            };
            let parsed =
                go_parse(&dynamic_list.fetch_url).map_err(|_| AutocompleteError::BadFetchUrl)?;
            // `URL.IsAbs()` is `Scheme != ""`.
            if parsed.scheme.is_empty() {
                let mut abs_url = base_url.clone();
                let base_path = String::from_utf8_lossy(&base_url.path).into_owned();
                abs_url.path =
                    crate::go_path::join(&[&base_path, &dynamic_list.fetch_url]).into_bytes();
                dynamic_list.fetch_url = abs_url.to_go_string();
            }
        }

        for command in self.sub_commands.iter_mut().flatten() {
            command.update_relative_urls_for_plugin_commands(base_url)?;
        }

        Ok(())
    }

    /// Port of `(*AutocompleteData).IsValid` (command_autocomplete.go:225).
    ///
    /// The rules, in Go's order:
    ///
    /// 1. a non-empty trigger, and it must already be lower-case — the check is
    ///    `strings.ToLower(t) != t`, so this **rejects** rather than normalises;
    /// 2. `RoleID` must be `system_admin`, `system_user`, or empty;
    /// 3. arguments and subcommands are mutually exclusive;
    /// 4. no positional argument may follow a named one;
    /// 5. each argument's `Data` must match its `Type`, and a positional text argument may not be
    ///    optional.
    ///
    /// Go's nil-receiver branch ("No nil commands are allowed") is unrepresentable on `&self`; a
    /// caller holding an `Option` checks it.
    pub fn is_valid(&self) -> Result<(), AutocompleteError> {
        if self.trigger.is_empty() {
            return Err(AutocompleteError::EmptyTrigger);
        }

        if go_to_lower(&self.trigger) != self.trigger {
            return Err(AutocompleteError::TriggerNotLowercase);
        }

        if !matches!(
            self.role_id.as_str(),
            SYSTEM_ADMIN_ROLE_ID | SYSTEM_USER_ROLE_ID | ""
        ) {
            return Err(AutocompleteError::WrongRole);
        }

        if !self.arguments_slice().is_empty() && !self.sub_commands_slice().is_empty() {
            return Err(AutocompleteError::ArgumentsAndSubcommands);
        }

        if !self.arguments_slice().is_empty() {
            let mut named_argument_index: Option<usize> = None;
            for (i, arg) in self.arguments_slice().iter().enumerate() {
                if arg.name.is_empty() {
                    // Positional.
                    if named_argument_index.is_some() {
                        return Err(AutocompleteError::NamedBeforePositional);
                    }
                } else if named_argument_index.is_none() {
                    named_argument_index = Some(i);
                }

                match arg.type_.as_str() {
                    AutocompleteArgType::DYNAMIC_LIST => {
                        let AutocompleteArgData::DynamicList(dynamic_list) = &arg.data else {
                            return Err(AutocompleteError::NotADynamicList);
                        };
                        go_parse(&dynamic_list.fetch_url)
                            .map_err(|_| AutocompleteError::BadFetchUrl)?;
                    }
                    AutocompleteArgType::STATIC_LIST => {
                        let AutocompleteArgData::StaticList(static_list) = &arg.data else {
                            return Err(AutocompleteError::NotAStaticList);
                        };
                        for item in static_list.possible_arguments.iter().flatten() {
                            if item.item.is_empty() {
                                return Err(AutocompleteError::EmptyPossibleArgument);
                            }
                        }
                    }
                    AutocompleteArgType::TEXT => {
                        if !matches!(arg.data, AutocompleteArgData::Text(_)) {
                            return Err(AutocompleteError::NotATextInput);
                        }
                        if arg.name.is_empty() && !arg.required {
                            return Err(AutocompleteError::OptionalPositional);
                        }
                    }
                    // Go has no `else` — an unknown type passes.
                    _ => {}
                }
            }
        }

        for command in self.sub_commands_slice() {
            command.is_valid()?;
        }

        Ok(())
    }
}

/// The errors `command_autocomplete.go` returns. Go builds them with `errors.New`; each message
/// here is Go's, verbatim, including its capitalisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AutocompleteError {
    #[error("An empty command name in the autocomplete data")]
    EmptyTrigger,
    #[error("Command should be lowercase")]
    TriggerNotLowercase,
    #[error("Wrong role in the autocomplete data")]
    WrongRole,
    #[error("Command can't have arguments and subcommands")]
    ArgumentsAndSubcommands,
    #[error("Named argument should not be before positional argument")]
    NamedBeforePositional,
    #[error("Not a proper DynamicList type argument")]
    NotADynamicList,
    #[error("Not a proper StaticList type argument")]
    NotAStaticList,
    #[error("Not a proper TextInput type argument")]
    NotATextInput,
    #[error("Possible argument name not set in StaticList argument")]
    EmptyPossibleArgument,
    #[error("Positional argument can not be optional")]
    OptionalPositional,
    #[error("FetchURL is not a proper url")]
    BadFetchUrl,
}
