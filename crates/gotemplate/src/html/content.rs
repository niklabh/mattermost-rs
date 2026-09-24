//! Port of `html/template/content.go`: typed content and `stringify`, which every escaper starts
//! with.

use crate::value::{ContentType, Value};

/// `indirect` (content.go:118): dereferences non-nil pointers down to the base value.
pub(crate) fn indirect(v: &Value) -> &Value {
    let mut v = v;
    while let Value::Ptr(inner) = v {
        v = inner;
    }
    v
}

/// `stringify` (content.go:160): the string form of the arguments and its content type.
///
/// A single argument of a typed-content type keeps its type; anything else is `fmt.Sprint` of
/// the (dereferenced) arguments with untyped nils dropped — so an absent map value prints as
/// nothing while a typed nil pointer prints as `<nil>`.
pub(crate) fn stringify(args: &[Option<&Value>]) -> (String, ContentType) {
    if args.len() == 1
        && let Some(v) = args[0]
        && let Some((s, t)) = indirect(v).as_go_string()
    {
        // Only the exact types `string`, `CSS`, `HTML`, ... match Go's type switch; every
        // string-kinded Value is one of them.
        return (s.to_string(), t);
    }
    let kept: Vec<Option<&Value>> = args
        .iter()
        .filter_map(|a| match a {
            None | Some(Value::Nil) => None,
            Some(v) => Some(Some(indirect(v))),
        })
        .collect();
    (crate::fmt::sprint(&kept), ContentType::Plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stringify_rules() {
        let h = Value::Html("<b>".into());
        assert_eq!(
            stringify(&[Some(&h)]),
            ("<b>".to_string(), ContentType::Html)
        );
        let p = Value::Ptr(Box::new(Value::Url("u".into())));
        assert_eq!(stringify(&[Some(&p)]), ("u".to_string(), ContentType::Url));
        assert_eq!(stringify(&[None]), (String::new(), ContentType::Plain));
        let n = Value::NilPtr("*string".into());
        assert_eq!(
            stringify(&[Some(&n)]),
            ("<nil>".to_string(), ContentType::Plain)
        );
        let i = Value::Int(3);
        assert_eq!(
            stringify(&[Some(&h), None, Some(&i)]),
            ("<b>3".to_string(), ContentType::Plain)
        );
    }
}
