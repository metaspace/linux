// SPDX-License-Identifier: GPL-2.0

use kernel::try_pin_init;
use pin_init::{
    pin_data,
    pin_init,
    PinInit, //
};

/// Wrapper type that alings content to a 64 byte cache line.
#[repr(align(64))]
#[pin_data]
pub struct CacheAligned<T: ?Sized> {
    #[pin]
    value: T,
}

impl<T> CacheAligned<T> {
    /// Creates an initializer for `CacheAligned<T>` form an initalizer for `T`
    pub fn new(t: impl PinInit<T>) -> impl PinInit<CacheAligned<T>> {
        pin_init!( CacheAligned {
            value <- t
        })
    }

    /// Creates a fallible initializer for `CacheAligned<T>` form a fallible
    /// initalizer for `T`
    pub fn try_new(
        t: impl PinInit<T, crate::error::Error>,
    ) -> impl PinInit<CacheAligned<T>, crate::error::Error> {
        try_pin_init!( CacheAligned {
            value <- t
        }? crate::error::Error )
    }

    /// Get a pointer to the contained value without creating a reference.
    ///
    /// # Safety
    ///
    /// - `ptr` must be dereferenceable.
    pub const unsafe fn raw_get(ptr: *mut Self) -> *mut T {
        // SAFETY: by function safety requirements `ptr` is valid for read
        unsafe { &raw mut ((*ptr).value) }
    }
}

impl<T: ?Sized> core::ops::Deref for CacheAligned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: ?Sized> core::ops::DerefMut for CacheAligned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}
