//! Go's floating-point fusion, per target architecture.
//!
//! The Go compiler rewrites `a + x*y`, `a - x*y` and `x*y - a` into single fused instructions on
//! arm64 (FMADDD / FMSUBD / FNMSUBD; `cmd/compile/internal/ssa/_gen/ARM64.rules`), with **no**
//! single-use condition on the product — `aw := s*w; a += aw; r += c*aw` fuses the `a += aw` too.
//! amd64 at the default `GOAMD64=v1` fuses nothing. Rust never fuses on its own, so every place
//! the ported Go code would be fused calls one of these, and the choice follows the target.
//!
//! The oracle fixtures were generated on arm64, so the fused forms are the ones the tests prove.

/// `a + x*y` as the Go compiler emits it for this architecture.
#[inline(always)]
pub fn madd(a: f64, x: f64, y: f64) -> f64 {
    if FUSES { x.mul_add(y, a) } else { a + x * y }
}

/// `a - x*y` as the Go compiler emits it for this architecture.
#[inline(always)]
pub fn msub(a: f64, x: f64, y: f64) -> f64 {
    if FUSES { (-x).mul_add(y, a) } else { a - x * y }
}

/// `x*y - a` as the Go compiler emits it for this architecture.
#[inline(always)]
pub fn nmsub(x: f64, y: f64, a: f64) -> f64 {
    if FUSES { x.mul_add(y, -a) } else { x * y - a }
}

/// Whether Go fuses multiply-add on this target: arm64, ppc64(le), s390x, riscv64 and loong64 do;
/// amd64 (GOAMD64=v1), 386 and arm do not.
pub const FUSES: bool = cfg!(any(
    target_arch = "aarch64",
    target_arch = "powerpc64",
    target_arch = "s390x",
    target_arch = "riscv64",
    target_arch = "loongarch64"
));
