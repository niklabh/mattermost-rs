//! Port of the address half of Go's `net/mail` (message.go, Go 1.26.4): `ParseAddress`,
//! `ParseAddressList` and `Address.String`.
//!
//! Mattermost reaches these three ways: `validateSingleAddress` runs `ParseAddressList` over the
//! To and Cc values, go-mail's `SetAddrHeader` runs `ParseAddress` over them again, and go-mail
//! writes every address header with `Address.String`. The parse errors reach the caller verbatim
//! (wrapped), so every message below is Go's.
//!
//! Two Go 1.26.4 details a reader of an older tree would get wrong: consecutive RFC 2047 words in
//! a phrase are joined **without** a space only while they stay encoded (`consumePhrase`'s
//! string builder), and `textproto`-style `%q` quoting is used for the "expected single address"
//! remainder.
//!
//! Go's "invalid utf-8" errors are unreachable: a Rust `&str` is valid UTF-8 by construction.

use crate::mime::WordEncoder;
use crate::mime::word::{self, DecodeError};
use crate::strconv;

/// Port of `mail.Address` (message.go:228).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Address {
    /// Proper name; may be empty.
    pub name: String,
    /// `user@domain`.
    pub address: String,
}

/// Every error `ParseAddress` / `ParseAddressList` can return, with Go's text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("mail: misformatted parenthetical comment")]
    MisformattedComment,
    #[error("mail: expected comma")]
    ExpectedComma,
    #[error("mail: expected single address, got {}", strconv::quote(.0))]
    ExpectedSingleAddress(String),
    #[error("mail: empty group")]
    EmptyGroup,
    #[error("mail: group with multiple addresses")]
    GroupWithMultipleAddresses,
    #[error("mail: no address")]
    NoAddress,
    #[error("mail: missing '@' or angle-addr")]
    MissingAtOrAngleAddr,
    #[error("mail: no angle-addr")]
    NoAngleAddr,
    #[error("mail: unclosed angle-addr")]
    UnclosedAngleAddr,
    #[error("mail: no addr-spec")]
    NoAddrSpec,
    #[error("mail: empty quoted string in addr-spec")]
    EmptyQuotedString,
    #[error("mail: missing @ in addr-spec")]
    MissingAt,
    #[error("mail: no domain in addr-spec")]
    NoDomain,
    /// `fmt.Errorf("mail: missing word in phrase: %v", err)`.
    #[error("mail: missing word in phrase: {0}")]
    MissingWordInPhrase(Box<ParseError>),
    #[error("mail: unclosed quoted-string")]
    UnclosedQuotedString,
    #[error("mail: bad character in quoted-string: {}", strconv::quote_rune(*.0))]
    BadCharacterInQuotedString(char),
    #[error("mail: invalid string")]
    InvalidString,
    #[error("mail: leading dot in atom")]
    LeadingDot,
    #[error("mail: double dot in atom")]
    DoubleDot,
    #[error("mail: trailing dot in atom")]
    TrailingDot,
    #[error("mail: missing \"[\" in domain-literal")]
    MissingOpenBracket,
    #[error("mail: unclosed domain-literal")]
    UnclosedDomainLiteral,
    #[error("mail: bad character in domain-literal: {}", strconv::quote_rune(*.0))]
    BadCharacterInDomainLiteral(char),
    #[error("mail: invalid IP address in domain-literal: {}", strconv::quote(.0))]
    InvalidIpInDomainLiteral(String),
    #[error("mail: comment does not start with (")]
    CommentDoesNotStart,
    /// The `CharsetReader` refusal of an encoded word, `charset not supported: %q`.
    #[error(transparent)]
    Charset(DecodeError),
}

/// Port of `mail.ParseAddress` (message.go:237).
pub fn parse_address(address: &str) -> Result<Address, ParseError> {
    Parser { s: address }.parse_single_address()
}

/// Port of `mail.ParseAddressList` (message.go:242).
pub fn parse_address_list(list: &str) -> Result<Vec<Address>, ParseError> {
    Parser { s: list }.parse_address_list()
}

impl std::fmt::Display for Address {
    /// Port of `Address.String` (message.go:267): the angle-addr, the local part quoted when it
    /// is not a dot-atom, and the name quoted when it is printable ASCII or else RFC 2047
    /// encoded — B when it holds a special that Q would leave bare, Q otherwise.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (local, domain) = match self.address.rfind('@') {
            Some(at) => (&self.address[..at], &self.address[at + 1..]),
            None => (self.address.as_str(), ""),
        };
        let bytes = local.as_bytes();
        let mut quote_local = false;
        for (i, r) in local.char_indices() {
            if is_atext(r, false) {
                continue;
            }
            if r == '.' && i > 0 && bytes[i - 1] != b'.' && i < local.len() - 1 {
                continue;
            }
            quote_local = true;
            break;
        }
        let local = if quote_local {
            quote_string(local)
        } else {
            local.to_owned()
        };
        let s = format!("<{local}@{domain}>");
        if self.name.is_empty() {
            return f.write_str(&s);
        }
        let all_printable = self
            .name
            .chars()
            .all(|r| (is_vchar(r) || is_wsp(r)) && !is_multibyte(r));
        if all_printable {
            return write!(f, "{} {s}", quote_string(&self.name));
        }
        let encoder = if self
            .name
            .contains(|c| "\"#$%&'(),.:;<>@[]^`{|}~".contains(c))
        {
            WordEncoder::B
        } else {
            WordEncoder::Q
        };
        write!(f, "{} {s}", encoder.encode("utf-8", &self.name))
    }
}

/// `addrParser` (message.go:329): the unconsumed remainder of the input.
#[derive(Clone, Copy)]
struct Parser<'a> {
    s: &'a str,
}

impl<'a> Parser<'a> {
    /// `parseAddressList` (message.go:335).
    fn parse_address_list(&mut self) -> Result<Vec<Address>, ParseError> {
        let mut list = Vec::new();
        loop {
            self.skip_space();
            // allow skipping empty entries (RFC 5322 obs-addr-list)
            if self.consume(b',') {
                continue;
            }
            let addrs = self.parse_address(true)?;
            list.extend(addrs);
            if !self.skip_cfws() {
                return Err(ParseError::MisformattedComment);
            }
            if self.empty() {
                break;
            }
            if self.peek() != Some(b',') {
                return Err(ParseError::ExpectedComma);
            }
            // Skip empty entries for obs-addr-list.
            while self.consume(b',') {
                self.skip_space();
            }
            if self.empty() {
                break;
            }
        }
        Ok(list)
    }

    /// `parseSingleAddress` (message.go:372).
    fn parse_single_address(&mut self) -> Result<Address, ParseError> {
        let mut addrs = self.parse_address(true)?;
        if !self.skip_cfws() {
            return Err(ParseError::MisformattedComment);
        }
        if !self.empty() {
            return Err(ParseError::ExpectedSingleAddress(self.s.to_owned()));
        }
        match addrs.len() {
            0 => Err(ParseError::EmptyGroup),
            1 => Ok(addrs.remove(0)),
            _ => Err(ParseError::GroupWithMultipleAddresses),
        }
    }

    /// `parseAddress` (message.go:393).
    fn parse_address(&mut self, handle_group: bool) -> Result<Vec<Address>, ParseError> {
        self.skip_space();
        if self.empty() {
            return Err(ParseError::NoAddress);
        }
        // address = mailbox / group; mailbox = name-addr / addr-spec.
        // Try an addr-spec first; a failed attempt consumes nothing.
        if let Ok(spec) = self.consume_addr_spec() {
            let mut display_name = String::new();
            self.skip_space();
            if self.peek() == Some(b'(') {
                display_name = self.consume_display_name_comment()?;
            }
            return Ok(vec![Address {
                name: display_name,
                address: spec,
            }]);
        }

        let mut display_name = String::new();
        if self.peek() != Some(b'<') {
            display_name = self.consume_phrase()?;
        }
        self.skip_space();
        if handle_group && self.consume(b':') {
            return self.consume_group_list();
        }
        if !self.consume(b'<') {
            if display_name.chars().all(|r| is_atext(r, true)) {
                // Looks like the user used a bare local part — "name" with no domain.
                return Err(ParseError::MissingAtOrAngleAddr);
            }
            return Err(ParseError::NoAngleAddr);
        }
        let spec = self.consume_addr_spec()?;
        if !self.consume(b'>') {
            return Err(ParseError::UnclosedAngleAddr);
        }
        Ok(vec![Address {
            name: display_name,
            address: spec,
        }])
    }

    /// `consumeGroupList` (message.go:476).
    fn consume_group_list(&mut self) -> Result<Vec<Address>, ParseError> {
        let mut group = Vec::new();
        self.skip_space();
        if self.consume(b';') {
            if !self.skip_cfws() {
                return Err(ParseError::MisformattedComment);
            }
            return Ok(group);
        }
        loop {
            self.skip_space();
            // embedded groups not allowed.
            let addrs = self.parse_address(false)?;
            group.extend(addrs);
            if !self.skip_cfws() {
                return Err(ParseError::MisformattedComment);
            }
            if self.consume(b';') {
                if !self.skip_cfws() {
                    return Err(ParseError::MisformattedComment);
                }
                break;
            }
            if !self.consume(b',') {
                return Err(ParseError::ExpectedComma);
            }
        }
        Ok(group)
    }

    /// `consumeAddrSpec` (message.go:513). On any error the parser is restored.
    fn consume_addr_spec(&mut self) -> Result<String, ParseError> {
        let orig = *self;
        let result = self.consume_addr_spec_inner();
        if result.is_err() {
            *self = orig;
        }
        result
    }

    fn consume_addr_spec_inner(&mut self) -> Result<String, ParseError> {
        self.skip_space();
        if self.empty() {
            return Err(ParseError::NoAddrSpec);
        }
        let local_part = if self.peek() == Some(b'"') {
            // Go assigns the error *and* checks `localPart == ""`, which a failed parse also
            // leaves empty — so every quoted-string failure here reads "empty quoted string".
            match self.consume_quoted_string() {
                Ok(s) if !s.is_empty() => s,
                _ => return Err(ParseError::EmptyQuotedString),
            }
        } else {
            self.consume_atom(true, false)?
        };
        if !self.consume(b'@') {
            return Err(ParseError::MissingAt);
        }
        self.skip_space();
        if self.empty() {
            return Err(ParseError::NoDomain);
        }
        let domain = if self.peek() == Some(b'[') {
            self.consume_domain_literal()?
        } else {
            self.consume_atom(true, false)?
        };
        Ok(format!("{local_part}@{domain}"))
    }

    /// `consumePhrase` (message.go:575, Go 1.26.4).
    fn consume_phrase(&mut self) -> Result<String, ParseError> {
        let mut words: Vec<String> = Vec::new();
        let mut encoded_run = String::new();
        let mut err = None;
        loop {
            // obs-phrase allows CFWS after one word
            if !words.is_empty() && !self.skip_cfws() {
                return Err(ParseError::MisformattedComment);
            }
            self.skip_space();
            if self.empty() {
                break;
            }
            let word = if self.peek() == Some(b'"') {
                self.consume_quoted_string().map(|w| (w, false))
            } else {
                // atom — dot-atom, to be more permissive than RFC 5322.
                self.consume_atom(true, true)
                    .and_then(|atom| decode_rfc2047_word(&atom))
            };
            let (word, is_encoded) = match word {
                Ok(w) => w,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            };
            if is_encoded {
                encoded_run.push_str(&word);
            } else if !encoded_run.is_empty() {
                words.push(std::mem::take(&mut encoded_run));
                words.push(word);
            } else {
                words.push(word);
            }
        }
        if !encoded_run.is_empty() {
            words.push(encoded_run);
        }
        if let Some(e) = err {
            if words.is_empty() {
                return Err(ParseError::MissingWordInPhrase(Box::new(e)));
            }
        }
        Ok(words.join(" "))
    }

    /// `consumeQuotedString` (message.go:628).
    fn consume_quoted_string(&mut self) -> Result<String, ParseError> {
        // Assume first byte is '"'.
        let mut qsb = String::new();
        let mut escaped = false;
        let mut chars = self.s[1..].char_indices();
        let end = loop {
            let Some((i, r)) = chars.next() else {
                return Err(ParseError::UnclosedQuotedString);
            };
            if escaped {
                // quoted-pair = ("\" (VCHAR / WSP))
                if !is_vchar(r) && !is_wsp(r) {
                    return Err(ParseError::BadCharacterInQuotedString(r));
                }
                qsb.push(r);
                escaped = false;
            } else if is_qtext(r) || is_wsp(r) {
                qsb.push(r);
            } else if r == '"' {
                break 1 + i;
            } else if r == '\\' {
                escaped = true;
            } else {
                return Err(ParseError::BadCharacterInQuotedString(r));
            }
        };
        self.s = &self.s[end + 1..];
        Ok(qsb)
    }

    /// `consumeAtom` (message.go:682). `dot` admits '.', `permissive` skips the dot-atom checks.
    fn consume_atom(&mut self, dot: bool, permissive: bool) -> Result<String, ParseError> {
        let end = self
            .s
            .char_indices()
            .find(|&(_, r)| !is_atext(r, dot))
            .map_or(self.s.len(), |(i, _)| i);
        if end == 0 {
            return Err(ParseError::InvalidString);
        }
        let atom = &self.s[..end];
        self.s = &self.s[end..];
        if !permissive {
            if atom.starts_with('.') {
                return Err(ParseError::LeadingDot);
            }
            if atom.contains("..") {
                return Err(ParseError::DoubleDot);
            }
            if atom.ends_with('.') {
                return Err(ParseError::TrailingDot);
            }
        }
        Ok(atom.to_owned())
    }

    /// `consumeDomainLiteral` (message.go:720).
    fn consume_domain_literal(&mut self) -> Result<String, ParseError> {
        if !self.consume(b'[') {
            return Err(ParseError::MissingOpenBracket);
        }
        let start = self.s;
        let mut len = 0;
        loop {
            let Some(r) = self.s.chars().next() else {
                return Err(ParseError::UnclosedDomainLiteral);
            };
            if r == ']' {
                break;
            }
            if !is_dtext(r) {
                return Err(ParseError::BadCharacterInDomainLiteral(r));
            }
            len += r.len_utf8();
            self.s = &self.s[r.len_utf8()..];
        }
        let dtext = &start[..len];
        if !self.consume(b']') {
            return Err(ParseError::UnclosedDomainLiteral);
        }
        if !go_parse_ip(dtext) {
            return Err(ParseError::InvalidIpInDomainLiteral(dtext.to_owned()));
        }
        Ok(format!("[{dtext}]"))
    }

    /// `consumeDisplayNameComment` (message.go:763).
    fn consume_display_name_comment(&mut self) -> Result<String, ParseError> {
        if !self.consume(b'(') {
            return Err(ParseError::CommentDoesNotStart);
        }
        let (comment, ok) = self.consume_comment();
        if !ok {
            return Err(ParseError::MisformattedComment);
        }
        // TODO(stapelberg): parse quoted-string within comment
        let mut words = Vec::new();
        for word in comment.split([' ', '\t']).filter(|w| !w.is_empty()) {
            let (decoded, is_encoded) = decode_rfc2047_word(word)?;
            words.push(if is_encoded { decoded } else { word.to_owned() });
        }
        Ok(words.join(" "))
    }

    fn consume(&mut self, c: u8) -> bool {
        if self.peek() != Some(c) {
            return false;
        }
        self.s = &self.s[1..];
        true
    }

    /// `skipSpace` — `strings.TrimLeft(p.s, " \t")`.
    fn skip_space(&mut self) {
        self.s = self.s.trim_start_matches([' ', '\t']);
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().first().copied()
    }

    fn empty(&self) -> bool {
        self.s.is_empty()
    }

    /// `skipCFWS` (message.go:813): spaces and comments; false on an unterminated comment.
    fn skip_cfws(&mut self) -> bool {
        self.skip_space();
        loop {
            if !self.consume(b'(') {
                break;
            }
            if !self.consume_comment().1 {
                return false;
            }
            self.skip_space();
        }
        true
    }

    /// `consumeComment` (message.go:842), '(' already consumed. Works byte-wise as Go does;
    /// it only ever stops just after an ASCII ')' or at the end, so the remainder stays on a
    /// character boundary.
    fn consume_comment(&mut self) -> (String, bool) {
        let bytes = self.s.as_bytes();
        let mut depth = 1;
        let mut comment = Vec::new();
        let mut i = 0;
        while i < bytes.len() && depth != 0 {
            if bytes[i] == b'\\' && bytes.len() - i > 1 {
                i += 1;
            } else if bytes[i] == b'(' {
                depth += 1;
            } else if bytes[i] == b')' {
                depth -= 1;
            }
            if depth > 0 {
                comment.push(bytes[i]);
            }
            i += 1;
        }
        self.s = &self.s[i..];
        (String::from_utf8_lossy(&comment).into_owned(), depth == 0)
    }
}

/// `decodeRFC2047Word` (message.go:868): `(decoded, true)` for a word that decodes, the word
/// unchanged and `false` for one that is not an encoded word at all, and an error only when the
/// charset is unsupported.
fn decode_rfc2047_word(s: &str) -> Result<(String, bool), ParseError> {
    match word::decode_word(s) {
        Ok(decoded) => Ok((decoded, true)),
        Err(e @ DecodeError::CharsetNotSupported(_)) => Err(ParseError::Charset(e)),
        Err(_) => Ok((s.to_owned(), false)),
    }
}

/// `isAtext` (message.go:925).
fn is_atext(r: char, dot: bool) -> bool {
    match r {
        '.' => dot,
        '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | '\\' | ',' | '"' => false,
        _ => is_vchar(r),
    }
}

/// `isQtext` (message.go:938).
fn is_qtext(r: char) -> bool {
    r != '\\' && r != '"' && is_vchar(r)
}

/// `quoteString` (message.go:947): printable characters kept (`\` and `"` escaped), anything
/// else — control characters — silently dropped.
fn quote_string(s: &str) -> String {
    let mut b = String::with_capacity(s.len() + 2);
    b.push('"');
    for r in s.chars() {
        if is_qtext(r) || is_wsp(r) {
            b.push(r);
        } else if is_vchar(r) {
            b.push('\\');
            b.push(r);
        }
    }
    b.push('"');
    b
}

fn is_vchar(r: char) -> bool {
    ('!'..='~').contains(&r) || is_multibyte(r)
}

fn is_multibyte(r: char) -> bool {
    u32::from(r) >= 0x80
}

fn is_wsp(r: char) -> bool {
    r == ' ' || r == '\t'
}

fn is_dtext(r: char) -> bool {
    r != '[' && r != ']' && r != '\\' && is_vchar(r)
}

/// `net.ParseIP(s) != nil`: a dotted-quad IPv4 or an IPv6 address without a zone. Rust's parser
/// agrees with `netip.ParseAddr` on leading zeros (both refuse) and on embedded IPv4.
fn go_parse_ip(s: &str) -> bool {
    s.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_mail.json")).unwrap()
    }

    fn check_addr(got: &Address, want: &serde_json::Value, ctx: &str) {
        assert_eq!(got.name, want["name"].as_str().unwrap(), "{ctx} name");
        assert_eq!(
            got.address,
            want["address"].as_str().unwrap(),
            "{ctx} address"
        );
        assert_eq!(
            got.to_string(),
            want["string"].as_str().unwrap(),
            "{ctx} string"
        );
    }

    #[test]
    fn parse_matches_go() {
        let oracle = oracle();
        let rows = oracle["address_parse"].as_array().unwrap();
        let mut checked = 0;
        for row in rows {
            let input = row["input"].as_str().unwrap();
            if input.contains('\u{fffd}') {
                continue;
            }
            match parse_address(input) {
                Ok(a) => {
                    assert!(
                        row["single_error"].is_null(),
                        "{input:?}: Go failed: {}",
                        row["single_error"]
                    );
                    check_addr(&a, &row["single"], input);
                }
                Err(e) => assert_eq!(
                    Some(e.to_string().as_str()),
                    row["single_error"].as_str(),
                    "{input:?}"
                ),
            }
            match parse_address_list(input) {
                Ok(list) => {
                    assert!(
                        row["list_error"].is_null(),
                        "{input:?}: Go failed: {}",
                        row["list_error"]
                    );
                    let want = row["list"].as_array().unwrap();
                    assert_eq!(list.len(), want.len(), "{input:?}");
                    for (a, w) in list.iter().zip(want) {
                        check_addr(a, w, input);
                    }
                }
                Err(e) => assert_eq!(
                    Some(e.to_string().as_str()),
                    row["list_error"].as_str(),
                    "list {input:?}"
                ),
            }
            checked += 1;
        }
        assert!(checked > 80, "{checked}");
    }

    #[test]
    fn string_matches_go() {
        let oracle = oracle();
        for row in oracle["address_string"].as_array().unwrap() {
            let a = Address {
                name: row["name"].as_str().unwrap().to_owned(),
                address: row["address"].as_str().unwrap().to_owned(),
            };
            assert_eq!(a.to_string(), row["string"].as_str().unwrap(), "{a:?}");
        }
    }
}
