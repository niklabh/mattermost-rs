//! Port of `html/template/attr.go`: the content type of an attribute's value, by name.

use crate::value::ContentType;

/// `attrTypeMap` (attr.go:23).
fn attr_type_map(name: &str) -> Option<ContentType> {
    use ContentType::*;
    Some(match name {
        "accept" => Plain,
        "accept-charset" => Unsafe,
        "action" => Url,
        "alt" => Plain,
        "archive" => Url,
        "async" => Unsafe,
        "autocomplete" => Plain,
        "autofocus" => Plain,
        "autoplay" => Plain,
        "background" => Url,
        "border" => Plain,
        "checked" => Plain,
        "cite" => Url,
        "challenge" => Unsafe,
        "charset" => Unsafe,
        "class" => Plain,
        "classid" => Url,
        "codebase" => Url,
        "cols" => Plain,
        "colspan" => Plain,
        "content" => Unsafe,
        "contenteditable" => Plain,
        "contextmenu" => Plain,
        "controls" => Plain,
        "coords" => Plain,
        "crossorigin" => Unsafe,
        "data" => Url,
        "datetime" => Plain,
        "default" => Plain,
        "defer" => Unsafe,
        "dir" => Plain,
        "dirname" => Plain,
        "disabled" => Plain,
        "draggable" => Plain,
        "dropzone" => Plain,
        "enctype" => Unsafe,
        "for" => Plain,
        "form" => Unsafe,
        "formaction" => Url,
        "formenctype" => Unsafe,
        "formmethod" => Unsafe,
        "formnovalidate" => Unsafe,
        "formtarget" => Plain,
        "headers" => Plain,
        "height" => Plain,
        "hidden" => Plain,
        "high" => Plain,
        "href" => Url,
        "hreflang" => Plain,
        "http-equiv" => Unsafe,
        "icon" => Url,
        "id" => Plain,
        "ismap" => Plain,
        "keytype" => Unsafe,
        "kind" => Plain,
        "label" => Plain,
        "lang" => Plain,
        "language" => Unsafe,
        "list" => Plain,
        "longdesc" => Url,
        "loop" => Plain,
        "low" => Plain,
        "manifest" => Url,
        "max" => Plain,
        "maxlength" => Plain,
        "media" => Plain,
        "mediagroup" => Plain,
        "method" => Unsafe,
        "min" => Plain,
        "multiple" => Plain,
        "name" => Plain,
        "novalidate" => Unsafe,
        "open" => Plain,
        "optimum" => Plain,
        "pattern" => Unsafe,
        "placeholder" => Plain,
        "poster" => Url,
        "profile" => Url,
        "preload" => Plain,
        "pubdate" => Plain,
        "radiogroup" => Plain,
        "readonly" => Plain,
        "rel" => Unsafe,
        "required" => Plain,
        "reversed" => Plain,
        "rows" => Plain,
        "rowspan" => Plain,
        "sandbox" => Unsafe,
        "spellcheck" => Plain,
        "scope" => Plain,
        "scoped" => Plain,
        "seamless" => Plain,
        "selected" => Plain,
        "shape" => Plain,
        "size" => Plain,
        "sizes" => Plain,
        "span" => Plain,
        "src" => Url,
        "srcdoc" => Html,
        "srclang" => Plain,
        "srcset" => Srcset,
        "start" => Plain,
        "step" => Plain,
        "style" => Css,
        "tabindex" => Plain,
        "target" => Plain,
        "title" => Plain,
        "type" => Unsafe,
        "usemap" => Url,
        "value" => Unsafe,
        "width" => Plain,
        "wrap" => Plain,
        "xmlns" => Url,
        _ => return None,
    })
}

/// `attrType` (attr.go:151): the content type of the named (lower-cased) attribute's value.
pub(crate) fn attr_type(name: &str) -> ContentType {
    let mut name = name;
    if let Some(rest) = name.strip_prefix("data-") {
        name = rest;
    } else if let Some((prefix, short)) = name.split_once(':') {
        if prefix == "xmlns" {
            return ContentType::Url;
        }
        name = short;
    }
    if let Some(t) = attr_type_map(name) {
        return t;
    }
    if name.starts_with("on") {
        return ContentType::Js;
    }
    if name.contains("src") || name.contains("uri") || name.contains("url") {
        return ContentType::Url;
    }
    ContentType::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_types() {
        assert_eq!(attr_type("href"), ContentType::Url);
        assert_eq!(attr_type("data-href"), ContentType::Url);
        assert_eq!(attr_type("xmlns:foo"), ContentType::Url);
        assert_eq!(attr_type("foo:href"), ContentType::Url);
        assert_eq!(attr_type("onclick"), ContentType::Js);
        assert_eq!(attr_type("data-onx"), ContentType::Js);
        assert_eq!(attr_type("myurl"), ContentType::Url);
        assert_eq!(attr_type("style"), ContentType::Css);
        assert_eq!(attr_type("title"), ContentType::Plain);
        assert_eq!(attr_type("onion"), ContentType::Js);
    }
}
