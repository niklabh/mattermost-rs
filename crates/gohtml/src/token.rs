//! Port of `golang.org/x/net@v0.56.0/html`'s tokenizer (token.go) and entity decoding
//! (escape.go), plus the standard library's `html.UnescapeString`.
//!
//! # Two callers
//!
//! Mattermost's link previews (`github.com/dyatlov/go-opengraph`) read a page's `<meta>` tags off
//! this tokenizer token by token, with no tree builder behind it — so the tokenizer's own rules
//! decide what reaches a preview: the ten **raw-text** elements (`<script>`, `<style>`,
//! `<noscript>`, `<title>`, `<textarea>`, …) swallow any `<meta>` inside them, and a tag's
//! **duplicate attributes are dropped, the first one winning** (`attrNames`, readTag). That
//! oracle is `fixtures/behaviour_opengraph.json`, which records Go's whole token stream for every
//! corpus document. The other caller is this crate's tree builder ([`crate::parse()`]), which drives
//! [`Tokenizer::token`], [`Tokenizer::allow_cdata`] and [`Tokenizer::next_is_not_raw_text`] exactly
//! as parse.go drives Go's.
//!
//! # What is not ported
//!
//! `maxBuf` (`SetMaxBuf` is never called on either path), `NewTokenizerFragment` and `Raw`'s
//! byte-sharing contract. The reader plumbing collapses to a slice: Go's `readByte` refills from
//! an `io.Reader`, but both callers hand the whole document over, so running out of buffer and
//! reaching `io.EOF` are the same event.
//!
//! # Two different `unescape`s
//!
//! [`unescape`] is x/net's (escape.go:62), which the tokenizer applies to text and attribute
//! values. [`unescape_string`] is the standard library's `html.UnescapeString`
//! (html/escape.go:187), which `openGraphDecodeHTMLEntities` applies **again** to the title and
//! description. They are separate implementations and they disagree: `&#x;` is left alone by
//! the first and becomes U+FFFD in the second, a numeric reference past `0x10FFFF` is clamped by
//! the first and wraps `int32` in the second, and the second does not know `&nLt;` or `&nGt;`
//! ([`crate::entity::ENTITY2_NOT_IN_STD`]).

use std::collections::HashSet;

use crate::atom::{self, Atom};
use crate::entity::{ENTITY, ENTITY2, ENTITY2_NOT_IN_STD};

/// Port of `html.TokenType` (token.go:18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Error,
    Text,
    StartTag,
    EndTag,
    SelfClosingTag,
    Comment,
    Doctype,
}

impl TokenType {
    /// `TokenType.String()` (token.go:42).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenType::Error => "Error",
            TokenType::Text => "Text",
            TokenType::StartTag => "StartTag",
            TokenType::EndTag => "EndTag",
            TokenType::SelfClosingTag => "SelfClosingTag",
            TokenType::Comment => "Comment",
            TokenType::Doctype => "Doctype",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Span {
    start: usize,
    end: usize,
}

/// Port of `html.Tokenizer` (token.go:127) over an in-memory document.
#[derive(Debug)]
pub struct Tokenizer<'a> {
    buf: &'a [u8],
    tt: TokenType,
    /// `z.err != nil`. The only error a slice can produce is `io.EOF`.
    eof: bool,
    raw: Span,
    data: Span,
    pending_attr: [Span; 2],
    attr: Vec<[Span; 2]>,
    attr_names: HashSet<Vec<u8>>,
    n_attr_returned: usize,
    raw_tag: Vec<u8>,
    text_is_raw: bool,
    convert_nul: bool,
    allow_cdata: bool,
}

const WHITESPACE: [u8; 5] = [b' ', b'\n', b'\r', b'\t', 0x0c];

impl<'a> Tokenizer<'a> {
    /// Port of `html.NewTokenizer` (token.go:1287).
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            tt: TokenType::Error,
            eof: false,
            raw: Span::default(),
            data: Span::default(),
            pending_attr: [Span::default(); 2],
            attr: Vec::new(),
            attr_names: HashSet::new(),
            n_attr_returned: 0,
            raw_tag: Vec::new(),
            text_is_raw: false,
            convert_nul: false,
            allow_cdata: false,
        }
    }

    /// Port of `AllowCDATA` (token.go:189): whether `<![CDATA[x]]>` is the text `x` (foreign
    /// content) or a bogus comment (everything else, and the default).
    pub fn allow_cdata(&mut self, allow: bool) {
        self.allow_cdata = allow;
    }

    /// Port of `NextIsNotRawText` (token.go:217): the next token is not raw text even though the
    /// tag just read would normally make it so — the parser calls it for `<noscript>` without
    /// scripting and for any tag in foreign content.
    pub fn next_is_not_raw_text(&mut self) {
        self.raw_tag.clear();
    }

    /// `Err() != nil` (token.go:223): the only error a slice can produce is `io.EOF`, so this is
    /// whether the current token is the error token that ends the stream.
    pub fn at_eof(&self) -> bool {
        self.tt == TokenType::Error && self.eof
    }

    /// Port of `readByte` (token.go:205). At the end of the input it sets the error and answers
    /// `0` **without** advancing, which every caller relies on when it backs up with
    /// `z.raw.end--` only on the success path.
    fn read_byte(&mut self) -> u8 {
        if self.raw.end >= self.buf.len() {
            self.eof = true;
            return 0;
        }
        let x = self.buf[self.raw.end];
        self.raw.end += 1;
        x
    }

    /// Port of `skipWhiteSpace` (token.go:271).
    fn skip_white_space(&mut self) {
        if self.eof {
            return;
        }
        loop {
            let c = self.read_byte();
            if self.eof {
                return;
            }
            if !WHITESPACE.contains(&c) {
                self.raw.end -= 1;
                return;
            }
        }
    }

    /// Port of `readRawOrRCDATA` (token.go:291).
    fn read_raw_or_rcdata(&mut self) {
        if self.raw_tag == b"script" {
            self.read_script();
            self.text_is_raw = true;
            self.raw_tag.clear();
            return;
        }
        loop {
            let c = self.read_byte();
            if self.eof {
                break;
            }
            if c != b'<' {
                continue;
            }
            let c = self.read_byte();
            if self.eof {
                break;
            }
            if c != b'/' {
                self.raw.end -= 1;
                continue;
            }
            if self.read_raw_end_tag() || self.eof {
                break;
            }
        }
        self.data.end = self.raw.end;
        // A textarea's or title's RCDATA can contain escaped entities.
        self.text_is_raw = self.raw_tag != b"textarea" && self.raw_tag != b"title";
        self.raw_tag.clear();
    }

    /// Port of `readRawEndTag` (token.go:326).
    fn read_raw_end_tag(&mut self) -> bool {
        let tag = std::mem::take(&mut self.raw_tag);
        let matched = self.read_raw_end_tag_named(&tag);
        self.raw_tag = tag;
        matched
    }

    fn read_raw_end_tag_named(&mut self, tag: &[u8]) -> bool {
        for &t in tag {
            let c = self.read_byte();
            if self.eof {
                return false;
            }
            // `c != z.rawTag[i] && c != z.rawTag[i]-('a'-'A')` — the tag is lower case.
            if c != t && c != t.wrapping_sub(b'a' - b'A') {
                self.raw.end -= 1;
                return false;
            }
        }
        let c = self.read_byte();
        if self.eof {
            return false;
        }
        match c {
            b' ' | b'\n' | b'\r' | b'\t' | 0x0c | b'/' | b'>' => {
                // The 3 is 2 for the leading "</" plus 1 for the trailing character c.
                self.raw.end -= 3 + tag.len();
                true
            }
            _ => {
                self.raw.end -= 1;
                false
            }
        }
    }

    /// Port of `readScript` (token.go:352), Go's `goto` state machine as a loop over states.
    fn read_script(&mut self) {
        #[derive(Clone, Copy)]
        enum S {
            Data,
            LessThanSign,
            EndTagOpen,
            EscapeStart,
            EscapeStartDash,
            Escaped,
            EscapedDash,
            EscapedDashDash,
            EscapedLessThanSign,
            EscapedEndTagOpen,
            DoubleEscapeStart,
            DoubleEscaped,
            DoubleEscapedDash,
            DoubleEscapedDashDash,
            DoubleEscapedLessThanSign,
            DoubleEscapeEnd,
        }
        let mut state = S::Data;
        loop {
            state = match state {
                S::Data => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'<' { S::LessThanSign } else { S::Data }
                }
                S::LessThanSign => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'/' => S::EndTagOpen,
                        b'!' => S::EscapeStart,
                        _ => {
                            self.raw.end -= 1;
                            S::Data
                        }
                    }
                }
                S::EndTagOpen => {
                    if self.read_raw_end_tag() || self.eof {
                        break;
                    }
                    S::Data
                }
                S::EscapeStart => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'-' {
                        S::EscapeStartDash
                    } else {
                        self.raw.end -= 1;
                        S::Data
                    }
                }
                S::EscapeStartDash => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'-' {
                        S::EscapedDashDash
                    } else {
                        self.raw.end -= 1;
                        S::Data
                    }
                }
                S::Escaped => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::EscapedDash,
                        b'<' => S::EscapedLessThanSign,
                        _ => S::Escaped,
                    }
                }
                S::EscapedDash => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::EscapedDashDash,
                        b'<' => S::EscapedLessThanSign,
                        _ => S::Escaped,
                    }
                }
                S::EscapedDashDash => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::EscapedDashDash,
                        b'<' => S::EscapedLessThanSign,
                        b'>' => S::Data,
                        _ => S::Escaped,
                    }
                }
                S::EscapedLessThanSign => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'/' {
                        S::EscapedEndTagOpen
                    } else if c.is_ascii_alphabetic() {
                        S::DoubleEscapeStart
                    } else {
                        self.raw.end -= 1;
                        S::Data
                    }
                }
                S::EscapedEndTagOpen => {
                    if self.read_raw_end_tag() || self.eof {
                        break;
                    }
                    S::Escaped
                }
                S::DoubleEscapeStart => {
                    self.raw.end -= 1;
                    let mut next = None;
                    for (&lo, &up) in b"script".iter().zip(b"SCRIPT") {
                        let c = self.read_byte();
                        if self.eof {
                            break;
                        }
                        if c != lo && c != up {
                            self.raw.end -= 1;
                            next = Some(S::Escaped);
                            break;
                        }
                    }
                    if self.eof {
                        break;
                    }
                    match next {
                        Some(s) => s,
                        None => {
                            let c = self.read_byte();
                            if self.eof {
                                break;
                            }
                            match c {
                                b' ' | b'\n' | b'\r' | b'\t' | 0x0c | b'/' | b'>' => {
                                    S::DoubleEscaped
                                }
                                _ => {
                                    self.raw.end -= 1;
                                    S::Escaped
                                }
                            }
                        }
                    }
                }
                S::DoubleEscaped => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::DoubleEscapedDash,
                        b'<' => S::DoubleEscapedLessThanSign,
                        _ => S::DoubleEscaped,
                    }
                }
                S::DoubleEscapedDash => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::DoubleEscapedDashDash,
                        b'<' => S::DoubleEscapedLessThanSign,
                        _ => S::DoubleEscaped,
                    }
                }
                S::DoubleEscapedDashDash => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    match c {
                        b'-' => S::DoubleEscapedDashDash,
                        b'<' => S::DoubleEscapedLessThanSign,
                        b'>' => S::Data,
                        _ => S::DoubleEscaped,
                    }
                }
                S::DoubleEscapedLessThanSign => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'/' {
                        S::DoubleEscapeEnd
                    } else {
                        self.raw.end -= 1;
                        S::DoubleEscaped
                    }
                }
                S::DoubleEscapeEnd => {
                    if self.read_raw_end_tag() {
                        self.raw.end += "</script>".len();
                        S::Escaped
                    } else if self.eof {
                        break;
                    } else {
                        S::DoubleEscaped
                    }
                }
            };
        }
        // `defer func() { z.data.end = z.raw.end }()`
        self.data.end = self.raw.end;
    }

    /// Port of `readComment` (token.go:596).
    fn read_comment(&mut self) {
        self.data.start = self.raw.end;
        self.read_comment_body();
        // "It's a comment with no data, like <!-->."
        if self.data.end < self.data.start {
            self.data.end = self.data.start;
        }
    }

    fn read_comment_body(&mut self) {
        let mut dash_count = 0;
        let mut beginning = true;
        loop {
            let c = self.read_byte();
            if self.eof {
                self.data.end = self.calculate_abrupt_comment_data_end();
                return;
            }
            match c {
                b'-' => {
                    dash_count += 1;
                    continue;
                }
                b'>' => {
                    if dash_count >= 2 || beginning {
                        self.data.end = self.raw.end.saturating_sub("-->".len());
                        return;
                    }
                }
                b'!' if dash_count >= 2 => {
                    let c = self.read_byte();
                    if self.eof {
                        self.data.end = self.calculate_abrupt_comment_data_end();
                        return;
                    } else if c == b'>' {
                        self.data.end = self.raw.end.saturating_sub("--!>".len());
                        return;
                    } else if c == b'-' {
                        dash_count = 1;
                        beginning = false;
                        continue;
                    }
                }
                _ => {}
            }
            dash_count = 0;
            beginning = false;
        }
    }

    /// Port of `calculateAbruptCommentDataEnd` (token.go:647).
    fn calculate_abrupt_comment_data_end(&self) -> usize {
        let raw = &self.buf[self.raw.start..self.raw.end];
        if raw.len() >= 4 {
            let raw = &raw[4..];
            if raw.ends_with(b"--!") {
                return self.raw.end - 3;
            } else if raw.ends_with(b"--") {
                return self.raw.end - 2;
            } else if raw.ends_with(b"-") {
                return self.raw.end - 1;
            }
        }
        self.raw.end
    }

    /// Port of `readUntilCloseAngle` (token.go:676).
    fn read_until_close_angle(&mut self) {
        self.data.start = self.raw.end;
        loop {
            let c = self.read_byte();
            if self.eof {
                self.data.end = self.raw.end;
                return;
            }
            if c == b'>' {
                self.data.end = self.raw.end - 1;
                return;
            }
        }
    }

    /// Port of `readMarkupDeclaration` (token.go:701).
    fn read_markup_declaration(&mut self) -> TokenType {
        self.data.start = self.raw.end;
        let mut c = [0u8; 2];
        for i in 0..2 {
            c[i] = self.read_byte();
            if self.eof {
                // bogus comment
                self.data.end = self.raw.end;
                if i == 1 && c[0] == b'>' {
                    self.data.end -= 1;
                }
                return TokenType::Comment;
            }
        }
        if c == *b"--" {
            self.read_comment();
            return TokenType::Comment;
        }
        self.raw.end -= 2;
        if self.read_doctype() {
            return TokenType::Doctype;
        }
        if self.allow_cdata && self.read_cdata() {
            self.convert_nul = true;
            return TokenType::Text;
        }
        // It's a bogus comment.
        self.read_until_close_angle();
        TokenType::Comment
    }

    /// Port of `readDoctype` (token.go:726).
    fn read_doctype(&mut self) -> bool {
        for &s in b"DOCTYPE" {
            let c = self.read_byte();
            if self.eof {
                // "Back up to read the fragment of "DOCTYPE" again, reset z.err to signal EOF
                // on the next call."
                self.raw.end = self.data.start;
                self.eof = false;
                return false;
            }
            if c != s && c != s + (b'a' - b'A') {
                self.raw.end = self.data.start;
                return false;
            }
        }
        self.skip_white_space();
        if self.eof {
            self.data.start = self.raw.end;
            self.data.end = self.raw.end;
            return true;
        }
        self.read_until_close_angle();
        true
    }

    /// Port of `readCDATA` (token.go:766). The opening `<!` has been consumed.
    fn read_cdata(&mut self) -> bool {
        for &s in b"[CDATA[" {
            let c = self.read_byte();
            if self.eof {
                // "Back up to read the fragment of "[CDATA[" again, reset z.err to signal EOF
                // on the next call."
                self.raw.end = self.data.start;
                self.eof = false;
                return false;
            }
            if c != s {
                self.raw.end = self.data.start;
                return false;
            }
        }
        self.data.start = self.raw.end;
        let mut brackets = 0;
        loop {
            let c = self.read_byte();
            if self.eof {
                self.data.end = self.raw.end;
                return true;
            }
            match c {
                b']' => brackets += 1,
                b'>' if brackets >= 2 => {
                    self.data.end = self.raw.end - "]]>".len();
                    return true;
                }
                _ => brackets = 0,
            }
        }
    }

    /// Port of `startTagIn` (token.go:812).
    fn start_tag_in(&self, ss: &[&[u8]]) -> bool {
        let name = &self.buf[self.data.start..self.data.end];
        ss.iter().any(|s| name.eq_ignore_ascii_case(s))
    }

    /// Port of `readStartTag` (token.go:825).
    fn read_start_tag(&mut self) -> TokenType {
        self.read_tag(true);
        if self.eof {
            return TokenType::Error;
        }
        let c = self.buf[self.data.start].to_ascii_lowercase();
        let raw = match c {
            b'i' => self.start_tag_in(&[b"iframe"]),
            b'n' => self.start_tag_in(&[b"noembed", b"noframes", b"noscript"]),
            b'p' => self.start_tag_in(&[b"plaintext"]),
            b's' => self.start_tag_in(&[b"script", b"style"]),
            b't' => self.start_tag_in(&[b"textarea", b"title"]),
            b'x' => self.start_tag_in(&[b"xmp"]),
            _ => false,
        };
        if raw {
            self.raw_tag = self.buf[self.data.start..self.data.end].to_ascii_lowercase();
        }
        // "Look for a self-closing token (e.g. <br/>)" — but not `<p a=/>`, whose `/` is the
        // last character of an unquoted value.
        let n = self.attr.len();
        if self.buf[self.raw.end - 2] == b'/'
            && (n == 0 || self.raw.end - 2 != self.attr[n - 1][1].end.wrapping_sub(1))
        {
            return TokenType::SelfClosingTag;
        }
        TokenType::StartTag
    }

    /// Port of `readTag` (token.go:873). **A repeated attribute is dropped**, the first
    /// occurrence of a key (compared lower-cased) winning.
    fn read_tag(&mut self, save_attr: bool) {
        self.attr.clear();
        self.n_attr_returned = 0;
        self.attr_names.clear();
        self.read_tag_name();
        self.skip_white_space();
        if self.eof {
            return;
        }
        loop {
            let c = self.read_byte();
            if self.eof || c == b'>' {
                break;
            }
            self.raw.end -= 1;
            self.read_tag_attr_key();
            self.read_tag_attr_val();
            let key_span = self.pending_attr[0];
            let key = self.buf[key_span.start..key_span.end].to_ascii_lowercase();
            if save_attr && key_span.start != key_span.end && !self.attr_names.contains(&key) {
                self.attr.push(self.pending_attr);
                self.attr_names.insert(key);
            }
            self.skip_white_space();
            if self.eof {
                break;
            }
        }
    }

    /// Port of `readTagName` (token.go:903).
    fn read_tag_name(&mut self) {
        self.data.start = self.raw.end - 1;
        loop {
            let c = self.read_byte();
            if self.eof {
                self.data.end = self.raw.end;
                return;
            }
            match c {
                b' ' | b'\n' | b'\r' | b'\t' | 0x0c => {
                    self.data.end = self.raw.end - 1;
                    return;
                }
                b'/' | b'>' => {
                    self.raw.end -= 1;
                    self.data.end = self.raw.end;
                    return;
                }
                _ => {}
            }
        }
    }

    /// Port of `readTagAttrKey` (token.go:924).
    fn read_tag_attr_key(&mut self) {
        self.pending_attr[0].start = self.raw.end;
        loop {
            let c = self.read_byte();
            if self.eof {
                self.pending_attr[0].end = self.raw.end;
                return;
            }
            match c {
                // "If we see an equals sign before the attribute name begins, we treat it as a
                // character in the attribute name and continue."
                b'=' if self.pending_attr[0].start + 1 == self.raw.end => continue,
                b'=' | b' ' | b'\n' | b'\r' | b'\t' | 0x0c | b'/' | b'>' => {
                    self.raw.end -= 1;
                    self.pending_attr[0].end = self.raw.end;
                    return;
                }
                _ => {}
            }
        }
    }

    /// Port of `readTagAttrVal` (token.go:950).
    fn read_tag_attr_val(&mut self) {
        self.pending_attr[1].start = self.raw.end;
        self.pending_attr[1].end = self.raw.end;
        self.skip_white_space();
        if self.eof {
            return;
        }
        let c = self.read_byte();
        if self.eof {
            return;
        }
        if c == b'/' {
            // "Switch to the self-closing start tag state."
            return;
        }
        if c != b'=' {
            self.raw.end -= 1;
            return;
        }
        self.skip_white_space();
        if self.eof {
            return;
        }
        let quote = self.read_byte();
        if self.eof {
            return;
        }
        match quote {
            b'>' => {
                self.raw.end -= 1;
            }
            b'\'' | b'"' => {
                self.pending_attr[1].start = self.raw.end;
                loop {
                    let c = self.read_byte();
                    if self.eof {
                        self.pending_attr[1].end = self.raw.end;
                        return;
                    }
                    if c == quote {
                        self.pending_attr[1].end = self.raw.end - 1;
                        return;
                    }
                }
            }
            _ => {
                self.pending_attr[1].start = self.raw.end - 1;
                loop {
                    let c = self.read_byte();
                    if self.eof {
                        self.pending_attr[1].end = self.raw.end;
                        return;
                    }
                    match c {
                        b' ' | b'\n' | b'\r' | b'\t' | 0x0c => {
                            self.pending_attr[1].end = self.raw.end - 1;
                            return;
                        }
                        b'>' => {
                            self.raw.end -= 1;
                            self.pending_attr[1].end = self.raw.end;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Port of `Next` (token.go:1017).
    pub fn next_token(&mut self) -> TokenType {
        self.raw.start = self.raw.end;
        self.data.start = self.raw.end;
        self.data.end = self.raw.end;
        if self.eof {
            self.tt = TokenType::Error;
            return self.tt;
        }
        if !self.raw_tag.is_empty() {
            if self.raw_tag == b"plaintext" {
                // Read everything up to EOF.
                while !self.eof {
                    self.read_byte();
                }
                self.data.end = self.raw.end;
                self.text_is_raw = true;
            } else {
                self.read_raw_or_rcdata();
            }
            if self.data.end > self.data.start {
                self.tt = TokenType::Text;
                self.convert_nul = true;
                return self.tt;
            }
        }
        self.text_is_raw = false;
        self.convert_nul = false;

        loop {
            let c = self.read_byte();
            if self.eof {
                break;
            }
            if c != b'<' {
                continue;
            }
            // Check if the '<' we have just read is part of a tag, comment or doctype.
            let c = self.read_byte();
            if self.eof {
                break;
            }
            let token_type = if c.is_ascii_alphabetic() {
                TokenType::StartTag
            } else if c == b'/' {
                TokenType::EndTag
            } else if c == b'!' || c == b'?' {
                TokenType::Comment
            } else {
                // Reconsume the current character.
                self.raw.end -= 1;
                continue;
            };

            // Return any text accumulated before the tag first.
            let x = self.raw.end - 2;
            if self.raw.start < x {
                self.raw.end = x;
                self.data.end = x;
                self.tt = TokenType::Text;
                return self.tt;
            }
            match token_type {
                TokenType::StartTag => {
                    self.tt = self.read_start_tag();
                    return self.tt;
                }
                TokenType::EndTag => {
                    let c = self.read_byte();
                    if self.eof {
                        break;
                    }
                    if c == b'>' {
                        // "</>" does not generate a token at all; an empty comment stands in.
                        self.tt = TokenType::Comment;
                        return self.tt;
                    }
                    if c.is_ascii_alphabetic() {
                        self.read_tag(false);
                        self.tt = if self.eof {
                            TokenType::Error
                        } else {
                            TokenType::EndTag
                        };
                        return self.tt;
                    }
                    self.raw.end -= 1;
                    self.read_until_close_angle();
                    self.tt = TokenType::Comment;
                    return self.tt;
                }
                _ => {
                    if c == b'!' {
                        self.tt = self.read_markup_declaration();
                        return self.tt;
                    }
                    self.raw.end -= 1;
                    self.read_until_close_angle();
                    self.tt = TokenType::Comment;
                    return self.tt;
                }
            }
        }
        if self.raw.start < self.raw.end {
            self.data.end = self.raw.end;
            self.tt = TokenType::Text;
            return self.tt;
        }
        self.tt = TokenType::Error;
        self.tt
    }

    /// Port of `Text` (token.go:1175): newlines normalised, NUL replaced where Go replaces it,
    /// and entities decoded unless the text is raw.
    pub fn text(&mut self) -> Option<Vec<u8>> {
        match self.tt {
            TokenType::Text | TokenType::Comment | TokenType::Doctype => {
                let s = self.buf[self.data.start..self.data.end].to_vec();
                self.data.start = self.raw.end;
                self.data.end = self.raw.end;
                let mut s = convert_newlines(s);
                if (self.convert_nul || self.tt == TokenType::Comment) && s.contains(&0) {
                    s = replace_nul(&s);
                }
                if !self.text_is_raw {
                    s = unescape(&s, false);
                }
                Some(s)
            }
            _ => None,
        }
    }

    /// Port of `TagName` (token.go:1195): the lower-cased name, and whether there are
    /// attributes still to read. An end tag's attributes are never saved, so its answer is
    /// always `false`.
    pub fn tag_name(&mut self) -> (Option<Vec<u8>>, bool) {
        if self.data.start < self.data.end {
            if let TokenType::StartTag | TokenType::EndTag | TokenType::SelfClosingTag = self.tt {
                let s = replace_nul(&self.buf[self.data.start..self.data.end]);
                self.data.start = self.raw.end;
                self.data.end = self.raw.end;
                return (
                    Some(s.to_ascii_lowercase()),
                    self.n_attr_returned < self.attr.len(),
                );
            }
        }
        (None, false)
    }

    /// Port of `TagAttr` (token.go:1212): the lower-cased key, the value with its newlines
    /// normalised and its entities decoded in attribute mode, and whether more follow.
    pub fn tag_attr(&mut self) -> (Vec<u8>, Vec<u8>, bool) {
        if self.n_attr_returned < self.attr.len() {
            if let TokenType::StartTag | TokenType::SelfClosingTag = self.tt {
                let x = self.attr[self.n_attr_returned];
                self.n_attr_returned += 1;
                let key = replace_nul(&self.buf[x[0].start..x[0].end]).to_ascii_lowercase();
                let val = replace_nul(&self.buf[x[1].start..x[1].end]);
                let val = unescape(&convert_newlines(val), true);
                return (key, val, self.n_attr_returned < self.attr.len());
            }
        }
        (Vec::new(), Vec::new(), false)
    }
}

/// Port of `html.Attribute` (token.go:67). `namespace` is only ever set by the parser.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Attribute {
    pub namespace: String,
    pub key: String,
    pub val: String,
}

/// Port of `html.Token` (token.go:77). `data_atom` is `None` where Go's `DataAtom` is zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub token_type: TokenType,
    pub data_atom: Option<Atom>,
    pub data: String,
    pub attr: Vec<Attribute>,
}

impl Tokenizer<'_> {
    /// Port of `Token` (token.go:1255). Every string is built from spans that start and end on
    /// ASCII delimiters of the input, or from decoded entities, so a UTF-8 document yields UTF-8
    /// strings; `lossless_string` only guards a byte input that was not UTF-8 to begin with.
    pub fn token(&mut self) -> Token {
        let mut t = Token {
            token_type: self.tt,
            data_atom: None,
            data: String::new(),
            attr: Vec::new(),
        };
        match self.tt {
            TokenType::Text | TokenType::Comment | TokenType::Doctype => {
                t.data = lossless_string(self.text().unwrap_or_default());
            }
            TokenType::StartTag | TokenType::SelfClosingTag | TokenType::EndTag => {
                let (name, mut more) = self.tag_name();
                while more {
                    let (key, val, m) = self.tag_attr();
                    more = m;
                    t.attr.push(Attribute {
                        namespace: String::new(),
                        key: lossless_string(key),
                        val: lossless_string(val),
                    });
                }
                let name = name.unwrap_or_default();
                t.data_atom = atom::lookup(&name);
                t.data = lossless_string(name);
            }
            TokenType::Error => {}
        }
        t
    }
}

/// `string(b)`. Go keeps invalid UTF-8 as it is; a Rust `String` cannot, so a byte input that was
/// not UTF-8 has each maximal invalid sequence replaced by one U+FFFD — a **divergence** from Go
/// for such input. A `&str` input never reaches the fallback.
fn lossless_string(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `bytes.ReplaceAll(s, "\x00", "�")`.
fn replace_nul(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        if c == 0 {
            out.extend_from_slice("\u{fffd}".as_bytes());
        } else {
            out.push(c);
        }
    }
    out
}

/// Port of `convertNewlines` (token.go:1140): `\r` and `\r\n` become `\n`.
fn convert_newlines(s: Vec<u8>) -> Vec<u8> {
    if !s.contains(&b'\r') {
        return s;
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\r' {
            out.push(b'\n');
            if s.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
        } else {
            out.push(s[i]);
        }
        i += 1;
    }
    out
}

/// Windows-1252 replacements for numeric references `0x80..=0x9F` (escape.go:17).
const REPLACEMENT_TABLE: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// `longestEntityWithoutSemicolon` (entity.go:8).
const LONGEST_ENTITY_WITHOUT_SEMICOLON: usize = 6;

fn entity(name: &[u8]) -> Option<char> {
    ENTITY
        .binary_search_by(|(n, _)| n.as_bytes().cmp(name))
        .ok()
        .map(|i| ENTITY[i].1)
}

fn entity2(name: &[u8], std: bool) -> Option<(char, char)> {
    let i = ENTITY2
        .binary_search_by(|(n, _, _)| n.as_bytes().cmp(name))
        .ok()?;
    let (n, a, b) = ENTITY2[i];
    if std && ENTITY2_NOT_IN_STD.contains(&n) {
        return None;
    }
    Some((a, b))
}

/// The numeric-reference fix-ups both implementations share.
fn fix_numeric(x: i64) -> char {
    if (0x80..=0x9F).contains(&x) {
        REPLACEMENT_TABLE[(x - 0x80) as usize]
    } else if x == 0 || (0xD800..=0xDFFF).contains(&x) || x > 0x10FFFF {
        '\u{FFFD}'
    } else {
        // Negative values (the standard library's `int32` wrap) are not valid runes either;
        // `utf8.EncodeRune` writes U+FFFD for them.
        u32::try_from(x)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}')
    }
}

/// The name scan both implementations share: alphanumerics, then an optional `;`.
fn scan_entity_name(s: &[u8]) -> usize {
    let mut i = 1;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c.is_ascii_alphanumeric() {
            continue;
        }
        if c != b';' {
            i -= 1;
        }
        break;
    }
    i
}

/// Port of x/net's `unescapeEntity` (escape.go:62): `(r1, r2, consumed)`, where `('&', None, 1)`
/// means "not an entity".
fn unescape_entity(s: &[u8], attribute: bool) -> (char, Option<char>, usize) {
    let mut i = 1;
    if s.len() <= 1 {
        return ('&', None, 1);
    }
    if s[i] == b'#' {
        if s.len() <= 2 {
            return ('&', None, 1);
        }
        i += 1;
        let hex = matches!(s[i], b'x' | b'X');
        if hex {
            i += 1;
        }
        let i0 = i;
        let mut x: i64 = 0;
        while i < s.len() {
            let c = s[i];
            let (d, mult) = if hex {
                match c {
                    b'0'..=b'9' => (c - b'0', 16),
                    b'a'..=b'f' => (c - b'a' + 10, 16),
                    b'A'..=b'F' => (c - b'A' + 10, 16),
                    _ => break,
                }
            } else {
                match c {
                    b'0'..=b'9' => (c - b'0', 10),
                    _ => break,
                }
            };
            if x <= 0x10FFFF {
                x = mult * x + i64::from(d);
            }
            i += 1;
        }
        if i == i0 {
            return ('&', None, 1);
        }
        if i < s.len() && s[i] == b';' {
            i += 1;
        }
        return (fix_numeric(x), None, i);
    }

    let i = scan_entity_name(s);
    let name = &s[1..i];
    // Go's first two branches are no-ops: an empty name, and — in an attribute — a name
    // without `;` that is followed by `=` (`?a=1&amp=2` keeps its `&amp`).
    let left_alone = name.is_empty()
        || (attribute && name[name.len() - 1] != b';' && s.len() > i && s[i] == b'=');
    if left_alone {
        // No-op.
    } else if let Some(x) = entity(name) {
        return (x, None, i);
    } else if let Some((a, b)) = entity2(name, false) {
        return (a, Some(b), i);
    } else if !attribute {
        let max_len = (name.len() - 1).min(LONGEST_ENTITY_WITHOUT_SEMICOLON);
        for j in (2..=max_len).rev() {
            if let Some(x) = entity(&name[..j]) {
                return (x, None, j + 1);
            }
        }
    }
    ('&', None, 1)
}

/// Port of x/net's `unescape` (escape.go:188) — what the tokenizer applies to text (`attribute`
/// false) and to attribute values (`attribute` true). In attribute mode a name without `;`
/// followed by `=` is left alone (`?a=1&amp=2` keeps its `&amp`), and no prefix match is tried.
pub fn unescape(b: &[u8], attribute: bool) -> Vec<u8> {
    if !b.contains(&b'&') {
        return b.to_vec();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut src = 0;
    let mut tmp = [0u8; 4];
    while src < b.len() {
        if b[src] != b'&' {
            out.push(b[src]);
            src += 1;
            continue;
        }
        let (r1, r2, n) = unescape_entity(&b[src..], attribute);
        if n == 1 && r1 == '&' {
            out.push(b'&');
            src += 1;
            continue;
        }
        out.extend_from_slice(r1.encode_utf8(&mut tmp).as_bytes());
        if let Some(r2) = r2 {
            out.extend_from_slice(r2.encode_utf8(&mut tmp).as_bytes());
        }
        src += n;
    }
    out
}

/// Port of the standard library's `unescapeEntity` (html/escape.go:56): `(bytes written, bytes
/// consumed)` appended to `out`.
fn std_unescape_entity(s: &[u8], out: &mut Vec<u8>) -> usize {
    let mut tmp = [0u8; 4];
    let mut i = 1;
    if s.len() <= 1 {
        out.push(s[0]);
        return 1;
    }
    if s[i] == b'#' {
        // "We need to have at least "&#."."
        if s.len() <= 3 {
            out.push(s[0]);
            return 1;
        }
        i += 1;
        let hex = matches!(s[i], b'x' | b'X');
        if hex {
            i += 1;
        }
        // `x := '\x00'` is a Go `rune` — an `int32` that wraps on overflow.
        let mut x: i32 = 0;
        while i < s.len() {
            let c = s[i];
            i += 1;
            if hex {
                let d = match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                };
                if let Some(d) = d {
                    x = x.wrapping_mul(16).wrapping_add(i32::from(d));
                    continue;
                }
            } else if c.is_ascii_digit() {
                x = x.wrapping_mul(10).wrapping_add(i32::from(c - b'0'));
                continue;
            }
            if c != b';' {
                i -= 1;
            }
            break;
        }
        if i <= 3 {
            // No characters matched.
            out.push(s[0]);
            return 1;
        }
        out.extend_from_slice(fix_numeric(i64::from(x)).encode_utf8(&mut tmp).as_bytes());
        return i;
    }

    let i = scan_entity_name(s);
    let name = &s[1..i];
    if name.is_empty() {
        // No-op.
    } else if let Some(x) = entity(name) {
        out.extend_from_slice(x.encode_utf8(&mut tmp).as_bytes());
        return i;
    } else if let Some((a, b)) = entity2(name, true) {
        out.extend_from_slice(a.encode_utf8(&mut tmp).as_bytes());
        out.extend_from_slice(b.encode_utf8(&mut tmp).as_bytes());
        return i;
    } else {
        let max_len = (name.len() - 1).min(LONGEST_ENTITY_WITHOUT_SEMICOLON);
        for j in (2..=max_len).rev() {
            if let Some(x) = entity(&name[..j]) {
                out.extend_from_slice(x.encode_utf8(&mut tmp).as_bytes());
                return j + 1;
            }
        }
    }
    out.extend_from_slice(&s[..i]);
    i
}

/// Port of the standard library's `html.UnescapeString` (html/escape.go:187). See the module
/// docs for how it differs from [`unescape`].
pub fn unescape_string(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&b'&') {
        return s.to_owned();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut src = 0;
    while src < b.len() {
        if b[src] == b'&' {
            src += std_unescape_entity(&b[src..], &mut out);
        } else {
            out.push(b[src]);
            src += 1;
        }
    }
    // Every replacement is a whole UTF-8 sequence and every copied byte came from a `&str`, so
    // this cannot split a code point; the fallback exists only because library code does not
    // panic.
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Tok = (TokenType, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

    fn tokens(doc: &[u8]) -> Vec<Tok> {
        let mut z = Tokenizer::new(doc);
        let mut out = Vec::new();
        loop {
            let tt = z.next_token();
            if tt == TokenType::Error {
                return out;
            }
            match tt {
                TokenType::Text | TokenType::Comment | TokenType::Doctype => {
                    out.push((tt, z.text().unwrap(), Vec::new()));
                }
                _ => {
                    let (name, mut more) = z.tag_name();
                    let mut attrs = Vec::new();
                    while more {
                        let (k, v, m) = z.tag_attr();
                        attrs.push((k, v));
                        more = m;
                    }
                    out.push((tt, name.unwrap_or_default(), attrs));
                }
            }
        }
    }

    #[test]
    fn a_meta_inside_a_raw_text_element_is_text() {
        let t = tokens(b"<noscript><meta property=a></noscript><meta property=b>");
        let metas: Vec<_> = t.iter().filter(|(_, n, _)| n == b"meta").collect();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].2[0].1, b"b");
    }

    #[test]
    fn duplicate_attributes_keep_the_first() {
        let t = tokens(b"<META Property=a PROPERTY=b content='x'>");
        assert_eq!(t[0].1, b"meta");
        assert_eq!(
            t[0].2,
            vec![
                (b"property".to_vec(), b"a".to_vec()),
                (b"content".to_vec(), b"x".to_vec())
            ]
        );
    }

    #[test]
    fn unescape_differs_between_the_two_implementations() {
        assert_eq!(unescape(b"&#x;", false), b"&#x;");
        assert_eq!(unescape_string("&#x;"), "\u{FFFD}");
        assert_eq!(unescape(b"&nLt;", false), "\u{226A}\u{20D2}".as_bytes());
        assert_eq!(unescape_string("&nLt;"), "&nLt;");
        assert_eq!(unescape(b"a&amp=b", true), b"a&amp=b");
        assert_eq!(unescape(b"a&amp=b", false), b"a&=b");
        assert_eq!(unescape_string("&notit;"), "\u{AC}it;");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_opengraph.json"))
            .expect("the fixture is JSON")
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        use base64::Engine;
        match v.as_str() {
            Some(s) => base64::engine::general_purpose::STANDARD
                .decode(s)
                .expect("base64"),
            None => Vec::new(),
        }
    }

    type GoTok = (String, Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

    /// Every document of the corpus — hand-written and random — tokenized by Go and here:
    /// the same token types, the same text, tag names and attributes, in the same order.
    #[test]
    fn the_token_stream_matches_go() {
        let o = oracle();
        let cases = o["tokens"].as_array().expect("tokens");
        assert!(cases.len() > 400);
        for case in cases {
            let doc = bytes(&case["doc"]);
            let mut z = Tokenizer::new(&doc);
            let mut ours = Vec::new();
            loop {
                let tt = z.next_token();
                if tt == TokenType::Error {
                    break;
                }
                let (data, attrs) = match tt {
                    TokenType::Text | TokenType::Comment | TokenType::Doctype => {
                        (z.text().unwrap_or_default(), Vec::new())
                    }
                    _ => {
                        let (name, mut more) = z.tag_name();
                        let mut attrs = Vec::new();
                        while more {
                            let (k, v, m) = z.tag_attr();
                            attrs.push((k, v));
                            more = m;
                        }
                        (name.unwrap_or_default(), attrs)
                    }
                };
                ours.push((tt.as_str().to_owned(), data, attrs));
            }
            let theirs: Vec<GoTok> = case["tokens"]
                .as_array()
                .expect("a token list")
                .iter()
                .map(|t| {
                    let attrs = t["a"]
                        .as_array()
                        .expect("attrs")
                        .iter()
                        .map(|kv| (bytes(&kv[0]), bytes(&kv[1])))
                        .collect();
                    (
                        t["t"].as_str().expect("type").to_owned(),
                        bytes(&t["d"]),
                        attrs,
                    )
                })
                .collect();
            assert_eq!(ours, theirs, "document {:?}", String::from_utf8_lossy(&doc));
        }
    }

    /// x/net's `UnescapeString` — [`unescape`] in text mode.
    #[test]
    fn x_net_unescape_matches_go() {
        for case in oracle()["unescape"].as_array().expect("cases") {
            let input = bytes(&case["in"]);
            assert_eq!(
                unescape(&input, false),
                bytes(&case["out"]),
                "{:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    /// The standard library's `html.UnescapeString`.
    #[test]
    fn std_unescape_string_matches_go() {
        for case in oracle()["std_unescape"].as_array().expect("cases") {
            let input = String::from_utf8(bytes(&case["in"])).expect("the corpus is UTF-8");
            assert_eq!(
                unescape_string(&input).into_bytes(),
                bytes(&case["out"]),
                "{input:?}"
            );
        }
    }
}
