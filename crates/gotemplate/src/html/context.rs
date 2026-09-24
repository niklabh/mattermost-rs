//! Port of `html/template/context.go` (and the `stringer` output in `*_string.go`): the escaper's
//! parse state at a point in a template.

use std::fmt;
use std::sync::Arc;

use super::error::EscError;
use crate::parse::node::Src;

macro_rules! named_enum {
    ($(#[$m:meta])* $name:ident { $($v:ident = $s:literal,)* }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub(crate) enum $name {
            #[default]
            $($v,)*
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $($name::$v => $s,)* })
            }
        }
    };
}

named_enum!(
    /// `state` (context.go:106): the high-level HTML/JS/CSS lexer state.
    State {
        Text = "stateText",
        Tag = "stateTag",
        AttrName = "stateAttrName",
        AfterName = "stateAfterName",
        BeforeValue = "stateBeforeValue",
        HtmlCmt = "stateHTMLCmt",
        Rcdata = "stateRCDATA",
        Attr = "stateAttr",
        Url = "stateURL",
        Srcset = "stateSrcset",
        Js = "stateJS",
        JsDqStr = "stateJSDqStr",
        JsSqStr = "stateJSSqStr",
        JsTmplLit = "stateJSTmplLit",
        JsRegexp = "stateJSRegexp",
        JsBlockCmt = "stateJSBlockCmt",
        JsLineCmt = "stateJSLineCmt",
        JsHtmlOpenCmt = "stateJSHTMLOpenCmt",
        JsHtmlCloseCmt = "stateJSHTMLCloseCmt",
        Css = "stateCSS",
        CssDqStr = "stateCSSDqStr",
        CssSqStr = "stateCSSSqStr",
        CssDqUrl = "stateCSSDqURL",
        CssSqUrl = "stateCSSSqURL",
        CssUrl = "stateCSSURL",
        CssBlockCmt = "stateCSSBlockCmt",
        CssLineCmt = "stateCSSLineCmt",
        Error = "stateError",
        MetaContent = "stateMetaContent",
        MetaContentUrl = "stateMetaContentURL",
        Dead = "stateDead",
    }
);

named_enum!(
    /// `delim` (context.go:215): what ends the current attribute value.
    Delim {
        None = "delimNone",
        DoubleQuote = "delimDoubleQuote",
        SingleQuote = "delimSingleQuote",
        SpaceOrTagEnd = "delimSpaceOrTagEnd",
    }
);

named_enum!(
    /// `urlPart` (context.go:233).
    UrlPart {
        None = "urlPartNone",
        PreQuery = "urlPartPreQuery",
        QueryOrFrag = "urlPartQueryOrFrag",
        Unknown = "urlPartUnknown",
    }
);

named_enum!(
    /// `jsCtx` (context.go:254): whether a `/` starts a regexp or a division.
    JsCtx {
        Regexp = "jsCtxRegexp",
        DivOp = "jsCtxDivOp",
        Unknown = "jsCtxUnknown",
    }
);

named_enum!(
    /// `element` (context.go:272).
    Element {
        None = "elementNone",
        Script = "elementScript",
        Style = "elementStyle",
        Textarea = "elementTextarea",
        Title = "elementTitle",
        Meta = "elementMeta",
    }
);

named_enum!(
    /// `attr` (context.go:290).
    Attr {
        None = "attrNone",
        Script = "attrScript",
        ScriptType = "attrScriptType",
        Style = "attrStyle",
        Url = "attrURL",
        Srcset = "attrSrcset",
        MetaContent = "attrMetaContent",
    }
);

/// Where a `{{break}}` / `{{continue}}` context was recorded (`context.n`).
#[derive(Debug, Clone)]
pub(crate) struct NodeAt {
    pub pos: usize,
    pub src: Option<Arc<Src>>,
    pub line: usize,
}

/// `context` (context.go:17).
///
/// `js_brace_depth` is `None` where Go's slice is nil and `Some(vec![])` where it is empty but
/// non-nil: `eq` treats the two alike (`slices.Equal`), `mangle` does not. Go's slice is shared
/// between copies of a context (and a transition mutates it in place); this port copies it, which
/// differs only for `${` template-literal interpolation carried across a `{{template}}` call.
#[derive(Debug, Clone, Default)]
pub(crate) struct Context {
    pub state: State,
    pub delim: Delim,
    pub url_part: UrlPart,
    pub js_ctx: JsCtx,
    pub js_brace_depth: Option<Vec<i32>>,
    pub attr: Attr,
    pub element: Element,
    pub n: Option<NodeAt>,
    pub err: Option<Arc<EscError>>,
}

impl Context {
    pub(crate) fn with_state(state: State) -> Self {
        Context {
            state,
            ..Context::default()
        }
    }

    pub(crate) fn error(err: EscError) -> Self {
        Context {
            state: State::Error,
            err: Some(Arc::new(err)),
            ..Context::default()
        }
    }

    fn brace_slice(&self) -> &[i32] {
        self.js_brace_depth.as_deref().unwrap_or(&[])
    }

    /// `context.eq` (context.go:48). Errors compare by identity, as Go compares `*Error`.
    pub(crate) fn eq(&self, d: &Context) -> bool {
        self.state == d.state
            && self.delim == d.delim
            && self.url_part == d.url_part
            && self.js_ctx == d.js_ctx
            && self.brace_slice() == d.brace_slice()
            && self.attr == d.attr
            && self.element == d.element
            && match (&self.err, &d.err) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }

    /// `context.mangle` (context.go:62).
    pub(crate) fn mangle(&self, template_name: &str) -> String {
        if self.state == State::Text {
            return template_name.to_string();
        }
        let mut s = format!("{template_name}$htmltemplate_{}", self.state);
        if self.delim != Delim::None {
            s.push_str(&format!("_{}", self.delim));
        }
        if self.url_part != UrlPart::None {
            s.push_str(&format!("_{}", self.url_part));
        }
        if self.js_ctx != JsCtx::Regexp {
            s.push_str(&format!("_{}", self.js_ctx));
        }
        if let Some(d) = &self.js_brace_depth {
            s.push_str(&format!("_jsBraceDepth({})", fmt_ints(d)));
        }
        if self.attr != Attr::None {
            s.push_str(&format!("_{}", self.attr));
        }
        if self.element != Element::None {
            s.push_str(&format!("_{}", self.element));
        }
        s
    }
}

/// `%v` of a `[]int`.
fn fmt_ints(d: &[i32]) -> String {
    let inner: Vec<String> = d.iter().map(i32::to_string).collect();
    format!("[{}]", inner.join(" "))
}

impl fmt::Display for Context {
    /// `context.String` (context.go:40).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let err = match &self.err {
            Some(e) => e.to_string(),
            None => "<nil>".to_string(),
        };
        write!(
            f,
            "{{{} {} {} {} {} {} {} {}}}",
            self.state,
            self.delim,
            self.url_part,
            self.js_ctx,
            fmt_ints(self.brace_slice()),
            self.attr,
            self.element,
            err
        )
    }
}

/// `isComment` (context.go:181).
pub(crate) fn is_comment(s: State) -> bool {
    matches!(
        s,
        State::HtmlCmt
            | State::JsBlockCmt
            | State::JsLineCmt
            | State::JsHtmlOpenCmt
            | State::JsHtmlCloseCmt
            | State::CssBlockCmt
            | State::CssLineCmt
    )
}

/// `isInTag` (context.go:190).
pub(crate) fn is_in_tag(s: State) -> bool {
    matches!(
        s,
        State::Tag | State::AttrName | State::AfterName | State::BeforeValue | State::Attr
    )
}

/// `isInScriptLiteral` (context.go:199).
pub(crate) fn is_in_script_literal(s: State) -> bool {
    matches!(
        s,
        State::JsDqStr | State::JsSqStr | State::JsTmplLit | State::JsRegexp
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangle_names_every_non_default_field() {
        let c = Context {
            state: State::Attr,
            delim: Delim::DoubleQuote,
            url_part: UrlPart::PreQuery,
            js_ctx: JsCtx::DivOp,
            js_brace_depth: Some(vec![]),
            attr: Attr::Url,
            element: Element::Script,
            ..Context::default()
        };
        assert_eq!(
            c.mangle("t"),
            "t$htmltemplate_stateAttr_delimDoubleQuote_urlPartPreQuery_jsCtxDivOp_jsBraceDepth([])_attrURL_elementScript"
        );
        assert_eq!(Context::default().mangle("t"), "t");
    }

    #[test]
    fn display_and_eq() {
        let c = Context::with_state(State::Url);
        assert_eq!(
            c.to_string(),
            "{stateURL delimNone urlPartNone jsCtxRegexp [] attrNone elementNone <nil>}"
        );
        let mut d = c.clone();
        d.js_brace_depth = Some(vec![]);
        assert!(c.eq(&d));
        d.js_brace_depth = Some(vec![1]);
        assert!(!c.eq(&d));
    }
}
