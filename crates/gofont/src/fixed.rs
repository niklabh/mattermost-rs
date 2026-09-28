//! Port of the parts of `golang.org/x/image/math/fixed` the rasteriser uses.
//!
//! `fixed.Int26_6` is a Go `int32`, and Go's `int32` arithmetic **wraps**. Every operation here
//! that could overflow on a hostile font is written with `wrapping_*` so the result is Go's rather
//! than a panic; on the fonts this crate is used with nothing comes near the limit.

/// `fixed.Int26_6`: a signed 26.6 fixed-point number.
pub type Int26_6 = i32;

/// `fixed.I(i)`: `Int26_6(i << 6)`.
#[must_use]
pub fn i(value: i64) -> Int26_6 {
    // Go shifts the `int` and then truncates to `int32`.
    (value << 6) as Int26_6
}

/// `fixed.Point26_6`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point26_6 {
    pub x: Int26_6,
    pub y: Int26_6,
}

impl Point26_6 {
    #[must_use]
    pub fn new(x: Int26_6, y: Int26_6) -> Self {
        Self { x, y }
    }
}

/// `fixed.Rectangle26_6`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rectangle26_6 {
    pub min: Point26_6,
    pub max: Point26_6,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i_is_a_shift_by_six() {
        assert_eq!(i(128), 8192);
        assert_eq!(i(-3), -192);
        assert_eq!(i(0), 0);
    }
}
