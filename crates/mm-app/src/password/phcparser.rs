//! Port of `channels/app/password/phcparser/parser.go`.
//!
//! The parser that decides which hasher a stored `Users.Password` value belongs to. [D-109]'s
//! blocker, and the piece that makes `GetHasherFromPHCString` writable.
//!
//! # The PHC format
//!
//! ```text
//! $<id>[$v=<version>][$<name>=<value>(,<name>=<value>)*][$<salt>[$<hash>]]
//! ```
//!
//! <https://github.com/P-H-C/phc-string-format/blob/master/phc-sf-spec.md>
//!
//! # Six things measured rather than read
//!
//! Every one of these was a wrong first answer from reading the Go, corrected by the corpus in
//! `fixtures/behaviour_phcparser.json`.
//!
//! **1. A bcrypt hash does not parse — and that is the design.** `$2a$10$<salt><digest>` gives
//! `id = "2a"`, then `10` as the salt, then a digest containing `.` and `$`, which are not
//! base64. The parse *fails*, and `GetHasherFromPHCString` reads that failure as "this is a
//! legacy row" and hands back bcrypt with the whole string in `Hash`. So the error path is the
//! hasher-detection mechanism, not an exception.
//!
//! **2. `MaxRunes` counts bytes — and it cannot be observed doing so.**
//! `bufio.NewReader(io.LimitReader(r, MaxRunes))`, and `io.LimitReader`'s bound is a byte count
//! despite the constant's name and its doc comment. So the name is wrong.
//!
//! But the bug is **latent**: every character in all four classes is single-byte, so throughout
//! any legal prefix the byte index and the rune index are equal, and the two limiters cut at
//! exactly the same place. `limiters_are_indistinguishable` proves that over the whole corpus by
//! running each case through both. This port counts bytes because that is the mechanism; a port
//! that honoured the name would be equally correct, and a future reader should not spend time on
//! it. The first draft of this module claimed a decisive input existed. There is none.
//!
//! **3. Over-long input succeeds with a truncated field.** The reader simply ends, and every
//! `EOF` branch treats that as a well-formed terminator. A 415-byte string comes back parsed,
//! with its 400-character hash cut to 241. Not an error — silent, and the reason a stored value
//! longer than 256 bytes would fail to verify with no diagnostic.
//!
//! **4. A NUL byte is swallowed, not rejected and not a terminator.** `read` returns the `eof`
//! sentinel — which *is* `rune(0)` — for both a read error and a real NUL. `scanIdent` breaks on
//! it **without unreading**, so the NUL is consumed and parsing continues with whatever follows:
//! `$x\0$a=1` parses cleanly as `$x$a=1`.
//!
//! **5. The first parameter name is validated against a wider class than the rest.** It is
//! scanned as `B64ENCODED` (before the parser knows whether it is a name or a salt) and then used
//! as a name without re-checking. So `$x$A=1` and `$x$a+b/c=1` are accepted, while the same names
//! after a comma are not.
//!
//! **6. `v` means three different things in three positions.** First position: consumed as the
//! **version**. Second position, after a version block: an **error** ("only allowed as the
//! version key"). Inside the comma loop: an ordinary parameter name. The check at parser.go:350
//! reads as though it guards the first position; it cannot, because the version branch above has
//! already taken that case.
//!
//! # And one upstream bug that shapes every error message
//!
//! `parseToken` returns `("", err)` on failure — it discards the literal it found. Callers then
//! format that empty string, so most errors say `found "", expected …` rather than naming the
//! offending character. The character survives only in the *wrapped* error, and only where a
//! caller used `%w`. Reproduced, because the text is what a server operator reads in a log.
//!
//! `parseToken`'s own message is also hard-coded to `expected '$'` whatever it was expecting,
//! which is visible wherever a caller returns it unwrapped.

use std::fmt;

use mm_model::utils::{StringMap, go_quote};

/// Port of `phcparser.MaxRunes` (parser.go:44).
///
/// **Counts bytes**, not runes — see the module docs. The name is upstream's.
pub const MAX_RUNES: usize = 256;

/// Port of `phcparser.Token` (parser.go:52).
///
/// A bit set, so `token & expected != 0` is the acceptance test and [`Token::IDENT`] — the OR of
/// the four literal tokens — matches wherever any specific one was asked for. That is what lets
/// `scanIdent` return one generic token while the caller still gets the check it wanted: the
/// narrowing is done by the *predicate* passed to the scanner, not by the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token(pub u32);

impl Token {
    /// An illegal token.
    pub const ILLEGAL: Token = Token(1);
    /// The end of the input.
    pub const EOF: Token = Token(2);
    /// A `$`.
    pub const DOLLARSIGN: Token = Token(4);
    /// A `,`.
    pub const COMMA: Token = Token(8);
    /// An `=`.
    pub const EQUALSIGN: Token = Token(16);
    /// A non-empty run of `[a-z0-9-]`.
    pub const FUNCTIONID: Token = Token(32);
    /// A non-empty run of `[a-z0-9-]`.
    pub const PARAMNAME: Token = Token(64);
    /// A non-empty run of `[a-zA-Z0-9/+.-]`.
    pub const PARAMVALUE: Token = Token(128);
    /// A non-empty run of `[A-Za-z0-9+/]`.
    pub const B64ENCODED: Token = Token(256);

    /// Port of `phcparser.IDENT` (parser.go:86) — any of the four literal tokens.
    pub const IDENT: Token =
        Token(Token::FUNCTIONID.0 | Token::PARAMNAME.0 | Token::PARAMVALUE.0 | Token::B64ENCODED.0);

    fn matches(self, expected: Token) -> bool {
        self.0 & expected.0 != 0
    }
}

/// Port of `phcparser.PHC` (parser.go:16).
///
/// `Params` is a `BTreeMap` rather than a `HashMap` so it marshals in Go's map order ([D-027]).
/// Nothing in this package depends on the ordering, but the fixture comparison does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phc {
    /// The identifier of the hashing function.
    pub id: String,
    /// The optional version of the hashing function.
    pub version: String,
    /// The parameter set.
    pub params: StringMap,
    /// The base64-encoded salt.
    pub salt: String,
    /// The base64-encoded hash.
    pub hash: String,
}

/// A parse failure.
///
/// Go builds these with `fmt.Errorf` and `%w`, so the *text* is the whole contract — nothing
/// calls `errors.Is` or `errors.As` on them, and `GetHasherFromPHCString` discards them entirely.
/// `message` therefore already contains the wrapped text, exactly as Go's `"outer: %w"` renders
/// it; `source` keeps the chain inspectable, which Go's would be too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhcError {
    message: String,
    source: Option<Box<PhcError>>,
}

impl PhcError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Go's `fmt.Errorf("...: %w", inner)` — the outer text already includes the inner's.
    fn wrapping(prefix: impl fmt::Display, inner: PhcError) -> Self {
        Self {
            message: format!("{prefix}: {inner}"),
            source: Some(Box::new(inner)),
        }
    }
}

impl fmt::Display for PhcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PhcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

// ---------------------------------------------------------------------------
// Character classes (parser.go:92-130)
// ---------------------------------------------------------------------------

/// `[a-z]`
fn is_lowercase_letter(ch: char) -> bool {
    ch.is_ascii_lowercase()
}

/// `[A-Za-z]`
fn is_letter(ch: char) -> bool {
    ch.is_ascii_uppercase() || ch.is_ascii_lowercase()
}

/// `[0-9]`
fn is_digit(ch: char) -> bool {
    ch.is_ascii_digit()
}

/// `[A-Za-z0-9+/]` — note there is no `=`, so base64 **padding is rejected**, and no `-`/`_`, so
/// the URL-safe alphabet is rejected too. Both hashers emit `RawStdEncoding`, which fits exactly.
fn is_b64(ch: char) -> bool {
    is_letter(ch) || is_digit(ch) || ch == '+' || ch == '/'
}

/// `[/+.-]`
fn is_symbol(ch: char) -> bool {
    matches!(ch, '/' | '+' | '.' | '-')
}

/// `[a-z0-9-]`
fn is_lowercase_letter_or_digit_or_minus(ch: char) -> bool {
    is_lowercase_letter(ch) || is_digit(ch) || ch == '-'
}

/// `[a-zA-Z0-9/+.-]`
fn is_letter_or_digit_or_symbol(ch: char) -> bool {
    is_letter(ch) || is_digit(ch) || is_symbol(ch)
}

/// No identifiers allowed — the predicate `scanSeparator` passes so that only the four separator
/// tokens can match.
fn none(_ch: char) -> bool {
    false
}

/// Go's `eof` sentinel (parser.go:90), which is `rune(0)`.
///
/// It being a real codepoint rather than a distinct value is the whole of finding 4 above: a NUL
/// byte in the input is indistinguishable from the end of it.
const EOF_RUNE: char = '\0';

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Port of `phcparser.Parser` (parser.go:38).
pub struct Parser<'a> {
    /// The input, already cut to [`MAX_RUNES`] **bytes** by [`Parser::new`].
    input: &'a [u8],
    pos: usize,
    /// The width of the last successful rune read, so `unread` can undo exactly it.
    ///
    /// `bufio.Reader.UnreadRune` errors unless the previous call was a successful `ReadRune`, and
    /// Go discards that error (`_ = p.reader.UnreadRune()`). Taking the value models both: a
    /// second consecutive unread, or one after end-of-input, does nothing.
    last_width: Option<usize>,
}

impl<'a> Parser<'a> {
    /// Port of `phcparser.New` (parser.go:47).
    ///
    /// Takes bytes rather than a reader because every caller in the Go tree passes
    /// `strings.NewReader(storedPassword)`, and because the corpus needs to express a stored value
    /// that is not valid UTF-8 — which a `&str` could not hold.
    ///
    /// The truncation happens here, as Go's `io.LimitReader` does, and it is a **byte** count.
    pub fn new(input: &'a [u8]) -> Self {
        Self::over(truncate_to_byte_limit(input))
    }

    /// A parser over an already-truncated slice.
    ///
    /// Split out so `limiters_are_indistinguishable` can drive the same state machine over both
    /// the byte-limited and the rune-limited prefix of an input.
    fn over(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            last_width: None,
        }
    }

    /// Convenience for the common case of a stored password that is valid UTF-8.
    ///
    /// Deliberately not `FromStr`: that trait is fallible and returns `Self`, while constructing a
    /// parser cannot fail and parsing is a separate step.
    pub fn for_str(input: &'a str) -> Self {
        Self::new(input.as_bytes())
    }

    /// Port of `(*Parser).read` (parser.go:133).
    ///
    /// Returns [`EOF_RUNE`] at the end of the input — and also for a literal NUL byte, because
    /// Go's does. Invalid UTF-8 decodes to U+FFFD one byte at a time, matching `bufio.ReadRune`.
    fn read(&mut self) -> char {
        if self.pos >= self.input.len() {
            self.last_width = None;
            return EOF_RUNE;
        }
        let (ch, width) = decode_rune(&self.input[self.pos..]);
        self.pos += width;
        self.last_width = Some(width);
        ch
    }

    /// Port of `(*Parser).unread` (parser.go:142).
    fn unread(&mut self) {
        if let Some(width) = self.last_width.take() {
            self.pos -= width;
        }
    }

    /// Port of `(*Parser).scan` (parser.go:146).
    fn scan(&mut self, is_ident_allowed_rune: fn(char) -> bool) -> (Token, String) {
        let ch = self.read();

        if is_ident_allowed_rune(ch) {
            self.unread();
            return self.scan_ident(is_ident_allowed_rune);
        }

        match ch {
            EOF_RUNE => (Token::EOF, String::new()),
            '$' => (Token::DOLLARSIGN, ch.to_string()),
            ',' => (Token::COMMA, ch.to_string()),
            '=' => (Token::EQUALSIGN, ch.to_string()),
            _ => (Token::ILLEGAL, ch.to_string()),
        }
    }

    /// Port of `(*Parser).scanIdent` (parser.go:170).
    ///
    /// Always returns the generic [`Token::IDENT`]: the narrowing was already done by the
    /// predicate. Note the `eof` branch breaks **without** unreading, which is what swallows a
    /// NUL byte rather than leaving it to be rejected by the next scan.
    fn scan_ident(&mut self, is_ident_allowed_rune: fn(char) -> bool) -> (Token, String) {
        let mut buf = String::new();
        buf.push(self.read());

        loop {
            let ch = self.read();
            if ch == EOF_RUNE {
                break;
            }
            if !is_ident_allowed_rune(ch) {
                self.unread();
                break;
            }
            buf.push(ch);
        }

        (Token::IDENT, buf)
    }

    /// Port of `(*Parser).scanSeparator` (parser.go:198).
    fn scan_separator(&mut self) -> (Token, String) {
        self.scan(none)
    }

    /// Port of `(*Parser).parseToken` (parser.go:208).
    ///
    /// Two upstream oddities are reproduced deliberately:
    ///
    /// - the message says `expected '$'` for **every** expected token;
    /// - the literal is **discarded** on failure, so callers that format the returned value print
    ///   `found ""`. It survives only inside this error's own text.
    fn parse_token(&mut self, expected: Token) -> Result<String, PhcError> {
        let allowed_rune_fn: fn(char) -> bool = match expected {
            Token::FUNCTIONID | Token::PARAMNAME => is_lowercase_letter_or_digit_or_minus,
            Token::PARAMVALUE => is_letter_or_digit_or_symbol,
            Token::B64ENCODED => is_b64,
            _ => none,
        };

        let (token, literal) = self.scan(allowed_rune_fn);
        if !token.matches(expected) {
            return Err(PhcError::new(format!(
                "found {}, expected '$'",
                go_quote(&literal)
            )));
        }

        Ok(literal)
    }

    /// Port of `(*Parser).parseFunctionId` (parser.go:230).
    fn parse_function_id(&mut self) -> Result<String, PhcError> {
        // Go returns the *literal* alongside the error here and then formats it — but
        // `parseToken` blanked it, so both messages below always report `found ""`.
        self.parse_token(Token::DOLLARSIGN)
            .map_err(|_| PhcError::new("found \"\", expected '$'"))?;

        self.parse_token(Token::FUNCTIONID)
            .map_err(|_| PhcError::new("found \"\", expected a function identifier"))
    }

    /// Port of `(*Parser).parseHash` (parser.go:244).
    ///
    /// Requires the input to end immediately afterwards, so a fifth `$`-delimited field is an
    /// error rather than ignored.
    fn parse_hash(&mut self) -> Result<String, PhcError> {
        let hash = self
            .parse_token(Token::B64ENCODED)
            .map_err(|_| PhcError::new("found \"\", expected the hash"))?;

        self.parse_token(Token::EOF)
            .map_err(|_| PhcError::new("found \"\", expected EOF"))?;

        Ok(hash)
    }

    /// Port of `(*Parser).parseParamRHS` (parser.go:262).
    ///
    /// Returns `parseToken`'s error **unwrapped**, which is the one place its hard-coded
    /// `expected '$'` text reaches a caller verbatim.
    fn parse_param_rhs(&mut self) -> Result<String, PhcError> {
        self.parse_token(Token::EQUALSIGN)?;
        self.parse_token(Token::PARAMVALUE)
    }

    /// Port of `(*Parser).Parse` (parser.go:274).
    ///
    /// On failure Go returns a **zero** `PHC` — discarding the `Params` map it allocated on the
    /// first line — so the map is nil on every error path and non-nil on every success. A `Result`
    /// expresses that better than Go's pair does, and no caller reads the value on error.
    pub fn parse(&mut self) -> Result<Phc, PhcError> {
        let mut out = Phc::default();

        // First, we expect '$functionId'.
        let id = self
            .parse_function_id()
            .map_err(|e| PhcError::wrapping("failed to parse function ID", e))?;
        out.id = id;

        // Now either EOF, or a '$' and we continue.
        match self.scan_separator() {
            (Token::EOF, _) => return Ok(out),
            (Token::DOLLARSIGN, _) => {}
            (_, literal) => {
                return Err(PhcError::new(format!(
                    "found {}, expected '$' or EOF",
                    go_quote(&literal)
                )));
            }
        }

        // The next identifier is the version key, a parameter name, or the salt. `B64ENCODED` is
        // a superset of all three, so it is used before we know which — **and that is why the
        // first parameter name may hold characters a later one may not**.
        let mut version_key_or_param_name_or_salt =
            self.parse_token(Token::B64ENCODED).map_err(|e| {
                PhcError::wrapping(
                    "found \"\", expected the version key, 'v', a parameter name or the salt",
                    e,
                )
            })?;

        if version_key_or_param_name_or_salt == "v" {
            // `$v=versionStr`. This branch is why the "v is only allowed as the version key"
            // check further down can never fire for a *first*-position `v`.
            let version_str = self
                .parse_param_rhs()
                .map_err(|e| PhcError::wrapping("failed parsing version string", e))?;
            out.version = version_str;

            match self.scan_separator() {
                (Token::EOF, _) => return Ok(out),
                (Token::DOLLARSIGN, _) => {}
                (_, literal) => {
                    return Err(PhcError::new(format!(
                        "found {}, expected '$' or EOF",
                        go_quote(&literal)
                    )));
                }
            }

            version_key_or_param_name_or_salt =
                self.parse_token(Token::B64ENCODED).map_err(|_| {
                    PhcError::new("found \"\", expected a parameter name or the version key, 'v'")
                })?;
        }

        let param_name_or_salt = version_key_or_param_name_or_salt;

        match self.scan_separator() {
            // '=' — it was a parameter name.
            (Token::EQUALSIGN, _) => {
                let param_name = param_name_or_salt;
                // Reachable only in *second* position, i.e. after a version block.
                if param_name == "v" {
                    return Err(PhcError::new(
                        "found 'v' as a parameter name, which is only allowed as the version key",
                    ));
                }
                let param_value = self.parse_token(Token::PARAMVALUE).map_err(|_| {
                    PhcError::new(format!(
                        "found \"\", expected a value for parameter {}",
                        go_quote(&param_name)
                    ))
                })?;

                out.params.insert(param_name, param_value);
            }
            // '$' or EOF — it was the salt.
            (token, _) if token == Token::DOLLARSIGN || token == Token::EOF => {
                out.salt = param_name_or_salt;

                if token == Token::DOLLARSIGN {
                    out.hash = self.parse_hash()?;
                }

                return Ok(out);
            }
            (_, literal) => {
                return Err(PhcError::new(format!(
                    "found {}, expected either '$', or '=' or EOF",
                    go_quote(&literal)
                )));
            }
        }

        // A parameter value was just parsed. Now: EOF ends it, ',' starts another pair, '$'
        // begins `salt[$hash]`.
        loop {
            match self.scan_separator() {
                (Token::EOF, _) => return Ok(out),
                (Token::COMMA, _) => {
                    // Note the narrower class here than the first parameter name got.
                    let param_name = self.parse_token(Token::PARAMNAME)?;
                    let param_value = self.parse_param_rhs().map_err(|e| {
                        PhcError::wrapping(
                            format!(
                                "failed parsing value from parameter {}",
                                go_quote(&param_name)
                            ),
                            e,
                        )
                    })?;
                    out.params.insert(param_name, param_value);
                }
                (Token::DOLLARSIGN, _) => {
                    out.salt = self.parse_token(Token::B64ENCODED)?;

                    match self.scan_separator() {
                        (Token::DOLLARSIGN, _) => {
                            out.hash = self.parse_hash()?;
                            return Ok(out);
                        }
                        (Token::EOF, _) => return Ok(out),
                        (_, new_literal) => {
                            return Err(PhcError::new(format!(
                                "found {}, expected either '$', or EOF",
                                go_quote(&new_literal)
                            )));
                        }
                    }
                }
                (_, literal) => {
                    return Err(PhcError::new(format!(
                        "found {}, expected either ',', '$' or EOF",
                        go_quote(&literal)
                    )));
                }
            }
        }
    }
}

/// Decode one rune the way `bufio.Reader.ReadRune` does.
///
/// Go returns `(utf8.RuneError, 1, nil)` for a byte that does not begin a valid sequence, and for
/// a sequence truncated by the end of the input — **not** an error. So invalid UTF-8 becomes
/// U+FFFD one byte at a time, and U+FFFD is in none of the four character classes, which is how
/// a non-UTF-8 stored password ends up rejected rather than mangled.
fn decode_rune(bytes: &[u8]) -> (char, usize) {
    let max = bytes.len().min(4);
    for n in 1..=max {
        if let Ok(s) = std::str::from_utf8(&bytes[..n])
            && let Some(ch) = s.chars().next()
        {
            return (ch, n);
        }
    }
    (char::REPLACEMENT_CHARACTER, 1)
}

/// Go's `io.LimitReader(r, MaxRunes)` — a **byte** count, whatever the constant is called.
fn truncate_to_byte_limit(input: &[u8]) -> &[u8] {
    &input[..input.len().min(MAX_RUNES)]
}

/// What the limiter would do if it honoured its constant's name.
///
/// Not used in production — it exists so the equivalence can be *demonstrated* rather than
/// asserted. See `limiters_are_indistinguishable`.
#[cfg(test)]
fn truncate_to_rune_limit(input: &[u8]) -> &[u8] {
    let mut end = 0;
    let mut runes = 0;
    while end < input.len() && runes < MAX_RUNES {
        let (_, width) = decode_rune(&input[end..]);
        end += width;
        runes += 1;
    }
    &input[..end]
}

/// Convenience: parse a stored password string.
pub fn parse(input: &str) -> Result<Phc, PhcError> {
    Parser::for_str(input).parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_bits_are_a_set() {
        assert_eq!(Token::IDENT.0, 32 | 64 | 128 | 256);
        assert!(Token::IDENT.matches(Token::FUNCTIONID));
        assert!(Token::IDENT.matches(Token::B64ENCODED));
        assert!(!Token::ILLEGAL.matches(Token::DOLLARSIGN));
        assert!(!Token::EOF.matches(Token::B64ENCODED));
    }

    #[test]
    fn a_pbkdf2_string_round_trips_its_parts() {
        let phc = parse("$pbkdf2$f=SHA256,w=600000,l=32$c2FsdA$aGFzaA").unwrap();
        assert_eq!(phc.id, "pbkdf2");
        assert_eq!(phc.params.get("f").map(String::as_str), Some("SHA256"));
        assert_eq!(phc.params.get("w").map(String::as_str), Some("600000"));
        assert_eq!(phc.params.get("l").map(String::as_str), Some("32"));
        assert_eq!(phc.salt, "c2FsdA");
        assert_eq!(phc.hash, "aGFzaA");
        assert_eq!(phc.version, "");
    }

    /// The failure that drives hasher selection.
    #[test]
    fn a_bcrypt_string_does_not_parse() {
        assert!(parse("$2a$10$gSZylAupRaDSbThPRdNHa.a91BqVuwn.7B57P60bCRGYhXZtYfOCK").is_err());
    }

    #[test]
    fn decode_rune_matches_gos_read_rune() {
        assert_eq!(decode_rune(b"a"), ('a', 1));
        assert_eq!(decode_rune("é".as_bytes()), ('é', 2));
        assert_eq!(decode_rune("中".as_bytes()), ('中', 3));
        assert_eq!(decode_rune("🔒".as_bytes()), ('🔒', 4));
        // A lone continuation byte, and a sequence truncated by the end of the slice.
        assert_eq!(decode_rune(b"\x80"), (char::REPLACEMENT_CHARACTER, 1));
        assert_eq!(decode_rune(b"\xc3"), (char::REPLACEMENT_CHARACTER, 1));
        // A NUL is an ordinary codepoint here; `read` is what conflates it with the end.
        assert_eq!(decode_rune(b"\0"), ('\0', 1));
    }

    /// `unread` after end-of-input must not move the cursor — Go's `UnreadRune` errors there and
    /// the error is discarded.
    #[test]
    fn unread_at_the_end_is_a_no_op() {
        let mut p = Parser::for_str("a");
        assert_eq!(p.read(), 'a');
        assert_eq!(p.read(), EOF_RUNE);
        p.unread();
        p.unread();
        assert_eq!(p.read(), EOF_RUNE);
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../../fixtures/behaviour_phcparser.json");
            serde_json::from_str(raw).expect("behaviour_phcparser.json parses")
        })
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["MaxRunes"], MAX_RUNES);
        assert_eq!(
            c["limiter_counts"], "bytes — io.LimitReader(r, MaxRunes)",
            "the constant's name says runes; the mechanism says bytes"
        );
    }

    #[test]
    fn token_bits_match_go() {
        let t = &oracle()["tokens"];
        assert_eq!(t["ILLEGAL"], Token::ILLEGAL.0);
        assert_eq!(t["EOF"], Token::EOF.0);
        assert_eq!(t["DOLLARSIGN"], Token::DOLLARSIGN.0);
        assert_eq!(t["COMMA"], Token::COMMA.0);
        assert_eq!(t["EQUALSIGN"], Token::EQUALSIGN.0);
        assert_eq!(t["FUNCTIONID"], Token::FUNCTIONID.0);
        assert_eq!(t["PARAMNAME"], Token::PARAMNAME.0);
        assert_eq!(t["PARAMVALUE"], Token::PARAMVALUE.0);
        assert_eq!(t["B64ENCODED"], Token::B64ENCODED.0);
        assert_eq!(t["IDENT"], Token::IDENT.0);
        assert_eq!(t["ident_is_or"], true);
    }

    /// The whole corpus: every parsed field, and every error **message**, against Go's.
    #[test]
    fn parse_matches_go() {
        let cases = oracle()["cases"].as_array().unwrap();
        assert!(cases.len() >= 60, "the corpus should not have shrunk");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = STANDARD
                .decode(case["input_b64"].as_str().unwrap())
                .unwrap_or_else(|e| panic!("{name}: input_b64: {e}"));

            let got = Parser::new(&input).parse();

            assert_eq!(
                got.is_ok(),
                case["ok"].as_bool().unwrap(),
                "{name}: ok — Go said {:?}, we said {got:?}",
                case["error"]
            );

            match got {
                Ok(phc) => {
                    assert_eq!(phc.id, case["id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(
                        phc.version,
                        case["version"].as_str().unwrap(),
                        "{name}: version"
                    );
                    assert_eq!(phc.salt, case["salt"].as_str().unwrap(), "{name}: salt");
                    assert_eq!(phc.hash, case["hash"].as_str().unwrap(), "{name}: hash");

                    let want: StringMap = serde_json::from_value(case["params"].clone())
                        .unwrap_or_else(|e| panic!("{name}: params: {e}"));
                    assert_eq!(phc.params, want, "{name}: params");
                    assert_eq!(
                        case["params_is_nil"], false,
                        "{name}: a successful parse always has a non-nil map"
                    );
                }
                Err(err) => {
                    assert_eq!(
                        err.to_string(),
                        case["error"].as_str().unwrap(),
                        "{name}: error text"
                    );
                    assert_eq!(
                        case["params_is_nil"], true,
                        "{name}: Go discards the allocated map on every error path"
                    );
                }
            }
        }
    }

    /// The four character classes, swept through the position each one governs.
    ///
    /// Driven through `Parse` rather than against the predicates directly, because that measures
    /// the class **as the parser applies it** — which is how finding 5 (the first parameter name
    /// gets a wider class than the rest) became visible at all.
    #[test]
    fn character_classes_match_go() {
        let classes = &oracle()["character_classes"];

        /// A position in the grammar, and a minimal input that puts a codepoint exactly there.
        type Probe = (&'static str, fn(&str) -> String);

        let builders: [Probe; 4] = [
            ("function_id", |s| format!("$a{s}b")),
            ("param_name", |s| format!("$x$a{s}b=1")),
            ("param_value", |s| format!("$x$k=a{s}b")),
            ("salt", |s| format!("$x$a{s}b")),
        ];
        for (position, build) in builders {
            let probes = classes[position].as_array().unwrap();
            assert!(
                probes.len() >= 130,
                "{position}: the sweep should not shrink"
            );

            for probe in probes {
                let cp = u32::try_from(probe["codepoint"].as_i64().unwrap()).unwrap();
                let ch = char::from_u32(cp).unwrap();
                let input = build(&ch.to_string());

                let got = parse(&input);
                assert_eq!(
                    got.is_ok(),
                    probe["ok"].as_bool().unwrap(),
                    "{position} U+{cp:04X}: ok"
                );

                if let Ok(phc) = got {
                    let field = match position {
                        "function_id" => phc.id.clone(),
                        "param_name" => phc.params.keys().cloned().collect::<Vec<_>>().join(","),
                        "param_value" => phc.params.get("k").cloned().unwrap_or_default(),
                        _ => phc.salt.clone(),
                    };
                    assert_eq!(
                        field,
                        probe["field"].as_str().unwrap(),
                        "{position} U+{cp:04X}: field"
                    );
                    assert_eq!(
                        field.contains(ch),
                        probe["contains"].as_bool().unwrap(),
                        "{position} U+{cp:04X}: membership"
                    );
                }
            }
        }
    }

    /// Finding 2, corrected twice: the limiter counts **bytes**, and it is observable — but only
    /// where the cut splits a character.
    ///
    /// `io.LimitReader`'s bound is a byte count, so `MaxRunes`' name and doc comment are wrong.
    /// Whether that is *reachable* took two attempts to get right:
    ///
    /// - The first draft asserted a hand-picked "decisive" input. It was not decisive:
    ///   `"$x$" + "a"*253 + "é"*100` is 456 bytes and 356 runes, but its legal prefix is 256
    ///   characters under either rule, so both limiters stop before the padding. Every character
    ///   in all four classes is single-byte, so within a legal prefix byte index == rune index.
    /// - The second draft concluded from that the two are indistinguishable. Also wrong. When the
    ///   256-**byte** cut lands *inside* a multi-byte character, Go decodes the orphaned lead byte
    ///   as U+FFFD where a rune limiter would have delivered the whole character — and the error
    ///   text differs. `legal=252` with any pad, and `legal=250` with a 4-byte pad, are those
    ///   cases; Go reports `found "\u{fffd}"` for them and `found "é"` for their neighbours.
    ///
    /// Both drafts survived their own tests. A mutation — switching this port to count runes —
    /// is what exposed the first, and it named the same four inputs Go distinguishes.
    #[test]
    fn the_limit_counts_bytes_and_the_boundary_shows_it() {
        let rows = oracle()["limit_boundary"].as_array().unwrap();
        assert_eq!(rows.len(), 24, "8 prefix lengths x 3 pad widths");

        let mut split_a_character = 0;

        for row in rows {
            let legal = row["legal_prefix"].as_u64().unwrap() as usize;
            let pad = row["pad"].as_str().unwrap();
            let label = format!("legal={legal} pad={pad}");
            let input = format!("$x${}{}", "a".repeat(legal), pad.repeat(100));

            assert_eq!(input.len() as u64, row["input_bytes"].as_u64().unwrap());
            assert_eq!(
                input.chars().count() as u64,
                row["input_runes"].as_u64().unwrap()
            );

            // 1. We match Go.
            match Parser::new(input.as_bytes()).parse() {
                Ok(phc) => {
                    assert_eq!(row["ok"], true, "{label}: Go failed and we did not");
                    assert_eq!(phc.salt, row["salt"].as_str().unwrap(), "{label}: salt");
                }
                Err(err) => {
                    assert_eq!(row["ok"], false, "{label}: Go parsed and we did not");
                    assert_eq!(
                        err.to_string(),
                        row["error"].as_str().unwrap(),
                        "{label}: error text"
                    );
                }
            }

            // 2. And where the cut splits a character, a rune-counting limiter would NOT.
            let by_runes = Parser::over(truncate_to_rune_limit(input.as_bytes())).parse();
            let by_bytes = Parser::over(truncate_to_byte_limit(input.as_bytes())).parse();
            if by_runes != by_bytes {
                split_a_character += 1;
                assert!(
                    row["error"]
                        .as_str()
                        .unwrap_or_default()
                        .contains('\u{fffd}'),
                    "{label}: the limiters differ, so Go's error should carry the replacement \
                     character it decoded from the orphaned lead byte"
                );
            }
        }

        assert_eq!(
            split_a_character, 4,
            "exactly the inputs whose 256-byte cut falls inside a multi-byte character"
        );

        // The mechanism, as the oracle records it.
        assert_eq!(
            oracle()["constants"]["limiter_counts"],
            "bytes — io.LimitReader(r, MaxRunes)"
        );
        assert_eq!(
            oracle()["limit"]["longest_intact_ascii_salt"]
                .as_u64()
                .unwrap() as usize
                + 3,
            MAX_RUNES,
            "the longest salt that survives, plus the 3-byte '$x$' head, is exactly the limit"
        );
    }

    /// Finding 3: over-long input **succeeds** with a truncated field.
    #[test]
    fn over_long_input_truncates_silently_rather_than_failing() {
        let t = &oracle()["limit"]["over_limit_truncates_not_errors"];
        let input = format!("$x$c29tZXNhbHQ${}", "b".repeat(400));
        assert_eq!(input.len() as u64, t["input_bytes"].as_u64().unwrap());

        let phc = parse(&input).expect("Go parses this; so must we");
        assert_eq!(t["parses"], true);
        assert_eq!(phc.hash.len() as u64, t["hash_len_out"].as_u64().unwrap());
        assert!(
            phc.hash.len() < 400,
            "the hash comes back shorter than the input carried, with no error"
        );
        assert_eq!(t["hash_is_short"], true);
    }

    /// Finding 5 and 6, stated as assertions rather than as prose.
    ///
    /// These all live in `parse_matches_go` too; they are repeated here because each is a
    /// conclusion a reader would otherwise have to reconstruct from a 60-case loop.
    #[test]
    fn the_first_parameter_name_and_v_are_special_cased() {
        let by_name = |want: &str| {
            oracle()["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == want)
                .unwrap_or_else(|| panic!("case {want} is missing"))
        };

        // 5. The first name gets B64ENCODED's class; later names get PARAMNAME's.
        assert_eq!(by_name("first_param_name_may_be_uppercase")["ok"], true);
        assert_eq!(by_name("first_param_name_may_hold_b64_symbols")["ok"], true);
        assert_eq!(by_name("later_param_name_uppercase")["ok"], false);
        assert_eq!(by_name("later_param_name_b64_symbols")["ok"], false);
        assert_eq!(
            parse("$x$A=1").unwrap().params.get("A").map(String::as_str),
            Some("1")
        );
        assert!(parse("$x$a=1,B=2").is_err());

        // 6. `v` means three different things in three positions.
        assert_eq!(parse("$x$v=2$c29tZXNhbHQ").unwrap().version, "2");
        assert_eq!(
            parse("$x$a=1,v=2$c29tZXNhbHQ")
                .unwrap()
                .params
                .get("v")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(
            parse("$x$v=1$v=2").unwrap_err().to_string(),
            "found 'v' as a parameter name, which is only allowed as the version key"
        );
    }

    /// Finding 4: a NUL is swallowed mid-identifier rather than ending the parse or failing it.
    #[test]
    fn a_nul_byte_is_swallowed() {
        // `$x\0$a=1` parses exactly as `$x$a=1` would.
        let with_nul = Parser::new(b"$x\x00$a=1").parse().unwrap();
        let without = parse("$x$a=1").unwrap();
        assert_eq!(with_nul, without);

        // And mid-identifier it splits the identifier instead, which then fails.
        assert!(Parser::new(b"$arg\x00on2id").parse().is_err());
    }
}
