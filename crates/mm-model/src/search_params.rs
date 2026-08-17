//! Port of `model/search_params.go` (search_params.go:1–398).
//!
//! The search box: whatever a user typed becomes a `Vec<SearchParams>` here, and the store layer
//! turns each element into one query. Three things about it are load-bearing and none of them is
//! obvious from the source.
//!
//! # Go's `\d` and `\s` are ASCII; Rust's are Unicode
//!
//! Both term-trimming patterns are **negated** classes, so the difference inverts into
//! over-eager stripping rather than under-eager matching: a character Go does not count as a
//! digit is one Go *deletes*. Transcribed verbatim, `searchTermPuncStart` would leave `٣hello`
//! alone in Rust where Go returns `hello`, and `\s` would spare a NBSP that Go strips. Both
//! patterns therefore spell the classes out as `[0-9]` and `[\t\n\x0C\r ]`, measured over 169
//! codepoints rather than assumed.
//!
//! The same file uses `strings.Fields` two lines away, which splits on `unicode.IsSpace` — a
//! *different* set. So `a\u{a0}b` is two words, and the NBSP that separated them would also have
//! been stripped as punctuation had it been leading. Rust's `split_whitespace` agrees with
//! `strings.Fields` on the whole sweep, so that half needs no special handling.
//!
//! # `splitWords` and `parseSearchFlags` are unexported
//!
//! They cannot be called from the oracle package, so every parity case drives them through
//! [`parse_search_params`] and observes the composition. They are ported as private functions
//! here for the same reason they are private in Go.
//!
//! # A flag with an empty value eats the next word — unless it is last
//!
//! `in: town-square` is a channel filter. `in:` at the end of the input matches no branch at
//! all, falls through to the term path, has its trailing colon trimmed as punctuation, and
//! searches for the word **`in`**. Three of the corpus cases exist only to pin that.
//!
//! # The output is one, two or three params
//!
//! Plain terms and hashtag terms get a block each, sharing the same filters; and when there are
//! no terms but at least one filter, a third shape with empty terms is emitted instead. All
//! three carry the caller's timezone offset.

use std::borrow::Cow;
use std::sync::LazyLock;

use chrono::{Datelike, Local, NaiveDate};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};

use crate::integration_action::parse_go_iso_date;
use crate::utils::{
    AppError, AppResult, StringArray, collapse_leading_hashes, get_end_of_day_millis,
    get_start_of_day_millis, is_valid_hashtag, pad_date_string_zeros,
};

/// Port of `model.searchTermPuncStart` (search_params.go:13). Go: `^[^\pL\d\s#"]+`.
///
/// `\d` and `\s` are spelled out because Go's are ASCII-only — see the module docs. `#` and `"`
/// are excluded from the class, so a leading hashtag or quote survives; `*` is **not**, so a
/// leading wildcard is stripped while a trailing one is kept.
static TERM_PUNC_START: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new("^[^\\p{L}0-9\t\n\x0C\r #\"]+").ok());

/// Port of `model.searchTermPuncEnd` (search_params.go:14). Go: `[^\pL\p{M}\d\s*"]+$`.
///
/// Differs from [`TERM_PUNC_START`] in three places, all deliberate upstream: `\p{M}` is in the
/// class (a trailing combining mark is kept), `*` is in the class (wildcards survive), and `#`
/// is **not** (a trailing hash is stripped).
static TERM_PUNC_END: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new("[^\\p{L}\\p{M}0-9\t\n\x0C\r *\"]+$").ok());

/// Port of `model.searchFlags` (search_params.go:112).
pub const SEARCH_FLAGS: [&str; 7] = ["from", "channel", "in", "before", "after", "on", "ext"];

/// Port of `model.SearchParams` (search_params.go:16).
///
/// Every field carries `omitempty` except `modifier`, so the zero value is `{"modifier":""}` —
/// nineteen keys disappear. That also means the slices need no `Option`: Go's `omitempty` drops
/// a nil slice and an empty one alike, so the distinction `PostList` has to preserve is
/// unobservable here. Measured, not assumed — `ParseSearchParams` always allocates all six.
///
/// The container carries `#[serde(default)]` for [D-043]: Go leaves an absent field at its zero
/// value and a client sending a partial params object would otherwise be rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchParams {
    #[serde(rename = "terms", skip_serializing_if = "String::is_empty")]
    pub terms: String,

    #[serde(rename = "excluded_terms", skip_serializing_if = "String::is_empty")]
    pub excluded_terms: String,

    /// Go's JSON name is `ishashtag`, with no separator — the one field in the struct that is
    /// not snake_case.
    #[serde(rename = "ishashtag", skip_serializing_if = "is_false")]
    pub is_hashtag: bool,

    #[serde(
        rename = "in_channels",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub in_channels: StringArray,

    #[serde(
        rename = "excluded_channels",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub excluded_channels: StringArray,

    #[serde(
        rename = "from_users",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub from_users: StringArray,

    #[serde(
        rename = "excluded_users",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub excluded_users: StringArray,

    #[serde(rename = "after_date", skip_serializing_if = "String::is_empty")]
    pub after_date: String,

    #[serde(
        rename = "excluded_after_date",
        skip_serializing_if = "String::is_empty"
    )]
    pub excluded_after_date: String,

    #[serde(rename = "before_date", skip_serializing_if = "String::is_empty")]
    pub before_date: String,

    #[serde(
        rename = "excluded_before_date",
        skip_serializing_if = "String::is_empty"
    )]
    pub excluded_before_date: String,

    #[serde(
        rename = "extensions",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub extensions: StringArray,

    #[serde(
        rename = "excluded_extensions",
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub excluded_extensions: StringArray,

    #[serde(rename = "on_date", skip_serializing_if = "String::is_empty")]
    pub on_date: String,

    #[serde(rename = "excluded_date", skip_serializing_if = "String::is_empty")]
    pub excluded_date: String,

    #[serde(rename = "or_terms", skip_serializing_if = "is_false")]
    pub or_terms: bool,

    #[serde(rename = "include_deleted_channels", skip_serializing_if = "is_false")]
    pub include_deleted_channels: bool,

    /// Seconds east of UTC, straight from the client. Go's `int`, so `i64` here — and it is
    /// **not** range-checked anywhere, which is why [`get_start_of_day_millis`] cannot delegate
    /// to `chrono::FixedOffset`.
    #[serde(rename = "timezone_offset", skip_serializing_if = "is_zero_i64")]
    pub time_zone_offset: i64,

    /// True when the search does not originate from a "current user".
    #[serde(rename = "search_without_user_id", skip_serializing_if = "is_false")]
    pub search_without_user_id: bool,

    /// The only field with no `omitempty`, so it is always on the wire.
    #[serde(rename = "modifier")]
    pub modifier: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Go's `encoding/json` documents that unmarshalling `null` into a slice sets it to nil and
/// produces **no error**, so `{"in_channels":null}` is a legal params object that decodes to an
/// empty filter. serde rejects it outright, so every slice field routes through this.
///
/// Scalars have the same rule in Go and are **not** covered here — see [D-057].
fn null_as_empty<'de, D>(deserializer: D) -> Result<StringArray, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<StringArray>::deserialize(deserializer)?.unwrap_or_default())
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

impl SearchParams {
    /// Port of `(*SearchParams).GetAfterDateMillis` (search_params.go:41).
    ///
    /// **Falls back to the current time when the date does not parse**, so an unparseable
    /// `after:` filter silently becomes "after tomorrow" rather than an error. Go reads
    /// `time.Now()`, which is server-**local**, and then takes its calendar date — so the answer
    /// depends on the server's timezone as well as its clock. Reproduced, including the locality;
    /// see [D-008].
    pub fn get_after_date_millis(&self) -> i64 {
        self.after_date_millis(&self.after_date)
    }

    /// Port of `(*SearchParams).GetExcludedAfterDateMillis` (search_params.go:54). Identical to
    /// [`Self::get_after_date_millis`] but for the excluded field — Go duplicates the body.
    pub fn get_excluded_after_date_millis(&self) -> i64 {
        self.after_date_millis(&self.excluded_after_date)
    }

    /// Port of `(*SearchParams).GetBeforeDateMillis` (search_params.go:67).
    ///
    /// Returns **0** when the date does not parse, where the `after` pair reaches for the clock.
    /// Four of the six accessors take this branch and two do not; the asymmetry is upstream's.
    pub fn get_before_date_millis(&self) -> i64 {
        self.before_date_millis(&self.before_date)
    }

    /// Port of `(*SearchParams).GetExcludedBeforeDateMillis` (search_params.go:80).
    pub fn get_excluded_before_date_millis(&self) -> i64 {
        self.before_date_millis(&self.excluded_before_date)
    }

    /// Port of `(*SearchParams).GetOnDateMillis` (search_params.go:93).
    ///
    /// `(0, 0)` when the date does not parse.
    pub fn get_on_date_millis(&self) -> (i64, i64) {
        self.on_date_millis(&self.on_date)
    }

    /// Port of `(*SearchParams).GetExcludedDateMillis` (search_params.go:103).
    pub fn get_excluded_date_millis(&self) -> (i64, i64) {
        self.on_date_millis(&self.excluded_date)
    }

    /// The shared body of the two `after` accessors: the day **after** the given date, or after
    /// today when it does not parse.
    fn after_date_millis(&self, value: &str) -> i64 {
        let day = match parse_search_date(value) {
            Some(date) => date.succ_opt(),
            // Go: `date = time.Now()`, in the server's local zone, then `.Add(24h)`.
            None => {
                let now = Local::now();
                NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())
                    .and_then(|d| d.succ_opt())
            }
        };
        day.and_then(|d| start_of_day(d, self.time_zone_offset))
            .unwrap_or(0)
    }

    /// The shared body of the two `before` accessors: the end of the day **before** the given
    /// date, or 0.
    fn before_date_millis(&self, value: &str) -> i64 {
        parse_search_date(value)
            .and_then(|date| date.pred_opt())
            .and_then(|d| end_of_day(d, self.time_zone_offset))
            .unwrap_or(0)
    }

    /// The shared body of the two `on` accessors: both ends of the given day, or `(0, 0)`.
    fn on_date_millis(&self, value: &str) -> (i64, i64) {
        let Some(date) = parse_search_date(value) else {
            return (0, 0);
        };
        match (
            start_of_day(date, self.time_zone_offset),
            end_of_day(date, self.time_zone_offset),
        ) {
            (Some(start), Some(end)) => (start, end),
            _ => (0, 0),
        }
    }
}

/// `time.Parse("2006-01-02", PadDateStringZeros(value))`, which is what all six accessors open
/// with. Borrowed from `integration_action.rs` rather than re-transcribed — the layout and its
/// rejections (`2023-02-29`, a two-digit year, any trailing text) are identical.
fn parse_search_date(value: &str) -> Option<NaiveDate> {
    let (year, month, day) = parse_go_iso_date(&pad_date_string_zeros(value))?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn start_of_day(date: NaiveDate, offset: i64) -> Option<i64> {
    get_start_of_day_millis(&date.and_hms_opt(0, 0, 0)?.and_utc(), offset)
}

fn end_of_day(date: NaiveDate, offset: i64) -> Option<i64> {
    get_end_of_day_millis(&date.and_hms_opt(0, 0, 0)?.and_utc(), offset)
}

/// Port of `model.searchWord` (search_params.go:120).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchWord<'a> {
    value: Cow<'a, str>,
    exclude: bool,
}

/// Port of `model.flag` (search_params.go:114).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchFlag<'a> {
    /// The **canonical** name from [`SEARCH_FLAGS`], not the spelling the user typed: matching
    /// is `strings.EqualFold`, so `IN:x` records `in`.
    name: &'static str,
    value: &'a str,
    exclude: bool,
}

/// Port of `model.splitWords` (search_params.go:125).
///
/// Splits on whitespace, except that a `"` opens a region that runs to the next `"` and is
/// emitted as a single word **including both quotes**. Three details the loop encodes:
///
/// - A `-` immediately before an opening quote joins it, so `-"a b"` is one excluded word while
///   `a-"b"` is two (`a` and `-"b"`).
/// - An unclosed quote is not an error: the region never closes, so the trailing text is split
///   on whitespace as usual and the opening quote stays glued to its first word.
/// - The scan is over `char_indices`, and every character it compares (`"`, `-`) is one byte, so
///   the slicing is safe on any UTF-8 input.
fn split_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();

    let mut found_quote = false;
    let mut location = 0;
    for (i, char) in text.char_indices() {
        if char != '"' {
            continue;
        }
        if found_quote {
            words.push(&text[location..i + 1]);
            found_quote = false;
            location = i + 1;
        } else {
            let next_start = if i > 0 && text.as_bytes()[i - 1] == b'-' {
                i - 1
            } else {
                i
            };
            words.extend(text[location..next_start].split_whitespace());
            found_quote = true;
            location = next_start;
        }
    }

    words.extend(text[location..].split_whitespace());
    words
}

/// Port of `model.parseSearchFlags` (search_params.go:155).
///
/// A word containing a colon is a candidate flag. The name is matched case-insensitively against
/// [`SEARCH_FLAGS`]; a leading `-` excludes. Two branches are easy to miss:
///
/// - **An empty value consumes the following word** (`in: town-square`), *unless* the flag is the
///   last word — in which case no branch fires, `is_flag` stays false, and the word falls through
///   to the term path below. `in:` on its own therefore searches for the term `in`, because the
///   trailing colon is then trimmed as punctuation.
/// - **Exclusion is decided by a leading `-` on the raw word**, before any trimming, so `-hello`
///   excludes `hello` while `−hello` (U+2212) does not.
fn parse_search_flags<'a>(input: &[&'a str]) -> (Vec<SearchWord<'a>>, Vec<SearchFlag<'a>>) {
    let mut words = Vec::new();
    let mut flags = Vec::new();

    let mut skip_next_word = false;
    for (i, word) in input.iter().enumerate() {
        if skip_next_word {
            skip_next_word = false;
            continue;
        }

        let mut is_flag = false;

        if let Some(colon) = word.find(':') {
            let (flag_name, exclude) = if let Some(rest) = word.strip_prefix('-') {
                // Go slices `word[1:colon]`, which is the same span with the `-` removed.
                (&rest[..colon - 1], true)
            } else {
                (&word[..colon], false)
            };
            let value = &word[colon + 1..];

            for search_flag in SEARCH_FLAGS {
                if !flag_name.eq_ignore_ascii_case(search_flag) {
                    continue;
                }
                if !value.is_empty() {
                    flags.push(SearchFlag {
                        name: search_flag,
                        value,
                        exclude,
                    });
                    is_flag = true;
                } else if i < input.len() - 1 {
                    flags.push(SearchFlag {
                        name: search_flag,
                        value: input[i + 1],
                        exclude,
                    });
                    skip_next_word = true;
                    is_flag = true;
                }

                if is_flag {
                    break;
                }
            }
        }

        if !is_flag {
            let exclude = word.starts_with('-');
            let trimmed = trim_search_term(word);
            if !trimmed.is_empty() {
                words.push(SearchWord {
                    value: trimmed,
                    exclude,
                });
            }
        }
    }

    (words, flags)
}

/// The three rewrites `parseSearchFlags` applies to a term, in Go's order: strip leading
/// punctuation, strip trailing punctuation, then collapse a run of leading `#` into one.
fn trim_search_term(word: &str) -> Cow<'_, str> {
    let start = match TERM_PUNC_START.as_ref() {
        Some(re) => re.replace(word, ""),
        None => Cow::Borrowed(word),
    };
    let end = match TERM_PUNC_END.as_ref() {
        Some(re) => match re.replace(&start, "") {
            Cow::Borrowed(_) => start,
            Cow::Owned(owned) => Cow::Owned(owned),
        },
        None => start,
    };
    match collapse_leading_hashes(&end) {
        Cow::Borrowed(_) => end,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

/// Port of `model.ParseSearchParams` (search_params.go:232).
///
/// Returns between zero and **three** blocks: one for the plain terms, one for the hashtag
/// terms, and — only when there are no terms of either kind — one carrying the filters alone.
/// All three share the same filter values, so a `Vec` element is not independent of its
/// siblings in Go either (they are separate structs holding the same backing arrays; ours are
/// separate structs holding clones, which no caller can distinguish because nothing mutates
/// them).
pub fn parse_search_params(text: &str, time_zone_offset: i64) -> Vec<SearchParams> {
    let split = split_words(text);
    let (words, flags) = parse_search_flags(&split);

    let mut hashtag_terms = Vec::new();
    let mut excluded_hashtag_terms = Vec::new();
    let mut plain_terms = Vec::new();
    let mut excluded_plain_terms = Vec::new();

    for word in &words {
        let bucket = match (is_valid_hashtag(&word.value), word.exclude) {
            (true, false) => &mut hashtag_terms,
            (true, true) => &mut excluded_hashtag_terms,
            (false, false) => &mut plain_terms,
            (false, true) => &mut excluded_plain_terms,
        };
        bucket.push(word.value.as_ref());
    }

    let hashtag_terms = hashtag_terms.join(" ");
    let excluded_hashtag_terms = excluded_hashtag_terms.join(" ");
    let plain_terms = plain_terms.join(" ");
    let excluded_plain_terms = excluded_plain_terms.join(" ");

    let mut filters = Filters {
        time_zone_offset,
        ..Filters::default()
    };
    for flag in &flags {
        filters.apply(flag);
    }

    let mut params_list = Vec::new();

    if !plain_terms.is_empty() || !excluded_plain_terms.is_empty() {
        params_list.push(filters.build(plain_terms, excluded_plain_terms, false));
    }

    if !hashtag_terms.is_empty() || !excluded_hashtag_terms.is_empty() {
        params_list.push(filters.build(hashtag_terms, excluded_hashtag_terms, true));
    }

    // Special case: no terms at all, but at least one filter to apply.
    if params_list.is_empty() && filters.any() {
        params_list.push(filters.build(String::new(), String::new(), false));
    }

    params_list
}

/// The filter half of a parsed query, shared by every block `ParseSearchParams` emits. Go keeps
/// these as twelve locals and copies them into each struct literal; grouping them keeps the
/// three literals from drifting apart.
#[derive(Default)]
struct Filters {
    in_channels: StringArray,
    excluded_channels: StringArray,
    from_users: StringArray,
    excluded_users: StringArray,
    after_date: String,
    excluded_after_date: String,
    before_date: String,
    excluded_before_date: String,
    on_date: String,
    excluded_date: String,
    extensions: StringArray,
    excluded_extensions: StringArray,
    time_zone_offset: i64,
}

impl Filters {
    /// The dispatch at search_params.go:274. `channel` is an alias for `in`, and the three date
    /// flags **overwrite** rather than accumulate — so `on:a on:b` keeps `b`.
    fn apply(&mut self, flag: &SearchFlag<'_>) {
        let (list, scalar) = match (flag.name, flag.exclude) {
            ("in" | "channel", false) => (Some(&mut self.in_channels), None),
            ("in" | "channel", true) => (Some(&mut self.excluded_channels), None),
            ("from", false) => (Some(&mut self.from_users), None),
            ("from", true) => (Some(&mut self.excluded_users), None),
            ("ext", false) => (Some(&mut self.extensions), None),
            ("ext", true) => (Some(&mut self.excluded_extensions), None),
            ("after", false) => (None, Some(&mut self.after_date)),
            ("after", true) => (None, Some(&mut self.excluded_after_date)),
            ("before", false) => (None, Some(&mut self.before_date)),
            ("before", true) => (None, Some(&mut self.excluded_before_date)),
            ("on", false) => (None, Some(&mut self.on_date)),
            ("on", true) => (None, Some(&mut self.excluded_date)),
            _ => (None, None),
        };
        if let Some(list) = list {
            list.push(flag.value.to_string());
        }
        if let Some(scalar) = scalar {
            *scalar = flag.value.to_string();
        }
    }

    /// The condition at search_params.go:361 — whether any filter was set at all.
    fn any(&self) -> bool {
        !self.in_channels.is_empty()
            || !self.from_users.is_empty()
            || !self.excluded_channels.is_empty()
            || !self.excluded_users.is_empty()
            || !self.extensions.is_empty()
            || !self.excluded_extensions.is_empty()
            || !self.after_date.is_empty()
            || !self.excluded_after_date.is_empty()
            || !self.before_date.is_empty()
            || !self.excluded_before_date.is_empty()
            || !self.on_date.is_empty()
            || !self.excluded_date.is_empty()
    }

    fn build(&self, terms: String, excluded_terms: String, is_hashtag: bool) -> SearchParams {
        SearchParams {
            terms,
            excluded_terms,
            is_hashtag,
            in_channels: self.in_channels.clone(),
            excluded_channels: self.excluded_channels.clone(),
            from_users: self.from_users.clone(),
            excluded_users: self.excluded_users.clone(),
            after_date: self.after_date.clone(),
            excluded_after_date: self.excluded_after_date.clone(),
            before_date: self.before_date.clone(),
            excluded_before_date: self.excluded_before_date.clone(),
            extensions: self.extensions.clone(),
            excluded_extensions: self.excluded_extensions.clone(),
            on_date: self.on_date.clone(),
            excluded_date: self.excluded_date.clone(),
            time_zone_offset: self.time_zone_offset,
            // Go's three struct literals set none of these, so they stay zero even when the
            // caller's other params had them set.
            or_terms: false,
            include_deleted_channels: false,
            search_without_user_id: false,
            modifier: String::new(),
        }
    }
}

/// Port of `model.IsSearchParamsListValid` (search_params.go:390).
///
/// Every element must agree with the **first** on `include_deleted_channels`. Go indexes
/// `paramsList[0]` inside the loop, which would panic on an empty list — except that the loop
/// body never runs for one, so the empty and nil cases are both valid. Measured under `recover`
/// rather than reasoned about.
pub fn is_search_params_list_valid(params_list: &[SearchParams]) -> AppResult<()> {
    let Some(first) = params_list.first() else {
        return Ok(());
    };
    for params in params_list {
        if params.include_deleted_channels != first.include_deleted_channels {
            return Err(Box::new(AppError::new(
                "IsSearchParamsListValid",
                "model.search_params_list.is_valid.include_deleted_channels.app_error",
                None,
                "",
                500,
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        parse_search_params(text, 0)
            .into_iter()
            .map(|p| p.terms)
            .collect()
    }

    #[test]
    fn the_zero_value_is_one_key() {
        assert_eq!(
            serde_json::to_string(&SearchParams::default()).unwrap(),
            r#"{"modifier":""}"#
        );
    }

    #[test]
    fn an_empty_slice_and_a_nil_one_are_the_same_on_the_wire() {
        // Which is why the slices are not Options, unlike PostList's.
        let empty = SearchParams {
            in_channels: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            serde_json::to_string(&SearchParams::default()).unwrap()
        );
        let decoded: SearchParams = serde_json::from_str(r#"{"in_channels":null}"#).unwrap();
        assert!(decoded.in_channels.is_empty());
    }

    #[test]
    fn a_flag_with_no_value_eats_the_next_word_unless_it_is_last() {
        let eaten = parse_search_params("in: town-square", 0);
        assert_eq!(eaten[0].in_channels, vec!["town-square".to_string()]);

        // At the end of input it is not a flag at all — it becomes the term `in`.
        assert_eq!(terms("in:"), vec!["in".to_string()]);
        assert_eq!(terms("hello in:"), vec!["hello in".to_string()]);
    }

    #[test]
    fn a_leading_wildcard_is_stripped_and_a_trailing_one_is_kept() {
        assert_eq!(terms("*hello"), vec!["hello".to_string()]);
        assert_eq!(terms("hello*"), vec!["hello*".to_string()]);
    }

    #[test]
    fn a_non_ascii_digit_is_punctuation_to_go() {
        // The whole reason the patterns spell `\d` as `[0-9]`: Rust's Unicode `\d` would keep
        // these, Go strips them.
        assert_eq!(terms("\u{663}hello"), vec!["hello".to_string()]);
        assert_eq!(terms("hello\u{663}"), vec!["hello".to_string()]);
        // An ASCII digit is not punctuation.
        assert_eq!(terms("3hello"), vec!["3hello".to_string()]);
    }

    #[test]
    fn a_nbsp_splits_words_and_is_stripped_as_punctuation() {
        // strings.Fields and the trimming regexes disagree about U+00A0, and both are reachable.
        assert_eq!(terms("a\u{a0}b"), vec!["a b".to_string()]);
        assert_eq!(split_words("a\u{a0}b"), vec!["a", "b"]);
    }

    #[test]
    fn a_hyphen_before_a_quote_joins_it() {
        assert_eq!(split_words(r#"-"a b""#), vec![r#"-"a b""#]);
        assert_eq!(split_words(r#"a-"b""#), vec!["a", r#"-"b""#]);
        // An unclosed quote keeps the rest of the input as ordinary words.
        assert_eq!(
            split_words(r#""unclosed phrase"#),
            vec![r#""unclosed"#, "phrase"]
        );
    }

    #[test]
    fn a_one_letter_hashtag_is_a_plain_term() {
        let params = parse_search_params("#a", 0);
        assert_eq!(params.len(), 1);
        assert!(!params[0].is_hashtag);
        assert_eq!(params[0].terms, "#a");

        let params = parse_search_params("#ab", 0);
        assert!(params[0].is_hashtag);
    }

    #[test]
    fn plain_and_hashtag_terms_produce_two_blocks_sharing_the_filters() {
        let params = parse_search_params("#tag word in:town-square", 19800);
        assert_eq!(params.len(), 2);
        assert!(!params[0].is_hashtag && params[0].terms == "word");
        assert!(params[1].is_hashtag && params[1].terms == "#tag");
        for p in &params {
            assert_eq!(p.in_channels, vec!["town-square".to_string()]);
            assert_eq!(p.time_zone_offset, 19800);
        }
    }

    #[test]
    fn a_filter_with_no_terms_produces_one_empty_block() {
        let params = parse_search_params("in:town-square", 0);
        assert_eq!(params.len(), 1);
        assert!(params[0].terms.is_empty() && !params[0].is_hashtag);

        // No terms and no filters is no blocks at all.
        assert!(parse_search_params("   ", 0).is_empty());
        assert!(parse_search_params("::", 0).is_empty());
    }

    #[test]
    fn an_unparseable_before_date_is_zero_and_an_after_date_is_not() {
        let bad = SearchParams {
            before_date: "nonsense".into(),
            after_date: "nonsense".into(),
            ..Default::default()
        };
        assert_eq!(bad.get_before_date_millis(), 0);
        assert_ne!(bad.get_after_date_millis(), 0);
        assert_eq!(bad.get_on_date_millis(), (0, 0));
    }

    #[test]
    fn an_empty_list_is_valid_where_the_go_index_looks_unsafe() {
        assert!(is_search_params_list_valid(&[]).is_ok());
    }
}

/// Parity tests driven by `fixtures/behaviour_search_params.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_search_params.json"
        ))
        .unwrap()
    }

    fn section<'a>(oracle: &'a Value, key: &str) -> &'a [Value] {
        let cases = oracle[key].as_array().unwrap_or_else(|| panic!("{key}"));
        assert!(!cases.is_empty(), "{key} is empty");
        cases
    }

    /// The four patterns, read out of the Go source by the oracle. Asserting against them keeps
    /// the transcriptions below honest about everything *except* the two character classes, which
    /// deliberately differ — hence the explicit rewrites here rather than a raw comparison.
    #[test]
    fn the_patterns_are_the_ones_go_compiles() {
        let oracle = oracle();
        let patterns = &oracle["regexps"];
        assert_eq!(
            patterns["searchTermPuncStart"].as_str().unwrap(),
            r#"^[^\pL\d\s#"]+"#
        );
        assert_eq!(
            patterns["searchTermPuncEnd"].as_str().unwrap(),
            r#"[^\pL\p{M}\d\s*"]+$"#
        );
        assert_eq!(patterns["hashtagStart"].as_str().unwrap(), "^#{2,}");
        assert_eq!(
            patterns["validHashtag"].as_str().unwrap(),
            r"^(#\pL[\pL\d\-_.]*[\pL\d])$"
        );
    }

    /// 168 codepoints through each trimming pattern. This is the test that would have caught a
    /// verbatim transcription of `\d` and `\s`.
    #[test]
    fn the_character_classes_match_go() {
        let oracle = oracle();
        let probes = &oracle["regexp_probes"];

        let mut non_ascii_stripped = 0;
        for case in probes["search_term_punc_start"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            let want = case["out"].as_str().unwrap();
            let re = TERM_PUNC_START.as_ref().unwrap();
            assert_eq!(
                re.replace(input, ""),
                want,
                "searchTermPuncStart U+{:04X}",
                case["rune"].as_i64().unwrap()
            );
            if case["rune"].as_i64().unwrap() >= 0x80 && input != want {
                non_ascii_stripped += 1;
            }
        }
        assert!(
            non_ascii_stripped >= 30,
            "the sweep must actually strip non-ASCII, got {non_ascii_stripped}"
        );

        for case in probes["search_term_punc_end"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            let re = TERM_PUNC_END.as_ref().unwrap();
            assert_eq!(
                re.replace(input, ""),
                case["out"].as_str().unwrap(),
                "searchTermPuncEnd U+{:04X}",
                case["rune"].as_i64().unwrap()
            );
        }
    }

    #[test]
    fn the_word_corpus_matches_go() {
        let oracle = oracle();
        for case in oracle["regexp_probes"]["words"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                TERM_PUNC_START.as_ref().unwrap().replace(input, ""),
                case["search_term_punc_start"].as_str().unwrap(),
                "punc_start {input:?}"
            );
            assert_eq!(
                TERM_PUNC_END.as_ref().unwrap().replace(input, ""),
                case["search_term_punc_end"].as_str().unwrap(),
                "punc_end {input:?}"
            );
            assert_eq!(
                collapse_leading_hashes(input),
                case["hashtag_start"].as_str().unwrap(),
                "hashtag_start {input:?}"
            );
            assert_eq!(
                is_valid_hashtag(input),
                case["valid_hashtag"].as_bool().unwrap(),
                "valid_hashtag {input:?}"
            );
            assert_eq!(
                trim_search_term(input),
                case["trimmed"].as_str().unwrap(),
                "the composition, {input:?}"
            );
        }
    }

    /// `strings.Fields` against `str::split_whitespace`. They agree on the whole sweep — which is
    /// worth pinning precisely because the `\s` two lines away in the same Go file does not.
    #[test]
    fn strings_fields_matches_split_whitespace() {
        let oracle = oracle();
        let fields = &oracle["strings_fields"];

        let mut splitters = 0;
        for case in fields["sweep"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            let want: Vec<&str> = case["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(
                input.split_whitespace().collect::<Vec<_>>(),
                want,
                "U+{:04X}",
                case["rune"].as_i64().unwrap()
            );
            if want.len() > 1 {
                splitters += 1;
            }
        }
        assert!(
            splitters >= 15,
            "expected the whitespace set, got {splitters}"
        );

        for case in fields["corpus"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            let want: Vec<&str> = case["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(
                input.split_whitespace().collect::<Vec<_>>(),
                want,
                "{input:?}"
            );
        }
    }

    #[test]
    fn pad_date_string_zeros_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "pad_date_string_zeros") {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                pad_date_string_zeros(input),
                case["out"].as_str().unwrap(),
                "{input:?}"
            );
        }
    }

    /// The offsets here run past ±86400, which `chrono::FixedOffset` cannot represent — the
    /// reason `get_start_of_day_millis` does the arithmetic itself.
    #[test]
    fn the_day_bounds_match_go_at_any_offset() {
        let oracle = oracle();
        let mut beyond_a_day = 0;
        for case in section(&oracle, "day_millis") {
            let offset = case["offset"].as_i64().unwrap();
            let date = NaiveDate::from_ymd_opt(
                case["year"].as_i64().unwrap() as i32,
                case["month"].as_u64().unwrap() as u32,
                case["day"].as_u64().unwrap() as u32,
            )
            .unwrap();
            let what = format!("{} @ {offset}", case["name"].as_str().unwrap());

            assert_eq!(
                start_of_day(date, offset),
                case["start"].as_i64(),
                "start {what}"
            );
            assert_eq!(end_of_day(date, offset), case["end"].as_i64(), "end {what}");
            if offset.abs() > 86_400 {
                beyond_a_day += 1;
            }
        }
        assert!(
            beyond_a_day > 0,
            "the corpus must exercise offsets chrono cannot hold"
        );
    }

    #[test]
    fn the_date_accessors_match_go() {
        let oracle = oracle();
        let mut clock_cases = 0;
        for case in section(&oracle, "date_millis") {
            let date = case["date"].as_str().unwrap().to_string();
            let offset = case["offset"].as_i64().unwrap();
            let what = format!("{date:?} @ {offset}");

            let with = |set: fn(&mut SearchParams, String)| {
                let mut p = SearchParams {
                    time_zone_offset: offset,
                    ..Default::default()
                };
                set(&mut p, date.clone());
                p
            };

            let before = with(|p, d| p.before_date = d);
            assert_eq!(
                before.get_before_date_millis(),
                case["before"].as_i64().unwrap(),
                "before {what}"
            );
            let excluded_before = with(|p, d| p.excluded_before_date = d);
            assert_eq!(
                excluded_before.get_excluded_before_date_millis(),
                case["excluded_before"].as_i64().unwrap(),
                "excluded_before {what}"
            );

            let on = with(|p, d| p.on_date = d);
            assert_eq!(
                on.get_on_date_millis(),
                (
                    case["on_start"].as_i64().unwrap(),
                    case["on_end"].as_i64().unwrap()
                ),
                "on {what}"
            );
            let excluded_on = with(|p, d| p.excluded_date = d);
            assert_eq!(
                excluded_on.get_excluded_date_millis(),
                (
                    case["excluded_start"].as_i64().unwrap(),
                    case["excluded_end"].as_i64().unwrap()
                ),
                "excluded {what}"
            );

            let after = with(|p, d| p.after_date = d);
            let excluded_after = with(|p, d| p.excluded_after_date = d);
            if case["uses_now"].as_bool() == Some(true) {
                // Clock-dependent in Go, so the fixture records no value. Recompute both sides:
                // the answer must be the start of the day after *today*, in the same offset.
                clock_cases += 1;
                let now = Local::now();
                let expected = |d: NaiveDate| start_of_day(d, offset).unwrap();
                let today = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day()).unwrap();
                let answers = [
                    expected(today.succ_opt().unwrap()),
                    // Tolerate the test crossing local midnight between the two reads.
                    expected(today.succ_opt().unwrap().succ_opt().unwrap()),
                ];
                assert!(
                    answers.contains(&after.get_after_date_millis()),
                    "after {what}: clock fallback"
                );
                assert!(
                    answers.contains(&excluded_after.get_excluded_after_date_millis()),
                    "excluded_after {what}: clock fallback"
                );
            } else {
                assert_eq!(
                    after.get_after_date_millis(),
                    case["after"].as_i64().unwrap(),
                    "after {what}"
                );
                assert_eq!(
                    excluded_after.get_excluded_after_date_millis(),
                    case["excluded_after"].as_i64().unwrap(),
                    "excluded_after {what}"
                );
            }
        }
        assert!(
            clock_cases > 0,
            "the corpus must exercise the clock fallback"
        );
    }

    /// The end-to-end test, and the only evidence for `splitWords` and `parseSearchFlags` — both
    /// are unexported in Go, so the composition is all the oracle can reach.
    #[test]
    fn parse_search_params_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "parse_search_params") {
            let input = case["in"].as_str().unwrap();
            let offset = case["offset"].as_i64().unwrap();
            assert!(
                !case["panicked"].as_bool().unwrap(),
                "{input:?}: Go panicked"
            );

            let ours = parse_search_params(input, offset);
            assert_eq!(
                ours.len() as u64,
                case["count"].as_u64().unwrap(),
                "block count for {input:?}"
            );
            // Byte-for-byte: this is the shape a search endpoint hands to the store layer.
            assert_eq!(
                go_json_marshal(&ours).unwrap(),
                case["out"].as_str().unwrap(),
                "{input:?} @ {offset}"
            );
        }
    }

    #[test]
    fn is_search_params_list_valid_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "is_valid_list") {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let yes = SearchParams {
                include_deleted_channels: true,
                ..Default::default()
            };
            let no = SearchParams::default();
            let list: Vec<SearchParams> = match name {
                "nil" | "empty" => vec![],
                "one_true" => vec![yes],
                "one_false" => vec![no],
                "all_true" => vec![yes.clone(), yes.clone(), yes],
                "all_false" => vec![no.clone(), no],
                "mixed_true_first" => vec![yes, no],
                "mixed_false_first" => vec![no, yes],
                "mixed_late" => vec![no.clone(), no.clone(), no, yes],
                other => panic!("unhandled list {other}"),
            };

            match (is_search_params_list_valid(&list), case["err"].as_object()) {
                (Ok(()), None) => {}
                (Err(err), Some(want)) => {
                    assert_eq!(err.id, want["id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(err.where_, want["where"].as_str().unwrap(), "{name}: where");
                    assert_eq!(
                        i64::from(err.status_code),
                        want["status_code"].as_i64().unwrap(),
                        "{name}: status"
                    );
                    assert_eq!(
                        err.message,
                        want["message"].as_str().unwrap(),
                        "{name}: message"
                    );
                }
                (ours, want) => panic!("{name}: ours {ours:?}, Go {want:?}"),
            }
        }
    }

    /// The two documents Go accepts and we reject: `null` into a **scalar**. Listed by name
    /// rather than detected, so adding a corpus case cannot silently join the exemption.
    const NULL_SCALAR_ONLY: [&str; 2] = [
        r#"{"terms":null,"modifier":null}"#,
        r#"{"ishashtag":null,"timezone_offset":null}"#,
    ];

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "wire") {
            if case["kind"].as_str().unwrap() != "round_trip" {
                continue;
            }
            let input = case["in"].as_str().unwrap();
            if NULL_SCALAR_ONLY.contains(&input) {
                continue;
            }
            let decoded: SearchParams = serde_json::from_str(input).unwrap();
            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["out"].as_str().unwrap(),
                "{input}"
            );
            assert!(
                decoded.in_channels.is_empty() || !case["in_channels_nil"].as_bool().unwrap(),
                "{input}: in_channels"
            );
        }
    }

    /// [D-057]: Go's `encoding/json` leaves the destination untouched on `null` for **every**
    /// type, not only slices and pointers. The slice fields are handled (`null_as_empty`),
    /// because every other nullable slice in the crate decodes `null` via an `Option` and
    /// leaving these out would be the inconsistency. The scalars are not, because no ported type
    /// accepts a null scalar and fixing one type would be.
    ///
    /// Asserted rather than skipped: Go's answer for both documents is `{"modifier":""}`, so if
    /// the crate ever gains a null-tolerant scalar convention this test fails and the exemption
    /// can be deleted.
    #[test]
    fn a_null_scalar_is_accepted_by_go_and_rejected_here() {
        let oracle = oracle();
        for input in NULL_SCALAR_ONLY {
            let case = section(&oracle, "wire")
                .iter()
                .find(|c| c["in"].as_str() == Some(input))
                .unwrap_or_else(|| panic!("{input} is missing from the corpus"));

            assert!(case["err"].is_null(), "Go rejected {input}");
            assert_eq!(case["out"].as_str().unwrap(), r#"{"modifier":""}"#);
            assert!(
                serde_json::from_str::<SearchParams>(input).is_err(),
                "{input}: we accepted it, so [D-057] can be closed"
            );
        }

        // The slice half *is* closed, and this is what proves the two are treated differently.
        let decoded: SearchParams =
            serde_json::from_str(r#"{"in_channels":null,"extensions":null}"#).unwrap();
        assert!(decoded.in_channels.is_empty() && decoded.extensions.is_empty());
    }
}

/// Serialization parity against `fixtures/search_params.json` — every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/search_params.json");
        let decoded: SearchParams = serde_json::from_str(raw).unwrap();

        // Every omitempty field must have survived, or the round trip proves nothing about it.
        assert!(!decoded.terms.is_empty());
        assert!(!decoded.in_channels.is_empty());
        assert!(decoded.is_hashtag && decoded.or_terms && decoded.search_without_user_id);
        assert_ne!(decoded.time_zone_offset, 0);

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}
