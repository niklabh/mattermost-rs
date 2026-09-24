//! Port of `text/template/funcs.go`: the builtin functions, plus the dispatch table for the
//! `_html_template_*` escapers `html/template` registers.
//!
//! Each Go builtin's parameter types decide how `evalArg` and `validateType` treat its arguments
//! (a `reflect.Value` parameter receives an invalid value as invalid; an `any` parameter receives
//! it as a nil interface; `printf`'s `string` format rejects everything but a `string`), so each
//! function here carries its Go signature as a [`Sig`].

use std::borrow::Cow;

use crate::exec::fmt_rv;
use crate::rv::{Kind, Rv, project, truth};
use crate::value::Value;

/// A Go parameter type, as far as argument evaluation can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Param {
    /// `any`.
    Any,
    /// `reflect.Value`.
    RValue,
    /// `string`.
    Str,
}

/// A function's parameters: the fixed ones and the element type of a trailing `...`.
pub(crate) struct Sig {
    pub fixed: &'static [Param],
    pub variadic: Option<Param>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    And,
    Call,
    Html,
    Index,
    Slice,
    Js,
    Len,
    Not,
    Or,
    Print,
    Printf,
    Println,
    Urlquery,
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
    Ne,
}

/// The functions `html/template` adds (escape.go:65).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HtmlFn {
    AttrEscaper,
    CommentEscaper,
    CssEscaper,
    CssValueFilter,
    HtmlNameFilter,
    HtmlEscaper,
    JsRegexpEscaper,
    JsStrEscaper,
    JsTmplLitEscaper,
    JsValEscaper,
    NospaceEscaper,
    RcdataEscaper,
    SrcsetEscaper,
    UrlEscaper,
    UrlFilter,
    UrlNormalizer,
    EvalArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Func {
    Builtin(Builtin),
    Html(HtmlFn),
}

const BUILTINS: &[(&str, Builtin)] = &[
    ("and", Builtin::And),
    ("call", Builtin::Call),
    ("html", Builtin::Html),
    ("index", Builtin::Index),
    ("slice", Builtin::Slice),
    ("js", Builtin::Js),
    ("len", Builtin::Len),
    ("not", Builtin::Not),
    ("or", Builtin::Or),
    ("print", Builtin::Print),
    ("printf", Builtin::Printf),
    ("println", Builtin::Println),
    ("urlquery", Builtin::Urlquery),
    ("eq", Builtin::Eq),
    ("ge", Builtin::Ge),
    ("gt", Builtin::Gt),
    ("le", Builtin::Le),
    ("lt", Builtin::Lt),
    ("ne", Builtin::Ne),
];

pub(crate) const HTML_FUNCS: &[(&str, HtmlFn)] = &[
    ("_html_template_attrescaper", HtmlFn::AttrEscaper),
    ("_html_template_commentescaper", HtmlFn::CommentEscaper),
    ("_html_template_cssescaper", HtmlFn::CssEscaper),
    ("_html_template_cssvaluefilter", HtmlFn::CssValueFilter),
    ("_html_template_htmlnamefilter", HtmlFn::HtmlNameFilter),
    ("_html_template_htmlescaper", HtmlFn::HtmlEscaper),
    ("_html_template_jsregexpescaper", HtmlFn::JsRegexpEscaper),
    ("_html_template_jsstrescaper", HtmlFn::JsStrEscaper),
    ("_html_template_jstmpllitescaper", HtmlFn::JsTmplLitEscaper),
    ("_html_template_jsvalescaper", HtmlFn::JsValEscaper),
    ("_html_template_nospaceescaper", HtmlFn::NospaceEscaper),
    ("_html_template_rcdataescaper", HtmlFn::RcdataEscaper),
    ("_html_template_srcsetescaper", HtmlFn::SrcsetEscaper),
    ("_html_template_urlescaper", HtmlFn::UrlEscaper),
    ("_html_template_urlfilter", HtmlFn::UrlFilter),
    ("_html_template_urlnormalizer", HtmlFn::UrlNormalizer),
    ("_eval_args_", HtmlFn::EvalArgs),
];

/// Whether `name` is a builtin — the parser's `hasFunction` for a set with no `Funcs`.
pub(crate) fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|(n, _)| *n == name)
}

/// `findFunction` (funcs.go:138): the template's own functions (the html escapers, once
/// registered) first, then the builtins.
pub(crate) fn find(name: &str, html_funcs: bool) -> Option<Func> {
    if html_funcs && let Some((_, f)) = HTML_FUNCS.iter().find(|(n, _)| *n == name) {
        return Some(Func::Html(*f));
    }
    BUILTINS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| Func::Builtin(*b))
}

impl Func {
    pub(crate) fn signature(self) -> Sig {
        use Param::*;
        match self {
            Func::Html(_) => Sig {
                fixed: &[],
                variadic: Some(Any),
            },
            Func::Builtin(b) => match b {
                Builtin::And
                | Builtin::Or
                | Builtin::Call
                | Builtin::Index
                | Builtin::Slice
                | Builtin::Eq => Sig {
                    fixed: &[RValue],
                    variadic: Some(RValue),
                },
                Builtin::Html
                | Builtin::Js
                | Builtin::Urlquery
                | Builtin::Print
                | Builtin::Println => Sig {
                    fixed: &[],
                    variadic: Some(Any),
                },
                Builtin::Printf => Sig {
                    fixed: &[Str],
                    variadic: Some(Any),
                },
                Builtin::Len | Builtin::Not => Sig {
                    fixed: &[RValue],
                    variadic: None,
                },
                Builtin::Ge | Builtin::Gt | Builtin::Le | Builtin::Lt | Builtin::Ne => Sig {
                    fixed: &[RValue, RValue],
                    variadic: None,
                },
            },
        }
    }
}

/// Calls a function with evaluated arguments; `Err` is the error Go's function returns (or the
/// message of the panic `safeCall` recovers).
pub(crate) fn call<'a>(f: Func, argv: Vec<Rv<'a>>, callee: Option<&str>) -> Result<Rv<'a>, String> {
    let anys = |argv: &[Rv<'a>]| -> Vec<Option<Value>> {
        argv.iter().map(|v| v.as_any().cloned()).collect()
    };
    match f {
        Func::Html(h) => {
            let owned = anys(&argv);
            let args: Vec<Option<&Value>> = owned.iter().map(Option::as_ref).collect();
            Ok(Rv::owned(Value::String(crate::html::call_escaper(
                h, &args,
            ))))
        }
        Func::Builtin(b) => {
            let mut argv = argv;
            match b {
                Builtin::And | Builtin::Or => Err("unreachable".to_string()),
                Builtin::Call => {
                    let fun = argv.remove(0).indirect_interface();
                    if !fun.is_valid() {
                        return Err("call of nil".to_string());
                    }
                    Err(format!(
                        "non-function {} of type {}",
                        callee.unwrap_or_default(),
                        fun.type_name()
                    ))
                }
                Builtin::Html => {
                    let s = eval_args(&argv);
                    Ok(Rv::owned(Value::String(html_escape_string(&s))))
                }
                Builtin::Js => {
                    let s = eval_args(&argv);
                    Ok(Rv::owned(Value::String(js_escape_string(&s))))
                }
                Builtin::Urlquery => {
                    let s = eval_args(&argv);
                    Ok(Rv::owned(Value::String(query_escape(&s))))
                }
                Builtin::Print | Builtin::Println | Builtin::Printf => {
                    let owned = anys(&argv);
                    let args: Vec<Option<&Value>> = owned.iter().map(Option::as_ref).collect();
                    let s = match b {
                        Builtin::Print => crate::fmt::sprint(&args),
                        Builtin::Println => crate::fmt::sprintln(&args),
                        _ => {
                            let format = match args.first() {
                                Some(Some(Value::String(s))) => s.clone(),
                                _ => String::new(),
                            };
                            crate::fmt::sprintf(&format, &args[1..])
                        }
                    };
                    Ok(Rv::owned(Value::String(s)))
                }
                Builtin::Index => {
                    let item = argv.remove(0);
                    index(item, argv)
                }
                Builtin::Slice => {
                    let item = argv.remove(0);
                    slice(item, argv)
                }
                Builtin::Len => length(argv.remove(0)).map(|n| Rv::owned(Value::Int(n as i64))),
                Builtin::Not => Ok(Rv::owned(Value::Bool(!truth(&argv[0])))),
                Builtin::Eq => {
                    let a1 = argv.remove(0);
                    eq(a1, &argv).map(|b| Rv::owned(Value::Bool(b)))
                }
                Builtin::Ne => eq(argv[0].clone(), &argv[1..2]).map(|b| Rv::owned(Value::Bool(!b))),
                Builtin::Lt => lt(&argv[0], &argv[1]).map(|b| Rv::owned(Value::Bool(b))),
                Builtin::Le => le(&argv[0], &argv[1]).map(|b| Rv::owned(Value::Bool(b))),
                Builtin::Gt => le(&argv[0], &argv[1]).map(|b| Rv::owned(Value::Bool(!b))),
                Builtin::Ge => lt(&argv[0], &argv[1]).map(|b| Rv::owned(Value::Bool(!b))),
            }
        }
    }
}

/// `indexArg` (funcs.go:174).
fn index_arg(index: &Rv<'_>, cap: usize) -> Result<usize, String> {
    let x = match index.kind() {
        Kind::Int => match index.value() {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        Kind::Invalid => return Err("cannot index slice/array with nil".to_string()),
        _ => {
            return Err(format!(
                "cannot index slice/array with type {}",
                index.type_name()
            ));
        }
    };
    if x < 0 || x as usize > cap {
        return Err(format!("index out of range: {x}"));
    }
    Ok(x as usize)
}

/// `index` (funcs.go:198).
fn index<'a>(item: Rv<'a>, indexes: Vec<Rv<'a>>) -> Result<Rv<'a>, String> {
    let mut item = item.indirect_interface();
    if !item.is_valid() {
        return Err("index of untyped nil".to_string());
    }
    for index in indexes {
        let index = index.indirect_interface();
        let (it, is_nil) = item.indirect();
        if is_nil {
            return Err("index of nil pointer".to_string());
        }
        item = it;
        let Rv::Val(c, _) = &item else {
            return Err("unreachable".to_string());
        };
        match &**c {
            Value::List(l) => {
                let x = index_arg(&index, l.len())?;
                if x == l.len() {
                    return Err("reflect: slice index out of range".to_string());
                }
                let elem = project(c, |v| match v {
                    Value::List(l) => &l[x],
                    other => other,
                });
                item = Rv::from_cow(elem, true);
            }
            v if v.as_go_string().is_some() => {
                let s = v.as_go_string().map(|(s, _)| s).unwrap_or("");
                let x = index_arg(&index, s.len())?;
                if x == s.len() {
                    return Err("reflect: string index out of range".to_string());
                }
                item = Rv::owned(Value::Int(i64::from(s.as_bytes()[x])));
            }
            Value::Map(m) => {
                let key = match index.value() {
                    None => return Err("value is nil; should be of type string".to_string()),
                    Some(Value::String(k)) => k.clone(),
                    Some(_) => {
                        return Err(format!(
                            "value has type {}; should be string",
                            index.type_name()
                        ));
                    }
                };
                if m.contains_key(&key) {
                    let elem = project(c, |v| match v {
                        Value::Map(m) => m.get(key.as_str()).unwrap_or(v),
                        other => other,
                    });
                    item = Rv::from_cow(elem, true);
                } else {
                    item = Rv::nil_iface();
                }
            }
            _ => return Err(format!("can't index item of type {}", item.type_name())),
        }
    }
    Ok(item)
}

/// `slice` (funcs.go:243).
fn slice<'a>(item: Rv<'a>, indexes: Vec<Rv<'a>>) -> Result<Rv<'a>, String> {
    let item = item.indirect_interface();
    if !item.is_valid() {
        return Err("slice of untyped nil".to_string());
    }
    let (item, is_nil) = item.indirect();
    if is_nil {
        return Err("slice of nil pointer".to_string());
    }
    if indexes.len() > 3 {
        return Err(format!("too many slice indexes: {}", indexes.len()));
    }
    let Some(v) = item.value() else {
        return Err("unreachable".to_string());
    };
    let (len, is_string) = match v {
        Value::List(l) => (l.len(), false),
        other => match other.as_go_string() {
            Some((s, _)) => {
                if indexes.len() == 3 {
                    return Err("cannot 3-index slice a string".to_string());
                }
                (s.len(), true)
            }
            None => return Err(format!("can't slice item of type {}", item.type_name())),
        },
    };
    let mut idx = [0usize, len, 0];
    for (i, index) in indexes.iter().enumerate() {
        idx[i] = index_arg(index, len)?;
    }
    if idx[0] > idx[1] {
        return Err(format!("invalid slice index: {} > {}", idx[0], idx[1]));
    }
    if indexes.len() == 3 && idx[1] > idx[2] {
        return Err(format!("invalid slice index: {} > {}", idx[1], idx[2]));
    }
    let out = if is_string {
        let (s, _) = v
            .as_go_string()
            .unwrap_or(("", crate::value::ContentType::Plain));
        let sub = String::from_utf8_lossy(&s.as_bytes()[idx[0]..idx[1]]).into_owned();
        match v {
            Value::Html(_) => Value::Html(sub),
            Value::Url(_) => Value::Url(sub),
            Value::Css(_) => Value::Css(sub),
            Value::Js(_) => Value::Js(sub),
            Value::JsStr(_) => Value::JsStr(sub),
            Value::HtmlAttr(_) => Value::HtmlAttr(sub),
            Value::Srcset(_) => Value::Srcset(sub),
            _ => Value::String(sub),
        }
    } else {
        match v {
            Value::List(l) => Value::List(l[idx[0]..idx[1]].to_vec()),
            other => other.clone(),
        }
    };
    Ok(Rv::owned(out))
}

/// `length` (funcs.go:297).
fn length(item: Rv<'_>) -> Result<usize, String> {
    let (item, is_nil) = item.indirect();
    if is_nil {
        return Err("len of nil pointer".to_string());
    }
    match item.value() {
        None => Err("reflect: call of reflect.Value.Type on zero Value".to_string()),
        Some(Value::List(l)) => Ok(l.len()),
        Some(Value::Map(m)) => Ok(m.len()),
        Some(v) => match v.as_go_string() {
            Some((s, _)) => Ok(s.len()),
            None => Err(format!("len of type {}", item.type_name())),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BasicKind {
    Invalid,
    Bool,
    Int,
    Float,
    String,
}

fn basic_kind(v: &Rv<'_>) -> Result<BasicKind, String> {
    match v.kind() {
        Kind::Bool => Ok(BasicKind::Bool),
        Kind::Int => Ok(BasicKind::Int),
        Kind::Float => Ok(BasicKind::Float),
        Kind::String => Ok(BasicKind::String),
        _ => Err("invalid type for comparison".to_string()),
    }
}

/// `isNil` (funcs.go:436).
fn is_nil_value(v: &Rv<'_>) -> bool {
    !v.is_valid() || v.is_nil()
}

/// `reflect.Type.Comparable` for the static type of a value.
fn comparable(v: &Value) -> bool {
    match v {
        Value::List(_) | Value::Map(_) => false,
        Value::Struct(_, fields) => fields.iter().all(|(_, f)| comparable(f)),
        _ => true,
    }
}

fn scalar<'v>(v: &'v Rv<'_>) -> Option<&'v Value> {
    v.value()
}

/// `eq` (funcs.go:458).
fn eq(arg1: Rv<'_>, arg2: &[Rv<'_>]) -> Result<bool, String> {
    let arg1 = arg1.indirect_interface();
    if arg2.is_empty() {
        return Err("missing argument for comparison".to_string());
    }
    let k1 = basic_kind(&arg1).unwrap_or(BasicKind::Invalid);
    for arg in arg2 {
        let arg = arg.clone().indirect_interface();
        let k2 = basic_kind(&arg).unwrap_or(BasicKind::Invalid);
        let mut t = false;
        if k1 != k2 {
            if arg1.is_valid() && arg.is_valid() {
                return Err(format!(
                    "incompatible types for comparison: {} and {}",
                    arg1.type_name(),
                    arg.type_name()
                ));
            }
        } else {
            let (a, b) = (scalar(&arg1), scalar(&arg));
            match k1 {
                BasicKind::Bool => t = a == b,
                BasicKind::Float => {
                    if let (Some(Value::Float(x)), Some(Value::Float(y))) = (a, b) {
                        t = x == y;
                    }
                }
                BasicKind::Int => {
                    if let (Some(Value::Int(x)), Some(Value::Int(y))) = (a, b) {
                        t = x == y;
                    }
                }
                BasicKind::String => {
                    let x = a.and_then(Value::as_go_string).map(|p| p.0);
                    let y = b.and_then(Value::as_go_string).map(|p| p.0);
                    t = x == y;
                }
                BasicKind::Invalid => {
                    let (ka, kb) = (arg1.kind(), arg.kind());
                    if !(ka == kb || ka == Kind::Invalid || kb == Kind::Invalid) {
                        return Err(format!(
                            "non-comparable types {}: {}, {}: {}",
                            fmt_rv(&arg1, 's'),
                            arg1.type_name(),
                            arg.type_name(),
                            fmt_rv(&arg, 'v')
                        ));
                    }
                    if is_nil_value(&arg1) || is_nil_value(&arg) {
                        t = is_nil_value(&arg) == is_nil_value(&arg1);
                    } else {
                        let bv = arg
                            .value()
                            .map(Cow::Borrowed)
                            .unwrap_or(Cow::Owned(Value::Nil));
                        if !comparable(&bv) {
                            return Err(format!(
                                "non-comparable type {}: {}",
                                fmt_rv(&arg, 's'),
                                arg.type_name()
                            ));
                        }
                        t = arg1.type_name() == arg.type_name() && arg1.value() == arg.value();
                    }
                }
            }
        }
        if t {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `lt` (funcs.go:521).
fn lt(arg1: &Rv<'_>, arg2: &Rv<'_>) -> Result<bool, String> {
    let arg1 = arg1.clone().indirect_interface();
    let k1 = basic_kind(&arg1)?;
    let arg2 = arg2.clone().indirect_interface();
    let k2 = basic_kind(&arg2)?;
    if k1 != k2 {
        return Err(format!(
            "incompatible types for comparison: {} and {}",
            arg1.type_name(),
            arg2.type_name()
        ));
    }
    let (a, b) = (arg1.value(), arg2.value());
    Ok(match k1 {
        BasicKind::Bool | BasicKind::Invalid => {
            return Err("invalid type for comparison".to_string());
        }
        BasicKind::Float => match (a, b) {
            (Some(Value::Float(x)), Some(Value::Float(y))) => x < y,
            _ => false,
        },
        BasicKind::Int => match (a, b) {
            (Some(Value::Int(x)), Some(Value::Int(y))) => x < y,
            _ => false,
        },
        BasicKind::String => {
            let x = a.and_then(Value::as_go_string).map(|p| p.0).unwrap_or("");
            let y = b.and_then(Value::as_go_string).map(|p| p.0).unwrap_or("");
            x < y
        }
    })
}

/// `le` (funcs.go:564).
fn le(arg1: &Rv<'_>, arg2: &Rv<'_>) -> Result<bool, String> {
    let less = lt(arg1, arg2)?;
    if less {
        return Ok(true);
    }
    eq(arg1.clone(), std::slice::from_ref(arg2))
}

/// `evalArgs` (funcs.go:754): `fmt.Sprint` after `printableValue`, which dereferences pointers
/// and turns an invalid value into the string `<no value>`.
fn eval_args(args: &[Rv<'_>]) -> String {
    if args.len() == 1
        && let Some(Value::String(s)) = args[0].as_any()
    {
        return s.clone();
    }
    let no_value = Value::String("<no value>".to_string());
    let printable: Vec<Value> = args
        .iter()
        .map(|a| match a.as_any() {
            None => no_value.clone(),
            Some(v) => {
                let mut v = v;
                while let Value::Ptr(inner) = v {
                    v = inner;
                }
                v.clone()
            }
        })
        .collect();
    let refs: Vec<Option<&Value>> = printable.iter().map(Some).collect();
    crate::fmt::sprint(&refs)
}

/// `HTMLEscapeString` (funcs.go:626).
pub(crate) fn html_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push('\u{fffd}'),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// `JSEscapeString` (funcs.go:716).
pub(crate) fn js_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let u = c as u32;
        if u < 0x80 {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '"' => out.push_str("\\\""),
                '<' => out.push_str("\\u003C"),
                '>' => out.push_str("\\u003E"),
                '&' => out.push_str("\\u0026"),
                '=' => out.push_str("\\u003D"),
                _ if u < 0x20 => out.push_str(&format!("\\u00{u:02X}")),
                _ => out.push(c),
            }
        } else if crate::strconv::is_print(u) {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{u:04X}"));
        }
    }
    out
}

/// `url.QueryEscape`.
pub(crate) fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapers() {
        assert_eq!(
            html_escape_string("<a href='x'>&\"\0"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&#34;\u{fffd}"
        );
        assert_eq!(
            js_escape_string("a'b\"c<d>e&f=g\\h\u{2028}\u{1}"),
            "a\\'b\\\"c\\u003Cd\\u003Ee\\u0026f\\u003Dg\\\\h\\u2028\\u0001"
        );
        assert_eq!(query_escape("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn comparisons() {
        let one = Rv::owned(Value::Int(1));
        let s = Rv::owned(Value::str("1"));
        assert_eq!(
            eq(one.clone(), std::slice::from_ref(&s)),
            Err("incompatible types for comparison: int and string".to_string())
        );
        assert_eq!(eq(Rv::Invalid, &[Rv::Invalid]), Ok(true));
        assert_eq!(eq(Rv::Invalid, std::slice::from_ref(&one)), Ok(false));
        assert_eq!(lt(&one, &Rv::owned(Value::Int(2))), Ok(true));
        assert_eq!(
            lt(
                &Rv::owned(Value::Bool(true)),
                &Rv::owned(Value::Bool(false))
            ),
            Err("invalid type for comparison".to_string())
        );
        let h = Rv::owned(Value::Html("<b>".into()));
        assert_eq!(eq(h, &[Rv::owned(Value::str("<b>"))]), Ok(true));
    }

    #[test]
    fn index_and_len() {
        let l = Value::List(vec![Value::str("a"), Value::Int(2)]);
        let got = index(Rv::borrowed(&l, false), vec![Rv::owned(Value::Int(1))]).unwrap();
        assert_eq!(got.value(), Some(&Value::Int(2)));
        assert_eq!(
            index(Rv::borrowed(&l, false), vec![Rv::owned(Value::Int(2))]).unwrap_err(),
            "reflect: slice index out of range"
        );
        assert_eq!(
            index(Rv::borrowed(&l, false), vec![Rv::owned(Value::Int(3))]).unwrap_err(),
            "index out of range: 3"
        );
        assert_eq!(
            length(Rv::Invalid).unwrap_err(),
            "reflect: call of reflect.Value.Type on zero Value"
        );
        assert_eq!(
            length(Rv::owned(Value::Int(1))).unwrap_err(),
            "len of type int"
        );
    }
}
