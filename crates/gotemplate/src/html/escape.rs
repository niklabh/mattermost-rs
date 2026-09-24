//! Port of `html/template/escape.go`: the contextual escaper.
//!
//! The escaper walks a template's parse tree tracking the HTML/CSS/JS context at every point,
//! decides which escaping functions each `{{action}}` needs, strips comments from text, and
//! derives a copy of every template `{{template}}`-called from a non-text context. Go records the
//! edits against node pointers and applies them on `commit`; this port records them against node
//! ids ([`EscState`]) and the name space applies them by rebuilding the affected trees.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::context::{
    Attr, Context, Delim, Element, JsCtx, NodeAt, State, UrlPart, is_comment, is_in_script_literal,
};
use super::entity::unescape_string;
use super::error::{ErrNode, ErrorCode, EscError, errorf};
use super::js::is_js_type;
use super::transition::{attr_start_state, index_any, q, t_special_tag_end, transition};
use crate::exec::Common;
use crate::parse::node::{
    ActionNode, Arg, BranchNode, CommandNode, IdentifierNode, ListNode, Node, PipeNode,
    TemplateNode, TextNode, copy_list,
};
use crate::parse::parse::Tree;
use crate::strconv::quote;

/// `filterFailsafe` (escape.go:134).
pub(crate) const FILTER_FAILSAFE: &str = "ZgotmplZ";

/// `delimEnds` (escape.go:723): the bytes that end an attribute value of each delimiter kind.
pub(crate) fn delim_ends(d: Delim) -> &'static [u8] {
    match d {
        Delim::DoubleQuote => b"\"",
        Delim::SingleQuote => b"'",
        Delim::SpaceOrTagEnd => b" \t\n\x0c\r>",
        Delim::None => b"",
    }
}

/// The escaper state Go keeps in `nameSpace.esc` between executions: inferred output contexts,
/// derived templates, and the edits of an escape pass that has not been committed.
#[derive(Debug, Default)]
pub(crate) struct EscState {
    pub output: HashMap<String, Context>,
    pub derived: HashMap<String, Arc<Tree>>,
    pub called: HashSet<String>,
    pub action_edits: HashMap<u64, Vec<String>>,
    pub template_edits: HashMap<u64, String>,
    pub text_edits: HashMap<u64, String>,
}

/// `rangeContext` (escape.go:109). Shared (by `Rc`) between an escaper and the conditional
/// escapers it spawns, as Go shares the pointer.
#[derive(Debug, Default)]
struct RangeCtx {
    outer: Option<Rc<RefCell<RangeCtx>>>,
    breaks: Vec<Context>,
    continues: Vec<Context>,
}

/// The predicate `escapeListConditionally` keeps a conditional escaper's inferences by.
type Filter = dyn Fn(&EscState, &Context) -> bool;

/// `escaper` (escape.go:87) over a name space.
pub(crate) struct Escaper<'n> {
    common: &'n Common,
    html_names: &'n HashSet<String>,
    pub st: EscState,
    range_ctx: Option<Rc<RefCell<RangeCtx>>>,
}

fn err_node_list(l: &ListNode) -> ErrNode {
    ErrNode {
        pos: l.pos,
        src: l.src.clone(),
    }
}

/// Copies an error context with its error rewritten (Go mutates the shared `*Error`).
fn with_err(mut c: Context, f: impl FnOnce(&mut EscError)) -> Context {
    if let Some(e) = c.err.take() {
        let mut e2 = (*e).clone();
        f(&mut e2);
        c.err = Some(Arc::new(e2));
    }
    c
}

/// `nudge` (escape.go:453): the context after following empty-string transitions.
pub(crate) fn nudge(mut c: Context) -> Context {
    match c.state {
        State::Tag => c.state = State::AttrName,
        State::BeforeValue => {
            c.state = attr_start_state(c.attr);
            c.delim = Delim::SpaceOrTagEnd;
            c.attr = Attr::None;
        }
        State::AfterName => {
            c.state = State::AttrName;
            c.attr = Attr::None;
        }
        _ => {}
    }
    c
}

/// `join` (escape.go:471): the context after a branch whose arms end in `a` and `b`.
fn join(a: Context, b: Context, node: &ErrNode, node_name: &str) -> Context {
    if a.state == State::Error {
        return a;
    }
    if b.state == State::Error {
        return b;
    }
    if a.state == State::Dead {
        return b;
    }
    if b.state == State::Dead {
        return a;
    }
    if a.eq(&b) {
        return a;
    }
    let mut c = a.clone();
    c.url_part = b.url_part;
    if c.eq(&b) {
        c.url_part = UrlPart::Unknown;
        return c;
    }
    let mut c = a.clone();
    c.js_ctx = b.js_ctx;
    if c.eq(&b) {
        c.js_ctx = JsCtx::Unknown;
        return c;
    }
    let (c2, d2) = (nudge(a.clone()), nudge(b.clone()));
    if !(c2.eq(&a) && d2.eq(&b)) {
        let e = join(c2, d2, node, node_name);
        if e.state != State::Error {
            return e;
        }
    }
    Context::error(errorf(
        ErrorCode::BranchEnd,
        Some(node.clone()),
        0,
        format!("{{{{{node_name}}}}} branches end in different contexts: {a}, {b}"),
    ))
}

/// `joinRange` (escape.go:561).
fn join_range(c0: Context, rc: &Rc<RefCell<RangeCtx>>) -> Context {
    let (breaks, continues) = {
        let r = rc.borrow();
        (r.breaks.clone(), r.continues.clone())
    };
    let mut c0 = c0;
    for (list, what) in [(breaks, "break"), (continues, "continue")] {
        for c in list {
            let n = c.n.clone().unwrap_or(NodeAt {
                pos: 0,
                src: None,
                line: 0,
            });
            let node = ErrNode {
                pos: n.pos,
                src: n.src.clone(),
            };
            c0 = join(c0, c, &node, "range");
            if c0.state == State::Error {
                return with_err(c0, |e| {
                    e.line = n.line;
                    e.description = format!("at range loop {what}: {}", e.description);
                });
            }
        }
    }
    c0
}

/// `predefinedEscapers` (escape.go:350).
fn is_predefined_escaper(s: &str) -> bool {
    s == "html" || s == "urlquery"
}

/// `normalizeEscFn` (escape.go:383) over `equivEscapers` (escape.go:357).
fn normalize_esc_fn(e: &str) -> &str {
    match e {
        "_html_template_attrescaper"
        | "_html_template_htmlescaper"
        | "_html_template_rcdataescaper" => "html",
        "_html_template_urlescaper" | "_html_template_urlnormalizer" => "urlquery",
        other => other,
    }
}

/// `redundantFuncs` (escape.go:392).
fn redundant(a: &str, b: &str) -> bool {
    matches!(
        (a, b),
        (
            "_html_template_commentescaper",
            "_html_template_attrescaper" | "_html_template_htmlescaper"
        ) | ("_html_template_cssescaper", "_html_template_attrescaper")
            | (
                "_html_template_jsregexpescaper",
                "_html_template_attrescaper"
            )
            | ("_html_template_jsstrescaper", "_html_template_attrescaper")
            | (
                "_html_template_jstmpllitescaper",
                "_html_template_attrescaper"
            )
            | ("_html_template_urlescaper", "_html_template_urlnormalizer")
    )
}

fn first_ident(cmd: &CommandNode) -> Option<&str> {
    match cmd.args.first() {
        Some(Arg::Identifier(id)) => Some(&id.ident),
        _ => None,
    }
}

/// `appendCmd` (escape.go:416).
fn append_cmd(cmds: &mut Vec<CommandNode>, cmd: CommandNode) {
    if let (Some(last), Some(next)) = (cmds.last().and_then(first_ident), first_ident(&cmd))
        && redundant(last, next)
    {
        return;
    }
    cmds.push(cmd);
}

/// `newIdentCmd` (escape.go:428): a command of one identifier with no tree, as Go makes it.
fn new_ident_cmd(identifier: &str, pos: usize) -> CommandNode {
    CommandNode {
        pos: 0,
        src: None,
        args: vec![Arg::Identifier(IdentifierNode {
            pos,
            src: None,
            ident: identifier.to_string(),
        })],
    }
}

/// `ensurePipelineContains` (escape.go:285): appends the escapers `s` to the pipeline, merging
/// with a trailing predefined escaper (`html`, `urlquery`).
pub(crate) fn ensure_pipeline_contains(p: &mut PipeNode, s: &[String]) {
    if s.is_empty() {
        return;
    }
    let mut s: Vec<String> = s.to_vec();
    let mut pipeline_len = p.cmds.len();
    if pipeline_len > 0 {
        let last_idx = pipeline_len - 1;
        if let Some(esc) = first_ident(&p.cmds[last_idx]).map(str::to_string)
            && is_predefined_escaper(&esc)
        {
            if p.cmds.len() == 1 && p.cmds[last_idx].args.len() > 1 {
                let pos = p.cmds[last_idx].args[0].pos();
                p.cmds[last_idx].args[0] = Arg::Identifier(IdentifierNode {
                    pos,
                    src: None,
                    ident: "_eval_args_".to_string(),
                });
                let cmd = new_ident_cmd(&esc, p.pos);
                append_cmd(&mut p.cmds, cmd);
                pipeline_len += 1;
            }
            let mut dup = false;
            for e in s.iter_mut() {
                if normalize_esc_fn(&esc) == normalize_esc_fn(e) {
                    *e = esc.clone();
                    dup = true;
                }
            }
            if dup {
                pipeline_len -= 1;
            }
        }
    }
    let mut new_cmds: Vec<CommandNode> = p.cmds[..pipeline_len].to_vec();
    let mut inserted: HashSet<String> = HashSet::new();
    for cmd in &new_cmds {
        if let Some(id) = first_ident(cmd) {
            inserted.insert(normalize_esc_fn(id).to_string());
        }
    }
    for name in &s {
        if !inserted.contains(normalize_esc_fn(name)) {
            append_cmd(&mut new_cmds, new_ident_cmd(name, p.pos));
        }
    }
    p.cmds = new_cmds;
}

/// `containsSpecialScriptTag` / `escapeSpecialScriptTags` (escape.go:748): `(?i)<(script|/script|!--)`
/// rewritten to `\x3C$1`.
fn special_script_tag_at(s: &[u8], i: usize) -> Option<usize> {
    if s[i] != b'<' {
        return None;
    }
    let rest = &s[i + 1..];
    if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case(b"script") {
        return Some(6);
    }
    if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"/script") {
        return Some(7);
    }
    if rest.starts_with(b"!--") {
        return Some(3);
    }
    None
}

fn contains_special_script_tag(s: &[u8]) -> bool {
    (0..s.len()).any(|i| special_script_tag_at(s, i).is_some())
}

fn escape_special_script_tags(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < s.len() {
        if let Some(n) = special_script_tag_at(s, i) {
            out.extend_from_slice(b"\\x3C");
            out.extend_from_slice(&s[i + 1..i + 1 + n]);
            i += 1 + n;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// `contextAfterText` (escape.go:836): consumes tokens from the front of `s`.
fn context_after_text(c: Context, s: &[u8]) -> (Context, usize) {
    if c.delim == Delim::None {
        let (c1, i) = t_special_tag_end(c.clone(), s);
        if i == 0 {
            return (c1, 0);
        }
        return transition(c, &s[..i]);
    }
    let mut i = index_any(s, delim_ends(c.delim)).unwrap_or(s.len());
    if c.delim == Delim::SpaceOrTagEnd
        && let Some(j) = index_any(&s[..i], b"\"'<=`")
    {
        return (
            Context::error(errorf(
                ErrorCode::BadHtml,
                None,
                0,
                format!("{} in unquoted attr: {}", q(&s[j..j + 1]), q(&s[..i])),
            )),
            s.len(),
        );
    }
    if i == s.len() {
        let u = unescape_string(s);
        let mut u: &[u8] = &u;
        let mut c = c;
        while !u.is_empty() {
            let (c1, i1) = transition(c, u);
            c = c1;
            if i1 == 0 {
                // Go loops forever here; no transition function returns 0 on non-empty input.
                break;
            }
            u = &u[i1..];
        }
        return (c, s.len());
    }
    let mut element = c.element;
    if c.state == State::Attr
        && c.element == Element::Script
        && c.attr == Attr::ScriptType
        && !is_js_type(&String::from_utf8_lossy(&s[..i]))
    {
        element = Element::None;
    }
    if c.delim != Delim::SpaceOrTagEnd {
        i += 1;
    }
    (
        Context {
            state: State::Tag,
            element,
            ..Context::default()
        },
        i,
    )
}

impl<'n> Escaper<'n> {
    pub(crate) fn new(common: &'n Common, html_names: &'n HashSet<String>, st: EscState) -> Self {
        Escaper {
            common,
            html_names,
            st,
            range_ctx: None,
        }
    }

    /// `escaper.template` (escape.go:953): a template by (possibly mangled) name — from the text
    /// name space, else from this escaper's derived templates. The inner `None` is a template
    /// that exists without a tree.
    fn template(&self, name: &str) -> Option<Option<Arc<Tree>>> {
        if let Some(t) = self.common.tmpl.get(name) {
            return Some(t.tree.clone());
        }
        self.st.derived.get(name).map(|t| Some(t.clone()))
    }

    /// `escaper.escapeTree` (escape.go:638).
    pub(crate) fn escape_tree(
        &mut self,
        c: Context,
        node: ErrNode,
        name: &str,
        line: usize,
    ) -> (Context, String) {
        let dname = c.mangle(name);
        self.st.called.insert(dname.clone());
        if let Some(out) = self.st.output.get(&dname) {
            return (out.clone(), dname);
        }
        let t = match self.template(name) {
            None => {
                let desc = if self.html_names.contains(name) {
                    format!("{} is an incomplete or empty template", quote(name))
                } else {
                    format!("no such template {}", quote(name))
                };
                return (
                    Context::error(errorf(ErrorCode::NoSuchTemplate, Some(node), line, desc)),
                    dname,
                );
            }
            // Go dereferences the missing tree and panics; report it as incomplete.
            Some(None) => {
                return (
                    Context::error(errorf(
                        ErrorCode::NoSuchTemplate,
                        Some(node),
                        line,
                        format!("{} is an incomplete or empty template", quote(name)),
                    )),
                    dname,
                );
            }
            Some(Some(t)) => t,
        };
        let t = if dname != name {
            match self.template(&dname) {
                Some(Some(dt)) => dt,
                _ => {
                    let dt = Arc::new(Tree {
                        name: dname.clone(),
                        root: copy_list(&t.root),
                        src: None,
                    });
                    self.st.derived.insert(dname.clone(), dt.clone());
                    dt
                }
            }
        } else {
            t
        };
        (self.compute_out_ctx(c, &t), dname)
    }

    /// `escaper.computeOutCtx` (escape.go:678).
    fn compute_out_ctx(&mut self, c: Context, t: &Arc<Tree>) -> Context {
        let (mut c1, mut ok) = self.escape_template_body(c, t);
        if !ok {
            let (c2, ok2) = self.escape_template_body(c1.clone(), t);
            if ok2 {
                c1 = c2;
                ok = true;
            }
        }
        if !ok && c1.state != State::Error {
            return Context::error(errorf(
                ErrorCode::OutputContext,
                Some(err_node_list(&t.root)),
                0,
                format!("cannot compute output context for template {}", t.name),
            ));
        }
        c1
    }

    /// `escaper.escapeTemplateBody` (escape.go:700).
    fn escape_template_body(&mut self, c: Context, t: &Arc<Tree>) -> (Context, bool) {
        self.st.output.insert(t.name.clone(), c.clone());
        let name = t.name.clone();
        let assumed = c.clone();
        let filter = move |e1: &EscState, c1: &Context| -> bool {
            if c1.state == State::Error {
                // Do not update the input escaper.
                return false;
            }
            if !e1.called.contains(&name) {
                // If t is not recursively called, then c1 is an accurate output context.
                return true;
            }
            // c1 is accurate if it matches our assumed output context.
            assumed.eq(c1)
        };
        self.escape_list_conditionally(c, &t.root, Some(&filter))
    }

    /// `escaper.escapeListConditionally` (escape.go:602).
    fn escape_list_conditionally(
        &mut self,
        c: Context,
        n: &ListNode,
        filter: Option<&Filter>,
    ) -> (Context, bool) {
        let mut e1 = Escaper {
            common: self.common,
            html_names: self.html_names,
            st: EscState {
                output: self.st.output.clone(),
                ..EscState::default()
            },
            range_ctx: self.range_ctx.clone(),
        };
        let c = e1.escape_list(c, Some(n));
        let ok = filter.is_some_and(|f| f(&e1.st, &c));
        if ok {
            let st = e1.st;
            self.st.output.extend(st.output);
            self.st.derived.extend(st.derived);
            self.st.called.extend(st.called);
            self.st.action_edits.extend(st.action_edits);
            self.st.template_edits.extend(st.template_edits);
            self.st.text_edits.extend(st.text_edits);
        }
        (c, ok)
    }

    /// `escaper.escapeList` (escape.go:585).
    fn escape_list(&mut self, c: Context, n: Option<&ListNode>) -> Context {
        let Some(n) = n else {
            return c;
        };
        let mut c = c;
        for m in &n.nodes {
            c = self.escape(c, m);
            if c.state == State::Dead {
                break;
            }
        }
        c
    }

    /// `escaper.escape` (escape.go:137).
    fn escape(&mut self, c: Context, n: &Node) -> Context {
        match n {
            Node::Action(a) => self.escape_action(c, a),
            Node::Break(b) | Node::Continue(b) => {
                let mut c = c;
                c.n = Some(NodeAt {
                    pos: b.pos,
                    src: b.src.clone(),
                    line: b.line,
                });
                if let Some(rc) = &self.range_ctx {
                    let mut r = rc.borrow_mut();
                    if matches!(n, Node::Break(_)) {
                        r.breaks.push(c);
                    } else {
                        r.continues.push(c);
                    }
                }
                Context::with_state(State::Dead)
            }
            Node::Comment(_) => c,
            Node::If(b) => self.escape_branch(c, b, "if"),
            Node::Range(b) => self.escape_branch(c, b, "range"),
            Node::With(b) => self.escape_branch(c, b, "with"),
            Node::Template(t) => self.escape_template(c, t),
            Node::Text(t) => self.escape_text(c, t),
        }
    }

    /// `escaper.escapeAction` (escape.go:172).
    fn escape_action(&mut self, c: Context, n: &ActionNode) -> Context {
        if !n.pipe.decl.is_empty() {
            return c;
        }
        let mut c = nudge(c);
        let ncmds = n.pipe.cmds.len();
        for (pos, cmd) in n.pipe.cmds.iter().enumerate() {
            let Some(ident) = first_ident(cmd) else {
                continue;
            };
            if is_predefined_escaper(ident)
                && (pos < ncmds - 1
                    || (c.state == State::Attr
                        && c.delim == Delim::SpaceOrTagEnd
                        && ident == "html"))
            {
                return Context::error(errorf(
                    ErrorCode::PredefinedEscaper,
                    Some(ErrNode {
                        pos: n.pos,
                        src: n.src.clone(),
                    }),
                    n.pipe.line,
                    format!("predefined escaper {} disallowed in template", quote(ident)),
                ));
            }
        }
        let mut s: Vec<&str> = Vec::with_capacity(3);
        match c.state {
            State::Error => return c,
            State::Url
            | State::CssDqStr
            | State::CssSqStr
            | State::CssDqUrl
            | State::CssSqUrl
            | State::CssUrl => match c.url_part {
                UrlPart::None | UrlPart::PreQuery => {
                    if c.url_part == UrlPart::None {
                        s.push("_html_template_urlfilter");
                    }
                    match c.state {
                        State::CssDqStr | State::CssSqStr => s.push("_html_template_cssescaper"),
                        _ => s.push("_html_template_urlnormalizer"),
                    }
                }
                UrlPart::QueryOrFrag => s.push("_html_template_urlescaper"),
                UrlPart::Unknown => {
                    return Context::error(errorf(
                        ErrorCode::AmbigContext,
                        Some(ErrNode {
                            pos: n.pos,
                            src: n.src.clone(),
                        }),
                        n.pipe.line,
                        format!(
                            "{} appears in an ambiguous context within a URL",
                            Node::Action(n.clone())
                        ),
                    ));
                }
            },
            State::MetaContent => {}
            State::MetaContentUrl => s.push("_html_template_urlfilter"),
            State::Js => {
                s.push("_html_template_jsvalescaper");
                c.js_ctx = JsCtx::DivOp;
            }
            State::JsDqStr | State::JsSqStr => s.push("_html_template_jsstrescaper"),
            State::JsTmplLit => s.push("_html_template_jstmpllitescaper"),
            State::JsRegexp => s.push("_html_template_jsregexpescaper"),
            State::Css => s.push("_html_template_cssvaluefilter"),
            State::Text => s.push("_html_template_htmlescaper"),
            State::Rcdata => s.push("_html_template_rcdataescaper"),
            State::Attr => {}
            State::AttrName | State::Tag => {
                c.state = State::AttrName;
                s.push("_html_template_htmlnamefilter");
            }
            State::Srcset => s.push("_html_template_srcsetescaper"),
            st => {
                if is_comment(st) {
                    s.push("_html_template_commentescaper");
                }
            }
        }
        match c.delim {
            Delim::None => {}
            Delim::SpaceOrTagEnd => s.push("_html_template_nospaceescaper"),
            _ => s.push("_html_template_attrescaper"),
        }
        self.st
            .action_edits
            .insert(n.id, s.iter().map(|x| x.to_string()).collect());
        c
    }

    /// `escaper.escapeBranch` (escape.go:522).
    fn escape_branch(&mut self, c: Context, n: &BranchNode, node_name: &str) -> Context {
        let node = ErrNode {
            pos: n.pos,
            src: n.src.clone(),
        };
        let is_range = node_name == "range";
        if is_range {
            self.range_ctx = Some(Rc::new(RefCell::new(RangeCtx {
                outer: self.range_ctx.take(),
                ..RangeCtx::default()
            })));
        }
        let mut c0 = self.escape_list(c.clone(), Some(&n.list));
        if is_range {
            let rc = self.range_ctx.clone();
            if c0.state != State::Error
                && let Some(rc) = &rc
            {
                c0 = join_range(c0, rc);
            }
            self.range_ctx = rc.and_then(|r| r.borrow().outer.clone());
            if c0.state == State::Error {
                return c0;
            }
            // The body of a range can run more than once: its end context must also be a
            // valid start context.
            self.range_ctx = Some(Rc::new(RefCell::new(RangeCtx {
                outer: self.range_ctx.take(),
                ..RangeCtx::default()
            })));
            let (c1, _) = self.escape_list_conditionally(c0.clone(), &n.list, None);
            c0 = join(c0, c1, &node, node_name);
            let rc = self.range_ctx.clone();
            if c0.state == State::Error {
                self.range_ctx = rc.and_then(|r| r.borrow().outer.clone());
                return with_err(c0, |e| {
                    e.line = n.pipe.line;
                    e.description = format!("on range loop re-entry: {}", e.description);
                });
            }
            if let Some(rc) = &rc {
                c0 = join_range(c0, rc);
            }
            self.range_ctx = rc.and_then(|r| r.borrow().outer.clone());
            if c0.state == State::Error {
                return c0;
            }
        }
        let c1 = self.escape_list(c, n.else_list.as_ref());
        join(c0, c1, &node, node_name)
    }

    /// `escaper.escapeTemplate` (escape.go:628).
    fn escape_template(&mut self, c: Context, n: &TemplateNode) -> Context {
        let node = ErrNode {
            pos: n.pos,
            src: n.src.clone(),
        };
        let (c, name) = self.escape_tree(c, node, &n.name, n.line);
        if name != n.name {
            self.st.template_edits.insert(n.id, name);
        }
        c
    }

    /// `escaper.escapeText` (escape.go:759).
    fn escape_text(&mut self, c: Context, n: &TextNode) -> Context {
        let s = n.text.as_bytes();
        let mut written = 0;
        let mut i = 0;
        let mut b: Vec<u8> = Vec::new();
        let mut c = c;
        while i != s.len() {
            let (c1, nread) = context_after_text(c.clone(), &s[i..]);
            let i1 = i + nread;
            if c.state == State::Text || c.state == State::Rcdata {
                let mut end = i1;
                if c1.state != c.state {
                    for j in (i..end).rev() {
                        if s[j] == b'<' {
                            end = j;
                            break;
                        }
                    }
                }
                for j in i..end {
                    if s[j] == b'<'
                        && !(s.len() - j >= 9 && s[j..j + 9].eq_ignore_ascii_case(b"<!DOCTYPE"))
                    {
                        b.extend_from_slice(&s[written..j]);
                        b.extend_from_slice(b"&lt;");
                        written = j + 1;
                    }
                }
            } else if is_comment(c.state) && c.delim == Delim::None {
                match c.state {
                    State::JsBlockCmt => {
                        let seg = &s[written..i1];
                        let has_nl = seg.iter().any(|&x| x == b'\n' || x == b'\r')
                            || String::from_utf8_lossy(seg).contains(['\u{2028}', '\u{2029}']);
                        b.push(if has_nl { b'\n' } else { b' ' });
                    }
                    State::CssBlockCmt => b.push(b' '),
                    _ => {}
                }
                written = i1;
            }
            if c.state != c1.state && is_comment(c1.state) && c1.delim == Delim::None {
                let mut cs = i1 - 2;
                if c1.state == State::HtmlCmt || c1.state == State::JsHtmlOpenCmt {
                    cs -= 2;
                } else if c1.state == State::JsHtmlCloseCmt {
                    cs -= 1;
                }
                b.extend_from_slice(&s[written..cs]);
                written = i1;
            }
            if is_in_script_literal(c.state) && contains_special_script_tag(&s[i..i1]) {
                b.extend_from_slice(&s[written..i]);
                b.extend_from_slice(&escape_special_script_tags(&s[i..i1]));
                written = i1;
            }
            if i == i1 && c.state == c1.state {
                // Go panics with "infinite loop"; no transition reaches here on valid input.
                break;
            }
            c = c1;
            i = i1;
        }
        if written != 0 && c.state != State::Error {
            if !is_comment(c.state) || c.delim != Delim::None {
                b.extend_from_slice(&s[written..]);
            }
            self.st
                .text_edits
                .insert(n.id, String::from_utf8_lossy(&b).into_owned());
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe_of(idents: &[&str]) -> PipeNode {
        PipeNode {
            pos: 5,
            src: None,
            line: 1,
            is_assign: false,
            decl: vec![],
            cmds: idents.iter().map(|i| new_ident_cmd(i, 5)).collect(),
        }
    }

    fn names(p: &PipeNode) -> Vec<String> {
        p.cmds.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn ensure_pipeline_merges_predefined_escapers() {
        let mut p = pipe_of(&["x", "html"]);
        ensure_pipeline_contains(&mut p, &["_html_template_htmlescaper".to_string()]);
        assert_eq!(names(&p), vec!["x", "html"]);
        let mut p = pipe_of(&["x"]);
        ensure_pipeline_contains(
            &mut p,
            &[
                "_html_template_urlfilter".into(),
                "_html_template_urlnormalizer".into(),
            ],
        );
        assert_eq!(
            names(&p),
            vec![
                "x",
                "_html_template_urlfilter",
                "_html_template_urlnormalizer"
            ]
        );
        let mut p = pipe_of(&["x"]);
        ensure_pipeline_contains(
            &mut p,
            &[
                "_html_template_jsstrescaper".into(),
                "_html_template_attrescaper".into(),
            ],
        );
        assert_eq!(names(&p), vec!["x", "_html_template_jsstrescaper"]);
    }

    #[test]
    fn special_script_tags() {
        assert_eq!(
            escape_special_script_tags(b"a</SCRIPT><!--b<script"),
            b"a\\x3C/SCRIPT>\\x3C!--b\\x3Cscript"
        );
    }
}
