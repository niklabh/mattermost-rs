//! Port of the parts of Go's `fmt` (print.go, format.go) that templates reach: `fmt.Sprint`,
//! `fmt.Sprintln` and `fmt.Sprintf` over the [`Value`] kinds, plus `%v`/`%s` of a single value
//! for error messages.
//!
//! An argument is `Option<&Value>`: `None` is a nil interface (Go's untyped `nil` argument).
//!
//! # Divergence
//!
//! Go prints a non-nil pointer that is not at the top level of an argument (or that points at a
//! scalar) as its **address**, which no port can reproduce; this one prints the fixed stand-in
//! `0xc000010000`. Floats under `%b`, `%x` and `%X` are not supported (they print as `%!x(...)`
//! style bad-verb errors would not; they print the `%g` form).

use crate::strconv::{can_backquote, format_float, is_print, quote, quote_rune, quote_to_ascii};
use crate::value::Value;

const LDIGITS: &[u8] = b"0123456789abcdefx";
const UDIGITS: &[u8] = b"0123456789ABCDEFX";

/// The address printed for a non-nil pointer Go would print as an address.
const FAKE_ADDRESS: u64 = 0xc000010000;

#[derive(Default, Clone, Copy)]
struct Flags {
    wid_present: bool,
    prec_present: bool,
    minus: bool,
    plus: bool,
    sharp: bool,
    space: bool,
    zero: bool,
    plus_v: bool,
    sharp_v: bool,
    wid: usize,
    prec: usize,
}

#[derive(Default)]
struct Pp {
    buf: String,
    f: Flags,
}

fn norm(arg: Option<&Value>) -> Option<&Value> {
    match arg {
        Some(Value::Nil) | None => None,
        Some(v) => Some(v),
    }
}

fn is_string_kind(v: &Value) -> bool {
    v.as_go_string().is_some()
}

/// `fmt.Sprint`.
pub(crate) fn sprint(args: &[Option<&Value>]) -> String {
    let mut p = Pp::default();
    let mut prev_string = false;
    for (i, &arg) in args.iter().enumerate() {
        let arg = norm(arg);
        let is_string = arg.is_some_and(is_string_kind);
        if i > 0 && !is_string && !prev_string {
            p.buf.push(' ');
        }
        p.print_arg(arg, 'v');
        prev_string = is_string;
    }
    p.buf
}

/// `fmt.Sprintln`.
pub(crate) fn sprintln(args: &[Option<&Value>]) -> String {
    let mut p = Pp::default();
    for (i, &arg) in args.iter().enumerate() {
        if i > 0 {
            p.buf.push(' ');
        }
        p.print_arg(norm(arg), 'v');
    }
    p.buf.push('\n');
    p.buf
}

/// One argument under one verb with no flags (`fmt.Sprintf("%v", x)` and friends).
pub(crate) fn format_one(arg: Option<&Value>, verb: char) -> String {
    let mut p = Pp::default();
    p.print_arg(norm(arg), verb);
    p.buf
}

impl Pp {
    fn write_padding(&mut self, n: isize) {
        if n <= 0 {
            return;
        }
        let pad = if self.f.zero && !self.f.minus {
            '0'
        } else {
            ' '
        };
        for _ in 0..n {
            self.buf.push(pad);
        }
    }

    fn pad(&mut self, s: &str) {
        if !self.f.wid_present || self.f.wid == 0 {
            self.buf.push_str(s);
            return;
        }
        let width = self.f.wid as isize - s.chars().count() as isize;
        if !self.f.minus {
            self.write_padding(width);
            self.buf.push_str(s);
        } else {
            self.buf.push_str(s);
            self.write_padding(width);
        }
    }

    fn fmt_boolean(&mut self, v: bool) {
        self.pad(if v { "true" } else { "false" });
    }

    fn fmt_unicode(&mut self, u: u64) {
        let mut prec = 4usize;
        if self.f.prec_present && self.f.prec > 4 {
            prec = self.f.prec;
        }
        let mut digits = format!("{u:X}");
        while digits.len() < prec {
            digits.insert(0, '0');
        }
        let mut s = format!("U+{digits}");
        if self.f.sharp
            && u <= 0x10ffff
            && is_print(u as u32)
            && let Some(c) = char::from_u32(u as u32)
        {
            s.push_str(&format!(" '{c}'"));
        }
        let old_zero = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old_zero;
    }

    /// `fmt.fmtInteger` (format.go:170).
    fn fmt_integer(&mut self, u: u64, base: u32, is_signed: bool, verb: char, digits: &[u8]) {
        let negative = is_signed && (u as i64) < 0;
        let mut u = if negative {
            (u as i64).unsigned_abs()
        } else {
            u
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut prec = 0usize;
        if self.f.prec_present {
            prec = self.f.prec;
            if prec == 0 && u == 0 {
                let old_zero = self.f.zero;
                self.f.zero = false;
                self.write_padding(self.f.wid as isize);
                self.f.zero = old_zero;
                return;
            }
        } else if self.f.zero && !self.f.minus && self.f.wid_present {
            prec = self.f.wid;
            if negative || self.f.plus || self.f.space {
                prec = prec.saturating_sub(1);
            }
        }
        // Digits, least significant first.
        let b = u64::from(base);
        while u >= b {
            buf.push(digits[(u % b) as usize]);
            u /= b;
        }
        buf.push(digits[u as usize]);
        while buf.len() < prec {
            buf.push(b'0');
        }
        if self.f.sharp {
            match base {
                2 => buf.extend_from_slice(b"b0"),
                8 => {
                    if buf.last() != Some(&b'0') {
                        buf.push(b'0');
                    }
                }
                16 => {
                    buf.push(digits[16]);
                    buf.push(b'0');
                }
                _ => {}
            }
        }
        if verb == 'O' {
            buf.extend_from_slice(b"o0");
        }
        if negative {
            buf.push(b'-');
        } else if self.f.plus {
            buf.push(b'+');
        } else if self.f.space {
            buf.push(b' ');
        }
        buf.reverse();
        let s = String::from_utf8_lossy(&buf).into_owned();
        let old_zero = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old_zero;
    }

    fn truncate_string<'s>(&self, s: &'s str) -> &'s str {
        if self.f.prec_present {
            let mut n = self.f.prec;
            for (i, _) in s.char_indices() {
                if n == 0 {
                    return &s[..i];
                }
                n -= 1;
            }
        }
        s
    }

    fn fmt_s(&mut self, s: &str) {
        let s = self.truncate_string(s).to_string();
        self.pad(&s);
    }

    /// `fmt.fmtSbx` for a string.
    fn fmt_sx(&mut self, s: &str, digits: &[u8]) {
        let b = s.as_bytes();
        let mut length = b.len();
        if self.f.prec_present && self.f.prec < length {
            length = self.f.prec;
        }
        let mut width = 2 * length;
        if width > 0 {
            if self.f.space {
                if self.f.sharp {
                    width *= 2;
                }
                width += length - 1;
            } else if self.f.sharp {
                width += 2;
            }
        } else {
            if self.f.wid_present {
                self.write_padding(self.f.wid as isize);
            }
            return;
        }
        if self.f.wid_present && self.f.wid > width && !self.f.minus {
            self.write_padding((self.f.wid - width) as isize);
        }
        let mut out = String::new();
        if self.f.sharp {
            out.push('0');
            out.push(digits[16] as char);
        }
        for (i, &c) in b[..length].iter().enumerate() {
            if self.f.space && i > 0 {
                out.push(' ');
                if self.f.sharp {
                    out.push('0');
                    out.push(digits[16] as char);
                }
            }
            out.push(digits[(c >> 4) as usize] as char);
            out.push(digits[(c & 0xf) as usize] as char);
        }
        self.buf.push_str(&out);
        if self.f.wid_present && self.f.wid > width && self.f.minus {
            self.write_padding((self.f.wid - width) as isize);
        }
    }

    fn fmt_q(&mut self, s: &str) {
        let s = self.truncate_string(s).to_string();
        if self.f.sharp && can_backquote(&s) {
            self.pad(&format!("`{s}`"));
            return;
        }
        let q = if self.f.plus {
            quote_to_ascii(&s)
        } else {
            quote(&s)
        };
        self.pad(&q);
    }

    fn fmt_c(&mut self, c: u64) {
        let ch = if c > 0x10ffff {
            '\u{fffd}'
        } else {
            char::from_u32(c as u32).unwrap_or('\u{fffd}')
        };
        self.pad(&ch.to_string());
    }

    fn fmt_qc(&mut self, c: u64) {
        let r = if c > 0x10ffff { 0xfffd } else { c as u32 };
        let q = quote_rune(r, self.f.plus);
        self.pad(&q);
    }

    /// `fmt.fmtFloat` (format.go:497).
    fn fmt_float_raw(&mut self, v: f64, verb: char, prec: i32) {
        let prec = if self.f.prec_present {
            self.f.prec as i32
        } else {
            prec
        };
        let fmtc = match verb {
            'v' => b'g',
            'F' => b'f',
            'b' | 'x' | 'X' => b'g',
            c => c as u8,
        };
        let s = format_float(v, fmtc, prec);
        let mut num: Vec<u8> = Vec::with_capacity(s.len() + 1);
        if s.starts_with('-') || s.starts_with('+') {
            num.extend_from_slice(s.as_bytes());
        } else {
            num.push(b'+');
            num.extend_from_slice(s.as_bytes());
        }
        if self.f.space && num[0] == b'+' && !self.f.plus {
            num[0] = b' ';
        }
        if num.get(1) == Some(&b'I') || num.get(1) == Some(&b'N') {
            let old_zero = self.f.zero;
            self.f.zero = false;
            if num[1] == b'N' && !self.f.space && !self.f.plus {
                num.remove(0);
            }
            let s = String::from_utf8_lossy(&num).into_owned();
            self.pad(&s);
            self.f.zero = old_zero;
            return;
        }
        if self.f.sharp && verb != 'b' {
            let mut digits: i32 = match verb {
                'v' | 'g' | 'G' | 'x' => {
                    if prec == -1 {
                        6
                    } else {
                        prec
                    }
                }
                _ => 0,
            };
            let mut tail: Vec<u8> = Vec::new();
            let mut has_decimal_point = false;
            let mut saw_nonzero_digit = false;
            let mut i = 1;
            while i < num.len() {
                match num[i] {
                    b'.' => has_decimal_point = true,
                    b'p' | b'P' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                        break;
                    }
                    b'e' | b'E' if verb != 'x' && verb != 'X' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                        break;
                    }
                    c => {
                        if c != b'0' {
                            saw_nonzero_digit = true;
                        }
                        if saw_nonzero_digit {
                            digits -= 1;
                        }
                    }
                }
                i += 1;
            }
            if !has_decimal_point {
                if num.len() == 2 && num[1] == b'0' {
                    digits -= 1;
                }
                num.push(b'.');
            }
            while digits > 0 {
                num.push(b'0');
                digits -= 1;
            }
            num.extend_from_slice(&tail);
        }
        if self.f.plus || num[0] != b'+' {
            if self.f.zero && !self.f.minus && self.f.wid_present && self.f.wid > num.len() {
                self.buf.push(num[0] as char);
                self.write_padding((self.f.wid - num.len()) as isize);
                self.buf.push_str(&String::from_utf8_lossy(&num[1..]));
                return;
            }
            let s = String::from_utf8_lossy(&num).into_owned();
            self.pad(&s);
            return;
        }
        let s = String::from_utf8_lossy(&num[1..]).into_owned();
        self.pad(&s);
    }

    fn fmt_bool(&mut self, v: bool, verb: char, subject: &Value) {
        match verb {
            't' | 'v' => self.fmt_boolean(v),
            _ => self.bad_verb(verb, Some(subject)),
        }
    }

    fn fmt_integer_verb(&mut self, v: u64, is_signed: bool, verb: char, subject: &Value) {
        match verb {
            'v' | 'd' => self.fmt_integer(v, 10, is_signed, verb, LDIGITS),
            'b' => self.fmt_integer(v, 2, is_signed, verb, LDIGITS),
            'o' | 'O' => self.fmt_integer(v, 8, is_signed, verb, LDIGITS),
            'x' => self.fmt_integer(v, 16, is_signed, verb, LDIGITS),
            'X' => self.fmt_integer(v, 16, is_signed, verb, UDIGITS),
            'c' => self.fmt_c(v),
            'q' => self.fmt_qc(v),
            'U' => self.fmt_unicode(v),
            _ => self.bad_verb(verb, Some(subject)),
        }
    }

    fn fmt_float(&mut self, v: f64, verb: char, subject: &Value) {
        match verb {
            'v' => self.fmt_float_raw(v, 'g', -1),
            'b' | 'g' | 'G' | 'x' | 'X' => self.fmt_float_raw(v, verb, -1),
            'f' | 'e' | 'E' => self.fmt_float_raw(v, verb, 6),
            'F' => self.fmt_float_raw(v, 'f', 6),
            _ => self.bad_verb(verb, Some(subject)),
        }
    }

    fn fmt_string(&mut self, v: &str, verb: char, subject: &Value) {
        match verb {
            'v' => {
                if self.f.sharp_v {
                    self.fmt_q(v);
                } else {
                    self.fmt_s(v);
                }
            }
            's' => self.fmt_s(v),
            'x' => self.fmt_sx(v, LDIGITS),
            'X' => self.fmt_sx(v, UDIGITS),
            'q' => self.fmt_q(v),
            _ => self.bad_verb(verb, Some(subject)),
        }
    }

    fn fmt_0x64(&mut self, v: u64, leading0x: bool) {
        let sharp = self.f.sharp;
        self.f.sharp = leading0x;
        self.fmt_integer(v, 16, false, 'v', LDIGITS);
        self.f.sharp = sharp;
    }

    fn fmt_pointer(&mut self, v: &Value, verb: char) {
        let u = if matches!(v, Value::NilPtr(_)) {
            0
        } else {
            FAKE_ADDRESS
        };
        match verb {
            'v' => {
                if u == 0 {
                    self.pad("<nil>");
                } else {
                    self.fmt_0x64(u, !self.f.sharp);
                }
            }
            'p' => self.fmt_0x64(u, !self.f.sharp),
            'b' | 'o' | 'd' | 'x' | 'X' => self.fmt_integer_verb(u, false, verb, v),
            _ => self.bad_verb(verb, Some(v)),
        }
    }

    /// `pp.badVerb` (print.go:382).
    fn bad_verb(&mut self, verb: char, subject: Option<&Value>) {
        self.buf.push_str("%!");
        self.buf.push(verb);
        self.buf.push('(');
        match subject {
            Some(v) => {
                self.buf.push_str(&v.go_type());
                self.buf.push('=');
                self.print_value(v, 'v', 0);
            }
            None => self.buf.push_str("<nil>"),
        }
        self.buf.push(')');
    }

    /// `pp.printArg` (print.go:682).
    fn print_arg(&mut self, arg: Option<&Value>, verb: char) {
        let Some(arg) = arg else {
            match verb {
                'T' | 'v' => self.pad("<nil>"),
                _ => self.bad_verb(verb, None),
            }
            return;
        };
        match verb {
            'T' => {
                let t = arg.go_type();
                self.fmt_s(&t);
                return;
            }
            'p' => {
                self.fmt_pointer(arg, 'p');
                return;
            }
            _ => {}
        }
        self.print_value(arg, verb, 0);
    }

    /// `pp.printValue` (print.go:767).
    fn print_value(&mut self, v: &Value, verb: char, depth: usize) {
        match v {
            Value::Nil => {
                // An interface slot holding nil: `printValue`'s Interface case prints `<nil>`
                // whatever the verb.
                if self.f.sharp_v {
                    self.buf.push_str("interface {}(nil)");
                } else {
                    self.buf.push_str("<nil>");
                }
            }
            Value::Bool(b) => self.fmt_bool(*b, verb, v),
            Value::Int(i) => self.fmt_integer_verb(*i as u64, true, verb, v),
            Value::Float(f) => self.fmt_float(*f, verb, v),
            Value::String(s)
            | Value::Html(s)
            | Value::Url(s)
            | Value::Css(s)
            | Value::Js(s)
            | Value::JsStr(s)
            | Value::HtmlAttr(s)
            | Value::Srcset(s) => self.fmt_string(s, verb, v),
            Value::Map(m) => {
                self.buf.push_str("map[");
                for (i, (k, val)) in m.iter().enumerate() {
                    if i > 0 {
                        self.buf.push(' ');
                    }
                    self.print_value(&Value::String(k.clone()), verb, depth + 1);
                    self.buf.push(':');
                    self.print_elem(val, verb, depth + 1);
                }
                self.buf.push(']');
            }
            Value::Struct(_, fields) => {
                self.buf.push('{');
                for (i, (name, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        self.buf.push(' ');
                    }
                    if self.f.plus_v || self.f.sharp_v {
                        self.buf.push_str(name);
                        self.buf.push(':');
                    }
                    self.print_value(val, verb, depth + 1);
                }
                self.buf.push('}');
            }
            Value::List(items) => {
                self.buf.push('[');
                for (i, val) in items.iter().enumerate() {
                    if i > 0 {
                        self.buf.push(' ');
                    }
                    self.print_elem(val, verb, depth + 1);
                }
                self.buf.push(']');
            }
            Value::Ptr(inner) => {
                if depth == 0
                    && matches!(**inner, Value::List(_) | Value::Struct(..) | Value::Map(_))
                {
                    self.buf.push('&');
                    self.print_value(inner, verb, depth + 1);
                    return;
                }
                self.fmt_pointer(v, verb);
            }
            Value::NilPtr(_) => self.fmt_pointer(v, verb),
        }
    }

    /// An element of an `interface {}`-typed container: `printValue` sees the interface first and
    /// recurses on its element one level deeper.
    fn print_elem(&mut self, v: &Value, verb: char, depth: usize) {
        self.print_value(v, verb, depth + 1);
    }
}

fn parse_num(s: &[u8], start: usize, end: usize) -> (usize, bool, usize) {
    if start >= end {
        return (0, false, end);
    }
    let mut num: usize = 0;
    let mut is_num = false;
    let mut i = start;
    while i < end && s[i].is_ascii_digit() {
        if num > 1_000_000 {
            return (0, false, end);
        }
        num = num * 10 + (s[i] - b'0') as usize;
        is_num = true;
        i += 1;
    }
    (num, is_num, i)
}

/// `intFromArg` (print.go:936).
fn int_from_arg(args: &[Option<&Value>], arg_num: usize) -> (isize, bool, usize) {
    let mut new_arg_num = arg_num;
    let mut num: isize = 0;
    let mut is_int = false;
    if arg_num < args.len() {
        if let Some(Value::Int(i)) = norm(args[arg_num]) {
            num = *i as isize;
            is_int = true;
        }
        new_arg_num = arg_num + 1;
        if !(-1_000_000..=1_000_000).contains(&num) {
            num = 0;
            is_int = false;
        }
    }
    (num, is_int, new_arg_num)
}

/// `parseArgNumber` (print.go:966).
fn parse_arg_number(format: &[u8]) -> (isize, usize, bool) {
    if format.len() < 3 {
        return (0, 1, false);
    }
    for i in 1..format.len() {
        if format[i] == b']' {
            let (width, ok, newi) = parse_num(format, 1, i);
            if !ok || newi != i {
                return (0, i + 1, false);
            }
            return (width as isize - 1, i + 1, true);
        }
    }
    (0, 1, false)
}

/// `fmt.Sprintf` (`pp.doPrintf`, print.go:1020).
pub(crate) fn sprintf(format: &str, args: &[Option<&Value>]) -> String {
    let mut p = Pp::default();
    let fb = format.as_bytes();
    let end = fb.len();
    let mut arg_num: usize = 0;
    let mut after_index;
    let mut reordered = false;
    let mut i = 0;
    'format_loop: while i < end {
        let mut good_arg_num = true;
        let lasti = i;
        while i < end && fb[i] != b'%' {
            i += 1;
        }
        if i > lasti {
            p.buf.push_str(&format[lasti..i]);
        }
        if i >= end {
            break;
        }
        i += 1;
        p.f = Flags::default();
        while i < end {
            let c = fb[i];
            match c {
                b'#' => p.f.sharp = true,
                b'0' => p.f.zero = true,
                b'+' => p.f.plus = true,
                b'-' => p.f.minus = true,
                b' ' => p.f.space = true,
                _ => {
                    if c.is_ascii_lowercase() && arg_num < args.len() {
                        if c == b'v' {
                            p.f.sharp_v = p.f.sharp;
                            p.f.sharp = false;
                            p.f.plus_v = p.f.plus;
                            p.f.plus = false;
                        }
                        p.print_arg(norm(args[arg_num]), c as char);
                        arg_num += 1;
                        i += 1;
                        continue 'format_loop;
                    }
                    break;
                }
            }
            i += 1;
        }

        // Argument index, width.
        let arg_number = |p_arg_num: usize, i: usize, reordered: &mut bool, good: &mut bool| {
            if i >= end || fb[i] != b'[' {
                return (p_arg_num, i, false);
            }
            *reordered = true;
            let (index, wid, ok) = parse_arg_number(&fb[i..]);
            if ok && 0 <= index && (index as usize) < args.len() {
                return (index as usize, i + wid, true);
            }
            *good = false;
            (p_arg_num, i + wid, ok)
        };
        (arg_num, i, after_index) = arg_number(arg_num, i, &mut reordered, &mut good_arg_num);

        if i < end && fb[i] == b'*' {
            i += 1;
            let (w, ok, n) = int_from_arg(args, arg_num);
            arg_num = n;
            p.f.wid_present = ok;
            if !ok {
                p.buf.push_str("%!(BADWIDTH)");
            }
            if w < 0 {
                p.f.wid = (-w) as usize;
                p.f.minus = true;
                p.f.zero = false;
            } else {
                p.f.wid = w as usize;
            }
            after_index = false;
        } else {
            let (w, ok, ni) = parse_num(fb, i, end);
            p.f.wid = w;
            p.f.wid_present = ok;
            i = ni;
            if after_index && p.f.wid_present {
                good_arg_num = false;
            }
        }

        if i + 1 < end && fb[i] == b'.' {
            i += 1;
            if after_index {
                good_arg_num = false;
            }
            (arg_num, i, after_index) = arg_number(arg_num, i, &mut reordered, &mut good_arg_num);
            if i < end && fb[i] == b'*' {
                i += 1;
                let (pr, ok, n) = int_from_arg(args, arg_num);
                arg_num = n;
                p.f.prec_present = ok;
                if pr < 0 {
                    p.f.prec = 0;
                    p.f.prec_present = false;
                } else {
                    p.f.prec = pr as usize;
                }
                if !p.f.prec_present {
                    p.buf.push_str("%!(BADPREC)");
                }
                after_index = false;
            } else {
                let (pr, ok, ni) = parse_num(fb, i, end);
                p.f.prec = pr;
                p.f.prec_present = ok;
                i = ni;
                if !p.f.prec_present {
                    p.f.prec = 0;
                    p.f.prec_present = true;
                }
            }
        }

        if !after_index {
            (arg_num, i, after_index) = arg_number(arg_num, i, &mut reordered, &mut good_arg_num);
        }
        let _ = after_index;

        if i >= end {
            p.buf.push_str("%!(NOVERB)");
            break;
        }

        let verb = format[i..].chars().next().unwrap_or('\u{fffd}');
        i += verb.len_utf8();

        if verb == '%' {
            p.buf.push('%');
        } else if !good_arg_num {
            p.buf.push_str("%!");
            p.buf.push(verb);
            p.buf.push_str("(BADINDEX)");
        } else if arg_num >= args.len() {
            p.buf.push_str("%!");
            p.buf.push(verb);
            p.buf.push_str("(MISSING)");
        } else {
            if verb == 'v' || verb == 'w' {
                p.f.sharp_v = p.f.sharp;
                p.f.sharp = false;
                p.f.plus_v = p.f.plus;
                p.f.plus = false;
            }
            p.print_arg(norm(args[arg_num]), verb);
            arg_num += 1;
        }
    }

    if !reordered && arg_num < args.len() {
        p.f = Flags::default();
        p.buf.push_str("%!(EXTRA ");
        for (i, &arg) in args[arg_num..].iter().enumerate() {
            if i > 0 {
                p.buf.push_str(", ");
            }
            match norm(arg) {
                None => p.buf.push_str("<nil>"),
                Some(v) => {
                    p.buf.push_str(&v.go_type());
                    p.buf.push('=');
                    p.print_arg(Some(v), 'v');
                }
            }
        }
        p.buf.push(')');
    }
    p.buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    #[test]
    fn sprint_spacing() {
        let a = Value::Int(1);
        let b = Value::Int(2);
        let c = s("a");
        assert_eq!(
            sprint(&[Some(&a), Some(&b), Some(&c), Some(&c), Some(&a)]),
            "1 2aa1"
        );
        assert_eq!(sprint(&[None, Some(&a)]), "<nil> 1");
        assert_eq!(sprintln(&[Some(&a), Some(&c)]), "1 a\n");
    }

    #[test]
    fn sprintf_verbs() {
        let n = Value::Int(42);
        assert_eq!(
            sprintf("%5d|%-5d|%05d|%+d|%x", &[Some(&n); 5]),
            "   42|42   |00042|+42|2a"
        );
        let st = s("hé");
        assert_eq!(sprintf("%q|%x|%.1s", &[Some(&st); 3]), "\"hé\"|68c3a9|h");
        assert_eq!(sprintf("%d", &[Some(&st)]), "%!d(string=hé)");
        assert_eq!(sprintf("%d %d", &[Some(&n)]), "42 %!d(MISSING)");
        assert_eq!(
            sprintf("%d", &[Some(&n), Some(&st)]),
            "42%!(EXTRA string=hé)"
        );
        assert_eq!(sprintf("%", &[]), "%!(NOVERB)");
        let f = Value::Float(4.56789);
        assert_eq!(
            sprintf("%.2f|%8.3f|%e", &[Some(&f); 3]),
            "4.57|   4.568|4.567890e+00"
        );
    }

    #[test]
    fn containers() {
        let l = Value::List(vec![Value::Int(1), Value::Nil, s("x")]);
        assert_eq!(format_one(Some(&l), 'v'), "[1 <nil> x]");
        let m = Value::map([("b", Value::Int(2)), ("a", s("x"))]);
        assert_eq!(format_one(Some(&m), 'v'), "map[a:x b:2]");
        let st = Value::Struct(
            "t.S".into(),
            vec![("A".into(), s("a")), ("B".into(), Value::Int(1))],
        );
        assert_eq!(format_one(Some(&st), 'v'), "{a 1}");
        assert_eq!(format_one(Some(&Value::Ptr(Box::new(st))), 'v'), "&{a 1}");
        assert_eq!(
            format_one(Some(&Value::NilPtr("*string".into())), 'v'),
            "<nil>"
        );
    }
}
