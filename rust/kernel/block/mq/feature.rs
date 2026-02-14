// SPDX-License-Identifier: GPL-2.0

//! Block device feature flags.
//!
//! This module provides Rust abstractions for the C `blk_features_t` type and
//! the associated `BLK_FEAT_*` flags defined in `include/linux/blkdev.h`.

use kernel::prelude::*;

impl_flags! {
    /// A set of block device feature flags.
    ///
    /// This type wraps the C `blk_features_t` bitfield and represents a
    /// combination of zero or more [`Feature`] flags. It is used to describe
    /// the capabilities of a block device in [`struct queue_limits`].
    ///
    /// [`struct queue_limits`]: srctree/include/linux/blkdev.h
    #[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
    pub struct Features(u32);

    /// A block device feature flag.
    ///
    /// Each variant corresponds to a `BLK_FEAT_*` constant defined in
    /// `include/linux/blkdev.h`. These flags describe individual capabilities
    /// or properties of a block device.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Feature {
        /// Supports a volatile write cache.
        WriteCache = bindings::BLK_FEAT_WRITE_CACHE,

        /// Supports passing on the FUA bit.
        ForcedUnitAccess = bindings::BLK_FEAT_FUA,

        /// Rotational device (hard drive or floppy).
        Rotational = bindings::BLK_FEAT_ROTATIONAL,

        /// Contributes to the random number pool.
        AddRandom = bindings::BLK_FEAT_ADD_RANDOM,

        /// Enables disk/partitions I/O accounting.
        IoStat = bindings::BLK_FEAT_IO_STAT,

        /// Don't modify data until writeback is done.
        StableWrites = bindings::BLK_FEAT_STABLE_WRITES,

        /// Always completes in submit context.
        Synchronous = bindings::BLK_FEAT_SYNCHRONOUS,

        /// Supports REQ_NOWAIT.
        Nowait = bindings::BLK_FEAT_NOWAIT,

        /// Supports DAX.
        Dax = bindings::BLK_FEAT_DAX,

        /// Supports I/O polling.
        Poll = bindings::BLK_FEAT_POLL,

        /// Is a zoned device.
        Zoned = bindings::BLK_FEAT_ZONED,

        /// Supports PCI(e) p2p requests.
        PciP2Pdma = bindings::BLK_FEAT_PCI_P2PDMA,

        /// Skips this queue in `blk_mq_(un)quiesce_tagset`.
        SkipTagsetQuiesce = bindings::BLK_FEAT_SKIP_TAGSET_QUIESCE,

        /// Undocumented magic for bcache.
        RaidPartialStripesExpensive = bindings::BLK_FEAT_RAID_PARTIAL_STRIPES_EXPENSIVE,

        /// Atomic writes enabled.
        AtomicWrites = bindings::BLK_FEAT_ATOMIC_WRITES,
    }
}
