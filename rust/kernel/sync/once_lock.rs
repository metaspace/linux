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
/// `init` tracks the state of the container:
///
/// - If the container is empty, `init` is `0`.
/// - If the container is mutably accessed, `init` is `1`.
/// - If the container is populated and ready for shared access, `init` is `2`.
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
        // INVARIANT: The container is empty and we set `init` to `0`.
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
            // SAFETY: As determined by the load above, the object is ready for shared access.
            Some(unsafe { &*self.value.get() })
        } else {
            None
        }
    }

    /// Populate the [`OnceLock`].
    ///
    /// Returns `true` if the [`OnceLock`] was successfully populated.
    pub fn populate(&self, value: T) -> bool {
        // INVARIANT: We obtain exclusive access to the contained allocation and write 1 to
        // `init`.
        if let Ok(0) = self.init.cmpxchg(0, 1, Acquire) {
            // SAFETY: We obtained exclusive access to the contained object.
            unsafe { core::ptr::write(self.value.get(), value) };
            // INVARIANT: We release our exclusive access and transition the object to shared
            // access.
            self.init.store(2, Release);
            true
        } else {
            false
        }
    }
}

impl<T: Copy> OnceLock<T> {
    /// Get a copy of the contained object.
    ///
    /// Returns [`None`] if the [`OnceLock`] is empty.
    pub fn copy(&self) -> Option<T> {
        if self.init.load(Acquire) == 2 {
            // SAFETY: As determined by the load above, the object is ready for shared access.
            Some(unsafe { *self.value.get() })
        } else {
            None
        }
    }
}
