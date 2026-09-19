//! Port of `github.com/dyatlov/go-opengraph/opengraph@v0.0.0-20220524092352-606d7b1e5f8a`
//! (MIT) — the third-party package `model.LinkMetadata` and the link-preview path carry
//! OpenGraph data in. [D-105] decided in 2026-09-06 to forward rather than port it; that is
//! reversed now that `createPost` needs the path, and the risk it named — matching a library's
//! struct tags rather than the OpenGraph specification — is paid by porting **this** library,
//! tag for tag, and asserting Go's own `json.Marshal` bytes against it
//! (`fixtures/behaviour_opengraph.json`).
//!
//! # What reaches the wire, and what does not
//!
//! Field order is Go's declaration order and every `omitempty` is reproduced, because an
//! `opengraph` embed's `data` is this struct marshalled. Three fields are Go-unexported and
//! never serialised — `isArticle`, `isBook`, `isProfile` — but they steer [`OpenGraph::process_meta`]:
//! `og:article:*` tags are only read once `og:type` has said `article`, **in document order**.
//!
//! Nil and empty are different on the wire here: `locales_alternate`, `images`, `audios` and
//! `videos` are plain slices, so an untouched one marshals as `null`, which is why they are
//! `Option<Vec<_>>`. A `Video`'s own slices carry `omitempty`, so theirs are plain `Vec`s.
//!
//! # `*time.Time`
//!
//! [`GoTime`] keeps the wall clock and the offset `time.Parse(time.RFC3339, …)` produced, which is
//! all `MarshalJSON` prints. An offset hour of 24 parses (Go's general parser allows `+24:00`)
//! but cannot be marshalled — Go's `json.Marshal` then **fails** for the whole value.
//! [`OpenGraph::marshal_fails_in_go`] reports that so a caller can refuse rather than emit JSON Go
//! could not have produced.

use serde::{Deserialize, Serialize};

use crate::go_html::{TokenType, Tokenizer};

// --- *time.Time --------------------------------------------------------------------------------

/// A `time.Time` as `time.Parse(time.RFC3339, …)` leaves it: the wall clock in the parsed zone,
/// and that zone's offset. Only what `MarshalJSON` prints is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    pub nsec: u32,
    /// Seconds east of UTC.
    pub offset: i32,
}

fn days_in(month: u32, year: i32) -> u32 {
    match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `getnum` (time/format.go:926).
fn getnum(s: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    let d0 = *s.first()?;
    if !d0.is_ascii_digit() {
        return None;
    }
    match s.get(1) {
        Some(d1) if d1.is_ascii_digit() => {
            Some((u32::from(d0 - b'0') * 10 + u32::from(d1 - b'0'), &s[2..]))
        }
        _ if fixed => None,
        _ => Some((u32::from(d0 - b'0'), &s[1..])),
    }
}

impl GoTime {
    /// `time.Parse(time.RFC3339, s)` — the general parser for layout `2006-01-02T15:04:05Z07:00`
    /// (time/format.go:1048), which accepts everything the fast path `parseRFC3339` does and a
    /// little more: a one-digit hour, a comma before the fraction, and zone offsets up to
    /// `±24:60`.
    pub fn parse_rfc3339(s: &str) -> Option<GoTime> {
        let v = s.as_bytes();
        // stdLongYear: four bytes, the first a digit, all of them through `atoi` — which with a
        // digit first means four digits.
        if v.len() < 4 || !v[..4].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let year: i32 = std::str::from_utf8(&v[..4]).ok()?.parse().ok()?;
        let v = v[4..].strip_prefix(b"-")?;
        let (month, v) = getnum(v, true)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        let v = v.strip_prefix(b"-")?;
        let (day, v) = getnum(v, true)?;
        let v = v.strip_prefix(b"T")?;
        let (hour, v) = getnum(v, false)?;
        if hour >= 24 {
            return None;
        }
        let v = v.strip_prefix(b":")?;
        let (min, v) = getnum(v, true)?;
        if min >= 60 {
            return None;
        }
        let v = v.strip_prefix(b":")?;
        let (sec, mut v) = getnum(v, true)?;
        if sec >= 60 {
            return None;
        }
        let mut nsec = 0;
        if v.len() >= 2 && matches!(v[0], b'.' | b',') && v[1].is_ascii_digit() {
            let mut n = 2;
            while n < v.len() && v[n].is_ascii_digit() {
                n += 1;
            }
            // parseNanoseconds: at most nine digits count; the rest are consumed and ignored.
            let digits = &v[1..n.min(10)];
            let mut ns: u32 = 0;
            for d in digits {
                ns = ns * 10 + u32::from(d - b'0');
            }
            for _ in digits.len()..9 {
                ns *= 10;
            }
            nsec = ns;
            v = &v[n..];
        }
        let offset = if let Some(rest) = v.strip_prefix(b"Z") {
            v = rest;
            0
        } else {
            if v.len() < 6 || v[3] != b':' {
                return None;
            }
            let (hr, _) = getnum(&v[1..3], true)?;
            let (mm, _) = getnum(&v[4..6], true)?;
            if hr > 24 || mm > 60 {
                return None;
            }
            let off = i32::try_from((hr * 60 + mm) * 60).ok()?;
            let off = match v[0] {
                b'+' => off,
                b'-' => -off,
                _ => return None,
            };
            v = &v[6..];
            off
        };
        if !v.is_empty() {
            return None;
        }
        if day < 1 || day > days_in(month, year) {
            return None;
        }
        Some(GoTime {
            year,
            month,
            day,
            hour,
            min,
            sec,
            nsec,
            offset,
        })
    }

    /// `Time.MarshalJSON`'s text (RFC 3339 with nanoseconds, trailing zeros trimmed), or `None`
    /// where Go's `appendStrictRFC3339` refuses: an offset whose hour is 24 or more.
    pub fn format_rfc3339_nano(&self) -> Option<String> {
        let mut out = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        );
        if self.nsec != 0 {
            let frac = format!("{:09}", self.nsec);
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
        if self.offset == 0 {
            out.push('Z');
            return Some(out);
        }
        let zone = self.offset / 60;
        let (sign, zone) = if zone < 0 { ('-', -zone) } else { ('+', zone) };
        if zone / 60 >= 24 {
            return None;
        }
        out.push_str(&format!("{sign}{:02}:{:02}", zone / 60, zone % 60));
        Some(out)
    }
}

impl Serialize for GoTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.format_rfc3339_nano() {
            Some(text) => s.serialize_str(&text),
            None => Err(serde::ser::Error::custom(
                "Time.MarshalJSON: timezone hour outside of range [0,23]",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for GoTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        GoTime::parse_rfc3339(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("cannot parse {s:?} as RFC 3339")))
    }
}

// --- the types ---------------------------------------------------------------------------------

/// Port of `image.Image` (types/image/image.go:4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Image {
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "secure_url")]
    pub secure_url: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "width")]
    pub width: u64,
    #[serde(rename = "height")]
    pub height: u64,
}

/// Port of `audio.Audio` (types/audio/audio.go:4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "secure_url")]
    pub secure_url: String,
    #[serde(rename = "type")]
    pub type_: String,
}

/// Port of `actor.Actor` (types/actor/actor.go:4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Actor {
    #[serde(rename = "profile")]
    pub profile: String,
    #[serde(rename = "role")]
    pub role: String,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Port of `video.Video` (types/video/video.go:9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Video {
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "secure_url")]
    pub secure_url: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "width")]
    pub width: u64,
    #[serde(rename = "height")]
    pub height: u64,
    #[serde(
        rename = "actors",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub actors: Vec<Actor>,
    #[serde(
        rename = "directors",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub directors: Vec<String>,
    #[serde(
        rename = "writers",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub writers: Vec<String>,
    #[serde(rename = "duration", skip_serializing_if = "is_zero")]
    pub duration: u64,
    #[serde(rename = "release_date", skip_serializing_if = "Option::is_none")]
    pub release_date: Option<GoTime>,
    #[serde(
        rename = "tags",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub tags: Vec<String>,
}

fn null_vec<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// Port of `article.Article` (types/article/article.go:8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Article {
    #[serde(rename = "published_time")]
    pub published_time: Option<GoTime>,
    #[serde(rename = "modified_time")]
    pub modified_time: Option<GoTime>,
    #[serde(rename = "expiration_time")]
    pub expiration_time: Option<GoTime>,
    #[serde(rename = "section")]
    pub section: String,
    #[serde(rename = "tags")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "authors")]
    pub authors: Option<Vec<String>>,
}

/// Port of `book.Book` (types/book/book.go:8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    #[serde(rename = "isbn")]
    pub isbn: String,
    #[serde(rename = "release_date")]
    pub release_date: Option<GoTime>,
    #[serde(rename = "tags")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "authors")]
    pub authors: Option<Vec<String>>,
}

/// Port of `profile.Profile` (types/profile/profile.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    #[serde(rename = "first_name")]
    pub first_name: String,
    #[serde(rename = "last_name")]
    pub last_name: String,
    #[serde(rename = "username")]
    pub username: String,
    #[serde(rename = "gender")]
    pub gender: String,
}

/// Port of `music.Album` (types/music/music.go:18).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Album {
    #[serde(rename = "url", skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(rename = "disc", skip_serializing_if = "is_zero")]
    pub disc: u64,
    #[serde(rename = "track", skip_serializing_if = "is_zero")]
    pub track: u64,
}

/// Port of `music.Song` (types/music/music.go:24). Same shape as [`Album`].
pub type Song = Album;

/// Port of `music.Music` (types/music/music.go:9). **Not** blanked by `TruncateOpenGraph`, so
/// it is the one nested object that survives into a link preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Music {
    #[serde(
        rename = "musicians",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub musicians: Vec<String>,
    #[serde(
        rename = "creators",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    pub creators: Vec<String>,
    #[serde(rename = "duration", skip_serializing_if = "is_zero")]
    pub duration: u64,
    #[serde(rename = "release_date", skip_serializing_if = "Option::is_none")]
    pub release_date: Option<GoTime>,
    /// `*Album` without `omitempty`: `NewMusic` always allocates one.
    #[serde(rename = "album")]
    pub album: Option<Album>,
    #[serde(rename = "songs")]
    pub songs: Option<Vec<Song>>,
}

/// Port of `opengraph.OpenGraph` (opengraph.go:22).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenGraph {
    #[serde(skip)]
    is_article: bool,
    #[serde(skip)]
    is_book: bool,
    #[serde(skip)]
    is_profile: bool,
    /// A `content` value this parse stored was not valid UTF-8. Go keeps those bytes in its
    /// strings (and only `json.Marshal` turns them into U+FFFD, per byte); the lossy conversion
    /// here agrees on the wire, but not with what Go's URL resolution and truncation see first.
    #[serde(skip)]
    pub lossy: bool,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "title")]
    pub title: String,
    #[serde(rename = "description")]
    pub description: String,
    #[serde(rename = "determiner")]
    pub determiner: String,
    #[serde(rename = "site_name")]
    pub site_name: String,
    #[serde(rename = "locale")]
    pub locale: String,
    #[serde(rename = "locales_alternate")]
    pub locales_alternate: Option<Vec<String>>,
    #[serde(rename = "images")]
    pub images: Option<Vec<Image>>,
    #[serde(rename = "audios")]
    pub audios: Option<Vec<Audio>>,
    #[serde(rename = "videos")]
    pub videos: Option<Vec<Video>>,
    #[serde(rename = "article", skip_serializing_if = "Option::is_none")]
    pub article: Option<Article>,
    #[serde(rename = "book", skip_serializing_if = "Option::is_none")]
    pub book: Option<Book>,
    #[serde(rename = "profile", skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    #[serde(rename = "music", skip_serializing_if = "Option::is_none")]
    pub music: Option<Music>,
}

/// `strconv.ParseUint(s, 10, 64)`: ASCII digits only, no sign, no underscores, no overflow.
fn parse_uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A Go string's bytes as a Rust string: each invalid byte becomes U+FFFD — **one per byte**, which
/// is what `json.Marshal` writes for it and what `json.Unmarshal` reads it as, and not the W3C
/// maximal-subpart rule of [`String::from_utf8_lossy`]. The flag says whether anything was
/// replaced.
pub fn go_string(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), false),
        Err(_) => {
            let mut out = String::with_capacity(bytes.len());
            let mut rest = bytes;
            while !rest.is_empty() {
                match std::str::from_utf8(rest) {
                    Ok(s) => {
                        out.push_str(s);
                        break;
                    }
                    Err(err) => {
                        let (valid, after) = rest.split_at(err.valid_up_to());
                        out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                        out.push('\u{FFFD}');
                        rest = &after[1..];
                    }
                }
            }
            (out, true)
        }
    }
}

/// `images[len(images)-1]`, after `ensureHasImage`-style growth.
fn last<T: Default>(v: &mut Option<Vec<T>>, grow: bool) -> &mut T {
    let list = v.get_or_insert_with(Vec::new);
    if grow || list.is_empty() {
        list.push(T::default());
    }
    let n = list.len();
    &mut list[n - 1]
}

impl OpenGraph {
    /// Port of `ProcessHTML` (opengraph.go:65): every start, self-closing **or end** tag named
    /// `meta` with attributes goes to [`OpenGraph::process_meta`] — though an end tag never
    /// carries attributes the tokenizer returns, so only the first two count.
    pub fn process_html(&mut self, buffer: &[u8]) {
        let mut z = Tokenizer::new(buffer);
        loop {
            match z.next_token() {
                TokenType::Error => return,
                TokenType::StartTag | TokenType::SelfClosingTag | TokenType::EndTag => {
                    let (name, mut has_attr) = z.tag_name();
                    if name.as_deref() != Some(b"meta".as_slice()) || !has_attr {
                        continue;
                    }
                    let mut property = None;
                    let mut content = None;
                    while has_attr {
                        let (key, val, more) = z.tag_attr();
                        has_attr = more;
                        match key.as_slice() {
                            b"property" => property = Some(val),
                            b"content" => content = Some(val),
                            _ => {}
                        }
                    }
                    let (property, _) = go_string(property.as_deref().unwrap_or_default());
                    let (content, lossy) = go_string(content.as_deref().unwrap_or_default());
                    self.lossy |= lossy;
                    self.process_meta(&property, &content);
                }
                _ => {}
            }
        }
    }

    fn ensure_has_video(&mut self) -> &mut Video {
        last(&mut self.videos, false)
    }

    fn ensure_has_music(&mut self) -> &mut Music {
        self.music.get_or_insert_with(|| Music {
            album: Some(Album::default()),
            ..Music::default()
        })
    }

    /// Port of `ProcessMeta` (opengraph.go:98), with `metaAttrs["property"]` and
    /// `metaAttrs["content"]` passed directly (a missing attribute is `""`).
    pub fn process_meta(&mut self, property: &str, content: &str) {
        let c = content.to_owned();
        match property {
            "og:description" => self.description = c,
            "og:type" => {
                self.type_ = c;
                match self.type_.as_str() {
                    "article" => self.is_article = true,
                    "book" => self.is_book = true,
                    "profile" => self.is_profile = true,
                    _ => {}
                }
            }
            "og:title" => self.title = c,
            "og:url" => self.url = c,
            "og:determiner" => self.determiner = c,
            "og:site_name" => self.site_name = c,
            "og:locale" => self.locale = c,
            "og:locale:alternate" => self.locales_alternate.get_or_insert_with(Vec::new).push(c),
            // audio.AddUrl / AddSecureUrl / AddType: a new entry whenever the field is taken.
            "og:audio" => {
                let grow = self
                    .audios
                    .as_ref()
                    .is_some_and(|a| a.last().is_some_and(|x| !x.url.is_empty()));
                last(&mut self.audios, grow).url = c;
            }
            "og:audio:secure_url" => {
                let grow = self
                    .audios
                    .as_ref()
                    .is_some_and(|a| a.last().is_some_and(|x| !x.secure_url.is_empty()));
                last(&mut self.audios, grow).secure_url = c;
            }
            "og:audio:type" => {
                let grow = self
                    .audios
                    .as_ref()
                    .is_some_and(|a| a.last().is_some_and(|x| !x.type_.is_empty()));
                last(&mut self.audios, grow).type_ = c;
            }
            // image.AddURL: a new image unless the last one has no URL or the same URL.
            "og:image" | "og:image:url" => {
                let grow = self
                    .images
                    .as_ref()
                    .is_some_and(|a| a.last().is_some_and(|x| !x.url.is_empty() && x.url != c));
                last(&mut self.images, grow).url = c;
            }
            "og:image:secure_url" => last(&mut self.images, false).secure_url = c,
            "og:image:type" => last(&mut self.images, false).type_ = c,
            "og:image:width" => {
                if let Some(w) = parse_uint(&c) {
                    last(&mut self.images, false).width = w;
                }
            }
            "og:image:height" => {
                if let Some(h) = parse_uint(&c) {
                    last(&mut self.images, false).height = h;
                }
            }
            "og:video" | "og:video:url" => {
                let grow = self
                    .videos
                    .as_ref()
                    .is_some_and(|a| a.last().is_some_and(|x| !x.url.is_empty() && x.url != c));
                last(&mut self.videos, grow).url = c;
            }
            // `og:video:type` goes to AddTag, not AddType — a bug in the library, kept.
            "og:video:tag" | "og:video:type" => last(&mut self.videos, false).tags.push(c),
            "og:video:duration" => {
                if let Some(i) = parse_uint(&c) {
                    last(&mut self.videos, false).duration = i;
                }
            }
            "og:video:release_date" => {
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    last(&mut self.videos, false).release_date = Some(t);
                }
            }
            "og:video:secure_url" => last(&mut self.videos, false).secure_url = c,
            "og:video:width" => {
                if let Some(w) = parse_uint(&c) {
                    last(&mut self.videos, false).width = w;
                }
            }
            "og:video:height" => {
                if let Some(h) = parse_uint(&c) {
                    last(&mut self.videos, false).height = h;
                }
            }
            "og:video:actor" => {
                let v = self.ensure_has_video();
                if v.actors.last().is_none_or(|a| !a.profile.is_empty()) {
                    v.actors.push(Actor::default());
                }
                if let Some(a) = v.actors.last_mut() {
                    a.profile = c;
                }
            }
            "og:video:actor:role" => {
                let v = self.ensure_has_video();
                if v.actors.last().is_none_or(|a| !a.role.is_empty()) {
                    v.actors.push(Actor::default());
                }
                if let Some(a) = v.actors.last_mut() {
                    a.role = c;
                }
            }
            "og:video:director" => self.ensure_has_video().directors.push(c),
            "og:video:writer" => self.ensure_has_video().writers.push(c),
            "og:music:duration" => {
                let m = self.ensure_has_music();
                if let Some(i) = parse_uint(&c) {
                    m.duration = i;
                }
            }
            "og:music:release_date" => {
                let m = self.ensure_has_music();
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    m.release_date = Some(t);
                }
            }
            "og:music:album" => {
                self.ensure_has_music()
                    .album
                    .get_or_insert_with(Album::default)
                    .url = c;
            }
            "og:music:album:disc" => {
                let m = self.ensure_has_music();
                if let Some(i) = parse_uint(&c) {
                    m.album.get_or_insert_with(Album::default).disc = i;
                }
            }
            "og:music:album:track" => {
                let m = self.ensure_has_music();
                if let Some(i) = parse_uint(&c) {
                    m.album.get_or_insert_with(Album::default).track = i;
                }
            }
            "og:music:musician" => self.ensure_has_music().musicians.push(c),
            "og:music:creator" => self.ensure_has_music().creators.push(c),
            "og:music:song" => {
                let m = self.ensure_has_music();
                let grow = m
                    .songs
                    .as_ref()
                    .is_some_and(|s| s.last().is_some_and(|x| !x.url.is_empty()));
                last(&mut m.songs, grow).url = c;
            }
            "og:music:disc" => {
                let m = self.ensure_has_music();
                if let Some(i) = parse_uint(&c) {
                    last(&mut m.songs, false).disc = i;
                }
            }
            "og:music:track" => {
                let m = self.ensure_has_music();
                if let Some(i) = parse_uint(&c) {
                    last(&mut m.songs, false).track = i;
                }
            }
            _ => {
                if self.is_article {
                    self.process_article_meta(property, c);
                } else if self.is_book {
                    self.process_book_meta(property, c);
                } else if self.is_profile {
                    self.process_profile_meta(property, c);
                }
            }
        }
    }

    /// Port of `processArticleMeta` (opengraph.go:235). The article is allocated by **any**
    /// unrecognised property once the type is `article`, matched or not.
    fn process_article_meta(&mut self, property: &str, c: String) {
        let a = self.article.get_or_insert_with(Article::default);
        match property {
            "og:article:published_time" => {
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    a.published_time = Some(t);
                }
            }
            "og:article:modified_time" => {
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    a.modified_time = Some(t);
                }
            }
            "og:article:expiration_time" => {
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    a.expiration_time = Some(t);
                }
            }
            "og:article:section" => a.section = c,
            "og:article:tag" => a.tags.get_or_insert_with(Vec::new).push(c),
            "og:article:author" => a.authors.get_or_insert_with(Vec::new).push(c),
            _ => {}
        }
    }

    /// Port of `processBookMeta` (opengraph.go:264).
    fn process_book_meta(&mut self, property: &str, c: String) {
        let b = self.book.get_or_insert_with(Book::default);
        match property {
            "og:book:release_date" => {
                if let Some(t) = GoTime::parse_rfc3339(&c) {
                    b.release_date = Some(t);
                }
            }
            "og:book:isbn" => b.isbn = c,
            "og:book:tag" => b.tags.get_or_insert_with(Vec::new).push(c),
            "og:book:author" => b.authors.get_or_insert_with(Vec::new).push(c),
            _ => {}
        }
    }

    /// Port of `processProfileMeta` (opengraph.go:283).
    fn process_profile_meta(&mut self, property: &str, c: String) {
        let p = self.profile.get_or_insert_with(Profile::default);
        match property {
            "og:profile:first_name" => p.first_name = c,
            "og:profile:last_name" => p.last_name = c,
            "og:profile:username" => p.username = c,
            "og:profile:gender" => p.gender = c,
            _ => {}
        }
    }

    /// Whether Go's `json.Marshal` of this value fails: a `*time.Time` anywhere in it whose
    /// offset hour is 24 or more (see the module docs).
    pub fn marshal_fails_in_go(&self) -> bool {
        let bad = |t: &Option<GoTime>| {
            t.as_ref()
                .is_some_and(|t| t.format_rfc3339_nano().is_none())
        };
        self.videos.iter().flatten().any(|v| bad(&v.release_date))
            || self.article.as_ref().is_some_and(|a| {
                bad(&a.published_time) || bad(&a.modified_time) || bad(&a.expiration_time)
            })
            || self.book.as_ref().is_some_and(|b| bad(&b.release_date))
            || self.music.as_ref().is_some_and(|m| bad(&m.release_date))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn og(html: &str) -> OpenGraph {
        let mut og = OpenGraph::default();
        og.process_html(html.as_bytes());
        og
    }

    #[test]
    fn a_repeated_image_url_does_not_start_a_new_image() {
        let g = og(
            r#"<meta property="og:image" content="a"><meta property="og:image:width" content="3">
            <meta property="og:image" content="a"><meta property="og:image" content="b">"#,
        );
        let images = g.images.unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].width, 3);
        assert_eq!(images[1].url, "b");
    }

    #[test]
    fn article_tags_count_only_after_the_type() {
        let g = og(r#"<meta property="og:article:section" content="early">
            <meta property="og:type" content="article"><meta property="og:article:section" content="late">"#);
        assert_eq!(g.article.unwrap().section, "late");
    }

    /// `ensureHasMusic` is `NewMusic`, which allocates the album — so any music tag, even one
    /// that names no album, marshals `"album":{}` rather than `null`.
    #[test]
    fn any_music_tag_allocates_an_empty_album() {
        let g = og(r#"<meta property="og:music:disc" content="4">"#);
        let music = g.music.unwrap();
        assert_eq!(music.album, Some(Album::default()));
        assert_eq!(music.songs.unwrap()[0].disc, 4);
    }

    #[test]
    fn a_non_numeric_width_is_ignored() {
        let g = og(
            r#"<meta property="og:image:width" content="+5"><meta property="og:image:height" content="7">"#,
        );
        assert_eq!(
            g.images.unwrap()[0],
            Image {
                height: 7,
                ..Image::default()
            }
        );
    }

    #[test]
    fn times_keep_their_offset_and_trim_the_fraction() {
        let t = GoTime::parse_rfc3339("2020-02-29T5:04:05,120+05:30").unwrap();
        assert_eq!(
            t.format_rfc3339_nano().unwrap(),
            "2020-02-29T05:04:05.12+05:30"
        );
        assert!(GoTime::parse_rfc3339("2021-02-29T05:04:05Z").is_none());
        let t = GoTime::parse_rfc3339("2020-01-01T00:00:00+24:00").unwrap();
        assert!(t.format_rfc3339_nano().is_none());
        let t = GoTime::parse_rfc3339("2020-01-01T00:00:00-00:00").unwrap();
        assert_eq!(t.format_rfc3339_nano().unwrap(), "2020-01-01T00:00:00Z");
    }

    #[test]
    fn the_empty_parse_marshals_every_slice_as_null() {
        assert_eq!(
            serde_json::to_string(&OpenGraph::default()).unwrap(),
            r#"{"type":"","url":"","title":"","description":"","determiner":"","site_name":"","locale":"","locales_alternate":null,"images":null,"audios":null,"videos":null}"#
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    /// `time.Parse(time.RFC3339, …)` and the JSON of the `*time.Time` it produced.
    #[test]
    fn rfc3339_parse_and_marshal_match_go() {
        let o: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_opengraph.json"))
                .expect("JSON");
        for case in o["times"].as_array().expect("times") {
            let input = case["in"].as_str().expect("in");
            let ours = GoTime::parse_rfc3339(input);
            assert_eq!(
                ours.is_some(),
                case["ok"].as_bool().expect("ok"),
                "{input:?}"
            );
            if let Some(t) = ours {
                match serde_json::to_string(&t) {
                    Ok(json) => assert_eq!(json, case["json"].as_str().expect("json"), "{input:?}"),
                    Err(_) => assert!(case["json_err"].as_bool().expect("flag"), "{input:?}"),
                }
            }
        }
    }
}
