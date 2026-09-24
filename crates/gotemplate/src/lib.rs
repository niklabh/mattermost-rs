//! Port of Go's `text/template` and `html/template` (SKELETON — API contract only).

use std::collections::BTreeMap;

/// A value a template executes against: the Go data graph, reduced to what templates can see.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A nil interface, nil pointer or absent map key.
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A plain Go `string` — escaped by `html/template` for the context it lands in.
    String(String),
    /// `template.HTML` — trusted markup, not escaped in an HTML text context.
    Html(String),
    /// `template.URL`.
    Url(String),
    /// A Go slice or array.
    List(Vec<Value>),
    /// A Go `map[string]T`. `range` visits it in sorted key order, as Go does.
    Map(BTreeMap<String, Value>),
    /// A Go struct: field name to value, in declaration order.
    Struct(Vec<(String, Value)>),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("template: {0}")]
    Parse(String),
    #[error("template: {0}")]
    Exec(String),
    #[error("html/template: {0}")]
    Escape(String),
}

/// A set of named `html/template` templates, as `template.ParseGlob` builds them.
#[derive(Debug, Default)]
pub struct HtmlTemplates {}

impl HtmlTemplates {
    /// `template.New("").ParseFiles(...)`-style: each `(file name, source)` is parsed in order;
    /// the file's own name becomes a template, and every `{{define}}` inside it another.
    pub fn parse_files(_files: &[(String, String)]) -> Result<Self, Error> {
        Err(Error::Parse("not implemented".into()))
    }

    /// `ExecuteTemplate(w, name, data)`, returning what Go would have written.
    pub fn execute(&self, _name: &str, _data: &Value) -> Result<String, Error> {
        Err(Error::Exec("not implemented".into()))
    }
}
