//! Port of `html/template/template.go`: a set of templates whose output is contextually escaped.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::context::{Context, State};
use super::error::{ErrNode, ErrorCode, errorf};
use super::escape::{EscState, Escaper, ensure_pipeline_contains};
use crate::Error;
use crate::exec::{self, Common, MissingKey};
use crate::parse::node::{ListNode, Node};
use crate::parse::parse::Tree;
use crate::strconv::{can_backquote, quote};
use crate::text::{add_parse_tree, parse_into};
use crate::value::Value;

/// A template's escaping state (`Template.escapeErr`): not yet escaped, escaped, or failed with
/// a sticky error.
#[derive(Debug, Clone)]
enum Escaped {
    Pending,
    Ok,
    Failed(String),
}

/// The shared name space (`nameSpace` plus the underlying `text/template` common).
#[derive(Debug)]
struct NameSpace {
    common: Common,
    set: HashMap<String, Escaped>,
    names: HashSet<String>,
    esc: EscState,
}

/// A set of named `html/template` templates, as `template.ParseGlob` / `ParseFiles` builds them.
///
/// Escaping happens lazily, on a template's first execution, exactly as in Go — and, as in Go,
/// what one execution derives (the escaped copies of templates called from attribute, URL, JS
/// or CSS contexts) is remembered by the set and can affect later ones. The set is therefore
/// behind a mutex; executions are serialised.
#[derive(Debug)]
pub struct HtmlTemplates {
    inner: Mutex<NameSpace>,
}

fn base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

impl HtmlTemplates {
    fn empty(root: &str) -> NameSpace {
        let mut set = HashMap::new();
        set.insert(root.to_string(), Escaped::Pending);
        let mut names = HashSet::new();
        names.insert(root.to_string());
        NameSpace {
            common: Common::default(),
            set,
            names,
            esc: EscState::default(),
        }
    }

    /// `template.ParseFiles(files...)` over already-read files: each `(file name, source)` is
    /// parsed in order as the template named by the file's base name, and every `{{define}}` in
    /// it becomes another template of the set. The first file names the set.
    pub fn parse_files(files: &[(String, String)]) -> Result<Self, Error> {
        let Some((first, _)) = files.first() else {
            return Err(Error::Parse(
                "html/template: no files named in call to ParseFiles".to_string(),
            ));
        };
        let mut ns = Self::empty(base_name(first));
        for (file, text) in files {
            ns.parse_one(base_name(file), text)?;
        }
        Ok(HtmlTemplates {
            inner: Mutex::new(ns),
        })
    }

    /// `template.ParseGlob(dir + "/*.html")`, which Mattermost's `templates.New(dir)` is: every
    /// `.html` file directly in `dir`, in sorted name order.
    pub fn parse_glob_html(dir: &Path) -> Result<Self, Error> {
        let pattern = dir.join("*.html");
        let rd = std::fs::read_dir(dir)
            .map_err(|e| Error::Parse(format!("open {}: {e}", dir.display())));
        let mut names: Vec<String> = match rd {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".html"))
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        if names.is_empty() {
            let p = pattern.display().to_string();
            let shown = if can_backquote(&p) {
                format!("`{p}`")
            } else {
                quote(&p)
            };
            return Err(Error::Parse(format!(
                "html/template: pattern matches no files: {shown}"
            )));
        }
        let mut files = Vec::with_capacity(names.len());
        for n in names {
            let path = dir.join(&n);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Error::Parse(format!("open {}: {e}", path.display())))?;
            files.push((n, text));
        }
        Self::parse_files(&files)
    }

    /// `template.New(name).Parse(text)`.
    pub fn parse(name: &str, text: &str) -> Result<Self, Error> {
        let mut ns = Self::empty(name);
        ns.parse_one(name, text)?;
        Ok(HtmlTemplates {
            inner: Mutex::new(ns),
        })
    }

    /// `t.New(name).Parse(text)`: adds a template to the set. Go refuses to parse after the set
    /// has executed (`cannot Parse after Execute`); so does this.
    pub fn add(&self, name: &str, text: &str) -> Result<(), Error> {
        let mut ns = self.lock();
        if ns.set.values().any(|e| !matches!(e, Escaped::Pending)) {
            return Err(Error::Parse(
                "html/template: cannot Parse after Execute".to_string(),
            ));
        }
        ns.parse_one(name, text)
    }

    /// `Option("missingkey=...")`.
    pub fn set_missing_key(&self, m: MissingKey) {
        self.lock().common.missing_key = m;
    }

    /// Whether a template of this name is in the set.
    pub fn has(&self, name: &str) -> bool {
        self.lock().set.contains_key(name)
    }

    /// The names of the templates in the set, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lock().set.keys().cloned().collect();
        v.sort();
        v
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NameSpace> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `ExecuteTemplate(w, name, data)`, returning what Go would have written. On error Go may
    /// already have written a prefix of the output; this returns only the error.
    pub fn execute(&self, name: &str, data: &Value) -> Result<String, Error> {
        let mut ns = self.lock();
        ns.lookup_and_escape(name)?;
        let ns = &*ns;
        let Some(tmpl) = ns.common.tmpl.get(name) else {
            return Err(Error::Escape(format!(
                "html/template: {} is an incomplete template",
                quote(name)
            )));
        };
        exec::execute(&ns.common, tmpl, data).map_err(Error::Exec)
    }
}

impl NameSpace {
    /// `t.New(name).Parse(text)` in the html set.
    fn parse_one(&mut self, name: &str, text: &str) -> Result<(), Error> {
        // `Template.new`: a name that already exists gets a fresh, unescaped template.
        self.set.insert(name.to_string(), Escaped::Pending);
        self.names.insert(name.to_string());
        parse_into(&mut self.common, name, text)?;
        let text_names: Vec<String> = self.common.tmpl.keys().cloned().collect();
        for n in text_names {
            self.set.entry(n.clone()).or_insert(Escaped::Pending);
            self.names.insert(n);
        }
        Ok(())
    }

    /// `lookupAndEscapeTemplate` (template.go:143).
    fn lookup_and_escape(&mut self, name: &str) -> Result<(), Error> {
        let Some(status) = self.set.get(name).cloned() else {
            return Err(Error::Escape(format!(
                "html/template: {} is undefined",
                quote(name)
            )));
        };
        if let Escaped::Failed(e) = status {
            return Err(Error::Escape(e));
        }
        let has_tree = self.common.tmpl.get(name).is_some_and(|t| t.tree.is_some());
        if !has_tree {
            return Err(Error::Escape(format!(
                "html/template: {} is an incomplete template",
                quote(name)
            )));
        }
        if matches!(status, Escaped::Pending) {
            self.escape_template(name)?;
        }
        Ok(())
    }

    /// `escapeTemplate` (escape.go:24).
    fn escape_template(&mut self, name: &str) -> Result<(), Error> {
        let root = self
            .common
            .tmpl
            .get(name)
            .and_then(|t| t.tree.clone())
            .map(|t| ErrNode {
                pos: t.root.pos,
                src: t.root.src.clone(),
            });
        let st = std::mem::take(&mut self.esc);
        let (c, st) = {
            let mut e = Escaper::new(&self.common, &self.names, st);
            let node = root.unwrap_or(ErrNode { pos: 0, src: None });
            let (c, _) = e.escape_tree(Context::default(), node, name, 0);
            (c, e.st)
        };
        self.esc = st;
        let err = if let Some(e) = &c.err {
            let mut e = (**e).clone();
            e.name = name.to_string();
            Some(e.to_string())
        } else if c.state != State::Text {
            let mut e = errorf(
                ErrorCode::EndContext,
                None,
                0,
                format!("ends in a non-text context: {c}"),
            );
            e.name = name.to_string();
            Some(e.to_string())
        } else {
            None
        };
        if let Some(e) = err {
            self.set
                .insert(name.to_string(), Escaped::Failed(e.clone()));
            if let Some(t) = self.common.tmpl.get_mut(name) {
                t.tree = None;
            }
            return Err(Error::Escape(e));
        }
        self.commit();
        self.set.insert(name.to_string(), Escaped::Ok);
        Ok(())
    }

    /// `escaper.commit` (escape.go:923).
    fn commit(&mut self) {
        if !self.esc.output.is_empty() {
            self.common.html_funcs = true;
        }
        let derived: Vec<(String, Arc<Tree>)> = self.esc.derived.drain().collect();
        for (name, t) in derived {
            add_parse_tree(&mut self.common, &name, t);
        }
        let st = &self.esc;
        for tmpl in self.common.tmpl.values_mut() {
            let Some(tree) = &tmpl.tree else {
                continue;
            };
            if !list_has_edit(&tree.root, st) {
                continue;
            }
            let mut t = (**tree).clone();
            apply_edits(&mut t.root, st);
            tmpl.tree = Some(Arc::new(t));
        }
        self.esc.called.clear();
        self.esc.action_edits.clear();
        self.esc.template_edits.clear();
        self.esc.text_edits.clear();
    }
}

fn list_has_edit(l: &ListNode, st: &EscState) -> bool {
    l.nodes.iter().any(|n| match n {
        Node::Text(t) => st.text_edits.contains_key(&t.id),
        Node::Action(a) => st.action_edits.contains_key(&a.id),
        Node::Template(t) => st.template_edits.contains_key(&t.id),
        Node::If(b) | Node::Range(b) | Node::With(b) => {
            list_has_edit(&b.list, st) || b.else_list.as_ref().is_some_and(|e| list_has_edit(e, st))
        }
        _ => false,
    })
}

fn apply_edits(l: &mut ListNode, st: &EscState) {
    for n in &mut l.nodes {
        match n {
            Node::Text(t) => {
                if let Some(text) = st.text_edits.get(&t.id) {
                    t.text = text.clone();
                }
            }
            Node::Action(a) => {
                if let Some(s) = st.action_edits.get(&a.id) {
                    ensure_pipeline_contains(&mut a.pipe, s);
                }
            }
            Node::Template(t) => {
                if let Some(name) = st.template_edits.get(&t.id) {
                    t.name = name.clone();
                }
            }
            Node::If(b) | Node::Range(b) | Node::With(b) => {
                apply_edits(&mut b.list, st);
                if let Some(e) = &mut b.else_list {
                    apply_edits(e, st);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, data: &Value) -> Result<String, Error> {
        HtmlTemplates::parse("t", src)?.execute("t", data)
    }

    #[test]
    fn escapes_by_context() {
        let d = Value::map([("X", Value::str("<a href='x'>&"))]);
        assert_eq!(
            run("<p>{{.X}}</p>", &d).unwrap(),
            "<p>&lt;a href=&#39;x&#39;&gt;&amp;</p>"
        );
        let d = Value::map([("U", Value::str("javascript:alert(1)"))]);
        assert_eq!(
            run("<a href=\"{{.U}}\">", &d).unwrap(),
            "<a href=\"#ZgotmplZ\">"
        );
        assert_eq!(run("a<!-- x -->b", &Value::Nil).unwrap(), "ab");
        assert_eq!(
            run("{{.Missing}}|", &Value::map([("A", Value::Int(1))])).unwrap(),
            "|"
        );
    }

    #[test]
    fn errors_are_sticky() {
        let t = HtmlTemplates::parse("t", "<a href=\"{{.}}").unwrap();
        let e1 = t.execute("t", &Value::Nil).unwrap_err().to_string();
        assert_eq!(
            e1,
            "html/template:t: ends in a non-text context: {stateURL delimDoubleQuote urlPartNone jsCtxRegexp [] attrURL elementNone <nil>}"
        );
        assert_eq!(t.execute("t", &Value::Nil).unwrap_err().to_string(), e1);
        assert_eq!(
            t.execute("nope", &Value::Nil).unwrap_err().to_string(),
            "html/template: \"nope\" is undefined"
        );
    }
}
