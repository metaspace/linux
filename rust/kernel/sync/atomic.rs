// SPDX-License-Identifier: GPL-2.0

//! Atomic primitives.
//!
//! These primitives have the same semantics as their C counterparts: and the precise definitions of
//! semantics can be found at [`LKMM`]. Note that Linux Kernel Memory (Consistency) Model is the
//! only model for Rust code in kernel, and Rust's own atomics should be avoided.
//!
//! # Data races
//!
//! [`LKMM`] atomics have different rules regarding data races:
//!
//! - A normal write from C side is treated as an atomic write if
//!   CONFIG_KCSAN_ASSUME_PLAIN_WRITES_ATOMIC=y.
//! - Mixed-size atomic accesses don't cause data races.
//!
//! [`LKMM`]: srctree/tools/memory-mode/

pub mod generic;
pub mod ops;
pub mod ordering;

pub use generic::Atomic;
pub use ordering::{Acquire, Full, Relaxed, Release};

// SAFETY: `u64` and `i64` has the same size and alignment.
unsafe impl generic::AllowAtomic for u64 {
    type Repr = i64;

    fn into_repr(self) -> Self::Repr {
        self as Self::Repr
    }

    fn from_repr(repr: Self::Repr) -> Self {
        repr as Self
    }
}

impl generic::AllowAtomicArithmetic for u64 {
    type Delta = u64;

    fn delta_into_repr(d: Self::Delta) -> Self::Repr {
        d as Self::Repr
    }
}

// SAFETY: `u32` and `i32` has the same size and alignment.
unsafe impl generic::AllowAtomic for u32 {
    type Repr = i32;

    fn into_repr(self) -> Self::Repr {
        self as Self::Repr
    }

    fn from_repr(repr: Self::Repr) -> Self {
        repr as Self
    }
}

impl generic::AllowAtomicArithmetic for u32 {
    type Delta = u32;

    fn delta_into_repr(d: Self::Delta) -> Self::Repr {
        d as Self::Repr
    }
}

use crate::macros::kunit_tests;

#[kunit_tests(rust_atomics)]
mod tests {
    use super::*;

    // Call $fn($val) with each $type of $val.
    macro_rules! for_each_type {
        ($val:literal in [$($type:ty),*] $fn:expr) => {
            $({
                let v: $type = $val;

                $fn(v);
            })*
        }
    }

    #[test]
    fn atomic_basic_tests() {
        for_each_type!(42 in [i32, i64, u32, u64] |v| {
            let x = Atomic::new(v);

            assert_eq!(v, x.load(Relaxed));
        });
    }

    #[test]
    fn atomic_xchg_tests() {
        for_each_type!(42 in [i32, i64, u32, u64] |v| {
            let x = Atomic::new(v);

            let old = v;
            let new = v + 1;

            assert_eq!(old, x.xchg(new, Full));
            assert_eq!(new, x.load(Relaxed));
        });
    }

    #[test]
    fn atomic_cmpxchg_tests() {
        for_each_type!(42 in [i32, i64, u32, u64] |v| {
            let x = Atomic::new(v);

            let old = v;
            let new = v + 1;

            assert_eq!(Err(old), x.cmpxchg(new, new, Full));
            assert_eq!(old, x.load(Relaxed));
            assert_eq!(Ok(old), x.cmpxchg(old, new, Relaxed));
            assert_eq!(new, x.load(Relaxed));
        });
    }

    #[test]
    fn atomic_arithmetic_tests() {
        for_each_type!(42 in [i32, i64, u32, u64] |v| {
            let x = Atomic::new(v);

            assert_eq!(v, x.fetch_add(12, Full));
            assert_eq!(v + 12, x.load(Relaxed));

            x.add(13, Relaxed);

            assert_eq!(v + 25, x.load(Relaxed));
        });
    }
}
