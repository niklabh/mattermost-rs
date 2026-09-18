//! The log lines a go-plugin plugin writes to its process stderr, in go-hclog's JSON form, and how
//! the host turns them into log events (client.go, `logStderr`; log_entry.go, `parseJSON`).

use serde_json::{Map, Value};

/// go-hclog's JSON timestamp layout, `2006-01-02T15:04:05.000000Z07:00`.
pub const TIME_FORMAT_JSON: &str = "%Y-%m-%dT%H:%M:%S%.6f%:z";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// go-hclog's `LevelFromString`, for the levels it names. There is no `warning` alias, and
    /// `off` is not a level a line can be logged at: both come back `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// One line of plugin stderr, as the host classifies it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub level: Level,
    pub message: String,
    /// The remaining JSON keys (for a JSON line); empty otherwise.
    pub fields: Map<String, Value>,
}

/// Classifies plugin stderr lines. Holds the one bit of state go-plugin keeps: once a line starts a
/// Go panic, every following plain line is part of it and logged as an error.
#[derive(Debug, Default)]
pub struct StderrParser {
    in_panic: bool,
}

impl StderrParser {
    pub fn parse(&mut self, line: &str) -> Entry {
        if let Some(entry) = parse_json(line) {
            return entry;
        }
        let level = if line.starts_with("[TRACE]") {
            Level::Trace
        } else if line.starts_with("[DEBUG]") {
            Level::Debug
        } else if line.starts_with("[INFO]") {
            Level::Info
        } else if line.starts_with("[WARN]") {
            Level::Warn
        } else if line.starts_with("[ERROR]") {
            Level::Error
        } else if line.starts_with("panic: ") || line.starts_with("fatal error: ") {
            self.in_panic = true;
            Level::Error
        } else if self.in_panic {
            Level::Error
        } else {
            Level::Debug
        };
        Entry {
            level,
            message: line.to_owned(),
            fields: Map::new(),
        }
    }
}

/// log_entry.go, `parseJSON`: `None` if the line is not an hclog JSON object. A JSON line whose
/// level is missing or unknown is logged at debug with the whole line as its message, as Go does.
fn parse_json(line: &str) -> Option<Entry> {
    let Ok(Value::Object(mut raw)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    // A timestamp that does not parse makes the whole line non-JSON to go-plugin.
    if let Some(ts) = raw.remove("@timestamp") {
        let ts = ts.as_str()?;
        chrono::DateTime::parse_from_str(ts, TIME_FORMAT_JSON).ok()?;
    }
    let message = match raw.remove("@message") {
        Some(Value::String(m)) => m,
        Some(_) => return None,
        None => String::new(),
    };
    let level = raw
        .remove("@level")
        .and_then(|l| l.as_str().and_then(Level::parse));
    Some(match level {
        Some(level) => Entry {
            level,
            message,
            fields: raw,
        },
        None => Entry {
            level: Level::Debug,
            message: line.to_owned(),
            fields: Map::new(),
        },
    })
}

/// Emit a parsed entry as a `tracing` event.
pub fn emit(plugin: &str, entry: &Entry) {
    let fields = if entry.fields.is_empty() {
        String::new()
    } else {
        Value::Object(entry.fields.clone()).to_string()
    };
    match entry.level {
        Level::Trace => tracing::trace!(plugin, fields, "{}", entry.message),
        Level::Debug => tracing::debug!(plugin, fields, "{}", entry.message),
        Level::Info => tracing::info!(plugin, fields, "{}", entry.message),
        Level::Warn => tracing::warn!(plugin, fields, "{}", entry.message),
        Level::Error => tracing::error!(plugin, fields, "{}", entry.message),
    }
}

/// Format a log line the way a Go plugin's hclog JSON logger would, for a Rust plugin to write to
/// its stderr: `@level`, `@message`, `@module` (when `module` is not empty), `@timestamp`, then the
/// fields.
pub fn format_line(level: Level, module: &str, message: &str, fields: &[(&str, Value)]) -> String {
    let mut m = Map::new();
    m.insert("@level".into(), Value::String(level.as_str().into()));
    m.insert("@message".into(), Value::String(message.into()));
    if !module.is_empty() {
        m.insert("@module".into(), Value::String(module.into()));
    }
    m.insert(
        "@timestamp".into(),
        Value::String(chrono::Local::now().format(TIME_FORMAT_JSON).to_string()),
    );
    for (k, v) in fields {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Object(m).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines() {
        let mut p = StderrParser::default();
        let e = p.parse(r#"{"@level":"warn","@message":"hi","@module":"kv","@timestamp":"2026-09-17T11:42:07.123456+05:30","key":"value"}"#);
        assert_eq!((e.level, e.message.as_str()), (Level::Warn, "hi"));
        assert_eq!(e.fields.get("@module"), Some(&Value::String("kv".into())));
        assert_eq!(e.fields.get("key"), Some(&Value::String("value".into())));
        assert!(!e.fields.contains_key("@timestamp"));

        // A bad timestamp makes the line plain text, logged at debug.
        let bad = r#"{"@level":"error","@message":"x","@timestamp":"yesterday"}"#;
        assert_eq!(
            p.parse(bad),
            Entry {
                level: Level::Debug,
                message: bad.into(),
                fields: Map::new()
            }
        );
        // An unknown level logs the whole line at debug: go-hclog has no "warning" alias, and
        // "off" is not a level to log at.
        for odd in [
            r#"{"@level":"loud","@message":"x"}"#,
            r#"{"@level":"warning","@message":"x"}"#,
            r#"{"@level":"off","@message":"x"}"#,
        ] {
            assert_eq!(
                (p.parse(odd).level, p.parse(odd).message),
                (Level::Debug, odd.to_owned())
            );
        }
        // Levels are case-insensitive and trimmed.
        assert_eq!(
            p.parse(r#"{"@level":" INFO ","@message":"x"}"#).level,
            Level::Info
        );
    }

    #[test]
    fn plain_lines_and_panics() {
        let mut p = StderrParser::default();
        assert_eq!(p.parse("[WARN] careful").level, Level::Warn);
        assert_eq!(p.parse("[TRACE] x").level, Level::Trace);
        assert_eq!(p.parse("just text").level, Level::Debug);
        assert_eq!(p.parse("panic: boom").level, Level::Error);
        assert_eq!(
            p.parse("goroutine 1 [running]:").level,
            Level::Error,
            "a panic's trace follows"
        );
    }

    #[test]
    fn a_written_line_parses_back() {
        let line = format_line(
            Level::Info,
            "kv",
            "logged-line",
            &[("key", Value::String("value".into()))],
        );
        let e = StderrParser::default().parse(&line);
        assert_eq!((e.level, e.message.as_str()), (Level::Info, "logged-line"));
        assert_eq!(e.fields.get("key"), Some(&Value::String("value".into())));
    }
}
