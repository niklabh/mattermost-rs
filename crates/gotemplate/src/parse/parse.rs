//! Port of `text/template/parse/parse.go`: the recursive-descent parser.
//!
//! Go's parser reports errors by panicking out of arbitrarily deep recursion; this port returns
//! `Result<_, String>` where the string is the finished `err.Error()` text, built exactly as
//! `Tree.errorf` builds it (`template: <ParseName>:<line of token[0]>: ...`).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::lex::{Item, ItemType, Lexer};
use super::node::*;
use crate::strconv::{parse_float, parse_int0, parse_uint0, quote, unquote, unquote_char};

/// A parsed template (`parse.Tree`).
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    pub name: String,
    pub root: ListNode,
    /// The text and `ParseName` of the Parse call that produced this tree. A tree `html/template`
    /// derives has none: Go gives it an empty `ParseName` and text.
    pub src: Option<Arc<Src>>,
}

/// `maxStackDepth` (parse.go:48).
const MAX_STACK_DEPTH: usize = 10000;

type PResult<T> = Result<T, String>;

struct Shared<'f> {
    lex: Lexer,
    tree_set: BTreeMap<String, Tree>,
    has_function: &'f dyn Fn(&str) -> bool,
    src: Arc<Src>,
}

/// The per-tree parser state (`Tree`'s parsing-only fields).
struct TreeState {
    name: String,
    root: Option<ListNode>,
    token: [Item; 3],
    peek_count: usize,
    vars: Vec<String>,
    action_line: usize,
    range_depth: usize,
    stack_depth: usize,
}

fn empty_item() -> Item {
    Item {
        typ: ItemType::Eof,
        pos: 0,
        val: String::new(),
        line: 0,
    }
}

impl TreeState {
    fn new(name: &str) -> Self {
        TreeState {
            name: name.to_string(),
            root: None,
            token: [empty_item(), empty_item(), empty_item()],
            peek_count: 0,
            vars: vec!["$".to_string()],
            action_line: 0,
            range_depth: 0,
            stack_depth: 0,
        }
    }
}

/// What `textOrAction` / `action` produce: a node, or the `{{end}}` / `{{else}}` markers that
/// never enter a tree.
enum Parsed {
    Node(Node),
    End,
    Else(usize),
}

impl Parsed {
    fn describe(&self) -> String {
        match self {
            Parsed::Node(n) => n.to_string(),
            Parsed::End => "{{end}}".to_string(),
            Parsed::Else(_) => "{{else}}".to_string(),
        }
    }
}

struct P<'s, 'f> {
    sh: &'s mut Shared<'f>,
    t: TreeState,
}

/// `parse.Parse` (parse.go:78): parses `text` as template `name`, returning every tree it defines
/// (the top-level one under `name`, plus each `{{define}}` and `{{block}}`).
pub(crate) fn parse(
    name: &str,
    text: &str,
    has_function: &dyn Fn(&str) -> bool,
) -> Result<BTreeMap<String, Tree>, String> {
    let src = Arc::new(Src {
        parse_name: name.to_string(),
        text: Arc::from(text),
    });
    let mut lex = Lexer::new(text, "", "");
    lex.options.emit_comment = false;
    lex.options.break_ok = !has_function("break");
    lex.options.continue_ok = !has_function("continue");
    let mut sh = Shared {
        lex,
        tree_set: BTreeMap::new(),
        has_function,
        src,
    };
    {
        let mut p = P {
            sh: &mut sh,
            t: TreeState::new(name),
        };
        p.parse_top()?;
        p.add()?;
    }
    Ok(sh.tree_set)
}

impl P<'_, '_> {
    fn src(&self) -> SrcRef {
        Some(self.sh.src.clone())
    }

    fn next(&mut self) -> Item {
        if self.t.peek_count > 0 {
            self.t.peek_count -= 1;
        } else {
            self.t.token[0] = self.sh.lex.next_item();
        }
        self.t.token[self.t.peek_count].clone()
    }

    fn backup(&mut self) {
        self.t.peek_count += 1;
    }

    fn backup2(&mut self, t1: Item) {
        self.t.token[1] = t1;
        self.t.peek_count = 2;
    }

    fn backup3(&mut self, t2: Item, t1: Item) {
        self.t.token[1] = t1;
        self.t.token[2] = t2;
        self.t.peek_count = 3;
    }

    fn peek(&mut self) -> Item {
        if self.t.peek_count > 0 {
            return self.t.token[self.t.peek_count - 1].clone();
        }
        self.t.peek_count = 1;
        self.t.token[0] = self.sh.lex.next_item();
        self.t.token[0].clone()
    }

    fn next_non_space(&mut self) -> Item {
        loop {
            let token = self.next();
            if token.typ != ItemType::Space {
                return token;
            }
        }
    }

    fn peek_non_space(&mut self) -> Item {
        let token = self.next_non_space();
        self.backup();
        token
    }

    /// `Tree.errorf`.
    fn errorf<T>(&self, msg: impl std::fmt::Display) -> PResult<T> {
        Err(format!(
            "template: {}:{}: {}",
            self.sh.src.parse_name, self.t.token[0].line, msg
        ))
    }

    fn expect(&mut self, expected: ItemType, context: &str) -> PResult<Item> {
        let token = self.next_non_space();
        if token.typ != expected {
            return self.unexpected(&token, context);
        }
        Ok(token)
    }

    fn expect_one_of(&mut self, e1: ItemType, e2: ItemType, context: &str) -> PResult<Item> {
        let token = self.next_non_space();
        if token.typ != e1 && token.typ != e2 {
            return self.unexpected(&token, context);
        }
        Ok(token)
    }

    fn unexpected<T>(&self, token: &Item, context: &str) -> PResult<T> {
        if token.typ == ItemType::Error {
            let mut extra = String::new();
            if self.t.action_line != 0 && self.t.action_line != token.line {
                extra = format!(
                    " in action started at {}:{}",
                    self.sh.src.parse_name, self.t.action_line
                );
                if token.val.ends_with(" action") {
                    extra = extra[" in action".len()..].to_string();
                }
            }
            return self.errorf(format!("{token}{extra}"));
        }
        self.errorf(format!("unexpected {token} in {context}"))
    }

    /// `Tree.add` (parse.go:290).
    fn add(&mut self) -> PResult<()> {
        let root = self.t.root.clone().unwrap_or(ListNode {
            pos: 0,
            src: self.src(),
            nodes: vec![],
        });
        let replace = match self.sh.tree_set.get(&self.t.name) {
            None => true,
            Some(existing) => is_empty_list(&existing.root),
        };
        if replace {
            self.sh.tree_set.insert(
                self.t.name.clone(),
                Tree {
                    name: self.t.name.clone(),
                    root,
                    src: self.src(),
                },
            );
            return Ok(());
        }
        if !is_empty_list(&root) {
            return self.errorf(format!(
                "template: multiple definition of template {}",
                quote(&self.t.name)
            ));
        }
        Ok(())
    }

    fn new_list(&self, pos: usize) -> ListNode {
        ListNode {
            pos,
            src: self.src(),
            nodes: vec![],
        }
    }

    /// `Tree.parse` (parse.go:335).
    fn parse_top(&mut self) -> PResult<()> {
        let pos = self.peek().pos;
        self.t.root = Some(self.new_list(pos));
        while self.peek().typ != ItemType::Eof {
            if self.peek().typ == ItemType::LeftDelim {
                let delim = self.next();
                if self.next_non_space().typ == ItemType::Define {
                    let mut sub = P {
                        sh: &mut *self.sh,
                        t: TreeState::new("definition"),
                    };
                    sub.parse_definition()?;
                    continue;
                }
                self.backup2(delim);
            }
            match self.text_or_action()? {
                Parsed::Node(n) => {
                    if let Some(root) = self.t.root.as_mut() {
                        root.nodes.push(n);
                    }
                }
                other => return self.errorf(format!("unexpected {}", other.describe())),
            }
        }
        Ok(())
    }

    /// `Tree.parseDefinition` (parse.go:363).
    fn parse_definition(&mut self) -> PResult<()> {
        let context = "define clause";
        let name = self.expect_one_of(ItemType::String, ItemType::RawString, context)?;
        match unquote(&name.val) {
            Ok(n) => self.t.name = n,
            Err(()) => return self.errorf("invalid syntax"),
        }
        self.expect(ItemType::RightDelim, context)?;
        let (list, end) = self.item_list()?;
        self.t.root = Some(list);
        if !matches!(end, Parsed::End) {
            return self.errorf(format!("unexpected {} in {}", end.describe(), context));
        }
        self.add()
    }

    /// `Tree.itemList` (parse.go:385).
    fn item_list(&mut self) -> PResult<(ListNode, Parsed)> {
        let pos = self.peek_non_space().pos;
        let mut list = self.new_list(pos);
        while self.peek_non_space().typ != ItemType::Eof {
            let n = self.text_or_action()?;
            match n {
                Parsed::Node(node) => list.nodes.push(node),
                other => return Ok((list, other)),
            }
        }
        self.errorf("unexpected EOF")
    }

    /// `Tree.textOrAction` (parse.go:401).
    fn text_or_action(&mut self) -> PResult<Parsed> {
        let token = self.next_non_space();
        match token.typ {
            ItemType::Text => Ok(Parsed::Node(Node::Text(TextNode {
                id: next_id(),
                pos: token.pos,
                src: self.src(),
                text: token.val,
            }))),
            ItemType::LeftDelim => {
                self.t.action_line = token.line;
                let r = self.action();
                self.t.action_line = 0;
                r
            }
            ItemType::Comment => Ok(Parsed::Node(Node::Comment(CommentNode {
                pos: token.pos,
                src: self.src(),
                text: token.val,
            }))),
            _ => self.unexpected(&token, "input"),
        }
    }

    /// `Tree.action` (parse.go:427).
    fn action(&mut self) -> PResult<Parsed> {
        let token = self.next_non_space();
        match token.typ {
            ItemType::Block => return self.block_control().map(Parsed::Node),
            ItemType::Break => return self.break_control(token.pos, token.line, true),
            ItemType::Continue => return self.break_control(token.pos, token.line, false),
            ItemType::Else => return self.else_control(),
            ItemType::End => return self.end_control(),
            ItemType::If => return self.if_control().map(Parsed::Node),
            ItemType::Range => return self.range_control().map(Parsed::Node),
            ItemType::Template => return self.template_control().map(Parsed::Node),
            ItemType::With => return self.with_control().map(Parsed::Node),
            _ => {}
        }
        self.backup();
        let token = self.peek();
        let pipe = self.pipeline("command", ItemType::RightDelim)?;
        Ok(Parsed::Node(Node::Action(ActionNode {
            id: next_id(),
            pos: token.pos,
            src: self.src(),
            pipe,
        })))
    }

    /// `Tree.breakControl` / `Tree.continueControl` (parse.go:461).
    fn break_control(&mut self, pos: usize, line: usize, is_break: bool) -> PResult<Parsed> {
        let what = if is_break { "break" } else { "continue" };
        let token = self.next_non_space();
        if token.typ != ItemType::RightDelim {
            return self.unexpected(&token, &format!("{{{{{what}}}}}"));
        }
        if self.t.range_depth == 0 {
            return self.errorf(format!("{{{{{what}}}}} outside {{{{range}}}}"));
        }
        let n = PosNode {
            pos,
            src: self.src(),
            line,
        };
        Ok(Parsed::Node(if is_break {
            Node::Break(n)
        } else {
            Node::Continue(n)
        }))
    }

    /// `Tree.pipeline` (parse.go:490).
    fn pipeline(&mut self, context: &str, end: ItemType) -> PResult<PipeNode> {
        let token = self.peek_non_space();
        let mut pipe = PipeNode {
            pos: token.pos,
            src: self.src(),
            line: token.line,
            is_assign: false,
            decl: vec![],
            cmds: vec![],
        };
        // decls:
        loop {
            let v = self.peek_non_space();
            if v.typ != ItemType::Variable {
                break;
            }
            self.next();
            let token_after_variable = self.peek();
            let next = self.peek_non_space();
            if next.typ == ItemType::Assign || next.typ == ItemType::Declare {
                pipe.is_assign = next.typ == ItemType::Assign;
                self.next_non_space();
                pipe.decl.push(self.new_variable(v.pos, &v.val));
                self.t.vars.push(v.val.clone());
            } else if next.typ == ItemType::Char && next.val == "," {
                self.next_non_space();
                pipe.decl.push(self.new_variable(v.pos, &v.val));
                self.t.vars.push(v.val.clone());
                if context == "range" && pipe.decl.len() < 2 {
                    match self.peek_non_space().typ {
                        ItemType::Variable | ItemType::RightDelim | ItemType::RightParen => {
                            continue;
                        }
                        _ => return self.errorf("range can only initialize variables"),
                    }
                }
                return self.errorf(format!("too many declarations in {context}"));
            } else if token_after_variable.typ == ItemType::Space {
                self.backup3(v, token_after_variable);
            } else {
                self.backup2(v);
            }
            break;
        }
        loop {
            let token = self.next_non_space();
            if token.typ == end {
                self.check_pipeline(&pipe, context)?;
                return Ok(pipe);
            }
            match token.typ {
                ItemType::Bool
                | ItemType::CharConstant
                | ItemType::Complex
                | ItemType::Dot
                | ItemType::Field
                | ItemType::Identifier
                | ItemType::Number
                | ItemType::Nil
                | ItemType::RawString
                | ItemType::String
                | ItemType::Variable
                | ItemType::LeftParen => {
                    self.backup();
                    let cmd = self.command()?;
                    pipe.cmds.push(cmd);
                }
                _ => return self.unexpected(&token, context),
            }
        }
    }

    fn check_pipeline(&self, pipe: &PipeNode, context: &str) -> PResult<()> {
        if pipe.cmds.is_empty() {
            return self.errorf(format!("missing value for {context}"));
        }
        for (i, c) in pipe.cmds.iter().enumerate().skip(1) {
            if matches!(
                c.args.first(),
                Some(Arg::Bool(_) | Arg::Dot(_) | Arg::Nil(_) | Arg::Number(_) | Arg::String(_))
            ) {
                return self.errorf(format!(
                    "non executable command in pipeline stage {}",
                    i + 1
                ));
            }
        }
        Ok(())
    }

    /// `Tree.parseControl` (parse.go:568).
    fn parse_control(&mut self, context: &str) -> PResult<BranchNode> {
        let mark = self.t.vars.len();
        let pipe = self.pipeline(context, ItemType::RightDelim)?;
        if context == "range" {
            self.t.range_depth += 1;
        }
        let (list, next) = self.item_list()?;
        if context == "range" {
            self.t.range_depth -= 1;
        }
        let mut else_list = None;
        match next {
            Parsed::End => {}
            Parsed::Else(else_pos) => {
                if context == "if" && self.peek().typ == ItemType::If {
                    self.next();
                    let mut l = self.new_list(else_pos);
                    l.nodes.push(self.if_control()?);
                    else_list = Some(l);
                } else if context == "with" && self.peek().typ == ItemType::With {
                    self.next();
                    let mut l = self.new_list(else_pos);
                    l.nodes.push(self.with_control()?);
                    else_list = Some(l);
                } else {
                    let (l, next) = self.item_list()?;
                    if !matches!(next, Parsed::End) {
                        return self.errorf(format!("expected end; found {}", next.describe()));
                    }
                    else_list = Some(l);
                }
            }
            Parsed::Node(_) => {}
        }
        self.t.vars.truncate(mark);
        Ok(BranchNode {
            pos: pipe.pos,
            src: self.src(),
            pipe,
            list,
            else_list,
        })
    }

    fn if_control(&mut self) -> PResult<Node> {
        self.parse_control("if").map(Node::If)
    }

    fn range_control(&mut self) -> PResult<Node> {
        self.parse_control("range").map(Node::Range)
    }

    fn with_control(&mut self) -> PResult<Node> {
        self.parse_control("with").map(Node::With)
    }

    fn end_control(&mut self) -> PResult<Parsed> {
        self.expect(ItemType::RightDelim, "end")?;
        Ok(Parsed::End)
    }

    fn else_control(&mut self) -> PResult<Parsed> {
        let peek = self.peek_non_space();
        if peek.typ == ItemType::If || peek.typ == ItemType::With {
            return Ok(Parsed::Else(peek.pos));
        }
        let token = self.expect(ItemType::RightDelim, "else")?;
        Ok(Parsed::Else(token.pos))
    }

    /// `Tree.blockControl` (parse.go:678).
    fn block_control(&mut self) -> PResult<Node> {
        let context = "block clause";
        let token = self.next_non_space();
        let name = self.parse_template_name(&token, context)?;
        let pipe = self.pipeline(context, ItemType::RightDelim)?;
        let end = {
            let mut block = P {
                sh: &mut *self.sh,
                t: TreeState::new(&name),
            };
            let (list, end) = block.item_list()?;
            block.t.root = Some(list);
            if matches!(end, Parsed::End) {
                block.add()?;
            }
            end
        };
        if !matches!(end, Parsed::End) {
            return self.errorf(format!("unexpected {} in {}", end.describe(), context));
        }
        Ok(Node::Template(TemplateNode {
            id: next_id(),
            pos: token.pos,
            src: self.src(),
            line: token.line,
            name,
            pipe: Some(pipe),
        }))
    }

    /// `Tree.templateControl` (parse.go:710).
    fn template_control(&mut self) -> PResult<Node> {
        let context = "template clause";
        let token = self.next_non_space();
        let name = self.parse_template_name(&token, context)?;
        let mut pipe = None;
        if self.next_non_space().typ != ItemType::RightDelim {
            self.backup();
            pipe = Some(self.pipeline(context, ItemType::RightDelim)?);
        }
        Ok(Node::Template(TemplateNode {
            id: next_id(),
            pos: token.pos,
            src: self.src(),
            line: token.line,
            name,
            pipe,
        }))
    }

    fn parse_template_name(&self, token: &Item, context: &str) -> PResult<String> {
        match token.typ {
            ItemType::String | ItemType::RawString => match unquote(&token.val) {
                Ok(s) => Ok(s),
                Err(()) => self.errorf("invalid syntax"),
            },
            _ => self.unexpected(token, context),
        }
    }

    /// `Tree.command` (parse.go:744).
    fn command(&mut self) -> PResult<CommandNode> {
        let pos = self.peek_non_space().pos;
        let mut cmd = CommandNode {
            pos,
            src: self.src(),
            args: vec![],
        };
        loop {
            self.peek_non_space();
            if let Some(operand) = self.operand()? {
                cmd.args.push(operand);
            }
            let token = self.next();
            match token.typ {
                ItemType::Space => continue,
                ItemType::RightDelim | ItemType::RightParen => self.backup(),
                ItemType::Pipe => {}
                _ => return self.unexpected(&token, "operand"),
            }
            break;
        }
        if cmd.args.is_empty() {
            return self.errorf("empty command");
        }
        Ok(cmd)
    }

    /// `Tree.operand` (parse.go:778).
    fn operand(&mut self) -> PResult<Option<Arg>> {
        let Some(node) = self.term()? else {
            return Ok(None);
        };
        if self.peek().typ == ItemType::Field {
            let pos = self.peek().pos;
            let mut fields = Vec::new();
            while self.peek().typ == ItemType::Field {
                let f = self.next().val;
                fields.push(f[1..].to_string());
            }
            let chain = ChainNode {
                pos,
                src: self.src(),
                node: Box::new(node),
                field: fields,
            };
            let s = chain.to_string();
            return match &*chain.node {
                Arg::Field(_) => Ok(Some(Arg::Field(FieldNode {
                    pos,
                    src: self.src(),
                    ident: s[1..].split('.').map(str::to_string).collect(),
                }))),
                Arg::Variable(_) => Ok(Some(Arg::Variable(self.new_variable(pos, &s)))),
                Arg::Bool(_) | Arg::String(_) | Arg::Number(_) | Arg::Nil(_) | Arg::Dot(_) => self
                    .errorf(format!(
                        "unexpected . after term {}",
                        quote(&chain.node.to_string())
                    )),
                _ => Ok(Some(Arg::Chain(chain))),
            };
        }
        Ok(Some(node))
    }

    fn new_variable(&self, pos: usize, ident: &str) -> VariableNode {
        VariableNode {
            pos,
            src: self.src(),
            ident: ident.split('.').map(str::to_string).collect(),
        }
    }

    /// `Tree.term` (parse.go:822).
    fn term(&mut self) -> PResult<Option<Arg>> {
        let token = self.next_non_space();
        let src = self.src();
        let pos = token.pos;
        Ok(Some(match token.typ {
            ItemType::Identifier => {
                if !(self.sh.has_function)(&token.val) {
                    return self.errorf(format!("function {} not defined", quote(&token.val)));
                }
                Arg::Identifier(IdentifierNode {
                    pos,
                    src,
                    ident: token.val,
                })
            }
            ItemType::Dot => Arg::Dot(PosNode { pos, src, line: 0 }),
            ItemType::Nil => Arg::Nil(PosNode { pos, src, line: 0 }),
            ItemType::Variable => {
                let v = self.new_variable(pos, &token.val);
                if !self.t.vars.iter().any(|n| *n == v.ident[0]) {
                    return self.errorf(format!("undefined variable {}", quote(&v.ident[0])));
                }
                Arg::Variable(v)
            }
            ItemType::Field => Arg::Field(FieldNode {
                pos,
                src,
                ident: token.val[1..].split('.').map(str::to_string).collect(),
            }),
            ItemType::Bool => Arg::Bool(BoolNode {
                pos,
                src,
                value: token.val == "true",
            }),
            ItemType::CharConstant | ItemType::Complex | ItemType::Number => {
                match new_number(pos, src, &token.val, token.typ) {
                    Ok(n) => Arg::Number(n),
                    Err(e) => return self.errorf(e),
                }
            }
            ItemType::LeftParen => {
                if self.t.stack_depth >= MAX_STACK_DEPTH {
                    return self.errorf("max expression depth exceeded");
                }
                self.t.stack_depth += 1;
                let p = self.pipeline("parenthesized pipeline", ItemType::RightParen);
                self.t.stack_depth -= 1;
                Arg::Pipe(Box::new(p?))
            }
            ItemType::String | ItemType::RawString => match unquote(&token.val) {
                Ok(s) => Arg::String(StringNode {
                    pos,
                    src,
                    quoted: token.val,
                    text: s,
                }),
                Err(()) => return self.errorf("invalid syntax"),
            },
            _ => {
                self.backup();
                return Ok(None);
            }
        }))
    }
}

/// `Tree.newNumber` (node.go:640). Complex constants (`1+2i`, `3i`) are rejected: no template
/// Mattermost ships uses one, and supporting them would add a complex kind to every builtin.
fn new_number(pos: usize, src: SrcRef, text: &str, typ: ItemType) -> Result<NumberNode, String> {
    let mut n = NumberNode {
        pos,
        src,
        is_int: false,
        is_uint: false,
        is_float: false,
        int64: 0,
        uint64: 0,
        float64: 0.0,
        text: text.to_string(),
    };
    match typ {
        ItemType::CharConstant => {
            let quote_byte = text.as_bytes().first().copied().unwrap_or(b'\'');
            let u =
                unquote_char(&text[1..], quote_byte).map_err(|()| "invalid syntax".to_string())?;
            if u.tail != "'" {
                return Err(format!("malformed character constant: {text}"));
            }
            n.int64 = i64::from(u.value);
            n.is_int = true;
            n.uint64 = u64::from(u.value);
            n.is_uint = true;
            n.float64 = f64::from(u.value);
            n.is_float = true;
            return Ok(n);
        }
        ItemType::Complex => {
            return Err("complex constants are not supported by this port".to_string());
        }
        _ => {}
    }
    if text.ends_with('i') && parse_float(&text[..text.len() - 1]).is_some() {
        return Err("complex constants are not supported by this port".to_string());
    }
    let u = parse_uint0(text);
    if let Some(u) = u {
        n.is_uint = true;
        n.uint64 = u;
    }
    if let Some(i) = parse_int0(text) {
        n.is_int = true;
        n.int64 = i;
        if i == 0 {
            n.is_uint = true;
            n.uint64 = u.unwrap_or(0);
        }
    }
    if n.is_int {
        n.is_float = true;
        n.float64 = n.int64 as f64;
    } else if n.is_uint {
        n.is_float = true;
        n.float64 = n.uint64 as f64;
    } else if let Some(f) = parse_float(text) {
        if !text.contains(['.', 'e', 'E', 'p', 'P']) {
            return Err(format!("integer overflow: {}", quote(text)));
        }
        n.is_float = true;
        n.float64 = f;
        if !n.is_int && (f as i64) as f64 == f {
            n.is_int = true;
            n.int64 = f as i64;
        }
        if !n.is_uint && (f as u64) as f64 == f {
            n.is_uint = true;
            n.uint64 = f as u64;
        }
    }
    if !n.is_int && !n.is_uint && !n.is_float {
        return Err(format!("illegal number syntax: {}", quote(text)));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(name: &str) -> bool {
        crate::funcs::is_builtin(name)
    }

    fn parse_err(src: &str) -> String {
        match parse("t", src, &builtin) {
            Ok(_) => String::new(),
            Err(e) => e,
        }
    }

    #[test]
    fn round_trips_through_string() {
        let set = parse(
            "t",
            "a{{if .X}}b{{else if $}}c{{end}}{{range $i, $e := .L}}{{$i}}{{end}}{{template \"x\" .}}",
            &builtin,
        )
        .unwrap();
        assert_eq!(
            set["t"].root.to_string(),
            "a{{if .X}}b{{else}}{{if $}}c{{end}}{{end}}{{range $i, $e := .L}}{{$i}}{{end}}{{template \"x\" .}}"
        );
    }

    #[test]
    fn error_messages() {
        assert_eq!(parse_err("{{if .X}}"), "template: t:1: unexpected EOF");
        assert_eq!(
            parse_err("{{$x}}"),
            "template: t:1: undefined variable \"$x\""
        );
        assert_eq!(
            parse_err("{{nope}}"),
            "template: t:1: function \"nope\" not defined"
        );
        assert_eq!(
            parse_err("{{.S | 1}}"),
            "template: t:1: non executable command in pipeline stage 2"
        );
        assert_eq!(
            parse_err("{{089}}"),
            "template: t:1: integer overflow: \"089\""
        );
        assert_eq!(
            parse_err("{{define \"a\"}}1{{end}}{{define \"a\"}}2{{end}}"),
            "template: t:1: template: multiple definition of template \"a\""
        );
    }

    #[test]
    fn numbers() {
        let n = new_number(0, None, "1e3", ItemType::Number).unwrap();
        assert!(n.is_float && n.is_int && n.int64 == 1000);
        let n = new_number(0, None, "'a'", ItemType::CharConstant).unwrap();
        assert_eq!(n.int64, 97);
        assert!(new_number(0, None, "0x", ItemType::Number).is_err());
    }
}
