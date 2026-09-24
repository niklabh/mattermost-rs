//! Port of `text/template/exec.go`: executing a parse tree against a [`Value`].
//!
//! Go reports execution errors by panicking an `ExecError` out of the walk and uses two sentinel
//! panics for `{{break}}` and `{{continue}}`; this port threads [`Flow`] through `Result`
//! instead. Every error string is built as `state.errorf` builds it:
//! `template: <file>:<line>:<col>: executing "<name>" at <<node>>: <message>`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::funcs::{self, Func, Param};
use crate::parse::node::{
    Arg, BranchNode, CommandNode, ListNode, Node, NumberNode, PipeNode, Src, error_location,
};
use crate::parse::parse::Tree;
use crate::rv::{Kind, Rv, is_true};
use crate::strconv::quote;
use crate::value::Value;

/// `maxExecDepth` (exec.go:22). Go uses 100000 on amd64/arm64 and 1000 on wasm; this port uses
/// the wasm value, because 100000 nested `{{template}}` calls would overflow a Rust thread's
/// stack long before Go's growable goroutine stack notices. Only a template that recurses without
/// end can tell the difference, and it gets `exceeded maximum template depth (1000)`.
pub(crate) const MAX_EXEC_DEPTH: usize = 1000;

/// The `missingkey` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissingKey {
    /// `missingkey=default` / `missingkey=invalid`: an absent key yields no value.
    #[default]
    Invalid,
    /// `missingkey=zero`: an absent key yields the element type's zero value — for this model's
    /// `map[string]interface {}`, a nil interface.
    Zero,
    /// `missingkey=error`: an absent key stops execution.
    Error,
}

/// One entry of a template set's name space (`text/template.Template` minus its `common`).
#[derive(Debug, Clone)]
pub(crate) struct TextTmpl {
    pub name: String,
    pub tree: Option<Arc<Tree>>,
}

/// The name space templates share (`text/template.common`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Common {
    pub tmpl: HashMap<String, TextTmpl>,
    pub missing_key: MissingKey,
    /// Whether `html/template`'s escaper functions have been registered (`escaper.commit`).
    pub html_funcs: bool,
}

/// Why a walk stopped early.
pub(crate) enum Flow {
    Err(String),
    Break,
    Continue,
}

type R<T> = Result<T, Flow>;

/// The node a state is "at", for error context.
#[derive(Clone, Copy)]
enum Here<'t> {
    Node(&'t Node),
    List(&'t ListNode),
    Pipe(&'t PipeNode),
    Cmd(&'t CommandNode),
    Arg(&'t Arg),
}

impl Here<'_> {
    fn pos(&self) -> usize {
        match self {
            Here::Node(n) => n.pos(),
            Here::List(l) => l.pos,
            Here::Pipe(p) => p.pos,
            Here::Cmd(c) => c.pos,
            Here::Arg(a) => a.pos(),
        }
    }

    fn src(&self) -> Option<&Arc<Src>> {
        match self {
            Here::Node(n) => n.src(),
            Here::List(l) => l.src.as_ref(),
            Here::Pipe(p) => p.src.as_ref(),
            Here::Cmd(c) => c.src.as_ref(),
            Here::Arg(a) => a.src(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Here::Node(n) => n.to_string(),
            Here::List(l) => l.to_string(),
            Here::Pipe(p) => p.to_string(),
            Here::Cmd(c) => c.to_string(),
            Here::Arg(a) => a.to_string(),
        }
    }
}

struct State<'t, 'a> {
    common: &'t Common,
    tmpl: &'t TextTmpl,
    node: Option<Here<'t>>,
    vars: Vec<(String, Rv<'a>)>,
    depth: usize,
    out: String,
}

/// `Template.Execute` (exec.go:206) of the template `tmpl` in `common`.
pub(crate) fn execute(common: &Common, tmpl: &TextTmpl, data: &Value) -> Result<String, String> {
    let value = match data {
        Value::Nil => Rv::Invalid,
        other => Rv::borrowed(other, false),
    };
    let mut s = State {
        common,
        tmpl,
        node: None,
        vars: vec![("$".to_string(), value.clone())],
        depth: 0,
        out: String::new(),
    };
    let Some(tree) = tmpl.tree.as_deref() else {
        return Err(s.error_string(&format!(
            "{} is an incomplete or empty template",
            quote(&tmpl.name)
        )));
    };
    match s.walk(value, Node2::List(&tree.root)) {
        Ok(()) => Ok(s.out),
        Err(Flow::Err(e)) => Err(e),
        // A break or continue can only escape a range the parser proved encloses it.
        Err(Flow::Break | Flow::Continue) => Ok(s.out),
    }
}

/// The statement a walk visits: a list-held node, or a list.
#[derive(Clone, Copy)]
enum Node2<'t> {
    Node(&'t Node),
    List(&'t ListNode),
}

impl<'t, 'a> State<'t, 'a>
where
    't: 'a,
{
    fn at(&mut self, h: Here<'t>) {
        self.node = Some(h);
    }

    fn error_string(&self, msg: &str) -> String {
        let name = &self.tmpl.name;
        match &self.node {
            None => format!("template: {name}: {msg}"),
            Some(h) => {
                let fallback = self.tmpl.tree.as_ref().and_then(|t| t.src.clone());
                let src = h.src().cloned().or(fallback).unwrap_or_else(|| {
                    Arc::new(Src {
                        parse_name: String::new(),
                        text: Arc::from(""),
                    })
                });
                let location = error_location(h.pos(), &src);
                format!(
                    "template: {location}: executing {} at <{}>: {msg}",
                    quote(name),
                    h.describe()
                )
            }
        }
    }

    fn errorf<T>(&self, msg: impl AsRef<str>) -> R<T> {
        Err(Flow::Err(self.error_string(msg.as_ref())))
    }

    fn push(&mut self, name: &str, value: Rv<'a>) {
        self.vars.push((name.to_string(), value));
    }

    fn mark(&self) -> usize {
        self.vars.len()
    }

    fn pop(&mut self, mark: usize) {
        self.vars.truncate(mark);
    }

    fn set_var(&mut self, name: &str, value: Rv<'a>) -> R<()> {
        for v in self.vars.iter_mut().rev() {
            if v.0 == name {
                v.1 = value;
                return Ok(());
            }
        }
        self.errorf(format!("undefined variable: {name}"))
    }

    fn set_top_var(&mut self, n: usize, value: Rv<'a>) {
        let len = self.vars.len();
        if let Some(v) = self.vars.get_mut(len - n) {
            v.1 = value;
        }
    }

    fn var_value(&self, name: &str) -> R<Rv<'a>> {
        for v in self.vars.iter().rev() {
            if v.0 == name {
                return Ok(v.1.clone());
            }
        }
        self.errorf(format!("undefined variable: {name}"))
    }

    fn walk(&mut self, dot: Rv<'a>, node: Node2<'t>) -> R<()> {
        let node = match node {
            Node2::List(l) => {
                self.at(Here::List(l));
                for n in &l.nodes {
                    self.walk(dot.clone(), Node2::Node(n))?;
                }
                return Ok(());
            }
            Node2::Node(n) => n,
        };
        self.at(Here::Node(node));
        match node {
            Node::Action(a) => {
                let val = self.eval_pipeline(dot, Some(&a.pipe))?;
                if a.pipe.decl.is_empty() {
                    self.print_value(Here::Node(node), val)?;
                }
                Ok(())
            }
            Node::Break(_) => Err(Flow::Break),
            Node::Continue(_) => Err(Flow::Continue),
            Node::Comment(_) => Ok(()),
            Node::If(b) => self.walk_if_or_with(false, dot, b),
            Node::With(b) => self.walk_if_or_with(true, dot, b),
            Node::Range(b) => self.walk_range(dot, node, b),
            Node::Template(t) => {
                self.at(Here::Node(node));
                let Some(tmpl) = self.common.tmpl.get(&t.name) else {
                    return self.errorf(format!("template {} not defined", quote(&t.name)));
                };
                if self.depth == MAX_EXEC_DEPTH {
                    return self.errorf(format!(
                        "exceeded maximum template depth ({MAX_EXEC_DEPTH})"
                    ));
                }
                let dot = self.eval_pipeline(dot, t.pipe.as_ref())?;
                let Some(tree) = tmpl.tree.as_deref() else {
                    // Go dereferences the missing tree and panics; report it instead.
                    return self.errorf(format!(
                        "{} is an incomplete or empty template",
                        quote(&t.name)
                    ));
                };
                let saved_tmpl = self.tmpl;
                let saved_vars = std::mem::take(&mut self.vars);
                self.tmpl = tmpl;
                self.depth += 1;
                self.vars = vec![("$".to_string(), dot.clone())];
                let r = self.walk(dot, Node2::List(&tree.root));
                self.depth -= 1;
                self.tmpl = saved_tmpl;
                self.vars = saved_vars;
                r
            }
            Node::Text(t) => {
                self.out.push_str(&t.text);
                Ok(())
            }
        }
    }

    fn walk_if_or_with(&mut self, with: bool, dot: Rv<'a>, b: &'t BranchNode) -> R<()> {
        let mark = self.mark();
        let val = self.eval_pipeline(dot.clone(), Some(&b.pipe))?;
        let (truth, ok) = is_true(&val.clone().indirect_interface());
        if !ok {
            return self.errorf(format!("if/with can't use {}", fmt_rv(&val, 'v')));
        }
        if truth {
            if with {
                self.walk(val, Node2::List(&b.list))?;
            } else {
                self.walk(dot, Node2::List(&b.list))?;
            }
        } else if let Some(e) = &b.else_list {
            self.walk(dot, Node2::List(e))?;
        }
        self.pop(mark);
        Ok(())
    }

    fn one_iteration(
        &mut self,
        r: &'t BranchNode,
        mark: usize,
        index: Rv<'a>,
        elem: Rv<'a>,
    ) -> R<()> {
        let decl = &r.pipe.decl;
        if !decl.is_empty() {
            if r.pipe.is_assign {
                if decl.len() > 1 {
                    self.set_var(&decl[0].ident[0], index.clone())?;
                } else {
                    self.set_var(&decl[0].ident[0], elem.clone())?;
                }
            } else {
                self.set_top_var(1, elem.clone());
            }
        }
        if decl.len() > 1 {
            if r.pipe.is_assign {
                self.set_var(&decl[1].ident[0], elem.clone())?;
            } else {
                self.set_top_var(2, index);
            }
        }
        let res = self.walk(elem, Node2::List(&r.list));
        self.pop(mark);
        match res {
            Err(Flow::Continue) => Ok(()),
            other => other,
        }
    }

    fn walk_range(&mut self, dot: Rv<'a>, node: &'t Node, r: &'t BranchNode) -> R<()> {
        self.at(Here::Node(node));
        let outer_mark = self.mark();
        let res = self.walk_range_inner(dot, r);
        self.pop(outer_mark);
        match res {
            Err(Flow::Break) => Ok(()),
            other => other,
        }
    }

    fn walk_range_inner(&mut self, dot: Rv<'a>, r: &'t BranchNode) -> R<()> {
        let (val, _) = self.eval_pipeline(dot.clone(), Some(&r.pipe))?.indirect();
        let mark = self.mark();
        let ran = match val.kind() {
            Kind::Int => {
                if r.pipe.decl.len() > 1 {
                    return self.errorf(format!(
                        "can't use {} to iterate over more than one variable",
                        fmt_rv(&val, 'v')
                    ));
                }
                let n = match val.value() {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                };
                for i in 0..n.max(0) {
                    self.one_iteration(r, mark, Rv::Invalid, Rv::owned(Value::Int(i)))?;
                }
                n > 0
            }
            Kind::Slice => {
                let len = match val.value() {
                    Some(Value::List(l)) => l.len(),
                    _ => 0,
                };
                for i in 0..len {
                    let elem = match &val {
                        Rv::Val(c, _) => Rv::from_cow(
                            crate::rv::project(c, |x| match x {
                                Value::List(l) => &l[i],
                                other => other,
                            }),
                            true,
                        ),
                        Rv::Invalid => Rv::Invalid,
                    };
                    self.one_iteration(r, mark, Rv::owned(Value::Int(i as i64)), elem)?;
                }
                len > 0
            }
            Kind::Map => {
                let keys: Vec<String> = match val.value() {
                    Some(Value::Map(m)) => m.keys().cloned().collect(),
                    _ => vec![],
                };
                for k in &keys {
                    let elem = match &val {
                        Rv::Val(c, _) => Rv::from_cow(
                            crate::rv::project(c, |x| match x {
                                Value::Map(m) => m.get(k.as_str()).unwrap_or(x),
                                other => other,
                            }),
                            true,
                        ),
                        Rv::Invalid => Rv::Invalid,
                    };
                    self.one_iteration(r, mark, Rv::owned(Value::String(k.clone())), elem)?;
                }
                !keys.is_empty()
            }
            Kind::Invalid => false,
            _ => {
                return self.errorf(format!("range can't iterate over {}", fmt_rv(&val, 'v')));
            }
        };
        if !ran && let Some(e) = &r.else_list {
            self.walk(dot, Node2::List(e))?;
        }
        Ok(())
    }

    /// `state.evalPipeline` (exec.go:525).
    fn eval_pipeline(&mut self, dot: Rv<'a>, pipe: Option<&'t PipeNode>) -> R<Rv<'a>> {
        let Some(pipe) = pipe else {
            return Ok(Rv::Invalid);
        };
        self.at(Here::Pipe(pipe));
        let mut value: Option<Rv<'a>> = None;
        for cmd in &pipe.cmds {
            let v = self.eval_command(dot.clone(), cmd, value.take())?;
            let v = if v.kind() == Kind::Interface {
                v.indirect_interface()
            } else {
                v
            };
            value = Some(v);
        }
        let value = value.unwrap_or(Rv::Invalid);
        for variable in &pipe.decl {
            if pipe.is_assign {
                self.set_var(&variable.ident[0], value.clone())?;
            } else {
                self.push(&variable.ident[0], value.clone());
            }
        }
        Ok(value)
    }

    fn not_a_function(&self, args: &[Arg], fin: &Option<Rv<'a>>) -> R<()> {
        if args.len() > 1 || fin.is_some() {
            return self.errorf(format!("can't give argument to non-function {}", args[0]));
        }
        Ok(())
    }

    fn eval_command(
        &mut self,
        dot: Rv<'a>,
        cmd: &'t CommandNode,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        let first = &cmd.args[0];
        match first {
            Arg::Field(f) => {
                self.at(Here::Arg(first));
                return self.eval_field_chain(
                    dot.clone(),
                    dot,
                    first,
                    &f.ident,
                    Some(&cmd.args),
                    fin,
                );
            }
            Arg::Chain(_) => return self.eval_chain_node(dot, first, Some(&cmd.args), fin),
            Arg::Identifier(_) => {
                return self.eval_function(dot, first, Here::Cmd(cmd), Some(&cmd.args), fin);
            }
            Arg::Pipe(p) => {
                self.not_a_function(&cmd.args, &fin)?;
                return self.eval_pipeline(dot, Some(p));
            }
            Arg::Variable(_) => return self.eval_variable_node(dot, first, Some(&cmd.args), fin),
            _ => {}
        }
        self.at(Here::Arg(first));
        self.not_a_function(&cmd.args, &fin)?;
        match first {
            Arg::Bool(b) => Ok(Rv::owned(Value::Bool(b.value))),
            Arg::Dot(_) => Ok(dot),
            Arg::Nil(_) => self.errorf("nil is not a command"),
            Arg::Number(n) => self.ideal_constant(first, n),
            Arg::String(s) => Ok(Rv::owned(Value::String(s.text.clone()))),
            _ => self.errorf(format!(
                "can't evaluate command {}",
                quote(&first.to_string())
            )),
        }
    }

    /// `state.idealConstant` (exec.go:593).
    fn ideal_constant(&mut self, node: &'t Arg, n: &NumberNode) -> R<Rv<'a>> {
        self.at(Here::Arg(node));
        let is_hex_int = n.text.len() > 2
            && n.text.as_bytes()[0] == b'0'
            && (n.text.as_bytes()[1] == b'x' || n.text.as_bytes()[1] == b'X')
            && !n.text.contains(['p', 'P']);
        let is_rune_int = n.text.starts_with('\'');
        if n.is_float && !is_hex_int && !is_rune_int && n.text.contains(['.', 'e', 'E', 'p', 'P']) {
            return Ok(Rv::owned(Value::Float(n.float64)));
        }
        if n.is_int {
            return Ok(Rv::owned(Value::Int(n.int64)));
        }
        if n.is_uint {
            return self.errorf(format!("{} overflows int", n.text));
        }
        Ok(Rv::Invalid)
    }

    fn eval_chain_node(
        &mut self,
        dot: Rv<'a>,
        node: &'t Arg,
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        self.at(Here::Arg(node));
        let Arg::Chain(chain) = node else {
            return self.errorf("internal error: not a chain");
        };
        if chain.field.is_empty() {
            return self.errorf("internal error: no fields in evalChainNode");
        }
        if matches!(&*chain.node, Arg::Nil(_)) {
            return self.errorf(format!("indirection through explicit nil in {chain}"));
        }
        let pipe = self.eval_arg(dot.clone(), None, &chain.node)?;
        self.eval_field_chain(dot, pipe, node, &chain.field, args, fin)
    }

    fn eval_variable_node(
        &mut self,
        dot: Rv<'a>,
        node: &'t Arg,
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        self.at(Here::Arg(node));
        let Arg::Variable(v) = node else {
            return self.errorf("internal error: not a variable");
        };
        let value = self.var_value(&v.ident[0])?;
        if v.ident.len() == 1 {
            self.not_a_function(args.unwrap_or(&[]), &fin)?;
            return Ok(value);
        }
        self.eval_field_chain(dot, value, node, &v.ident[1..], args, fin)
    }

    fn eval_field_chain(
        &mut self,
        dot: Rv<'a>,
        receiver: Rv<'a>,
        node: &'t Arg,
        ident: &'t [String],
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        let n = ident.len();
        let mut receiver = receiver;
        for name in &ident[..n - 1] {
            receiver = self.eval_field(dot.clone(), name, node, None, None, receiver)?;
        }
        self.eval_field(dot, &ident[n - 1], node, args, fin, receiver)
    }

    fn eval_function(
        &mut self,
        dot: Rv<'a>,
        node: &'t Arg,
        cmd: Here<'t>,
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        self.at(Here::Arg(node));
        let Arg::Identifier(id) = node else {
            return self.errorf("internal error: not an identifier");
        };
        let Some(function) = funcs::find(&id.ident, self.common.html_funcs) else {
            return self.errorf(format!("{} is not a defined function", quote(&id.ident)));
        };
        self.eval_call(dot, function, cmd, &id.ident, args, fin)
    }

    /// `state.evalField` (exec.go:682).
    fn eval_field(
        &mut self,
        _dot: Rv<'a>,
        field_name: &str,
        _node: &'t Arg,
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
        receiver: Rv<'a>,
    ) -> R<Rv<'a>> {
        if !receiver.is_valid() {
            if self.common.missing_key == MissingKey::Error {
                return self.errorf(format!("nil data; no entry for key {}", quote(field_name)));
            }
            return Ok(Rv::Invalid);
        }
        let typ = receiver.type_name();
        let (receiver, is_nil) = receiver.indirect();
        if receiver.kind() == Kind::Interface && is_nil {
            return self.errorf(format!("nil pointer evaluating {typ}.{field_name}"));
        }
        let has_args = args.is_some_and(|a| a.len() > 1) || fin.is_some();
        match &receiver {
            Rv::Val(c, _) => match &**c {
                Value::Struct(_, fields) => {
                    if let Some(idx) = fields.iter().position(|(n, _)| n == field_name) {
                        if !field_name.chars().next().is_some_and(char::is_uppercase) {
                            return self.errorf(format!(
                                "{field_name} is an unexported field of struct type {typ}"
                            ));
                        }
                        if has_args {
                            return self.errorf(format!(
                                "{field_name} has arguments but cannot be invoked as function"
                            ));
                        }
                        let field = crate::rv::project(c, |x| match x {
                            Value::Struct(_, f) => &f[idx].1,
                            other => other,
                        });
                        return Ok(Rv::from_cow(field, false));
                    }
                }
                Value::Map(m) => {
                    if has_args {
                        return self
                            .errorf(format!("{field_name} is not a method but has arguments"));
                    }
                    if m.contains_key(field_name) {
                        let elem = crate::rv::project(c, |x| match x {
                            Value::Map(m) => m.get(field_name).unwrap_or(x),
                            other => other,
                        });
                        return Ok(Rv::from_cow(elem, true));
                    }
                    return match self.common.missing_key {
                        MissingKey::Invalid => Ok(Rv::Invalid),
                        MissingKey::Zero => Ok(Rv::nil_iface()),
                        MissingKey::Error => {
                            self.errorf(format!("map has no entry for key {}", quote(field_name)))
                        }
                    };
                }
                Value::NilPtr(_) if is_nil => {
                    return self.errorf(format!("nil pointer evaluating {typ}.{field_name}"));
                }
                _ => {}
            },
            Rv::Invalid => {}
        }
        self.errorf(format!("can't evaluate field {field_name} in type {typ}"))
    }

    /// `state.evalCall` (exec.go:772).
    fn eval_call(
        &mut self,
        dot: Rv<'a>,
        fun: Func,
        node: Here<'t>,
        name: &str,
        args: Option<&'t [Arg]>,
        fin: Option<Rv<'a>>,
    ) -> R<Rv<'a>> {
        let args: &'t [Arg] = match args {
            Some(a) if !a.is_empty() => &a[1..],
            _ => &[],
        };
        let sig = fun.signature();
        let num_in = args.len() + usize::from(fin.is_some());
        let mut num_fixed = args.len();
        if sig.variadic.is_some() {
            num_fixed = sig.fixed.len();
            if num_in < num_fixed {
                return self.errorf(format!(
                    "wrong number of args for {name}: want at least {} got {}",
                    sig.fixed.len(),
                    args.len()
                ));
            }
        } else if num_in != sig.fixed.len() {
            return self.errorf(format!(
                "wrong number of args for {name}: want {} got {num_in}",
                sig.fixed.len()
            ));
        }

        // Builtin and/or short-circuit.
        if let Func::Builtin(b @ (funcs::Builtin::And | funcs::Builtin::Or)) = fun {
            let is_or = b == funcs::Builtin::Or;
            let mut v = Rv::Invalid;
            for arg in args {
                v = self.eval_arg(dot.clone(), Some(Param::RValue), arg)?;
                if crate::rv::truth(&v) == is_or {
                    return Ok(v);
                }
            }
            if let Some(f) = fin {
                v = f;
            }
            return Ok(v);
        }

        let mut argv: Vec<Rv<'a>> = Vec::with_capacity(num_in);
        let mut i = 0;
        while i < num_fixed && i < args.len() {
            argv.push(self.eval_arg(dot.clone(), Some(sig.fixed[i]), &args[i])?);
            i += 1;
        }
        if let Some(var) = sig.variadic {
            while i < args.len() {
                argv.push(self.eval_arg(dot.clone(), Some(var), &args[i])?);
                i += 1;
            }
        }
        if let Some(f) = fin {
            let t = match sig.variadic {
                Some(var) => {
                    if num_in - 1 < num_fixed {
                        sig.fixed[num_in - 1]
                    } else {
                        var
                    }
                }
                None => sig.fixed[sig.fixed.len() - 1],
            };
            argv.push(self.validate_type(f, Some(t))?);
        }

        // The `call` builtin reports the callee by its source text.
        let callee = if let Func::Builtin(funcs::Builtin::Call) = fun {
            Some(if args.is_empty() {
                argv.first()
                    .and_then(|v| v.value())
                    .map(|v| crate::fmt::format_one(Some(v), 'v'))
                    .unwrap_or_default()
            } else {
                args[0].to_string()
            })
        } else {
            None
        };

        match funcs::call(fun, argv, callee.as_deref()) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.at(node);
                self.errorf(format!("error calling {name}: {e}"))
            }
        }
    }

    /// `state.validateType` (exec.go:892). `None` is a nil `reflect.Type`.
    fn validate_type(&mut self, value: Rv<'a>, typ: Option<Param>) -> R<Rv<'a>> {
        let Some(typ) = typ else {
            return Ok(value);
        };
        match typ {
            Param::RValue => Ok(value),
            Param::Any => {
                if value.is_valid() {
                    Ok(value)
                } else {
                    Ok(Rv::nil_iface())
                }
            }
            Param::Str => {
                if !value.is_valid() {
                    return self.errorf("invalid value; expected string");
                }
                let mut value = value;
                if matches!(value.value(), Some(Value::String(_)))
                    && value.kind() != Kind::Interface
                {
                    return Ok(value);
                }
                if value.kind() == Kind::Interface && !value.is_nil() {
                    value = value.indirect_interface();
                    if matches!(value.value(), Some(Value::String(_))) {
                        return Ok(value);
                    }
                }
                match value.value() {
                    Some(Value::Ptr(inner)) if matches!(**inner, Value::String(_)) => {
                        Ok(value.indirect().0)
                    }
                    Some(Value::NilPtr(t)) if t == "*string" => {
                        self.errorf("dereference of nil pointer of type string")
                    }
                    _ => self.errorf(format!(
                        "wrong type for value; expected string; got {}",
                        value.type_name()
                    )),
                }
            }
        }
    }

    /// `state.evalArg` (exec.go:934).
    fn eval_arg(&mut self, dot: Rv<'a>, typ: Option<Param>, n: &'t Arg) -> R<Rv<'a>> {
        self.at(Here::Arg(n));
        match n {
            Arg::Dot(_) => return self.validate_type(dot, typ),
            Arg::Nil(_) => {
                return match typ {
                    Some(Param::Any) => Ok(Rv::nil_iface()),
                    Some(Param::RValue) | None => Ok(Rv::Invalid),
                    Some(Param::Str) => self.errorf("cannot assign nil to string"),
                };
            }
            Arg::Field(f) => {
                let v = self.eval_field_chain(
                    dot.clone(),
                    dot,
                    n,
                    &f.ident,
                    Some(std::slice::from_ref(n)),
                    None,
                )?;
                return self.validate_type(v, typ);
            }
            Arg::Variable(_) => {
                let v = self.eval_variable_node(dot, n, None, None)?;
                return self.validate_type(v, typ);
            }
            Arg::Pipe(p) => {
                let v = self.eval_pipeline(dot, Some(p))?;
                return self.validate_type(v, typ);
            }
            Arg::Identifier(_) => {
                let v = self.eval_function(dot, n, Here::Arg(n), None, None)?;
                return self.validate_type(v, typ);
            }
            Arg::Chain(_) => {
                let v = self.eval_chain_node(dot, n, None, None)?;
                return self.validate_type(v, typ);
            }
            _ => {}
        }
        match typ {
            Some(Param::Str) => match n {
                Arg::String(s) => Ok(Rv::owned(Value::String(s.text.clone()))),
                _ => self.errorf(format!("expected string; found {n}")),
            },
            _ => self.eval_empty_interface(n),
        }
    }

    fn eval_empty_interface(&mut self, n: &'t Arg) -> R<Rv<'a>> {
        self.at(Here::Arg(n));
        match n {
            Arg::Bool(b) => Ok(Rv::owned(Value::Bool(b.value))),
            Arg::Number(num) => self.ideal_constant(n, num),
            Arg::String(s) => Ok(Rv::owned(Value::String(s.text.clone()))),
            _ => self.errorf(format!(
                "can't handle assignment of {n} to empty interface argument"
            )),
        }
    }

    /// `state.printValue` (exec.go:1101).
    fn print_value(&mut self, n: Here<'t>, v: Rv<'a>) -> R<()> {
        self.at(n);
        let v = if v.kind() == Kind::Pointer {
            v.indirect().0
        } else {
            v
        };
        if !v.is_valid() {
            self.out.push_str("<no value>");
            return Ok(());
        }
        let s = crate::fmt::sprint(&[v.as_any()]);
        self.out.push_str(&s);
        Ok(())
    }
}

/// `%v` (or another verb) of a `reflect.Value`, as fmt prints one: its underlying value.
pub(crate) fn fmt_rv(v: &Rv<'_>, verb: char) -> String {
    match v {
        Rv::Invalid => "<invalid reflect.Value>".to_string(),
        Rv::Val(c, _) => match &**c {
            Value::Nil => "<nil>".to_string(),
            other => crate::fmt::format_one(Some(other), verb),
        },
    }
}
