//! Port of `html/template`: the contextual autoescaper and its escaping functions.

pub(crate) mod attr;
pub(crate) mod content;
pub(crate) mod context;
pub(crate) mod css;
pub(crate) mod entity;
#[rustfmt::skip]
pub(crate) mod entity_table;
pub(crate) mod error;
pub(crate) mod escape;
#[allow(clippy::module_inception)]
pub(crate) mod html;
pub(crate) mod js;
pub(crate) mod json;
pub(crate) mod template;
pub(crate) mod transition;
pub(crate) mod url;

use crate::funcs::HtmlFn;
use crate::value::Value;

/// Calls one of the `_html_template_*` functions (escape.go:65) on its `...any` arguments.
pub(crate) fn call_escaper(f: HtmlFn, args: &[Option<&Value>]) -> String {
    match f {
        HtmlFn::AttrEscaper => html::attr_escaper(args),
        HtmlFn::CommentEscaper => html::comment_escaper(args),
        HtmlFn::CssEscaper => css::css_escaper(args),
        HtmlFn::CssValueFilter => css::css_value_filter(args),
        HtmlFn::HtmlNameFilter => html::html_name_filter(args),
        HtmlFn::HtmlEscaper => html::html_escaper(args),
        HtmlFn::JsRegexpEscaper => js::js_regexp_escaper(args),
        HtmlFn::JsStrEscaper => js::js_str_escaper(args),
        HtmlFn::JsTmplLitEscaper => js::js_tmpl_lit_escaper(args),
        HtmlFn::JsValEscaper => js::js_val_escaper(args),
        HtmlFn::NospaceEscaper => html::html_nospace_escaper(args),
        HtmlFn::RcdataEscaper => html::rcdata_escaper(args),
        HtmlFn::SrcsetEscaper => url::srcset_filter_and_escaper(args),
        HtmlFn::UrlEscaper => url::url_escaper(args),
        HtmlFn::UrlFilter => url::url_filter(args),
        HtmlFn::UrlNormalizer => url::url_normalizer(args),
        HtmlFn::EvalArgs => eval_args(args),
    }
}

/// `evalArgs` (escape.go:51): `fmt.Sprint` of the dereferenced arguments; untyped nils print as
/// `<nil>`.
fn eval_args(args: &[Option<&Value>]) -> String {
    if args.len() == 1
        && let Some(Value::String(s)) = args[0]
    {
        return s.clone();
    }
    let derefd: Vec<Option<&Value>> = args.iter().map(|a| a.map(content::indirect)).collect();
    crate::fmt::sprint(&derefd)
}
