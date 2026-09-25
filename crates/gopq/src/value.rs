//! `driver.Value` as lib/pq produces and consumes it (encode.go).

/// A `database/sql/driver.Value`: the six types lib/pq hands out, and nil.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `nil`: SQL `NULL`.
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    /// `[]byte`. An empty one is what a nil `[]byte` becomes after gob, and lib/pq sends a nil
    /// `[]byte` as `NULL`.
    Bytes(Vec<u8>),
    String(String),
    /// `time.Time`, decoded from a date or time column. Only its text is kept: the value cannot
    /// cross a gob interface anyway (`time.Time` is not registered), so nothing reads it.
    Time(String),
    /// Any other Go type a caller sent, by its type name; lib/pq refuses to encode it.
    Other(String),
}

/// `driver.NamedValue`. lib/pq reads only `Value`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedValue {
    pub name: String,
    pub ordinal: i64,
    pub value: Value,
}

impl NamedValue {
    /// `toNamedValue`'s element: ordinal `i + 1`, no name.
    pub fn positional(i: usize, value: Value) -> Self {
        NamedValue {
            name: String::new(),
            ordinal: i as i64 + 1,
            value,
        }
    }
}
