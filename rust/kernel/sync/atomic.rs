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

// SAFETY: `usize` has the same size and the alignment as `i64` for 64bit and the same as `i32` for
// 32bit.
unsafe impl generic::AllowAtomic for usize {
    #[cfg(CONFIG_64BIT)]
    type Repr = i64;
    #[cfg(not(CONFIG_64BIT))]
    type Repr = i32;

    fn into_repr(self) -> Self::Repr {
        self as Self::Repr
    }

    fn from_repr(repr: Self::Repr) -> Self {
        repr as Self
    }
}

impl generic::AllowAtomicArithmetic for usize {
    type Delta = usize;

    fn delta_into_repr(d: Self::Delta) -> Self::Repr {
        d as Self::Repr
    }
}

// SAFETY: `isize` has the same size and the alignment as `i64` for 64bit and the same as `i32` for
// 32bit.
unsafe impl generic::AllowAtomic for isize {
    #[cfg(CONFIG_64BIT)]
    type Repr = i64;
    #[cfg(not(CONFIG_64BIT))]
    type Repr = i32;

    fn into_repr(self) -> Self::Repr {
        self as Self::Repr
    }

    fn from_repr(repr: Self::Repr) -> Self {
        repr as Self
    }
}

impl generic::AllowAtomicArithmetic for isize {
    type Delta = isize;

    fn delta_into_repr(d: Self::Delta) -> Self::Repr {
        d as Self::Repr
    }
}
// SAFETY: A `*mut T` has the same size and the alignment as `i64` for 64bit and the same as `i32`
// for 32bit. And it's safe to transfer the ownership of a pointer value to another thread.
unsafe impl<T> generic::AllowAtomic for *mut T {
    #[cfg(CONFIG_64BIT)]
    type Repr = i64;
    #[cfg(not(CONFIG_64BIT))]
    type Repr = i32;

    fn into_repr(self) -> Self::Repr {
        self as Self::Repr
    }

    fn from_repr(repr: Self::Repr) -> Self {
        repr as Self
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
        for_each_type!(42 in [i32, i64, u32, u64, isize, usize] |v| {
            let x = Atomic::new(v);

            assert_eq!(v, x.load(Relaxed));
        });

        let x = Atomic::new(core::ptr::null_mut::<i32>());
        assert!(x.load(Relaxed).is_null());
    }

    #[test]
    fn atomic_xchg_tests() {
        for_each_type!(42 in [i32, i64, u32, u64, isize, usize] |v| {
            let x = Atomic::new(v);

            let old = v;
            let new = v + 1;

            assert_eq!(old, x.xchg(new, Full));
            assert_eq!(new, x.load(Relaxed));
        });
    }

    #[test]
    fn atomic_cmpxchg_tests() {
        for_each_type!(42 in [i32, i64, u32, u64, isize, usize] |v| {
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
        for_each_type!(42 in [i32, i64, u32, u64, isize, usize] |v| {
            let x = Atomic::new(v);

            assert_eq!(v, x.fetch_add(12, Full));
            assert_eq!(v + 12, x.load(Relaxed));

            x.add(13, Relaxed);

            assert_eq!(v + 25, x.load(Relaxed));
        });
    }

    #[test]
    fn atomic_ptr_tests() -> crate::error::Result {
        use crate::alloc::{flags::GFP_KERNEL, KBox};
        use core::ptr;

        let x = Atomic::new(ptr::null_mut::<i32>());

        assert!(x.load(Relaxed).is_null());

        let new = KBox::new(42, GFP_KERNEL)?;
        x.store(ptr::from_mut(KBox::leak(new)), Release);

        let ptr = x.load(Relaxed);
        assert!(!ptr.is_null());

        // SAFETY: `ptr` is a valid pointer from `KBox::leak()` and the address dependency
        // guarantees observation of the initialization of `KBox`.
        assert_eq!(42, unsafe { ptr.read_volatile() });

        x.xchg(ptr::null_mut(), Relaxed);
        assert!(x.load(Relaxed).is_null());

        // SAFETY: `ptr` is a valid pointer from `KBox::leak()` and no one is currently referencing
        // the pointer, so it's safety to convert the ownership back to a `KBox`.
        drop(unsafe { KBox::from_raw(ptr) });

        Ok(())
    }
}
