// SPDX-License-Identifier: GPL-2.0
use kernel::prelude::*;

impl_flags! {
    #[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
    pub struct Flags(u32);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Flag {
        FailfastDev = bindings::REQ_FAILFAST_DEV,
        FailfastTransport = bindings::REQ_FAILFAST_TRANSPORT,
        FailfastDriver = bindings::REQ_FAILFAST_DRIVER,
        Sync = bindings::REQ_SYNC,
        Meta = bindings::REQ_META,
        Priority = bindings::REQ_PRIO,
        NoMerge = bindings::REQ_NOMERGE,
        Idle = bindings::REQ_IDLE,
        Integrity = bindings::REQ_INTEGRITY,
        ForcedUnitAccess = bindings::REQ_FUA,
        Preflush = bindings::REQ_PREFLUSH,
        ReadAhead = bindings::REQ_RAHEAD,
        Background = bindings::REQ_BACKGROUND,
        NoWait = bindings::REQ_NOWAIT,
        Polled = bindings::REQ_POLLED,
        AllocCache = bindings::REQ_ALLOC_CACHE,
        Swap = bindings::REQ_SWAP,
        Driver = bindings::REQ_DRV,
        FsPrivate = bindings::REQ_FS_PRIVATE,
        Atomic = bindings::REQ_ATOMIC,
        NoUnmap = bindings::REQ_NOUNMAP,
    }
}
