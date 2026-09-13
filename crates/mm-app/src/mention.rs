//! Port of the mention engine's parsing half: `app/mention_keywords.go`,
//! `app/mention_results.go`, `app/mention_parser.go`, `app/mention_parser_standard.go` and
//! `getExplicitMentions` (app/notification.go:1431).
//!
//! The first caller is `countThreadMentions` (app/post.go:2505) behind
//! `PUT /users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}`, which reads only
//! [`MentionResults::mentions`]. `SendNotifications` is the second and reads all of it, so
//! nothing here is narrowed to the first caller.
//!
//! # Where the text comes from
//!
//! [`get_explicit_mentions`] does not scan the message: it walks the post's markdown through
//! `mm_markdown::inspect` and feeds the parser **text nodes only**. A mention inside a code
//! span, a fenced block or a link destination is not text and is never seen; the same `@name`
//! in link *text* is. That is the whole reason the markdown package is a dependency of this
//! crate, and why a scan of the raw message would be the wrong port.
//!
//! # The keyword table is byte-exact, and case is applied unevenly
//!
//! [`MentionKeywords::add_user`] lower-cases the username and the mention keys with Go's
//! `strings.ToLower` ([`go_to_lower`]) and stores the first name **as written**;
//! [`MentionKeywords::add_group`] stores `@` + the group name as written. The parser then looks
//! each word up lower-cased first and verbatim second (`checkForMention`), so a first name matches
//! case-sensitively and everything else case-insensitively — the asymmetry Go's comment calls
//! out and this port keeps.

use std::collections::BTreeMap;

use mm_model::channel_member::{
    IGNORE_CHANNEL_MENTIONS_DEFAULT, IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP,
    IGNORE_CHANNEL_MENTIONS_ON,
};
use mm_model::group::Group;
use mm_model::post::{AllStringsOptions, Post};
use mm_model::status::{STATUS_ONLINE, Status};
use mm_model::user::{
    CHANNEL_MENTIONS_NOTIFY_PROP, FIRST_NAME_NOTIFY_PROP, MARK_UNREAD_NOTIFY_PROP,
    USER_NOTIFY_MENTION, User,
};
use mm_model::utils::{StringMap, go_to_lower, is_go_letter, is_go_number};

const MENTIONABLE_USER_PREFIX: &str = "user:";
const MENTIONABLE_GROUP_PREFIX: &str = "group:";

/// Port of `app.MentionableID` (mention_keywords.go:19): a user or group id with a prefix saying
/// which, so one keyword table can hold both.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MentionableId(String);

impl MentionableId {
    /// Port of `mentionableUserID` (mention_keywords.go:21).
    pub fn user(user_id: &str) -> Self {
        Self(format!("{MENTIONABLE_USER_PREFIX}{user_id}"))
    }

    /// Port of `mentionableGroupID` (mention_keywords.go:25).
    pub fn group(group_id: &str) -> Self {
        Self(format!("{MENTIONABLE_GROUP_PREFIX}{group_id}"))
    }

    /// Port of `MentionableID.AsUserID` (mention_keywords.go:29).
    pub fn as_user_id(&self) -> Option<&str> {
        self.0.strip_prefix(MENTIONABLE_USER_PREFIX)
    }

    /// Port of `MentionableID.AsGroupID` (mention_keywords.go:38).
    pub fn as_group_id(&self) -> Option<&str> {
        self.0.strip_prefix(MENTIONABLE_GROUP_PREFIX)
    }
}

/// Port of `app.MentionType` (mention_results.go:34). The order **is** the priority:
/// [`MentionResults::add_mention`] keeps the higher of an existing and an incoming type, so the
/// discriminants are Go's `iota` values and must not be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MentionType {
    /// A placeholder that should never be used in practice.
    NoMention = 0,
    /// The post is in a GM.
    GmMention = 1,
    /// The post is in a thread that the user has commented on.
    ThreadMention = 2,
    /// The post is a comment on a thread started by the user.
    CommentMention = 3,
    /// The post contains an at-channel, at-all, or at-here.
    ChannelMention = 4,
    /// The post is a DM.
    DmMention = 5,
    /// The post contains an at-mention for the user.
    KeywordMention = 6,
    /// The post contains a group mention for the user.
    GroupMention = 7,
}

/// Port of `app.MentionResults` (mention_results.go:36).
///
/// Go leaves the two maps `nil` until the first insertion; nothing reads the difference (the
/// type never reaches the wire), so they are plain maps here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MentionResults {
    /// The id of each user mentioned, to how they were mentioned.
    pub mentions: BTreeMap<String, MentionType>,
    /// The id of each group mentioned, to how it was mentioned.
    pub group_mentions: BTreeMap<String, MentionType>,
    /// Strings that looked like mentions but had no corresponding keyword, **without** the `@`.
    pub other_potential_mentions: Vec<String>,
    /// The message contained `@here`.
    pub here_mentioned: bool,
    /// The message contained `@all`.
    pub all_mentioned: bool,
    /// The message contained `@channel`.
    pub channel_mentioned: bool,
}

impl MentionResults {
    /// Port of `(*MentionResults).isUserMentioned` (mention_results.go:57).
    ///
    /// Note the second lookup: it asks `GroupMentions` for a **user** id. That is Go's code, and
    /// since the two id spaces never collide it is a no-op that is reproduced rather than tidied.
    pub fn is_user_mentioned(&self, user_id: &str) -> bool {
        if self.mentions.contains_key(user_id) {
            return true;
        }
        if self.group_mentions.contains_key(user_id) {
            return true;
        }
        self.here_mentioned || self.all_mentioned || self.channel_mentioned
    }

    /// Port of `(*MentionResults).addMention` (mention_results.go:69): a user already recorded
    /// with a type **at least as high** keeps it.
    pub fn add_mention(&mut self, user_id: &str, mention_type: MentionType) {
        if let Some(current) = self.mentions.get(user_id) {
            if *current >= mention_type {
                return;
            }
        }
        self.mentions.insert(user_id.to_owned(), mention_type);
    }

    /// Port of `(*MentionResults).removeMention` (mention_results.go:81).
    pub fn remove_mention(&mut self, user_id: &str) {
        self.mentions.remove(user_id);
    }

    /// Port of `(*MentionResults).addGroupMention` (mention_results.go:85). Always
    /// [`MentionType::GroupMention`]; there is no priority question for groups.
    pub fn add_group_mention(&mut self, group_id: &str) {
        self.group_mentions
            .insert(group_id.to_owned(), MentionType::GroupMention);
    }
}

/// Port of `app.MentionKeywords` (mention_keywords.go:48): keyword → the ids it mentions.
///
/// A `BTreeMap` where Go has a `map`. The only place iteration order is observable is
/// [`is_keyword_multibyte`], and there Go's order is *random* — see that function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MentionKeywords(BTreeMap<String, Vec<MentionableId>>);

impl MentionKeywords {
    pub fn new() -> Self {
        Self::default()
    }

    /// The ids behind one keyword, exactly as stored.
    pub fn get(&self, keyword: &str) -> Option<&[MentionableId]> {
        self.0.get(keyword).map(Vec::as_slice)
    }

    fn push(&mut self, keyword: String, id: MentionableId) {
        self.0.entry(keyword).or_default().push(id);
    }

    /// Port of `MentionKeywords.AddUser` (mention_keywords.go:50).
    ///
    /// # Six decisions, in Go's order
    ///
    /// 1. `@` + the **lower-cased** username, always.
    /// 2. Every non-empty entry of `mention_keys`, lower-cased — so a key of `Bob` matches `bob`.
    /// 3. The first name **as written**, only when `notify_props.first_name == "true"` and the
    ///    name is non-empty. This is the one case-sensitive entry in the table.
    /// 4. `@channel` and `@all`, only when `allow_channel_mentions`, the user's
    ///    `notify_props.channel == "true"`, and the *channel* member's notify props do not
    ///    silence them: `ignore_channel_mentions == "on"`, or `mark_unread == "mention"` with
    ///    `ignore_channel_mentions == "default"`. A missing key is `""` in Go, which is neither
    ///    `"on"` nor `"default"` — so the empty map `countThreadMentions` passes silences nothing.
    /// 5. `@here` on top of those, only when the status given is `online`. `countThreadMentions`
    ///    passes a synthetic online status ("they would've triggered this").
    /// 6. `notify_props` absent on the user reads as every key `""`, so no first name and no
    ///    channel mentions.
    pub fn add_user(
        &mut self,
        profile: &User,
        channel_notify_props: &StringMap,
        status: Option<&Status>,
        allow_channel_mentions: bool,
    ) -> &mut Self {
        let mentionable_id = MentionableId::user(&profile.id);
        let notify = |key: &str| -> &str {
            profile
                .notify_props
                .as_ref()
                .and_then(|props| props.get(key))
                .map_or("", String::as_str)
        };
        let channel_notify =
            |key: &str| -> &str { channel_notify_props.get(key).map_or("", String::as_str) };

        let user_mention = format!("@{}", go_to_lower(&profile.username));
        self.push(user_mention, mentionable_id.clone());

        for mention_key in profile.get_mention_keys() {
            if !mention_key.is_empty() {
                // Lower-cased so the parser's first, case-insensitive lookup finds them.
                self.push(go_to_lower(&mention_key), mentionable_id.clone());
            }
        }

        if notify(FIRST_NAME_NOTIFY_PROP) == "true" && !profile.first_name.is_empty() {
            self.push(profile.first_name.clone(), mentionable_id.clone());
        }

        if allow_channel_mentions {
            let ignore_channel_mentions = channel_notify(IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP)
                == IGNORE_CHANNEL_MENTIONS_ON
                || (channel_notify(MARK_UNREAD_NOTIFY_PROP) == USER_NOTIFY_MENTION
                    && channel_notify(IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP)
                        == IGNORE_CHANNEL_MENTIONS_DEFAULT);

            if notify(CHANNEL_MENTIONS_NOTIFY_PROP) == "true" && !ignore_channel_mentions {
                self.push("@channel".to_owned(), mentionable_id.clone());
                self.push("@all".to_owned(), mentionable_id.clone());

                if status.is_some_and(|status| status.status == STATUS_ONLINE) {
                    self.push("@here".to_owned(), mentionable_id);
                }
            }
        }

        self
    }

    /// Port of `MentionKeywords.AddUserKeyword` (mention_keywords.go:89). No lower-casing here —
    /// the caller's keyword is stored verbatim.
    pub fn add_user_keyword(&mut self, user_id: &str, keyword: &str) -> &mut Self {
        self.push(keyword.to_owned(), MentionableId::user(user_id));
        self
    }

    /// Port of `MentionKeywords.AddGroup` (mention_keywords.go:95): `@` + the name **as
    /// written**, and nothing at all for a group whose name is `nil`.
    pub fn add_group(&mut self, group: &Group) -> &mut Self {
        if let Some(name) = &group.name {
            self.push(format!("@{name}"), MentionableId::group(&group.id));
        }
        self
    }

    /// Port of `MentionKeywords.AddGroupsMap` (mention_keywords.go:104).
    pub fn add_groups_map<'a>(&mut self, groups: impl IntoIterator<Item = &'a Group>) -> &mut Self {
        for group in groups {
            self.add_group(group);
        }
        self
    }
}

/// Port of `app.StandardMentionParser` (mention_parser_standard.go:15).
#[derive(Debug)]
pub struct StandardMentionParser<'k> {
    keywords: &'k MentionKeywords,
    results: MentionResults,
}

/// The three words `ProcessText` treats specially: never reported as "other potential
/// mentions", and (via `checkForMention`) each sets its own flag.
const SYSTEM_MENTIONS: [&str; 3] = ["@here", "@channel", "@all"];

/// Go's `strings.FieldsFunc`: split at every run of code points satisfying `is_separator`,
/// dropping empty fields.
fn go_fields_func(text: &str, is_separator: impl Fn(char) -> bool) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start: Option<usize> = None;
    for (index, c) in text.char_indices() {
        if is_separator(c) {
            if let Some(field_start) = start.take() {
                fields.push(&text[field_start..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(field_start) = start {
        fields.push(&text[field_start..]);
    }
    fields
}

/// The split predicate of `ProcessText`'s outer `FieldsFunc`: "any whitespace or punctuation
/// that can't be part of an at mention or emoji pattern".
///
/// `unicode.IsLetter` and `unicode.IsNumber` are the Go toolchain's category tables
/// ([`is_go_letter`], [`is_go_number`]), **not** `char::is_alphabetic`, which also accepts
/// combining marks and would keep a Devanagari word in one piece where Go splits it.
fn is_word_separator(c: char) -> bool {
    !(c == ':'
        || c == '.'
        || c == '-'
        || c == '_'
        || c == '@'
        || is_go_letter(c)
        || is_go_number(c))
}

impl<'k> StandardMentionParser<'k> {
    /// Port of `makeStandardMentionParser` (mention_parser_standard.go:21).
    pub fn new(keywords: &'k MentionKeywords) -> Self {
        Self {
            keywords,
            results: MentionResults::default(),
        }
    }

    /// Port of `(*StandardMentionParser).ProcessText` (mention_parser_standard.go:30).
    ///
    /// # Per word, in order, and every step is reachable
    ///
    /// 1. A word that both starts and ends with `:` is an emoji and is skipped — a lone `:` too.
    /// 2. Leading `:`, `.`, `-`, `_` are stripped (not `@`).
    /// 3. `checkForMention` on the whole word; a hit ends the word.
    /// 4. Trailing `.`, `-`, `:`, `_` are peeled one at a time, checking after each; a hit ends
    ///    the word. The peeled copy is discarded either way.
    /// 5. A word starting with `@` that is not a system mention is an "other potential mention",
    ///    minus one trailing `.`, `-` or `:` and minus the `@`. **Otherwise** a word containing
    ///    `.`, `-` or `:` is split on those and each piece is checked and, if `@`-prefixed and not
    ///    a system mention, reported. The two branches are exclusive, so `@a.@b` reports `a.@b`
    ///    and never looks at `@b`.
    /// 6. [`is_keyword_multibyte`] runs on the (step-2) word **regardless** of the branches above,
    ///    so a multibyte keyword inside a word already reported as a potential mention still adds
    ///    its ids.
    pub fn process_text(&mut self, text: &str) {
        for word in go_fields_func(text, is_word_separator) {
            // `word[0] == ':' && word[len(word)-1] == ':'` — byte checks; the set is ASCII.
            if word.starts_with(':') && word.ends_with(':') {
                continue;
            }

            let word = word.trim_start_matches([':', '.', '-', '_']);

            if self.check_for_mention(word) {
                continue;
            }

            let mut found_without_suffix = false;
            let mut word_without_suffix = word;
            while !word_without_suffix.is_empty()
                && word_without_suffix.ends_with(['.', '-', ':', '_'])
            {
                word_without_suffix = &word_without_suffix[..word_without_suffix.len() - 1];
                if self.check_for_mention(word_without_suffix) {
                    found_without_suffix = true;
                    break;
                }
            }
            if found_without_suffix {
                continue;
            }

            if !SYSTEM_MENTIONS.contains(&word) && word.starts_with('@') {
                // "No need to bother about unicode as we are looking for ASCII characters."
                let trimmed = match word.as_bytes()[word.len() - 1] {
                    b'.' | b'-' | b':' => &word[..word.len() - 1],
                    _ => word,
                };
                self.results
                    .other_potential_mentions
                    .push(trimmed[1..].to_owned());
            } else if word.contains(['.', '-', ':']) {
                for split_word in go_fields_func(word, |c| c == '.' || c == '-' || c == ':') {
                    if self.check_for_mention(split_word) {
                        continue;
                    }
                    if !SYSTEM_MENTIONS.contains(&split_word) && split_word.starts_with('@') {
                        self.results
                            .other_potential_mentions
                            .push(split_word[1..].to_owned());
                    }
                }
            }

            if let Some(ids) = is_keyword_multibyte(self.keywords, word) {
                self.add_mentions(ids, MentionType::KeywordMention);
            }
        }
    }

    /// Port of `(*StandardMentionParser).Results` (mention_parser_standard.go:97).
    pub fn results(self) -> MentionResults {
        self.results
    }

    /// Port of `checkForMention` (mention_parser_standard.go:102).
    ///
    /// The `@here`/`@channel`/`@all` flags are set by the `switch` **before** the table lookup,
    /// so `@here` in a message flips `here_mentioned` even when no user has `@here` in their
    /// keywords. The lookup is lower-cased first, then verbatim — the verbatim pass is what
    /// makes the first name case-sensitive.
    fn check_for_mention(&mut self, word: &str) -> bool {
        let lower = go_to_lower(word);
        let mention_type = match lower.as_str() {
            "@here" => {
                self.results.here_mentioned = true;
                MentionType::ChannelMention
            }
            "@channel" => {
                self.results.channel_mentioned = true;
                MentionType::ChannelMention
            }
            "@all" => {
                self.results.all_mentioned = true;
                MentionType::ChannelMention
            }
            _ => MentionType::KeywordMention,
        };

        if let Some(ids) = self.keywords.get(&lower) {
            self.add_mentions(ids, mention_type);
            return true;
        }

        // Case-sensitive check for first name.
        if let Some(ids) = self.keywords.get(word) {
            self.add_mentions(ids, mention_type);
            return true;
        }

        false
    }

    /// Port of `addMentions` (mention_parser_standard.go:131).
    fn add_mentions(&mut self, ids: &[MentionableId], mention_type: MentionType) {
        for id in ids {
            if let Some(user_id) = id.as_user_id() {
                self.results.add_mention(user_id, mention_type);
            } else if let Some(group_id) = id.as_group_id() {
                self.results.add_group_mention(group_id);
            }
        }
    }
}

/// Port of `isKeywordMultibyte` (mention_parser_standard.go:142): a word containing a multibyte
/// character matches every multibyte keyword it *contains* — a substring test, not a word test.
///
/// # Go's answer is not deterministic, and this one is
///
/// Go overwrites `ids` on every containing keyword while ranging over a map, so when a word
/// contains **two** multibyte keywords the ids returned are whichever key the runtime visited
/// last. This walks the table in key order and returns the lexicographically last containing
/// key's ids. No test can pin the difference because Go's own answer varies between runs; a
/// word containing two multibyte keywords is the one input on which the two servers can
/// legitimately disagree.
pub fn is_keyword_multibyte<'k>(
    keywords: &'k MentionKeywords,
    word: &str,
) -> Option<&'k [MentionableId]> {
    // `len(word) != utf8.RuneCountInString(word)` — any non-ASCII code point.
    if word.len() == word.chars().count() {
        return None;
    }
    let mut found = None;
    for (keyword, ids) in &keywords.0 {
        if keyword.len() != keyword.chars().count() && word.contains(keyword.as_str()) {
            found = Some(ids.as_slice());
        }
    }
    found
}

/// Port of `getExplicitMentions` (app/notification.go:1431).
///
/// Every string of the post ([`Post::all_strings`]) is parsed as markdown and only the **text
/// nodes** reach the parser. Adjacent text nodes are joined into one buffer, which is flushed by
/// the next non-text node — including the `None` the walker sends after each node's children —
/// and once more at the very end. So a mention split across two text nodes is joined, and one
/// inside a code span never arrives.
///
/// `mm_blocks_enabled` is `FeatureFlags.MmBlocksEnabled`, which decides whether the interactive
/// blocks' human-readable strings are among the inputs.
pub fn get_explicit_mentions(
    post: &Post,
    keywords: &MentionKeywords,
    mm_blocks_enabled: bool,
) -> MentionResults {
    let mut parser = StandardMentionParser::new(keywords);
    let mut buf = String::new();

    for message in post.all_strings(AllStringsOptions {
        omit_interactive_blocks: !mm_blocks_enabled,
    }) {
        mm_markdown::inspect(&message, |node| {
            match node.and_then(|node| node.as_text()) {
                None => {
                    // This node isn't a string so process any accumulated text in the buffer.
                    if !buf.is_empty() {
                        parser.process_text(&buf);
                    }
                    buf.clear();
                    true
                }
                Some(text) => {
                    // This node is a string, so add it to buf and continue onto the next node to
                    // see if it's more text.
                    buf.push_str(text);
                    false
                }
            }
        });
    }

    // Process any left over text.
    if !buf.is_empty() {
        parser.process_text(&buf);
    }

    parser.results()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "aliceaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "bobbbbbbbbbbbbbbbbbbbbbbbb";

    fn user(id: &str, username: &str, first_name: &str, props: &[(&str, &str)]) -> User {
        let mut user = User {
            id: id.to_owned(),
            username: username.to_owned(),
            first_name: first_name.to_owned(),
            ..Default::default()
        };
        if !props.is_empty() {
            user.notify_props = Some(
                props
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            );
        }
        user
    }

    fn online() -> Status {
        Status {
            status: STATUS_ONLINE.to_owned(),
            ..Default::default()
        }
    }

    fn keywords_for(user: &User) -> MentionKeywords {
        let mut keywords = MentionKeywords::new();
        keywords.add_user(user, &StringMap::new(), Some(&online()), true);
        keywords
    }

    fn parse(keywords: &MentionKeywords, text: &str) -> MentionResults {
        let mut parser = StandardMentionParser::new(keywords);
        parser.process_text(text);
        parser.results()
    }

    fn mentioned(results: &MentionResults, user_id: &str) -> Option<MentionType> {
        results.mentions.get(user_id).copied()
    }

    // ---- MentionableId --------------------------------------------------------------------

    #[test]
    fn a_mentionable_id_knows_which_kind_it_is() {
        let user = MentionableId::user("u1");
        let group = MentionableId::group("g1");
        assert_eq!(user.as_user_id(), Some("u1"));
        assert_eq!(user.as_group_id(), None);
        assert_eq!(group.as_group_id(), Some("g1"));
        assert_eq!(group.as_user_id(), None);
    }

    // ---- MentionResults -------------------------------------------------------------------

    #[test]
    fn add_mention_keeps_the_higher_priority() {
        let mut results = MentionResults::default();
        results.add_mention(ALICE, MentionType::KeywordMention);
        results.add_mention(ALICE, MentionType::ChannelMention);
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::KeywordMention)
        );

        results.add_mention(ALICE, MentionType::GroupMention);
        assert_eq!(mentioned(&results, ALICE), Some(MentionType::GroupMention));

        // Equal does not rewrite either — `currentType >= mentionType` returns.
        results.add_mention(ALICE, MentionType::GroupMention);
        assert_eq!(mentioned(&results, ALICE), Some(MentionType::GroupMention));
    }

    #[test]
    fn the_mention_type_order_is_gos_iota() {
        use MentionType::*;
        let order = [
            NoMention,
            GmMention,
            ThreadMention,
            CommentMention,
            ChannelMention,
            DmMention,
            KeywordMention,
            GroupMention,
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} must sort below {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(GroupMention as u8, 7);
    }

    #[test]
    fn is_user_mentioned_reads_the_three_flags() {
        let mut results = MentionResults::default();
        assert!(!results.is_user_mentioned(ALICE));
        results.here_mentioned = true;
        assert!(results.is_user_mentioned(ALICE));
        results.here_mentioned = false;
        results.add_group_mention(ALICE);
        assert!(results.is_user_mentioned(ALICE));
        results.group_mentions.clear();
        results.add_mention(ALICE, MentionType::DmMention);
        assert!(results.is_user_mentioned(ALICE));
        results.remove_mention(ALICE);
        assert!(!results.is_user_mentioned(ALICE));
    }

    // ---- MentionKeywords::add_user ----------------------------------------------------------

    #[test]
    fn add_user_lower_cases_the_username_and_mention_keys_but_not_the_first_name() {
        let alice = user(
            ALICE,
            "Alice",
            "Alicia",
            &[
                ("mention_keys", "Boss, ,@Chief"),
                ("first_name", "true"),
                ("channel", "true"),
            ],
        );
        let keywords = keywords_for(&alice);
        let id = [MentionableId::user(ALICE)];
        assert_eq!(keywords.get("@alice"), Some(&id[..]));
        assert_eq!(keywords.get("@Alice"), None);
        assert_eq!(keywords.get("boss"), Some(&id[..]));
        assert_eq!(keywords.get("@chief"), Some(&id[..]));
        assert_eq!(keywords.get(""), None, "the blank key is dropped");
        assert_eq!(keywords.get("Alicia"), Some(&id[..]));
        assert_eq!(
            keywords.get("alicia"),
            None,
            "the first name is case-sensitive"
        );
        assert_eq!(keywords.get("@channel"), Some(&id[..]));
        assert_eq!(keywords.get("@all"), Some(&id[..]));
        assert_eq!(keywords.get("@here"), Some(&id[..]), "online, so @here too");
    }

    #[test]
    fn add_user_without_notify_props_adds_only_the_username() {
        let alice = user(ALICE, "alice", "Alicia", &[]);
        let keywords = keywords_for(&alice);
        assert_eq!(keywords.0.len(), 1);
        assert!(keywords.get("@alice").is_some());
    }

    #[test]
    fn add_user_first_name_needs_the_flag_and_a_name() {
        let flag_no_name = user(ALICE, "alice", "", &[("first_name", "true")]);
        assert_eq!(keywords_for(&flag_no_name).get(""), None);
        let name_no_flag = user(ALICE, "alice", "Alicia", &[("first_name", "false")]);
        assert_eq!(keywords_for(&name_no_flag).get("Alicia"), None);
    }

    #[test]
    fn add_user_channel_mentions_need_every_gate() {
        let alice = user(ALICE, "alice", "", &[("channel", "true")]);

        // The server does not allow them.
        let mut keywords = MentionKeywords::new();
        keywords.add_user(&alice, &StringMap::new(), Some(&online()), false);
        assert_eq!(keywords.get("@channel"), None);

        // Allowed, but not online: @channel and @all, no @here.
        let mut keywords = MentionKeywords::new();
        keywords.add_user(&alice, &StringMap::new(), None, true);
        assert!(keywords.get("@channel").is_some());
        assert!(keywords.get("@all").is_some());
        assert_eq!(keywords.get("@here"), None);

        // An away status is not online either.
        let away = Status {
            status: "away".to_owned(),
            ..Default::default()
        };
        let mut keywords = MentionKeywords::new();
        keywords.add_user(&alice, &StringMap::new(), Some(&away), true);
        assert_eq!(keywords.get("@here"), None);

        // The user turned them off.
        let quiet = user(ALICE, "alice", "", &[("channel", "false")]);
        let mut keywords = MentionKeywords::new();
        keywords.add_user(&quiet, &StringMap::new(), Some(&online()), true);
        assert_eq!(keywords.get("@channel"), None);
    }

    #[test]
    fn add_user_channel_member_props_can_silence_channel_mentions() {
        let alice = user(ALICE, "alice", "", &[("channel", "true")]);
        let props = |pairs: &[(&str, &str)]| -> StringMap {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };

        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            &alice,
            &props(&[("ignore_channel_mentions", "on")]),
            Some(&online()),
            true,
        );
        assert_eq!(keywords.get("@channel"), None, "explicitly ignored");

        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            &alice,
            &props(&[
                ("mark_unread", "mention"),
                ("ignore_channel_mentions", "default"),
            ]),
            Some(&online()),
            true,
        );
        assert_eq!(
            keywords.get("@channel"),
            None,
            "muted with the default setting"
        );

        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            &alice,
            &props(&[
                ("mark_unread", "mention"),
                ("ignore_channel_mentions", "off"),
            ]),
            Some(&online()),
            true,
        );
        assert!(
            keywords.get("@channel").is_some(),
            "muted but explicitly not ignoring"
        );

        // A missing key is `""`, which is neither `on` nor `default`: an empty map silences
        // nothing even though `mark_unread` is also missing. This is the map
        // `countThreadMentions` passes.
        let mut keywords = MentionKeywords::new();
        keywords.add_user(
            &alice,
            &props(&[("mark_unread", "mention")]),
            Some(&online()),
            true,
        );
        assert!(keywords.get("@channel").is_some());
    }

    #[test]
    fn add_group_uses_the_name_as_written_and_skips_a_nil_name() {
        let named = Group {
            id: "g1".to_owned(),
            name: Some("DevOps".to_owned()),
            ..Default::default()
        };
        let nameless = Group {
            id: "g2".to_owned(),
            name: None,
            ..Default::default()
        };
        let mut keywords = MentionKeywords::new();
        keywords.add_groups_map([&named, &nameless]);
        assert_eq!(
            keywords.get("@DevOps"),
            Some(&[MentionableId::group("g1")][..])
        );
        assert_eq!(keywords.get("@devops"), None);
        assert_eq!(keywords.0.len(), 1);
    }

    // ---- go_fields_func -------------------------------------------------------------------

    #[test]
    fn fields_func_splits_on_runs_and_drops_empties() {
        assert_eq!(
            go_fields_func("  a  b ", char::is_whitespace),
            vec!["a", "b"]
        );
        assert_eq!(go_fields_func("", char::is_whitespace), Vec::<&str>::new());
        assert_eq!(
            go_fields_func("   ", char::is_whitespace),
            Vec::<&str>::new()
        );
        assert_eq!(go_fields_func("abc", char::is_whitespace), vec!["abc"]);
    }

    #[test]
    fn the_word_separator_is_gos_letter_table_not_alphabetic() {
        // U+0947 DEVANAGARI VOWEL SIGN E is `Alphabetic` in Unicode but not category L, so Go
        // splits on it and `char::is_alphabetic` would not.
        assert!(is_word_separator('\u{0947}'));
        assert!(!'\u{0947}'.is_alphabetic() || is_word_separator('\u{0947}'));
        assert!(!is_word_separator('न'));
        assert!(!is_word_separator('é'));
        assert!(!is_word_separator('7'));
        assert!(!is_word_separator('٣'), "Arabic-Indic digit is category Nd");
        for keep in [':', '.', '-', '_', '@'] {
            assert!(!is_word_separator(keep));
        }
        for split in [' ', ',', '!', '(', '\n', '~', '#'] {
            assert!(is_word_separator(split), "{split:?} must split");
        }
    }

    // ---- ProcessText, branch by branch -----------------------------------------------------

    #[test]
    fn a_plain_at_mention_is_a_keyword_mention() {
        let alice = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&alice);
        let results = parse(&keywords, "hello @alice, hi");
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::KeywordMention)
        );
        assert!(results.other_potential_mentions.is_empty());
    }

    #[test]
    fn the_username_lookup_is_case_insensitive_and_the_first_name_is_not() {
        let alice = user(ALICE, "alice", "Alicia", &[("first_name", "true")]);
        let keywords = keywords_for(&alice);
        assert!(parse(&keywords, "@ALICE").mentions.contains_key(ALICE));
        assert!(parse(&keywords, "Alicia").mentions.contains_key(ALICE));
        assert!(!parse(&keywords, "alicia").mentions.contains_key(ALICE));
        assert!(!parse(&keywords, "ALICIA").mentions.contains_key(ALICE));
    }

    #[test]
    fn an_emoji_shaped_word_is_skipped_even_when_it_contains_a_keyword() {
        let alice = user(ALICE, "alice", "", &[("mention_keys", "alice")]);
        let keywords = keywords_for(&alice);
        assert!(parse(&keywords, ":alice:").mentions.is_empty());
        // A lone colon is both first and last byte.
        assert!(parse(&keywords, ":").mentions.is_empty());
        // Only leading-and-trailing: a leading colon alone is stripped and the word checked.
        assert!(parse(&keywords, ":alice").mentions.contains_key(ALICE));
    }

    #[test]
    fn leading_punctuation_is_stripped_before_the_lookup() {
        let alice = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&alice);
        for text in ["-@alice", "._@alice", ":@alice", "-.-:@alice"] {
            assert!(
                parse(&keywords, text).mentions.contains_key(ALICE),
                "{text}"
            );
        }
    }

    #[test]
    fn trailing_punctuation_is_peeled_one_character_at_a_time() {
        let alice = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&alice);
        for text in ["@alice.", "@alice:", "@alice-", "@alice_", "@alice.-:_"] {
            let results = parse(&keywords, text);
            assert!(results.mentions.contains_key(ALICE), "{text}");
            assert!(results.other_potential_mentions.is_empty(), "{text}");
        }
    }

    #[test]
    fn an_unknown_at_word_is_a_potential_mention_minus_one_trailing_mark() {
        let keywords = keywords_for(&user(ALICE, "alice", "", &[]));
        assert_eq!(
            parse(&keywords, "@bob.").other_potential_mentions,
            vec!["bob"]
        );
        assert_eq!(
            parse(&keywords, "@bob-").other_potential_mentions,
            vec!["bob"]
        );
        assert_eq!(
            parse(&keywords, "@bob:").other_potential_mentions,
            vec!["bob"]
        );
        // Only one, and only those three: `_` and a second `.` stay.
        assert_eq!(
            parse(&keywords, "@bob_").other_potential_mentions,
            vec!["bob_"]
        );
        assert_eq!(
            parse(&keywords, "@bob..").other_potential_mentions,
            vec!["bob."]
        );
        // A bare `@` reports the empty string.
        assert_eq!(parse(&keywords, "@").other_potential_mentions, vec![""]);
    }

    #[test]
    fn the_at_branch_and_the_split_branch_are_exclusive() {
        let keywords = keywords_for(&user(ALICE, "alice", "", &[]));
        // Starts with `@`: reported whole, and `@alice` inside it is never looked at.
        let results = parse(&keywords, "@bob.@alice");
        assert_eq!(results.other_potential_mentions, vec!["bob.@alice"]);
        assert!(results.mentions.is_empty());

        // Does not start with `@`: split on `.`, `-`, `:` and each piece is checked.
        let results = parse(&keywords, "cc:@alice.@bob");
        assert!(results.mentions.contains_key(ALICE));
        assert_eq!(results.other_potential_mentions, vec!["bob"]);
    }

    #[test]
    fn system_mentions_set_their_flag_whether_or_not_anyone_has_the_keyword() {
        let quiet = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&quiet);
        assert_eq!(keywords.get("@here"), None);
        let results = parse(&keywords, "@here @channel @all");
        assert!(results.here_mentioned);
        assert!(results.channel_mentioned);
        assert!(results.all_mentioned);
        assert!(results.mentions.is_empty());
        assert!(
            results.other_potential_mentions.is_empty(),
            "system mentions are never potential mentions"
        );

        let loud = user(ALICE, "alice", "", &[("channel", "true")]);
        let keywords = keywords_for(&loud);
        let results = parse(&keywords, "@Here");
        assert!(
            results.here_mentioned,
            "the switch is on the lower-cased word"
        );
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::ChannelMention)
        );
    }

    #[test]
    fn a_keyword_mention_outranks_a_channel_mention_on_the_same_user() {
        let alice = user(ALICE, "alice", "", &[("channel", "true")]);
        let keywords = keywords_for(&alice);
        let results = parse(&keywords, "@channel @alice");
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::KeywordMention)
        );
        let results = parse(&keywords, "@alice @channel");
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::KeywordMention)
        );
    }

    #[test]
    fn a_multibyte_keyword_matches_as_a_substring_of_a_multibyte_word() {
        let alice = user(ALICE, "alice", "", &[("mention_keys", "José")]);
        let keywords = keywords_for(&alice);
        // Lower-cased on the way in, so the keyword is `josé`.
        assert!(parse(&keywords, "xxjoséxx").mentions.contains_key(ALICE));
        // An ASCII word never takes the multibyte path.
        assert!(!parse(&keywords, "xxjosexx").mentions.contains_key(ALICE));
        // An ASCII keyword is never matched as a substring, even of a multibyte word.
        let bob = user(BOB, "bob", "", &[("mention_keys", "bob")]);
        let keywords = keywords_for(&bob);
        assert!(!parse(&keywords, "xxbobxxé").mentions.contains_key(BOB));
    }

    #[test]
    fn the_multibyte_path_runs_even_after_the_word_was_reported_as_potential() {
        let alice = user(ALICE, "alice", "", &[("mention_keys", "josé")]);
        let keywords = keywords_for(&alice);
        let results = parse(&keywords, "@josésmith");
        assert_eq!(results.other_potential_mentions, vec!["josésmith"]);
        assert!(results.mentions.contains_key(ALICE));
    }

    #[test]
    fn is_keyword_multibyte_returns_the_last_containing_key_in_table_order() {
        let mut keywords = MentionKeywords::new();
        keywords.add_user_keyword(ALICE, "é");
        keywords.add_user_keyword(BOB, "ü");
        let ids = is_keyword_multibyte(&keywords, "éü").expect("a match");
        assert_eq!(ids, &[MentionableId::user(BOB)][..]);
        assert!(is_keyword_multibyte(&keywords, "plain").is_none());
        assert!(is_keyword_multibyte(&keywords, "ö").is_none());
    }

    #[test]
    fn words_are_split_on_whitespace_and_punctuation_only() {
        let alice = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&alice);
        for text in [
            "(@alice)",
            "hi,@alice!",
            "@alice\n",
            "\t@alice\t",
            "«@alice»",
            "~@alice",
            "#@alice",
        ] {
            assert!(
                parse(&keywords, text).mentions.contains_key(ALICE),
                "{text:?}"
            );
        }
        // A letter glued on is part of the word.
        assert!(!parse(&keywords, "@alicex").mentions.contains_key(ALICE));
        assert!(!parse(&keywords, "x@alice").mentions.contains_key(ALICE));
    }

    // ---- get_explicit_mentions ---------------------------------------------------------------

    fn post_with(message: &str) -> Post {
        Post {
            message: message.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_mentions_walks_the_message_through_the_parser() {
        let alice = user(ALICE, "alice", "", &[]);
        let keywords = keywords_for(&alice);
        let results = get_explicit_mentions(&post_with("hey @alice"), &keywords, true);
        assert_eq!(
            mentioned(&results, ALICE),
            Some(MentionType::KeywordMention)
        );
        let results = get_explicit_mentions(&post_with("nobody here"), &keywords, true);
        assert!(results.mentions.is_empty());
    }

    #[test]
    fn explicit_mentions_of_an_empty_message_reads_nothing() {
        let keywords = keywords_for(&user(ALICE, "alice", "", &[]));
        let results = get_explicit_mentions(&post_with(""), &keywords, true);
        assert_eq!(results, MentionResults::default());
    }
}
