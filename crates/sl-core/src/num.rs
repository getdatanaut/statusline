//! Float-to-integer conversions, made in one place.
//!
//! Rust's `as` from a float saturates at the target's bounds and turns NaN
//! into 0, which is what every caller in this workspace wants: these are
//! display numbers and pixel counts, never values a wrap could corrupt. The
//! cast lints flag each such `as` all the same, so they are written here,
//! once, with that reasoning, and called by name everywhere else.

/// `x` toward zero as an `i64`, saturating; NaN is 0.
#[expect(clippy::cast_possible_truncation, reason = "float-to-int `as` saturates; see the module docs")]
pub fn trunc_i64(x: f64) -> i64 {
    x as i64
}

/// `x` toward zero as a `u64`, saturating; negatives and NaN are 0.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "float-to-int `as` saturates; see the module docs")]
pub fn trunc_u64(x: f64) -> u64 {
    x as u64
}

/// `x` toward zero as a `usize`, saturating; negatives and NaN are 0.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "float-to-int `as` saturates; see the module docs")]
pub fn trunc_usize(x: f64) -> usize {
    x as usize
}

/// `x` toward zero as a `u32`, saturating; negatives and NaN are 0.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "float-to-int `as` saturates; see the module docs")]
pub fn trunc_u32(x: f64) -> u32 {
    x as u32
}

/// `x` toward zero as a `u8`, saturating; negatives and NaN are 0.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "float-to-int `as` saturates; see the module docs")]
pub fn trunc_u8(x: f64) -> u8 {
    x as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_truncate_and_saturate() {
        assert_eq!(trunc_i64(-2.7), -2, "toward zero");
        assert_eq!(trunc_i64(f64::INFINITY), i64::MAX, "saturates");
        assert_eq!(trunc_i64(f64::NAN), 0, "NaN is zero");
        assert_eq!(trunc_u64(-5.0), 0, "no negatives");
        assert_eq!(trunc_usize(3.99), 3, "toward zero");
        assert_eq!(trunc_u32(1e12), u32::MAX, "saturates");
        assert_eq!(trunc_u8(300.0), 255, "saturates");
    }
}
