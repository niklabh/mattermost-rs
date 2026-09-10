//! Tell cargo which environment variables the *test* harness bakes in at compile time.
//!
//! `tests/common/mod.rs` resolves `GO` and `RUST` with `option_env!`, which is evaluated by the
//! compiler and therefore invisible to cargo's change detection: without the lines below, moving
//! a checkout from one stack to another would silently keep the previous stack's ports in an
//! already-built test binary and every parity test would compare the wrong two servers.
//!
//! It is a compile-time lookup rather than a runtime one because `GO` and `RUST` are `&'static
//! str` consts used inside inline format captures — `format!("{GO}/api/v4/x")` — in roughly
//! thirteen hundred places. Making them runtime values means rewriting every one of those; making
//! them compile-time values costs this file. Each worktree has its own `target/`, so pinning a
//! worktree to a stack costs one rebuild and nothing after that.
fn main() {
    println!("cargo:rerun-if-env-changed=MMRS_GO_BASE");
    println!("cargo:rerun-if-env-changed=MMRS_RUST_BASE");
}
