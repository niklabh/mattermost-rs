//! Port of `html/template/js.go`: the JavaScript lexing heuristic and the JS escapers.

use super::content::{indirect, stringify};
use super::context::JsCtx;
use super::json;
use crate::value::{ContentType, Value};

/// `jsWhitespace` (js.go:20).
const JS_WHITESPACE: &str = "\x0c\n\r\t\x0b\u{0020}\u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

/// `nextJSCtx` (js.go:36): whether a `/` after `s` starts a regexp or a division.
pub(crate) fn next_js_ctx(s: &[u8], preceding: JsCtx) -> JsCtx {
    let text = String::from_utf8_lossy(s);
    let trimmed = text.trim_end_matches(|c| JS_WHITESPACE.contains(c));
    let s = trimmed.as_bytes();
    if s.is_empty() {
        return preceding;
    }
    let n = s.len();
    let c = s[n - 1];
    match c {
        b'+' | b'-' => {
            let mut start = n - 1;
            while start > 0 && s[start - 1] == c {
                start -= 1;
            }
            if (n - start) & 1 == 1 {
                return JsCtx::Regexp;
            }
            JsCtx::DivOp
        }
        b'.' => {
            if n != 1 && s[n - 2].is_ascii_digit() {
                return JsCtx::DivOp;
            }
            JsCtx::Regexp
        }
        b',' | b'<' | b'>' | b'=' | b'*' | b'%' | b'&' | b'|' | b'^' | b'?' => JsCtx::Regexp,
        b'!' | b'~' => JsCtx::Regexp,
        b'(' | b'[' => JsCtx::Regexp,
        b':' | b';' | b'{' => JsCtx::Regexp,
        b'}' => JsCtx::Regexp,
        _ => {
            let mut j = n;
            while j > 0 && is_js_ident_part(u32::from(s[j - 1])) {
                j -= 1;
            }
            if is_regexp_preceder_keyword(&s[j..]) {
                return JsCtx::Regexp;
            }
            JsCtx::DivOp
        }
    }
}

fn is_regexp_preceder_keyword(w: &[u8]) -> bool {
    matches!(
        w,
        b"break"
            | b"case"
            | b"continue"
            | b"delete"
            | b"do"
            | b"else"
            | b"finally"
            | b"in"
            | b"instanceof"
            | b"return"
            | b"throw"
            | b"try"
            | b"typeof"
            | b"void"
    )
}

/// `isJSIdentPart` (js.go:443).
pub(crate) fn is_js_ident_part(r: u32) -> bool {
    r == u32::from(b'$')
        || (u32::from(b'0')..=u32::from(b'9')).contains(&r)
        || (u32::from(b'A')..=u32::from(b'Z')).contains(&r)
        || r == u32::from(b'_')
        || (u32::from(b'a')..=u32::from(b'z')).contains(&r)
}

/// `isJSType` (js.go:462).
pub(crate) fn is_js_type(mime_type: &str) -> bool {
    let m = mime_type.split(';').next().unwrap_or("").to_lowercase();
    let m = m.trim_matches(char::is_whitespace);
    matches!(
        m,
        "application/ecmascript"
            | "application/javascript"
            | "application/json"
            | "application/ld+json"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "module"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// `jsValEscaper` (js.go:147): the arguments as a JS expression.
pub(crate) fn js_val_escaper(args: &[Option<&Value>]) -> String {
    let marshaled: Result<String, String> = if args.len() == 1 {
        let a = args[0].map(indirect);
        match a {
            Some(Value::Js(s)) => return s.clone(),
            Some(Value::JsStr(s)) => return format!("\"{s}\""),
            other => json::marshal(other),
        }
    } else {
        let derefd: Vec<Option<&Value>> = args
            .iter()
            .map(|a| match a {
                None | Some(Value::Nil) => None,
                Some(v) => Some(indirect(v)),
            })
            .collect();
        let s = crate::fmt::sprint(&derefd);
        json::marshal(Some(&Value::String(s)))
    };
    let b = match marshaled {
        Ok(b) => b,
        Err(e) => {
            let mut err = replace_script_tags(&e);
            err = err.replace("*/", "* /");
            err = err.replace("<!--", "\\x3C!--");
            return format!(" /* {err} */null ");
        }
    };
    if b.is_empty() {
        return " null ".to_string();
    }
    let first = b.chars().next().map_or(0, |c| c as u32);
    let last = b.chars().next_back().map_or(0, |c| c as u32);
    let pad = is_js_ident_part(first) || is_js_ident_part(last);
    let mut buf = String::new();
    if pad {
        buf.push(' ');
    }
    let mut written = 0;
    for (i, ch) in b.char_indices() {
        let repl = match ch {
            '\u{2028}' => "\\u2028",
            '\u{2029}' => "\\u2029",
            _ => continue,
        };
        buf.push_str(&b[written..i]);
        buf.push_str(repl);
        written = i + ch.len_utf8();
    }
    if !buf.is_empty() {
        buf.push_str(&b[written..]);
        if pad {
            buf.push(' ');
        }
        return buf;
    }
    b
}

/// `scriptTagRe.ReplaceAll(err, "\x3C${1}script")`: `(?i)<(/?)script`.
fn replace_script_tags(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut written = 0;
    while i < b.len() {
        if b[i] == b'<' {
            let slash = b.get(i + 1) == Some(&b'/');
            let start = i + 1 + usize::from(slash);
            if b.len() >= start + 6 && b[start..start + 6].eq_ignore_ascii_case(b"script") {
                out.push_str(&s[written..i]);
                out.push_str("\\x3C");
                if slash {
                    out.push('/');
                }
                out.push_str("script");
                i = start + 6;
                written = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[written..]);
    out
}

/// `jsStrEscaper` (js.go:231).
pub(crate) fn js_str_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::JsStr {
        return replace(&s, js_str_norm_table);
    }
    replace(&s, js_str_table)
}

/// `jsTmplLitEscaper` (js.go:239).
pub(crate) fn js_tmpl_lit_escaper(args: &[Option<&Value>]) -> String {
    let (s, _) = stringify(args);
    replace(&s, js_bq_str_table)
}

/// `jsRegexpEscaper` (js.go:248).
pub(crate) fn js_regexp_escaper(args: &[Option<&Value>]) -> String {
    let (s, _) = stringify(args);
    let s = replace(&s, js_regexp_table);
    if s.is_empty() {
        return "(?:)".to_string();
    }
    s
}

/// `replace` (js.go:264).
fn replace(s: &str, table: fn(char) -> Option<&'static str>) -> String {
    let mut b = String::new();
    let mut written = 0;
    for (i, r) in s.char_indices() {
        let repl: String = if (r as u32) < 0x20 {
            low_unicode(r)
        } else if let Some(t) = table(r) {
            t.to_string()
        } else if r == '\u{2028}' {
            "\\u2028".to_string()
        } else if r == '\u{2029}' {
            "\\u2029".to_string()
        } else {
            continue;
        };
        b.push_str(&s[written..i]);
        b.push_str(&repl);
        written = i + r.len_utf8();
    }
    if written == 0 {
        return s.to_string();
    }
    b.push_str(&s[written..]);
    b
}

/// `lowUnicodeReplacementTable` (js.go:294).
fn low_unicode(r: char) -> String {
    match r {
        '\t' => "\\t".to_string(),
        '\n' => "\\n".to_string(),
        '\x0c' => "\\f".to_string(),
        '\r' => "\\r".to_string(),
        _ => format!("\\u{:04x}", r as u32),
    }
}

fn js_str_table(r: char) -> Option<&'static str> {
    Some(match r {
        '"' => "\\u0022",
        '`' => "\\u0060",
        '&' => "\\u0026",
        '\'' => "\\u0027",
        '+' => "\\u002b",
        '/' => "\\/",
        '<' => "\\u003c",
        '>' => "\\u003e",
        '\\' => "\\\\",
        _ => return None,
    })
}

fn js_bq_str_table(r: char) -> Option<&'static str> {
    Some(match r {
        '$' => "\\u0024",
        '{' => "\\u007b",
        '}' => "\\u007d",
        other => return js_str_table(other),
    })
}

fn js_str_norm_table(r: char) -> Option<&'static str> {
    match r {
        '\\' => None,
        other => js_str_table(other),
    }
}

fn js_regexp_table(r: char) -> Option<&'static str> {
    Some(match r {
        '"' => "\\u0022",
        '$' => "\\$",
        '&' => "\\u0026",
        '\'' => "\\u0027",
        '(' => "\\(",
        ')' => "\\)",
        '*' => "\\*",
        '+' => "\\u002b",
        '-' => "\\-",
        '.' => "\\.",
        '/' => "\\/",
        '<' => "\\u003c",
        '>' => "\\u003e",
        '?' => "\\?",
        '[' => "\\[",
        '\\' => "\\\\",
        ']' => "\\]",
        '^' => "\\^",
        '{' => "\\{",
        '|' => "\\|",
        '}' => "\\}",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_ctx_heuristic() {
        assert_eq!(next_js_ctx(b"x = ", JsCtx::DivOp), JsCtx::Regexp);
        assert_eq!(next_js_ctx(b"x", JsCtx::Regexp), JsCtx::DivOp);
        assert_eq!(next_js_ctx(b"return", JsCtx::DivOp), JsCtx::Regexp);
        assert_eq!(next_js_ctx(b"x++", JsCtx::Regexp), JsCtx::DivOp);
        assert_eq!(next_js_ctx(b"x+", JsCtx::DivOp), JsCtx::Regexp);
        assert_eq!(next_js_ctx(b"42.", JsCtx::Regexp), JsCtx::DivOp);
        assert_eq!(
            next_js_ctx("  \u{a0}".as_bytes(), JsCtx::DivOp),
            JsCtx::DivOp
        );
    }

    #[test]
    fn escapers() {
        let v = Value::str("a\"b</script>\u{2028}\n");
        assert_eq!(
            js_str_escaper(&[Some(&v)]),
            "a\\u0022b\\u003c\\/script\\u003e\\u2028\\n"
        );
        assert_eq!(js_regexp_escaper(&[Some(&Value::str(""))]), "(?:)");
        assert_eq!(js_val_escaper(&[None]), " null ");
        assert_eq!(js_val_escaper(&[Some(&Value::Int(12))]), " 12 ");
        assert_eq!(js_val_escaper(&[Some(&Value::str("x<"))]), "\"x\\u003c\"");
        assert_eq!(
            js_val_escaper(&[Some(&Value::Float(f64::NAN))]),
            " /* json: unsupported value: NaN */null "
        );
        assert!(is_js_type("Text/JavaScript; charset=utf-8"));
        assert!(!is_js_type("text/template"));
    }
}
