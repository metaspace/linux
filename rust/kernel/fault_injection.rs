// SPDX-License-Identifier: GPL-2.0

use crate::{prelude::*, types::Opaque};

/// # INVARIANTS
/// - `self.inner` is always a valid `bindings::fault_config`.
#[pin_data]
pub struct FaultConfig {
    #[pin]
    inner: Opaque<bindings::fault_config>,
}

impl FaultConfig {
    /// Create a new [`FaultConfig`].
    ///
    /// If attached to a configfs group, this [`FaultConfig`] will appear as a directory named `name`.
    pub fn new(name: &CStr) -> impl PinInit<Self> + use<'_> {
        pin_init!(Self {
            // INVARIANT: `self.inner` is initialized in ffi_init.
            inner <- Opaque::zeroed().chain(|inner| {
                let ptr = inner.get();
                unsafe { bindings::fault_config_init( ptr, name.as_ptr().cast()) };
                Ok(())
            }),
        })
    }
}

impl kernel::configfs::CDefaultGroup for FaultConfig {
    fn group_ptr(&self) -> *mut bindings::config_group {
        unsafe { &raw mut (*self.inner.get()).group }
    }
}

impl FaultConfig {
    /// Query for failure.
    ///
    /// Returns true if the operation should fail.
    pub fn should_fail(&self, size: isize) -> bool {
        // SAFETY: By type invariant, self is always valid.
        let attr = unsafe { &raw const (*self.inner.get()).attr };

        // SAFETY: By type invariant, self is always valid.
        unsafe { bindings::should_fail(attr.cast_mut(), size) }
    }
}

// SAFETY: FaultConfig can be used from any task.
unsafe impl Send for FaultConfig {}

// SAFETY: FaultConfig applies internal synchronization.
unsafe impl Sync for FaultConfig {}
