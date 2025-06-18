// SPDX-License-Identifier: GPL-2.0

//! Atomic implementations.
//!
//! Provides 1:1 mapping of atomic implementations.

use crate::bindings::*;
use crate::macros::paste;

mod private {
    /// Sealed trait marker to disable customized impls on atomic implementation traits.
    pub trait Sealed {}
}

// `i32` and `i64` are only supported atomic implementations.
impl private::Sealed for i32 {}
impl private::Sealed for i64 {}

/// A marker trait for types that implement atomic operations with C side primitives.
///
/// This trait is sealed, and only types that have directly mapping to the C side atomics should
/// impl this:
///
/// - `i32` maps to `atomic_t`.
/// - `i64` maps to `atomic64_t`.
pub trait AtomicImpl: Sized + Send + Copy + private::Sealed {}

// `atomic_t` implements atomic operations on `i32`.
impl AtomicImpl for i32 {}

// `atomic64_t` implements atomic operations on `i64`.
impl AtomicImpl for i64 {}

// This macro generates the function signature with given argument list and return type.
macro_rules! declare_atomic_method {
    (
        $func:ident($($arg:ident : $arg_type:ty),*) $(-> $ret:ty)?
    ) => {
        paste!(
            #[doc = concat!("Atomic ", stringify!($func))]
            #[doc = "# Safety"]
            #[doc = "- Any pointer passed to the function has to be a valid pointer"]
            #[doc = "- Accesses must not cause data races per LKMM:"]
            #[doc = "  - Atomic read racing with normal read, normal write or atomic write is not data race."]
            #[doc = "  - Atomic write racing with normal read or normal write is data-race, unless the"]
            #[doc = "    normal accesses are done at C side and considered as immune to data"]
            #[doc = "    races, e.g. CONFIG_KCSAN_ASSUME_PLAIN_WRITES_ATOMIC."]
            unsafe fn [< atomic_ $func >]($($arg: $arg_type,)*) $(-> $ret)?;
        );
    };
    (
        $func:ident [$variant:ident $($rest:ident)*]($($arg_sig:tt)*) $(-> $ret:ty)?
    ) => {
        paste!(
            declare_atomic_method!(
                [< $func _ $variant >]($($arg_sig)*) $(-> $ret)?
            );
        );

        declare_atomic_method!(
            $func [$($rest)*]($($arg_sig)*) $(-> $ret)?
        );
    };
    (
        $func:ident []($($arg_sig:tt)*) $(-> $ret:ty)?
    ) => {
        declare_atomic_method!(
            $func($($arg_sig)*) $(-> $ret)?
        );
    }
}

// This macro generates the function implementation with given argument list and return type, and it
// will replace "call(...)" expression with "$ctype _ $func" to call the real C function.
macro_rules! impl_atomic_method {
    (
        ($ctype:ident) $func:ident($($arg:ident: $arg_type:ty),*) $(-> $ret:ty)? {
            call($($c_arg:expr),*)
        }
    ) => {
        paste!(
            #[inline(always)]
            unsafe fn [< atomic_ $func >]($($arg: $arg_type,)*) $(-> $ret)? {
                // SAFETY: Per function safety requirement, all pointers are valid, and accesses
                // won't cause data race per LKMM.
                unsafe { [< $ctype _ $func >]($($c_arg,)*) }
            }
        );
    };
    (
        ($ctype:ident) $func:ident[$variant:ident $($rest:ident)*]($($arg_sig:tt)*) $(-> $ret:ty)? {
            call($($arg:tt)*)
        }
    ) => {
        paste!(
            impl_atomic_method!(
                ($ctype) [< $func _ $variant >]($($arg_sig)*) $( -> $ret)? {
                    call($($arg)*)
            }
            );
        );
        impl_atomic_method!(
            ($ctype) $func [$($rest)*]($($arg_sig)*) $( -> $ret)? {
                call($($arg)*)
            }
        );
    };
    (
        ($ctype:ident) $func:ident[]($($arg_sig:tt)*) $( -> $ret:ty)? {
            call($($arg:tt)*)
        }
    ) => {
        impl_atomic_method!(
            ($ctype) $func($($arg_sig)*) $(-> $ret)? {
                call($($arg)*)
            }
        );
    }
}

// Delcares $ops trait with methods and implements the trait for `i32` and `i64`.
macro_rules! declare_and_impl_atomic_methods {
    ($ops:ident ($doc:literal) {
        $(
            $func:ident [$($variant:ident),*]($($arg_sig:tt)*) $( -> $ret:ty)? {
                call($($arg:tt)*)
            }
        )*
    }) => {
        #[doc = $doc]
        pub trait $ops: AtomicImpl {
            $(
                declare_atomic_method!(
                    $func[$($variant)*]($($arg_sig)*) $(-> $ret)?
                );
            )*
        }

        impl $ops for i32 {
            $(
                impl_atomic_method!(
                    (atomic) $func[$($variant)*]($($arg_sig)*) $(-> $ret)? {
                        call($($arg)*)
                    }
                );
            )*
        }

        impl $ops for i64 {
            $(
                impl_atomic_method!(
                    (atomic64) $func[$($variant)*]($($arg_sig)*) $(-> $ret)? {
                        call($($arg)*)
                    }
                );
            )*
        }
    }
}

declare_and_impl_atomic_methods!(
    AtomicHasBasicOps ("Basic atomic operations") {
        read[acquire](ptr: *mut Self) -> Self {
            call(ptr as *mut _)
        }

        set[release](ptr: *mut Self, v: Self) {
            call(ptr as *mut _, v)
        }
    }
);

declare_and_impl_atomic_methods!(
    AtomicHasXchgOps ("Exchange and compare-and-exchange atomic operations") {
        xchg[acquire, release, relaxed](ptr: *mut Self, v: Self) -> Self {
            call(ptr as *mut _, v)
        }

        cmpxchg[acquire, release, relaxed](ptr: *mut Self, old: Self, new: Self) -> Self {
            call(ptr as *mut _, old, new)
        }

        try_cmpxchg[acquire, release, relaxed](ptr: *mut Self, old: *mut Self, new: Self) -> bool {
            call(ptr as *mut _, old, new)
        }
    }
);

declare_and_impl_atomic_methods!(
    AtomicHasArithmeticOps ("Atomic arithmetic operations") {
        add[](ptr: *mut Self, v: Self) {
            call(v, ptr as *mut _)
        }

        fetch_add[acquire, release, relaxed](ptr: *mut Self, v: Self) -> Self {
            call(v, ptr as *mut _)
        }
    }
);
