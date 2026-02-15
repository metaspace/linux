// SPDX-License-Identifier: GPL-2.0

//! Fault injection capabilities infrastructure.
//!
//! This module provides a Rust API for the kernel fault injection framework.
//! Fault injection allows simulation of failures in kernel code paths to test
//! error handling.
//!
//! [`FaultConfig`] represents a fault injection control point that can be:
//!
//! - Attached to a configfs tree as a default group, allowing userspace control
//!   of fault injection parameters.
//! - Queried via [`FaultConfig::should_fail`] to determine if an operation
//!   should be simulated as failing.
//!
//! Please see the [fault injection documentation] for details on configuring
//! and using fault injection from userspace.
//!
//! C header: [`include/linux/fault-inject.h`](srctree/include/linux/fault-inject.h)
//!
//! [fault injection documentation]: srctree/Documentation/fault-injection/fault-injection.rst

use crate::{prelude::*, types::Opaque};

/// A fault injection control point.
///
/// This type wraps a `struct fault_config` from the C fault injection
/// framework. It provides a way to create controllable fault injection points
/// that can be configured via configfs.
///
/// When attached to a configfs subsystem as a default group, userspace can
/// configure fault injection parameters through the configfs interface. The
/// kernel code can then query [`FaultConfig::should_fail`] to determine
/// whether to simulate a failure.
///
/// # Invariants
///
/// - `self.inner` is always a valid `struct fault_config`.
#[pin_data]
pub struct FaultConfig {
    #[pin]
    inner: Opaque<bindings::fault_config>,
}

impl FaultConfig {
    /// Create a new [`FaultConfig`].
    ///
    /// If attached to a configfs group, this [`FaultConfig`] will appear as a directory named
    /// `name`.
    pub fn new(name: &CStr) -> impl PinInit<Self> + use<'_> {
        pin_init!(Self {
            // INVARIANT: `self.inner` is initialized in ffi_init.
            inner <- Opaque::zeroed().chain(|inner| {
                let ptr = inner.get();
                // SAFETY: `ptr` points to a zeroed allocation and the second argument is null
                // terminated string.
                unsafe { bindings::fault_config_init( ptr, name.as_ptr().cast()) };
                Ok(())
            }),
        })
    }
}

impl kernel::configfs::CDefaultGroup for FaultConfig {
    fn group_ptr(&self) -> *mut bindings::config_group {
        // SAFETY: By type invariant, `self.inner` is valid.
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
