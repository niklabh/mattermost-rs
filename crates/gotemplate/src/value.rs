//! The data graph a template executes against — Go's `reflect.Value` world, reduced to what a
//! template can observe.

use std::collections::BTreeMap;

/// A value a template executes against.
///
/// # How this maps onto Go's types
///
/// Go templates see data through `reflect`, so the *static* type of a slot changes behaviour. This
/// model fixes the static types to what Mattermost's `templates.Data` produces:
///
/// * [`Value::Map`] is a `map[string]interface {}` and [`Value::List`] a `[]interface {}`: every
///   element sits in an interface-typed slot. A key that is **absent** and a key present with a
///   [`Value::Nil`] value therefore differ exactly as they do in Go — `.M.Absent.X` silently yields
///   no value, `.M.NilKey.X` is `nil pointer evaluating interface {}.X`.
/// * A [`Value::Struct`] field has the static type of its value, except that a [`Value::Nil`] field
///   is an `interface {}` field holding nil. Embedded (promoted) fields are expected flattened into
///   the outer struct's field list, which is what `FieldByName` sees.
/// * A map with a non-interface element type (`map[string]template.HTML`, Mattermost's
///   `Data.HTML`) is modelled as a `map[string]interface {}` holding [`Value::Html`] values. The two
///   differ only in `index` on an absent key (Go returns the element type's zero value, this model
///   returns nil) and in the type names of error messages.
///
/// Values have no methods: a template that calls a method on a Go value has no counterpart here.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A nil interface. As the root data it is `Execute(w, nil)`; inside a map, list or struct it
    /// is a present-but-nil `interface {}`.
    Nil,
    /// `bool`.
    Bool(bool),
    /// `int` (64-bit, as on every platform Mattermost ships for).
    Int(i64),
    /// `float64`.
    Float(f64),
    /// A plain Go `string` — escaped by `html/template` for the context it lands in.
    String(String),
    /// `template.HTML` — trusted markup, not escaped in an HTML text context.
    Html(String),
    /// `template.URL` — a trusted URL, exempt from the `#ZgotmplZ` scheme filter.
    Url(String),
    /// `template.CSS`.
    Css(String),
    /// `template.JS`.
    Js(String),
    /// `template.JSStr`.
    JsStr(String),
    /// `template.HTMLAttr`.
    HtmlAttr(String),
    /// `template.Srcset`.
    Srcset(String),
    /// A Go slice, modelled as `[]interface {}`.
    List(Vec<Value>),
    /// A Go `map[string]interface {}`. `range` visits it in sorted key order, as Go does.
    Map(BTreeMap<String, Value>),
    /// A Go struct: its type name as `reflect.Type.String()` prints it (`"templates.Data"`), then
    /// field name to value in declaration order.
    Struct(String, Vec<(String, Value)>),
    /// A non-nil Go pointer to a value.
    Ptr(Box<Value>),
    /// A typed nil pointer, with its Go type name as `reflect` prints it (`"*string"`). Distinct
    /// from [`Value::Nil`], a nil interface: `{{if}}` treats both as false, but `html/template`
    /// prints a nil pointer as `&lt;nil&gt;` and a nil interface as nothing.
    NilPtr(String),
}

/// The `html/template` content type of a string-kinded value (content.go:103).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentType {
    Plain,
    Css,
    Html,
    HtmlAttr,
    Js,
    JsStr,
    Url,
    Srcset,
    /// Only produced by `attrType` (attr.go), never by a value.
    Unsafe,
}

impl Value {
    /// A plain `string` value.
    pub fn str(s: impl Into<String>) -> Self {
        Value::String(s.into())
    }

    /// A `map[string]interface {}` built from pairs.
    pub fn map<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Self {
        Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// The Go type name, as `reflect.Type.String()` prints it.
    ///
    pub fn go_type(&self) -> String {
        match self {
            Value::Nil => "<nil>".to_string(),
            Value::Bool(_) => "bool".to_string(),
            Value::Int(_) => "int".to_string(),
            Value::Float(_) => "float64".to_string(),
            Value::String(_) => "string".to_string(),
            Value::Html(_) => "template.HTML".to_string(),
            Value::Url(_) => "template.URL".to_string(),
            Value::Css(_) => "template.CSS".to_string(),
            Value::Js(_) => "template.JS".to_string(),
            Value::JsStr(_) => "template.JSStr".to_string(),
            Value::HtmlAttr(_) => "template.HTMLAttr".to_string(),
            Value::Srcset(_) => "template.Srcset".to_string(),
            Value::List(_) => "[]interface {}".to_string(),
            Value::Map(_) => "map[string]interface {}".to_string(),
            Value::Struct(name, _) => name.clone(),
            Value::Ptr(v) => format!("*{}", v.go_type()),
            Value::NilPtr(t) => t.clone(),
        }
    }

    /// The string payload and content type of a string-kinded value.
    pub(crate) fn as_go_string(&self) -> Option<(&str, ContentType)> {
        match self {
            Value::String(s) => Some((s, ContentType::Plain)),
            Value::Html(s) => Some((s, ContentType::Html)),
            Value::Url(s) => Some((s, ContentType::Url)),
            Value::Css(s) => Some((s, ContentType::Css)),
            Value::Js(s) => Some((s, ContentType::Js)),
            Value::JsStr(s) => Some((s, ContentType::JsStr)),
            Value::HtmlAttr(s) => Some((s, ContentType::HtmlAttr)),
            Value::Srcset(s) => Some((s, ContentType::Srcset)),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::List(v)
    }
}

impl From<BTreeMap<String, Value>> for Value {
    fn from(m: BTreeMap<String, Value>) -> Self {
        Value::Map(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_type_names() {
        assert_eq!(
            Value::Map(BTreeMap::new()).go_type(),
            "map[string]interface {}"
        );
        assert_eq!(Value::Ptr(Box::new(Value::str("x"))).go_type(), "*string");
        assert_eq!(Value::NilPtr("*main.inner".into()).go_type(), "*main.inner");
        assert_eq!(Value::Html(String::new()).go_type(), "template.HTML");
        assert_eq!(
            Value::Struct("templates.Data".into(), vec![]).go_type(),
            "templates.Data"
        );
    }

    #[test]
    fn content_types() {
        assert_eq!(
            Value::str("a").as_go_string(),
            Some(("a", ContentType::Plain))
        );
        assert_eq!(
            Value::Url("u".into()).as_go_string(),
            Some(("u", ContentType::Url))
        );
        assert_eq!(Value::Int(1).as_go_string(), None);
    }
}
