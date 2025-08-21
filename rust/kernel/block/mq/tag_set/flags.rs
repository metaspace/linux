// SPDX-License-Identifier: GPL-2.0

use kernel::prelude::*;

impl_flags! {

    /// Flags to be used when creating [`super::TagSet`] objects.
    #[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
    pub struct Flags(u32);

    /// Allowed values for [`Flags`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Flag {
        /// Indicate that the queues associated with this tag set might sleep when
        /// processing IO. When this flag is not set, IO is processed in atomic
        /// context. When this flag is set, IO is processed in process context.
        Blocking = bindings::BLK_MQ_F_BLOCKING,
    }
}
