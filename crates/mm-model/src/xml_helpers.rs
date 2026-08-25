//! Port of `model/xml_helpers.go` — the XML representation of `StringMap` and `StringInterface`.
//!
//! Go's `encoding/xml` cannot marshal a map at all, so these two types carry hand-written
//! `MarshalXML`/`UnmarshalXML` that render a map as a sequence of
//! `<Entry key="…" value="…"/>` elements. The compliance and message exports rely on it.
//!
//! # Three details that decide whether an export round-trips
//!
//! - **Keys are sorted** before encoding, explicitly "for deterministic output" — Go's map
//!   iteration order is randomised, so without the sort two exports of the same data would
//!   differ. [`string_map_entries`] and [`string_interface_entries`] sort the same way, and this
//!   crate's `StringMap`/`StringInterface` are already ordered maps, so the order agrees.
//! - **A nil map encodes as an empty element**, not as an absent one: `EncodeElement(struct{}{})`
//!   writes `<Name></Name>`.
//! - **`StringInterface` marks non-strings with `type="json"`** and stores the JSON text in the
//!   `value` attribute. A `nil` value is `type="json"` with the literal `null`, and a **string**
//!   value carries no `type` at all — which is what makes `"1"` and `1` distinguishable on the
//!   way back.
//!
//! # Only the encoding half is ported
//!
//! Decoding needs a streaming XML parser (Go walks tokens and calls `d.Skip()` on anything that
//! is not an `Entry`), and this crate has no XML dependency — the same call as `saml.go`'s
//! metadata tree and `shared_channel.go`'s `SyncMsg`. What is ported is the entry projection and
//! the exact element text Go writes, which is the half a Rust exporter needs; the decoder lands
//! with an XML crate when a route requires it.

use crate::utils::{StringInterface, StringMap};

/// Port of `xmlStringMapEntry` (xml_helpers.go:14).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmlStringMapEntry {
    pub key: String,
    pub value: String,
}

/// Port of `xmlStringInterfaceEntry` (xml_helpers.go:77).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmlStringInterfaceEntry {
    pub key: String,
    pub value: String,
    /// `"json"` when `value` holds JSON text rather than a plain string; `omitempty`, so a
    /// string entry writes no `type` attribute at all.
    pub json_type: String,
}

/// The entries a `StringMap` encodes to, in Go's order.
pub fn string_map_entries(m: &StringMap) -> Vec<XmlStringMapEntry> {
    // `StringMap` is a `BTreeMap`, so iteration is already sorted — the same order Go's explicit
    // `sort.Strings` produces.
    m.iter()
        .map(|(key, value)| XmlStringMapEntry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// The entries a `StringInterface` encodes to, in Go's order.
///
/// Port of the type switch in `(StringInterface).MarshalXML` (xml_helpers.go:104): a string is
/// stored as-is, `nil` becomes `type="json"` with `null`, and everything else is JSON-encoded.
///
/// Go returns an error when a value cannot be marshalled; `serde_json::Value` always can, so the
/// error branch is unreachable — the signature keeps it anyway so a caller written against Go's
/// shape needs no change.
pub fn string_interface_entries(
    m: &StringInterface,
) -> Result<Vec<XmlStringInterfaceEntry>, XmlHelpersError> {
    let mut out = Vec::with_capacity(m.len());
    for (key, value) in m.iter() {
        let entry = match value {
            serde_json::Value::String(text) => XmlStringInterfaceEntry {
                key: key.clone(),
                value: text.clone(),
                json_type: String::new(),
            },
            serde_json::Value::Null => XmlStringInterfaceEntry {
                key: key.clone(),
                value: "null".to_string(),
                json_type: "json".to_string(),
            },
            other => {
                let text = crate::utils::go_json_marshal(other)
                    .map_err(|_| XmlHelpersError::Marshal(key.clone()))?;
                XmlStringInterfaceEntry {
                    key: key.clone(),
                    value: text,
                    json_type: "json".to_string(),
                }
            }
        };
        out.push(entry);
    }
    Ok(out)
}

/// Go's `encoding/xml` attribute escaping.
///
/// It escapes more than the minimum: `&`, `<`, `>`, `"` and `'` become entities, and the three
/// whitespace characters that an XML parser would otherwise normalise away — tab, newline and
/// carriage return — become **numeric** references, so a value containing a newline survives a
/// round trip.
pub fn escape_xml_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

/// Encode a `StringMap` the way `MarshalXML` does, under an element named `name`.
///
/// A `None` map produces the empty element Go's `EncodeElement(struct{}{}, start)` writes.
pub fn encode_string_map(name: &str, m: Option<&StringMap>) -> String {
    let Some(m) = m else {
        return format!("<{name}></{name}>");
    };

    let mut out = format!("<{name}>");
    for entry in string_map_entries(m) {
        out.push_str(&format!(
            "<Entry key=\"{}\" value=\"{}\"></Entry>",
            escape_xml_attr(&entry.key),
            escape_xml_attr(&entry.value)
        ));
    }
    out.push_str(&format!("</{name}>"));
    out
}

/// Encode a `StringInterface` the way `MarshalXML` does, under an element named `name`.
pub fn encode_string_interface(
    name: &str,
    m: Option<&StringInterface>,
) -> Result<String, XmlHelpersError> {
    let Some(m) = m else {
        return Ok(format!("<{name}></{name}>"));
    };

    let mut out = format!("<{name}>");
    for entry in string_interface_entries(m)? {
        out.push_str(&format!(
            "<Entry key=\"{}\" value=\"{}\"",
            escape_xml_attr(&entry.key),
            escape_xml_attr(&entry.value)
        ));
        if !entry.json_type.is_empty() {
            out.push_str(&format!(" type=\"{}\"", escape_xml_attr(&entry.json_type)));
        }
        out.push_str("></Entry>");
    }
    out.push_str(&format!("</{name}>"));
    Ok(out)
}

/// The errors `xml_helpers.go` returns. Both wrap a key, as Go's `%q`-quoted messages do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlHelpersError {
    #[error("failed to marshal StringInterface value for key {}", crate::utils::go_quote(.0))]
    Marshal(String),
    /// Produced by the decoder, which is not ported — kept so the error set matches Go's.
    #[error(
        "failed to unmarshal StringInterface JSON value for key {}",
        crate::utils::go_quote(.0)
    )]
    Unmarshal(String),
}

#[cfg(test)]
mod go_parity {
    use super::{encode_string_interface, encode_string_map};
    use crate::utils::{StringInterface, StringMap};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// The oracle records the **whole element** Go's encoder wrote — `<Probe><Props>…</Props>`
    /// — so this asserts the port's entry rendering, its attribute escaping and its key order in
    /// one comparison rather than three separate guesses.
    #[test]
    fn string_map_xml_matches_go() {
        let oracle = oracle();
        let cases = oracle["string_map_xml"].as_array().unwrap();
        assert!(cases.len() >= 8);
        for case in cases {
            let input: Option<StringMap> = match &case["in"] {
                serde_json::Value::Null => None,
                value => Some(serde_json::from_value(value.clone()).unwrap()),
            };
            let got = format!(
                "<Probe>{}</Probe>",
                encode_string_map("Props", input.as_ref())
            );
            assert_eq!(
                got,
                case["out"].as_str().unwrap(),
                "StringMap {:?}",
                case["in"]
            );
        }
    }

    /// The `type="json"` half: a string passes through untagged, and everything else — `null`
    /// included — is JSON-encoded and tagged.
    #[test]
    fn string_interface_xml_matches_go() {
        let oracle = oracle();
        let cases = oracle["string_iface_xml"].as_array().unwrap();
        assert!(cases.len() >= 7);
        for case in cases {
            let input: Option<StringInterface> = match &case["in"] {
                serde_json::Value::Null => None,
                value => Some(serde_json::from_value(value.clone()).unwrap()),
            };
            let got = format!(
                "<Probe>{}</Probe>",
                encode_string_interface("Props", input.as_ref()).unwrap()
            );
            assert_eq!(
                got,
                case["out"].as_str().unwrap(),
                "StringInterface {:?}",
                case["in"]
            );
        }
    }
}
