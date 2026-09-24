//! Port of `html/template/html.go`: the HTML escapers, `stripTags` and the attribute-name filter.

use super::attr::attr_type;
use super::content::stringify;
use super::context::{Context, Delim, State, is_in_tag};
use super::escape::{FILTER_FAILSAFE, delim_ends};
use super::transition::{index_any, t_special_tag_end, transition};
use crate::value::{ContentType, Value};

/// Which replacement table `htmlReplacer` uses (html.go:66-151).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// `htmlReplacementTable`.
    Html,
    /// `htmlNormReplacementTable`: without `&`, so entities are not double-encoded.
    HtmlNorm,
    /// `htmlNospaceReplacementTable`.
    Nospace,
    /// `htmlNospaceNormReplacementTable`.
    NospaceNorm,
}

fn replacement(table: Table, r: char) -> Option<&'static str> {
    let nospace = matches!(table, Table::Nospace | Table::NospaceNorm);
    let norm = matches!(table, Table::HtmlNorm | Table::NospaceNorm);
    Some(match r {
        '\0' => {
            if nospace {
                "&#xfffd;"
            } else {
                "\u{fffd}"
            }
        }
        '"' => "&#34;",
        '&' if !norm => "&amp;",
        '\'' => "&#39;",
        '+' => "&#43;",
        '<' => "&lt;",
        '>' => "&gt;",
        '\t' if nospace => "&#9;",
        '\n' if nospace => "&#10;",
        '\x0b' if nospace => "&#11;",
        '\x0c' if nospace => "&#12;",
        '\r' if nospace => "&#13;",
        ' ' if nospace => "&#32;",
        '=' if nospace => "&#61;",
        '`' if nospace => "&#96;",
        _ => return None,
    })
}

/// The table's length (`len(replacementTable)`): runes at or beyond it are never replaced.
fn table_len(table: Table) -> u32 {
    match table {
        Table::Html | Table::HtmlNorm => u32::from(b'>') + 1,
        Table::Nospace | Table::NospaceNorm => u32::from(b'`') + 1,
    }
}

/// `htmlReplacer` (html.go:154): replaces runes per the table; when `bad_runes` is false, also
/// entity-encodes U+FDD0..U+FDEF and U+FFF0..U+FFFF, which IE rejects in unquoted attributes.
fn html_replacer(s: &str, table: Table, bad_runes: bool) -> String {
    let mut b = String::new();
    let mut written = 0;
    for (i, r) in s.char_indices() {
        let u = r as u32;
        if u < table_len(table) {
            if let Some(repl) = replacement(table, r) {
                b.push_str(&s[written..i]);
                b.push_str(repl);
                written = i + r.len_utf8();
            }
        } else if bad_runes {
            // No-op.
        } else if (0xfdd0..=0xfdef).contains(&u) || (0xfff0..=0xffff).contains(&u) {
            b.push_str(&s[written..i]);
            b.push_str(&format!("&#x{u:x};"));
            written = i + r.len_utf8();
        }
    }
    if written == 0 {
        return s.to_string();
    }
    b.push_str(&s[written..]);
    b
}

/// `htmlNospaceEscaper` (html.go:15): for unquoted attribute values.
pub(crate) fn html_nospace_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if s.is_empty() {
        return FILTER_FAILSAFE.to_string();
    }
    if t == ContentType::Html {
        return html_replacer(&strip_tags(&s), Table::NospaceNorm, false);
    }
    html_replacer(&s, Table::Nospace, false)
}

/// `attrEscaper` (html.go:28): for quoted attribute values.
pub(crate) fn attr_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::Html {
        return html_replacer(&strip_tags(&s), Table::HtmlNorm, true);
    }
    html_replacer(&s, Table::Html, true)
}

/// `rcdataEscaper` (html.go:37): for `<title>` and `<textarea>` bodies.
pub(crate) fn rcdata_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::Html {
        return html_replacer(&s, Table::HtmlNorm, true);
    }
    html_replacer(&s, Table::Html, true)
}

/// `htmlEscaper` (html.go:46): for HTML text; `template.HTML` passes through.
pub(crate) fn html_escaper(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::Html {
        return s;
    }
    html_replacer(&s, Table::Html, true)
}

/// `stripTags` (html.go:189): the text content of an HTML snippet.
pub(crate) fn strip_tags(html: &str) -> String {
    let s = html.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let mut c = Context::default();
    let mut i = 0;
    let mut all_text = true;
    while i != s.len() {
        if c.delim == Delim::None {
            let mut st = c.state;
            // Use RCDATA instead of parsing into JS or CSS styles.
            if c.element != super::context::Element::None && !is_in_tag(st) {
                st = State::Rcdata;
            }
            // Go calls the RCDATA transition function on the unchanged context.
            let (d, nread) = if st == State::Rcdata {
                t_special_tag_end(c.clone(), &s[i..])
            } else {
                transition(c.clone(), &s[i..])
            };
            let i1 = i + nread;
            if c.state == State::Text || c.state == State::Rcdata {
                let mut j = i1;
                if d.state != c.state {
                    for j1 in (i..j).rev() {
                        if s[j1] == b'<' {
                            j = j1;
                            break;
                        }
                    }
                }
                b.extend_from_slice(&s[i..j]);
            } else {
                all_text = false;
            }
            c = d;
            i = i1;
            continue;
        }
        let Some(off) = index_any(&s[i..], delim_ends(c.delim)) else {
            break;
        };
        let mut i1 = i + off;
        if c.delim != Delim::SpaceOrTagEnd {
            i1 += 1;
        }
        c = Context {
            state: State::Tag,
            element: c.element,
            ..Context::default()
        };
        i = i1;
    }
    if all_text {
        return html.to_string();
    } else if c.state == State::Text || c.state == State::Rcdata {
        b.extend_from_slice(&s[i..]);
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// `htmlNameFilter` (html.go:241): accepts valid parts of an attribute or tag name, or a
/// known-safe `template.HTMLAttr`.
pub(crate) fn html_name_filter(args: &[Option<&Value>]) -> String {
    let (s, t) = stringify(args);
    if t == ContentType::HtmlAttr {
        return s;
    }
    if s.is_empty() {
        return FILTER_FAILSAFE.to_string();
    }
    let s = s.to_lowercase();
    if attr_type(&s) != ContentType::Plain {
        return FILTER_FAILSAFE.to_string();
    }
    for r in s.chars() {
        if !(r.is_ascii_digit() || r.is_ascii_lowercase()) {
            return FILTER_FAILSAFE.to_string();
        }
    }
    s
}

/// `commentEscaper` (html.go:271): interpolations into comments are dropped.
pub(crate) fn comment_escaper(_args: &[Option<&Value>]) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip() {
        assert_eq!(
            strip_tags("<b>&iexcl;Hi!</b> <script>...</script>"),
            "&iexcl;Hi! "
        );
        assert_eq!(strip_tags(r#"<div title="1>2">x</div>"#), "x");
        assert_eq!(strip_tags("I <3 Ponies!"), "I <3 Ponies!");
    }

    #[test]
    fn escapers() {
        let v = Value::str("a b'<>&\"+=`");
        assert_eq!(
            html_escaper(&[Some(&v)]),
            "a b&#39;&lt;&gt;&amp;&#34;&#43;=`"
        );
        assert_eq!(
            html_nospace_escaper(&[Some(&v)]),
            "a&#32;b&#39;&lt;&gt;&amp;&#34;&#43;&#61;&#96;"
        );
        assert_eq!(html_nospace_escaper(&[Some(&Value::str(""))]), "ZgotmplZ");
        assert_eq!(
            html_nospace_escaper(&[Some(&Value::str("\u{fdd0}"))]),
            "&#xfdd0;"
        );
        assert_eq!(html_name_filter(&[Some(&Value::str("TITLE"))]), "title");
        assert_eq!(
            html_name_filter(&[Some(&Value::str("onclick"))]),
            "ZgotmplZ"
        );
        assert_eq!(html_name_filter(&[Some(&Value::str("a-b"))]), "ZgotmplZ");
    }
}
