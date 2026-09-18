//! Spike 2: decode the gob stream written by `reference/dump/spike/gobgen` and compare every
//! value with the reflection-derived expectation.
//!
//!     gobdump <dir>    reads <dir>/stream.gob and <dir>/expected.json

use anyhow::{Context, Result, bail};
use plugin_phase0::gob::{Decoder, Item, split_messages};
use serde_json::Value;

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).context("usage: gobdump <dir>")?;
    let stream = std::fs::read(format!("{dir}/stream.gob"))?;
    let expected: Vec<Value> =
        serde_json::from_slice(&std::fs::read(format!("{dir}/expected.json"))?)?;
    let mut dec = Decoder::default();
    let mut values = Vec::new();
    let (mut typedefs, mut messages, mut spans) = (0, 0, 0);
    let mut pending: Vec<u8> = Vec::new();
    let mut ends: Vec<usize> = Vec::new();
    for msg in split_messages(&stream)? {
        messages += 1;
        let item = if pending.is_empty() {
            dec.message(msg)
        } else {
            pending.extend_from_slice(msg);
            ends.push(pending.len());
            dec.message_continued(&pending, &ends)
        };
        match item.with_context(|| format!("message {messages} ({} bytes)", msg.len()))? {
            Item::NeedMore => {
                if pending.is_empty() {
                    pending.extend_from_slice(msg);
                    ends.push(pending.len());
                }
                spans += 1;
                continue;
            }
            Item::TypeDefined(_) => typedefs += 1,
            Item::Value(_, v) => values.push(v),
        }
        pending.clear();
        ends.clear();
    }
    println!("{spans} values continued into a following message");
    println!(
        "{} bytes, {messages} messages, {typedefs} type definitions, {} values",
        stream.len(),
        values.len()
    );
    let mut failed = 0;
    for (i, want) in expected.iter().enumerate() {
        let name = want["name"].as_str().unwrap_or("?");
        match values.get(i) {
            Some(got) if *got == want["value"] => println!("  ok   {name}"),
            Some(got) => {
                failed += 1;
                println!(
                    "  DIFF {name}\n    want {}\n    got  {}",
                    want["value"], got
                );
            }
            None => {
                failed += 1;
                println!("  MISSING {name}");
            }
        }
    }
    if failed > 0 || values.len() != expected.len() {
        bail!("{failed} of {} values differ", expected.len());
    }
    for (id, wt) in dec.types.iter().filter(|(id, _)| **id >= 65) {
        println!("  type {id}: {wt:?}");
    }
    Ok(())
}
