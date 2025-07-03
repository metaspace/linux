//! A container that can be initialized at most once.

use super::atomic::ordering::Acquire;
use super::atomic::ordering::Release;
use super::atomic::Atomic;
use kernel::types::Opaque;

/// A container that can be populated at most once. Thread safe.
///
/// Once the a [`OnceLock`] is populated, it remains populated by the same object for the
/// lifetime `Self`.
///
/// # Invariants
///
/// - `init` may only increase in value.
/// - `init` may only assume values in the range `0..=2`.
/// - `init == 0` if and only if the container is empty.
/// - `init == 1` if and only if being mutably accessed.
/// - `init == 2` if and only if the container is populated and valid for shared access.
///
/// # Example
///
/// ```
/// # use kernel::sync::once_lock::OnceLock;
/// let value = OnceLock::new();
/// assert_eq!(None, value.as_ref());
///
/// let status = value.populate(42u8);
/// assert_eq!(true, status);
/// assert_eq!(Some(&42u8), value.as_ref());
/// assert_eq!(Some(42u8), value.copy());
///
/// let status = value.populate(101u8);
/// assert_eq!(false, status);
/// assert_eq!(Some(&42u8), value.as_ref());
/// assert_eq!(Some(42u8), value.copy());
/// ```
pub struct OnceLock<T> {
    init: Atomic<u32>,
    value: Opaque<T>,
}

impl<T> Default for OnceLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> OnceLock<T> {
    /// Create a new [`OnceLock`].
    ///
    /// The returned instance will be empty.
    pub const fn new() -> Self {
        // INVARIANT: The container is empty and we initialize `init` to `0`.
        Self {
            value: Opaque::uninit(),
            init: Atomic::new(0),
        }
    }

    /// Get a reference to the contained object.
    ///
    /// Returns [`None`] if this [`OnceLock`] is empty.
    pub fn as_ref(&self) -> Option<&T> {
        if self.init.load(Acquire) == 2 {
            // SAFETY: By the type invariants of `Self`, `self.init == 2` means that `self.value`
            // contains a valid value.
            Some(unsafe { &*self.value.get() })
        } else {
            None
        }
    }

    /// Populate the [`OnceLock`].
    ///
    /// Returns `true` if the [`OnceLock`] was successfully populated.
    pub fn populate(&self, value: T) -> bool {
        // INVARIANT: If the swap succeeds:
        //  - We increase `init`.
        //  - We write the valid value `1` to `init`.
        //  - Only one thread can succeed in this write, so we have exclusive access after the
        //    write.
        if let Ok(0) = self.init.cmpxchg(0, 1, Acquire) {
            // SAFETY: By the type invariants of `Self`, the fact that we succeeded in writing `1`
            // to `self.init` means we obtained exclusive access to the contained object.
            unsafe { core::ptr::write(self.value.get(), value) };
            // INVARIANT:
            //  - We increase `init`.
            //  - We write the valid value `2` to `init`.
            //  - We release our exclusive access to the contained object and the object is now
            //    valid for shared access.
            self.init.store(2, Release);
            true
        } else {
            false
        }
    }

    /// Get a copy of the contained object.
    ///
    /// Returns [`None`] if the [`OnceLock`] is empty.
    pub fn copy(&self) -> Option<T>
    where
        T: Copy,
    {
        self.as_ref().copied()
    }
}
