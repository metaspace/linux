// SPDX-License-Identifier: GPL-2.0

//! A one-time only global execution primitive.
//!
//! This primitive is meant to be used to execute code only once. It is
//! similar in design to Rust's
//! [`std::sync:Once`](https://doc.rust-lang.org/std/sync/struct.Once.html),
//! but borrowing the non-blocking mechanism used in the kernel's
//! [`DO_ONCE_LITE`] macro.
//!
//! An example use case would be to print a message only the first time.
//!
//! [`DO_ONCE_LITE`]: srctree/include/linux/once_lite.h
//!
//! C header: [`include/linux/once_lite.h`](srctree/include/linux/once_lite.h)
//!
//! Reference: <https://doc.rust-lang.org/std/sync/struct.Once.html>

use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// A low-level synchronization primitive for one-time global execution.
///
/// Based on the
/// [`std::sync:Once`](https://doc.rust-lang.org/std/sync/struct.Once.html)
/// interface, but internally equivalent the kernel's [`DO_ONCE_LITE`]
/// macro. The Rust macro `do_once_lite` replacing it uses `OnceLite`
/// internally.
///
/// [`DO_ONCE_LITE`]: srctree/include/linux/once_lite.h
///
/// # Examples
///
/// ```rust
/// static START: kernel::once_lite::OnceLite =
///     kernel::once_lite::OnceLite::new();
///
/// let mut x: i32 = 0;
///
/// START.call_once(|| {
///   // run initialization here
///   x = 42;
/// });
/// while !START.is_completed() { /* busy wait */ }
/// assert_eq!(x, 42);
/// ```
///
pub struct OnceLite(AtomicBool, AtomicBool);

impl OnceLite {
    /// Creates a new `OnceLite` value.
    #[inline(always)]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false), AtomicBool::new(false))
    }

    /// Performs an initialization routine once and only once. The given
    /// closure will be executed if this is the first time `call_once` has
    /// been called, and otherwise the routine will not be invoked.
    ///
    /// This method will _not_ block the calling thread if another
    /// initialization is currently running. It is _not_ guaranteed that the
    /// initialization routine will have completed by the time the calling
    /// thread continues execution.
    ///
    /// Note that this is different from the guarantees made by
    /// [`std::sync::Once`], but identical to the way the implementation of
    /// the kernel's [`DO_ONCE_LITE_IF`] macro is behaving at the time of
    /// writing.
    ///
    /// [`std::sync::Once`]: https://doc.rust-lang.org/std/sync/struct.Once.html
    /// [`DO_ONCE_LITE_IF`]: srctree/include/once_lite.h
    #[inline(always)]
    pub fn call_once<F: FnOnce()>(&self, f: F) {
        if !self.0.load(Relaxed) && !self.0.swap(true, Relaxed) {
            f()
        };
        self.1.store(true, Relaxed);
    }

    /// Returns `true` if some `call_once` call has completed successfully.
    /// Specifically, `is_completed` will return `false` in the following
    /// situations:
    ///
    /// 1. `call_once()` was not called at all,
    /// 2. `call_once()` was called, but has not yet completed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// static INIT: kernel::once_lite::OnceLite =
    ///     kernel::once_lite::OnceLite::new();
    ///
    /// assert_eq!(INIT.is_completed(), false);
    /// INIT.call_once(|| {
    ///     assert_eq!(INIT.is_completed(), false);
    /// });
    /// assert_eq!(INIT.is_completed(), true);
    /// ```
    #[inline(always)]
    pub fn is_completed(&self) -> bool {
        self.1.load(Relaxed)
    }
}

/// Executes code only once.
///
/// Equivalent to the kernel's [`DO_ONCE_LITE`] macro: Expression is
/// evaluated at most once by the first thread, other threads will not be
/// blocked while executing in first thread, nor are there any guarantees
/// regarding the visibility of side-effects of the called expression.
///
/// [`DO_ONCE_LITE`]: srctree/include/linux/once_lite.h
///
/// # Examples
///
/// ```
/// let mut x: i32 = 0;
/// kernel::do_once_lite!((||{x = 42;})());
/// ```
#[macro_export]
macro_rules! do_once_lite {
    ($e: expr) => {{
        #[link_section = ".data.once"]
        static ONCE: $crate::once_lite::OnceLite = $crate::once_lite::OnceLite::new();
        ONCE.call_once(|| $e);
    }};
}
