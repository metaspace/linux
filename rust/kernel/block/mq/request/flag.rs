// SPDX-License-Identifier: GPL-2.0
use kernel::prelude::*;

impl_flags! {
    /// A set of request flags.
    ///
    /// This type wraps the C `REQ_*` flags and allows combining multiple flags
    /// together. These flags modify how a block I/O request is processed.
    #[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
    pub struct Flags(u32);

    /// Individual request flags for block I/O operations.
    ///
    /// These flags correspond to the C `REQ_*` defines in `linux/blk_types.h`
    /// and are used to modify the behavior of block I/O requests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Flag {
        /// No driver retries on device errors.
        FailfastDev = bindings::REQ_FAILFAST_DEV,
        /// No driver retries on transport errors.
        FailfastTransport = bindings::REQ_FAILFAST_TRANSPORT,
        /// No driver retries on driver errors.
        FailfastDriver = bindings::REQ_FAILFAST_DRIVER,
        /// Request is synchronous (sync write or read).
        Sync = bindings::REQ_SYNC,
        /// Metadata I/O request.
        Meta = bindings::REQ_META,
        /// Boost priority in CFQ scheduler.
        Priority = bindings::REQ_PRIO,
        /// Don't merge this request with others.
        NoMerge = bindings::REQ_NOMERGE,
        /// Anticipate more I/O after this one.
        Idle = bindings::REQ_IDLE,
        /// I/O includes block integrity payload.
        Integrity = bindings::REQ_INTEGRITY,
        /// Forced unit access - data must be written to persistent storage
        /// before command completion is signaled.
        ForcedUnitAccess = bindings::REQ_FUA,
        /// Request a cache flush before this operation.
        Preflush = bindings::REQ_PREFLUSH,
        /// Read ahead request, can fail anytime.
        ReadAhead = bindings::REQ_RAHEAD,
        /// Background I/O operation.
        Background = bindings::REQ_BACKGROUND,
        /// Don't wait if the request would block.
        NoWait = bindings::REQ_NOWAIT,
        /// Caller polls for completion using `bio_poll`.
        Polled = bindings::REQ_POLLED,
        /// Allocate I/O from cache if available.
        AllocCache = bindings::REQ_ALLOC_CACHE,
        /// Swap I/O operation.
        Swap = bindings::REQ_SWAP,
        /// Reserved for driver use.
        Driver = bindings::REQ_DRV,
        /// Reserved for file system (submitter) use.
        FsPrivate = bindings::REQ_FS_PRIVATE,
        /// Atomic write operation.
        Atomic = bindings::REQ_ATOMIC,
        /// Do not free blocks when zeroing (for write zeroes operations).
        NoUnmap = bindings::REQ_NOUNMAP,
    }
}
