//! The executor's `reflect.Value`: a [`Value`] plus the two facts `reflect` adds to it that
//! change template behaviour — whether it is valid at all, and whether its static type is
//! `interface {}`.

use std::borrow::Cow;

use crate::value::Value;

/// `reflect.Kind`, reduced to the kinds a [`Value`] can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Invalid,
    Interface,
    Pointer,
    Bool,
    Int,
    Float,
    String,
    Slice,
    Map,
    Struct,
}

/// A `reflect.Value`. `Val(v, true)` is a value whose static type is `interface {}` (a map or
/// list element, a nil interface field); a nil interface is always `Val(Value::Nil, true)`.
#[derive(Debug, Clone)]
pub(crate) enum Rv<'a> {
    Invalid,
    Val(Cow<'a, Value>, bool),
}

/// Projects a `Cow` onto a child value, borrowing when the parent is borrowed.
pub(crate) fn project<'a>(
    c: &Cow<'a, Value>,
    f: impl for<'x> Fn(&'x Value) -> &'x Value,
) -> Cow<'a, Value> {
    match c {
        Cow::Borrowed(b) => Cow::Borrowed(f(b)),
        Cow::Owned(o) => Cow::Owned(f(o).clone()),
    }
}

impl<'a> Rv<'a> {
    /// A borrowed value in a slot of the given static type.
    pub(crate) fn borrowed(v: &'a Value, iface: bool) -> Self {
        let iface = iface || matches!(v, Value::Nil);
        Rv::Val(Cow::Borrowed(v), iface)
    }

    /// An owned value of its own (concrete) type.
    pub(crate) fn owned(v: Value) -> Self {
        let iface = matches!(v, Value::Nil);
        Rv::Val(Cow::Owned(v), iface)
    }

    pub(crate) fn from_cow(v: Cow<'a, Value>, iface: bool) -> Self {
        let iface = iface || matches!(&*v, Value::Nil);
        Rv::Val(v, iface)
    }

    /// `reflect.Zero(interface {})`: a valid, nil interface.
    pub(crate) fn nil_iface() -> Self {
        Rv::Val(Cow::Owned(Value::Nil), true)
    }

    pub(crate) fn is_valid(&self) -> bool {
        matches!(self, Rv::Val(..))
    }

    pub(crate) fn value(&self) -> Option<&Value> {
        match self {
            Rv::Invalid => None,
            Rv::Val(v, _) => Some(v),
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Rv::Invalid => Kind::Invalid,
            Rv::Val(_, true) => Kind::Interface,
            Rv::Val(v, false) => value_kind(v),
        }
    }

    /// `reflect.Value.Type().String()`.
    pub(crate) fn type_name(&self) -> String {
        match self {
            Rv::Invalid => "<nil>".to_string(),
            Rv::Val(_, true) => "interface {}".to_string(),
            Rv::Val(v, false) => v.go_type(),
        }
    }

    /// `v.IsNil()` for the nillable kinds (false for the rest, where Go would panic).
    pub(crate) fn is_nil(&self) -> bool {
        match self {
            Rv::Invalid => false,
            Rv::Val(v, _) => matches!(&**v, Value::Nil | Value::NilPtr(_)),
        }
    }

    /// `indirectInterface` (exec.go:1089): the concrete value inside an interface, or invalid.
    pub(crate) fn indirect_interface(self) -> Rv<'a> {
        match self {
            Rv::Val(v, true) => {
                if matches!(&*v, Value::Nil) {
                    Rv::Invalid
                } else {
                    Rv::Val(v, false)
                }
            }
            other => other,
        }
    }

    /// `indirect` (exec.go:1076): through pointers and interfaces, stopping at a nil one.
    pub(crate) fn indirect(self) -> (Rv<'a>, bool) {
        let mut v = self;
        loop {
            match v {
                Rv::Val(c, true) => {
                    if matches!(&*c, Value::Nil) {
                        return (Rv::Val(c, true), true);
                    }
                    v = Rv::Val(c, false);
                }
                Rv::Val(c, false) => match &*c {
                    Value::Ptr(_) => {
                        let inner = project(&c, |x| match x {
                            Value::Ptr(b) => b,
                            other => other,
                        });
                        v = Rv::from_cow(inner, false);
                    }
                    Value::NilPtr(_) => return (Rv::Val(c, false), true),
                    _ => return (Rv::Val(c, false), false),
                },
                Rv::Invalid => return (Rv::Invalid, false),
            }
        }
    }

    /// The value as a function receiving `any` sees it: `None` for a nil interface or an
    /// invalid value (which `validateType` turns into a nil interface).
    pub(crate) fn as_any(&self) -> Option<&Value> {
        match self {
            Rv::Invalid => None,
            Rv::Val(v, _) => match &**v {
                Value::Nil => None,
                other => Some(other),
            },
        }
    }
}

pub(crate) fn value_kind(v: &Value) -> Kind {
    match v {
        Value::Nil => Kind::Interface,
        Value::Bool(_) => Kind::Bool,
        Value::Int(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::String(_)
        | Value::Html(_)
        | Value::Url(_)
        | Value::Css(_)
        | Value::Js(_)
        | Value::JsStr(_)
        | Value::HtmlAttr(_)
        | Value::Srcset(_) => Kind::String,
        Value::List(_) => Kind::Slice,
        Value::Map(_) => Kind::Map,
        Value::Struct(..) => Kind::Struct,
        Value::Ptr(_) | Value::NilPtr(_) => Kind::Pointer,
    }
}

/// `isTrue` (exec.go:324), applied to an already-`indirectInterface`d value.
pub(crate) fn is_true(v: &Rv<'_>) -> (bool, bool) {
    let Some(val) = v.value() else {
        return (false, true);
    };
    let truth = match val {
        Value::Nil => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::List(l) => !l.is_empty(),
        Value::Map(m) => !m.is_empty(),
        Value::Struct(..) => true,
        Value::Ptr(_) => true,
        Value::NilPtr(_) => false,
        other => other.as_go_string().is_some_and(|(s, _)| !s.is_empty()),
    };
    (truth, true)
}

/// `truth` (funcs.go:363).
pub(crate) fn truth(v: &Rv<'_>) -> bool {
    is_true(&v.clone().indirect_interface()).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indirect_stops_at_nil() {
        let p = Value::Ptr(Box::new(Value::Ptr(Box::new(Value::Int(3)))));
        let (v, nil) = Rv::borrowed(&p, true).indirect();
        assert!(!nil);
        assert_eq!(v.value(), Some(&Value::Int(3)));
        let n = Value::NilPtr("*int".into());
        let (v, nil) = Rv::borrowed(&n, false).indirect();
        assert!(nil);
        assert_eq!(v.kind(), Kind::Pointer);
        let (v, nil) = Rv::nil_iface().indirect();
        assert!(nil);
        assert_eq!(v.kind(), Kind::Interface);
    }

    #[test]
    fn truthiness() {
        assert_eq!(is_true(&Rv::Invalid), (false, true));
        assert!(!truth(&Rv::owned(Value::str(""))));
        assert!(truth(&Rv::owned(Value::Html("x".into()))));
        assert!(truth(&Rv::owned(Value::Struct("s".into(), vec![]))));
        assert!(!truth(&Rv::owned(Value::NilPtr("*s".into()))));
        assert!(truth(&Rv::owned(Value::Ptr(Box::new(Value::str(""))))));
    }
}
