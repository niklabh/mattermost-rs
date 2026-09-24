//! Parity against Go: every case of `fixtures/behaviour_gotemplate.json` (written by
//! `reference/dump/behaviour_gotemplate.go`, which runs the real `text/template`,
//! `html/template` and Mattermost's `templates.New`) must produce byte-identical output, or the
//! identical error string.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value as J;

use crate::{HtmlTemplates, MissingKey, TextTemplates, Value};

/// Cases whose Go behaviour this port deliberately does not reproduce, with the reason.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[
    (
        "parse/complex_rejected",
        "complex constants are rejected at parse time (see parse::new_number)",
    ),
    (
        "fn/index_typed_map_missing",
        "Value::Map is map[string]interface {}: index of an absent key is nil, not the zero of \
         a typed element (see Value)",
    ),
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture() -> J {
    let path = root().join("fixtures/behaviour_gotemplate.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Rebuilds a `Value` from the oracle's type-tagged encoding.
pub(crate) fn decode(j: &J) -> Value {
    let t = j["t"].as_str().unwrap();
    let v = &j["v"];
    let s = || v.as_str().unwrap().to_string();
    match t {
        "nil" => Value::Nil,
        "bool" => Value::Bool(v.as_bool().unwrap()),
        "int" => Value::Int(v.as_i64().unwrap()),
        "float" => Value::Float(match v.as_str().unwrap() {
            "NaN" => f64::NAN,
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            other => other.parse().unwrap(),
        }),
        "string" => Value::String(s()),
        "html" => Value::Html(s()),
        "url" => Value::Url(s()),
        "css" => Value::Css(s()),
        "js" => Value::Js(s()),
        "jsstr" => Value::JsStr(s()),
        "htmlattr" => Value::HtmlAttr(s()),
        "srcset" => Value::Srcset(s()),
        "list" => Value::List(v.as_array().unwrap().iter().map(decode).collect()),
        "map" => Value::Map(
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, x)| (k.clone(), decode(x)))
                .collect::<BTreeMap<_, _>>(),
        ),
        "struct" => Value::Struct(
            j["name"].as_str().unwrap().to_string(),
            v.as_array()
                .unwrap()
                .iter()
                .map(|f| (f[0].as_str().unwrap().to_string(), decode(&f[1])))
                .collect(),
        ),
        "ptr" => {
            if v.is_null() {
                Value::NilPtr(j["type"].as_str().unwrap().to_string())
            } else {
                Value::Ptr(Box::new(decode(v)))
            }
        }
        other => panic!("unknown tag {other}"),
    }
}

fn missing_key(opt: &str) -> Option<MissingKey> {
    match opt {
        "" => None,
        "missingkey=zero" => Some(MissingKey::Zero),
        "missingkey=error" => Some(MissingKey::Error),
        "missingkey=default" | "missingkey=invalid" => Some(MissingKey::Invalid),
        other => panic!("unknown option {other}"),
    }
}

#[derive(Default)]
struct Tally {
    run: usize,
    failures: Vec<String>,
}

impl Tally {
    fn check(&mut self, case: &str, what: &str, got: Result<String, String>, run: &J) {
        self.run += 1;
        let want_err = run["err"].as_str();
        let ok = match (&got, want_err) {
            (Ok(out), None) => out == run["out"].as_str().unwrap(),
            (Err(e), Some(w)) => e == w,
            _ => false,
        };
        if !ok {
            let want = match want_err {
                Some(e) => format!("Err({e:?})"),
                None => format!("Ok({:?})", run["out"].as_str().unwrap()),
            };
            self.failures
                .push(format!("{case} [{what}]\n   got: {got:?}\n  want: {want}"));
        }
    }

    fn finish(self, label: &str) {
        if !self.failures.is_empty() {
            let shown: Vec<&String> = self.failures.iter().take(40).collect();
            panic!(
                "{label}: {} of {} runs differ from Go:\n{}",
                self.failures.len(),
                self.run,
                shown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        assert!(self.run > 0, "{label}: no runs");
    }
}

fn run_cases(cases: &J, html: bool, label: &str) {
    let mut tally = Tally::default();
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        if KNOWN_DIVERGENCES.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let sources: Vec<(String, String)> = c["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s[0].as_str().unwrap().to_string(),
                    s[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let data = decode(&c["data"]);
        let opt = missing_key(c["option"].as_str().unwrap_or(""));
        let parse_err = c["parse_err"].as_str();
        if html {
            let built = (|| {
                let t = HtmlTemplates::parse(&sources[0].0, &sources[0].1)?;
                for (n, s) in &sources[1..] {
                    t.add(n, s)?;
                }
                Ok::<_, crate::Error>(t)
            })();
            match (built, parse_err) {
                (Err(e), Some(w)) => {
                    tally.run += 1;
                    if e.to_string() != w {
                        tally
                            .failures
                            .push(format!("{name} [parse]\n   got: {e}\n  want: {w}"));
                    }
                }
                (Err(e), None) => {
                    tally.run += 1;
                    tally
                        .failures
                        .push(format!("{name} [parse]\n   got: {e}\n  want: success"));
                }
                (Ok(_), Some(w)) => {
                    tally.run += 1;
                    tally
                        .failures
                        .push(format!("{name} [parse]\n   got: success\n  want: {w}"));
                }
                (Ok(t), None) => {
                    if let Some(m) = opt {
                        t.set_missing_key(m);
                    }
                    for run in c["runs"].as_array().unwrap() {
                        let exec = run["name"].as_str().unwrap();
                        let got = t.execute(exec, &data).map_err(|e| e.to_string());
                        tally.check(name, exec, got, run);
                    }
                }
            }
        } else {
            let built = (|| {
                let mut t = TextTemplates::new(&sources[0].0);
                for (n, s) in &sources {
                    t.add(n, s)?;
                }
                Ok::<_, crate::Error>(t)
            })();
            match (built, parse_err) {
                (Err(e), Some(w)) => {
                    tally.run += 1;
                    if e.to_string() != w {
                        tally
                            .failures
                            .push(format!("{name} [parse]\n   got: {e}\n  want: {w}"));
                    }
                }
                (Err(e), None) => {
                    tally.run += 1;
                    tally
                        .failures
                        .push(format!("{name} [parse]\n   got: {e}\n  want: success"));
                }
                (Ok(_), Some(w)) => {
                    tally.run += 1;
                    tally
                        .failures
                        .push(format!("{name} [parse]\n   got: success\n  want: {w}"));
                }
                (Ok(mut t), None) => {
                    if let Some(m) = opt {
                        t.set_missing_key(m);
                    }
                    for run in c["runs"].as_array().unwrap() {
                        let exec = run["name"].as_str().unwrap();
                        let got = t.execute_template(exec, &data).map_err(|e| e.to_string());
                        tally.check(name, exec, got, run);
                    }
                }
            }
        }
    }
    tally.finish(label);
}

#[test]
fn html_escaper_corpus() {
    run_cases(&fixture()["html"], true, "html");
}

#[test]
fn text_corpus() {
    run_cases(&fixture()["text"], false, "text");
}

#[test]
fn text_corpus_through_html_template() {
    run_cases(&fixture()["text_as_html"], true, "text_as_html");
}

#[test]
fn mattermost_templates() {
    let f = fixture();
    let mm = &f["mattermost"];
    let dir = root().join("reference/mattermost/server/templates");
    // The files Go globbed are the files this test globs.
    for rec in mm["files"].as_array().unwrap() {
        let name = rec["name"].as_str().unwrap();
        let len = std::fs::metadata(dir.join(name)).unwrap().len();
        assert_eq!(
            len,
            rec["len"].as_u64().unwrap(),
            "{name} changed since the fixture was generated"
        );
    }
    let mut tally = Tally::default();
    for variant in mm["variants"].as_array().unwrap() {
        let vname = variant["variant"].as_str().unwrap();
        let data = decode(&variant["data"]);
        let t = HtmlTemplates::parse_glob_html(&dir).unwrap();
        let want_names: Vec<String> = mm["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_string())
            .collect();
        assert_eq!(t.names(), want_names);
        for run in variant["runs"].as_array().unwrap() {
            let name = run["name"].as_str().unwrap();
            let got = t.execute(name, &data).map_err(|e| e.to_string());
            tally.check(vname, name, got, run);
        }
    }
    tally.finish("mattermost");
}

/// The fixture must exercise what it claims to: no variant's data may be empty, and every
/// variant but the nil-Props one must reach real output for most templates.
#[test]
fn fixture_is_populated() {
    let f = fixture();
    let mm = &f["mattermost"];
    assert!(mm["names"].as_array().unwrap().len() > 60);
    for variant in mm["variants"].as_array().unwrap() {
        let runs = variant["runs"].as_array().unwrap();
        let ok = runs.iter().filter(|r| r["err"].is_null()).count();
        assert!(ok + 3 >= runs.len(), "{} errors", variant["variant"]);
    }
    assert!(f["html"].as_array().unwrap().len() > 2000);
    assert!(f["text"].as_array().unwrap().len() > 100);
}

/// `html.UnescapeString`, which html/template runs over attribute values before their
/// transitions: the decoded text decides the context, so it is checked directly.
#[test]
fn html_unescape_string() {
    let f = fixture();
    let cases = f["unescape"].as_array().unwrap();
    assert!(!cases.is_empty());
    for c in cases {
        let input = c["in"].as_str().unwrap();
        let got = crate::html::entity::unescape_string(input.as_bytes());
        assert_eq!(
            String::from_utf8_lossy(&got),
            c["out"].as_str().unwrap(),
            "{input:?}"
        );
    }
}
