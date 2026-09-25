//! Port of `encoding/csv`'s `Writer` with its defaults (`Comma: ','`, `UseCRLF: false`) — the
//! encoder the user-export report is written with (`App.saveCSVChunk`, `compileCSVChunks`,
//! app/report.go).
//!
//! # When a field is quoted
//!
//! `fieldNeedsQuotes` (encoding/csv/writer.go): never when empty; always for the exact field
//! `\.`; when any byte is a comma, a double quote, CR or LF; and otherwise only when the **first**
//! rune is Unicode whitespace — a trailing space is written bare. Inside quotes a `"` doubles and
//! CR and LF are written as they are. Each record ends in a single `\n`.
//!
//! Pinned against Go's own writer by `fixtures/behaviour_go_stdlib.json`'s `encoding_csv` corpus.

/// Port of `(*csv.Writer).Write` followed by `Flush`, for one record: the bytes Go appends.
///
/// Go's `Write` can only fail on an invalid delimiter or an I/O error; neither exists for the
/// default comma writing into memory, so this cannot fail.
pub fn write_record(out: &mut String, record: &[String]) {
    for (n, field) in record.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        if !field_needs_quotes(field) {
            out.push_str(field);
            continue;
        }
        out.push('"');
        for c in field.chars() {
            if c == '"' {
                out.push_str("\"\"");
            } else {
                out.push(c);
            }
        }
        out.push('"');
    }
    out.push('\n');
}

/// Port of `(*csv.Writer).fieldNeedsQuotes` for the default comma.
fn field_needs_quotes(field: &str) -> bool {
    if field.is_empty() {
        return false;
    }
    if field == r"\." {
        return true;
    }
    if field
        .bytes()
        .any(|b| matches!(b, b'\n' | b'\r' | b'"' | b','))
    {
        return true;
    }
    // `unicode.IsSpace` of the first rune: the Latin-1 six plus U+0085 and U+00A0, and above
    // that the `White_Space` property — which is what `char::is_whitespace` implements.
    field.chars().next().is_some_and(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(record: &[&str]) -> String {
        let record: Vec<String> = record.iter().map(|s| (*s).to_owned()).collect();
        let mut out = String::new();
        write_record(&mut out, &record);
        out
    }

    #[test]
    fn quoting_follows_the_first_rune_and_the_four_bytes() {
        assert_eq!(line(&[""]), "\n");
        assert_eq!(line(&["a", "", "b"]), "a,,b\n");
        assert_eq!(line(&[r"\."]), "\"\\.\"\n");
        assert_eq!(line(&["x,y"]), "\"x,y\"\n");
        assert_eq!(line(&["say \"hi\""]), "\"say \"\"hi\"\"\"\n");
        assert_eq!(line(&[" lead"]), "\" lead\"\n");
        assert_eq!(line(&["trail "]), "trail \n");
        assert_eq!(line(&["a\r\nb"]), "\"a\r\nb\"\n");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    #[test]
    fn every_record_is_written_as_go_writes_it() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
                .unwrap();
        let cases = oracle["encoding_csv"].as_array().unwrap();
        assert!(cases.len() >= 20);
        for case in cases {
            let record: Vec<String> = case["record"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(case["error"], "", "{record:?}");
            let mut out = String::new();
            write_record(&mut out, &record);
            assert_eq!(out, case["out"].as_str().unwrap(), "{record:?}");
        }
    }
}
