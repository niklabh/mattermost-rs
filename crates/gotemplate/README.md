# gotemplate

A byte-exact Rust port of Go's [`text/template`](https://pkg.go.dev/text/template) and
[`html/template`](https://pkg.go.dev/html/template) (Go 1.26): the same template language, the
same builtins, the same error text, and `html/template`'s contextual autoescaper making the same
choice Go makes in every context — HTML text, attributes, URLs, CSS and JavaScript.

```rust
use gotemplate::{HtmlTemplates, TextTemplates, Value};

let t = HtmlTemplates::parse(
    "greeting",
    r#"<a href="{{.URL}}" title="{{.Name}}">Hi {{.Name}}</a>"#,
).unwrap();
let data = Value::map([
    ("Name", Value::str("<Bob>")),
    ("URL", Value::str("javascript:alert(1)")),
]);
assert_eq!(
    t.execute("greeting", &data).unwrap(),
    r##"<a href="#ZgotmplZ" title="&lt;Bob&gt;">Hi &lt;Bob&gt;</a>"##,
);

let t = TextTemplates::parse(
    "list",
    r#"{{range $i, $e := .}}{{if $i}}, {{end}}{{printf "%q" $e}}{{end}}"#,
).unwrap();
assert_eq!(t.execute(&Value::from(vec![Value::str("a"), Value::str("b")])).unwrap(), r#""a", "b""#);
```

Both expected strings are what Go 1.26 prints for the same template and data.

## Why a port

If a Go program renders something and a Rust program has to render the *same bytes* — an e-mail
body, a generated config, a page two implementations must agree on — a different template
engine produces *a* result, not *the* result. This crate ports the Go packages line by line and
is tested against Go itself: a Go program runs the real packages over a corpus of templates and
data, and the tests assert byte equality with its output, error messages included.

## Data

Go templates walk arbitrary Go values through reflection. Here the data is a `Value` tree that
models the Go values a template can see: scalars, slices, maps with sorted keys, structs with
ordered fields, pointers and nil pointers, and `html/template`'s typed strings (`template.HTML`,
`template.URL`, `template.CSS`, `template.JS`, …), which the escaper trusts as Go's does.

## Differences from Go

- There is no reflection: methods on data values, and custom `FuncMap` functions, are not
  supported. The builtin functions are.
- Only what the port's user needed is exposed: parse, add, execute, `Option("missingkey=…")`,
  and `ParseFiles` / `ParseGlob`-style loading.

## Licence

BSD-3-Clause, the licence of the Go code it is ported from (`LICENSE-GO`).
