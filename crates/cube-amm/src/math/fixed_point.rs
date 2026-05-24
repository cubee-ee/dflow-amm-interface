//! 18-decimal fixed-point primitives. Ported verbatim from
//! cubic-pool/src/math/fixed_point.rs (only the Anchor `Result` /
//! `ErrorCode` shell is swapped for `anyhow`).

use crate::constants::ONE;
use anyhow::{anyhow, Result};

pub struct FixedPoint;

impl FixedPoint {
    /// `(a * b) / ONE`, rounded down.
    pub fn mul_down(a: u128, b: u128) -> Result<u128> {
        let product = a
            .checked_mul(b)
            .ok_or_else(|| anyhow!("FixedPoint::mul_down overflow"))?;
        Ok(product / ONE)
    }

    /// `floor((a * ONE) / b)`.
    pub fn div_down(a: u128, b: u128) -> Result<u128> {
        if b == 0 {
            return Err(anyhow!("FixedPoint::div_down by zero"));
        }
        let numerator = a
            .checked_mul(ONE)
            .ok_or_else(|| anyhow!("FixedPoint::div_down overflow"))?;
        Ok(numerator / b)
    }

    /// `ceil((a * ONE) / b)`.
    pub fn div_up(a: u128, b: u128) -> Result<u128> {
        if b == 0 {
            return Err(anyhow!("FixedPoint::div_up by zero"));
        }
        let numerator = a
            .checked_mul(ONE)
            .ok_or_else(|| anyhow!("FixedPoint::div_up overflow"))?;
        if numerator == 0 {
            return Ok(0);
        }
        Ok((numerator - 1) / b + 1)
    }

    /// `1 - x` in 18-decimal fixed-point (`x` must be ≤ ONE).
    pub fn complement(x: u128) -> Result<u128> {
        if x > ONE {
            return Err(anyhow!("FixedPoint::complement out of range"));
        }
        Ok(ONE - x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_down_basic() {
        assert_eq!(FixedPoint::mul_down(2 * ONE, 3 * ONE).unwrap(), 6 * ONE);
    }

    #[test]
    fn div_down_basic() {
        assert_eq!(FixedPoint::div_down(6 * ONE, 2 * ONE).unwrap(), 3 * ONE);
    }

    #[test]
    fn div_up_rounds_up() {
        assert!(FixedPoint::div_up(1, 3).unwrap() > ONE / 3);
    }

    #[test]
    fn complement_quarter() {
        assert_eq!(FixedPoint::complement(ONE / 4).unwrap(), 3 * ONE / 4);
    }
}
