//! The DB-backed tests in `tests/` resolve the Go server's base URL with `option_env!`, which the
//! compiler evaluates and cargo therefore cannot see. Without this, moving a checkout between
//! stacks would leave an already-built test binary talking to the previous stack's server — which
//! answers 401 to this stack's session token, and looks like a permission bug in the port.
//!
//! See `crates/mm-api/tests/common/mod.rs` for why the lookup is compile-time at all.
fn main() {
    println!("cargo:rerun-if-env-changed=MMRS_GO_BASE");
    println!("cargo:rerun-if-env-changed=MMRS_RUST_BASE");
}
