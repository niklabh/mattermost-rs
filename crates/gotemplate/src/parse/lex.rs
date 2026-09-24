//! Port of `text/template/parse/lex.go`: the template lexer.
//!
//! Go runs the lexer as a state machine the parser pulls from one item at a time
//! (`nextItem`); this port keeps that shape, so positions, line counts and every error string
//! match item for item.

use crate::strconv::{is_digit, is_letter};

/// `itemType` (lex.go:38).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ItemType {
    Error,
    Bool,
    Char,
    CharConstant,
    Comment,
    Complex,
    Assign,
    Declare,
    Eof,
    Field,
    Identifier,
    LeftDelim,
    LeftParen,
    Number,
    Pipe,
    RawString,
    RightDelim,
    RightParen,
    Space,
    String,
    Text,
    Variable,
    // Keywords appear after all the rest.
    Keyword,
    Block,
    Break,
    Continue,
    Dot,
    Define,
    Else,
    End,
    If,
    Nil,
    Range,
    Template,
    With,
}

fn keyword(word: &str) -> Option<ItemType> {
    Some(match word {
        "." => ItemType::Dot,
        "block" => ItemType::Block,
        "break" => ItemType::Break,
        "continue" => ItemType::Continue,
        "define" => ItemType::Define,
        "else" => ItemType::Else,
        "end" => ItemType::End,
        "if" => ItemType::If,
        "range" => ItemType::Range,
        "nil" => ItemType::Nil,
        "template" => ItemType::Template,
        "with" => ItemType::With,
        _ => return None,
    })
}

/// `item` (lex.go:15).
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub typ: ItemType,
    pub pos: usize,
    pub val: String,
    pub line: usize,
}

impl std::fmt::Display for Item {
    /// `item.String` (lex.go:22).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.typ {
            ItemType::Eof => f.write_str("EOF"),
            ItemType::Error => f.write_str(&self.val),
            t if t > ItemType::Keyword => write!(f, "<{}>", self.val),
            _ => {
                if self.val.chars().count() > 10 {
                    // `%.10q...`: the first ten runes, quoted.
                    let head: String = self.val.chars().take(10).collect();
                    write!(f, "{}...", crate::strconv::quote(&head))
                } else {
                    f.write_str(&crate::strconv::quote(&self.val))
                }
            }
        }
    }
}

const EOF: i32 = -1;
const SPACE_CHARS: &str = " \t\r\n";
const TRIM_MARKER: u8 = b'-';
const TRIM_MARKER_LEN: usize = 2;
const LEFT_COMMENT: &str = "/*";
const RIGHT_COMMENT: &str = "*/";

/// `lexOptions` (lex.go:130).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LexOptions {
    pub emit_comment: bool,
    pub break_ok: bool,
    pub continue_ok: bool,
}

#[derive(Clone, Copy)]
enum State {
    Text,
    LeftDelim,
    Comment,
    RightDelim,
    InsideAction,
    Space,
    Identifier,
    Field,
    Variable,
    Char,
    Number,
    Quote,
    RawQuote,
}

/// `lexer` (lex.go:114).
pub(crate) struct Lexer {
    input: String,
    left_delim: String,
    right_delim: String,
    pos: usize,
    start: usize,
    at_eof: bool,
    paren_depth: i32,
    line: usize,
    start_line: usize,
    item: Item,
    inside_action: bool,
    pub options: LexOptions,
}

fn is_space(r: i32) -> bool {
    r == ' ' as i32 || r == '\t' as i32 || r == '\r' as i32 || r == '\n' as i32
}

/// `isAlphaNumeric` (lex.go:680): `_`, a Unicode letter or a Unicode digit.
fn is_alpha_numeric(r: i32) -> bool {
    r == '_' as i32 || (r >= 0 && (is_letter(r as u32) || is_digit(r as u32)))
}

fn has_left_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0] == TRIM_MARKER && is_space(i32::from(b[1]))
}

fn has_right_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && is_space(i32::from(b[0])) && b[1] == TRIM_MARKER
}

fn right_trim_length(s: &str) -> usize {
    s.len() - s.trim_end_matches(|c| SPACE_CHARS.contains(c)).len()
}

fn left_trim_length(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c| SPACE_CHARS.contains(c)).len()
}

impl Lexer {
    /// `lex` (lex.go:219).
    pub(crate) fn new(input: &str, left: &str, right: &str) -> Self {
        Lexer {
            input: input.to_string(),
            left_delim: if left.is_empty() { "{{" } else { left }.to_string(),
            right_delim: if right.is_empty() { "}}" } else { right }.to_string(),
            pos: 0,
            start: 0,
            at_eof: false,
            paren_depth: 0,
            line: 1,
            start_line: 1,
            item: Item {
                typ: ItemType::Eof,
                pos: 0,
                val: String::new(),
                line: 1,
            },
            inside_action: false,
            options: LexOptions::default(),
        }
    }

    fn rest(&self) -> &str {
        self.input.get(self.pos..).unwrap_or("")
    }

    fn next(&mut self) -> i32 {
        if self.pos >= self.input.len() {
            self.at_eof = true;
            return EOF;
        }
        let r = self.rest().chars().next().map_or(0xfffd, |c| c as i32);
        let w = self.rest().chars().next().map_or(1, char::len_utf8);
        self.pos += w;
        if r == '\n' as i32 {
            self.line += 1;
        }
        r
    }

    fn peek(&mut self) -> i32 {
        let r = self.next();
        self.backup();
        r
    }

    fn backup(&mut self) {
        if !self.at_eof
            && self.pos > 0
            && let Some(c) = self.input[..self.pos].chars().next_back()
        {
            self.pos -= c.len_utf8();
            if c == '\n' {
                self.line -= 1;
            }
        }
    }

    fn this_item(&mut self, t: ItemType) -> Item {
        let i = Item {
            typ: t,
            pos: self.start,
            val: self.input[self.start..self.pos].to_string(),
            line: self.start_line,
        };
        self.start = self.pos;
        self.start_line = self.line;
        i
    }

    fn emit(&mut self, t: ItemType) -> Option<State> {
        let i = self.this_item(t);
        self.emit_item(i)
    }

    fn emit_item(&mut self, i: Item) -> Option<State> {
        self.item = i;
        None
    }

    fn ignore(&mut self) {
        self.line += self.input[self.start..self.pos].matches('\n').count();
        self.start = self.pos;
        self.start_line = self.line;
    }

    fn accept(&mut self, valid: &str) -> bool {
        let r = self.next();
        if r >= 0 && char::from_u32(r as u32).is_some_and(|c| valid.contains(c)) {
            return true;
        }
        self.backup();
        false
    }

    fn accept_run(&mut self, valid: &str) {
        loop {
            let r = self.next();
            if !(r >= 0 && char::from_u32(r as u32).is_some_and(|c| valid.contains(c))) {
                break;
            }
        }
        self.backup();
    }

    fn errorf(&mut self, msg: String) -> Option<State> {
        self.item = Item {
            typ: ItemType::Error,
            pos: self.start,
            val: msg,
            line: self.start_line,
        };
        self.start = 0;
        self.pos = 0;
        self.input.clear();
        None
    }

    /// `nextItem` (lex.go:204).
    pub(crate) fn next_item(&mut self) -> Item {
        self.item = Item {
            typ: ItemType::Eof,
            pos: self.pos,
            val: "EOF".to_string(),
            line: self.start_line,
        };
        let mut state = if self.inside_action {
            State::InsideAction
        } else {
            State::Text
        };
        loop {
            let next = match state {
                State::Text => self.lex_text(),
                State::LeftDelim => self.lex_left_delim(),
                State::Comment => self.lex_comment(),
                State::RightDelim => self.lex_right_delim(),
                State::InsideAction => self.lex_inside_action(),
                State::Space => self.lex_space(),
                State::Identifier => self.lex_identifier(),
                State::Field => self.lex_field_or_variable(ItemType::Field),
                State::Variable => self.lex_variable(),
                State::Char => self.lex_char(),
                State::Number => self.lex_number(),
                State::Quote => self.lex_quote(),
                State::RawQuote => self.lex_raw_quote(),
            };
            match next {
                Some(s) => state = s,
                None => return self.item.clone(),
            }
        }
    }

    fn lex_text(&mut self) -> Option<State> {
        if let Some(x) = self.rest().find(self.left_delim.as_str()) {
            if x > 0 {
                self.pos += x;
                let mut trim_length = 0;
                let delim_end = self.pos + self.left_delim.len();
                if has_left_trim_marker(&self.input[delim_end..]) {
                    trim_length = right_trim_length(&self.input[self.start..self.pos]);
                }
                self.pos -= trim_length;
                self.line += self.input[self.start..self.pos].matches('\n').count();
                let i = self.this_item(ItemType::Text);
                self.pos += trim_length;
                self.ignore();
                if !i.val.is_empty() {
                    return self.emit_item(i);
                }
            }
            return Some(State::LeftDelim);
        }
        self.pos = self.input.len();
        if self.pos > self.start {
            self.line += self.input[self.start..self.pos].matches('\n').count();
            return self.emit(ItemType::Text);
        }
        self.emit(ItemType::Eof)
    }

    fn at_right_delim(&self) -> (bool, bool) {
        let rest = self.rest();
        if has_right_trim_marker(rest)
            && rest
                .get(TRIM_MARKER_LEN..)
                .is_some_and(|r| r.starts_with(self.right_delim.as_str()))
        {
            return (true, true);
        }
        if rest.starts_with(self.right_delim.as_str()) {
            return (true, false);
        }
        (false, false)
    }

    fn lex_left_delim(&mut self) -> Option<State> {
        self.pos += self.left_delim.len();
        let trim_space = has_left_trim_marker(self.rest());
        let after_marker = if trim_space { TRIM_MARKER_LEN } else { 0 };
        if self
            .input
            .get(self.pos + after_marker..)
            .is_some_and(|r| r.starts_with(LEFT_COMMENT))
        {
            self.pos += after_marker;
            self.ignore();
            return Some(State::Comment);
        }
        let i = self.this_item(ItemType::LeftDelim);
        self.inside_action = true;
        self.pos += after_marker;
        self.ignore();
        self.paren_depth = 0;
        self.emit_item(i)
    }

    fn lex_comment(&mut self) -> Option<State> {
        self.pos += LEFT_COMMENT.len();
        let Some(x) = self.rest().find(RIGHT_COMMENT) else {
            return self.errorf("unclosed comment".to_string());
        };
        self.pos += x + RIGHT_COMMENT.len();
        let (delim, trim_space) = self.at_right_delim();
        if !delim {
            return self.errorf("comment ends before closing delimiter".to_string());
        }
        self.line += self.input[self.start..self.pos].matches('\n').count();
        let i = self.this_item(ItemType::Comment);
        if trim_space {
            self.pos += TRIM_MARKER_LEN;
        }
        self.pos += self.right_delim.len();
        if trim_space {
            self.pos += left_trim_length(self.rest());
        }
        self.ignore();
        if self.options.emit_comment {
            return self.emit_item(i);
        }
        Some(State::Text)
    }

    fn lex_right_delim(&mut self) -> Option<State> {
        let (_, trim_space) = self.at_right_delim();
        if trim_space {
            self.pos += TRIM_MARKER_LEN;
            self.ignore();
        }
        self.pos += self.right_delim.len();
        let i = self.this_item(ItemType::RightDelim);
        if trim_space {
            self.pos += left_trim_length(self.rest());
            self.ignore();
        }
        self.inside_action = false;
        self.emit_item(i)
    }

    fn lex_inside_action(&mut self) -> Option<State> {
        let (delim, _) = self.at_right_delim();
        if delim {
            if self.paren_depth == 0 {
                return Some(State::RightDelim);
            }
            return self.errorf("unclosed left paren".to_string());
        }
        let r = self.next();
        if r == EOF {
            return self.errorf("unclosed action".to_string());
        }
        if is_space(r) {
            self.backup();
            return Some(State::Space);
        }
        let c = char::from_u32(r as u32).unwrap_or('\u{fffd}');
        match c {
            '=' => self.emit(ItemType::Assign),
            ':' => {
                if self.next() != '=' as i32 {
                    return self.errorf("expected :=".to_string());
                }
                self.emit(ItemType::Declare)
            }
            '|' => self.emit(ItemType::Pipe),
            '"' => Some(State::Quote),
            '`' => Some(State::RawQuote),
            '$' => Some(State::Variable),
            '\'' => Some(State::Char),
            '.' => {
                // Special look-ahead for ".field" so we don't break backup().
                if self.pos < self.input.len() {
                    let b = self.input.as_bytes()[self.pos];
                    if !b.is_ascii_digit() {
                        return Some(State::Field);
                    }
                }
                self.backup();
                Some(State::Number)
            }
            '+' | '-' | '0'..='9' => {
                self.backup();
                Some(State::Number)
            }
            _ if is_alpha_numeric(r) => {
                self.backup();
                Some(State::Identifier)
            }
            '(' => {
                self.paren_depth += 1;
                self.emit(ItemType::LeftParen)
            }
            ')' => {
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return self.errorf("unexpected right paren".to_string());
                }
                self.emit(ItemType::RightParen)
            }
            _ if r < 0x80 && (0x20..0x7f).contains(&r) => self.emit(ItemType::Char),
            _ => {
                let msg = format!("unrecognized character in action: {}", go_sharp_u(r));
                self.errorf(msg)
            }
        }
    }

    fn lex_space(&mut self) -> Option<State> {
        let mut num_spaces = 0;
        loop {
            let r = self.peek();
            if !is_space(r) {
                break;
            }
            self.next();
            num_spaces += 1;
        }
        let tail = &self.input[self.pos - 1..];
        if has_right_trim_marker(tail)
            && tail
                .get(TRIM_MARKER_LEN..)
                .is_some_and(|r| r.starts_with(self.right_delim.as_str()))
        {
            self.backup();
            if num_spaces == 1 {
                return Some(State::RightDelim);
            }
        }
        self.emit(ItemType::Space)
    }

    fn lex_identifier(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if is_alpha_numeric(r) {
                continue;
            }
            self.backup();
            let word = self.input[self.start..self.pos].to_string();
            if !self.at_terminator() {
                return self.errorf(format!("bad character {}", go_sharp_u(r)));
            }
            if let Some(item) = keyword(&word).filter(|&k| k > ItemType::Keyword) {
                if (item == ItemType::Break && !self.options.break_ok)
                    || (item == ItemType::Continue && !self.options.continue_ok)
                {
                    return self.emit(ItemType::Identifier);
                }
                return self.emit(item);
            }
            if word.starts_with('.') {
                return self.emit(ItemType::Field);
            }
            if word == "true" || word == "false" {
                return self.emit(ItemType::Bool);
            }
            return self.emit(ItemType::Identifier);
        }
    }

    fn lex_variable(&mut self) -> Option<State> {
        if self.at_terminator() {
            return self.emit(ItemType::Variable);
        }
        self.lex_field_or_variable(ItemType::Variable)
    }

    fn lex_field_or_variable(&mut self, typ: ItemType) -> Option<State> {
        if self.at_terminator() {
            if typ == ItemType::Variable {
                return self.emit(ItemType::Variable);
            }
            return self.emit(ItemType::Dot);
        }
        let mut r;
        loop {
            r = self.next();
            if !is_alpha_numeric(r) {
                self.backup();
                break;
            }
        }
        if !self.at_terminator() {
            return self.errorf(format!("bad character {}", go_sharp_u(r)));
        }
        self.emit(typ)
    }

    fn at_terminator(&mut self) -> bool {
        let r = self.peek();
        if is_space(r) {
            return true;
        }
        if r == EOF {
            return true;
        }
        if let Some(c) = char::from_u32(r as u32)
            && matches!(c, '.' | ',' | '|' | ':' | ')' | '(')
        {
            return true;
        }
        self.rest().starts_with(self.right_delim.as_str())
    }

    fn lex_char(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if r == '\\' as i32 {
                let r2 = self.next();
                if r2 != EOF && r2 != '\n' as i32 {
                    continue;
                }
                return self.errorf("unterminated character constant".to_string());
            }
            if r == EOF || r == '\n' as i32 {
                return self.errorf("unterminated character constant".to_string());
            }
            if r == '\'' as i32 {
                break;
            }
        }
        self.emit(ItemType::CharConstant)
    }

    fn lex_number(&mut self) -> Option<State> {
        if !self.scan_number() {
            let msg = format!(
                "bad number syntax: {}",
                crate::strconv::quote(&self.input[self.start..self.pos])
            );
            return self.errorf(msg);
        }
        let sign = self.peek();
        if sign == '+' as i32 || sign == '-' as i32 {
            if !self.scan_number() || self.input.as_bytes()[self.pos - 1] != b'i' {
                let msg = format!(
                    "bad number syntax: {}",
                    crate::strconv::quote(&self.input[self.start..self.pos])
                );
                return self.errorf(msg);
            }
            return self.emit(ItemType::Complex);
        }
        self.emit(ItemType::Number)
    }

    fn scan_number(&mut self) -> bool {
        self.accept("+-");
        let mut digits = "0123456789_";
        if self.accept("0") {
            if self.accept("xX") {
                digits = "0123456789abcdefABCDEF_";
            } else if self.accept("oO") {
                digits = "01234567_";
            } else if self.accept("bB") {
                digits = "01_";
            }
        }
        self.accept_run(digits);
        if self.accept(".") {
            self.accept_run(digits);
        }
        if digits.len() == 10 + 1 && self.accept("eE") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        if digits.len() == 16 + 6 + 1 && self.accept("pP") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        self.accept("i");
        if is_alpha_numeric(self.peek()) {
            self.next();
            return false;
        }
        true
    }

    fn lex_quote(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if r == '\\' as i32 {
                let r2 = self.next();
                if r2 != EOF && r2 != '\n' as i32 {
                    continue;
                }
                return self.errorf("unterminated quoted string".to_string());
            }
            if r == EOF || r == '\n' as i32 {
                return self.errorf("unterminated quoted string".to_string());
            }
            if r == '"' as i32 {
                break;
            }
        }
        self.emit(ItemType::String)
    }

    fn lex_raw_quote(&mut self) -> Option<State> {
        loop {
            let r = self.next();
            if r == EOF {
                return self.errorf("unterminated raw quoted string".to_string());
            }
            if r == '`' as i32 {
                break;
            }
        }
        self.emit(ItemType::RawString)
    }
}

/// `%#U` of a rune: `U+0041 'A'`, the quoted form only when the rune is printable.
fn go_sharp_u(r: i32) -> String {
    if r < 0 {
        // `%#U` of eof (-1) formats the uint64 of -1.
        return "U+FFFFFFFFFFFFFFFF".to_string();
    }
    let u = r as u32;
    let mut s = format!("U+{u:04X}");
    if crate::strconv::is_print(u)
        && let Some(c) = char::from_u32(u)
    {
        s.push_str(&format!(" '{c}'"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(src: &str) -> Vec<(ItemType, String)> {
        let mut l = Lexer::new(src, "", "");
        l.options.break_ok = true;
        l.options.continue_ok = true;
        let mut out = Vec::new();
        loop {
            let i = l.next_item();
            let done = matches!(i.typ, ItemType::Eof | ItemType::Error);
            out.push((i.typ, i.val));
            if done {
                return out;
            }
        }
    }

    #[test]
    fn trim_markers_and_numbers() {
        let got = items("a  {{- 3 -}}  b{{-3}}");
        assert_eq!(
            got,
            vec![
                (ItemType::Text, "a".into()),
                (ItemType::LeftDelim, "{{".into()),
                (ItemType::Number, "3".into()),
                (ItemType::RightDelim, "}}".into()),
                (ItemType::Text, "b".into()),
                (ItemType::LeftDelim, "{{".into()),
                (ItemType::Number, "-3".into()),
                (ItemType::RightDelim, "}}".into()),
                // lexText emits EOF through thisItem: its value is the empty remainder.
                (ItemType::Eof, "".into()),
            ]
        );
    }

    #[test]
    fn errors() {
        assert_eq!(
            items("{{.S").last().map(|i| i.1.clone()),
            Some("unclosed action".into())
        );
        assert_eq!(
            items("{{\u{1}}}").last().map(|i| i.1.clone()),
            Some("unrecognized character in action: U+0001".into())
        );
        assert_eq!(
            items("{{.S@}}").last().map(|i| i.1.clone()),
            Some("bad character U+0040 '@'".into())
        );
    }
}
