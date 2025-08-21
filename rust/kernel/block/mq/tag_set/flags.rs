// SPDX-License-Identifier: GPL-2.0

use core::default::Default;
use kernel::ffi::c_uint;

/// Flags to be used when creating [`super::TagSet`] objects.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Flags(c_uint);

impl Flags {
    /// Indicate that the queues associated with this tag set might sleep when
    /// processing IO. When this flag is not set, IO is processed in atomic
    /// context. When this flag is set, IO is processed in process context.
    pub const BLOCKING: Flags = Flags::new(bindings::BLK_MQ_F_BLOCKING);

    /// Select 'none' during queue registration in case of a single hwq or shared
    /// hwqs instead of 'mq-deadline'.
    pub const NO_DEFAULT_SCHEDULER: Flags = Flags::new(bindings::BLK_MQ_F_NO_SCHED_BY_DEFAULT);

    pub(crate) fn into_inner(self) -> c_uint {
        self.0
    }

    const fn new(value: u32) -> Self {
        Self(value)
    }
}

impl Default for Flags {
    fn default() -> Self {
        Self::new(0)
    }
}

impl core::ops::BitOr for Flags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitAnd for Flags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl core::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAndAssign for Flags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl core::ops::Not for Flags {
    type Output = Self;
    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}
