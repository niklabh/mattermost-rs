//! Port of `golang.org/x/net@v0.56.0/html/parse.go` — the HTML5 tree-construction algorithm as Go
//! implements it, divergences from the specification included — together with `foreign.go`,
//! `doctype.go` and `const.go`, which only it uses.
//!
//! # Shape of the port
//!
//! Each insertion mode is a method, dispatched through [`Im`]; Go stores a function value in
//! `p.im`, and a `nil` one is reachable (`resetInsertionMode` copies the top of an empty template
//! stack), so the mode is an `Option`. Go's `parse` recovers every panic into the error `Parse`
//! returns — the explicit ones (`"html: open stack of elements exceeds 512 nodes"`) and runtime
//! ones (a nil dereference, an index out of range) alike — so every operation that panics in Go
//! returns [`Panic`] here, carrying Go's message, and `?` plays the part of unwinding.
//!
//! # What is not ported
//!
//! `ParseFragment` (fragment parsing and its context element): neither caller parses fragments.
//! Every `p.fragment` branch is therefore dead and omitted, and `adjustedCurrentNode` is the
//! current node.

use std::mem;

use crate::atom::{Atom, lookup};
use crate::gostrings::{WHITESPACE, equal_fold, to_lower, trim_left_ws};
use crate::node::{Attribute, Document, LinkPanic, Node, NodeId, NodeType};
use crate::token::{Token, TokenType, Tokenizer};

/// The error `html.Parse` returns: a recovered panic, formatted with `%s` as parse.go:2210 does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// A Go panic in flight, with the message `fmt.Errorf("%s", panicErr)` would produce.
#[derive(Debug)]
struct Panic(String);

impl From<LinkPanic> for Panic {
    fn from(p: LinkPanic) -> Self {
        Panic(p.0.to_owned())
    }
}

type R<T> = Result<T, Panic>;

/// A nil pointer dereference, as the Go runtime words it.
fn nil_deref() -> Panic {
    Panic("runtime error: invalid memory address or nil pointer dereference".to_owned())
}

/// An index out of range, as the Go runtime words it.
fn out_of_range(i: isize, len: usize) -> Panic {
    if i < 0 {
        Panic(format!("runtime error: index out of range [{i}]"))
    } else {
        Panic(format!(
            "runtime error: index out of range [{i}] with length {len}"
        ))
    }
}

/// `ParseOptionEnableScripting` (parse.go:2256). Scripting is on by default, as in Go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOptions {
    pub scripting: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions { scripting: true }
    }
}

/// Port of `html.Parse` (parse.go:2238): the parse tree of `input`, or the error a recovered
/// panic became. Documents nested deeper than 512 elements are rejected, as in Go.
pub fn parse(input: &str) -> Result<Document, ParseError> {
    parse_with_options(input, ParseOptions::default())
}

/// Port of `html.ParseWithOptions` (parse.go:2264).
pub fn parse_with_options(input: &str, opts: ParseOptions) -> Result<Document, ParseError> {
    let mut doc = Document::new();
    let marker = doc.alloc(Node::new(NodeType::ScopeMarker, ""));
    let mut p = Parser {
        tokenizer: Tokenizer::new(input.as_bytes()),
        tok: Token {
            token_type: TokenType::Error,
            data_atom: None,
            data: String::new(),
            attr: Vec::new(),
        },
        has_self_closing_token: false,
        doc,
        marker,
        oe: Vec::new(),
        afe: Vec::new(),
        head: None,
        form: None,
        scripting: opts.scripting,
        frameset_ok: true,
        template_stack: Vec::new(),
        im: Some(Im::Initial),
        original_im: None,
        foster_parenting: false,
        quirks: false,
    };
    match p.parse() {
        Ok(()) => Ok(p.doc),
        Err(Panic(msg)) => Err(ParseError(msg)),
    }
}

/// An insertion mode (section 12.2.4.1): which `...IM` function `p.im` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Im {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
    IgnoreTheRemainingTokens,
}

/// The scopes `indexOfElementInScope` accepts (parse.go:73). Go's other two constants only reach
/// `clearStackToContext`, which takes [`ContextScope`]; the split lets the type system rule out
/// the `unknown scope` panics Go guards with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Default,
    ListItem,
    Button,
    Table,
}

/// The scopes `clearStackToContext` accepts (parse.go:159).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextScope {
    Table,
    TableRow,
    TableBody,
}

use Atom as A;

/// `defaultScopeStopTags` (parse.go:63), by namespace.
fn default_scope_stop_tags(ns: &str) -> &'static [Atom] {
    match ns {
        "" => &[
            A::Applet,
            A::Caption,
            A::Html,
            A::Table,
            A::Td,
            A::Th,
            A::Marquee,
            A::Object,
            A::Template,
            A::Select,
        ],
        "math" => &[A::AnnotationXml, A::Mi, A::Mn, A::Mo, A::Ms, A::Mtext],
        "svg" => &[A::Desc, A::ForeignObject, A::Title],
        _ => &[],
    }
}

fn is(a: Option<Atom>, set: &[Atom]) -> bool {
    matches!(a, Some(x) if set.contains(&x))
}

/// `isSpecialElementMap` (const.go:9).
const SPECIAL: &[&str] = &[
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "base",
    "basefont",
    "bgsound",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "keygen",
    "li",
    "link",
    "listing",
    "main",
    "marquee",
    "menu",
    "meta",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "ol",
    "p",
    "param",
    "plaintext",
    "pre",
    "script",
    "section",
    "select",
    "source",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
    "wbr",
    "xmp",
];

/// `isSpecialElement` (const.go:95).
fn is_special_element(n: &Node) -> bool {
    match n.namespace.as_str() {
        "" | "html" => SPECIAL.contains(&n.data.as_str()),
        "math" => matches!(
            n.data.as_str(),
            "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml"
        ),
        "svg" => matches!(n.data.as_str(), "foreignObject" | "desc" | "title"),
        _ => false,
    }
}

/// `breakout` (foreign.go:69).
const BREAKOUT: &[&str] = &[
    "b",
    "big",
    "blockquote",
    "body",
    "br",
    "center",
    "code",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "hr",
    "i",
    "img",
    "li",
    "listing",
    "menu",
    "meta",
    "nobr",
    "ol",
    "p",
    "pre",
    "ruby",
    "s",
    "small",
    "span",
    "strong",
    "strike",
    "sub",
    "sup",
    "table",
    "tt",
    "u",
    "ul",
    "var",
];

/// `svgTagNameAdjustments` (foreign.go:117).
fn svg_tag_name_adjustment(name: &str) -> Option<&'static str> {
    Some(match name {
        "altglyph" => "altGlyph",
        "altglyphdef" => "altGlyphDef",
        "altglyphitem" => "altGlyphItem",
        "animatecolor" => "animateColor",
        "animatemotion" => "animateMotion",
        "animatetransform" => "animateTransform",
        "clippath" => "clipPath",
        "feblend" => "feBlend",
        "fecolormatrix" => "feColorMatrix",
        "fecomponenttransfer" => "feComponentTransfer",
        "fecomposite" => "feComposite",
        "feconvolvematrix" => "feConvolveMatrix",
        "fediffuselighting" => "feDiffuseLighting",
        "fedisplacementmap" => "feDisplacementMap",
        "fedistantlight" => "feDistantLight",
        "feflood" => "feFlood",
        "fefunca" => "feFuncA",
        "fefuncb" => "feFuncB",
        "fefuncg" => "feFuncG",
        "fefuncr" => "feFuncR",
        "fegaussianblur" => "feGaussianBlur",
        "feimage" => "feImage",
        "femerge" => "feMerge",
        "femergenode" => "feMergeNode",
        "femorphology" => "feMorphology",
        "feoffset" => "feOffset",
        "fepointlight" => "fePointLight",
        "fespecularlighting" => "feSpecularLighting",
        "fespotlight" => "feSpotLight",
        "fetile" => "feTile",
        "feturbulence" => "feTurbulence",
        "foreignobject" => "foreignObject",
        "glyphref" => "glyphRef",
        "lineargradient" => "linearGradient",
        "radialgradient" => "radialGradient",
        "textpath" => "textPath",
        _ => return None,
    })
}

/// `mathMLAttributeAdjustments` (foreign.go:157).
fn mathml_attribute_adjustment(name: &str) -> Option<&'static str> {
    (name == "definitionurl").then_some("definitionURL")
}

/// `svgAttributeAdjustments` (foreign.go:161).
fn svg_attribute_adjustment(name: &str) -> Option<&'static str> {
    Some(match name {
        "attributename" => "attributeName",
        "attributetype" => "attributeType",
        "basefrequency" => "baseFrequency",
        "baseprofile" => "baseProfile",
        "calcmode" => "calcMode",
        "clippathunits" => "clipPathUnits",
        "diffuseconstant" => "diffuseConstant",
        "edgemode" => "edgeMode",
        "filterunits" => "filterUnits",
        "glyphref" => "glyphRef",
        "gradienttransform" => "gradientTransform",
        "gradientunits" => "gradientUnits",
        "kernelmatrix" => "kernelMatrix",
        "kernelunitlength" => "kernelUnitLength",
        "keypoints" => "keyPoints",
        "keysplines" => "keySplines",
        "keytimes" => "keyTimes",
        "lengthadjust" => "lengthAdjust",
        "limitingconeangle" => "limitingConeAngle",
        "markerheight" => "markerHeight",
        "markerunits" => "markerUnits",
        "markerwidth" => "markerWidth",
        "maskcontentunits" => "maskContentUnits",
        "maskunits" => "maskUnits",
        "numoctaves" => "numOctaves",
        "pathlength" => "pathLength",
        "patterncontentunits" => "patternContentUnits",
        "patterntransform" => "patternTransform",
        "patternunits" => "patternUnits",
        "pointsatx" => "pointsAtX",
        "pointsaty" => "pointsAtY",
        "pointsatz" => "pointsAtZ",
        "preservealpha" => "preserveAlpha",
        "preserveaspectratio" => "preserveAspectRatio",
        "primitiveunits" => "primitiveUnits",
        "refx" => "refX",
        "refy" => "refY",
        "repeatcount" => "repeatCount",
        "repeatdur" => "repeatDur",
        "requiredextensions" => "requiredExtensions",
        "requiredfeatures" => "requiredFeatures",
        "specularconstant" => "specularConstant",
        "specularexponent" => "specularExponent",
        "spreadmethod" => "spreadMethod",
        "startoffset" => "startOffset",
        "stddeviation" => "stdDeviation",
        "stitchtiles" => "stitchTiles",
        "surfacescale" => "surfaceScale",
        "systemlanguage" => "systemLanguage",
        "tablevalues" => "tableValues",
        "targetx" => "targetX",
        "targety" => "targetY",
        "textlength" => "textLength",
        "viewbox" => "viewBox",
        "viewtarget" => "viewTarget",
        "xchannelselector" => "xChannelSelector",
        "ychannelselector" => "yChannelSelector",
        "zoomandpan" => "zoomAndPan",
        _ => return None,
    })
}

/// `adjustAttributeNames` (foreign.go:11).
fn adjust_attribute_names(aa: &mut [Attribute], map: fn(&str) -> Option<&'static str>) {
    for a in aa {
        if let Some(new_name) = map(&a.key) {
            a.key = new_name.to_owned();
        }
    }
}

/// `adjustForeignAttributes` (foreign.go:19).
fn adjust_foreign_attributes(aa: &mut [Attribute]) {
    for a in aa {
        if !a.key.starts_with('x') {
            continue;
        }
        if matches!(
            a.key.as_str(),
            "xlink:actuate"
                | "xlink:arcrole"
                | "xlink:href"
                | "xlink:role"
                | "xlink:show"
                | "xlink:title"
                | "xlink:type"
                | "xml:lang"
                | "xml:space"
                | "xmlns:xlink"
        ) {
            if let Some((ns, key)) = a.key.split_once(':') {
                let (ns, key) = (ns.to_owned(), key.to_owned());
                a.namespace = ns;
                a.key = key;
            }
        }
    }
}

/// `htmlIntegrationPoint` (foreign.go:34).
fn html_integration_point(n: &Node) -> bool {
    if n.node_type != NodeType::Element {
        return false;
    }
    match n.namespace.as_str() {
        "math" => {
            n.data == "annotation-xml"
                && n.attr.iter().any(|a| {
                    a.key == "encoding"
                        && (equal_fold(&a.val, "text/html")
                            || equal_fold(&a.val, "application/xhtml+xml"))
                })
        }
        "svg" => matches!(n.data.as_str(), "desc" | "foreignObject" | "title"),
        _ => false,
    }
}

/// `mathMLTextIntegrationPoint` (foreign.go:56). No node-type check, as in Go.
fn mathml_text_integration_point(n: &Node) -> bool {
    n.namespace == "math" && matches!(n.data.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
}

/// `quirkyIDs` (doctype.go:103).
const QUIRKY_IDS: &[&str] = &[
    "+//silmaril//dtd html pro v0r11 19970101//",
    "-//advasoft ltd//dtd html 3.0 aswedit + extensions//",
    "-//as//dtd html 3.0 aswedit + extensions//",
    "-//ietf//dtd html 2.0 level 1//",
    "-//ietf//dtd html 2.0 level 2//",
    "-//ietf//dtd html 2.0 strict level 1//",
    "-//ietf//dtd html 2.0 strict level 2//",
    "-//ietf//dtd html 2.0 strict//",
    "-//ietf//dtd html 2.0//",
    "-//ietf//dtd html 2.1e//",
    "-//ietf//dtd html 3.0//",
    "-//ietf//dtd html 3.2 final//",
    "-//ietf//dtd html 3.2//",
    "-//ietf//dtd html 3//",
    "-//ietf//dtd html level 0//",
    "-//ietf//dtd html level 1//",
    "-//ietf//dtd html level 2//",
    "-//ietf//dtd html level 3//",
    "-//ietf//dtd html strict level 0//",
    "-//ietf//dtd html strict level 1//",
    "-//ietf//dtd html strict level 2//",
    "-//ietf//dtd html strict level 3//",
    "-//ietf//dtd html strict//",
    "-//ietf//dtd html//",
    "-//metrius//dtd metrius presentational//",
    "-//microsoft//dtd internet explorer 2.0 html strict//",
    "-//microsoft//dtd internet explorer 2.0 html//",
    "-//microsoft//dtd internet explorer 2.0 tables//",
    "-//microsoft//dtd internet explorer 3.0 html strict//",
    "-//microsoft//dtd internet explorer 3.0 html//",
    "-//microsoft//dtd internet explorer 3.0 tables//",
    "-//netscape comm. corp.//dtd html//",
    "-//netscape comm. corp.//dtd strict html//",
    "-//o'reilly and associates//dtd html 2.0//",
    "-//o'reilly and associates//dtd html extended 1.0//",
    "-//o'reilly and associates//dtd html extended relaxed 1.0//",
    "-//softquad software//dtd hotmetal pro 6.0::19990601::extensions to html 4.0//",
    "-//softquad//dtd hotmetal pro 4.0::19971010::extensions to html 4.0//",
    "-//spyglass//dtd html 2.0 extended//",
    "-//sq//dtd html 2.0 hotmetal + extensions//",
    "-//sun microsystems corp.//dtd hotjava html//",
    "-//sun microsystems corp.//dtd hotjava strict html//",
    "-//w3c//dtd html 3 1995-03-24//",
    "-//w3c//dtd html 3.2 draft//",
    "-//w3c//dtd html 3.2 final//",
    "-//w3c//dtd html 3.2//",
    "-//w3c//dtd html 3.2s draft//",
    "-//w3c//dtd html 4.0 frameset//",
    "-//w3c//dtd html 4.0 transitional//",
    "-//w3c//dtd html experimental 19960712//",
    "-//w3c//dtd html experimental 970421//",
    "-//w3c//dtd w3 html//",
    "-//w3o//dtd w3 html 3.0//",
    "-//webtechs//dtd mozilla html 2.0//",
    "-//webtechs//dtd mozilla html//",
];

/// Port of `parseDoctype` (doctype.go:16): the doctype node and whether it puts the document in
/// quirks mode.
fn parse_doctype(s: &str) -> (Node, bool) {
    let mut n = Node::new(NodeType::Doctype, "");
    let space = s.find(WHITESPACE).unwrap_or(s.len());
    let name = &s[..space];
    // "The comparison to "html" is case-sensitive."
    let mut quirks = name != "html";
    n.data = to_lower(name);
    let mut s = trim_left_ws(&s[space..]).as_bytes();

    if s.len() < 6 {
        // "It can't start with "PUBLIC" or "SYSTEM". Ignore the rest of the string."
        return (n, quirks || !s.is_empty());
    }

    // `strings.ToLower(s[:6])` can only equal "public" or "system" when those six bytes are
    // ASCII (a non-ASCII rune is at least two bytes, and no rune lowers into one of those
    // letters from outside ASCII except `K`, which neither word holds), so an ASCII fold
    // decides the comparison exactly — and never splits a code point.
    let lowered = s[..6].to_ascii_lowercase();
    let mut key: &str = match lowered.as_slice() {
        b"public" => "public",
        b"system" => "system",
        _ => "?",
    };
    s = &s[6..];
    while key == "public" || key == "system" {
        s = trim_left_ws_bytes(s);
        let Some(&quote) = s.first() else {
            break;
        };
        if quote != b'"' && quote != b'\'' {
            break;
        }
        s = &s[1..];
        let id;
        match s.iter().position(|&c| c == quote) {
            None => {
                id = s;
                s = &[];
            }
            Some(q) => {
                id = &s[..q];
                s = &s[q + 1..];
            }
        }
        n.attr.push(Attribute {
            namespace: String::new(),
            key: key.to_owned(),
            val: String::from_utf8_lossy(id).into_owned(),
        });
        key = if key == "public" { "system" } else { "" };
    }

    if !key.is_empty() || !s.is_empty() {
        quirks = true;
    } else if let Some(first) = n.attr.first() {
        if first.key == "public" {
            let public = to_lower(&first.val);
            match public.as_str() {
                "-//w3o//dtd w3 html strict 3.0//en//"
                | "-/w3d/dtd html 4.0 transitional/en"
                | "html" => quirks = true,
                _ => {
                    if QUIRKY_IDS.iter().any(|q| public.starts_with(q)) {
                        quirks = true;
                    }
                }
            }
            // "The following two public IDs only cause quirks mode if there is no system ID."
            if n.attr.len() == 1
                && (public.starts_with("-//w3c//dtd html 4.01 frameset//")
                    || public.starts_with("-//w3c//dtd html 4.01 transitional//"))
            {
                quirks = true;
            }
        }
        if let Some(last) = n.attr.last() {
            if last.key == "system"
                && equal_fold(
                    &last.val,
                    "http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd",
                )
            {
                quirks = true;
            }
        }
    }
    (n, quirks)
}

fn trim_left_ws_bytes(s: &[u8]) -> &[u8] {
    let i = s
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0x0c))
        .unwrap_or(s.len());
    &s[i..]
}

/// Port of `parser` (parse.go:20).
struct Parser<'a> {
    tokenizer: Tokenizer<'a>,
    tok: Token,
    has_self_closing_token: bool,
    doc: Document,
    /// The one `scopeMarker` node every marker entry of `afe` points at.
    marker: NodeId,
    oe: Vec<NodeId>,
    afe: Vec<NodeId>,
    head: Option<NodeId>,
    form: Option<NodeId>,
    scripting: bool,
    frameset_ok: bool,
    template_stack: Vec<Im>,
    im: Option<Im>,
    original_im: Option<Im>,
    foster_parenting: bool,
    quirks: bool,
}

impl Parser<'_> {
    fn n(&self, id: NodeId) -> &Node {
        &self.doc[id]
    }

    fn nm(&mut self, id: NodeId) -> &mut Node {
        self.doc.get_mut(id)
    }

    // ----- the two node stacks (node.go:160) -----

    /// `p.oe.top()`: `None` is Go's nil.
    fn oe_top(&self) -> Option<NodeId> {
        self.oe.last().copied()
    }

    /// `p.oe.top()` where Go dereferences the result.
    fn oe_top_deref(&self) -> R<NodeId> {
        self.oe_top().ok_or_else(nil_deref)
    }

    /// `p.oe.pop()`, which panics on an empty stack.
    fn oe_pop(&mut self) -> R<NodeId> {
        self.oe.pop().ok_or_else(|| out_of_range(-1, 0))
    }

    fn oe_at(&self, i: isize) -> R<NodeId> {
        usize::try_from(i)
            .ok()
            .and_then(|u| self.oe.get(u).copied())
            .ok_or_else(|| out_of_range(i, self.oe.len()))
    }

    fn oe_index(&self, n: NodeId) -> isize {
        index_of(&self.oe, n)
    }

    /// `nodeStack.contains` (node.go:188): an HTML element with atom `a`.
    fn oe_contains(&self, a: Atom) -> bool {
        self.oe
            .iter()
            .any(|&n| self.n(n).data_atom == Some(a) && self.n(n).namespace.is_empty())
    }

    fn afe_index(&self, n: NodeId) -> isize {
        index_of(&self.afe, n)
    }

    fn afe_pop(&mut self) -> R<NodeId> {
        self.afe.pop().ok_or_else(|| out_of_range(-1, 0))
    }

    fn is_marker(&self, n: NodeId) -> bool {
        self.n(n).node_type == NodeType::ScopeMarker
    }

    /// `p.top()` (parse.go:56): the current node, or the document.
    fn top(&self) -> NodeId {
        self.oe_top().unwrap_or_else(|| self.doc.root())
    }

    fn top_atom(&self) -> Option<Atom> {
        self.n(self.top()).data_atom
    }

    // ----- parse.go helpers -----

    /// Port of `popUntil` (parse.go:101).
    fn pop_until(&mut self, s: Scope, match_tags: &[Atom]) -> bool {
        match self.index_of_element_in_scope(s, match_tags) {
            Some(i) => {
                self.oe.truncate(i);
                true
            }
            None => false,
        }
    }

    /// Port of `indexOfElementInScope` (parse.go:112).
    fn index_of_element_in_scope(&self, s: Scope, match_tags: &[Atom]) -> Option<usize> {
        for i in (0..self.oe.len()).rev() {
            let n = self.n(self.oe[i]);
            let tag_atom = n.data_atom;
            if n.namespace.is_empty() {
                if is(tag_atom, match_tags) {
                    return Some(i);
                }
                match s {
                    Scope::Default => {}
                    Scope::ListItem => {
                        if is(tag_atom, &[A::Ol, A::Ul]) {
                            return None;
                        }
                    }
                    Scope::Button => {
                        if tag_atom == Some(A::Button) {
                            return None;
                        }
                    }
                    Scope::Table => {
                        if is(tag_atom, &[A::Html, A::Table, A::Template]) {
                            return None;
                        }
                    }
                }
            }
            if matches!(s, Scope::Default | Scope::ListItem | Scope::Button)
                && is(tag_atom, default_scope_stop_tags(&n.namespace))
            {
                return None;
            }
        }
        None
    }

    fn element_in_scope(&self, s: Scope, match_tags: &[Atom]) -> bool {
        self.index_of_element_in_scope(s, match_tags).is_some()
    }

    /// Port of `clearStackToContext` (parse.go:159).
    fn clear_stack_to_context(&mut self, s: ContextScope) {
        let stop: &[Atom] = match s {
            ContextScope::Table => &[A::Html, A::Table, A::Template],
            ContextScope::TableRow => &[A::Html, A::Tr, A::Template],
            ContextScope::TableBody => &[A::Html, A::Tbody, A::Tfoot, A::Thead, A::Template],
        };
        for i in (0..self.oe.len()).rev() {
            if is(self.n(self.oe[i]).data_atom, stop) {
                self.oe.truncate(i + 1);
                return;
            }
        }
    }

    /// Port of `parseGenericRawTextElement` (parse.go:191).
    fn parse_generic_raw_text_element(&mut self) -> R<()> {
        self.add_element()?;
        self.original_im = self.im;
        self.im = Some(Im::Text);
        Ok(())
    }

    /// Port of `generateImpliedEndTags` (parse.go:200).
    fn generate_implied_end_tags(&mut self, exceptions: &[&str]) {
        let mut i = self.oe.len();
        while i > 0 {
            let n = self.n(self.oe[i - 1]);
            if n.node_type != NodeType::Element {
                break;
            }
            let implied = is(
                n.data_atom,
                &[
                    A::Dd,
                    A::Dt,
                    A::Li,
                    A::Optgroup,
                    A::Option,
                    A::P,
                    A::Rb,
                    A::Rp,
                    A::Rt,
                    A::Rtc,
                ],
            );
            if !implied || exceptions.contains(&n.data.as_str()) {
                break;
            }
            i -= 1;
        }
        self.oe.truncate(i);
    }

    /// Port of `addChild` (parse.go:226).
    fn add_child(&mut self, n: NodeId) -> R<()> {
        if self.should_foster_parent() {
            self.foster_parent(n)?;
        } else {
            let top = self.top();
            self.doc.append_child(top, n)?;
        }
        if self.n(n).node_type == NodeType::Element {
            self.insert_open_element(n)?;
        }
        Ok(())
    }

    /// Port of `insertOpenElement` (parse.go:238).
    fn insert_open_element(&mut self, n: NodeId) -> R<()> {
        self.oe.push(n);
        if self.oe.len() > 512 {
            return Err(Panic(
                "html: open stack of elements exceeds 512 nodes".to_owned(),
            ));
        }
        Ok(())
    }

    /// Port of `shouldFosterParent` (parse.go:247).
    fn should_foster_parent(&self) -> bool {
        self.foster_parenting
            && is(
                self.top_atom(),
                &[A::Table, A::Tbody, A::Tfoot, A::Thead, A::Tr],
            )
    }

    /// Port of `fosterParent` (parse.go:259).
    fn foster_parent(&mut self, n: NodeId) -> R<()> {
        let mut table = None;
        let mut i: isize = self.oe.len() as isize - 1;
        while i >= 0 {
            let e = self.oe[i as usize];
            if self.n(e).data_atom == Some(A::Table) {
                table = Some(e);
                break;
            }
            i -= 1;
        }
        let mut template = None;
        let mut j: isize = self.oe.len() as isize - 1;
        while j >= 0 {
            let e = self.oe[j as usize];
            if self.n(e).data_atom == Some(A::Template) {
                template = Some(e);
                break;
            }
            j -= 1;
        }

        if let Some(t) = template {
            if table.is_none() || j > i {
                self.doc.append_child(t, n)?;
                return Ok(());
            }
        }

        let parent = match table {
            // "The foster parent is the html element."
            None => Some(self.oe_at(0)?),
            Some(t) => self.n(t).parent,
        };
        let parent = match parent {
            Some(p) => p,
            None => self.oe_at(i - 1)?,
        };

        let prev = match table {
            Some(t) => self.n(t).prev_sibling,
            None => self.n(parent).last_child,
        };
        if let Some(prev) = prev {
            if self.n(prev).node_type == NodeType::Text && self.n(n).node_type == NodeType::Text {
                let data = mem::take(&mut self.nm(n).data);
                self.nm(prev).data.push_str(&data);
                return Ok(());
            }
        }
        self.doc.insert_before(parent, n, table)?;
        Ok(())
    }

    /// Port of `addText` (parse.go:307).
    fn add_text(&mut self, text: &str) -> R<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.should_foster_parent() {
            let t = self.doc.alloc(Node::new(NodeType::Text, text));
            return self.foster_parent(t);
        }
        let t = self.top();
        if let Some(last) = self.n(t).last_child {
            if self.n(last).node_type == NodeType::Text {
                self.nm(last).data.push_str(text);
                return Ok(());
            }
        }
        let n = self.doc.alloc(Node::new(NodeType::Text, text));
        self.add_child(n)
    }

    fn add_comment(&mut self) -> R<()> {
        let c = self.new_comment();
        self.add_child(c)
    }

    fn new_comment(&mut self) -> NodeId {
        let data = self.tok.data.clone();
        self.doc.alloc(Node::new(NodeType::Comment, data))
    }

    /// Port of `addElement` (parse.go:340). Go's node shares the token's attribute slice; the
    /// only later write through that alias is `addFormattingElement`'s sort, which sorts the
    /// node's copy here too.
    fn add_element(&mut self) -> R<()> {
        let mut n = Node::new(NodeType::Element, self.tok.data.as_str());
        n.data_atom = self.tok.data_atom;
        n.attr = self.tok.attr.clone();
        let id = self.doc.alloc(n);
        self.add_child(id)
    }

    /// Port of `addFormattingElement` (parse.go:350), the Noah's Ark clause with three per family.
    fn add_formatting_element(&mut self) -> R<()> {
        let tag_atom = self.tok.data_atom;
        self.add_element()?;
        self.tok.attr.sort();
        let attr = &self.tok.attr;

        let mut identical_elements = 0;
        let mut i = self.afe.len();
        let mut remove = Vec::new();
        while i > 0 {
            i -= 1;
            let id = self.afe[i];
            let n = self.n(id);
            if n.node_type == NodeType::ScopeMarker {
                break;
            }
            if n.node_type != NodeType::Element
                || !n.namespace.is_empty()
                || n.data_atom != tag_atom
                || n.attr != *attr
            {
                continue;
            }
            identical_elements += 1;
            if identical_elements >= 3 {
                remove.push(id);
            }
        }
        // Go removes as it walks down; each removal shifts only entries above `i`, which the walk
        // has already passed, so removing afterwards visits the same entries.
        for id in remove {
            remove_from(&mut self.afe, id);
        }

        let top = self.top();
        self.nm(top).attr.sort();
        self.afe.push(top);
        Ok(())
    }

    /// Port of `clearActiveFormattingElements` (parse.go:393).
    fn clear_active_formatting_elements(&mut self) -> R<()> {
        loop {
            let n = self.afe_pop()?;
            if self.afe.is_empty() || self.is_marker(n) {
                return Ok(());
            }
        }
    }

    /// Port of `reconstructActiveFormattingElements` (parse.go:402).
    fn reconstruct_active_formatting_elements(&mut self) -> R<()> {
        let Some(mut n) = self.afe.last().copied() else {
            return Ok(());
        };
        if self.is_marker(n) || self.oe_index(n) != -1 {
            return Ok(());
        }
        let mut i: isize = self.afe.len() as isize - 1;
        while !self.is_marker(n) && self.oe_index(n) == -1 {
            if i == 0 {
                i = -1;
                break;
            }
            i -= 1;
            n = self.afe[i as usize];
        }
        loop {
            i += 1;
            let clone = self.doc.clone_node(self.afe[i as usize]);
            self.add_child(clone)?;
            self.afe[i as usize] = clone;
            if i as usize == self.afe.len() - 1 {
                break;
            }
        }
        Ok(())
    }

    /// Port of `acknowledgeSelfClosingTag` (parse.go:429).
    fn acknowledge_self_closing_tag(&mut self) {
        self.has_self_closing_token = false;
    }

    /// Port of `setOriginalIM` (parse.go:442).
    fn set_original_im(&mut self) -> R<()> {
        if self.original_im.is_some() {
            return Err(Panic(
                "html: bad parser state: originalIM was set twice".to_owned(),
            ));
        }
        self.original_im = self.im;
        Ok(())
    }

    /// Port of `resetInsertionMode` (parse.go:451).
    fn reset_insertion_mode(&mut self) {
        for i in (0..self.oe.len()).rev() {
            let n = self.n(self.oe[i]);
            let last = i == 0;
            self.im = match n.data_atom {
                Some(A::Td | A::Th) => Some(Im::InCell),
                Some(A::Tr) => Some(Im::InRow),
                Some(A::Tbody | A::Thead | A::Tfoot) => Some(Im::InTableBody),
                Some(A::Caption) => Some(Im::InCaption),
                Some(A::Colgroup) => Some(Im::InColumnGroup),
                Some(A::Table) => Some(Im::InTable),
                Some(A::Template) => {
                    if !n.namespace.is_empty() {
                        continue;
                    }
                    self.template_stack.last().copied()
                }
                Some(A::Head) => Some(Im::InHead),
                Some(A::Body) => Some(Im::InBody),
                Some(A::Frameset) => Some(Im::InFrameset),
                Some(A::Html) => {
                    if self.head.is_none() {
                        Some(Im::BeforeHead)
                    } else {
                        Some(Im::AfterHead)
                    }
                }
                _ => {
                    if last {
                        self.im = Some(Im::InBody);
                        return;
                    }
                    continue;
                }
            };
            return;
        }
    }

    /// Port of `parseImpliedToken` (parse.go:2145).
    fn parse_implied_token(&mut self, t: TokenType, data_atom: Atom) -> R<()> {
        let real_token = mem::replace(
            &mut self.tok,
            Token {
                token_type: t,
                data_atom: Some(data_atom),
                data: data_atom.as_str().to_owned(),
                attr: Vec::new(),
            },
        );
        let self_closing = self.has_self_closing_token;
        self.has_self_closing_token = false;
        self.parse_current_token()?;
        self.tok = real_token;
        self.has_self_closing_token = self_closing;
        Ok(())
    }

    /// Port of `parseCurrentToken` (parse.go:2159).
    fn parse_current_token(&mut self) -> R<()> {
        if self.tok.token_type == TokenType::SelfClosingTag {
            self.has_self_closing_token = true;
            self.tok.token_type = TokenType::StartTag;
        }
        let mut consumed = false;
        while !consumed {
            consumed = if self.in_foreign_content() {
                self.parse_foreign_content()?
            } else {
                self.call_im()?
            };
        }
        if self.has_self_closing_token {
            // "This is a parse error, but ignore it."
            self.has_self_closing_token = false;
        }
        Ok(())
    }

    /// Port of `parse` (parse.go:2182).
    fn parse(&mut self) -> R<()> {
        loop {
            // "CDATA sections are allowed only in foreign content."
            let allow = self
                .oe_top()
                .is_some_and(|n| !self.n(n).namespace.is_empty());
            self.tokenizer.allow_cdata(allow);
            self.tokenizer.next_token();
            self.tok = self.tokenizer.token();
            let eof = self.tok.token_type == TokenType::Error;
            self.parse_current_token()?;
            if eof {
                return Ok(());
            }
        }
    }

    /// `p.im(p)`.
    fn call_im(&mut self) -> R<bool> {
        match self.im {
            None => Err(nil_deref()),
            Some(im) => self.run(im),
        }
    }

    fn run(&mut self, im: Im) -> R<bool> {
        match im {
            Im::Initial => self.initial_im(),
            Im::BeforeHtml => self.before_html_im(),
            Im::BeforeHead => self.before_head_im(),
            Im::InHead => self.in_head_im(),
            Im::InHeadNoscript => self.in_head_noscript_im(),
            Im::AfterHead => self.after_head_im(),
            Im::InBody => self.in_body_im(),
            Im::Text => self.text_im(),
            Im::InTable => self.in_table_im(),
            Im::InCaption => self.in_caption_im(),
            Im::InColumnGroup => self.in_column_group_im(),
            Im::InTableBody => self.in_table_body_im(),
            Im::InRow => self.in_row_im(),
            Im::InCell => self.in_cell_im(),
            Im::InTemplate => self.in_template_im(),
            Im::AfterBody => self.after_body_im(),
            Im::InFrameset => self.in_frameset_im(),
            Im::AfterFrameset => self.after_frameset_im(),
            Im::AfterAfterBody => self.after_after_body_im(),
            Im::AfterAfterFrameset => self.after_after_frameset_im(),
            Im::IgnoreTheRemainingTokens => Ok(true),
        }
    }

    // ----- insertion modes -----

    /// Port of `initialIM` (parse.go:507).
    fn initial_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                self.tok.data = trim_left_ws(&self.tok.data).to_owned();
                if self.tok.data.is_empty() {
                    // "It was all whitespace, so ignore it."
                    return Ok(true);
                }
            }
            TokenType::Comment => {
                let c = self.new_comment();
                let root = self.doc.root();
                self.doc.append_child(root, c)?;
                return Ok(true);
            }
            TokenType::Doctype => {
                let (n, quirks) = parse_doctype(&self.tok.data);
                let n = self.doc.alloc(n);
                let root = self.doc.root();
                self.doc.append_child(root, n)?;
                self.quirks = quirks;
                self.im = Some(Im::BeforeHtml);
                return Ok(true);
            }
            _ => {}
        }
        self.quirks = true;
        self.im = Some(Im::BeforeHtml);
        Ok(false)
    }

    /// Port of `beforeHTMLIM` (parse.go:534).
    fn before_html_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Doctype => return Ok(true),
            TokenType::Text => {
                self.tok.data = trim_left_ws(&self.tok.data).to_owned();
                if self.tok.data.is_empty() {
                    return Ok(true);
                }
            }
            TokenType::StartTag => {
                if self.tok.data_atom == Some(A::Html) {
                    self.add_element()?;
                    self.im = Some(Im::BeforeHead);
                    return Ok(true);
                }
            }
            TokenType::EndTag => {
                return if is(self.tok.data_atom, &[A::Head, A::Body, A::Html, A::Br]) {
                    self.parse_implied_token(TokenType::StartTag, A::Html)?;
                    Ok(false)
                } else {
                    Ok(true)
                };
            }
            TokenType::Comment => {
                let c = self.new_comment();
                let root = self.doc.root();
                self.doc.append_child(root, c)?;
                return Ok(true);
            }
            _ => {}
        }
        self.parse_implied_token(TokenType::StartTag, A::Html)?;
        Ok(false)
    }

    /// Port of `beforeHeadIM` (parse.go:572).
    fn before_head_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                self.tok.data = trim_left_ws(&self.tok.data).to_owned();
                if self.tok.data.is_empty() {
                    return Ok(true);
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Head) => {
                    self.add_element()?;
                    self.head = Some(self.top());
                    self.im = Some(Im::InHead);
                    return Ok(true);
                }
                Some(A::Html) => return self.in_body_im(),
                _ => {}
            },
            TokenType::EndTag => {
                return if is(self.tok.data_atom, &[A::Head, A::Body, A::Html, A::Br]) {
                    self.parse_implied_token(TokenType::StartTag, A::Head)?;
                    Ok(false)
                } else {
                    Ok(true)
                };
            }
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            TokenType::Doctype => return Ok(true),
            _ => {}
        }
        self.parse_implied_token(TokenType::StartTag, A::Head)?;
        Ok(false)
    }

    /// The leading-whitespace step shared by several modes: add the whitespace prefix as text and
    /// report whether nothing is left. The token keeps only the rest.
    fn split_leading_whitespace(&mut self) -> R<bool> {
        let data = mem::take(&mut self.tok.data);
        let rest_len = trim_left_ws(&data).len();
        if rest_len < data.len() {
            let (ws, rest) = data.split_at(data.len() - rest_len);
            self.add_text(ws)?;
            if rest.is_empty() {
                self.tok.data = data;
                return Ok(true);
            }
            self.tok.data = rest.to_owned();
        } else {
            self.tok.data = data;
        }
        Ok(false)
    }

    /// Port of `inHeadIM` (parse.go:615).
    fn in_head_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                if self.split_leading_whitespace()? {
                    return Ok(true);
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Base | A::Basefont | A::Bgsound | A::Link | A::Meta) => {
                    self.add_element()?;
                    self.oe_pop()?;
                    self.acknowledge_self_closing_tag();
                    return Ok(true);
                }
                Some(A::Noscript) => {
                    if self.scripting {
                        self.parse_generic_raw_text_element()?;
                        return Ok(true);
                    }
                    self.add_element()?;
                    self.im = Some(Im::InHeadNoscript);
                    // "Don't let the tokenizer go into raw text mode when scripting is disabled."
                    self.tokenizer.next_is_not_raw_text();
                    return Ok(true);
                }
                Some(A::Script | A::Title) => {
                    self.add_element()?;
                    self.set_original_im()?;
                    self.im = Some(Im::Text);
                    return Ok(true);
                }
                Some(A::Noframes | A::Style) => {
                    self.parse_generic_raw_text_element()?;
                    return Ok(true);
                }
                Some(A::Head) => return Ok(true),
                Some(A::Template) => {
                    // Go's divergence: templates mixed with foreign content ignore the rest of
                    // the document, to avoid an infinite loop.
                    if self.oe.iter().any(|&e| !self.n(e).namespace.is_empty()) {
                        self.im = Some(Im::IgnoreTheRemainingTokens);
                        return Ok(true);
                    }
                    self.add_element()?;
                    self.afe.push(self.marker);
                    self.frameset_ok = false;
                    self.im = Some(Im::InTemplate);
                    self.template_stack.push(Im::InTemplate);
                    return Ok(true);
                }
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Head) => {
                    self.oe_pop()?;
                    self.im = Some(Im::AfterHead);
                    return Ok(true);
                }
                Some(A::Body | A::Html | A::Br) => {
                    self.parse_implied_token(TokenType::EndTag, A::Head)?;
                    return Ok(false);
                }
                Some(A::Template) => {
                    if !self.oe_contains(A::Template) {
                        return Ok(true);
                    }
                    self.close_template()?;
                    return Ok(true);
                }
                _ => return Ok(true),
            },
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            TokenType::Doctype => return Ok(true),
            _ => {}
        }
        self.parse_implied_token(TokenType::EndTag, A::Head)?;
        Ok(false)
    }

    /// The template-closing steps `inHeadIM` (`</template>`) and `inTemplateIM` (EOF) share.
    fn close_template(&mut self) -> R<()> {
        self.generate_implied_end_tags(&[]);
        for i in (0..self.oe.len()).rev() {
            let n = self.n(self.oe[i]);
            if n.namespace.is_empty() && n.data_atom == Some(A::Template) {
                self.oe.truncate(i);
                break;
            }
        }
        self.clear_active_formatting_elements()?;
        if self.template_stack.pop().is_none() {
            return Err(out_of_range(-1, 0));
        }
        self.reset_insertion_mode();
        Ok(())
    }

    /// Port of `inHeadNoscriptIM` (parse.go:729). Unreachable with scripting on, the default.
    fn in_head_noscript_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Doctype => return Ok(true),
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Basefont | A::Bgsound | A::Link | A::Meta | A::Noframes | A::Style) => {
                    return self.in_head_im();
                }
                Some(A::Head) => return Ok(true),
                Some(A::Noscript) => {
                    self.tokenizer.next_is_not_raw_text();
                    return Ok(true);
                }
                _ => {}
            },
            TokenType::EndTag => {
                if !is(self.tok.data_atom, &[A::Noscript, A::Br]) {
                    return Ok(true);
                }
            }
            TokenType::Text => {
                if trim_left_ws(&self.tok.data).is_empty() {
                    return self.in_head_im();
                }
            }
            TokenType::Comment => return self.in_head_im(),
            _ => {}
        }
        self.oe_pop()?;
        if self.top_atom() != Some(A::Head) {
            return Err(Panic(
                "html: the new current node will be a head element.".to_owned(),
            ));
        }
        self.im = Some(Im::InHead);
        Ok(self.tok.data_atom == Some(A::Noscript))
    }

    /// Port of `afterHeadIM` (parse.go:778).
    fn after_head_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                if self.split_leading_whitespace()? {
                    return Ok(true);
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Body) => {
                    self.add_element()?;
                    self.frameset_ok = false;
                    self.im = Some(Im::InBody);
                    return Ok(true);
                }
                Some(A::Frameset) => {
                    self.add_element()?;
                    self.im = Some(Im::InFrameset);
                    return Ok(true);
                }
                Some(
                    A::Base
                    | A::Basefont
                    | A::Bgsound
                    | A::Link
                    | A::Meta
                    | A::Noframes
                    | A::Script
                    | A::Style
                    | A::Template
                    | A::Title,
                ) => {
                    let head = self.head.ok_or_else(nil_deref)?;
                    self.insert_open_element(head)?;
                    // `defer p.oe.remove(p.head)`
                    let r = self.in_head_im();
                    remove_from(&mut self.oe, head);
                    return r;
                }
                Some(A::Head) => return Ok(true),
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                // "Drop down to creating an implied <body> tag."
                Some(A::Body | A::Html | A::Br) => {}
                Some(A::Template) => return self.in_head_im(),
                _ => return Ok(true),
            },
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            TokenType::Doctype => return Ok(true),
            _ => {}
        }
        self.parse_implied_token(TokenType::StartTag, A::Body)?;
        self.frameset_ok = true;
        // "Stop parsing."
        Ok(self.tok.token_type == TokenType::Error)
    }

    /// Port of `copyAttributes` (parse.go:855): the token's attributes the node lacks, appended.
    fn copy_attributes(&mut self, dst: NodeId) {
        if self.tok.attr.is_empty() {
            return;
        }
        let mut keys: Vec<String> = self.n(dst).attr.iter().map(|a| a.key.clone()).collect();
        for t in &self.tok.attr {
            if !keys.contains(&t.key) {
                keys.push(t.key.clone());
                self.doc.get_mut(dst).attr.push(t.clone());
            }
        }
    }

    /// Port of `inBodyIM` (parse.go:872).
    fn in_body_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                let n = self.oe_top_deref()?;
                let mut d: &str = &self.tok.data;
                if is(self.n(n).data_atom, &[A::Pre, A::Listing]) && self.n(n).first_child.is_none()
                {
                    // "Ignore a newline at the start of a <pre> block."
                    d = d.strip_prefix('\r').unwrap_or(d);
                    d = d.strip_prefix('\n').unwrap_or(d);
                }
                let d = d.replace('\0', "");
                if d.is_empty() {
                    return Ok(true);
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_text(&d)?;
                if self.frameset_ok && !trim_left_ws(&d).is_empty() {
                    // "There were non-whitespace characters inserted."
                    self.frameset_ok = false;
                }
            }
            TokenType::StartTag => return self.in_body_start_tag(),
            TokenType::EndTag => return self.in_body_end_tag(),
            TokenType::Comment => self.add_comment()?,
            TokenType::Error => {
                // Go's divergence from the specification.
                if !self.template_stack.is_empty() {
                    self.im = Some(Im::InTemplate);
                    return Ok(false);
                }
                for &e in &self.oe {
                    if !is(
                        self.n(e).data_atom,
                        &[
                            A::Dd,
                            A::Dt,
                            A::Li,
                            A::Optgroup,
                            A::Option,
                            A::P,
                            A::Rb,
                            A::Rp,
                            A::Rt,
                            A::Rtc,
                            A::Tbody,
                            A::Td,
                            A::Tfoot,
                            A::Th,
                            A::Thead,
                            A::Tr,
                            A::Body,
                            A::Html,
                        ],
                    ) {
                        return Ok(true);
                    }
                }
            }
            TokenType::Doctype | TokenType::SelfClosingTag => {}
        }
        Ok(true)
    }

    /// The `StartTagToken` arm of `inBodyIM` (parse.go:897).
    fn in_body_start_tag(&mut self) -> R<bool> {
        let atom = self.tok.data_atom;
        match atom {
            Some(A::Html) => {
                if self.oe_contains(A::Template) {
                    return Ok(true);
                }
                let html = self.oe_at(0)?;
                self.copy_attributes(html);
            }
            Some(
                A::Base
                | A::Basefont
                | A::Bgsound
                | A::Link
                | A::Meta
                | A::Noframes
                | A::Script
                | A::Style
                | A::Template
                | A::Title,
            ) => return self.in_head_im(),
            Some(A::Body) => {
                if self.oe_contains(A::Template) {
                    return Ok(true);
                }
                if self.oe.len() >= 2 {
                    let body = self.oe[1];
                    if self.n(body).node_type == NodeType::Element
                        && self.n(body).data_atom == Some(A::Body)
                    {
                        self.frameset_ok = false;
                        self.copy_attributes(body);
                    }
                }
            }
            Some(A::Frameset) => {
                if !self.frameset_ok
                    || self.oe.len() < 2
                    || self.n(self.oe[1]).data_atom != Some(A::Body)
                {
                    return Ok(true);
                }
                let body = self.oe[1];
                if let Some(parent) = self.n(body).parent {
                    self.doc.remove_child(parent, body)?;
                }
                self.oe.truncate(1);
                self.add_element()?;
                self.im = Some(Im::InFrameset);
                return Ok(true);
            }
            Some(
                A::Address
                | A::Article
                | A::Aside
                | A::Blockquote
                | A::Center
                | A::Details
                | A::Dialog
                | A::Dir
                | A::Div
                | A::Dl
                | A::Fieldset
                | A::Figcaption
                | A::Figure
                | A::Footer
                | A::Header
                | A::Hgroup
                | A::Main
                | A::Menu
                | A::Nav
                | A::Ol
                | A::P
                | A::Search
                | A::Section
                | A::Summary
                | A::Ul,
            ) => {
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
            }
            Some(A::H1 | A::H2 | A::H3 | A::H4 | A::H5 | A::H6) => {
                self.pop_until(Scope::Button, &[A::P]);
                if is(self.top_atom(), &[A::H1, A::H2, A::H3, A::H4, A::H5, A::H6]) {
                    self.oe_pop()?;
                }
                self.add_element()?;
            }
            Some(A::Pre | A::Listing) => {
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
                // "The newline, if any, will be dealt with by the TextToken case."
                self.frameset_ok = false;
            }
            Some(A::Form) => {
                if self.form.is_some() && !self.oe_contains(A::Template) {
                    return Ok(true);
                }
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
                if !self.oe_contains(A::Template) {
                    self.form = Some(self.top());
                }
            }
            Some(A::Li) => {
                self.frameset_ok = false;
                self.close_list_item(&[A::Li]);
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
            }
            Some(A::Dd | A::Dt) => {
                self.frameset_ok = false;
                self.close_list_item(&[A::Dd, A::Dt]);
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
            }
            Some(A::Plaintext) => {
                self.pop_until(Scope::Button, &[A::P]);
                self.add_element()?;
            }
            Some(A::Button) => {
                if self.element_in_scope(Scope::Default, &[A::Button]) {
                    self.generate_implied_end_tags(&[]);
                    self.pop_until(Scope::Default, &[A::Button]);
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                self.frameset_ok = false;
            }
            Some(A::A) => {
                let mut i = self.afe.len();
                while i > 0 && !self.is_marker(self.afe[i - 1]) {
                    i -= 1;
                    let n = self.afe[i];
                    if self.n(n).node_type == NodeType::Element && self.n(n).data_atom == Some(A::A)
                    {
                        self.in_body_end_tag_formatting(A::A, "a")?;
                        remove_from(&mut self.oe, n);
                        remove_from(&mut self.afe, n);
                        break;
                    }
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_formatting_element()?;
            }
            Some(
                A::B
                | A::Big
                | A::Code
                | A::Em
                | A::Font
                | A::I
                | A::S
                | A::Small
                | A::Strike
                | A::Strong
                | A::Tt
                | A::U,
            ) => {
                self.reconstruct_active_formatting_elements()?;
                self.add_formatting_element()?;
            }
            Some(A::Nobr) => {
                self.reconstruct_active_formatting_elements()?;
                if self.element_in_scope(Scope::Default, &[A::Nobr]) {
                    self.in_body_end_tag_formatting(A::Nobr, "nobr")?;
                    self.reconstruct_active_formatting_elements()?;
                }
                self.add_formatting_element()?;
            }
            Some(A::Applet | A::Marquee | A::Object) => {
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                self.afe.push(self.marker);
                self.frameset_ok = false;
            }
            Some(A::Table) => {
                if !self.quirks {
                    self.pop_until(Scope::Button, &[A::P]);
                }
                self.add_element()?;
                self.frameset_ok = false;
                self.im = Some(Im::InTable);
                return Ok(true);
            }
            Some(A::Area | A::Br | A::Embed | A::Img | A::Keygen | A::Wbr) => {
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                self.oe_pop()?;
                self.acknowledge_self_closing_tag();
                self.frameset_ok = false;
            }
            Some(A::Input) => {
                self.pop_until(Scope::Default, &[A::Select]);
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                self.oe_pop()?;
                self.acknowledge_self_closing_tag();
                if self.is_hidden_input() {
                    // "Skip setting framesetOK = false"
                    return Ok(true);
                }
                self.frameset_ok = false;
            }
            Some(A::Param | A::Source | A::Track) => {
                self.add_element()?;
                self.oe_pop()?;
                self.acknowledge_self_closing_tag();
            }
            Some(A::Hr) => {
                if self.element_in_scope(Scope::Button, &[A::P]) {
                    self.generate_implied_end_tags(&["p"]);
                    self.pop_until(Scope::Default, &[A::P]);
                }
                if self.element_in_scope(Scope::Default, &[A::Select]) {
                    self.generate_implied_end_tags(&[]);
                }
                self.add_element()?;
                self.oe_pop()?;
                self.acknowledge_self_closing_tag();
                self.frameset_ok = false;
            }
            Some(A::Image) => {
                self.tok.data_atom = Some(A::Img);
                self.tok.data = A::Img.as_str().to_owned();
                return Ok(false);
            }
            Some(A::Textarea) => {
                self.add_element()?;
                self.set_original_im()?;
                self.frameset_ok = false;
                self.im = Some(Im::Text);
            }
            Some(A::Xmp) => {
                self.pop_until(Scope::Button, &[A::P]);
                self.reconstruct_active_formatting_elements()?;
                self.frameset_ok = false;
                self.parse_generic_raw_text_element()?;
            }
            Some(A::Iframe) => {
                self.frameset_ok = false;
                self.parse_generic_raw_text_element()?;
            }
            Some(A::Noembed) => self.parse_generic_raw_text_element()?,
            Some(A::Noscript) => {
                if self.scripting {
                    self.parse_generic_raw_text_element()?;
                    return Ok(true);
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                // "Don't let the tokenizer go into raw text mode when scripting is disabled."
                self.tokenizer.next_is_not_raw_text();
            }
            Some(A::Select) => {
                if self.pop_until(Scope::Default, &[A::Select]) {
                    return Ok(true);
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
                self.frameset_ok = false;
                return Ok(true);
            }
            Some(A::Option) => {
                if self.element_in_scope(Scope::Default, &[A::Select]) {
                    self.generate_implied_end_tags(&["optgroup"]);
                } else if self.top_atom() == Some(A::Option) {
                    self.oe_pop()?;
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
            }
            Some(A::Optgroup) => {
                if self.element_in_scope(Scope::Default, &[A::Select]) {
                    self.generate_implied_end_tags(&[]);
                } else if self.top_atom() == Some(A::Option) {
                    self.oe_pop()?;
                }
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
            }
            Some(A::Rb | A::Rtc) => {
                if self.element_in_scope(Scope::Default, &[A::Ruby]) {
                    self.generate_implied_end_tags(&[]);
                }
                self.add_element()?;
            }
            Some(A::Rp | A::Rt) => {
                if self.element_in_scope(Scope::Default, &[A::Ruby]) {
                    self.generate_implied_end_tags(&["rtc"]);
                }
                self.add_element()?;
            }
            Some(A::Math | A::Svg) => {
                self.reconstruct_active_formatting_elements()?;
                if atom == Some(A::Math) {
                    adjust_attribute_names(&mut self.tok.attr, mathml_attribute_adjustment);
                } else {
                    adjust_attribute_names(&mut self.tok.attr, svg_attribute_adjustment);
                }
                adjust_foreign_attributes(&mut self.tok.attr);
                self.add_element()?;
                let top = self.top();
                let ns = self.tok.data.clone();
                self.nm(top).namespace = ns;
                if self.has_self_closing_token {
                    self.oe_pop()?;
                    self.acknowledge_self_closing_tag();
                }
                return Ok(true);
            }
            Some(
                A::Caption
                | A::Col
                | A::Colgroup
                | A::Frame
                | A::Head
                | A::Tbody
                | A::Td
                | A::Tfoot
                | A::Th
                | A::Thead
                | A::Tr,
            ) => {
                // "Ignore the token."
            }
            _ => {
                self.reconstruct_active_formatting_elements()?;
                self.add_element()?;
            }
        }
        Ok(true)
    }

    /// The `<li>` / `<dd>` / `<dt>` stack walk of `inBodyIM` (parse.go:946 and 962).
    fn close_list_item(&mut self, closes: &[Atom]) {
        for i in (0..self.oe.len()).rev() {
            let node = self.n(self.oe[i]);
            if is(node.data_atom, closes) {
                self.oe.truncate(i);
            } else if is(node.data_atom, &[A::Address, A::Div, A::P]) || !is_special_element(node) {
                continue;
            }
            break;
        }
    }

    /// Whether the current token has a `type` attribute equal (under case folding) to `hidden`.
    fn is_hidden_input(&self) -> bool {
        self.tok
            .attr
            .iter()
            .any(|t| t.key == "type" && equal_fold(&t.val, "hidden"))
    }

    /// The `EndTagToken` arm of `inBodyIM` (parse.go:1144).
    fn in_body_end_tag(&mut self) -> R<bool> {
        let atom = self.tok.data_atom;
        match atom {
            Some(A::Body) => {
                if self.element_in_scope(Scope::Default, &[A::Body]) {
                    self.im = Some(Im::AfterBody);
                }
            }
            Some(A::Html) => {
                if self.element_in_scope(Scope::Default, &[A::Body]) {
                    self.parse_implied_token(TokenType::EndTag, A::Body)?;
                    return Ok(false);
                }
                return Ok(true);
            }
            Some(
                a @ (A::Address
                | A::Article
                | A::Aside
                | A::Blockquote
                | A::Button
                | A::Center
                | A::Details
                | A::Dialog
                | A::Dir
                | A::Div
                | A::Dl
                | A::Fieldset
                | A::Figcaption
                | A::Figure
                | A::Footer
                | A::Header
                | A::Hgroup
                | A::Listing
                | A::Main
                | A::Menu
                | A::Nav
                | A::Ol
                | A::Pre
                | A::Search
                | A::Section
                | A::Select
                | A::Summary
                | A::Ul),
            ) => {
                if !self.element_in_scope(Scope::Default, &[a]) {
                    return Ok(true);
                }
                self.generate_implied_end_tags(&[]);
                self.pop_until(Scope::Default, &[a]);
            }
            Some(A::Form) => {
                if self.oe_contains(A::Template) {
                    let Some(i) = self.index_of_element_in_scope(Scope::Default, &[A::Form]) else {
                        return Ok(true);
                    };
                    self.generate_implied_end_tags(&[]);
                    if self.n(self.oe_at(i as isize)?).data_atom != Some(A::Form) {
                        return Ok(true);
                    }
                    self.pop_until(Scope::Default, &[A::Form]);
                } else {
                    let node = self.form.take();
                    let i = self.index_of_element_in_scope(Scope::Default, &[A::Form]);
                    let (Some(node), Some(i)) = (node, i) else {
                        return Ok(true);
                    };
                    if self.oe[i] != node {
                        return Ok(true);
                    }
                    self.generate_implied_end_tags(&[]);
                    remove_from(&mut self.oe, node);
                }
            }
            Some(A::P) => {
                if !self.element_in_scope(Scope::Button, &[A::P]) {
                    self.parse_implied_token(TokenType::StartTag, A::P)?;
                }
                self.pop_until(Scope::Button, &[A::P]);
            }
            Some(A::Li) => {
                self.pop_until(Scope::ListItem, &[A::Li]);
            }
            Some(a @ (A::Dd | A::Dt)) => {
                self.pop_until(Scope::Default, &[a]);
            }
            Some(A::H1 | A::H2 | A::H3 | A::H4 | A::H5 | A::H6) => {
                self.pop_until(Scope::Default, &[A::H1, A::H2, A::H3, A::H4, A::H5, A::H6]);
            }
            Some(
                a @ (A::A
                | A::B
                | A::Big
                | A::Code
                | A::Em
                | A::Font
                | A::I
                | A::Nobr
                | A::S
                | A::Small
                | A::Strike
                | A::Strong
                | A::Tt
                | A::U),
            ) => {
                let name = self.tok.data.clone();
                self.in_body_end_tag_formatting(a, &name)?;
            }
            Some(a @ (A::Applet | A::Marquee | A::Object)) => {
                if self.pop_until(Scope::Default, &[a]) {
                    self.clear_active_formatting_elements()?;
                }
            }
            Some(A::Br) => {
                self.tok.token_type = TokenType::StartTag;
                return Ok(false);
            }
            Some(A::Template) => return self.in_head_im(),
            _ => {
                let name = self.tok.data.clone();
                self.in_body_end_tag_other(atom, &name);
            }
        }
        Ok(true)
    }

    /// Port of `inBodyEndTagFormatting` (parse.go:1260), the adoption agency algorithm.
    fn in_body_end_tag_formatting(&mut self, tag_atom: Atom, tag_name: &str) -> R<()> {
        // Steps 1-2.
        let current = self.oe_top_deref()?;
        if self.n(current).data == tag_name && self.afe_index(current) == -1 {
            self.oe_pop()?;
            return Ok(());
        }

        // Steps 3-5. The outer loop.
        for _ in 0..8 {
            // Step 6. Find the formatting element.
            let mut formatting_element = None;
            for j in (0..self.afe.len()).rev() {
                let e = self.afe[j];
                if self.is_marker(e) {
                    break;
                }
                if self.n(e).data_atom == Some(tag_atom) {
                    formatting_element = Some(e);
                    break;
                }
            }
            let Some(fe) = formatting_element else {
                self.in_body_end_tag_other(Some(tag_atom), tag_name);
                return Ok(());
            };

            // Step 7.
            let fe_index = self.oe_index(fe);
            if fe_index == -1 {
                remove_from(&mut self.afe, fe);
                return Ok(());
            }
            // Step 8.
            if !self.element_in_scope(Scope::Default, &[tag_atom]) {
                return Ok(());
            }

            // Steps 10-11. Find the furthest block.
            let furthest_block = self.oe[fe_index as usize..]
                .iter()
                .copied()
                .find(|&e| is_special_element(self.n(e)));
            let Some(furthest_block) = furthest_block else {
                let mut e = self.oe_pop()?;
                while e != fe {
                    e = self.oe_pop()?;
                }
                remove_from(&mut self.afe, e);
                return Ok(());
            };

            // Steps 12-13.
            let common_ancestor = self.oe_at(fe_index - 1)?;
            let mut bookmark = self.afe_index(fe);

            // Step 14. The inner loop.
            let mut last_node = furthest_block;
            let mut node;
            let mut x = self.oe_index(furthest_block);
            let mut j = 0;
            loop {
                j += 1;
                x -= 1;
                node = self.oe_at(x)?;
                // Step 14.4.
                if node == fe {
                    break;
                }
                // Step 14.5.
                let ni = self.afe_index(node);
                if j > 3 && ni > -1 {
                    remove_from(&mut self.afe, node);
                    if ni <= bookmark {
                        bookmark -= 1;
                    }
                    continue;
                }
                // Step 14.6.
                if self.afe_index(node) == -1 {
                    remove_from(&mut self.oe, node);
                    continue;
                }
                // Step 14.7.
                let clone = self.doc.clone_node(node);
                let ai = self.afe_index(node) as usize;
                self.afe[ai] = clone;
                let oi = self.oe_index(node) as usize;
                self.oe[oi] = clone;
                node = clone;
                // Step 14.8.
                if last_node == furthest_block {
                    bookmark = self.afe_index(node) + 1;
                }
                // Step 14.9.
                if let Some(parent) = self.n(last_node).parent {
                    self.doc.remove_child(parent, last_node)?;
                }
                self.doc.append_child(node, last_node)?;
                // Step 14.10.
                last_node = node;
            }

            // Step 15.
            if let Some(parent) = self.n(last_node).parent {
                self.doc.remove_child(parent, last_node)?;
            }
            if is(
                self.n(common_ancestor).data_atom,
                &[A::Table, A::Tbody, A::Tfoot, A::Thead, A::Tr],
            ) {
                self.foster_parent(last_node)?;
            } else {
                self.doc.append_child(common_ancestor, last_node)?;
            }

            // Steps 16-18.
            let clone = self.doc.clone_node(fe);
            self.doc.reparent_children(clone, furthest_block)?;
            self.doc.append_child(furthest_block, clone)?;

            // Step 19.
            let old_loc = self.afe_index(fe);
            if old_loc != -1 && old_loc < bookmark {
                bookmark -= 1;
            }
            remove_from(&mut self.afe, fe);
            insert_at(&mut self.afe, bookmark, clone)?;

            // Step 20.
            remove_from(&mut self.oe, fe);
            let at = self.oe_index(furthest_block) + 1;
            insert_at(&mut self.oe, at, clone)?;
        }
        Ok(())
    }

    /// Port of `inBodyEndTagOther` (parse.go:1425).
    fn in_body_end_tag_other(&mut self, tag_atom: Option<Atom>, tag_name: &str) {
        for i in (0..self.oe.len()).rev() {
            let n = self.n(self.oe[i]);
            // "The if condition here is equivalent to (p.oe[i].Data == tagName)."
            if n.namespace.is_empty()
                && n.data_atom == tag_atom
                && (tag_atom.is_some() || n.data == tag_name)
            {
                self.oe.truncate(i);
                break;
            }
            if is_special_element(n) {
                break;
            }
        }
    }

    /// Port of `textIM` (parse.go:1445).
    fn text_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Error => {
                self.oe_pop()?;
            }
            TokenType::Text => {
                let n = self.oe_top_deref()?;
                let mut d: &str = &self.tok.data;
                if self.n(n).data_atom == Some(A::Textarea) && self.n(n).first_child.is_none() {
                    // "Ignore a newline at the start of a <textarea> block."
                    d = d.strip_prefix('\r').unwrap_or(d);
                    d = d.strip_prefix('\n').unwrap_or(d);
                }
                if d.is_empty() {
                    return Ok(true);
                }
                let d = d.to_owned();
                self.add_text(&d)?;
                return Ok(true);
            }
            TokenType::EndTag => {
                self.oe_pop()?;
            }
            _ => {}
        }
        self.im = self.original_im.take();
        Ok(self.tok.token_type == TokenType::EndTag)
    }

    /// Port of `inTableIM` (parse.go:1476).
    fn in_table_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                self.tok.data = self.tok.data.replace('\0', "");
                let top = self.oe_top_deref()?;
                if is(
                    self.n(top).data_atom,
                    &[A::Table, A::Tbody, A::Tfoot, A::Thead, A::Tr],
                ) && self.tok.data.trim_matches(WHITESPACE).is_empty()
                {
                    let data = self.tok.data.clone();
                    self.add_text(&data)?;
                    return Ok(true);
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Caption) => {
                    self.clear_stack_to_context(ContextScope::Table);
                    self.afe.push(self.marker);
                    self.add_element()?;
                    self.im = Some(Im::InCaption);
                    return Ok(true);
                }
                Some(A::Colgroup) => {
                    self.clear_stack_to_context(ContextScope::Table);
                    self.add_element()?;
                    self.im = Some(Im::InColumnGroup);
                    return Ok(true);
                }
                Some(A::Col) => {
                    self.parse_implied_token(TokenType::StartTag, A::Colgroup)?;
                    return Ok(false);
                }
                Some(A::Tbody | A::Tfoot | A::Thead) => {
                    self.clear_stack_to_context(ContextScope::Table);
                    self.add_element()?;
                    self.im = Some(Im::InTableBody);
                    return Ok(true);
                }
                Some(A::Td | A::Th | A::Tr) => {
                    self.parse_implied_token(TokenType::StartTag, A::Tbody)?;
                    return Ok(false);
                }
                Some(A::Table) => {
                    if self.pop_until(Scope::Table, &[A::Table]) {
                        self.reset_insertion_mode();
                        return Ok(false);
                    }
                    return Ok(true);
                }
                Some(A::Style | A::Script | A::Template) => return self.in_head_im(),
                Some(A::Input) => {
                    if self.is_hidden_input() {
                        self.add_element()?;
                        self.oe_pop()?;
                        return Ok(true);
                    }
                    // "Otherwise drop down to the default action."
                }
                Some(A::Form) => {
                    if self.oe_contains(A::Template) || self.form.is_some() {
                        return Ok(true);
                    }
                    self.add_element()?;
                    self.form = Some(self.oe_pop()?);
                }
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Table) => {
                    if self.pop_until(Scope::Table, &[A::Table]) {
                        self.reset_insertion_mode();
                    }
                    return Ok(true);
                }
                Some(
                    A::Body
                    | A::Caption
                    | A::Col
                    | A::Colgroup
                    | A::Html
                    | A::Tbody
                    | A::Td
                    | A::Tfoot
                    | A::Th
                    | A::Thead
                    | A::Tr,
                ) => return Ok(true),
                Some(A::Template) => return self.in_head_im(),
                _ => {}
            },
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            TokenType::Doctype => return Ok(true),
            TokenType::Error => return self.in_body_im(),
            TokenType::SelfClosingTag => {}
        }
        self.foster_parenting = true;
        // `defer func() { p.fosterParenting = false }()`
        let r = self.in_body_im();
        self.foster_parenting = false;
        r
    }

    /// Port of `inCaptionIM` (parse.go:1545).
    fn in_caption_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::StartTag => {
                if is(
                    self.tok.data_atom,
                    &[
                        A::Caption,
                        A::Col,
                        A::Colgroup,
                        A::Tbody,
                        A::Td,
                        A::Tfoot,
                        A::Thead,
                        A::Tr,
                    ],
                ) {
                    if !self.pop_until(Scope::Table, &[A::Caption]) {
                        return Ok(true);
                    }
                    self.clear_active_formatting_elements()?;
                    self.im = Some(Im::InTable);
                    return Ok(false);
                }
            }
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Caption) => {
                    if self.pop_until(Scope::Table, &[A::Caption]) {
                        self.clear_active_formatting_elements()?;
                        self.im = Some(Im::InTable);
                    }
                    return Ok(true);
                }
                Some(A::Table) => {
                    if !self.pop_until(Scope::Table, &[A::Caption]) {
                        return Ok(true);
                    }
                    self.clear_active_formatting_elements()?;
                    self.im = Some(Im::InTable);
                    return Ok(false);
                }
                Some(
                    A::Body
                    | A::Col
                    | A::Colgroup
                    | A::Html
                    | A::Tbody
                    | A::Td
                    | A::Tfoot
                    | A::Th
                    | A::Thead
                    | A::Tr,
                ) => return Ok(true),
                _ => {}
            },
            _ => {}
        }
        self.in_body_im()
    }

    /// Port of `inColumnGroupIM` (parse.go:1582).
    fn in_column_group_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                if self.split_leading_whitespace()? {
                    return Ok(true);
                }
            }
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            TokenType::Doctype => return Ok(true),
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Col) => {
                    self.add_element()?;
                    self.oe_pop()?;
                    self.acknowledge_self_closing_tag();
                    return Ok(true);
                }
                Some(A::Template) => return self.in_head_im(),
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Colgroup) => {
                    let top = self.oe_top_deref()?;
                    if self.n(top).data_atom == Some(A::Colgroup) {
                        self.oe_pop()?;
                        self.im = Some(Im::InTable);
                    }
                    return Ok(true);
                }
                Some(A::Col) => return Ok(true),
                Some(A::Template) => return self.in_head_im(),
                _ => {}
            },
            TokenType::Error => return self.in_body_im(),
            TokenType::SelfClosingTag => {}
        }
        let top = self.oe_top_deref()?;
        if self.n(top).data_atom != Some(A::Colgroup) {
            return Ok(true);
        }
        self.oe_pop()?;
        self.im = Some(Im::InTable);
        Ok(false)
    }

    /// Port of `inTableBodyIM` (parse.go:1641).
    fn in_table_body_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Tr) => {
                    self.clear_stack_to_context(ContextScope::TableBody);
                    self.add_element()?;
                    self.im = Some(Im::InRow);
                    return Ok(true);
                }
                Some(A::Td | A::Th) => {
                    self.parse_implied_token(TokenType::StartTag, A::Tr)?;
                    return Ok(false);
                }
                Some(A::Caption | A::Col | A::Colgroup | A::Tbody | A::Tfoot | A::Thead) => {
                    if self.pop_until(Scope::Table, &[A::Tbody, A::Thead, A::Tfoot]) {
                        self.im = Some(Im::InTable);
                        return Ok(false);
                    }
                    return Ok(true);
                }
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(a @ (A::Tbody | A::Tfoot | A::Thead)) => {
                    if self.element_in_scope(Scope::Table, &[a]) {
                        self.clear_stack_to_context(ContextScope::TableBody);
                        self.oe_pop()?;
                        self.im = Some(Im::InTable);
                    }
                    return Ok(true);
                }
                Some(A::Table) => {
                    if self.pop_until(Scope::Table, &[A::Tbody, A::Thead, A::Tfoot]) {
                        self.im = Some(Im::InTable);
                        return Ok(false);
                    }
                    return Ok(true);
                }
                Some(
                    A::Body | A::Caption | A::Col | A::Colgroup | A::Html | A::Td | A::Th | A::Tr,
                ) => return Ok(true),
                _ => {}
            },
            TokenType::Comment => {
                self.add_comment()?;
                return Ok(true);
            }
            _ => {}
        }
        self.in_table_im()
    }

    /// Port of `inRowIM` (parse.go:1695).
    fn in_row_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Td | A::Th) => {
                    self.clear_stack_to_context(ContextScope::TableRow);
                    self.add_element()?;
                    self.afe.push(self.marker);
                    self.im = Some(Im::InCell);
                    return Ok(true);
                }
                Some(
                    A::Caption | A::Col | A::Colgroup | A::Tbody | A::Tfoot | A::Thead | A::Tr,
                ) => {
                    if self.element_in_scope(Scope::Table, &[A::Tr]) {
                        self.clear_stack_to_context(ContextScope::TableRow);
                        self.oe_pop()?;
                        self.im = Some(Im::InTableBody);
                        return Ok(false);
                    }
                    return Ok(true);
                }
                _ => {}
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Tr) => {
                    if self.element_in_scope(Scope::Table, &[A::Tr]) {
                        self.clear_stack_to_context(ContextScope::TableRow);
                        self.oe_pop()?;
                        self.im = Some(Im::InTableBody);
                    }
                    return Ok(true);
                }
                Some(A::Table) => {
                    if self.element_in_scope(Scope::Table, &[A::Tr]) {
                        self.clear_stack_to_context(ContextScope::TableRow);
                        self.oe_pop()?;
                        self.im = Some(Im::InTableBody);
                        return Ok(false);
                    }
                    return Ok(true);
                }
                Some(a @ (A::Tbody | A::Tfoot | A::Thead)) => {
                    if self.element_in_scope(Scope::Table, &[a])
                        && self.element_in_scope(Scope::Table, &[A::Tr])
                    {
                        self.clear_stack_to_context(ContextScope::TableRow);
                        self.oe_pop()?;
                        self.im = Some(Im::InTableBody);
                        return Ok(false);
                    }
                    return Ok(true);
                }
                Some(A::Body | A::Caption | A::Col | A::Colgroup | A::Html | A::Td | A::Th) => {
                    return Ok(true);
                }
                _ => {}
            },
            _ => {}
        }
        self.in_table_im()
    }

    /// Port of `inCellIM` (parse.go:1759).
    fn in_cell_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::StartTag => {
                if is(
                    self.tok.data_atom,
                    &[
                        A::Caption,
                        A::Col,
                        A::Colgroup,
                        A::Tbody,
                        A::Td,
                        A::Tfoot,
                        A::Th,
                        A::Thead,
                        A::Tr,
                    ],
                ) {
                    if self.pop_until(Scope::Table, &[A::Td, A::Th]) {
                        // "Close the cell and reprocess."
                        self.clear_active_formatting_elements()?;
                        self.im = Some(Im::InRow);
                        return Ok(false);
                    }
                    return Ok(true);
                }
            }
            TokenType::EndTag => match self.tok.data_atom {
                Some(a @ (A::Td | A::Th)) => {
                    if !self.pop_until(Scope::Table, &[a]) {
                        return Ok(true);
                    }
                    self.clear_active_formatting_elements()?;
                    self.im = Some(Im::InRow);
                    return Ok(true);
                }
                Some(A::Body | A::Caption | A::Col | A::Colgroup | A::Html) => return Ok(true),
                Some(a @ (A::Table | A::Tbody | A::Tfoot | A::Thead | A::Tr)) => {
                    if !self.element_in_scope(Scope::Table, &[a]) {
                        return Ok(true);
                    }
                    // "Close the cell and reprocess."
                    if self.pop_until(Scope::Table, &[A::Td, A::Th]) {
                        self.clear_active_formatting_elements()?;
                    }
                    self.im = Some(Im::InRow);
                    return Ok(false);
                }
                _ => {}
            },
            _ => {}
        }
        self.in_body_im()
    }

    /// Switch the top of the template stack to `im` and reprocess in it.
    fn switch_template_mode(&mut self, im: Im) -> R<bool> {
        if self.template_stack.pop().is_none() {
            return Err(out_of_range(-1, 0));
        }
        self.template_stack.push(im);
        self.im = Some(im);
        Ok(false)
    }

    /// Port of `inTemplateIM` (parse.go:1806).
    fn in_template_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text | TokenType::Comment | TokenType::Doctype => self.in_body_im(),
            TokenType::StartTag => match self.tok.data_atom {
                Some(
                    A::Base
                    | A::Basefont
                    | A::Bgsound
                    | A::Link
                    | A::Meta
                    | A::Noframes
                    | A::Script
                    | A::Style
                    | A::Template
                    | A::Title,
                ) => self.in_head_im(),
                Some(A::Caption | A::Colgroup | A::Tbody | A::Tfoot | A::Thead) => {
                    self.switch_template_mode(Im::InTable)
                }
                Some(A::Col) => self.switch_template_mode(Im::InColumnGroup),
                Some(A::Tr) => self.switch_template_mode(Im::InTableBody),
                Some(A::Td | A::Th) => self.switch_template_mode(Im::InRow),
                _ => self.switch_template_mode(Im::InBody),
            },
            TokenType::EndTag => match self.tok.data_atom {
                Some(A::Template) => self.in_head_im(),
                _ => Ok(true),
            },
            TokenType::Error => {
                if !self.oe_contains(A::Template) {
                    return Ok(true);
                }
                self.close_template()?;
                Ok(false)
            }
            TokenType::SelfClosingTag => Ok(false),
        }
    }

    /// Port of `afterBodyIM` (parse.go:1873).
    fn after_body_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Error => return Ok(true),
            TokenType::Text => {
                if trim_left_ws(&self.tok.data).is_empty() {
                    return self.in_body_im();
                }
            }
            TokenType::StartTag => {
                if self.tok.data_atom == Some(A::Html) {
                    return self.in_body_im();
                }
            }
            TokenType::EndTag => {
                if self.tok.data_atom == Some(A::Html) {
                    self.im = Some(Im::AfterAfterBody);
                    return Ok(true);
                }
            }
            TokenType::Comment => {
                // "The comment is attached to the <html> element."
                let html = self.oe.first().copied();
                let Some(html) = html.filter(|&h| self.n(h).data_atom == Some(A::Html)) else {
                    return Err(Panic(
                        "html: bad parser state: <html> element not found, in the after-body insertion mode"
                            .to_owned(),
                    ));
                };
                let c = self.new_comment();
                self.doc.append_child(html, c)?;
                return Ok(true);
            }
            _ => {}
        }
        self.im = Some(Im::InBody);
        Ok(false)
    }

    /// "Ignore all text but whitespace" (parse.go:1920): `strings.Map` keeping the five
    /// whitespace characters.
    fn whitespace_only(&self) -> String {
        self.tok
            .data
            .chars()
            .filter(|c| matches!(c, ' ' | '\t' | '\n' | '\u{0c}' | '\r'))
            .collect()
    }

    /// Port of `inFramesetIM` (parse.go:1911).
    fn in_frameset_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Comment => self.add_comment()?,
            TokenType::Text => {
                let s = self.whitespace_only();
                if !s.is_empty() {
                    self.add_text(&s)?;
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Frameset) => self.add_element()?,
                Some(A::Frame) => {
                    self.add_element()?;
                    self.oe_pop()?;
                    self.acknowledge_self_closing_tag();
                }
                Some(A::Noframes) => return self.in_head_im(),
                _ => {}
            },
            TokenType::EndTag if self.tok.data_atom == Some(A::Frameset) => {
                let top = self.oe_top_deref()?;
                if self.n(top).data_atom != Some(A::Html) {
                    self.oe_pop()?;
                    let top = self.oe_top_deref()?;
                    if self.n(top).data_atom != Some(A::Frameset) {
                        self.im = Some(Im::AfterFrameset);
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
        Ok(true)
    }

    /// Port of `afterFramesetIM` (parse.go:1961).
    fn after_frameset_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Comment => self.add_comment()?,
            TokenType::Text => {
                let s = self.whitespace_only();
                if !s.is_empty() {
                    self.add_text(&s)?;
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Noframes) => return self.in_head_im(),
                _ => {}
            },
            TokenType::EndTag if self.tok.data_atom == Some(A::Html) => {
                self.im = Some(Im::AfterAfterFrameset);
                return Ok(true);
            }
            _ => {}
        }
        Ok(true)
    }

    /// Port of `afterAfterBodyIM` (parse.go:1998).
    fn after_after_body_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Error => return Ok(true),
            TokenType::Text => {
                if trim_left_ws(&self.tok.data).is_empty() {
                    return self.in_body_im();
                }
            }
            TokenType::StartTag => {
                if self.tok.data_atom == Some(A::Html) {
                    return self.in_body_im();
                }
            }
            TokenType::Comment => {
                let c = self.new_comment();
                let root = self.doc.root();
                self.doc.append_child(root, c)?;
                return Ok(true);
            }
            TokenType::Doctype => return self.in_body_im(),
            _ => {}
        }
        self.im = Some(Im::InBody);
        Ok(false)
    }

    /// Port of `afterAfterFramesetIM` (parse.go:2022).
    fn after_after_frameset_im(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Comment => {
                let c = self.new_comment();
                let root = self.doc.root();
                self.doc.append_child(root, c)?;
            }
            TokenType::Text => {
                let s = self.whitespace_only();
                if !s.is_empty() {
                    self.tok.data = s;
                    return self.in_body_im();
                }
            }
            TokenType::StartTag => match self.tok.data_atom {
                Some(A::Html) => return self.in_body_im(),
                Some(A::Noframes) => return self.in_head_im(),
                _ => {}
            },
            TokenType::Doctype => return self.in_body_im(),
            _ => {}
        }
        Ok(true)
    }

    // ----- foreign content -----

    /// Port of `parseForeignContent` (parse.go:2063).
    fn parse_foreign_content(&mut self) -> R<bool> {
        match self.tok.token_type {
            TokenType::Text => {
                if self.frameset_ok {
                    self.frameset_ok = self
                        .tok
                        .data
                        .trim_start_matches([' ', '\t', '\r', '\n', '\u{0c}', '\0'])
                        .is_empty();
                }
                self.tok.data = self.tok.data.replace('\0', "\u{fffd}");
                let data = self.tok.data.clone();
                self.add_text(&data)?;
            }
            TokenType::Comment => self.add_comment()?,
            TokenType::StartTag => {
                let mut b = BREAKOUT.contains(&self.tok.data.as_str());
                if self.tok.data_atom == Some(A::Font)
                    && self
                        .tok
                        .attr
                        .iter()
                        .any(|a| matches!(a.key.as_str(), "color" | "face" | "size"))
                {
                    b = true;
                }
                if b {
                    for i in (0..self.oe.len()).rev() {
                        let n = self.n(self.oe[i]);
                        if n.namespace.is_empty()
                            || html_integration_point(n)
                            || mathml_text_integration_point(n)
                        {
                            self.oe.truncate(i + 1);
                            break;
                        }
                    }
                    return self.call_im();
                }
                let current = self.oe_top_deref()?;
                let namespace = self.n(current).namespace.clone();
                match namespace.as_str() {
                    "math" => {
                        adjust_attribute_names(&mut self.tok.attr, mathml_attribute_adjustment)
                    }
                    "svg" => {
                        // "Adjust SVG tag names. The tokenizer lower-cases tag names, but SVG
                        // wants e.g. "foreignObject" with a capital second "O"."
                        if let Some(x) = svg_tag_name_adjustment(&self.tok.data) {
                            self.tok.data_atom = lookup(x.as_bytes());
                            self.tok.data = x.to_owned();
                        }
                        adjust_attribute_names(&mut self.tok.attr, svg_attribute_adjustment);
                    }
                    _ => {
                        return Err(Panic(
                            "html: bad parser state: unexpected namespace".to_owned(),
                        ));
                    }
                }
                adjust_foreign_attributes(&mut self.tok.attr);
                self.add_element()?;
                let top = self.top();
                self.nm(top).namespace.clone_from(&namespace);
                if !namespace.is_empty() {
                    // "Don't let the tokenizer go into raw text mode in foreign content (e.g. in
                    // an SVG <title> tag)."
                    self.tokenizer.next_is_not_raw_text();
                }
                if self.has_self_closing_token {
                    self.oe_pop()?;
                    self.acknowledge_self_closing_tag();
                }
            }
            TokenType::EndTag => {
                let last = self.oe_top_deref()?;
                if equal_fold(&self.n(last).data, &self.tok.data) {
                    self.oe.pop();
                    return Ok(true);
                }
                for i in (0..self.oe.len()).rev() {
                    if equal_fold(&self.n(self.oe[i]).data, &self.tok.data) {
                        self.oe.truncate(i);
                        return Ok(true);
                    }
                    if i > 0 && self.n(self.oe[i - 1]).namespace.is_empty() {
                        break;
                    }
                }
                return self.call_im();
            }
            _ => {}
        }
        Ok(true)
    }

    /// Port of `inForeignContent` (parse.go:2123).
    fn in_foreign_content(&self) -> bool {
        let Some(n) = self.oe_top() else {
            return false;
        };
        let n = self.n(n);
        if n.namespace.is_empty() {
            return false;
        }
        let tt = self.tok.token_type;
        if mathml_text_integration_point(n) {
            if tt == TokenType::StartTag
                && self.tok.data_atom != Some(A::Mglyph)
                && self.tok.data_atom != Some(A::Malignmark)
            {
                return false;
            }
            if tt == TokenType::Text {
                return false;
            }
        }
        if n.namespace == "math"
            && n.data_atom == Some(A::AnnotationXml)
            && tt == TokenType::StartTag
            && self.tok.data_atom == Some(A::Svg)
        {
            return false;
        }
        if html_integration_point(n) && matches!(tt, TokenType::StartTag | TokenType::Text) {
            return false;
        }
        tt != TokenType::Error
    }
}

/// `nodeStack.index` (node.go:178): the top-most position of `n`, or -1.
fn index_of(s: &[NodeId], n: NodeId) -> isize {
    s.iter().rposition(|&x| x == n).map_or(-1, |i| i as isize)
}

/// `nodeStack.remove` (node.go:204): drop the top-most occurrence of `n`, if any.
fn remove_from(s: &mut Vec<NodeId>, n: NodeId) {
    if let Some(i) = s.iter().rposition(|&x| x == n) {
        s.remove(i);
    }
}

/// `nodeStack.insert` (node.go:197), which panics outside `0..=len`.
fn insert_at(s: &mut Vec<NodeId>, i: isize, n: NodeId) -> R<()> {
    match usize::try_from(i) {
        Ok(u) if u <= s.len() => {
            s.insert(u, n);
            Ok(())
        }
        _ => Err(Panic(format!(
            "runtime error: slice bounds out of range [{}:{}]",
            i + 1,
            s.len() + 1
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeId;

    /// A compact S-expression of the tree, for readable assertions.
    fn sexp(doc: &Document, id: NodeId) -> String {
        let n = &doc[id];
        let mut s = match n.node_type {
            NodeType::Document => String::new(),
            NodeType::Element if n.namespace.is_empty() => format!("<{}", n.data),
            NodeType::Element => format!("<{} {}", n.namespace, n.data),
            NodeType::Text => return format!("{:?}", n.data),
            NodeType::Comment => return format!("<!--{}-->", n.data),
            NodeType::Doctype => return format!("<!DOCTYPE {}>", n.data),
            _ => return "?".to_owned(),
        };
        for a in &n.attr {
            s.push_str(&format!(" {}={:?}", a.key, a.val));
        }
        if n.node_type == NodeType::Element {
            s.push('>');
        }
        for c in doc.children(id) {
            s.push_str(&sexp(doc, c));
        }
        s
    }

    fn tree(input: &str) -> String {
        let doc = parse(input).unwrap();
        sexp(&doc, doc.root())
    }

    #[test]
    fn implied_html_head_and_body() {
        assert_eq!(tree("x"), r#"<html><head><body>"x""#);
        assert_eq!(tree(""), "<html><head><body>");
        assert_eq!(
            tree("<!DOCTYPE html><!--c-->x"),
            r#"<!DOCTYPE html><!--c--><html><head><body>"x""#
        );
    }

    #[test]
    fn misnested_formatting_goes_through_the_adoption_agency() {
        assert_eq!(
            tree("<b><p>x</b>y</p>"),
            r#"<html><head><body><b><p><b>"x""y""#
        );
        assert_eq!(tree("<a><p>x</a>y"), r#"<html><head><body><a><p><a>"x""y""#);
    }

    #[test]
    fn text_in_a_table_is_foster_parented() {
        assert_eq!(
            tree("<table>t<tr><td>x</table>"),
            r#"<html><head><body>"t"<table><tbody><tr><td>"x""#
        );
    }

    /// Go's `addFormattingElement` sorts the element's attributes (the Noah's Ark search compares
    /// sorted slices); every other element keeps the source order.
    #[test]
    fn formatting_elements_have_sorted_attributes() {
        assert_eq!(
            tree("<b z=1 a=2><div z=1 a=2>"),
            r#"<html><head><body><b a="2" z="1"><div z="1" a="2">"#
        );
    }

    #[test]
    fn noahs_ark_keeps_three_per_family() {
        // The fourth <b> evicts the first from the list of active formatting elements, so only
        // three are reconstructed in the second paragraph; a <b> with other attributes is a
        // different family.
        assert_eq!(
            tree("<p><b><b><b><b></p><p>x"),
            r#"<html><head><body><p><b><b><b><b><p><b><b><b>"x""#
        );
        assert_eq!(
            tree("<p><b a=1><b a=1><b a=2><b a=1></p><p>x"),
            r#"<html><head><body><p><b a="1"><b a="1"><b a="2"><b a="1"><p><b a="1"><b a="1"><b a="2"><b a="1">"x""#
        );
    }

    #[test]
    fn foreign_content_is_namespaced_and_adjusted() {
        assert_eq!(
            tree("<svg viewbox=1 xlink:href=y><foreignobject><p>x"),
            r#"<html><head><body><svg svg viewBox="1" href="y"><svg foreignObject><p>"x""#
        );
        let doc = parse("<svg xlink:href=y>").unwrap();
        let svg = doc
            .children(doc.root())
            .flat_map(|h| doc.children(h).collect::<Vec<_>>())
            .flat_map(|b| doc.children(b).collect::<Vec<_>>())
            .next()
            .unwrap();
        assert_eq!(doc[svg].attr[0].namespace, "xlink");
        assert_eq!(
            tree("<svg><![CDATA[a<b]]></svg><![CDATA[c]]>"),
            r#"<html><head><body><svg svg>"a<b"<!--[CDATA[c]]-->"#
        );
    }

    #[test]
    fn nesting_past_512_is_an_error() {
        assert_eq!(
            parse(&"<div>".repeat(511)).unwrap_err().to_string(),
            "html: open stack of elements exceeds 512 nodes"
        );
        assert!(parse(&"<div>".repeat(510)).is_ok());
    }

    #[test]
    fn scripting_decides_what_noscript_holds() {
        assert_eq!(
            tree("<noscript><p>x</p></noscript>"),
            r#"<html><head><noscript>"<p>x</p>"<body>"#
        );
        let doc = parse_with_options(
            "<noscript><p>x</p></noscript>",
            ParseOptions { scripting: false },
        )
        .unwrap();
        assert_eq!(
            sexp(&doc, doc.root()),
            r#"<html><head><noscript><body><p>"x""#
        );
    }

    #[test]
    fn doctype_quirks() {
        assert!(!parse_doctype("html").1);
        assert!(parse_doctype("HTML").1);
        assert!(parse_doctype("html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\"").1);
        assert!(
            !parse_doctype("html PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\" \"http://x\"").1
        );
        assert!(
            parse_doctype(
                "html SYSTEM \"http://www.ibm.com/data/dtd/v11/IBMXHTML1-TRANSITIONAL.dtd\""
            )
            .1
        );
        assert!(parse_doctype("html junk").1);
        assert!(parse_doctype("html PUBLIC 'x").0.attr[0].val == "x");
        assert_eq!(parse_doctype("İtml").0.data, "itml");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::{Map, Value, json};

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_html2text.json"))
            .expect("the fixture is JSON")
    }

    /// Go's `NodeType` numbering (node.go:15).
    fn type_number(t: NodeType) -> u64 {
        match t {
            NodeType::Error => 0,
            NodeType::Text => 1,
            NodeType::Document => 2,
            NodeType::Element => 3,
            NodeType::Comment => 4,
            NodeType::Doctype => 5,
            NodeType::Raw => 6,
            NodeType::ScopeMarker => 7,
        }
    }

    /// The fixture's shape of a tree (behaviour_html2text.go `h2tDump`): pre-order, with depth.
    /// Iterative, like the fixture's flat list, so a 512-deep tree needs no deep stack.
    fn dump(doc: &Document) -> Vec<Value> {
        let mut out = Vec::new();
        let mut stack = vec![(doc.root(), 0u64)];
        while let Some((id, level)) = stack.pop() {
            let n = &doc[id];
            let mut m = Map::new();
            m.insert("l".into(), json!(level));
            m.insert("t".into(), json!(type_number(n.node_type)));
            if !n.data.is_empty() {
                m.insert("d".into(), json!(n.data));
            }
            if let Some(a) = n.data_atom {
                m.insert("a".into(), json!(a.as_str()));
            }
            if !n.namespace.is_empty() {
                m.insert("ns".into(), json!(n.namespace));
            }
            if !n.attr.is_empty() {
                let at: Vec<Value> = n
                    .attr
                    .iter()
                    .map(|a| json!([a.namespace, a.key, a.val]))
                    .collect();
                m.insert("at".into(), Value::Array(at));
            }
            out.push(Value::Object(m));
            let kids: Vec<NodeId> = doc.children(id).collect();
            stack.extend(kids.into_iter().rev().map(|c| (c, level + 1)));
        }
        out
    }

    /// Every input — the html5lib and Go tree-construction data, the adversarial list and the
    /// random soup — parses to Go's tree, node for node, or fails with Go's error.
    #[test]
    fn the_parse_tree_matches_go() {
        let o = oracle();
        let cases = o["parse"].as_array().expect("parse cases");
        assert!(cases.len() > 3000, "{}", cases.len());
        let mut failures = Vec::new();
        for case in cases {
            let input = case["in"].as_str().expect("input");
            let scripting = !case["noscript"].as_bool().unwrap_or(false);
            let src = case["src"].as_str().unwrap_or("?");
            match parse_with_options(input, ParseOptions { scripting }) {
                Ok(doc) => {
                    let tree = Value::Array(dump(&doc));
                    if case["err"].is_string() {
                        failures.push(format!("{src}: Go failed with {}, we parsed", case["err"]));
                    } else if tree != case["tree"] {
                        failures.push(format!("{src}: tree differs for {input:?}"));
                    }
                }
                Err(e) => {
                    if case["err"].as_str() != Some(e.0.as_str()) {
                        failures.push(format!("{src}: we failed with {e:?}, Go: {}", case["err"]));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            cases.len(),
            failures[..failures.len().min(30)].join("\n")
        );
    }
}
