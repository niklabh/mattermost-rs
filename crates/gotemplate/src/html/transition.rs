//! Port of `html/template/transition.go`: the context transition functions for template text.
//!
//! Each function takes a context and the text that follows it, and returns the context after
//! the first token it recognises plus the number of bytes consumed.

use super::attr::attr_type;
use super::context::{
    Attr, Context, Element, JsCtx, State, UrlPart, is_comment, is_in_script_literal,
};
use super::css::{decode_css, ends_with_css_keyword};
use super::error::{ErrorCode, errorf};
use super::js::next_js_ctx;
use crate::strconv::quote;
use crate::value::ContentType;

/// `%q` of a byte slice.
pub(crate) fn q(b: &[u8]) -> String {
    quote(&String::from_utf8_lossy(b))
}

/// `%.32q` of a byte slice: at most 32 runes, quoted.
pub(crate) fn q32(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let t: String = s.chars().take(32).collect();
    quote(&t)
}

fn index_byte(s: &[u8], b: u8) -> Option<usize> {
    s.iter().position(|&c| c == b)
}

pub(crate) fn index_any(s: &[u8], chars: &[u8]) -> Option<usize> {
    s.iter().position(|c| chars.contains(c))
}

pub(crate) fn index_of(s: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    s.windows(needle.len()).position(|w| w == needle)
}

/// `transitionFunc[c.state](c, s)` (transition.go:16).
pub(crate) fn transition(c: Context, s: &[u8]) -> (Context, usize) {
    match c.state {
        State::Text => t_text(c, s),
        State::Tag => t_tag(c, s),
        State::AttrName => t_attr_name(c, s),
        State::AfterName => t_after_name(c, s),
        State::BeforeValue => t_before_value(c, s),
        State::HtmlCmt => t_html_cmt(c, s),
        State::Rcdata => t_special_tag_end(c, s),
        State::Attr => (c, s.len()),
        State::Url | State::Srcset => t_url(c, s),
        State::MetaContent => t_meta_content(c, s),
        State::MetaContentUrl => t_meta_content_url(c, s),
        State::Js => t_js(c, s),
        State::JsDqStr | State::JsSqStr | State::JsRegexp => t_js_delimited(c, s),
        State::JsTmplLit => t_js_tmpl(c, s),
        State::JsBlockCmt | State::CssBlockCmt => t_block_cmt(c, s),
        State::JsLineCmt | State::JsHtmlOpenCmt | State::JsHtmlCloseCmt | State::CssLineCmt => {
            t_line_cmt(c, s)
        }
        State::Css => t_css(c, s),
        State::CssDqStr | State::CssSqStr | State::CssDqUrl | State::CssSqUrl | State::CssUrl => {
            t_css_str(c, s)
        }
        State::Error | State::Dead => (c, s.len()),
    }
}

const COMMENT_START: &[u8] = b"<!--";
const COMMENT_END: &[u8] = b"-->";

/// `tText` (transition.go:57).
fn t_text(c: Context, s: &[u8]) -> (Context, usize) {
    let mut k = 0;
    loop {
        let Some(off) = index_byte(&s[k..], b'<') else {
            return (c, s.len());
        };
        let mut i = k + off;
        if i + 1 == s.len() {
            return (c, s.len());
        }
        if i + 4 <= s.len() && &s[i..i + 4] == COMMENT_START {
            return (Context::with_state(State::HtmlCmt), i + 4);
        }
        i += 1;
        let mut end = false;
        if s[i] == b'/' {
            if i + 1 == s.len() {
                return (c, s.len());
            }
            end = true;
            i += 1;
        }
        let (j, mut e) = eat_tag_name(s, i);
        if j != i {
            if end {
                e = Element::None;
            }
            return (
                Context {
                    state: State::Tag,
                    element: e,
                    ..Context::default()
                },
                j,
            );
        }
        k = j;
    }
}

/// `elementContentType` (transition.go:87).
fn element_content_type(e: Element) -> State {
    match e {
        Element::None => State::Text,
        Element::Script => State::Js,
        Element::Style => State::Css,
        Element::Textarea | Element::Title => State::Rcdata,
        Element::Meta => State::Text,
    }
}

/// `tTag` (transition.go:97).
fn t_tag(c: Context, s: &[u8]) -> (Context, usize) {
    let i = eat_white_space(s, 0);
    if i == s.len() {
        return (c, s.len());
    }
    if s[i] == b'>' {
        if c.element == Element::Meta {
            return (Context::with_state(State::Text), i + 1);
        }
        return (
            Context {
                state: element_content_type(c.element),
                element: c.element,
                ..Context::default()
            },
            i + 1,
        );
    }
    let j = match eat_attr_name(s, i) {
        Ok(j) => j,
        Err(e) => return (Context::error(e), s.len()),
    };
    if i == j {
        return (
            Context::error(errorf(
                ErrorCode::BadHtml,
                None,
                0,
                format!(
                    "expected space, attr name, or end of tag, but got {}",
                    q(&s[i..])
                ),
            )),
            s.len(),
        );
    }
    let attr_name = String::from_utf8_lossy(&s[i..j]).to_lowercase();
    let attr = if c.element == Element::Script && attr_name == "type" {
        Attr::ScriptType
    } else if c.element == Element::Meta && attr_name == "content" {
        Attr::MetaContent
    } else {
        match attr_type(&attr_name) {
            ContentType::Url => Attr::Url,
            ContentType::Css => Attr::Style,
            ContentType::Js => Attr::Script,
            ContentType::Srcset => Attr::Srcset,
            _ => Attr::None,
        }
    };
    let state = if j == s.len() {
        State::AttrName
    } else {
        State::AfterName
    };
    (
        Context {
            state,
            element: c.element,
            attr,
            ..Context::default()
        },
        j,
    )
}

/// `tAttrName` (transition.go:152).
fn t_attr_name(mut c: Context, s: &[u8]) -> (Context, usize) {
    match eat_attr_name(s, 0) {
        Err(e) => (Context::error(e), s.len()),
        Ok(i) => {
            if i != s.len() {
                c.state = State::AfterName;
            }
            (c, i)
        }
    }
}

/// `tAfterName` (transition.go:163).
fn t_after_name(mut c: Context, s: &[u8]) -> (Context, usize) {
    let i = eat_white_space(s, 0);
    if i == s.len() {
        return (c, s.len());
    } else if s[i] != b'=' {
        c.state = State::Tag;
        return (c, i);
    }
    c.state = State::BeforeValue;
    (c, i + 1)
}

/// `attrStartStates` (transition.go:177).
pub(crate) fn attr_start_state(a: Attr) -> State {
    match a {
        Attr::None | Attr::ScriptType => State::Attr,
        Attr::Script => State::Js,
        Attr::Style => State::Css,
        Attr::Url => State::Url,
        Attr::Srcset => State::Srcset,
        Attr::MetaContent => State::MetaContent,
    }
}

/// `tBeforeValue` (transition.go:188).
fn t_before_value(mut c: Context, s: &[u8]) -> (Context, usize) {
    use super::context::Delim;
    let mut i = eat_white_space(s, 0);
    if i == s.len() {
        return (c, s.len());
    }
    let mut delim = Delim::SpaceOrTagEnd;
    match s[i] {
        b'\'' => {
            delim = Delim::SingleQuote;
            i += 1;
        }
        b'"' => {
            delim = Delim::DoubleQuote;
            i += 1;
        }
        _ => {}
    }
    c.state = attr_start_state(c.attr);
    c.delim = delim;
    (c, i)
}

/// `tHTMLCmt` (transition.go:206).
fn t_html_cmt(c: Context, s: &[u8]) -> (Context, usize) {
    if let Some(i) = index_of(s, COMMENT_END) {
        return (Context::default(), i + 3);
    }
    (c, s.len())
}

fn special_tag_end_marker(e: Element) -> &'static [u8] {
    match e {
        Element::Script => b"script",
        Element::Style => b"style",
        Element::Textarea => b"textarea",
        Element::Title => b"title",
        Element::Meta | Element::None => b"",
    }
}

/// `tSpecialTagEnd` (transition.go:229).
pub(crate) fn t_special_tag_end(c: Context, s: &[u8]) -> (Context, usize) {
    if c.element != Element::None {
        if c.element == Element::Script && (is_in_script_literal(c.state) || is_comment(c.state)) {
            return (c, s.len());
        }
        if let Some(i) = index_tag_end(s, special_tag_end_marker(c.element)) {
            return (Context::default(), i);
        }
    }
    (c, s.len())
}

/// `indexTagEnd` (transition.go:245): the index of a case-insensitive `</tag` followed by a
/// separator.
fn index_tag_end(s: &[u8], tag: &[u8]) -> Option<usize> {
    let mut res = 0;
    let plen = 2;
    let mut s = s;
    while !s.is_empty() {
        let i = index_of(s, b"</")?;
        s = &s[i + plen..];
        if tag.len() <= s.len() && s[..tag.len()].eq_ignore_ascii_case(tag) {
            s = &s[tag.len()..];
            if !s.is_empty() && b"> \t\n\x0c/".contains(&s[0]) {
                return Some(res + i);
            }
            res += tag.len();
        }
        res += i + plen;
    }
    None
}

/// `tURL` (transition.go:275).
fn t_url(mut c: Context, s: &[u8]) -> (Context, usize) {
    if index_any(s, b"#?").is_some() {
        c.url_part = UrlPart::QueryOrFrag;
    } else if s.len() != eat_white_space(s, 0) && c.url_part == UrlPart::None {
        c.url_part = UrlPart::PreQuery;
    }
    (c, s.len())
}

/// `tJS` (transition.go:288).
fn t_js(mut c: Context, s: &[u8]) -> (Context, usize) {
    let Some(mut i) = index_any(s, b"\"`'/{}<-#") else {
        c.js_ctx = next_js_ctx(s, c.js_ctx);
        return (c, s.len());
    };
    c.js_ctx = next_js_ctx(&s[..i], c.js_ctx);
    match s[i] {
        b'"' => {
            c.state = State::JsDqStr;
            c.js_ctx = JsCtx::Regexp;
        }
        b'\'' => {
            c.state = State::JsSqStr;
            c.js_ctx = JsCtx::Regexp;
        }
        b'`' => {
            c.state = State::JsTmplLit;
            c.js_ctx = JsCtx::Regexp;
        }
        b'/' => {
            if i + 1 < s.len() && s[i + 1] == b'/' {
                c.state = State::JsLineCmt;
                i += 1;
            } else if i + 1 < s.len() && s[i + 1] == b'*' {
                c.state = State::JsBlockCmt;
                i += 1;
            } else if c.js_ctx == JsCtx::Regexp {
                c.state = State::JsRegexp;
            } else if c.js_ctx == JsCtx::DivOp {
                c.js_ctx = JsCtx::Regexp;
            } else {
                return (
                    Context::error(errorf(
                        ErrorCode::SlashAmbig,
                        None,
                        0,
                        format!("'/' could start a division or regexp: {}", q32(&s[i..])),
                    )),
                    s.len(),
                );
            }
        }
        b'<' => {
            if i + 3 < s.len() && &s[i..i + 4] == COMMENT_START {
                c.state = State::JsHtmlOpenCmt;
                i += 3;
            }
        }
        b'-' => {
            if i + 2 < s.len() && &s[i..i + 3] == COMMENT_END {
                c.state = State::JsHtmlCloseCmt;
                i += 2;
            }
        }
        b'#' => {
            if i + 1 < s.len() && s[i + 1] == b'!' {
                c.state = State::JsLineCmt;
                i += 1;
            }
        }
        b'{' => {
            // Brace depth is tracked only inside a template literal's `${`.
            match c.js_brace_depth.as_mut().and_then(|d| d.last_mut()) {
                Some(last) => *last += 1,
                None => return (c, i + 1),
            }
        }
        b'}' => {
            let Some(depth) = c.js_brace_depth.as_mut() else {
                return (c, i + 1);
            };
            if depth.is_empty() {
                return (c, i + 1);
            }
            if let Some(last) = depth.last_mut() {
                *last -= 1;
                if *last >= 0 {
                    return (c, i + 1);
                }
            }
            depth.pop();
            c.state = State::JsTmplLit;
        }
        _ => {}
    }
    (c, i + 1)
}

/// `tJSTmpl` (transition.go:372).
fn t_js_tmpl(mut c: Context, s: &[u8]) -> (Context, usize) {
    let mut k = 0;
    while let Some(off) = index_any(&s[k..], b"`\\$") {
        let mut i = k + off;
        match s[i] {
            b'\\' => {
                i += 1;
                if i == s.len() {
                    return (
                        Context::error(errorf(
                            ErrorCode::PartialEscape,
                            None,
                            0,
                            format!("unfinished escape sequence in JS string: {}", q(s)),
                        )),
                        s.len(),
                    );
                }
            }
            b'$' => {
                if s.len() >= i + 2 && s[i + 1] == b'{' {
                    c.js_brace_depth.get_or_insert_with(Vec::new).push(0);
                    c.state = State::Js;
                    return (c, i + 2);
                }
            }
            b'`' => {
                c.state = State::Js;
                return (c, i + 1);
            }
            _ => {}
        }
        k = i + 1;
    }
    (c, s.len())
}

/// `tJSDelimited` (transition.go:407).
fn t_js_delimited(mut c: Context, s: &[u8]) -> (Context, usize) {
    let specials: &[u8] = match c.state {
        State::JsSqStr => b"\\'",
        State::JsRegexp => b"\\/[]",
        _ => b"\\\"",
    };
    let mut k = 0;
    let mut in_charset = false;
    while let Some(off) = index_any(&s[k..], specials) {
        let mut i = k + off;
        match s[i] {
            b'\\' => {
                i += 1;
                if i == s.len() {
                    return (
                        Context::error(errorf(
                            ErrorCode::PartialEscape,
                            None,
                            0,
                            format!("unfinished escape sequence in JS string: {}", q(s)),
                        )),
                        s.len(),
                    );
                }
            }
            b'[' => in_charset = true,
            b']' => in_charset = false,
            b'/' => {
                if i > 0 && i + 7 <= s.len() && s[i - 1..i + 7].eq_ignore_ascii_case(b"</script") {
                    i += 1;
                } else if !in_charset {
                    c.state = State::Js;
                    c.js_ctx = JsCtx::DivOp;
                    return (c, i + 1);
                }
            }
            _ => {
                if !in_charset {
                    c.state = State::Js;
                    c.js_ctx = JsCtx::DivOp;
                    return (c, i + 1);
                }
            }
        }
        k = i + 1;
    }
    if in_charset {
        return (
            Context::error(errorf(
                ErrorCode::PartialCharset,
                None,
                0,
                format!("unfinished JS regexp charset: {}", q(s)),
            )),
            s.len(),
        );
    }
    (c, s.len())
}

/// `tBlockCmt` (transition.go:462).
fn t_block_cmt(mut c: Context, s: &[u8]) -> (Context, usize) {
    let Some(i) = index_of(s, b"*/") else {
        return (c, s.len());
    };
    c.state = if c.state == State::JsBlockCmt {
        State::Js
    } else {
        State::Css
    };
    (c, i + 2)
}

/// `tLineCmt` (transition.go:479).
fn t_line_cmt(mut c: Context, s: &[u8]) -> (Context, usize) {
    let (terminators, end_state): (&[&[u8]], State) = if c.state == State::CssLineCmt {
        (&[b"\n", b"\x0c", b"\r"], State::Css)
    } else {
        (
            &[b"\n", b"\r", "\u{2028}".as_bytes(), "\u{2029}".as_bytes()],
            State::Js,
        )
    };
    // `bytes.IndexAny` over runes: the first position where any terminator starts.
    let mut found = None;
    let mut i = 0;
    while i < s.len() {
        if terminators.iter().any(|t| s[i..].starts_with(t)) {
            found = Some(i);
            break;
        }
        i += 1;
    }
    let Some(i) = found else {
        return (c, s.len());
    };
    c.state = end_state;
    (c, i)
}

/// `tCSS` (transition.go:510).
fn t_css(mut c: Context, s: &[u8]) -> (Context, usize) {
    let mut k = 0;
    loop {
        let Some(off) = index_any(&s[k..], b"(\"'/") else {
            return (c, s.len());
        };
        let i = k + off;
        match s[i] {
            b'(' => {
                let p = trim_right(&s[..i], b"\t\n\x0c\r ");
                if ends_with_css_keyword(p, "url") {
                    let rest = trim_left(&s[i + 1..], b"\t\n\x0c\r ");
                    let mut j = s.len() - rest.len();
                    if j != s.len() && s[j] == b'"' {
                        c.state = State::CssDqUrl;
                        j += 1;
                    } else if j != s.len() && s[j] == b'\'' {
                        c.state = State::CssSqUrl;
                        j += 1;
                    } else {
                        c.state = State::CssUrl;
                    }
                    return (c, j);
                }
            }
            b'/' => {
                if i + 1 < s.len() {
                    match s[i + 1] {
                        b'/' => {
                            c.state = State::CssLineCmt;
                            return (c, i + 2);
                        }
                        b'*' => {
                            c.state = State::CssBlockCmt;
                            return (c, i + 2);
                        }
                        _ => {}
                    }
                }
            }
            b'"' => {
                c.state = State::CssDqStr;
                return (c, i + 1);
            }
            b'\'' => {
                c.state = State::CssSqStr;
                return (c, i + 1);
            }
            _ => {}
        }
        k = i + 1;
    }
}

fn trim_right<'s>(s: &'s [u8], set: &[u8]) -> &'s [u8] {
    let mut end = s.len();
    while end > 0 && set.contains(&s[end - 1]) {
        end -= 1;
    }
    &s[..end]
}

fn trim_left<'s>(s: &'s [u8], set: &[u8]) -> &'s [u8] {
    let mut start = 0;
    while start < s.len() && set.contains(&s[start]) {
        start += 1;
    }
    &s[start..]
}

/// `tCSSStr` (transition.go:582).
fn t_css_str(mut c: Context, s: &[u8]) -> (Context, usize) {
    let end_and_esc: &[u8] = match c.state {
        State::CssDqStr | State::CssDqUrl => b"\\\"",
        State::CssSqStr | State::CssSqUrl => b"\\'",
        _ => b"\\\t\n\x0c\r )",
    };
    let mut k = 0;
    loop {
        let Some(off) = index_any(&s[k..], end_and_esc) else {
            let (c2, nread) = t_url(c, &decode_css(&s[k..]));
            return (c2, k + nread);
        };
        let mut i = k + off;
        if s[i] == b'\\' {
            i += 1;
            if i == s.len() {
                return (
                    Context::error(errorf(
                        ErrorCode::PartialEscape,
                        None,
                        0,
                        format!("unfinished escape sequence in CSS string: {}", q(s)),
                    )),
                    s.len(),
                );
            }
        } else {
            c.state = State::Css;
            return (c, i + 1);
        }
        c = t_url(c, &decode_css(&s[..i + 1])).0;
        k = i + 1;
    }
}

/// `tMetaContent` (transition.go:635).
fn t_meta_content(mut c: Context, s: &[u8]) -> (Context, usize) {
    for i in 0..s.len() {
        if i + 3 < s.len() && s[i..i + 4].eq_ignore_ascii_case(b"url=") {
            c.state = State::MetaContentUrl;
            return (c, i + 4);
        }
    }
    (c, s.len())
}

/// `tMetaContentURL` (transition.go:646).
fn t_meta_content_url(mut c: Context, s: &[u8]) -> (Context, usize) {
    for (i, &b) in s.iter().enumerate() {
        if b == b';' {
            c.state = State::MetaContent;
            return (c, i + 1);
        }
    }
    (c, s.len())
}

/// `eatAttrName` (transition.go:660).
fn eat_attr_name(s: &[u8], i: usize) -> Result<usize, super::error::EscError> {
    for j in i..s.len() {
        match s[j] {
            b' ' | b'\t' | b'\n' | b'\x0c' | b'\r' | b'=' | b'>' => return Ok(j),
            b'\'' | b'"' | b'<' => {
                return Err(errorf(
                    ErrorCode::BadHtml,
                    None,
                    0,
                    format!("{} in attribute name: {}", q(&s[j..j + 1]), q32(s)),
                ));
            }
            _ => {}
        }
    }
    Ok(s.len())
}

fn element_by_name(name: &str) -> Element {
    match name {
        "script" => Element::Script,
        "style" => Element::Style,
        "textarea" => Element::Textarea,
        "title" => Element::Title,
        "meta" => Element::Meta,
        _ => Element::None,
    }
}

/// `eatTagName` (transition.go:697).
fn eat_tag_name(s: &[u8], i: usize) -> (usize, Element) {
    if i == s.len() || !s[i].is_ascii_alphabetic() {
        return (i, Element::None);
    }
    let mut j = i + 1;
    while j < s.len() {
        let x = s[j];
        if x.is_ascii_alphanumeric() {
            j += 1;
            continue;
        }
        if (x == b':' || x == b'-') && j + 1 < s.len() && s[j + 1].is_ascii_alphanumeric() {
            j += 2;
            continue;
        }
        break;
    }
    let name = String::from_utf8_lossy(&s[i..j]).to_lowercase();
    (j, element_by_name(&name))
}

/// `eatWhiteSpace` (transition.go:717).
pub(crate) fn eat_white_space(s: &[u8], i: usize) -> usize {
    for (j, &b) in s.iter().enumerate().skip(i) {
        match b {
            b' ' | b'\t' | b'\n' | b'\x0c' | b'\r' => {}
            _ => return j,
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_finds_tags_and_comments() {
        let (c, n) = transition(Context::default(), b"a <b>");
        assert_eq!((c.state, n), (State::Tag, 4));
        let (c, n) = transition(Context::default(), b"a<!--x");
        assert_eq!((c.state, n), (State::HtmlCmt, 5));
        let (c, n) = transition(Context::default(), b"I <3");
        assert_eq!((c.state, n), (State::Text, 4));
        let (c, _) = transition(Context::default(), b"<script");
        assert_eq!(c.element, Element::Script);
        let (c, _) = transition(Context::default(), b"</script");
        assert_eq!(c.element, Element::None);
    }

    #[test]
    fn tag_end_index() {
        assert_eq!(index_tag_end(b"x</script>", b"script"), Some(1));
        assert_eq!(index_tag_end(b"x</scripty></SCRIPT ", b"script"), Some(11));
        assert_eq!(index_tag_end(b"x</script", b"script"), None);
    }

    #[test]
    fn js_slash() {
        let c = Context::with_state(State::Js);
        let (c2, _) = transition(c.clone(), b"x = /");
        assert_eq!(c2.state, State::JsRegexp);
        let (c2, _) = transition(c, b"x / ");
        assert_eq!((c2.state, c2.js_ctx), (State::Js, JsCtx::Regexp));
    }
}
