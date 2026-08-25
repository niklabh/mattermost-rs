//! Port of `model/builtin.go` — Go's two nil-handling generic helpers.
//!
//! `NewPointer[T](t) *T` exists because Go has no `&literal`; Rust does, and `Option::Some` is
//! the pointer. It is kept only so a mechanical translation of a Go call site has something to
//! land on — prefer `Some(x)` in new code.

/// Port of `model.NewPointer` (builtin.go:6).
///
/// Go's `*T` is this crate's `Option<T>`, so the port returns `Some`.
pub fn new_pointer<T>(t: T) -> Option<T> {
    Some(t)
}

/// Port of `model.SafeDereference` (builtin.go:10) — the zero value when the pointer is nil.
pub fn safe_dereference<T: Default>(t: Option<T>) -> T {
    t.unwrap_or_default()
}

/// Borrowing form of [`safe_dereference`], for the common `*string` case where the caller only
/// needs to read through the pointer and cloning would be waste.
pub fn safe_deref_str(t: Option<&String>) -> &str {
    match t {
        Some(s) => s.as_str(),
        None => "",
    }
}
