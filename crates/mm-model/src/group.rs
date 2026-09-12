//! Port of `model/group.go` — LDAP-synced, plugin-provided and custom user groups.
//!
//! # `Name` is nullable and that is the whole design
//!
//! `Group.Name` is a `*string` with `omitempty`, and a group **without** a name is legal as long
//! as `AllowReference` is false: an unnamed group cannot be `@mentioned`, so it needs no unique
//! mention handle. Turning on `allow_reference` is what makes the name mandatory. Modelling it as
//! a plain `String` would erase that distinction and make every unnamed LDAP group invalid.
//!
//! # A plugin source is a *prefix*
//!
//! `Source` is `ldap`, `custom`, or **anything starting with `plugin_`**. Three separate
//! predicates depend on that prefix — validity, `IsSyncable`, and whether a remote id is
//! required — so a source set is not a closed set here.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_none;
use crate::user::{CHANNEL_MENTIONS_NOTIFY_PROP, USER_NOTIFY_ALL, USER_NOTIFY_HERE};
use crate::utils::{AppError, AppResult, is_valid_id};

/// Port of `model.GroupSource` (group.go:24) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupSource(pub String);

impl GroupSource {
    pub const LDAP: &'static str = "ldap";
    pub const CUSTOM: &'static str = "custom";
    /// Plugin groups **prefix** their source with this; it is not a source on its own.
    pub const PLUGIN_PREFIX: &'static str = "plugin_";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_plugin(&self) -> bool {
        self.0.starts_with(Self::PLUGIN_PREFIX)
    }
}

impl From<&str> for GroupSource {
    fn from(s: &str) -> Self {
        GroupSource(s.to_string())
    }
}

pub const GROUP_NAME_MAX_LENGTH: usize = 64;
pub const GROUP_SOURCE_MAX_LENGTH: usize = 64;
pub const GROUP_DISPLAY_NAME_MAX_LENGTH: usize = 128;
pub const GROUP_DESCRIPTION_MAX_LENGTH: usize = 1024;
pub const GROUP_REMOTE_ID_MAX_LENGTH: usize = 48;

/// Port of `model.GetSyncableGroupSources` (group.go:236).
pub fn get_syncable_group_sources() -> Vec<GroupSource> {
    vec![GroupSource::LDAP.into()]
}

/// Port of `model.GetSyncableGroupSourcePrefixes` (group.go:240).
pub fn get_syncable_group_source_prefixes() -> Vec<GroupSource> {
    vec![GroupSource::PLUGIN_PREFIX.into()]
}

/// Port of `validGroupnameChars` (group.go:266) — `^[a-z0-9\.\-_]+$`. Lower-case only.
///
/// **The `!name.is_empty()` is the regex's `+` and is unreachable from the only caller.**
/// `is_valid_name` rejects an empty name for its *length* two branches earlier, so no input can
/// arrive here empty; deleting the guard survives the whole behavioural corpus, which is a fact
/// about the call graph and not a gap in the fixtures. It stays because it is what the `+` means,
/// and because a second caller — the store, or the patch-time name derivation — would reach it.
fn is_valid_group_name_chars(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
        })
}

/// Port of `model.Group` (group.go:27).
///
/// The four `db:"-"` fields are computed per query, not stored: `has_syncables`, `member_count`,
/// `channel_member_count`, `channel_member_timezones_count` and `member_ids`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    #[serde(rename = "id")]
    pub id: String,

    /// `omitempty` **and** nullable — see the module docs.
    #[serde(rename = "name", skip_serializing_if = "is_none")]
    pub name: Option<String>,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "source")]
    pub source: GroupSource,

    /// Nullable but **not** `omitempty`, unlike `name` — so it is always written, as `null` when
    /// absent.
    #[serde(rename = "remote_id")]
    pub remote_id: Option<String>,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// `db:"-"` — whether the group is linked to any team or channel.
    #[serde(rename = "has_syncables")]
    pub has_syncables: bool,

    /// `db:"-"`, `omitempty`.
    #[serde(rename = "member_count", skip_serializing_if = "is_none")]
    pub member_count: Option<i64>,

    #[serde(rename = "allow_reference")]
    pub allow_reference: bool,

    /// `db:"-"`, `omitempty`.
    #[serde(rename = "channel_member_count", skip_serializing_if = "is_none")]
    pub channel_member_count: Option<i64>,

    /// `db:"-"`, `omitempty`.
    #[serde(
        rename = "channel_member_timezones_count",
        skip_serializing_if = "is_none"
    )]
    pub channel_member_timezones_count: Option<i64>,

    /// `db:"-"`, and **not** `omitempty` — `null` when not requested.
    #[serde(rename = "member_ids")]
    pub member_ids: Option<Vec<String>>,
}

impl Group {
    /// Port of `(*Group).GetName` (group.go:293) — `""` for a nameless group.
    pub fn get_name(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    /// Port of `(*Group).GetRemoteId` (group.go:297).
    pub fn get_remote_id(&self) -> &str {
        self.remote_id.as_deref().unwrap_or("")
    }

    /// Port of `(*Group).GetMemberCount` (group.go:301).
    pub fn get_member_count(&self) -> i64 {
        self.member_count.unwrap_or(0)
    }

    /// Port of `(*Group).Patch` (group.go:180).
    ///
    /// **`Source` and `RemoteId` are deliberately absent from `GroupPatch`** — Go's comment is
    /// explicit that allowing them would let a caller repoint a group at an LDAP source.
    pub fn patch(&mut self, patch: &GroupPatch) {
        if patch.name.is_some() {
            self.name = patch.name.clone();
        }
        if let Some(display_name) = &patch.display_name {
            self.display_name = display_name.clone();
        }
        if let Some(description) = &patch.description {
            self.description = description.clone();
        }
        if let Some(allow_reference) = patch.allow_reference {
            self.allow_reference = allow_reference;
        }
    }

    /// Port of `(*Group).requiresRemoteId` (group.go:232) — LDAP and plugin groups only.
    fn requires_remote_id(&self) -> bool {
        self.source.as_str() == GroupSource::LDAP || self.source.is_plugin()
    }

    /// Port of `(*Group).IsSyncable` (group.go:244). The same test as
    /// `requires_remote_id`, spelled out separately in Go too.
    pub fn is_syncable(&self) -> bool {
        self.source.as_str() == GroupSource::LDAP || self.source.is_plugin()
    }

    /// Port of `(*Group).IsValidName` (group.go:268).
    ///
    /// Three reserved names — `all`, `channel`, `here` — because a group name shares the mention
    /// namespace with them. Note the reserved-name branch's `Where` is **`IsValidName`**, without
    /// the `Group.` prefix the other two carry.
    ///
    /// Lengths are **bytes**, not runes.
    pub fn is_valid_name(&self) -> AppResult {
        match &self.name {
            None => {
                if self.allow_reference {
                    return Err(name_err(
                        "Group.IsValidName",
                        "model.group.name.app_error",
                        true,
                    ));
                }
            }
            Some(name) => {
                if name.is_empty() || name.len() > GROUP_NAME_MAX_LENGTH {
                    return Err(name_err(
                        "Group.IsValidName",
                        "model.group.name.invalid_length.app_error",
                        true,
                    ));
                }

                if name.as_str() == USER_NOTIFY_ALL
                    || name.as_str() == CHANNEL_MENTIONS_NOTIFY_PROP
                    || name.as_str() == USER_NOTIFY_HERE
                {
                    return Err(name_err(
                        "IsValidName",
                        "model.group.name.reserved_name.app_error",
                        false,
                    ));
                }

                if !is_valid_group_name_chars(name) {
                    return Err(name_err(
                        "Group.IsValidName",
                        "model.group.name.invalid_chars.app_error",
                        false,
                    ));
                }
            }
        }

        Ok(())
    }

    /// Port of `(*Group).IsValidForCreate` (group.go:196).
    ///
    /// The remote-id branch is one condition with two halves: it fails when the id is **missing
    /// and required**, or when it is **too long** — so a `custom` group may have no remote id but
    /// still may not have an over-long one.
    pub fn is_valid_for_create(&self) -> AppResult {
        self.is_valid_name()?;

        let display_len = self.display_name.len();
        if display_len == 0 || display_len > GROUP_DISPLAY_NAME_MAX_LENGTH {
            return Err(create_err(
                "model.group.display_name.app_error",
                Some(("GroupDisplayNameMaxLength", GROUP_DISPLAY_NAME_MAX_LENGTH)),
            ));
        }

        if self.description.len() > GROUP_DESCRIPTION_MAX_LENGTH {
            return Err(create_err(
                "model.group.description.app_error",
                Some(("GroupDescriptionMaxLength", GROUP_DESCRIPTION_MAX_LENGTH)),
            ));
        }

        let is_valid_source = self.source.as_str() == GroupSource::LDAP
            || self.source.as_str() == GroupSource::CUSTOM
            || self.source.is_plugin();

        if !is_valid_source {
            return Err(create_err("model.group.source.app_error", None));
        }

        if (self.get_remote_id().is_empty() && self.requires_remote_id())
            || self.get_remote_id().len() > GROUP_REMOTE_ID_MAX_LENGTH
        {
            return Err(create_err("model.group.remote_id.app_error", None));
        }

        Ok(())
    }

    /// Port of `(*Group).IsValidForUpdate` (group.go:248).
    ///
    /// The id branch's error id is **`app.group.id.app_error`** — `app.`, not `model.`, the only
    /// one in the file.
    pub fn is_valid_for_update(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(Box::new(AppError::new(
                "Group.IsValidForUpdate",
                "app.group.id.app_error",
                None,
                "",
                400,
            )));
        }
        if self.create_at == 0 {
            return Err(Box::new(AppError::new(
                "Group.IsValidForUpdate",
                "model.group.create_at.app_error",
                None,
                "",
                400,
            )));
        }
        if self.update_at == 0 {
            return Err(Box::new(AppError::new(
                "Group.IsValidForUpdate",
                "model.group.update_at.app_error",
                None,
                "",
                400,
            )));
        }
        self.is_valid_for_create()
    }
}

fn name_err(where_: &'static str, id: &'static str, with_max_length: bool) -> Box<AppError> {
    let params = with_max_length.then(|| {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "GroupNameMaxLength".to_string(),
            serde_json::Value::from(GROUP_NAME_MAX_LENGTH as i64),
        );
        params
    });
    Box::new(AppError::new(where_, id, params, "", 400))
}

fn create_err(id: &'static str, param: Option<(&str, usize)>) -> Box<AppError> {
    let params = param.map(|(key, value)| {
        let mut params = std::collections::HashMap::new();
        params.insert(key.to_string(), serde_json::Value::from(value as i64));
        params
    });
    Box::new(AppError::new("Group.IsValidForCreate", id, params, "", 400))
}

/// Port of `model.GroupWithUserIds` (group.go:74) — `Group` inlined, plus the member list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupWithUserIds {
    #[serde(flatten)]
    pub group: Group,

    #[serde(rename = "user_ids")]
    pub user_ids: Option<Vec<String>>,
}

/// Port of `model.GroupWithSchemeAdmin` (group.go:93).
///
/// `SchemeAdmin` comes from the **syncable** join (`db:"SyncableSchemeAdmin"`), not from the
/// group itself: it says whether members of this group are admins of the team or channel the
/// group is linked to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupWithSchemeAdmin {
    #[serde(flatten)]
    pub group: Group,

    #[serde(rename = "scheme_admin", skip_serializing_if = "is_none")]
    pub scheme_admin: Option<bool>,
}

/// Port of `model.GroupsAssociatedToChannelWithSchemeAdmin` (group.go:98).
///
/// **`channel_id` is declared before the embedded `Group`**, so it is the first key on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupsAssociatedToChannelWithSchemeAdmin {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(flatten)]
    pub group: Group,

    #[serde(rename = "scheme_admin", skip_serializing_if = "is_none")]
    pub scheme_admin: Option<bool>,
}

/// Port of `model.GroupsAssociatedToChannel` (group.go:103).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupsAssociatedToChannel {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "groups")]
    pub groups: Option<Vec<GroupWithSchemeAdmin>>,
}

/// Port of `model.GroupPatch` (group.go:108). Four fields; `source` and `remote_id` are excluded
/// for security — see [`Group::patch`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupPatch {
    #[serde(rename = "name")]
    pub name: Option<String>,

    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "description")]
    pub description: Option<String>,

    #[serde(rename = "allow_reference")]
    pub allow_reference: Option<bool>,
}

/// Port of `model.LdapGroupSearchOpts` (group.go:117). No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LdapGroupSearchOpts {
    pub q: String,
    pub is_linked: Option<bool>,
    pub is_configured: Option<bool>,
}

/// Port of `model.PageOpts` (group.go:160).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageOpts {
    pub page: i64,
    pub per_page: i64,
}

/// Port of `model.GroupSearchOpts` (group.go:123). No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupSearchOpts {
    pub q: String,
    pub not_associated_to_team: String,
    pub not_associated_to_channel: String,
    pub include_member_count: bool,
    pub filter_allow_reference: bool,
    pub page_opts: Option<PageOpts>,
    pub since: i64,
    pub source: GroupSource,

    /// Intersects the result with the groups linked to the parent team. **Ignored** unless the
    /// parent team is group-constrained *and* `not_associated_to_channel` is set.
    pub filter_parent_team_permitted: bool,

    /// Intersects the result with the groups this user belongs to.
    pub filter_has_member: String,

    /// A **channel id**, despite the boolean-sounding name: the count is per that channel.
    pub include_channel_member_count: String,
    pub include_timezones: bool,
    pub include_member_ids: bool,

    pub include_archived: bool,
    /// Return **only** archived groups. Distinct from `include_archived`.
    pub filter_archived: bool,

    pub only_syncable_sources: bool,
}

/// Port of `model.GetGroupOpts` (group.go:155).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GetGroupOpts {
    pub include_member_count: bool,
    pub include_member_ids: bool,
}

/// Port of `model.GroupStats` (group.go:165).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupStats {
    #[serde(rename = "group_id")]
    pub group_id: String,

    #[serde(rename = "total_member_count")]
    pub total_member_count: i64,
}

/// Port of `model.GroupModifyMembers` (group.go:170).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupModifyMembers {
    #[serde(rename = "user_ids")]
    pub user_ids: Option<Vec<String>>,
}

/// Port of `model.GroupsWithCount` (group.go:305).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupsWithCount {
    #[serde(rename = "groups")]
    pub groups: Option<Vec<Group>>,

    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.CreateDefaultMembershipParams` (group.go:310). No `json:` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateDefaultMembershipParams {
    /// Epoch milliseconds.
    pub since: i64,
    /// Whether to re-add members who were previously removed by hand.
    pub re_add_removed_members: bool,
    pub scoped_user_id: Option<String>,
    pub scoped_team_id: Option<String>,
    pub scoped_channel_id: Option<String>,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn group_round_trips_the_fixture() {
        assert_fixture_round_trips!(Group, "group");
    }
    #[test]
    fn group_with_user_ids_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupWithUserIds, "group_with_user_ids");
    }
    #[test]
    fn group_with_scheme_admin_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupWithSchemeAdmin, "group_with_scheme_admin");
    }
    #[test]
    fn groups_associated_to_channel_with_scheme_admin_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            GroupsAssociatedToChannelWithSchemeAdmin,
            "groups_associated_to_channel_with_scheme_admin"
        );
    }
    #[test]
    fn groups_associated_to_channel_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupsAssociatedToChannel, "groups_associated_to_channel");
    }
    #[test]
    fn group_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupPatch, "group_patch");
    }
    #[test]
    fn group_stats_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupStats, "group_stats");
    }
    #[test]
    fn group_modify_members_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupModifyMembers, "group_modify_members");
    }
    #[test]
    fn groups_with_count_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupsWithCount, "groups_with_count");
    }
}

#[cfg(test)]
mod go_parity {
    //! Asserts against `fixtures/behaviour_group.json`, generated by
    //! `reference/dump/behaviour_group.go` from the Go implementation.
    //!
    //! The three validators are chains of refusals, and the corpus carries inputs that violate
    //! two rules at once so that the *order* of the chain is asserted rather than read: swapping
    //! any adjacent pair changes the `id` those rows report.
    //!
    //! **`AppError.params` is unexported in Go** (utils.go:240) and so is absent from the corpus.
    //! The three `Group*MaxLength` interpolation params this port sets are transcribed from the
    //! Go source, not pinned here; they never reach the wire, because `Message` is the
    //! untranslated id.

    use super::*;
    use serde::Deserialize;

    const CORPUS: &str = include_str!("../../../fixtures/behaviour_group.json");

    #[derive(Deserialize)]
    struct Corpus {
        is_valid_name: Vec<ValidationRow>,
        is_valid_for_create: Vec<ValidationRow>,
        is_valid_for_update: Vec<ValidationRow>,
        patch: Vec<PatchRow>,
        is_syncable: Vec<SyncableRow>,
        syncable_sources: SyncableSources,
    }

    #[derive(Deserialize)]
    struct ValidationRow {
        name: String,
        #[serde(rename = "in")]
        input: String,
        ok: bool,
        id: Option<String>,
        status_code: Option<i32>,
        #[serde(rename = "where")]
        where_: Option<String>,
        detailed_error: Option<String>,
    }

    #[derive(Deserialize)]
    struct PatchRow {
        name: String,
        #[serde(rename = "in")]
        input: String,
        patch: String,
        out: String,
        name_is_nil: bool,
        name_value: String,
        display_name: String,
        description: String,
        allow_reference: bool,
    }

    #[derive(Deserialize)]
    struct SyncableRow {
        source: String,
        is_syncable: bool,
    }

    #[derive(Deserialize)]
    struct SyncableSources {
        sources: Vec<String>,
        prefixes: Vec<String>,
    }

    fn corpus() -> Corpus {
        serde_json::from_str(CORPUS).expect("the behaviour corpus decodes")
    }

    /// Drive one validator over one section of the corpus.
    fn assert_section(rows: Vec<ValidationRow>, min: usize, run: fn(&Group) -> AppResult) {
        assert!(rows.len() >= min, "the corpus shrank");
        for row in rows {
            let group: Group =
                serde_json::from_str(&row.input).unwrap_or_else(|e| panic!("{}: {e}", row.name));
            match (run(&group), row.ok) {
                (Ok(()), true) => {}
                (Err(err), false) => {
                    assert_eq!(row.id.as_deref(), Some(err.id.as_str()), "{}", row.name);
                    assert_eq!(row.status_code, Some(err.status_code), "{}", row.name);
                    assert_eq!(
                        row.where_.as_deref(),
                        Some(err.where_.as_str()),
                        "{}: the `where` distinguishes which branch refused",
                        row.name
                    );
                    assert_eq!(
                        row.detailed_error.as_deref(),
                        Some(err.detailed_error.as_str()),
                        "{}",
                        row.name
                    );
                }
                (Ok(()), false) => {
                    panic!("{}: Go refused with {:?} and we accepted", row.name, row.id)
                }
                (Err(err), true) => {
                    panic!("{}: Go accepted and we refused with {}", row.name, err.id)
                }
            }
        }
    }

    #[test]
    fn is_valid_name_matches_go_on_every_branch() {
        assert_section(corpus().is_valid_name, 20, Group::is_valid_name);
    }

    #[test]
    fn is_valid_for_create_matches_go_on_every_branch() {
        assert_section(corpus().is_valid_for_create, 30, Group::is_valid_for_create);
    }

    #[test]
    fn is_valid_for_update_matches_go_on_every_branch() {
        assert_section(corpus().is_valid_for_update, 15, Group::is_valid_for_update);
    }

    /// The corpus is the evidence; these are the three facts in it that a reader is most likely to
    /// get wrong, named so a failure says which one moved.
    #[test]
    fn the_corpus_pins_the_three_easy_mistakes() {
        let corpus = corpus();
        let find = |rows: &[ValidationRow], name: &str| -> (Option<String>, Option<String>) {
            let row = rows
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("{name} is in the corpus"));
            (row.id.clone(), row.where_.clone())
        };

        // 1. The reserved-name branch's `where` has no `Group.` prefix.
        assert_eq!(
            find(&corpus.is_valid_name, "reserved_all"),
            (
                Some("model.group.name.reserved_name.app_error".to_owned()),
                Some("IsValidName".to_owned())
            )
        );
        // 2. Lengths are bytes: 33 two-byte characters is 66 bytes and is refused for length,
        //    *before* the charset check that the same character would also fail.
        assert_eq!(
            find(&corpus.is_valid_name, "name_33_two_byte_chars")
                .0
                .as_deref(),
            Some("model.group.name.invalid_length.app_error")
        );
        assert_eq!(
            find(&corpus.is_valid_name, "name_10_two_byte_chars")
                .0
                .as_deref(),
            Some("model.group.name.invalid_chars.app_error")
        );
        // 3. `GroupSourceMaxLength` is declared and never enforced.
        assert_eq!(
            find(&corpus.is_valid_for_create, "source_200_char_plugin").0,
            None,
            "a 200-character plugin source is accepted"
        );
    }

    #[test]
    fn patch_matches_go() {
        let rows = corpus().patch;
        assert!(rows.len() >= 10, "the corpus shrank");
        for row in rows {
            let mut group: Group =
                serde_json::from_str(&row.input).unwrap_or_else(|e| panic!("{}: {e}", row.name));
            let patch: GroupPatch =
                serde_json::from_str(&row.patch).unwrap_or_else(|e| panic!("{}: {e}", row.name));
            group.patch(&patch);

            assert_eq!(group.name.is_none(), row.name_is_nil, "{}", row.name);
            assert_eq!(group.get_name(), row.name_value, "{}", row.name);
            assert_eq!(group.display_name, row.display_name, "{}", row.name);
            assert_eq!(group.description, row.description, "{}", row.name);
            assert_eq!(group.allow_reference, row.allow_reference, "{}", row.name);

            // The whole document too, so a field `Patch` touches that the rows above do not name
            // — `source`, `remote_id`, the timestamps — fails here rather than silently.
            let expected: serde_json::Value = serde_json::from_str(&row.out).unwrap();
            assert_eq!(
                serde_json::to_value(&group).unwrap(),
                expected,
                "{}: the patched document",
                row.name
            );
        }
    }

    #[test]
    fn is_syncable_matches_go() {
        let rows = corpus().is_syncable;
        assert!(rows.len() >= 12, "the corpus shrank");
        for row in rows {
            let group = Group {
                source: row.source.as_str().into(),
                ..Group::default()
            };
            assert_eq!(
                group.is_syncable(),
                row.is_syncable,
                "source {:?}",
                row.source
            );
            // Go spells the same predicate twice; both spellings are ported, so both are checked.
            assert_eq!(
                group.requires_remote_id(),
                row.is_syncable,
                "source {:?}: requiresRemoteId is IsSyncable's predicate verbatim",
                row.source
            );
        }
    }

    #[test]
    fn the_syncable_source_lists_match_go() {
        let expected = corpus().syncable_sources;
        assert_eq!(
            get_syncable_group_sources()
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect::<Vec<_>>(),
            expected.sources
        );
        assert_eq!(
            get_syncable_group_source_prefixes()
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect::<Vec<_>>(),
            expected.prefixes
        );
    }
}
