//! Differential tests against `fixtures/behaviour_markdown.json`, which records what the **real**
//! Go package answers for every input in the corpus (`reference/dump/behaviour_markdown.go`):
//! `RenderHTML`, the complete `Inspect` trace (with and without a refusing callback), `Parse`'s
//! reference definitions, and each paragraph's `ParseInlines` and `MergeInlineText` results.
//! Plus the entity table entry by entry, `strings.EqualFold` over its awkward pairs, and
//! `unicode.IsPunct`/`IsSpace` probes.
//!
//! Every row is asserted; a failing row names itself. Reading the Go branch and reasoning about
//! it is what produces confident, wrong translations — this suite is the reason the port can say
//! "byte for byte" rather than "I believe".

use serde_json::{Value, json};

use crate::blocks::Block;
use crate::inlines::Inline;
use crate::inspect::Node;
use crate::reference_definition::ReferenceDefinition;
use crate::{inspect, inspect_inline, max_len, merge_inline_text, parse, render_html};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/behaviour_markdown.json")).unwrap()
}

fn block_event(block: &Block<'_>) -> Value {
    match block {
        Block::Document(_) => json!({"type": "Document"}),
        Block::Paragraph(_) => json!({"type": "Paragraph"}),
        Block::List(l) => json!({
            "type": "List",
            "is_ordered": l.is_ordered,
            "ordered_start": l.ordered_start,
            "is_loose": l.is_loose,
            "bullet": (l.bullet_or_delimiter as char).to_string(),
        }),
        Block::ListItem(i) => json!({"type": "ListItem", "indentation": i.indentation}),
        Block::BlockQuote(_) => json!({"type": "BlockQuote"}),
        Block::FencedCode(f) => json!({"type": "FencedCode", "info": f.info(), "code": f.code()}),
        Block::IndentedCode(c) => json!({"type": "IndentedCode", "code": c.code()}),
    }
}

fn inline_event(inline: &Inline<'_>) -> Value {
    match inline {
        Inline::Text(t) => {
            json!({"type": "Text", "text": t.text, "pos": t.range.position, "end": t.range.end})
        }
        Inline::CodeSpan(c) => json!({"type": "CodeSpan", "code": c.code}),
        Inline::HardLineBreak => json!({"type": "HardLineBreak"}),
        Inline::SoftLineBreak => json!({"type": "SoftLineBreak"}),
        Inline::InlineLink(l) => {
            json!({"type": "InlineLink", "destination": l.destination(), "title": l.title()})
        }
        Inline::InlineImage(l) => {
            json!({"type": "InlineImage", "destination": l.destination(), "title": l.title()})
        }
        Inline::ReferenceLink(l) => json!({
            "type": "ReferenceLink", "destination": l.destination(), "title": l.title(), "label": l.label(),
        }),
        Inline::ReferenceImage(l) => json!({
            "type": "ReferenceImage", "destination": l.destination(), "title": l.title(), "label": l.label(),
        }),
        Inline::Autolink(a) => json!({"type": "Autolink", "destination": a.destination()}),
        Inline::Emoji(e) => json!({"type": "Emoji", "name": e.name}),
    }
}

fn node_event(node: Option<&Node<'_>>) -> Value {
    match node {
        None => json!({"type": "nil"}),
        Some(Node::Block(b)) => block_event(b),
        Some(Node::Inline(i)) => inline_event(i),
    }
}

/// The refusing callback of `markdownPrune` in the generator.
fn prune(node: &Node<'_>) -> bool {
    !matches!(
        node,
        Node::Block(Block::BlockQuote(_) | Block::List(_))
            | Node::Inline(Inline::InlineLink(_) | Inline::InlineImage(_))
    )
}

fn trace(input: &str, pruning: bool) -> Value {
    let mut out = Vec::new();
    inspect(input, |node| {
        out.push(node_event(node));
        match node {
            Some(node) if pruning => prune(node),
            _ => true,
        }
    });
    Value::Array(out)
}

fn inline_trace(inlines: &[Inline<'_>]) -> Vec<Value> {
    let mut out = Vec::new();
    for inline in inlines {
        inspect_inline(inline, |i| {
            out.push(match i {
                None => json!({"type": "nil"}),
                Some(i) => inline_event(i),
            });
            true
        });
    }
    out
}

fn collect_paragraphs(block: &Block<'_>, defs: &[ReferenceDefinition<'_>], out: &mut Vec<Value>) {
    if let Block::Paragraph(p) = block {
        let raw = p.parse_inlines(defs);
        let inlines = inline_trace(&raw);
        let merged = inline_trace(&merge_inline_text(raw));
        out.push(json!({"inlines": inlines, "merged": merged}));
        return;
    }
    for child in block.children() {
        collect_paragraphs(child, defs, out);
    }
}

fn case_input(case: &Value) -> String {
    let unit = case["input"].as_str().unwrap();
    match case["repeat"].as_u64() {
        Some(n) if n > 0 => unit.repeat(n as usize),
        _ => unit.to_owned(),
    }
}

#[test]
fn max_len_default_matches_go() {
    assert_eq!(
        fixture()["max_len_default"].as_u64().unwrap() as usize,
        max_len()
    );
}

#[test]
fn corpus_is_not_vacuous() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 150, "{} cases", cases.len());
    let with_events = cases
        .iter()
        .filter(|c| !c["trace"].as_array().unwrap().is_empty())
        .count();
    assert!(
        with_events > cases.len() - 10,
        "{with_events} of {} rows produce events",
        cases.len()
    );
    assert!(cases.iter().any(|c| c["repeat"].as_u64().unwrap_or(0) > 0));
}

#[test]
fn corpus_render_html() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = case_input(case);
        let want = case["html"].as_str().unwrap();
        let got = render_html(&input);
        if got != want {
            failures.push(format!(
                "{name}\n  input: {input:?}\n  want:  {want:?}\n  got:   {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows differ:\n{}",
        failures.len(),
        fixture["cases"].as_array().unwrap().len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_inspect_trace() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = case_input(case);
        for (key, pruning) in [("trace", false), ("trace_prune", true)] {
            let got = trace(&input, pruning);
            if got != case[key] {
                failures.push(format!(
                    "{name} ({key})\n  input: {input:?}\n  want:  {}\n  got:   {got}",
                    case[key]
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} rows differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_definitions_and_paragraphs() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = case_input(case);
        let (document, defs) = parse(&input);
        let got_defs: Vec<Value> = defs
            .iter()
            .map(
                |d| json!({"label": d.label(), "destination": d.destination(), "title": d.title()}),
            )
            .collect();
        if Value::Array(got_defs.clone()) != case["definitions"] {
            failures.push(format!(
                "{name} (definitions)\n  input: {input:?}\n  want:  {}\n  got:   {}",
                case["definitions"],
                Value::Array(got_defs)
            ));
        }
        let mut paragraphs = Vec::new();
        collect_paragraphs(&Block::Document(document), &defs, &mut paragraphs);
        if Value::Array(paragraphs.clone()) != case["paragraphs"] {
            failures.push(format!(
                "{name} (paragraphs)\n  input: {input:?}\n  want:  {}\n  got:   {}",
                case["paragraphs"],
                Value::Array(paragraphs)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} rows differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn entity_table_matches_go() {
    let fixture = fixture();
    let entities = fixture["entities"].as_object().unwrap();
    assert_eq!(entities.len(), crate::html_entities::HTML_ENTITIES.len());
    for (name, value) in entities {
        assert_eq!(
            crate::character_reference(name),
            value.as_str().unwrap(),
            "&{name};"
        );
    }
    // Sorted, so the binary search is sound.
    assert!(
        crate::html_entities::HTML_ENTITIES
            .windows(2)
            .all(|w| w[0].0 < w[1].0)
    );
}

#[test]
fn equal_fold_matches_go() {
    let fixture = fixture();
    for case in fixture["equal_fold"].as_array().unwrap() {
        let (a, b) = (case["a"].as_str().unwrap(), case["b"].as_str().unwrap());
        let want = case["eq"].as_bool().unwrap();
        assert_eq!(
            crate::unicode::equal_fold(a, b),
            want,
            "EqualFold({a:?}, {b:?})"
        );
        assert_eq!(
            crate::unicode::equal_fold(b, a),
            want,
            "EqualFold({b:?}, {a:?})"
        );
    }
}

#[test]
fn rune_probes_match_go() {
    let fixture = fixture();
    for probe in fixture["rune_probes"].as_array().unwrap() {
        let c = probe["rune"].as_str().unwrap().chars().next().unwrap();
        assert_eq!(
            crate::unicode::is_punct(c),
            probe["is_punct"].as_bool().unwrap(),
            "IsPunct({c:?})"
        );
        assert_eq!(
            crate::unicode::is_space(c),
            probe["is_space"].as_bool().unwrap(),
            "IsSpace({c:?})"
        );
    }
}
