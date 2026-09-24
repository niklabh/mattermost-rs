//! Port of `github.com/wneessen/go-mail` v0.8.1's `Msg` and `msgWriter` — the subset
//! Mattermost's `sendMail` drives: generic, preformatted and address headers; a body with one
//! alternative (multipart/alternative); embedded files (multipart/related, base64 through
//! `base64LineBreaker`); attachments (multipart/mixed) for completeness of the structure; the
//! default headers (`Date`, `Message-ID`, `MIME-Version`, `User-Agent`/`X-Mailer`); and
//! `WriteTo`.
//!
//! Not ported: S/MIME, PGP, DKIM, middlewares, templates, file/FS/reader-seeker sources, and the
//! delivery half of go-mail (Mattermost uses `net/smtp` for that).
//!
//! # Where the port has to choose
//!
//! - **Preformatted headers** (`In-Reply-To`, `References`) are written in Go **map order**,
//!   which is random per run. This port writes them in sorted key order — one of the orders Go
//!   produces, and indistinguishable to a reader; the oracle regenerates until Go matches.
//! - **Randomness and the clock** come from a [`WriteEnv`], so tests fix the multipart
//!   boundaries (drawn once per multipart level, in Go's order: mixed, related, alternative)
//!   and the defaults; [`SystemEnv`] is the production one.
//! - **`mime.TypeByExtension`** is the env's too: the host-aware port lives in `mm_app::mime`.
//! - **Errors**: rendering is in memory and cannot fail except through a boundary set with
//!   [`Msg::set_boundary`] that `multipart.SetBoundary` refuses. Go then keeps writing through a
//!   failed writer and wraps the error in one or more "failed to write due to previous error: "
//!   prefixes depending on the part structure; this port returns the `SetBoundary` error itself.
//!   Mattermost never sets a boundary.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};

use crate::mime::WordEncoder;
use crate::mime::multipart::{self, BoundaryError};
use crate::mime::quotedprintable;
use crate::netmail::{self, Address};

/// `VERSION` (doc.go:14).
pub const VERSION: &str = "0.8.1";

/// `MaxHeaderLength` (msgwriter.go:23).
pub const MAX_HEADER_LENGTH: i64 = 76;
/// `MaxBodyLength` (msgwriter.go:26): the base64 line length.
pub const MAX_BODY_LENGTH: usize = 76;

/// `CharsetUTF8` (encoding.go:27), the default charset.
pub const CHARSET_UTF8: &str = "UTF-8";

/// Port of `Encoding` (encoding.go:12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `EncodingB64`, "base64".
    Base64,
    /// `EncodingQP`, "quoted-printable" — the default.
    QuotedPrintable,
    /// `EncodingUSASCII`, "7bit" — written through the quoted-printable encoder, as Go's
    /// `writeBody` default branch does.
    UsAscii,
    /// `NoEncoding`, "8bit".
    NoEncoding,
}

impl Encoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Base64 => "base64",
            Encoding::QuotedPrintable => "quoted-printable",
            Encoding::UsAscii => "7bit",
            Encoding::NoEncoding => "8bit",
        }
    }

    /// `getEncoder` (msg.go:3084): B for base64, Q for everything else.
    fn word_encoder(self) -> WordEncoder {
        match self {
            Encoding::Base64 => WordEncoder::B,
            _ => WordEncoder::Q,
        }
    }
}

/// `TypeTextPlain` (encoding.go:107).
pub const TYPE_TEXT_PLAIN: &str = "text/plain";
/// `TypeTextHTML` (encoding.go:105).
pub const TYPE_TEXT_HTML: &str = "text/html";

/// The multipart kinds `writeMsg` nests, outermost first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MimeType {
    Mixed,
    Related,
    Alternative,
}

impl MimeType {
    fn as_str(self) -> &'static str {
        match self {
            MimeType::Mixed => "mixed",
            MimeType::Related => "related",
            MimeType::Alternative => "alternative",
        }
    }
}

/// Port of `AddrHeader` (header.go:99).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddrHeader {
    Bcc,
    Cc,
    EnvelopeFrom,
    From,
    ReplyTo,
    To,
}

impl AddrHeader {
    pub fn as_str(self) -> &'static str {
        match self {
            AddrHeader::Bcc => "Bcc",
            AddrHeader::Cc => "Cc",
            AddrHeader::EnvelopeFrom => "EnvelopeFrom",
            AddrHeader::From => "From",
            AddrHeader::ReplyTo => "Reply-To",
            AddrHeader::To => "To",
        }
    }
}

/// `errParseMailAddr` (msg.go:56): `failed to parse mail address %q: %w`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("failed to parse mail address {}: {source}", crate::strconv::quote(.address))]
pub struct AddrError {
    pub address: String,
    #[source]
    pub source: netmail::ParseError,
}

/// Why `WriteTo` failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Boundary(#[from] BoundaryError),
}

/// What rendering needs from the outside world. See the module docs.
pub trait WriteEnv {
    /// `multipart.randomBoundary` — called once per multipart level, even when a fixed boundary
    /// then replaces it, as Go's `multipart.NewWriter` always draws one.
    fn random_boundary(&mut self) -> String;
    /// `mime.TypeByExtension`.
    fn type_by_extension(&self, ext: &str) -> String;
    /// `time.Now()`, for a message with no `Date`.
    fn now(&self) -> DateTime<FixedOffset>;
    /// Fill `buf` from `crypto/rand`, for a message with no `Message-ID`.
    fn random_bytes(&mut self, buf: &mut [u8]);
    /// `os.Hostname()`, `None` when it fails.
    fn hostname(&self) -> Option<String>;
}

/// The production [`WriteEnv`]: `crypto/rand`, the local clock, `/proc/sys/kernel/hostname`,
/// and a caller-supplied `TypeByExtension`.
pub struct SystemEnv {
    pub type_by_extension: fn(&str) -> String,
}

impl WriteEnv for SystemEnv {
    fn random_boundary(&mut self) -> String {
        multipart::random_boundary()
    }

    fn type_by_extension(&self, ext: &str) -> String {
        (self.type_by_extension)(ext)
    }

    fn now(&self) -> DateTime<FixedOffset> {
        chrono::Local::now().fixed_offset()
    }

    fn random_bytes(&mut self, buf: &mut [u8]) {
        use rand::RngCore as _;
        rand::rng().fill_bytes(buf);
    }

    /// Go's `os.Hostname` on Linux reads `/proc/sys/kernel/hostname` first.
    fn hostname(&self) -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .map(|h| h.trim_end_matches('\n').to_owned())
            .filter(|h| !h.is_empty())
    }
}

/// Port of `Part` (part.go:20), with its content held rather than a write function.
#[derive(Debug, Clone)]
pub struct Part {
    content_type: String,
    charset: String,
    description: String,
    encoding: Encoding,
    content: Vec<u8>,
}

/// Port of `File` (file.go:21) as `EmbedReader` / `AttachReader` build it: a name, the bytes,
/// and a header map filled in on first write. Keys are canonical MIME keys (`Content-Id`, not
/// `Content-ID`), because go-mail sets them through `textproto.MIMEHeader.Set`.
#[derive(Debug, Clone)]
pub struct File {
    pub name: String,
    pub content: Vec<u8>,
    header: BTreeMap<String, Vec<String>>,
}

impl File {
    pub fn new(name: impl Into<String>, content: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            content,
            header: BTreeMap::new(),
        }
    }

    /// `getHeader`: present and non-empty.
    fn has_header(&self, key: &str) -> bool {
        self.header
            .get(key)
            .and_then(|v| v.first())
            .is_some_and(|v| !v.is_empty())
    }

    fn set_header(&mut self, key: &str, value: String) {
        self.header.insert(key.to_owned(), vec![value]);
    }
}

/// Port of `Msg` (msg.go:100).
#[derive(Debug, Clone)]
pub struct Msg {
    addr_header: BTreeMap<AddrHeader, Vec<Address>>,
    attachments: Vec<File>,
    boundary: String,
    charset: String,
    embeds: Vec<File>,
    encoding: Encoding,
    gen_header: BTreeMap<String, Vec<String>>,
    mime_version: String,
    multipart_boundary: BTreeMap<MimeType, String>,
    parts: Vec<Part>,
    preform_header: BTreeMap<String, String>,
    no_default_user_agent: bool,
}

impl Default for Msg {
    fn default() -> Self {
        Self::new()
    }
}

impl Msg {
    /// `NewMsg()` (msg.go:207) with no options: UTF-8, quoted-printable, MIME 1.0.
    pub fn new() -> Self {
        Self {
            addr_header: BTreeMap::new(),
            attachments: Vec::new(),
            boundary: String::new(),
            charset: CHARSET_UTF8.to_owned(),
            embeds: Vec::new(),
            encoding: Encoding::QuotedPrintable,
            gen_header: BTreeMap::new(),
            mime_version: "1.0".to_owned(),
            multipart_boundary: BTreeMap::new(),
            parts: Vec::new(),
            preform_header: BTreeMap::new(),
            no_default_user_agent: false,
        }
    }

    /// `SetBoundary` / `WithBoundary`: one boundary for **every** multipart level.
    pub fn set_boundary(&mut self, boundary: impl Into<String>) {
        self.boundary = boundary.into();
    }

    /// `WithNoDefaultUserAgent`.
    pub fn set_no_default_user_agent(&mut self) {
        self.no_default_user_agent = true;
    }

    /// `SetEncoding` (msg.go:416): the body encoding and, with it, the header word encoder.
    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
    }

    /// `encodeString` (msg.go:2747).
    fn encode_string(&self, s: &str) -> String {
        self.encoding.word_encoder().encode(&self.charset, s)
    }

    /// `SetGenHeader` (msg.go:557): every value RFC 2047-encoded when it needs it.
    pub fn set_gen_header(&mut self, header: &str, values: &[&str]) {
        let values = values.iter().map(|v| self.encode_string(v)).collect();
        self.gen_header.insert(header.to_owned(), values);
    }

    /// `SetGenHeaderPreformatted` (msg.go:599): written verbatim, never folded or encoded.
    pub fn set_gen_header_preformatted(&mut self, header: &str, value: &str) {
        self.preform_header
            .insert(header.to_owned(), value.to_owned());
    }

    /// `GetGenHeader`.
    pub fn gen_header(&self, header: &str) -> Option<&[String]> {
        self.gen_header.get(header).map(Vec::as_slice)
    }

    /// `SetAddrHeader` (msg.go:626): each value through `mail.ParseAddress`; From keeps only the
    /// first.
    pub fn set_addr_header(
        &mut self,
        header: AddrHeader,
        values: &[&str],
    ) -> Result<(), AddrError> {
        let mut addresses = Vec::with_capacity(values.len());
        for value in values {
            let address = netmail::parse_address(value).map_err(|source| AddrError {
                address: (*value).to_owned(),
                source,
            })?;
            addresses.push(address);
        }
        match header {
            AddrHeader::From => {
                if let Some(first) = addresses.into_iter().next() {
                    self.addr_header.insert(header, vec![first]);
                }
            }
            _ => {
                self.addr_header.insert(header, addresses);
            }
        }
        Ok(())
    }

    /// `SetAddrHeaderFromMailAddress` (msg.go:665): no parsing; From and EnvelopeFrom keep the
    /// first, Reply-To is only set when non-empty.
    pub fn set_addr_header_from_mail_address(&mut self, header: AddrHeader, values: &[Address]) {
        match header {
            AddrHeader::EnvelopeFrom | AddrHeader::From => {
                if let Some(first) = values.first() {
                    self.addr_header.insert(header, vec![first.clone()]);
                }
            }
            AddrHeader::ReplyTo => {
                if !values.is_empty() {
                    self.addr_header.insert(header, values.to_vec());
                }
            }
            _ => {
                self.addr_header.insert(header, values.to_vec());
            }
        }
    }

    /// `SetMessageIDWithValue` (msg.go:1323): wraps the value in angle brackets.
    pub fn set_message_id_with_value(&mut self, message_id: &str) {
        self.set_gen_header("Message-ID", &[&format!("<{message_id}>")]);
    }

    /// `SetMessageID` (msg.go:1283): `<{randomStringSecure(22)}@{hostname}>`, the hostname
    /// falling back to `localhost.localdomain`.
    pub fn set_message_id(&mut self, env: &mut dyn WriteEnv) {
        let hostname = env
            .hostname()
            .unwrap_or_else(|| "localhost.localdomain".to_owned());
        let random = random_string_secure(22, env);
        self.set_message_id_with_value(&format!("{random}@{hostname}"));
    }

    /// `SetDateWithValue` (msg.go:1368): `time.RFC1123Z` in the value's own offset.
    pub fn set_date_with_value(&mut self, date: DateTime<FixedOffset>) {
        let formatted = date.format("%a, %d %b %Y %H:%M:%S %z").to_string();
        self.set_gen_header("Date", &[&formatted]);
    }

    /// `SetUserAgent` (msg.go:1421): both `User-Agent` and `X-Mailer`.
    pub fn set_user_agent(&mut self, user_agent: &str) {
        self.set_gen_header("User-Agent", &[user_agent]);
        self.set_gen_header("X-Mailer", &[user_agent]);
    }

    fn new_part(&self, content_type: &str, content: &str) -> Part {
        Part {
            content_type: content_type.to_owned(),
            charset: self.charset.clone(),
            description: String::new(),
            encoding: self.encoding,
            content: content.as_bytes().to_vec(),
        }
    }

    /// `SetBodyString` (msg.go:1979): replaces every part.
    pub fn set_body_string(&mut self, content_type: &str, content: &str) {
        self.parts = vec![self.new_part(content_type, content)];
    }

    /// `AddAlternativeString` (msg.go:2023).
    pub fn add_alternative_string(&mut self, content_type: &str, content: &str) {
        let part = self.new_part(content_type, content);
        self.parts.push(part);
    }

    /// `EmbedReader` (msg.go:2199). Go's only failure is the reader's; a byte slice has none.
    pub fn embed_reader(&mut self, name: &str, content: Vec<u8>) {
        self.embeds.push(File::new(name, content));
    }

    /// `AttachReader` (msg.go:2089).
    pub fn attach_reader(&mut self, name: &str, content: Vec<u8>) {
        self.attachments.push(File::new(name, content));
    }

    /// `hasAlt` (msg.go:2763).
    fn has_alt(&self) -> bool {
        self.parts.len() > 1
    }

    /// `hasMixed` (msg.go:2784).
    fn has_mixed(&self) -> bool {
        (!self.parts.is_empty() && !self.attachments.is_empty()) || self.attachments.len() > 1
    }

    /// `hasRelated` (msg.go:2819).
    fn has_related(&self) -> bool {
        (!self.parts.is_empty() && !self.embeds.is_empty()) || self.embeds.len() > 1
    }

    /// `checkUserAgent` (msg.go:2891).
    fn check_user_agent(&mut self) {
        if self.no_default_user_agent {
            return;
        }
        if !self.gen_header.contains_key("User-Agent") && !self.gen_header.contains_key("X-Mailer")
        {
            self.set_user_agent(&format!(
                "go-mail v{VERSION} // https://github.com/wneessen/go-mail"
            ));
        }
    }

    /// `addDefaultHeader` (msg.go:2913).
    fn add_default_header(&mut self, env: &mut dyn WriteEnv) {
        if !self.gen_header.contains_key("Date") {
            self.set_date_with_value(env.now());
        }
        if !self.gen_header.contains_key("Message-ID") {
            self.set_message_id(env);
        }
        let version = self.mime_version.clone();
        self.set_gen_header("MIME-Version", &[&version]);
    }

    /// `WriteTo` (msg.go:2321): the whole message, as `writeMsg` (msgwriter.go:88) lays it out.
    pub fn write_to(&mut self, env: &mut dyn WriteEnv) -> Result<Vec<u8>, WriteError> {
        self.add_default_header(env);
        self.check_user_agent();
        let mut mw = MsgWriter {
            out: Vec::new(),
            depth: 0,
            writers: Vec::new(),
            charset: self.charset.clone(),
            encoder: self.encoding.word_encoder(),
        };

        for (key, values) in &self.gen_header {
            mw.write_header(key, values);
        }
        for (key, value) in &self.preform_header {
            mw.write_string(&format!("{key}: {value}\r\n"));
        }

        let from = self
            .addr_header
            .get(&AddrHeader::From)
            .filter(|f| !f.is_empty())
            .or_else(|| {
                self.addr_header
                    .get(&AddrHeader::EnvelopeFrom)
                    .filter(|f| !f.is_empty())
            });
        if let Some(from) = from {
            mw.write_header("From", &[from[0].to_string()]);
        }
        for header in [AddrHeader::To, AddrHeader::Cc, AddrHeader::ReplyTo] {
            if let Some(addresses) = self.addr_header.get(&header) {
                let values: Vec<String> = addresses.iter().map(Address::to_string).collect();
                mw.write_header(header.as_str(), &values);
            }
        }

        for (kind, wanted) in [
            (MimeType::Mixed, self.has_mixed()),
            (MimeType::Related, self.has_related()),
            (MimeType::Alternative, self.has_alt()),
        ] {
            if !wanted {
                continue;
            }
            let boundary = if !self.boundary.is_empty() {
                self.boundary.clone()
            } else {
                self.multipart_boundary
                    .get(&kind)
                    .cloned()
                    .unwrap_or_default()
            };
            let has_content_type = self.gen_header.contains_key("Content-Type");
            let boundary = mw.start_mp(env, kind, &boundary, has_content_type)?;
            self.multipart_boundary.insert(kind, boundary);
            if mw.depth == 1 {
                mw.write_string("\r\n\r\n");
            }
        }

        for part in &self.parts {
            mw.write_part(part);
        }
        if self.has_alt() {
            mw.stop_mp();
        }
        mw.add_files(env, &mut self.embeds, false);
        if self.has_related() {
            mw.stop_mp();
        }
        mw.add_files(env, &mut self.attachments, true);
        if self.has_mixed() {
            mw.stop_mp();
        }
        Ok(mw.out)
    }
}

/// `randomStringSecure` (random.go:26): 6 bits per character from 8-byte pools, rejecting
/// indexes past the 64-character set (none are — the set has exactly 64).
fn random_string_secure(length: usize, env: &mut dyn WriteEnv) -> String {
    const CR: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._";
    const LETTER_IDX_BITS: u32 = 6;
    const LETTER_IDX_MASK: u64 = (1 << LETTER_IDX_BITS) - 1;
    const LETTER_IDX_MAX: u32 = 63 / LETTER_IDX_BITS;
    let mut out = String::with_capacity(length);
    let mut pool = [0u8; 8];
    env.random_bytes(&mut pool);
    let mut chr = u64::from_be_bytes(pool);
    let mut rest = LETTER_IDX_MAX;
    let mut remaining = length;
    while remaining > 0 {
        if rest == 0 {
            env.random_bytes(&mut pool);
            chr = u64::from_be_bytes(pool);
            rest = LETTER_IDX_MAX;
        }
        let i = (chr & LETTER_IDX_MASK) as usize;
        if i < CR.len() {
            out.push(char::from(CR[i]));
            remaining -= 1;
        }
        chr >>= LETTER_IDX_BITS;
        rest -= 1;
    }
    out
}

/// Port of `msgWriter` (msgwriter.go:57), writing into memory.
struct MsgWriter {
    out: Vec<u8>,
    depth: usize,
    writers: Vec<multipart::Writer>,
    charset: String,
    encoder: WordEncoder,
}

impl MsgWriter {
    fn write_string(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    /// `writeHeader` (msgwriter.go:400): join the values with ", ", split on single spaces, and
    /// fold before any word that would leave one character or fewer of the 76-byte budget.
    /// Byte lengths throughout, and the budget may go negative — Go's arithmetic is on `int`.
    fn write_header(&mut self, key: &str, values: &[String]) {
        let mut buffer = String::new();
        let mut char_length = MAX_HEADER_LENGTH - 2;
        buffer.push_str(key);
        char_length -= key.len() as i64;
        if values.is_empty() {
            // Go appends ":\r\n" to its local buffer and returns without writing it — a header
            // with no values is dropped.
            return;
        }
        buffer.push_str(": ");
        char_length -= 2;
        let full_value = values.join(", ");
        let words: Vec<&str> = full_value.split(' ').collect();
        for (i, val) in words.iter().enumerate() {
            let len = val.len() as i64;
            if char_length - len <= 1 {
                buffer.push_str("\r\n ");
                char_length = MAX_HEADER_LENGTH - 3;
            }
            buffer.push_str(val);
            if i < words.len() - 1 {
                buffer.push(' ');
                char_length -= 1;
            }
            char_length -= len;
        }
        let buffer = buffer.replace(" \r\n", "\r\n");
        self.write_string(&buffer);
        self.write_string("\r\n");
    }

    /// `startMP` (msgwriter.go:236).
    fn start_mp(
        &mut self,
        env: &mut dyn WriteEnv,
        kind: MimeType,
        boundary: &str,
        has_content_type: bool,
    ) -> Result<String, WriteError> {
        let mut writer = multipart::Writer::new(env.random_boundary());
        if !boundary.is_empty() {
            writer.set_boundary(boundary)?;
        }
        let content_type = format!(
            "multipart/{};\r\n boundary={}",
            kind.as_str(),
            writer.boundary()
        );
        let drawn = writer.boundary().to_owned();
        self.writers.push(writer);
        if !has_content_type {
            if self.depth == 0 {
                self.write_string(&format!("Content-Type: {content_type}"));
            } else {
                self.new_part(&[("Content-Type".to_owned(), vec![content_type])]);
            }
        }
        self.depth += 1;
        Ok(drawn)
    }

    /// `stopMP` (msgwriter.go:268).
    fn stop_mp(&mut self) {
        if self.depth > 0 {
            if let Some(mut writer) = self.writers.pop() {
                writer.close(&mut self.out);
            }
            self.depth -= 1;
        }
    }

    /// `newPart` (msgwriter.go:350): a part of the writer at `depth - 1`. Inside `startMP` the
    /// new level's writer is already stored at `depth` but `depth` is not yet raised, so the
    /// nested multipart's header lands in its **parent**.
    fn new_part(&mut self, header: &[(String, Vec<String>)]) {
        if let Some(writer) = self
            .depth
            .checked_sub(1)
            .and_then(|i| self.writers.get_mut(i))
        {
            writer.create_part(&mut self.out, header);
        }
    }

    /// `writePart` (msgwriter.go:362).
    fn write_part(&mut self, part: &Part) {
        let charset = if part.charset.is_empty() {
            self.charset.clone()
        } else {
            part.charset.clone()
        };
        let content_type = format!("{}; charset={charset}", part.content_type);
        let cte = part.encoding.as_str().to_owned();
        if self.depth == 0 {
            self.write_header("Content-Transfer-Encoding", std::slice::from_ref(&cte));
            self.write_header("Content-Type", std::slice::from_ref(&content_type));
            self.write_string("\r\n");
        } else {
            let mut header = Vec::with_capacity(3);
            if !part.description.is_empty() {
                header.push((
                    "Content-Description".to_owned(),
                    vec![part.description.clone()],
                ));
            }
            header.push(("Content-Transfer-Encoding".to_owned(), vec![cte]));
            header.push(("Content-Type".to_owned(), vec![content_type]));
            self.new_part(&header);
        }
        self.write_body(&part.content, part.encoding);
    }

    /// `addFiles` (msgwriter.go:283).
    fn add_files(&mut self, env: &mut dyn WriteEnv, files: &mut [File], is_attachment: bool) {
        for file in files.iter_mut() {
            let encoding = Encoding::Base64;
            let sanitized = sanitize_filename(&file.name);
            if !file.has_header("Content-Type") {
                let mut mime_type = env.type_by_extension(filepath_ext(&file.name));
                if mime_type.is_empty() {
                    mime_type = "application/octet-stream".to_owned();
                }
                let value = format!(
                    "{mime_type}; name=\"{}\"",
                    self.encoder.encode(&self.charset, &sanitized)
                );
                file.set_header("Content-Type", value);
            }
            // Go only reads `File.Enc` (which this port does not carry) when the header is
            // absent; a preset header leaves the body base64 whatever it says.
            if !file.has_header("Content-Transfer-Encoding") {
                file.set_header("Content-Transfer-Encoding", encoding.as_str().to_owned());
            }
            if !file.has_header("Content-Disposition") {
                let disposition = if is_attachment {
                    "attachment"
                } else {
                    "inline"
                };
                let value = format!(
                    "{disposition}; filename=\"{}\"",
                    self.encoder.encode(&self.charset, &sanitized)
                );
                file.set_header("Content-Disposition", value);
            }
            if !is_attachment && !file.has_header("Content-Id") {
                file.set_header("Content-Id", format!("<{sanitized}>"));
            }
            let header: Vec<(String, Vec<String>)> = file
                .header
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if self.depth == 0 {
                // Go ranges over the header map here — random order; sorted is one of them.
                for (k, v) in &header {
                    self.write_header(k, v);
                }
                self.write_string("\r\n");
            } else {
                self.new_part(&header);
            }
            self.write_body(&file.content, encoding);
        }
    }

    /// `writeBody` (msgwriter.go:448).
    fn write_body(&mut self, content: &[u8], encoding: Encoding) {
        match encoding {
            Encoding::NoEncoding => self.out.extend_from_slice(content),
            Encoding::Base64 => {
                let encoded = crate::base64::encode(content);
                for line in encoded.as_bytes().chunks(MAX_BODY_LENGTH) {
                    self.out.extend_from_slice(line);
                    self.out.extend_from_slice(b"\r\n");
                }
            }
            Encoding::QuotedPrintable | Encoding::UsAscii => {
                let mut qp = quotedprintable::Writer::new();
                qp.write(content);
                self.out.extend_from_slice(&qp.close());
            }
        }
    }
}

/// `filepath.Ext`: from the last '.' after the last '/', or "".
fn filepath_ext(path: &str) -> &str {
    for (i, b) in path.bytes().enumerate().rev() {
        if b == b'/' {
            break;
        }
        if b == b'.' {
            return &path[i..];
        }
    }
    ""
}

/// `sanitizeFilename` (msgwriter.go:593): control bytes, DEL and `" / : < > ? \ |` become `_`.
pub fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\u{0}'..='\u{1f}' | '"' | '/' | ':' | '<' | '>' | '?' | '\\' | '|' | '\u{7f}' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle's environment: the boundaries Go drew, in order, and the builtin `.png`.
    pub(crate) struct FixedEnv {
        pub boundaries: std::collections::VecDeque<String>,
    }

    impl WriteEnv for FixedEnv {
        fn random_boundary(&mut self) -> String {
            self.boundaries.pop_front().expect("Go drew as many")
        }
        fn type_by_extension(&self, ext: &str) -> String {
            match ext.to_ascii_lowercase().as_str() {
                ".png" => "image/png",
                ".jpg" => "image/jpeg",
                ".gif" => "image/gif",
                _ => "",
            }
            .to_owned()
        }
        fn now(&self) -> DateTime<FixedOffset> {
            unreachable!("every corpus message sets Date")
        }
        fn random_bytes(&mut self, _: &mut [u8]) {
            unreachable!("every corpus message sets Message-ID")
        }
        fn hostname(&self) -> Option<String> {
            None
        }
    }

    /// The oracle's go-mail corpus, built in the order of Mattermost's `sendMail` exactly as
    /// `reference/dump/behaviour_mail.go:buildLikeSendMail` does.
    #[test]
    fn messages_match_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_mail.json")).unwrap();
        let rows = oracle["messages"].as_array().unwrap();
        assert!(rows.len() >= 15);
        for row in rows {
            let input = &row["input"];
            let s = |k: &str| input[k].as_str().unwrap().to_owned();
            let name = s("name");
            let mut m = Msg::new();
            let from = Address {
                name: s("feedback_name"),
                address: s("feedback_email"),
            };
            let reply_to = Address {
                name: s("feedback_name"),
                address: s("reply_to_address"),
            };
            m.set_addr_header_from_mail_address(AddrHeader::From, std::slice::from_ref(&from));
            m.set_addr_header(AddrHeader::To, &[&s("to")]).unwrap();
            m.set_gen_header("Subject", &[&s("subject")]);
            m.set_gen_header("Content-Transfer-Encoding", &["8bit"]);
            m.set_gen_header("Auto-Submitted", &["auto-generated"]);
            m.set_gen_header("Precedence", &["bulk"]);
            if !s("category").is_empty() {
                let category = format!(
                    "{{\"category\": {}}}",
                    crate::strconv::quote(&s("category"))
                );
                m.set_gen_header("X-SMTPAPI", &[&category]);
            }
            if !reply_to.address.is_empty() {
                m.set_addr_header_from_mail_address(AddrHeader::ReplyTo, &[reply_to]);
            }
            if !s("cc").is_empty() {
                m.set_addr_header(AddrHeader::Cc, &[&s("cc")]).unwrap();
            }
            m.set_gen_header("Message-ID", &[&s("message_id")]);
            if !s("in_reply_to").is_empty() {
                m.set_gen_header_preformatted("In-Reply-To", &s("in_reply_to"));
            }
            if !s("references").is_empty() {
                m.set_gen_header_preformatted("References", &s("references"));
            }
            let offset = FixedOffset::east_opt(
                i32::try_from(input["date_offset_seconds"].as_i64().unwrap()).unwrap(),
            )
            .unwrap();
            let date = DateTime::from_timestamp(input["date_unix"].as_i64().unwrap(), 0)
                .unwrap()
                .with_timezone(&offset);
            m.set_date_with_value(date);
            m.set_body_string(TYPE_TEXT_PLAIN, row["text"].as_str().unwrap());
            m.add_alternative_string(TYPE_TEXT_HTML, &s("html_body"));
            for e in input["embedded"].as_array().into_iter().flatten() {
                let content = crate::base64::decode(e["content_base64"].as_str().unwrap()).unwrap();
                m.embed_reader(e["name"].as_str().unwrap(), content);
            }
            let mut env = FixedEnv {
                boundaries: row["boundaries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_str().unwrap().to_owned())
                    .collect(),
            };
            let out = m.write_to(&mut env).unwrap();
            assert!(env.boundaries.is_empty(), "{name}: Go drew more boundaries");
            let want = row["output"].as_str().unwrap();
            let got = String::from_utf8(out).unwrap();
            if got != want {
                for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
                    assert_eq!(g, w, "{name}: first difference at line {i}");
                }
                assert_eq!(got, want, "{name}");
            }
        }
    }

    /// `writeHeader`'s budget, swept: three-word subjects whose lengths straddle every fold
    /// point, cut down to the Subject header as Go wrote it.
    #[test]
    fn header_folding_sweep_matches_go() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_mail.json")).unwrap();
        let rows = oracle["header_fold"].as_array().unwrap();
        assert!(rows.len() > 1000);
        for row in rows {
            let subject = row["subject"].as_str().unwrap();
            let mut mw = MsgWriter {
                out: Vec::new(),
                depth: 0,
                writers: Vec::new(),
                charset: CHARSET_UTF8.into(),
                encoder: WordEncoder::Q,
            };
            mw.write_header("Subject", &[subject.to_owned()]);
            assert_eq!(
                String::from_utf8(mw.out).unwrap(),
                row["header"].as_str().unwrap(),
                "{subject}"
            );
        }
    }

    #[test]
    fn filepath_ext_and_sanitize() {
        assert_eq!(filepath_ext("a/b.c/d"), "");
        assert_eq!(filepath_ext("a.b.PNG"), ".PNG");
        assert_eq!(filepath_ext("noext"), "");
        assert_eq!(filepath_ext(".hidden"), ".hidden");
        assert_eq!(
            sanitize_filename("a\u{1}\"/:<>?\\|\u{7f}é b"),
            "a__________é b"
        );
    }

    #[test]
    fn write_header_folds_like_go() {
        let mut mw = MsgWriter {
            out: Vec::new(),
            depth: 0,
            writers: Vec::new(),
            charset: "UTF-8".into(),
            encoder: WordEncoder::Q,
        };
        mw.write_header("Empty", &[]);
        assert_eq!(mw.out, b"");
        mw.write_header("X", &["a".into(), "b".into()]);
        assert_eq!(mw.out, b"X: a, b\r\n");
    }

    #[test]
    fn random_string_secure_takes_six_bits_per_char() {
        struct Counting(u8);
        impl WriteEnv for Counting {
            fn random_boundary(&mut self) -> String {
                String::new()
            }
            fn type_by_extension(&self, _: &str) -> String {
                String::new()
            }
            fn now(&self) -> DateTime<FixedOffset> {
                unreachable!()
            }
            fn random_bytes(&mut self, buf: &mut [u8]) {
                buf.fill(self.0);
                self.0 += 1;
            }
            fn hostname(&self) -> Option<String> {
                Some("h".into())
            }
        }
        // 0x0000_0000_0000_0000 → index 0 ('A') ten times per pool, then the next pool.
        let mut env = Counting(0);
        let s = random_string_secure(12, &mut env);
        assert_eq!(&s[..10], "AAAAAAAAAA");
        // pool 0x0101…01: low six bits 000001 → 'B', then 0x01>>6 pattern.
        assert_eq!(s.len(), 12);
        let mut msg = Msg::new();
        msg.set_message_id(&mut Counting(0));
        assert!(msg.gen_header("Message-ID").unwrap()[0].ends_with("@h>"));
    }
}
