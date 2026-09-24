//! Port of `text/template/template.go` and `helper.go`: a set of associated templates that share
//! a name space, built by `Parse` / `ParseFiles` and run by `Execute` / `ExecuteTemplate`.

use std::sync::Arc;

use crate::Error;
use crate::exec::{self, Common, MissingKey, TextTmpl};
use crate::parse::node::is_empty_list;
use crate::parse::parse::{Tree, parse};
use crate::strconv::quote;
use crate::value::Value;

/// `Template.AddParseTree` (template.go:128) for a set whose root is `root`: installs `tree`
/// under `name` unless an existing definition would be replaced by an empty one.
pub(crate) fn add_parse_tree(common: &mut Common, name: &str, tree: Arc<Tree>) {
    if let Some(old) = common.tmpl.get(name)
        && is_empty_list(&tree.root)
        && old.tree.is_some()
    {
        return;
    }
    common.tmpl.insert(
        name.to_string(),
        TextTmpl {
            name: name.to_string(),
            tree: Some(tree),
        },
    );
}

/// `Template.Parse` of `text` as the template `name` into `common`: every tree the text defines
/// is added. Returns the names added, in Go's `Templates()` sense (all of them).
pub(crate) fn parse_into(common: &mut Common, name: &str, text: &str) -> Result<(), Error> {
    let trees = parse(name, text, &crate::funcs::is_builtin).map_err(Error::Parse)?;
    for (tname, tree) in trees {
        add_parse_tree(common, &tname, Arc::new(tree));
    }
    Ok(())
}

/// A set of `text/template` templates sharing one name space (`*template.Template` and the
/// templates associated with it).
///
/// ```
/// use gotemplate::{TextTemplates, Value};
/// let t = TextTemplates::parse("greet", "Hello, {{.Name}}!").unwrap();
/// let data = Value::map([("Name", Value::str("Ann"))]);
/// assert_eq!(t.execute(&data).unwrap(), "Hello, Ann!");
/// ```
#[derive(Debug, Clone)]
pub struct TextTemplates {
    name: String,
    common: Common,
}

impl TextTemplates {
    /// `template.New(name).Parse(text)`.
    pub fn parse(name: &str, text: &str) -> Result<Self, Error> {
        let mut t = TextTemplates::new(name);
        t.add(name, text)?;
        Ok(t)
    }

    /// `template.New(name)`: an empty set whose root template is `name`.
    pub fn new(name: &str) -> Self {
        TextTemplates {
            name: name.to_string(),
            common: Common::default(),
        }
    }

    /// `t.New(name).Parse(text)`: parses `text` as the template `name` in this set (`name` may
    /// be the root's own name).
    pub fn add(&mut self, name: &str, text: &str) -> Result<(), Error> {
        parse_into(&mut self.common, name, text)
    }

    /// `template.New(first file).ParseFiles(...)`: each `(file name, source)` is parsed in
    /// order as the template named by the file's base name.
    pub fn parse_files(files: &[(String, String)]) -> Result<Self, Error> {
        let Some((first, _)) = files.first() else {
            return Err(Error::Parse(
                "template: no files named in call to ParseFiles".to_string(),
            ));
        };
        let mut t = TextTemplates::new(first);
        for (name, text) in files {
            t.add(name, text)?;
        }
        Ok(t)
    }

    /// `Option("missingkey=...")`.
    pub fn set_missing_key(&mut self, m: MissingKey) {
        self.common.missing_key = m;
    }

    /// `Execute`: runs the root template.
    pub fn execute(&self, data: &Value) -> Result<String, Error> {
        self.execute_template(&self.name, data)
    }

    /// `ExecuteTemplate`.
    pub fn execute_template(&self, name: &str, data: &Value) -> Result<String, Error> {
        let Some(tmpl) = self.common.tmpl.get(name) else {
            return Err(Error::Exec(format!(
                "template: no template {} associated with template {}",
                quote(name),
                quote(&self.name)
            )));
        };
        exec::execute(&self.common, tmpl, data).map_err(Error::Exec)
    }

    /// Whether a template of this name is defined.
    pub fn has(&self, name: &str) -> bool {
        self.common.tmpl.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn define_and_execute() {
        let mut t =
            TextTemplates::parse("t", "{{define \"a\"}}[{{.}}]{{end}}x{{template \"a\" 1}}")
                .unwrap();
        assert_eq!(t.execute(&Value::Nil).unwrap(), "x[1]");
        t.add("u", "{{template \"a\" \"s\"}}").unwrap();
        assert_eq!(t.execute_template("u", &Value::Nil).unwrap(), "[s]");
        assert_eq!(
            t.execute_template("zz", &Value::Nil)
                .unwrap_err()
                .to_string(),
            "template: no template \"zz\" associated with template \"t\""
        );
    }

    #[test]
    fn empty_redefinition_keeps_body() {
        let mut t = TextTemplates::parse("t", "{{define \"a\"}}body{{end}}").unwrap();
        t.add("u", "{{define \"a\"}}  {{end}}").unwrap();
        assert_eq!(t.execute_template("a", &Value::Nil).unwrap(), "body");
    }
}
